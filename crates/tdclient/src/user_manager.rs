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
//!   business, star-rating and note fields arrive with their subsystems
//!   (a value with those fields set fails to parse). The
//!   `first_saved_music_file_id` is tracked in memory (M.4.13) but not
//!   persisted yet — the C++ stores it through
//!   `FileManager::store_file_id`, so the store flag stays forced off
//!   until the Files step and the id does not survive a reopen;
//! - the profile-photos and saved-music listing caches (`user_photos_`,
//!   `user_saved_music_`) with their pending-request retry machines are
//!   in (M.4.13); every file id they carry is a placeholder until the
//!   Files step: the saved-music document ids live in a side registry
//!   (`saved_music_file_locations`) standing in for
//!   `get_saved_music_document_id`'s file-manager lookup, the
//!   photo/saved-music file sources are allocated by a local counter
//!   standing in for the FileReferenceManager, and the chatPhoto/audio
//!   objects answer with the codegen-forced empty payloads;
//! - the users binlog mirror (`UserLogEvent` + `on_binlog_user_event`) is
//!   in: unsaved users log to the binlog (`HandlerType::Users`), the
//!   sqlite save retires the event, and reopen replays it; the C++'s
//!   async-save window collapses onto the synchronous "common" kv write;
//! - the secret-chat storage half (`SecretChatLogEvent` +
//!   `on_binlog_secret_chat_event`, `HandlerType::SecretChatInfos`, the
//!   `sc<id>` "common"-kv rows, the `on_update_secret_chat` intake, the
//!   `update_secret_chat` diff machine, the `secret_chats_with_user`
//!   index and the read getters) is in with the same synchronous-collapse
//!   divergence; `create_new_secret_chat` (cpp:9845) stays unported — it
//!   funnels through `SecretChatsManager` (Wave 3) and
//!   `DialogManager::force_create_dialog` — and the `HandlerType::
//!   SecretChats` (= 1) events route to that manager, not here;
//! - the contacts slice (cpp:7910-8889) is in: the `user_contacts` /
//!   `user_imported_contacts` sqlite rows and the
//!   `saved_contact_count`/`next_contacts_sync_date` binlog-pmc pair (the
//!   constructor block cpp:2748-2759), `on_get_contacts`/
//!   `on_get_contacts_statuses`, the `ImportContactsTask` machine with
//!   the unique-id dedup (`import_contacts`/`on_imported_contacts`/
//!   `change_imported_contacts`/`on_update_contacts_reset`),
//!   `contacts_hints_` search, close friends, the birthdate sync
//!   (`ContactBirthdates` + `updateContactCloseBirthdays`) and
//!   `search_user_by_phone_number`/`on_resolved_phone_number`. The
//!   query senders (`contacts.getContacts`, the import family, the
//!   close-friends editor, `contacts.getBirthdays`, the phone resolver)
//!   ride the net step; the load-gated entry points keep the C++
//!   caller-retry contract (park the promise, return the empty result)
//!   with `add_contact`/`get_imported_contact_count` re-dispatched from
//!   a parked-call queue (the C++ actor-message analogue);
//! - the contact-policy slice (cpp:7754-7909 + the policy intakes) is in:
//!   the message-users registry (`user_messages_` +
//!   `register_message_users`/`unregister_message_users`; the
//!   `ChatManager::have_channel` gate is the `known_channels` stand-in,
//!   empty until the ChatManager slice feeds it), `can_send_message_to_user`
//!   with the `get_is_premium_required_to_contact_queries_` QueryMerger
//!   (the merger actor is created by `Td::init`; the merged
//!   `users.getRequirementsToContact` query rides the
//!   the `UserManager` net handle), the `user_full_contact_price_` cache +
//!   its intake `on_get_is_premium_required_to_contact_users`,
//!   `allow_send_message_
//!   to_user`, `share_phone_number` (parked in the contacts-load queue),
//!   the noforwards-request registry + deadline map
//!   (`register_noforwards_request`/`unregister_noforwards_request`/
//!   `on_noforwards_request_timeout`), the `user_rating_timeout_` deadline
//!   plumbing (the arming fields arrive with the StarRating subsystem),
//!   `get_web_app_placeholder` (bot_info absent until its subsystem, so
//!   the answer is the C++ null), the freeze state
//!   (`init_freeze_state`/`on_update_freeze_state`/`get_update_freeze_
//!   state_object`), `on_update_phone_number_privacy` and
//!   `on_ignored_restriction_reasons_changed` (+ `restricted_user_ids_`);
//!   the `inputPeerUserFromMessage`/`inputUserFromMessage` conversions
//!   wait for `ChatManager::get_simple_input_peer`;
//! - cross-manager notifications (MessagesManager dialog title/photo
//!   updates, StoryManager, ReactionManager, ContactsManager hints,
//!   OnlineManager, ConfigManager) are documented no-ops in
//!   `update_user` until those managers land;
//! - the users-core net families ride the dispatcher: `get_user`/
//!   `reload_user` park on the GetUserMerger (`get_user_queries_`),
//!   `load_user_full`/`reload_user_full` on the GetUserFullCombiner
//!   (`get_user_full_queries_`) — both created by `Td::init` — and the
//!   answers drain through the `Td` net-answer chain
//!   (`dispatch_completed_user_net`);
//! - the read surface (cpp:5196-5880) ports the InputUser/InputPeer/
//!   `inputEncryptedChat` construction, `get_bot_data` and the field
//!   getters; the min-user fromMessage fallback (`user_messages_` +
//!   `ChatManager::get_simple_input_peer`) is half in — the registry and
//!   the `have_input_peer_user` admission landed with M.4.15, the
//!   fromMessage conversions wait for the ChatManager slice,
//!   `get_user_default_permissions`/`get_secret_chat_
//!   default_permissions` and `get_bot_commands` carry
//!   `RestrictedRights`/`BotCommands` from `DialogParticipant.h` into
//!   the ChatManager slice, and `send_get_me_query` is
//!   the net step;
//! - the current codegen cannot express nullable td_api fields (the
//!   nullability modeling lands with the S76 codegen merge), so
//!   `get_user_object`/`get_user_full_info_object` emit the C++-empty
//!   payloads (empty `usernames`, `userStatusEmpty`, the empty photo,
//!   `activeStoryStateRead`, ...) where the C++ emits `null`.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::birthdate::Birthdate;
use crate::chat_manager::{ChannelType, ProfileTab, RestrictedRights};
use crate::contact::Contact;
use crate::emoji_status::{CustomEmojiId, EmojiStatus};
use crate::global::Global;
use crate::message_id::{DialogType, MessageFullId};
use crate::option_manager::OptionManager;
use crate::peer_color::{AccentColorId, PeerColor, PeerColorCollectible};
use crate::photo::{Photo, get_chat_photo_object, get_photo, photo_get_file_ids};
use crate::restriction_reason::{
    RestrictionOptions, RestrictionReason, get_restriction_info_object, parse_restriction_reasons,
    store_restriction_reasons,
};
use crate::store_parse::{self, Parser};
use crate::td_api::{self, Object};
use crate::usernames::Usernames;
use tddb::binlog_key_value::BinlogKeyValue;
use tdmtproto::telegram_api;
use tdutils::hints::Hints;
use tdutils::promise::{Promise, PromiseCreator};
use tdutils::random::Xorshift128plus;
use tdutils::status::Status;

/// `User::CACHE_VERSION` (UserManager.h:633).
const USER_CACHE_VERSION: u32 = 4;
/// `USER_FULL_EXPIRE_TIME` (UserManager.h:876).
const USER_FULL_EXPIRE_TIME: f64 = 60.0;
/// `MAX_ACTIVE_STORY_ID_RELOAD_TIME` (UserManager.h:874).
const MAX_ACTIVE_STORY_ID_RELOAD_TIME: f64 = 3600.0;
/// `SecretChatLayer::Default` (SecretChatId.h).
const SECRET_CHAT_DEFAULT_LAYER: i32 = 73;
/// `MAX_NAME_LENGTH` (UserManager.h:872).
const MAX_NAME_LENGTH: usize = 64;
/// `ACCOUNT_UPDATE_FIRST_NAME`/`ACCOUNT_UPDATE_LAST_NAME`/
/// `ACCOUNT_UPDATE_ABOUT` (UserManager.h:878-880) — the
/// `account.updateProfile` flag matrix.
const ACCOUNT_UPDATE_FIRST_NAME: i32 = 1 << 0;
const ACCOUNT_UPDATE_LAST_NAME: i32 = 1 << 1;
const ACCOUNT_UPDATE_ABOUT: i32 = 1 << 2;
/// `MAX_GET_PROFILE_PHOTOS` (UserManager.h:870).
const MAX_GET_PROFILE_PHOTOS: i32 = 100;
/// `MAX_GET_SAVED_MUSIC` (UserManager.h:871) — pub(crate) for the
/// netted round-trip asserts.
pub(crate) const MAX_GET_SAVED_MUSIC: i32 = 100;

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

/// `td::split(s, ' ')` (tdutils/misc.h:26-32) — the first delimiter; the
/// second element keeps the remainder (embedded delimiters included).
fn split_first_space(s: &str) -> (String, String) {
    match s.find(' ') {
        Some(pos) => (s[..pos].to_owned(), s[pos + 1..].to_owned()),
        None => (s.to_owned(), String::new()),
    }
}

/// `Random::fast` (td/utils/Random.h) — a one-shot xorshift seeded from
/// the process-wide fast generator (the C++ keeps one global state; the
/// per-call sequence is equivalent for a stateless draw).
fn random_fast(min_value: i32, max_value: i32) -> i32 {
    Xorshift128plus::from_seed(tdutils::random::fast_uint64()).fast(min_value, max_value)
}

/// `get_vector_hash` (misc.cpp:311-320) — the contacts hash fold.
fn get_vector_hash(numbers: &[u64]) -> i64 {
    let mut acc: u64 = 0;
    for number in numbers {
        acc ^= acc >> 21;
        acc ^= acc << 35;
        acc ^= acc >> 4;
        acc = acc.wrapping_add(*number);
    }
    acc as i64
}

/// `td::set_promises` (Promise.h) — resolve the queue with the default
/// value.
fn set_promises<T: Send + Default>(promises: &mut Vec<Promise<T>>) {
    for mut promise in std::mem::take(promises) {
        promise.set_value(T::default());
    }
}

/// `td::fail_promises` (Promise.h) — fail the queue with a cloned error.
fn fail_promises<T: Send>(promises: &mut Vec<Promise<T>>, error: &Status) {
    for mut promise in std::mem::take(promises) {
        promise.set_error(error.clone());
    }
}

/// `log_event_store(vector<UserId>)` for the `user_contacts` sqlite row
/// (UserManager.cpp:7983-7991): the log-event version, the vector count
/// and the int64 ids (the `UserId::store` = `store_long` framing).
fn store_user_ids_log_event(out: &mut Vec<u8>, user_ids: &[i64]) {
    store_parse::store_log_event_version(out);
    store_parse::store_i32(out, user_ids.len() as i32);
    for user_id in user_ids {
        store_parse::store_i64(out, *user_id);
    }
}

/// `log_event_parse(vector<UserId>)` — the parse half.
fn parse_user_ids_log_event(input: &[u8]) -> Result<Vec<i64>, &'static str> {
    let mut parser = Parser::new_log_event(input)?;
    let count = parser.i32_()?;
    if count < 0 || count as usize > input.len() {
        // the C++ parse(vector) get_left_len gate (tl_helpers.h) — a
        // hostile count must not drive a huge allocation
        return Err("Wrong vector length");
    }
    let mut user_ids = Vec::with_capacity(count as usize);
    for _ in 0..count {
        user_ids.push(parser.i64_()?);
    }
    parser.fetch_end()?;
    Ok(user_ids)
}

/// `log_event_store(vector<Contact>)` for the `user_imported_contacts`
/// sqlite row (UserManager.cpp:8297-8306).
fn store_contacts_log_event(out: &mut Vec<u8>, contacts: &[Contact]) {
    store_parse::store_log_event_version(out);
    store_parse::store_i32(out, contacts.len() as i32);
    for contact in contacts {
        contact.store(out);
    }
}

/// `log_event_parse(vector<Contact>)` — the parse half.
fn parse_contacts_log_event(input: &[u8]) -> Result<Vec<Contact>, &'static str> {
    let mut parser = Parser::new_log_event(input)?;
    let count = parser.i32_()?;
    if count < 0 || count as usize > input.len() {
        // the C++ parse(vector) get_left_len gate (tl_helpers.h)
        return Err("Wrong vector length");
    }
    let mut contacts = Vec::with_capacity(count as usize);
    for _ in 0..count {
        contacts.push(Contact::parse_from(&mut parser)?);
    }
    parser.fetch_end()?;
    Ok(contacts)
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

    /// `UserId::store` (UserId.h:53-55) — the raw id as int64.
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.0);
    }

    /// `UserId::parse` (UserId.h:57-64) — int64 from
    /// `Version::Support64BitIds` (33) on, int32 before.
    pub fn parse_from(parser: &mut store_parse::Parser) -> Result<UserId, &'static str> {
        if parser.version() >= store_parse::version::SUPPORT_64_BIT_IDS {
            Ok(UserId(parser.i64_()?))
        } else {
            Ok(UserId(parser.i32_()? as i64))
        }
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

/// `td::FileId` (files/FileId.h) — re-exported from the Files step
/// (M.12.1), which landed the real two-component handle (id + remote id,
/// compared by id alone). The photo structures' serialization still
/// writes no file ids: the C++ `FileId` store is FileManager-mediated
/// (`FileManager::store_file`), so that gate lifts with M.12.2.
///
/// `td::FileSourceId` (files/FileSourceId.h) — re-exported too; the ids
/// this module allocates still come from the local stand-in counter and
/// never leave the manager boundary (the values only key the two
/// file-source maps below) until the FileReferenceManager lands.
pub use crate::files::{FileId, FileSourceId, FileUploadId};

/// `td::StoryId` (StoryId.h) — the M.4.13 stand-in moved to the M.47
/// canonical module (`crate::story_ids`).
pub use crate::story_ids::StoryId;

/// `td::CommunityId` (CommunityId.h) — the local twin unified on the
/// canonical module by the community notification-settings slice (#113).
pub use crate::community_id::CommunityId;

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
            store_i32(out, self.small_file_id.get());
            store_i32(out, self.big_file_id.get());
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
fn get_profile_photo_id(profile_photo: &telegram_api::UserProfilePhoto) -> i64 {
    match profile_photo {
        telegram_api::UserProfilePhoto::userProfilePhoto(photo) => photo.photo_id,
        telegram_api::UserProfilePhoto::userProfilePhotoEmpty(_) => 0,
    }
}

/// `get_user_id(const telegram_api::User *)` — the id read of the wire
/// user records (the `on_set_profile_photo` have-user scan).
fn get_wire_user_id(user: &telegram_api::User) -> UserId {
    match user {
        telegram_api::User::user(user) => UserId::new(user.id),
        telegram_api::User::userEmpty(user) => UserId::new(user.id),
    }
}

/// The `UserManager::drop_user_saved_music` map mutation
/// (UserManager.cpp:9535-9550) — a free fn so the &mut self callers
/// inside the add/remove state machines stay borrow-clean.
fn drop_user_saved_music_map(
    map: &mut BTreeMap<i64, UserSavedMusic>,
    user_id: UserId,
    is_empty: bool,
) {
    if let Some(user_saved_music) = map.get_mut(&user_id.get()) {
        let new_count = if is_empty { 0 } else { -1 };
        if user_saved_music.count == new_count {
            debug_assert!(user_saved_music.saved_music_file_ids.is_empty());
            debug_assert_eq!(user_saved_music.offset, user_saved_music.count);
        } else {
            user_saved_music.saved_music_file_ids.clear();
            user_saved_music.count = new_count;
            user_saved_music.offset = user_saved_music.count;
        }
    }
}

/// The forced-empty `audio` payload — the C++
/// `audios_manager_->get_audio_object` reads AudiosManager state; the
/// object becomes real with the Audios step (module docs).
fn empty_audio_object() -> td_api::Audio {
    td_api::Audio::default()
}

/// `as_fake_dialog_photo`/`as_profile_photo` (DialogPhoto.cpp:142-208),
/// reduced like `get_profile_photo`: the 'a'/'c' size selection, the
/// minithumbnail and the animation flag with placeholder file ids. The
/// C++ "Failed to convert" error fires when the two sizes carry no
/// valid file id — a condition the placeholders cannot express until
/// the Files step, so the fields are kept instead (module docs).
fn as_profile_photo(photo: &Photo, is_personal: bool) -> ProfilePhoto {
    let mut result = ProfilePhoto::default();
    if !photo.is_empty() {
        let mut has_small = false;
        let mut has_big = false;
        for size in &photo.photos {
            if size.size_type == b'a' as i32 {
                has_small = true;
            } else if size.size_type == b'c' as i32 {
                has_big = true;
            }
        }
        if has_small && has_big {
            result.minithumbnail = photo.minithumbnail.clone();
            result.has_animation = !photo.animations.is_empty();
            result.is_personal = is_personal;
            result.id = photo.id;
        }
    }
    result
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

/// `clean_phone_number` (misc.cpp:58-60) — keep digits only (the C++
/// `remove_if(!is_digit)` drops the '+' too).
fn clean_phone_number(phone_number: String) -> String {
    phone_number
        .chars()
        .filter(|c| c.is_ascii_digit())
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
    pub has_bot_forum_sidebar: bool,
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
    pub is_noforwards_inited: bool,
    pub noforwards_my_enabled: bool,
    pub noforwards_peer_enabled: bool,

    /// `User::photo_ids` (UserManager.h:631) — the ids of the photos
    /// registered from listings/userFull, runtime-only state in the C++
    /// (never in the store blob).
    pub photo_ids: BTreeSet<i64>,

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
            has_bot_forum_sidebar: false,
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
            is_noforwards_inited: false,
            noforwards_my_enabled: false,
            noforwards_peer_enabled: false,
            photo_ids: BTreeSet::new(),
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
/// Public for the S78 fuzz family (`user_log_event` target) and the
/// byte-pinned tests.
pub fn store_user_log_event(out: &mut Vec<u8>, user_id: UserId, u: &User, ctx: &TimeCtx) {
    store_parse::store_log_event_version(out);
    store_parse::store_i64(out, user_id.get());
    u.store(out, ctx);
}

/// `UserManager::UserLogEvent`, parse side (`log_event_parse` over the
/// binlog event data). Public for the S78 fuzz family.
pub fn parse_user_log_event(input: &[u8], ctx: &TimeCtx) -> Result<(UserId, User), &'static str> {
    let mut parser = Parser::new_log_event(input)?;
    let user_id = UserId::new(parser.i64_()?);
    let user = User::parse_impl(&mut parser, ctx)?;
    parser.fetch_end()?;
    Ok((user_id, user))
}

/// `UserManager::SecretChatLogEvent` (UserManager.cpp:2692-2714), store
/// side: the log-event version prefix, then the secret chat id, then the
/// `SecretChat` store. Public for the S78 fuzz family and the
/// byte-pinned tests.
pub fn store_secret_chat_log_event(
    out: &mut Vec<u8>,
    secret_chat_id: SecretChatId,
    c: &SecretChat,
) {
    store_parse::store_log_event_version(out);
    store_parse::store_i32(out, secret_chat_id.get());
    c.store(out);
}

/// `UserManager::SecretChatLogEvent`, parse side (`log_event_parse` over
/// the binlog event data / the `sc<id>` database value). Public for the
/// S78 fuzz family.
pub fn parse_secret_chat_log_event(
    input: &[u8],
) -> Result<(SecretChatId, SecretChat), &'static str> {
    let mut parser = Parser::new_log_event(input)?;
    let secret_chat_id = SecretChatId::new(parser.i32_()?);
    let secret_chat = SecretChat::parse_impl(&mut parser)?;
    parser.fetch_end()?;
    Ok((secret_chat_id, secret_chat))
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

    pub(crate) fn store_time(out: &mut Vec<u8>, time_at: f64, ctx: &TimeCtx) {
        if time_at == 0.0 {
            store_parse::store_f64(out, -1.0);
        } else {
            let time_left = (time_at - ctx.now).max(0.0);
            store_parse::store_f64(out, time_left);
            store_parse::store_f64(out, ctx.server_time);
        }
    }

    pub(crate) fn parse_time(parser: &mut Parser, ctx: &TimeCtx) -> Result<f64, &'static str> {
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
                    self.is_noforwards_inited,
                    self.noforwards_my_enabled,
                    self.noforwards_peer_enabled,
                    // 1.8.70: appended last (UserManager.cpp STORE_FLAG
                    // order) — the shared binlog layout stays
                    // byte-compatible.
                    self.has_bot_forum_sidebar,
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
            is_noforwards_inited,
            noforwards_my_enabled,
            noforwards_peer_enabled,
            // 1.8.70: appended last (UserManager.cpp PARSE_FLAG order).
            has_bot_forum_sidebar,
        ] = if has_flags2 {
            parser.parse_flags()?
        } else {
            [false; 28]
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
        u.has_bot_forum_sidebar = has_bot_forum_sidebar;
        u.has_live_story = has_live_story;
        u.can_bot_create_topics = can_bot_create_topics;
        u.can_manage_bots = can_manage_bots;
        u.is_guestchat_bot = is_guestchat_bot;
        u.is_guard_bot = is_guard_bot;
        u.is_noforwards_inited = is_noforwards_inited;
        u.noforwards_my_enabled = noforwards_my_enabled;
        u.noforwards_peer_enabled = noforwards_peer_enabled;

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

    /// `UserFull::gift_settings` (UserManager.h) — the `setGiftSettings`
    /// slice ([`crate::star_gift_settings`]).
    pub gift_settings: crate::star_gift_settings::StarGiftSettings,

    /// `UserFull::main_profile_tab` (UserManager.h:747) — the account's
    /// chosen main profile tab ([`crate::chat_manager::ProfileTab`]).
    pub main_profile_tab: crate::chat_manager::ProfileTab,

    /// `UserFull::first_saved_music_file_id` (UserManager.h:787) — the
    /// id is a placeholder until the Files step and the store keeps the
    /// flag forced off (the C++ persists it through
    /// `FileManager::store_file_id`), so it does not survive a reopen
    /// yet.
    pub first_saved_music_file_id: FileId,

    pub is_first_saved_music_file_id_changed: bool,

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
            gift_settings: crate::star_gift_settings::StarGiftSettings::default(),
            main_profile_tab: crate::chat_manager::ProfileTab::default(),
            first_saved_music_file_id: FileId::default(),
            is_first_saved_music_file_id_changed: false,
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
        let has_gift_settings = !self.gift_settings.is_default();
        let has_star_rating = false; // StarRating subsystem
        let has_pending_star_rating = false;
        let has_main_profile_tab = self.main_profile_tab != ProfileTab::Default;
        let has_first_saved_music_file_id = false; // DIVERGENCE(Files step)
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
        if has_gift_settings {
            self.gift_settings.store(out);
        }
        if has_main_profile_tab {
            store_i32(out, self.main_profile_tab.as_raw());
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
            || has_star_rating
            || has_pending_star_rating
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
        if has_gift_settings {
            result.gift_settings = crate::star_gift_settings::StarGiftSettings::parse_from(parser)?;
        }
        if has_main_profile_tab {
            result.main_profile_tab = ProfileTab::from_raw(parser.i32_()?);
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
    pub log_event_id: u64,
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
            log_event_id: 0,
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
        let mut parser = Parser::new_log_event(input)?;
        let result = Self::parse_impl(&mut parser)?;
        parser.fetch_end()?;
        Ok(result)
    }

    /// The parser-position form `SecretChatLogEvent::parse` reaches
    /// through (the id is parsed around it).
    fn parse_impl(parser: &mut Parser) -> Result<SecretChat, &'static str> {
        use store_parse::version;
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
        Ok(result)
    }
}

/// The shared-context slice every manager method needs — the `Td *td_` of
/// the C++, narrowed to the pieces the user manager touches.
/// `td::UserManager::BotData` (UserManager.h:261-276).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BotData {
    pub username: String,
    pub can_be_edited: bool,
    pub can_join_groups: bool,
    pub can_read_all_group_messages: bool,
    pub has_main_app: bool,
    pub has_bot_forum_view: bool,
    pub can_bot_create_topics: bool,
    pub can_manage_bots: bool,
    pub is_inline: bool,
    pub is_guestchat: bool,
    pub is_business: bool,
    pub is_guard: bool,
    pub need_location: bool,
    pub can_be_added_to_attach_menu: bool,
}

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

/// The user-plane in-flight query kinds — the C++ ResultHandler classes
/// of the landed families (the users core cpp:1432-1803, the emoji-status
/// family EmojiStatus.cpp:165-352, the profile-photo plane
/// UserManager.cpp:662-816/1489). The kinds ride the
/// `UserManager::net` handle (the `td_` back-pointer narrowed to
/// the net plane); the answers drain through
/// [`Td::dispatch_completed_user_net`](crate::td::Td::dispatch_completed_user_net).
#[derive(Clone, Copy, Debug)]
pub(crate) enum UserNetKind {
    /// `GetUsersQuery` (cpp:1432) — the merged `users.getUsers` query of
    /// the GetUserMerger; the answer feeds `on_get_users` and flushes
    /// the merger's result promise.
    GetUsers,
    /// `GetFullUserQuery` (cpp:1459) — the combined `users.getFullUser`
    /// query; the answer feeds `on_get_users`, `on_get_chats` and
    /// `on_get_user_full`, then flushes the combiner's result promise.
    GetFullUser,
    /// `GetIsPremiumRequiredToContactQuery` (cpp:1777) — the merged
    /// `users.getRequirementsToContact` query; the answer feeds
    /// `on_get_is_premium_required_to_contact_users` (the user ids ride
    /// the family queue — the handler's `user_ids_` member).
    GetRequirementsToContact,
    /// `UpdateContactNoteQuery` (cpp:835) — the
    /// `contacts.updateContactNote` query; the user id, the normalized
    /// note and the promise ride the family queue, the ok answer runs
    /// `on_set_user_note` (the cache-write ack).
    UpdateContactNote,
    /// `SetMainProfileTabQuery` (cpp:1299) — the
    /// `account.setMainProfileTab` query; the tab and the promise ride
    /// the family queue, the Bool answer runs `on_set_main_profile_tab`
    /// (the user_full write ack, cpp:6702).
    SetMainProfileTab,
    /// `SuggestUserBirthdayQuery` (cpp:862) — the
    /// `users.suggestBirthday` query; the promise rides the family
    /// queue into the Updates answer's `on_get_updates` call.
    SuggestBirthday,
    /// `GetSupportUserQuery` (cpp:1748) — the `help.getSupport` query;
    /// the answer feeds `on_get_user` then `on_get_support_user`, which
    /// flushes the parked promises with the fresh object.
    GetSupport,
    /// `UpdateEmojiStatusQuery` (cpp:1373) — the
    /// `account.updateEmojiStatus` query; the cloned status and the
    /// promise ride the family queue, the Bool answer runs
    /// `on_set_emoji_status` (the local-user apply, cpp:6765), the
    /// error band refetches the recent list first (cpp:1399-1401).
    UpdateEmojiStatus,
    /// `UpdateColorQuery` (UserManager.cpp:890) — the
    /// `account.updateColor` query (the user leg; the channel twins
    /// ride the ChatManager handle); the kind carries the answer's
    /// `on_update_accent_color_success` members (`for_profile_`,
    /// `accent_color_id_`, `background_custom_emoji_id_`), the promise
    /// rides the family queue.
    UpdateColor {
        for_profile: bool,
        accent_color_id: AccentColorId,
        background_custom_emoji_id: CustomEmojiId,
    },
    /// `GetDefaultEmojiStatusesQuery` (cpp:165).
    GetDefault,
    /// `GetChannelDefaultEmojiStatusesQuery` (cpp:209).
    GetChannelDefault,
    /// `GetRecentEmojiStatusesQuery` (cpp:251).
    GetRecent,
    /// `ClearRecentEmojiStatusesQuery` (cpp:293).
    ClearRecent,
    /// `GetCollectibleEmojiStatusesQuery` (cpp:319).
    GetCollectible,
    /// `SaveMusicQuery` (cpp:1540) — the kind carries the anchor pair
    /// for the `on_add_saved_music` tail (the handler's `file_id_`/
    /// `after_file_id_` members); the file-reference repair of on_error
    /// rides the Files plane (the plain error fails the promise).
    SaveMusic {
        file_id: FileId,
        after_file_id: FileId,
    },
    /// `UnsaveMusicQuery` (cpp:1596) — the kind carries the removed
    /// file for the `on_remove_saved_music` tail (the handler's
    /// `file_id_`; the on_error file-reference log rides Files).
    UnsaveMusic { file_id: FileId },
    /// `GetUserSavedMusicQuery` (cpp:1630) — the kind carries the
    /// handler's `user_id_`/`offset_`/`limit_` members for the intake;
    /// the answer constructor gate (non-`users_savedMusic` → 500) and
    /// the query-promise lambda (`finish_get_user_saved_music`) run on
    /// the Td side.
    GetUserSavedMusic {
        user_id: UserId,
        offset: i32,
        limit: i32,
    },
    /// `GetUserPhotosQuery` (cpp:1489) — the kind carries the answer
    /// members the handler kept (`user_id_`, `offset_`, `limit_`); the
    /// offset `>= 0` answer routes to the pending-request retry machine
    /// (`on_get_user_profile_photos`), the offset `-1` answer to the
    /// reload leg's query-carried promise (cpp:6988-6992).
    GetUserPhotos {
        user_id: UserId,
        offset: i32,
        limit: i32,
    },
    /// `GetUserSavedMusicByIdQuery` (cpp:1672) — the kind carries the
    /// owner user for the document intake; the caller promise rides the
    /// family queue (the C++ carries it on the query — the §5 item
    /// restored with net-drain 09).
    GetUserSavedMusicById { user_id: UserId },
    /// `GetSavedMusicIdsQuery` (cpp:1720) — no members; the answer
    /// rides the `reload_my_saved_music_queries` family queue.
    GetSavedMusicIds,
    /// `GetContactsQuery` (cpp:117) — fire-and-forget; the load promises
    /// park in `load_contacts_queries` (the `load_contacts` shape), the
    /// answer runs the `on_get_contacts` intake.
    GetContacts,
    /// `GetContactsStatusesQuery` (cpp:187) — fire-and-forget at both
    /// send sites (the `on_get_contacts` notModified arm and
    /// `on_update_online_status_privacy`); the answer runs
    /// `on_get_contacts_statuses`.
    GetContactsStatuses,
    /// `AddContactQuery` (cpp:209) — the user id (the error tail's
    /// dialog action-bar reload — the MessagesManager step parks with
    /// its plane) and the query-carried promise ride the family
    /// queue (the §5 restore); the Updates answer re-enters
    /// `on_get_updates`.
    AddContact,
    /// `ImportContactsQuery` (cpp:353) — the kind carries the handler's
    /// `random_id_`/`sent_size_` members (the 429 all-retry band reads
    /// the sent size); the promise rides the task registry.
    ImportContacts { random_id: i64, sent_size: usize },
    /// `DeleteContactsQuery` (cpp:383) — the query-carried promise rides
    /// the family queue (the §5 restore); the Updates answer re-enters
    /// `on_get_updates`.
    DeleteContacts,
    /// `DeleteContactsByPhoneNumberQuery` (cpp:411) — the handler's
    /// `user_ids_` (the `on_deleted_contacts` intake) and the promise
    /// ride the family queue; shares the `DeleteContacts` error tail.
    DeleteContactsByPhones,
    /// `ResetContactsQuery` (cpp:448) — the promise rides the family
    /// queue; the Bool true band runs `on_update_contacts_reset`.
    ResetContacts,
    /// `GetContactsBirthdaysQuery` (cpp:139) — fire-and-forget; the
    /// answer (and the error band) runs `on_get_contact_birthdates`.
    GetBirthdays,
    /// `DismissContactBirthdaysSuggestionQuery` (cpp:161) — the promise
    /// rides the family queue (the §5 restore).
    DismissBirthdays,
    /// `EditCloseFriendsQuery` (cpp:250) — the handler's `user_ids_`
    /// (the `on_set_close_friends` intake) and the promise ride the
    /// family queue (the §5 restore).
    EditCloseFriends,
    /// `ResolvePhoneQuery` (cpp:278) — the handler's `phone_number_`
    /// (the `on_resolved_phone_number` cache write) and the promise
    /// ride the family queue (the §5 restore).
    ResolvePhone,
    /// `AcceptContactQuery` (cpp:322) — the user id (the error tail's
    /// dialog action-bar reload — the MessagesManager step parks with
    /// its plane) and the promise ride the family queue (the §5
    /// restore); the Updates answer re-enters `on_get_updates`.
    AcceptContact,
    /// `UpdateProfilePhotoQuery` (cpp:662) — the kind carries the
    /// answer's `on_set_profile_photo` members: the user, the replaced
    /// photo (the `delete_my_profile_photo_from_cache` head) and the
    /// fallback flag. Serves the prev-photo arm (cpp:6027-6030) and the
    /// delete reset arm (cpp:6418-6422).
    UpdateProfilePhoto {
        user_id: UserId,
        old_photo_id: i64,
        is_fallback: bool,
    },
    /// `DeleteContactProfilePhotoQuery` (cpp:766) — the kind carries
    /// the user for the `on_set_profile_photo` tail (the handler nulls
    /// the photo and passes `old_photo_id` 0 / `is_fallback` false,
    /// cpp:789-793).
    DeleteContactProfilePhoto { user_id: UserId },
    /// `DeleteProfilePhotoQuery` (cpp:799) — the kind carries the photo
    /// id for the `on_delete_profile_photo` cache delete (the
    /// handler's `profile_photo_id_` member).
    DeleteProfilePhoto { profile_photo_id: i64 },
    /// `UploadProfilePhotoQuery` (cpp:531) — the kind carries the
    /// handler's answer members (`user_id_`, `is_fallback_`,
    /// `only_suggest_`) and the `file_upload_id_` of the
    /// `delete_partial_remote_location` tail (invalid on the sticker
    /// arm, which uploads no file); the ok answer runs
    /// `on_set_profile_photo` (the net-drain 08 intake) or resolves
    /// directly on `only_suggest`.
    UploadProfilePhoto {
        user_id: UserId,
        is_fallback: bool,
        only_suggest: bool,
        file_upload_id: FileUploadId,
    },
    /// `UploadSavedMusicFileQuery` (cpp:482) — the kind carries the
    /// handler members (`file_upload_id_`, `is_url_`, `was_uploaded_`)
    /// for the `on_uploaded_saved_music_file` tail and the on_error
    /// upload legs; the promise's ok refires `add_saved_music` (the
    /// `PromiseCreator` lambda of `add_new_saved_music`,
    /// cpp:7205-7212).
    UploadSavedMusicFile {
        file_upload_id: FileUploadId,
        is_url: bool,
        was_uploaded: bool,
    },
}

/// `td::AccessRights` (AccessRights.h) — `enum class AccessRights : int32 {
/// Know, Read, Edit, Write }`; the variants stay in-memory only here, so the
/// declaration order keeps the pre-M.6 subset first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessRights {
    Know,
    Read,
    /// `AccessRights::Edit` — arrives with the DialogManager slice.
    Edit,
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

/// `UserManager::PendingGetPhotoRequest` (UserManager.h:841).
pub struct PendingGetPhotoRequest {
    offset: i32,
    limit: i32,
    retry_count: i32,
    promise: Promise<td_api::ChatPhotos>,
}

/// `UserManager::UploadedProfilePhoto` (UserManager.h) — the registry
/// entry of an in-flight profile-photo upload
/// (`being_uploaded_profile_photos_`).
pub(crate) struct UploadedProfilePhoto {
    user_id: UserId,
    is_fallback: bool,
    only_suggest: bool,
    main_frame_timestamp: f64,
    is_animation: bool,
    reupload_count: i32,
    promise: Promise<()>,
}

/// `UserManager::UserPhotos` (UserManager.h:848) — the cached window of
/// the user's profile photos; `count`/`offset` `-1` = unknown (the C++
/// default member initializers).
#[derive(Clone, Debug, PartialEq)]
pub struct UserPhotos {
    pub photos: Vec<Photo>,
    pub count: i32,
    pub offset: i32,
}

impl Default for UserPhotos {
    fn default() -> Self {
        UserPhotos::new()
    }
}

impl UserPhotos {
    pub fn new() -> UserPhotos {
        UserPhotos {
            photos: Vec::new(),
            count: -1,
            offset: -1,
        }
    }
}

/// `UserManager::PendingGetSavedMusicRequest` (UserManager.h:854).
pub struct PendingGetSavedMusicRequest {
    offset: i32,
    limit: i32,
    retry_count: i32,
    promise: Promise<td_api::Audios>,
}

/// `UserManager::UserSavedMusic` (UserManager.h:861) — `count`/`offset`
/// `-1` = unknown (the C++ default member initializers).
#[derive(Clone, Debug, PartialEq)]
pub struct UserSavedMusic {
    pub saved_music_file_ids: Vec<FileId>,
    pub count: i32,
    pub offset: i32,
}

impl Default for UserSavedMusic {
    fn default() -> Self {
        UserSavedMusic::new()
    }
}

impl UserSavedMusic {
    pub fn new() -> UserSavedMusic {
        UserSavedMusic {
            saved_music_file_ids: Vec::new(),
            count: -1,
            offset: -1,
        }
    }
}

/// `UserManager::ImportContactsTask` (UserManager.h:1345-1351) — the
/// in-flight import: the caller promise, the input contacts and the two
/// per-position result buffers. The retry round rebuilds the
/// `inputPhoneContact`s from `input_contacts` with the server's client
/// ids (cpp:8248-8259).
struct ImportContactsTask {
    promise: Promise<()>,
    input_contacts: Vec<Contact>,
    imported_user_ids: Vec<UserId>,
    unimported_contact_invites: Vec<i32>,
}

/// `UserManager::ContactBirthdates` (UserManager.h:1388-1393).
#[derive(Default)]
struct ContactBirthdates {
    users: Vec<(UserId, Birthdate)>,
    next_sync_time: f64,
    is_being_synced: bool,
    need_drop: bool,
}

/// The load-gated calls the C++ re-dispatches as actor messages from the
/// load-contacts promise; the Rust manager parks them here (the
/// actor-message-queue analogue) and replays them on load completion or
/// errors them on load failure.
enum PendingLoadContactsCall {
    AddContact {
        user_id: UserId,
        contact: Contact,
        share_phone_number: bool,
        promise: Promise<()>,
    },
    GetImportedContactCount {
        promise: Promise<i32>,
    },
    SharePhoneNumber {
        user_id: UserId,
        promise: Promise<()>,
    },
}
/// The load-gated td_api requests (`Requests.cpp` request actors
/// `GetCloseFriendsRequest`/`SearchContactsRequest`/
/// `ImportContactsRequest`/`ChangeImportedContactsRequest`): parked while
/// the contacts load. The C++ request actors re-run `do_run` after the
/// load and answer from `do_send_result`; the Rust park carries a
/// data promise the request layer resolves (the manager builds the
/// td_api object on the re-run, like `do_send_result` does).
pub(crate) enum PendingUserRequest {
    GetCloseFriends {
        promise: Promise<Object>,
    },
    SearchContacts {
        query: String,
        limit: i32,
        promise: Promise<Object>,
    },
    ImportContacts {
        contacts: Vec<Contact>,
        random_id: i64,
        promise: Promise<Object>,
    },
    ChangeImportedContacts {
        contacts: Vec<Contact>,
        random_id: i64,
        promise: Promise<Object>,
    },
}

/// The td-plane pair the photo-set flow reads — the C++ reaches all
/// through `td_`: the shared user env, the file-manager surface the
/// flows read and write (`get_input_file_id`, the upload registration)
/// with its loader plane, the profile-photo upload callback (the
/// Td-side outbox bridge standing in for
/// `upload_profile_photo_callback_`) and the stickers read surface of
/// the sticker arm (`have_sticker`/`have_custom_emoji`/`get_input_sticker_set`).
/// The `pub` visibility follows [`UserEnv`] (a param struct of the pub
/// manager methods).
pub struct SetProfilePhotoTd<'a> {
    pub env: &'a UserEnv<'a>,
    pub files: &'a mut crate::files::FileManager,
    pub fllm: &'a mut crate::files::FileLoadManager,
    pub upload_callback: &'a std::sync::Arc<parking_lot::Mutex<dyn crate::files::UploadCallback>>,
    pub stickers: &'a crate::stickers_manager::StickersManager,
}

/// The td-plane pair the saved-music upload flow reads — the C++
/// `td_->file_manager_`/`td_->audios_manager_` reach of
/// `add_new_saved_music` (`upload_saved_music_callback_` is the
/// Td-side outbox bridge; `close_flag` is the `G()->close_status()`
/// read of the `do_upload_saved_music` gate).
pub struct SavedMusicUploadTd<'a> {
    pub files: &'a mut crate::files::FileManager,
    pub fllm: &'a mut crate::files::FileLoadManager,
    pub upload_callback: &'a std::sync::Arc<parking_lot::Mutex<dyn crate::files::UploadCallback>>,
    pub audios: &'a mut crate::audios_manager::AudiosManager,
    pub close_flag: bool,
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
    loaded_from_database_secret_chats: BTreeSet<i32>,
    /// `secret_chats_with_user_` — per user, the secret chats with them.
    secret_chats_with_user: BTreeMap<i64, Vec<i32>>,

    /// `unavailable_user_fulls_` (UserManager.cpp:8919) — ids whose
    /// `usf<id>` row is known absent; guards a re-read per id.
    unavailable_user_fulls: BTreeSet<i64>,

    resolved_phone_numbers: BTreeMap<String, UserId>,
    restricted_user_ids: BTreeSet<i64>,

    /// `my_saved_music_ids_` (UserManager.h:1296) — the server document
    /// ids of my saved music, over the `my_saved_music_ids` binlog-pmc
    /// key.
    my_saved_music_ids: BTreeSet<i64>,
    are_my_saved_music_ids_inited: bool,

    /// `user_photos_` (UserManager.h:1258) — the profile-photos listing
    /// cache.
    user_photos: BTreeMap<i64, UserPhotos>,
    /// `pending_get_user_photos_requests_` (UserManager.h:1259).
    pending_get_user_photos_requests: BTreeMap<i64, Vec<PendingGetPhotoRequest>>,
    /// The `SaveMusicQuery` promises (UserManager.cpp:1542).
    pending_add_saved_music_queries: Vec<Promise<()>>,
    /// The `UnsaveMusicQuery` promises (UserManager.cpp:1598).
    pending_remove_saved_music_queries: Vec<Promise<()>>,
    /// The `GetUserSavedMusicByIdQuery` promises (UserManager.cpp:1674)
    /// — the C++ carries the promise on the query; ours rides this
    /// FIFO queue (the §5 pairing shape).
    pending_reload_saved_music_by_id_queries: Vec<Promise<()>>,
    /// The parked `GetUserPhotosQuery` promises of the offset `-1`
    /// reload leg (the query-carried form, cpp:6990 — the §5 fidelity
    /// restore): every offset `-1` send parks exactly one and every
    /// offset `-1` answer pops the head, so the pairing is exact.
    pending_get_user_photos_reload_queries: Vec<Promise<()>>,
    /// The parked `UpdateProfilePhotoQuery` promises (the handler's
    /// `promise_` member; FIFO per kind per the §5 note).
    pending_update_profile_photo_queries: Vec<Promise<()>>,
    /// The parked `DeleteContactProfilePhotoQuery` promises.
    pending_delete_contact_profile_photo_queries: Vec<Promise<()>>,
    /// The parked `DeleteProfilePhotoQuery` promises.
    pending_delete_profile_photo_queries: Vec<Promise<()>>,
    /// the promise queue of the UploadProfilePhoto family — the C++
    /// carries the promise on the query; the FIFO family queue is the
    /// §5 convention of the landed kinds.
    pending_upload_profile_photo_queries: Vec<Promise<()>>,
    /// the promise queue of the UploadSavedMusicFile family (§5).
    pending_upload_saved_music_queries: Vec<Promise<()>>,
    /// `being_uploaded_profile_photos_` (UserManager.h) — the in-flight
    /// profile-photo upload registry keyed by `FileUploadId`.
    being_uploaded_profile_photos: BTreeMap<FileUploadId, UploadedProfilePhoto>,
    /// `being_uploaded_saved_music_files_` (UserManager.h) — the
    /// in-flight saved-music upload promises keyed by `FileUploadId`.
    being_uploaded_saved_music_files: BTreeMap<FileUploadId, Promise<()>>,
    /// `pending_user_photos_` (UserManager.h:1262) — the bot
    /// without-chat-info-database photo hold.
    pending_user_photos: BTreeMap<i64, telegram_api::UserProfilePhoto>,
    /// `user_profile_photo_file_source_ids_` (UserManager.h:1255) keyed
    /// by (user id, photo id).
    user_profile_photo_file_source_ids: BTreeMap<(i64, i64), FileSourceId>,
    /// `my_photo_file_id_` (UserManager.h:1256) — my photo id → the
    /// first file id of the registered photo.
    my_photo_file_id: BTreeMap<i64, FileId>,

    /// `user_saved_music_` (UserManager.h:1291) — the saved-music listing
    /// cache.
    user_saved_music: BTreeMap<i64, UserSavedMusic>,
    /// `pending_get_user_saved_music_requests_` (UserManager.h:1292).
    pending_get_user_saved_music_requests: BTreeMap<i64, Vec<PendingGetSavedMusicRequest>>,
    /// `user_saved_music_file_source_ids_` (UserManager.h:1289) keyed by
    /// the `UserSavedMusicId` triple.
    user_saved_music_file_source_ids: BTreeMap<(i64, i64, i64), FileSourceId>,
    /// The `get_saved_music_document_id` stand-in registry
    /// (UserManager.cpp:4673-4693): FileId → the remote
    /// (document id, access hash). The C++ reads both off the file
    /// manager's full remote location; the mapping is populated at the
    /// saved-music intake until the Files step replaces it.
    saved_music_file_locations: BTreeMap<i32, (i64, i64)>,
    /// `reload_my_saved_music_queries_` (UserManager.h:1297).
    reload_my_saved_music_queries: Vec<Promise<()>>,
    /// The `is_saved_music` not-inited follow-ups — the C++ queues the
    /// `check_is_saved_music` lambda into the reload queue
    /// (cpp:7155-7163); the file ids ride here and the intake drains
    /// them after the queue resolves (the collapsed actor hop).
    pending_saved_music_checks: Vec<(FileId, Promise<()>)>,

    /// `support_user_id_` (UserManager.h:1318) — the cached support user.
    support_user_id: UserId,
    /// The parked `GetSupportUserQuery` promises (the query is live
    /// over the `net` handle; `on_get_support_user` flushes them with
    /// the fresh object, `fail_pending_support_user_queries` with the
    /// error).
    pending_support_user_queries: Vec<Promise<td_api::User>>,
    /// The parked `GetDefaultEmojiStatusesQuery` promises (the acks
    /// split from the channel family — the per-handler promise shape).
    pending_default_emoji_statuses_queries: Vec<Promise<td_api::EmojiStatusCustomEmojis>>,
    /// The parked `GetChannelDefaultEmojiStatusesQuery` promises —
    /// `pending_channel_default_emoji_statuses_queries_` would be the
    /// C++ field; the acks split by family at the query integration.
    pending_channel_emoji_statuses_queries: Vec<Promise<td_api::EmojiStatusCustomEmojis>>,
    /// The parked `GetRecentEmojiStatusesQuery` promises.
    pending_recent_emoji_statuses_queries: Vec<Promise<td_api::EmojiStatuses>>,
    /// The parked `GetCollectibleEmojiStatusesQuery` promises — split
    /// from the recent family like the channel twin.
    pending_upgraded_gift_emoji_statuses_queries: Vec<Promise<td_api::EmojiStatuses>>,
    /// The parked `ClearRecentEmojiStatusesQuery` promises (the ok tail).
    pending_clear_recent_emoji_statuses_queries: Vec<Promise<()>>,
    /// The net dispatch surface — the `td_` back-pointer the C++ manager
    /// uses for its query sends, narrowed to the net plane (`None` in
    /// the net-inert tests; set at the dispatcher install).
    pub(crate) net: Option<crate::td::UserNetHandle>,

    /// The FileReferenceManager stand-in: the allocator behind every
    /// `FileSourceId` this module hands out.
    next_file_source_id: i32,
    /// The placeholder allocator for saved-music `FileId`s (the
    /// AudiosManager/DocumentsManager allocation is the Files step); the
    /// values are internal-only and never persisted.
    next_saved_music_file_id: i32,

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
    /// `user_rating_timeout_` deadlines — the M.4.15 plumbing; the
    /// StarRating fields that arm it arrive with their subsystem, so the
    /// map stays empty until then.
    user_rating_timeouts: BTreeMap<i64, f64>,
    /// `noforwards_request_timeout_` deadlines.
    noforwards_request_timeouts: BTreeMap<i32, f64>,

    // ------------------------------------------------------------------
    // Contact policy + registries (UserManager.cpp:7754-7909 + the policy
    // intakes) — M.4.15.
    /// `user_messages_` (UserManager.h:1358) — per user, the channel
    /// messages that referenced them (the min-user fallback registry).
    user_messages: BTreeMap<i64, BTreeSet<MessageFullId>>,
    /// `user_full_contact_price_` (UserManager.h:1377) — the cached
    /// premium-required-to-contact price per user: `-1` premium required,
    /// `0` no restriction, `> 0` the paid-messages star count.
    user_full_contact_price: BTreeMap<i64, i64>,
    /// `noforwards_request_ids_` / `noforwards_request_message_full_ids_`
    /// (UserManager.h:1379-1380) — the two directions of the
    /// noforwards-request registry. Hash maps like the C++ FlatHashMaps:
    /// lookups must never run `MessageId`'s scheduled/ordinary ordering
    /// CHECK, which a key-sorted container would trip.
    noforwards_request_ids: HashMap<MessageFullId, i32>,
    noforwards_request_message_full_ids: HashMap<i32, MessageFullId>,
    /// `current_noforwards_request_id_` (UserManager.h:1254).
    current_noforwards_request_id: i32,
    /// `freeze_since_date_` / `freeze_until_date_` / `freeze_appeal_url_`
    /// (UserManager.h:1384-1386).
    freeze_since_date: i32,
    freeze_until_date: i32,
    freeze_appeal_url: String,
    /// The `ChatManager::have_channel` stand-in behind the
    /// `register_message_users` channel gate: the known-channel registry
    /// ChatManager will feed when it lands, empty until then.
    known_channels: BTreeSet<i64>,
    /// `get_is_premium_required_to_contact_queries_` (UserManager.h:1312)
    /// — the QueryMerger actor, created by the Td init.
    pub(crate) get_is_premium_required_to_contact_queries:
        Option<tdactor::ActorId<crate::query_merger::QueryMerger>>,
    /// `get_user_queries_` (UserManager.h:1310) — the GetUserMerger actor
    /// ("GetUserMerger", 3, 50), created by the Td init.
    pub(crate) get_user_queries: Option<tdactor::ActorId<crate::query_merger::QueryMerger>>,
    /// `get_user_full_queries_` (UserManager.h:1314) — the
    /// GetUserFullCombiner actor ("GetUserFullCombiner", 2.0), created by
    /// the Td init.
    pub(crate) get_user_full_queries:
        Option<tdactor::ActorId<crate::query_combiner::QueryCombiner>>,
    /// The `GetUsersQuery` in-flight promises (the C++ handler's
    /// `promise_`, cpp:1428-1445) — the merger's result promise per
    /// merged batch; the answer pops the head.
    pending_get_users_queries: Vec<Promise<()>>,
    /// The `GetFullUserQuery` in-flight promises (cpp:1461-1481) — the
    /// combiner's result promise per combined query.
    pending_get_full_user_queries: Vec<Promise<()>>,
    /// The `GetIsPremiumRequiredToContactQuery` in-flight states (the
    /// handler's `user_ids_` + `promise_`, cpp:1779-1803) — the answer
    /// intake keys on the user ids.
    pending_get_requirements_to_contact_queries: Vec<(Vec<UserId>, Promise<()>)>,
    /// The owning Td actor (the C++ `td_` back-pointer) — the
    /// `can_send_message_to_user` merger re-dispatch rides it.
    pub(crate) td_actor_id: Option<tdactor::ActorId<crate::td::Td>>,

    // ------------------------------------------------------------------
    // Contacts (UserManager.cpp:7910-8889) + the constructor's contacts
    // block (cpp:2748-2759). The query senders (`contacts.getContacts`,
    // the import family, `contacts.getStatuses`, `contacts.getBirthdays`,
    // `contacts.resolvePhone`, the close-friends editor) ride the net
    // step; the local state machines, the intakes and the persistence
    // are here.
    /// `are_contacts_loaded_` (UserManager.h:1360).
    are_contacts_loaded: bool,
    /// `next_contacts_sync_date_` (UserManager.h:1361).
    next_contacts_sync_date: i32,
    /// `contacts_hints_` (UserManager.h:1362) — the contact search index
    /// over first/last names and usernames.
    contacts_hints: Hints,
    /// `load_contacts_queries_` (UserManager.h:1363) — the pending
    /// load-contacts promises (the caller-retry contract).
    load_contacts_queries: Vec<Promise<()>>,
    /// The load-gated internal re-dispatches (see
    /// [`PendingLoadContactsCall`]).
    pending_load_contacts_calls: Vec<PendingLoadContactsCall>,
    /// The `UpdateContactNoteQuery` in-flight states (the handler's
    /// query promise lambda members, cpp:835-858) — the user id, the
    /// normalized note and the promise ride the family queue; the ok
    /// answer runs `on_set_user_note` with them.
    pending_update_contact_note_queries:
        Vec<(UserId, crate::message_entity::FormattedText, Promise<()>)>,
    /// The `SetMainProfileTabQuery` in-flight states (cpp:1299-1321) —
    /// the tab and the promise; the Bool answer runs
    /// `on_set_main_profile_tab` (cpp:6702).
    pending_set_main_profile_tab_queries: Vec<(ProfileTab, Promise<()>)>,
    /// The `SuggestUserBirthdayQuery` promises (cpp:862-884) — the
    /// answer's `on_get_updates` call consumes the promise.
    pending_suggest_birthday_queries: Vec<Promise<()>>,
    /// The `UpdateEmojiStatusQuery` in-flight states (cpp:1373-1403) —
    /// the cloned status and the promise; the ok answer runs
    /// `on_set_emoji_status` (cpp:6765).
    pending_update_emoji_status_queries: Vec<(Option<EmojiStatus>, Promise<()>)>,
    /// The `UpdateColorQuery` promises (UserManager.cpp:897-931) — the
    /// `setAccentColor`/`setChatProfileAccentColor` user-leg promises.
    pending_update_color_queries: Vec<Promise<()>>,
    /// The load-gated td_api requests (see
    /// [`PendingUserRequest`]) — the request-actor re-run analogue: parked
    /// with the request id while the contacts load, re-run to completion
    /// on load finish / failed.
    pending_user_requests: Vec<PendingUserRequest>,
    /// `saved_contact_count_` (UserManager.h:1365) — `-1` before the
    /// first load (the `new()` override).
    saved_contact_count: i32,

    /// The `AddContactQuery` in-flight pairs (the handler's `user_id_`
    /// member + the query-carried promise, the §5 restore) — popped FIFO
    /// by the Updates answers.
    pending_add_contact_queries: Vec<(UserId, Promise<()>)>,
    /// The `DeleteContactsQuery` promises (the query-carried §5 restore)
    /// — popped FIFO by the Updates answers.
    pending_delete_contacts_queries: Vec<Promise<()>>,
    /// The `DeleteContactsByPhoneNumberQuery` pairs (the handler's
    /// `user_ids_` member + the promise) — popped FIFO by the Bool
    /// answers; the true band runs `on_deleted_contacts` with them.
    pending_delete_contacts_by_phones_queries: Vec<(Vec<UserId>, Promise<()>)>,
    /// The `ResetContactsQuery` promises — popped FIFO by the Bool
    /// answers; the true band runs `on_update_contacts_reset`.
    pending_reset_contacts_queries: Vec<Promise<()>>,
    /// The `DismissContactBirthdaysSuggestionQuery` promises — popped
    /// FIFO by the Bool answers.
    pending_dismiss_birthdays_queries: Vec<Promise<()>>,
    /// The `EditCloseFriendsQuery` pairs (the handler's `user_ids_`
    /// member + the promise) — popped FIFO by the Bool answers; the ok
    /// band runs `on_set_close_friends` with them.
    pending_edit_close_friends_queries: Vec<(Vec<UserId>, Promise<()>)>,
    /// The `ResolvePhoneQuery` pairs (the handler's `phone_number_`
    /// member + the promise) — popped FIFO by the
    /// `contacts.resolvedPhone` answers.
    pending_resolve_phone_queries: Vec<(String, Promise<()>)>,
    /// The `AcceptContactQuery` pairs (the handler's `user_id_` member +
    /// the promise) — popped FIFO by the Updates answers.
    pending_accept_contact_queries: Vec<(UserId, Promise<()>)>,
    /// The parked `searchUserByPhoneNumber` requests awaiting a resolve
    /// answer — the `SearchUserByPhoneNumberRequest` do_run/do_send_result
    /// re-run collapse (the answer caches the phone, the drain answers
    /// from the cache).
    pub(crate) pending_search_phone_requests: Vec<(String, Promise<Object>)>,

    /// `are_imported_contacts_loaded_` (UserManager.h:1370).
    are_imported_contacts_loaded: bool,
    /// `load_imported_contacts_queries_` (UserManager.h:1371).
    load_imported_contacts_queries: Vec<Promise<()>>,
    /// `all_imported_contacts_` (UserManager.h:1373) — the persisted
    /// imported-contacts list.
    all_imported_contacts: Vec<Contact>,
    /// `are_imported_contacts_changing_` (UserManager.h:1374).
    are_imported_contacts_changing: bool,
    /// `need_clear_imported_contacts_` (UserManager.h:1375).
    need_clear_imported_contacts: bool,
    /// `import_contact_tasks_` (UserManager.h:1355) — the in-flight
    /// import tasks by random_id.
    import_contact_tasks: BTreeMap<i64, ImportContactsTask>,
    /// `imported_contacts_` (UserManager.h:1354) — the reserved
    /// import-result slots by random_id.
    imported_contacts: BTreeMap<i64, (Vec<UserId>, Vec<i32>)>,

    /// `next_all_imported_contacts_` / `imported_contacts_unique_id_` /
    /// `imported_contacts_pos_` (UserManager.h:1396-1398) — the
    /// `change_imported_contacts` state between the removal and the
    /// import.
    next_all_imported_contacts: Vec<Contact>,
    imported_contacts_unique_id: Vec<usize>,
    imported_contacts_pos: Vec<usize>,
    /// `imported_contact_user_ids_` / `unimported_contact_invites_`
    /// (UserManager.h:1400-1401) — the `change_imported_contacts`
    /// results.
    imported_contact_user_ids: Vec<UserId>,
    unimported_contact_invites: Vec<i32>,

    /// `contact_birthdates_` (UserManager.h:1388-1393).
    contact_birthdates: ContactBirthdates,
}

impl UserManager {
    pub fn new() -> UserManager {
        UserManager {
            saved_contact_count: -1,
            ..UserManager::default()
        }
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
    /// half of the block is [`init_freeze_state`](Self::init_freeze_state).
    /// `is_online` is `OnlineManager::is_online()` — `false` until its
    /// step lands.
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

    /// The constructor's freeze-state restore (UserManager.cpp:2773-2781):
    /// the binlog pmc `freeze_state` string `"since until appeal"` (the
    /// C++ two `split`s) plus the `updateFreezeState` emission on a
    /// non-empty value.
    pub fn init_freeze_state(&mut self, binlog_pmc: &mut BinlogKeyValue) {
        let log_event_string = binlog_pmc.get("freeze_state");
        if !log_event_string.is_empty() {
            let (freeze_since_date, rest) = split_first_space(&log_event_string);
            let (freeze_until_date, freeze_appeal_url) = split_first_space(&rest);
            self.freeze_since_date = freeze_since_date.parse().unwrap_or(0);
            self.freeze_until_date = freeze_until_date.parse().unwrap_or(0);
            self.freeze_appeal_url = freeze_appeal_url;
            let update = self.get_update_freeze_state_object();
            self.send_update(td_api::Update::updateFreezeState(update));
        }
    }

    /// `UserManager::get_my_id` (UserManager.cpp:2994-2997).
    pub fn get_my_id(&self) -> UserId {
        if !self.my_id.is_valid() {
            tdutils::log_error!(at "UserManager.cpp", 2995, "Wrong or unknown my ID returned");
        }
        self.my_id
    }

    /// `UserManager::on_update_my_user_gift_settings`
    /// (UserManager.cpp:4253-4263) — the `setGiftSettings` chain's
    /// local half; the full-info flip is the only real effect and the
    /// promise answers ok (the C++ caller rides the
    /// `account.setGlobalPrivacySettings` query result).
    pub fn on_update_my_user_gift_settings(
        &mut self,
        env: &mut UserEnv,
        gift_settings: crate::star_gift_settings::StarGiftSettings,
        mut promise: Promise<()>,
    ) {
        // TRY_STATUS_PROMISE(promise, G()->close_status()) — the collapsed
        // close gate drops the promise (the "Lost promise" convention).
        if env.global.close_flag() {
            return;
        }
        let user_id = self.get_my_id();
        if self.get_user_full_force(env, user_id, "on_update_my_user_gift_settings") {
            let is_changed = {
                let user_full = self.users_full.get_mut(&user_id.get()).unwrap();
                if user_full.gift_settings != gift_settings {
                    user_full.gift_settings = gift_settings;
                    user_full.is_changed = true;
                    true
                } else {
                    false
                }
            };
            if is_changed {
                self.update_user_full(env, user_id, "on_update_my_user_gift_settings", false);
            }
        }
        promise.set_value(());
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

    /// `UserManager::have_user_force` (UserManager.cpp:5016-5018) — the
    /// boolean form of `get_user_force`; the bot-info/get_me call sites
    /// land with the later M.4 slices.
    pub fn have_user_force(&mut self, env: &mut UserEnv, user_id: UserId, source: &str) -> bool {
        self.get_user_force(env, user_id, source)
    }

    /// `UserManager::add_service_notifications_user`
    /// (UserManager.cpp:3023-3029).
    pub fn add_service_notifications_user(&mut self, env: &mut UserEnv) -> UserId {
        let user_id = Self::get_service_notifications_user_id();
        if !self.have_user_force(env, user_id, "add_service_notifications_user") {
            tdutils::log_fatal!("Failed to load service notification user");
        }
        user_id
    }

    /// `UserManager::add_anonymous_bot_user` (UserManager.cpp:3051-3057).
    pub fn add_anonymous_bot_user(&mut self, env: &mut UserEnv) -> UserId {
        let user_id = Self::get_anonymous_bot_user_id(env.global.is_test_dc());
        if !self.have_user_force(env, user_id, "add_anonymous_bot_user") {
            tdutils::log_fatal!("Failed to load anonymous bot user");
        }
        user_id
    }

    /// `UserManager::add_channel_bot_user` (UserManager.cpp:3059-3065).
    pub fn add_channel_bot_user(&mut self, env: &mut UserEnv) -> UserId {
        let user_id = Self::get_channel_bot_user_id(env.global.is_test_dc());
        if !self.have_user_force(env, user_id, "add_channel_bot_user") {
            tdutils::log_fatal!("Failed to load channel bot user");
        }
        user_id
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

    /// `get_user_linked_community_id` (UserManager.cpp:5481-5487).
    pub fn get_user_linked_community_id(&self, user_id: UserId) -> CommunityId {
        self.get_user(user_id)
            .map_or_else(CommunityId::default, |user| user.linked_community_id)
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

    /// `UserManager::have_accessible_user` (UserManager.cpp:4824-4828):
    /// `u != nullptr && u->is_received` — a registered but not-yet-received
    /// user is not accessible.
    pub fn have_accessible_user(&self, user_id: UserId) -> bool {
        self.get_user(user_id).is_some_and(|u| u.is_received)
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

    /// `UserManager::on_binlog_secret_chat_event` (UserManager.cpp:3490-3518):
    /// the crash-window replay — restore the secret chat from its binlog
    /// mirror and keep the event id so the save rewrites (not duplicates)
    /// it.
    pub fn on_binlog_secret_chat_event(
        &mut self,
        env: &mut UserEnv,
        event: &tddb::binlog::event::BinlogEvent,
    ) {
        let binlog = env.global.td_db().binlog();
        if !env.global.use_chat_info_database() {
            binlog.erase(event.id);
            return;
        }

        let parsed = parse_secret_chat_log_event(event.get_data());
        let (secret_chat_id, secret_chat) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                tdutils::log_error!("Failed to load a secret chat from binlog: {error}");
                binlog.erase(event.id);
                return;
            }
        };
        if self.have_secret_chat(secret_chat_id) || !secret_chat_id.is_valid() {
            tdutils::log_error!("Skip adding already added {secret_chat_id:?}");
            binlog.erase(event.id);
            return;
        }

        let c = self.add_secret_chat(secret_chat_id);
        *c = secret_chat;
        let c = self.secret_chats.get_mut(&secret_chat_id.get()).unwrap();
        c.log_event_id = event.id;

        self.update_secret_chat(env, secret_chat_id, true, false);
    }

    // The secret-chat storage half (cpp:9609-9871 + the diff machine
    // cpp:10084-10126 + the read getters cpp:5785-5838).

    /// `UserManager::have_secret_chat` (UserManager.cpp:9609-9611).
    pub fn have_secret_chat(&self, secret_chat_id: SecretChatId) -> bool {
        self.secret_chats.contains_key(&secret_chat_id.get())
    }

    /// `UserManager::get_secret_chat` (UserManager.cpp:9613-9619).
    pub fn get_secret_chat(&self, secret_chat_id: SecretChatId) -> Option<&SecretChat> {
        self.secret_chats.get(&secret_chat_id.get())
    }

    /// `UserManager::add_secret_chat` (UserManager.cpp:9621-9628).
    fn add_secret_chat(&mut self, secret_chat_id: SecretChatId) -> &mut SecretChat {
        assert!(secret_chat_id.is_valid());
        self.secret_chats.entry(secret_chat_id.get()).or_default()
    }

    /// `UserManager::have_secret_chat_force` (UserManager.cpp:9630-9632).
    pub fn have_secret_chat_force(
        &mut self,
        env: &mut UserEnv,
        secret_chat_id: SecretChatId,
        source: &str,
    ) -> bool {
        self.get_secret_chat_force(env, secret_chat_id, source)
    }

    /// `UserManager::get_secret_chat_force` (UserManager.cpp:9634-9657) —
    /// the cache hit also force-loads the chat's user; the miss falls
    /// through the synchronous database load. Returns whether the secret
    /// chat is now in the cache.
    pub fn get_secret_chat_force(
        &mut self,
        env: &mut UserEnv,
        secret_chat_id: SecretChatId,
        source: &str,
    ) -> bool {
        if !secret_chat_id.is_valid() {
            return false;
        }
        if let Some(c) = self.secret_chats.get(&secret_chat_id.get()) {
            let user_id = c.user_id;
            if !self.get_user_force(env, user_id, source) {
                tdutils::log_error!("Can't find {user_id:?} from {secret_chat_id:?} from {source}");
            }
            return true;
        }
        if !env.global.use_chat_info_database() {
            return false;
        }
        if self
            .loaded_from_database_secret_chats
            .contains(&secret_chat_id.get())
        {
            return false;
        }

        tdutils::log_info!("Trying to load {secret_chat_id:?} from database from {source}");
        self.load_secret_chat_from_database(env, secret_chat_id, true);
        self.secret_chats.contains_key(&secret_chat_id.get())
    }

    /// `UserManager::get_secret_chat_database_key` (UserManager.cpp:9701-9703).
    fn get_secret_chat_database_key(secret_chat_id: SecretChatId) -> Vec<u8> {
        format!("sc{}", secret_chat_id.get()).into_bytes()
    }

    /// `UserManager::save_secret_chat` (UserManager.cpp:9680-9699): while
    /// the secret chat is unsaved, mirror it into the binlog
    /// (`binlog_add` the first time, `binlog_rewrite` after) with the
    /// SecretChatInfos handler type, then push the row into the "common"
    /// kv.
    fn save_secret_chat(
        &mut self,
        env: &mut UserEnv,
        secret_chat_id: SecretChatId,
        from_binlog: bool,
    ) {
        if !env.global.use_chat_info_database() {
            return;
        }
        let need_save = match self.secret_chats.get(&secret_chat_id.get()) {
            Some(c) => !c.is_saved,
            None => return,
        };
        if need_save {
            if !from_binlog {
                let data = {
                    let c = self.secret_chats.get(&secret_chat_id.get()).unwrap();
                    let mut data = Vec::new();
                    store_secret_chat_log_event(&mut data, secret_chat_id, c);
                    data
                };
                let binlog = env.global.td_db().binlog();
                let new_id = if self.secret_chats[&secret_chat_id.get()].log_event_id == 0 {
                    binlog.add(tddb::td_db::LOG_EVENT_HANDLER_TYPE_SECRET_CHAT_INFOS, &data)
                } else {
                    binlog.rewrite(
                        self.secret_chats[&secret_chat_id.get()].log_event_id,
                        tddb::td_db::LOG_EVENT_HANDLER_TYPE_SECRET_CHAT_INFOS,
                        &data,
                    )
                };
                self.secret_chats
                    .get_mut(&secret_chat_id.get())
                    .unwrap()
                    .log_event_id = new_id;
            }

            self.save_secret_chat_to_database(env, secret_chat_id);
        }
    }

    /// `UserManager::save_secret_chat_to_database` + `_impl` +
    /// `on_save_secret_chat_to_database` (UserManager.cpp:9709-9763)
    /// collapsed onto the synchronous kv — the same divergence as
    /// `save_user_to_database`: the C++ guards order against the async pmc
    /// and cannot interleave here, and the write is infallible, so the
    /// failure branch (`is_saved = false` + re-save) is unreachable.
    fn save_secret_chat_to_database(&mut self, env: &mut UserEnv, secret_chat_id: SecretChatId) {
        let value = {
            let Some(c) = self.secret_chats.get_mut(&secret_chat_id.get()) else {
                return;
            };
            if c.is_being_saved {
                return;
            }
            c.is_being_saved = true;
            c.is_saved = true;
            let mut value = Vec::new();
            c.store_value(&mut value);
            value
        };
        env.global
            .td_db()
            .common_kv()
            .set(&Self::get_secret_chat_database_key(secret_chat_id), &value);
        let c = self.secret_chats.get_mut(&secret_chat_id.get()).unwrap();
        c.is_being_saved = false;
        // `on_save_secret_chat_to_database`: the row is durable, retire the
        // binlog mirror.
        if c.log_event_id != 0 {
            let log_event_id = c.log_event_id;
            env.global.td_db().binlog().erase(log_event_id);
            c.log_event_id = 0;
        }
    }

    /// `UserManager::load_secret_chat_from_database` + `_impl` +
    /// `on_load_secret_chat_from_database` (UserManager.cpp:9766-9843)
    /// collapsed onto the synchronous kv (the per-id promise queue exists
    /// only to batch the async completions). A loaded-but-empty row means
    /// the secret chat is simply absent; a raced in-memory secret chat
    /// keeps the newer memory state, or retires the mirror when equal.
    fn load_secret_chat_from_database(
        &mut self,
        env: &mut UserEnv,
        secret_chat_id: SecretChatId,
        force: bool,
    ) {
        if env.global.close_flag() && !force {
            return;
        }
        if !self
            .loaded_from_database_secret_chats
            .insert(secret_chat_id.get())
        {
            return;
        }
        let value = env
            .global
            .td_db()
            .common_kv()
            .get(&Self::get_secret_chat_database_key(secret_chat_id));

        // The chat that reaches the final user check (the C++ runs it for
        // every branch where `c != nullptr` after the load, cpp:9838).
        let mut loaded_user_id = None;
        if let Some(c) = self.secret_chats.get_mut(&secret_chat_id.get()) {
            // The C++ races an in-memory secret chat against the async
            // load; here the only entry is a binlog-replayed secret chat:
            // keep the newer memory state if it differs from the row, else
            // retire the mirror.
            let user_id = c.user_id;
            let mut new_value = Vec::new();
            c.store_value(&mut new_value);
            if value == new_value {
                if c.log_event_id != 0 {
                    let log_event_id = c.log_event_id;
                    env.global.td_db().binlog().erase(log_event_id);
                    c.log_event_id = 0;
                }
            } else {
                self.save_secret_chat_to_database(env, secret_chat_id);
            }
            loaded_user_id = Some(user_id);
        } else if value.is_empty() {
            return;
        } else {
            match SecretChat::parse_value(&value) {
                Ok(secret_chat) => {
                    let user_id = secret_chat.user_id;
                    let c = self.add_secret_chat(secret_chat_id);
                    *c = secret_chat;
                    let c = self.secret_chats.get_mut(&secret_chat_id.get()).unwrap();
                    c.is_saved = true;
                    self.update_secret_chat(env, secret_chat_id, true, true);
                    loaded_user_id = Some(user_id);
                }
                Err(error) => {
                    tdutils::log_error!("Failed to load {secret_chat_id:?} from database: {error}");
                    self.secret_chats.remove(&secret_chat_id.get());
                }
            }
        }
        // The C++'s `have_user_force` check — it force-loads the owner
        // (database rows / special-user synthesis) exactly like this.
        if let Some(user_id) = loaded_user_id
            && !self.get_user_force(env, user_id, "on_load_secret_chat_from_database")
        {
            tdutils::log_error!("Can't find {user_id:?} from {secret_chat_id:?}");
        }
    }

    /// `UserManager::on_update_secret_chat` (UserManager.cpp:4714-4766) —
    /// the intake: applies the delta against the cached record with the
    /// C++'s change-flag bookkeeping (`access_hash`/`ttl`/`date` dirty the
    /// database value, the rest only the update), maintains the
    /// `secret_chats_with_user` index, then runs the diff machine.
    #[allow(clippy::too_many_arguments)] // the C++ intake has the same shape
    pub fn on_update_secret_chat(
        &mut self,
        env: &mut UserEnv,
        secret_chat_id: SecretChatId,
        access_hash: i64,
        user_id: UserId,
        state: SecretChatState,
        is_outbound: bool,
        ttl: i32,
        date: i32,
        mut key_hash: String,
        layer: i32,
        initial_folder_id: i32,
    ) {
        tdutils::log_info!(
            "Update {secret_chat_id:?} with {user_id:?} and access_hash {access_hash}"
        );
        // The access-hash delta needs no cross-field access; the user-id
        // branch does (the `secret_chats_with_user` index), so it runs
        // between the two field passes.
        let old_user_id = {
            let secret_chat = self.add_secret_chat(secret_chat_id);
            if access_hash != secret_chat.access_hash {
                secret_chat.access_hash = access_hash;
                secret_chat.need_save_to_database = true;
            }
            let old_user_id = secret_chat.user_id;
            if user_id.is_valid() && user_id != old_user_id {
                secret_chat.user_id = user_id;
                secret_chat.is_changed = true;
                Some(old_user_id)
            } else {
                None
            }
        };
        if let Some(old_user_id) = old_user_id {
            if old_user_id.is_valid() {
                tdutils::log_error!(
                    "Secret chat user has changed from {old_user_id:?} to {user_id:?}"
                );
            }
            // The C++ indexes through `operator[]` (creating empty lists);
            // an absent list and an empty list are indistinguishable here.
            self.secret_chats_with_user
                .entry(old_user_id.get())
                .or_default()
                .retain(|&id| id != secret_chat_id.get());
            self.secret_chats_with_user
                .entry(user_id.get())
                .or_default()
                .push(secret_chat_id.get());
        }
        {
            let secret_chat = self.add_secret_chat(secret_chat_id);
            if state != SecretChatState::Unknown && state != secret_chat.state {
                secret_chat.state = state;
                secret_chat.is_changed = true;
                secret_chat.is_state_changed = true;
            }
            if is_outbound != secret_chat.is_outbound {
                secret_chat.is_outbound = is_outbound;
                secret_chat.is_changed = true;
            }

            if ttl != -1 && ttl != secret_chat.ttl {
                secret_chat.ttl = ttl;
                secret_chat.need_save_to_database = true;
                secret_chat.is_ttl_changed = true;
            }
            if date != 0 && date != secret_chat.date {
                secret_chat.date = date;
                secret_chat.need_save_to_database = true;
            }
            if !key_hash.is_empty() && key_hash != secret_chat.key_hash {
                secret_chat.key_hash = std::mem::take(&mut key_hash);
                secret_chat.is_changed = true;
            }
            if layer != 0 && layer != secret_chat.layer {
                secret_chat.layer = layer;
                secret_chat.is_changed = true;
            }
            if initial_folder_id != 0 && initial_folder_id != secret_chat.initial_folder_id {
                secret_chat.initial_folder_id = initial_folder_id;
                secret_chat.is_changed = true;
            }
        }

        self.update_secret_chat(env, secret_chat_id, false, false);
    }

    /// `UserManager::update_secret_chat` (UserManager.cpp:10084-10126) —
    /// the diff machine: fold `is_changed` into the save, notify the
    /// MessagesManager/DialogManager consumers (documented no-ops until
    /// those managers land), emit `updateSecretChat`, then save.
    fn update_secret_chat(
        &mut self,
        env: &mut UserEnv,
        secret_chat_id: SecretChatId,
        from_binlog: bool,
        from_database: bool,
    ) {
        if self
            .secret_chats
            .get(&secret_chat_id.get())
            .unwrap()
            .is_being_updated
        {
            tdutils::log_error!("Detected recursive update of {secret_chat_id:?}");
        }
        self.secret_chats
            .get_mut(&secret_chat_id.get())
            .unwrap()
            .is_being_updated = true;
        self.update_secret_chat_impl(env, secret_chat_id, from_binlog, from_database);
        if let Some(c) = self.secret_chats.get_mut(&secret_chat_id.get()) {
            c.is_being_updated = false;
        }
    }

    fn update_secret_chat_impl(
        &mut self,
        env: &mut UserEnv,
        secret_chat_id: SecretChatId,
        from_binlog: bool,
        from_database: bool,
    ) {
        {
            let c = self.secret_chats.get_mut(&secret_chat_id.get()).unwrap();
            c.need_save_to_database |= c.is_changed;
            if c.need_save_to_database {
                if !from_database {
                    c.is_saved = false;
                }
                c.need_save_to_database = false;

                // MessagesManager::force_create_dialog(DialogId(secret_chat_id))
                // — with the MessagesManager step ("update secret chat").
                if c.is_state_changed {
                    // MessagesManager::on_update_secret_chat_state — with
                    // its step.
                    c.is_state_changed = false;
                }
                if c.is_ttl_changed {
                    // MessagesManager::on_update_dialog_message_ttl — with
                    // its step.
                    c.is_ttl_changed = false;
                }
            }
        }
        let is_changed = self
            .secret_chats
            .get(&secret_chat_id.get())
            .unwrap()
            .is_changed;
        if is_changed {
            let secret_chat_object =
                make_secret_chat_object(secret_chat_id, &self.secret_chats[&secret_chat_id.get()]);
            self.send_update(td_api::Update::updateSecretChat(td_api::updateSecretChat {
                secret_chat: Some(secret_chat_object),
            }));
            let c = self.secret_chats.get_mut(&secret_chat_id.get()).unwrap();
            c.is_changed = false;
        }

        if !from_database {
            self.save_secret_chat(env, secret_chat_id, from_binlog);
        }
    }

    /// `UserManager::for_each_secret_chat_with_user`
    /// (UserManager.cpp:5764-5771).
    pub fn for_each_secret_chat_with_user(&self, user_id: UserId) -> Vec<SecretChatId> {
        self.secret_chats_with_user
            .get(&user_id.get())
            .map(|ids| ids.iter().map(|&id| SecretChatId::new(id)).collect())
            .unwrap_or_default()
    }

    /// `UserManager::get_secret_chat_date` (UserManager.cpp:5785-5791).
    pub fn get_secret_chat_date(&self, secret_chat_id: SecretChatId) -> i32 {
        self.get_secret_chat(secret_chat_id)
            .map(|c| c.date)
            .unwrap_or(0)
    }

    /// `UserManager::get_secret_chat_ttl` (UserManager.cpp:5793-5799).
    pub fn get_secret_chat_ttl(&self, secret_chat_id: SecretChatId) -> i32 {
        self.get_secret_chat(secret_chat_id)
            .map(|c| c.ttl)
            .unwrap_or(0)
    }

    /// `UserManager::get_secret_chat_user_id` (UserManager.cpp:5801-5807).
    pub fn get_secret_chat_user_id(&self, secret_chat_id: SecretChatId) -> UserId {
        self.get_secret_chat(secret_chat_id)
            .map(|c| c.user_id)
            .unwrap_or_default()
    }

    /// `UserManager::get_secret_chat_is_outbound`
    /// (UserManager.cpp:5809-5815).
    pub fn get_secret_chat_is_outbound(&self, secret_chat_id: SecretChatId) -> bool {
        self.get_secret_chat(secret_chat_id)
            .map(|c| c.is_outbound)
            .unwrap_or(false)
    }

    /// `UserManager::get_secret_chat_state` (UserManager.cpp:5817-5823).
    pub fn get_secret_chat_state(&self, secret_chat_id: SecretChatId) -> SecretChatState {
        self.get_secret_chat(secret_chat_id)
            .map(|c| c.state)
            .unwrap_or(SecretChatState::Unknown)
    }

    /// `UserManager::get_secret_chat_layer` (UserManager.cpp:5825-5831).
    pub fn get_secret_chat_layer(&self, secret_chat_id: SecretChatId) -> i32 {
        self.get_secret_chat(secret_chat_id)
            .map(|c| c.layer)
            .unwrap_or(0)
    }

    /// `UserManager::get_secret_chat_initial_folder_id`
    /// (UserManager.cpp:5833-5839); `FolderId::main()` is `FolderId()`.
    pub fn get_secret_chat_initial_folder_id(&self, secret_chat_id: SecretChatId) -> i32 {
        self.get_secret_chat(secret_chat_id)
            .map(|c| c.initial_folder_id)
            .unwrap_or(0)
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
            // `CHECK(u != nullptr && u->is_received)`
            // (UserManager.cpp:5113) — the synthesis always parses.
            loaded = self
                .users
                .get(&user_id.get())
                .is_some_and(|u| u.is_received);
            assert!(
                loaded,
                "the synthetic special user {user_id:?} must be received"
            );
            // `reload_user(user_id, Promise<Unit>(), "get_user_force")`
            // (cpp:5115) — refreshes the synthesized special user from
            // the server
            self.reload_user(env, user_id, Promise::default(), "get_user_force");
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
            user.photo = Some(telegram_api::UserProfilePhoto::userProfilePhoto(
                telegram_api::userProfilePhoto {
                    flags: 0,
                    has_video: false,
                    personal: false,
                    photo_id: profile_photo_id,
                    stripped_thumb: Vec::new(),
                    dc_id: profile_photo_dc_id,
                },
            ));
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
            } else if is_contact && !self.are_contacts_loaded {
                // preload contact users from database to know that
                // is_contact didn't change and the list of contacts
                // doesn't need to be saved to the database
                let preloaded = self.get_user_force_impl(env, user_id, "on_get_user 3");
                if !preloaded {
                    tdutils::log_info!("Receive contact {user_id:?} for the first time");
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
        // cpp:3247-3250 — the sidebar is gated by the forum view (with
        // the error band for the ungated wire form).
        let mut has_bot_forum_sidebar = has_bot_forum_view && user.bot_forum_sidebar;
        if user.bot_forum_sidebar && !has_bot_forum_view {
            tdutils::log_error!(
                "Receive forum sidebar without forum view for {user_id:?} from {source}"
            );
        }
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
        let stories_available = (user.flags2 & USER_STORIES_MAX_ID_MASK) != 0
            && user
                .stories_max_id
                .as_ref()
                .is_some_and(|stories_max_id| stories_max_id.max_id > 0);
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
            has_bot_forum_sidebar = false;
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
                || has_bot_forum_sidebar != u.has_bot_forum_sidebar
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
                u.has_bot_forum_sidebar = has_bot_forum_sidebar;
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
                // the changed facts retire the cached contact price
                self.user_full_contact_price.remove(&user_id.get());
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
            self.on_update_user_photo(env, user_id, photo.unwrap_or_default(), source);
        }
        if is_me_regular_user {
            if is_received || !self.u_is_received(user_id) {
                let phone = std::mem::take(&mut user.phone);
                self.on_update_user_phone_number_impl(env, user_id, phone);
            }
            if is_received || !self.u_is_received(user_id) || self.u_was_online(user_id) == 0 {
                let status = std::mem::take(&mut user.status);
                self.on_update_user_online_impl(env, user_id, status.unwrap_or_default());
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
        let emoji_status = EmojiStatus::get_emoji_status(
            std::mem::take(&mut user.emoji_status).unwrap_or_default(),
        );
        self.on_update_user_emoji_status_impl(user_id, emoji_status);

        if matches!(
            user.color,
            Some(telegram_api::PeerColor::peerColorCollectible(_))
        ) {
            let Some(telegram_api::PeerColor::peerColorCollectible(collectible)) = user
                .color
                .replace(telegram_api::PeerColor::peerColor(Default::default()))
            else {
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
            let peer_color = user
                .color
                .as_ref()
                .map(PeerColor::from_telegram)
                .unwrap_or_default();
            self.on_update_user_colors_impl(
                user_id,
                peer_color.accent_color_id(),
                peer_color.background_custom_emoji_id(),
                None,
            );
        }
        let profile_peer_color = user
            .profile_color
            .as_ref()
            .map(PeerColor::from_telegram)
            .unwrap_or_default();
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
                let (live, max_id) = user
                    .stories_max_id
                    .as_ref()
                    .map(|stories_max_id| (stories_max_id.live, stories_max_id.max_id))
                    .unwrap_or((false, 0));
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

    /// `UserManager::on_update_user_photo` (UserManager.cpp:3615-3646):
    /// the bot-without-chat-info-database path holds the photo in
    /// `pending_user_photos_` until `apply_pending_user_photo`, then
    /// suppresses no-op redeliveries for received users.
    pub fn on_update_user_photo(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        photo: telegram_api::UserProfilePhoto,
        source: &str,
    ) {
        if env.auth_is_bot && !env.global.use_chat_info_database() {
            let is_photo_inited = self
                .users
                .get(&user_id.get())
                .map(|u| u.is_photo_inited)
                .unwrap_or(false);
            if !is_photo_inited {
                let new_photo_id = get_profile_photo_id(&photo);
                let old_photo_id = self
                    .pending_user_photos
                    .get(&user_id.get())
                    .map(get_profile_photo_id)
                    .unwrap_or(0);
                if new_photo_id == old_photo_id {
                    return;
                }
                let mut photo = photo;
                if let telegram_api::UserProfilePhoto::userProfilePhoto(profile_photo) = &mut photo
                {
                    profile_photo.stripped_thumb.clear();
                }

                self.pending_user_photos.insert(user_id.get(), photo);

                self.drop_user_photos(user_id, new_photo_id == 0, "on_update_user_photo");
                // must not load UserFull
                if self.get_user_full(user_id).is_some()
                    && new_photo_id != self.get_user_full_profile_photo_id_from_cache(user_id)
                {
                    // we didn't send updateUser yet, so we must not send
                    // updateUserFull with new_photo_id yet
                    self.drop_user_full_photos(env, user_id, 0, "on_update_user_photo");
                }
                return;
            }
            let is_received = self
                .users
                .get(&user_id.get())
                .map(|u| u.is_received)
                .unwrap_or(false);
            if is_received {
                let new_photo_id = get_profile_photo_id(&photo);
                let current_id = self
                    .users
                    .get(&user_id.get())
                    .map(|u| u.photo.id)
                    .unwrap_or(0);
                if new_photo_id == current_id {
                    return;
                }
            }
        }

        self.do_update_user_photo_from_telegram(env, user_id, photo, source);
    }

    /// `UserManager::do_update_user_photo` (the telegram_photo overload,
    /// UserManager.cpp:3668-3673): conversion plus the bots'
    /// minithumbnail wipe.
    fn do_update_user_photo_from_telegram(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        photo: telegram_api::UserProfilePhoto,
        source: &str,
    ) {
        let mut new_photo = get_profile_photo(photo);
        if env.auth_is_bot {
            new_photo.minithumbnail.clear();
        }
        self.do_update_user_photo(env, user_id, new_photo, true, source);
    }

    /// `UserManager::do_update_user_photo` (the ProfilePhoto overload,
    /// UserManager.cpp:3675-3687).
    fn do_update_user_photo(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        new_photo: ProfilePhoto,
        invalidate_photo_cache: bool,
        source: &str,
    ) {
        let new_photo_id;
        let photo_changed = {
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
                true
            } else if need_update_dialog_photo_minithumbnail(
                &u.photo.minithumbnail,
                &new_photo.minithumbnail,
            ) {
                u.photo.minithumbnail = new_photo.minithumbnail;
                u.is_photo_changed = true;
                u.is_changed = true;
                false
            } else {
                false
            }
        };
        if photo_changed {
            let u = self.users.get(&user_id.get()).unwrap();
            new_photo_id = u.photo.id;
            if invalidate_photo_cache {
                self.drop_user_photos(user_id, new_photo_id == 0, source);
            }
            // must not load UserFull
            if self.get_user_full(user_id).is_some()
                && new_photo_id != self.get_user_full_profile_photo_id_from_cache(user_id)
            {
                // we didn't send updateUser yet, so we must not send
                // updateUserFull with u->photo.id yet
                self.drop_user_full_photos(env, user_id, 0, "do_update_user_photo");
            }
        }
    }

    /// `get_user_full_profile_photo_id` over the cached record (the
    /// caller checked presence).
    fn get_user_full_profile_photo_id_from_cache(&self, user_id: UserId) -> i64 {
        Self::get_user_full_profile_photo_id(self.users_full.get(&user_id.get()).unwrap())
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
        let mut reload_contact_birthdates = false;
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
                reload_contact_birthdates = true;
            }
            u.is_close_friend = is_close_friend;
            u.is_changed = true;
        }
        if reload_contact_birthdates {
            self.reload_contact_birthdates(env, true);
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

    /// `UserManager::get_bot_commands` (UserManager.cpp:5841-5883) — the
    /// bot-info command extraction behind the chatFull/channelFull
    /// intakes. `participants` filters channel bots to chat members.
    pub fn get_bot_commands(
        &mut self,
        env: &mut UserEnv,
        bot_infos: Vec<telegram_api::BotInfo>,
        participants: Option<&[crate::chat_manager::DialogParticipant]>,
    ) -> Vec<crate::chat_manager::BotCommands> {
        let mut result = Vec::new();
        if env.auth_is_bot {
            return result;
        }
        for bot_info in bot_infos {
            if bot_info.commands.is_empty() {
                continue;
            }
            let user_id = UserId::new(bot_info.user_id);
            if !self.get_user_force(env, user_id, "get_bot_commands") {
                tdutils::log_error!("Receive unknown {user_id:?}");
                continue;
            }
            if !self.is_user_bot_of(user_id) {
                if !self.is_user_deleted_of(user_id) {
                    tdutils::log_error!("Receive non-bot {user_id:?}");
                }
                continue;
            }
            if let Some(participants) = participants
                && !participants
                    .iter()
                    .any(|p| p.dialog_id_ == crate::chat_manager::DialogId::from_user_id(user_id))
            {
                tdutils::log_error!("Skip commands of non-member bot {user_id:?}");
                continue;
            }
            result.push(crate::chat_manager::BotCommands::from_telegram(
                user_id,
                bot_info.commands,
            ));
        }
        result
    }

    /// `UserManager::have_input_peer_user` (UserManager.cpp:5271-5300);
    /// the `user_messages_` admission landed with M.4.15.
    pub fn have_input_peer_user(
        &self,
        env: &UserEnv,
        user_id: UserId,
        access_rights: AccessRights,
    ) -> bool {
        self.have_input_peer_user_for(
            self.get_user(user_id),
            user_id,
            access_rights,
            env.auth_is_bot,
        )
    }

    /// `UserManager::have_input_peer_user(const User *, ...)`
    /// (UserManager.cpp:5271-5286); the registry admission is the
    /// `user_messages_.count(user_id) != 0` check.
    pub(crate) fn have_input_peer_user_for(
        &self,
        u: Option<&User>,
        user_id: UserId,
        access_rights: AccessRights,
        auth_is_bot: bool,
    ) -> bool {
        if user_id == self.my_id {
            return true;
        }
        let Some(u) = u else {
            if auth_is_bot && user_id.is_valid() {
                return true;
            }
            return self.user_messages.contains_key(&user_id.get());
        };
        if u.access_hash == -1 || u.is_min_access_hash {
            if auth_is_bot && user_id.is_valid() {
                return true;
            }
            return self.user_messages.contains_key(&user_id.get());
        }
        match access_rights {
            AccessRights::Know | AccessRights::Read => true,
            AccessRights::Write | AccessRights::Edit => !u.is_deleted,
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

    /// `UserManager::get_user_search_text` (UserManager.cpp:5759-5762) —
    /// first name, last name and ALL active usernames, space-joined (the
    /// C++ `implode`).
    pub fn get_user_search_text_of(&self, user_id: UserId) -> String {
        let Some(u) = self.get_user(user_id) else {
            return String::new();
        };
        format!(
            "{} {} {}",
            u.first_name,
            u.last_name,
            u.usernames.get_active_usernames().join(" ")
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
            status: Some(status),
        }));
        // DialogParticipantManager::update_user_online_member_count
    }

    /// Drive the online/emoji-status/noforwards/rating deadlines (the
    /// MultiTimeout replacement); the Td actor polls this on its tick.
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
        let expired: Vec<i32> = self
            .noforwards_request_timeouts
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for request_id in expired {
            self.noforwards_request_timeouts.remove(&request_id);
            self.on_noforwards_request_timeout(env, request_id);
        }
        let expired: Vec<i64> = self
            .user_rating_timeouts
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for user_id in expired {
            self.user_rating_timeouts.remove(&user_id);
            self.on_user_rating_timeout(env, UserId::new(user_id));
        }
    }

    /// The earliest armed deadline (the member MultiTimeouts' next firing
    /// time), or `None` when nothing is armed — the Td actor's re-arm key.
    pub(crate) fn next_deadline(&self) -> Option<f64> {
        self.user_online_timeouts
            .values()
            .chain(self.user_emoji_status_timeouts.values())
            .chain(self.noforwards_request_timeouts.values())
            .chain(self.user_rating_timeouts.values())
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
            // the C++ also clears is_username_changed here, unconditionally
            // of which of the three flags triggered the hints update — the
            // `take_user_flag(Username)` above is that clear
            self.update_contacts_hints(env, user_id, from_database);
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
                self.update_user_full(env, user_id, "update_user", false);
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
                user: Some(user_object),
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
                status: Some(status),
            }));
            let u = self.users.get_mut(&user_id.get()).unwrap();
            u.is_status_changed = false;
        }
        self.take_user_flag(user_id, UserFlag::OnlineStatus);
        // DialogParticipantManager::update_user_online_member_count

        if !from_database {
            self.save_user(env, user_id, from_binlog);
        }

        // The cache repair trigger (UserManager.cpp:10065-10071). The
        // `reload_user` half needs the users.getUsers query — the net
        // step; the one-shot `is_repaired` latch lands now so the C++
        // no-retrigger invariant holds already (and stays held once the
        // query sender arrives).
        {
            let needs_repair = {
                let u = self.users.get(&user_id.get()).unwrap();
                u.cache_version != USER_CACHE_VERSION && !u.is_repaired
            };
            if needs_repair
                && self.have_input_peer_user(env, user_id, AccessRights::Read)
                && !env.global.close_flag()
            {
                let u = self.users.get_mut(&user_id.get()).unwrap();
                u.is_repaired = true;
                tdutils::log_info!("Repairing cache of {user_id:?}");
                // `reload_user(user_id, Promise<Unit>(), "update_user")`
                // (cpp:10070)
                self.reload_user(env, user_id, Promise::default(), "update_user");
            }
        }

        if self.take_user_flag(user_id, UserFlag::FullInfo)
            && self.users_full.contains_key(&user_id.get())
        {
            let user_full = self.users_full.get_mut(&user_id.get()).unwrap();
            user_full.need_send_update = true;
            self.update_user_full(env, user_id, "update_user is_full_info_changed", false);
            // `reload_user_full(user_id, Promise<Unit>(), "update_user")`
            // (cpp:10077)
            self.reload_user_full(env, user_id, Promise::default(), "update_user");
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
    /// (UserManager.cpp:10128-10225).
    pub fn update_user_full(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        source: &str,
        from_database: bool,
    ) {
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
        self.update_user_full_impl(env, user_id, source, from_database);
        if let Some(user_full) = self.users_full.get_mut(&user_id.get()) {
            user_full.is_being_updated = false;
        }
    }

    /// `UserManager::on_update_bot_has_preview_medias`
    /// (UserManager.cpp:4589-4604) — the `GetPreviewMediasQuery` ok tail:
    /// flips the bot's `UserFull::has_preview_medias` and re-runs the
    /// full-update broadcast. The `on_update_user_full_has_preview_medias`
    /// static helper (cpp:4608-4614) folds inline.
    pub fn on_update_bot_has_preview_medias(
        &mut self,
        env: &mut UserEnv,
        bot_user_id: UserId,
        has_preview_medias: bool,
    ) {
        if !bot_user_id.is_valid() {
            tdutils::log_error!("Receive updateBotHasPreviewMedias about invalid {bot_user_id:?}");
            return;
        }
        if !self.have_user_force(env, bot_user_id, "on_update_bot_has_preview_medias")
            || !self.is_user_bot_of(bot_user_id)
        {
            return;
        }
        if env.auth_is_bot {
            return;
        }
        if self.get_user_full_force(env, bot_user_id, "on_update_bot_has_preview_medias") {
            {
                let user_full = self.users_full.get_mut(&bot_user_id.get()).unwrap();
                if user_full.has_preview_medias != has_preview_medias {
                    user_full.has_preview_medias = has_preview_medias;
                    user_full.is_changed = true;
                }
            }
            self.update_user_full(env, bot_user_id, "on_update_bot_has_preview_medias", false);
        }
    }

    fn update_user_full_impl(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        source: &str,
        from_database: bool,
    ) {
        self.unavailable_user_fulls.remove(&user_id.get()); // don't needed anymore
        // register_user_saved_music (cpp:10152-10154) runs after the
        // record borrow closes — it needs the env for the bot gate
        let mut pending_saved_music_registration: Option<FileId> = None;
        if let Some(user_full) = self.users_full.get_mut(&user_id.get()) {
            if user_full.is_common_chat_count_changed {
                user_full.is_common_chat_count_changed = false;
                // CommonDialogManager::drop_common_dialogs_cache — with
                // its step
            }
            // the star-rating timeout arm (cpp:10144-10151) — the deadline
            // plumbing landed with M.4.15, the arming fields and the
            // `is_pending_star_rating_changed` flag arrive with StarRating
            if user_full.is_first_saved_music_file_id_changed {
                user_full.is_first_saved_music_file_id_changed = false;
                pending_saved_music_registration = Some(user_full.first_saved_music_file_id);
            }
            if user_full.is_has_protected_content_changed {
                user_full.is_has_protected_content_changed = false;
                // MessagesManager::on_dialog_has_protected_content_updated
                // — with its step
            }
            // the file-source registration block — with the Files step
            user_full.need_send_update |= user_full.is_changed;
            user_full.need_save_to_database |= user_full.is_changed;
            user_full.is_changed = false;
        }
        if let Some(file_id) = pending_saved_music_registration {
            self.register_user_saved_music(env, user_id, file_id);
        }
        let (need_send_update, need_save_to_database) = {
            let user_full = self.users_full.get(&user_id.get()).unwrap();
            (user_full.need_send_update, user_full.need_save_to_database)
        };
        if need_send_update {
            // `CHECK(u == nullptr || u->is_update_user_sent)`
            // (UserManager.cpp:10204) — the full object is only announced
            // after the user object itself has been announced.
            assert!(
                self.users
                    .get(&user_id.get())
                    .is_none_or(|u| u.is_update_user_sent),
                "updateUser must precede updateUserFullInfo for {user_id:?}"
            );
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
                    user_full_info: Some(full_info),
                },
            ));
            let user_full = self.users_full.get_mut(&user_id.get()).unwrap();
            user_full.need_send_update = false;
            // the my-birthdate suggested-action dismissal — with the
            // SuggestedActions step
        }
        if need_save_to_database {
            if !from_database {
                self.save_user_full(env, user_id);
            }
            let user_full = self.users_full.get_mut(&user_id.get()).unwrap();
            user_full.need_save_to_database = false;
        }
    }

    /// `UserManager::on_get_user_full` (UserManager.cpp:8975-9343) — the
    /// supported subset of the `userFull` fields (module docs); the
    /// photos / bot-info / business / star-rating / gift-settings / note
    /// / stories parts of the telegram object are handled by their
    /// subsystems, the MessagesManager dialog fan-outs (notify settings,
    /// wallpaper, theme, pinned message, folder, ttl, dialog is_blocked,
    /// translatable), StoryManager and the personal-channel Broadcast
    /// type check (ChatManager) arrive with their waves.
    pub fn on_get_user_full(&mut self, env: &mut UserEnv, mut user_full: telegram_api::UserFull) {
        let user_id = UserId::new(user_full.id);
        if !user_id.is_valid() {
            tdutils::log_error!("Receive invalid {user_id:?}");
            return;
        }
        if !self.get_user_force(env, user_id, "on_get_user_full") {
            return;
        }
        // The u borrow covers the whole application below
        // (apply_pending_user_photo, cpp:8986)
        self.apply_pending_user_photo(env, user_id, "on_get_user_full");

        // The User facts the application reads (cpp:9112, 9140, 9235,
        // 9049-9051).
        let (is_premium_user, is_contact, user_linked_community_id, is_deleted_user, is_bot_user) = {
            let u = self.users.get(&user_id.get()).unwrap();
            (
                u.is_premium,
                u.is_contact,
                u.linked_community_id,
                u.is_deleted,
                u.is_bot,
            )
        };

        let flags = user_full.flags;
        let flags2 = user_full.flags2;
        let user_full_expires_at = tdutils::time::now() + USER_FULL_EXPIRE_TIME;

        let is_blocked = flags & 1 != 0;
        let is_blocked_for_stories = flags & (1 << 27) != 0;

        self.users_full.entry(user_id.get()).or_default();

        let about = std::mem::take(&mut user_full.about);
        let has_private_calls = flags & (1 << 5) != 0;
        let can_be_called = flags & (1 << 4) != 0 && !has_private_calls;
        let supports_video_calls = flags & (1 << 13) != 0 && !has_private_calls;
        let can_pin_messages = flags & (1 << 7) != 0;
        let voice_messages_forbidden = is_premium_user && flags & (1 << 20) != 0;
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

        // on_update_user_full_gift_count / _common_chat_count
        // (cpp:4241-4262, 4203-4223).
        let mut gift_count = user_full.stargifts_count;
        if gift_count < 0 {
            tdutils::log_error!("Receive {gift_count} as gift count with {user_id:?}");
            gift_count = 0;
        }
        let mut common_chat_count = user_full.common_chats_count;
        if common_chat_count < 0 {
            tdutils::log_error!(
                "Receive {common_chat_count} as common group count with {user_id:?}"
            );
            common_chat_count = 0;
        }
        // The Broadcast type check (cpp:9237-9240) needs the ChatManager.
        let personal_channel_id = if flags2 & (1 << 6) != 0 {
            user_full.personal_channel_id
        } else {
            0
        };
        // on_update_user_full_send_paid_message_stars (cpp:9034-9035).
        let send_paid_message_stars = get_star_count(user_full.send_paid_messages_stars);
        // on_update_user_full_charge_paid_message_stars (cpp:9032-9033).
        let charge_paid_message_stars = get_star_count(
            user_full
                .settings
                .as_ref()
                .map(|settings| settings.charge_paid_message_stars)
                .unwrap_or_default(),
        );
        // the gift-settings intake (cpp:9048-9051) — the deleted/bot
        // override forces the allow-nothing settings.
        let mut gift_settings = crate::star_gift_settings::StarGiftSettings::new(
            user_full.display_gifts_button,
            user_full.disallowed_gifts,
        );
        if is_deleted_user || is_bot_user {
            gift_settings = crate::star_gift_settings::StarGiftSettings::allow_nothing();
        }
        // on_update_user_full_need_phone_number_privacy_exception
        // (cpp:9029-9031 + hook cpp:4446-4459).
        let mut need_phone_number_privacy_exception = user_full
            .settings
            .as_ref()
            .map(|settings| settings.need_contacts_exception)
            .unwrap_or_default();
        if need_phone_number_privacy_exception
            && (is_deleted_user || is_contact || user_id == self.my_id)
        {
            need_phone_number_privacy_exception = false;
        }
        let birthdate = if flags2 & (1 << 5) != 0 {
            user_full
                .birthday
                .as_ref()
                .map(|birthday| Birthdate::new(birthday.day, birthday.month, birthday.year))
                .unwrap_or_default()
        } else {
            Birthdate::default()
        };
        let private_forward_name = std::mem::take(&mut user_full.private_forward_name);
        let linked_community_id = user_linked_community_id;
        // `get_profile_tab(user->main_tab_, ChannelType::Unknown)`
        // (cpp:9059-9060) — the disallowed band logs and answers Default
        // (ProfileTab.cpp:47-50).
        let main_profile_tab = crate::chat_manager::get_profile_tab(
            std::mem::take(&mut user_full.main_tab),
            ChannelType::Unknown,
        );

        // on_update_user_full_noforwards' User cache (61e454f02,
        // cpp:4534-4553): both update flags are set from this caller, so
        // the User record caches the noforwards state for the
        // has_protected_content accessors' fast path.
        {
            let need_save = {
                let u = self.users.get_mut(&user_id.get()).unwrap();
                if u.noforwards_my_enabled != noforwards_my_enabled {
                    u.noforwards_my_enabled = noforwards_my_enabled;
                    u.need_save_to_database = true;
                }
                if u.noforwards_peer_enabled != noforwards_peer_enabled {
                    u.noforwards_peer_enabled = noforwards_peer_enabled;
                    u.need_save_to_database = true;
                }
                if !u.is_noforwards_inited {
                    u.is_noforwards_inited = true;
                    u.need_save_to_database = true;
                }
                u.need_save_to_database
            };
            if need_save {
                self.update_user(env, user_id, false, false);
            }
        }

        {
            let stored = self.users_full.get_mut(&user_id.get()).unwrap();
            // expires_at is set unconditionally on every delivery
            // (cpp:9014-9015).
            stored.expires_at = user_full_expires_at;
            // on_update_user_full_is_blocked (cpp:9021 + hook
            // cpp:4157-4168); the MessagesManager dialog-is_blocked
            // fan-out (cpp:8999) arrives with its step.
            if stored.is_blocked != is_blocked
                || stored.is_blocked_for_stories != is_blocked_for_stories
            {
                stored.is_blocked = is_blocked;
                stored.is_blocked_for_stories = is_blocked_for_stories;
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
            // the business/location/away/greeting/intro/note hooks
            // (cpp:9024-9028, 9194-9201) — with their subsystems
            if stored.charge_paid_message_stars != charge_paid_message_stars {
                stored.charge_paid_message_stars = charge_paid_message_stars;
                stored.is_changed = true;
            }
            if stored.send_paid_message_stars != send_paid_message_stars {
                stored.send_paid_message_stars = send_paid_message_stars;
                stored.is_changed = true;
            }
            if stored.need_phone_number_privacy_exception != need_phone_number_privacy_exception {
                stored.need_phone_number_privacy_exception = need_phone_number_privacy_exception;
                stored.is_changed = true;
            }
            if stored.wallpaper_overridden != wallpaper_overridden {
                stored.wallpaper_overridden = wallpaper_overridden;
                stored.is_changed = true;
            }
            // on_update_user_full_noforwards (cpp:9037 + hook
            // cpp:4531-4556).
            let old_noforwards = stored.noforwards_my_enabled || stored.noforwards_peer_enabled;
            if stored.noforwards_my_enabled != noforwards_my_enabled {
                stored.noforwards_my_enabled = noforwards_my_enabled;
                stored.need_save_to_database = true;
            }
            if stored.noforwards_peer_enabled != noforwards_peer_enabled {
                stored.noforwards_peer_enabled = noforwards_peer_enabled;
                stored.need_save_to_database = true;
            }
            if old_noforwards != (stored.noforwards_my_enabled || stored.noforwards_peer_enabled) {
                stored.is_has_protected_content_changed = true;
            }
            // The can_pin block (cpp:9107-9134); bot_verification /
            // star_rating — with their subsystems.
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
            if stored.sponsored_enabled != sponsored_enabled {
                stored.sponsored_enabled = sponsored_enabled;
                stored.is_changed = true;
            }
            if stored.can_view_revenue != can_view_revenue {
                stored.can_view_revenue = can_view_revenue;
                stored.is_changed = true;
            }
            if stored.unofficial_security_risk != unofficial_security_risk {
                stored.unofficial_security_risk = unofficial_security_risk;
                stored.is_changed = true;
            }
            if stored.gift_settings != gift_settings {
                stored.gift_settings = gift_settings;
                stored.is_changed = true;
            }
            // the main_tab apply of the cpp:9060-9082 block
            if stored.main_profile_tab != main_profile_tab {
                stored.main_profile_tab = main_profile_tab;
                stored.is_changed = true;
            }
            if stored.birthdate != birthdate {
                stored.birthdate = birthdate;
                stored.is_changed = true;
                // reload_contact_birthdates for a mutual contact
                // (cpp:9140-9141) — with the Contacts step
            }
            if stored.private_forward_name != private_forward_name {
                if stored.private_forward_name.is_empty() != private_forward_name.is_empty() {
                    stored.is_changed = true;
                }
                stored.private_forward_name = private_forward_name;
                stored.need_save_to_database = true;
            }
            if stored.read_dates_private != read_dates_private
                || stored.contact_require_premium != contact_require_premium
            {
                stored.read_dates_private = read_dates_private;
                stored.contact_require_premium = contact_require_premium;
                stored.need_save_to_database = true;
            }
            if stored.about != about {
                stored.about = about;
                stored.is_changed = true;
                // GroupCallManager::on_update_dialog_about (cpp:9167) —
                // with its step
            }
            // the bot-info block (cpp:9170-9233) — with the BotInfo step
            if stored.linked_community_id != linked_community_id {
                stored.linked_community_id = linked_community_id;
                stored.is_changed = true;
            }
            if stored.can_manage_emoji_status != can_manage_emoji_status {
                stored.can_manage_emoji_status = can_manage_emoji_status;
                stored.is_changed = true;
            }
            if stored.personal_channel_id != personal_channel_id {
                stored.personal_channel_id = personal_channel_id;
                stored.is_changed = true;
            }
            // get_channel_difference_if_needed for the personal chat
            // (cpp:9250-9254) — with MessagesManager
        }

        // The photo application (cpp:9245-9293): do_update_user_photo
        // should be a no-op if the server sent consistent data. The
        // record's photos are stored as the reduced `as_profile_photo`
        // form (module docs); register_user_photo works over the full
        // converted photos.
        let photo = get_photo(std::mem::take(&mut user_full.profile_photo).unwrap_or_default());
        let personal_photo =
            get_photo(std::mem::take(&mut user_full.personal_photo).unwrap_or_default());
        let fallback_photo =
            get_photo(std::mem::take(&mut user_full.fallback_photo).unwrap_or_default());
        let (photo_ptr, is_personal) = if !personal_photo.is_empty() {
            (&personal_photo, true)
        } else if !photo.is_empty() {
            (&photo, false)
        } else {
            (&fallback_photo, false)
        };
        let is_photo_empty = photo_ptr.is_empty();
        // the u->access_hash file-manager registration inside the C++
        // conversion — the Files step
        self.do_update_user_photo(
            env,
            user_id,
            as_profile_photo(photo_ptr, is_personal),
            false,
            "on_get_user_full",
        );
        {
            let stored = self.users_full.get_mut(&user_id.get()).unwrap();
            let new_photo = as_profile_photo(&photo, false);
            if new_photo != stored.photo {
                stored.photo = new_photo;
                stored.is_changed = true;
            }
            let new_personal_photo = as_profile_photo(&personal_photo, true);
            if new_personal_photo != stored.personal_photo {
                stored.personal_photo = new_personal_photo;
                stored.is_changed = true;
            }
            let new_fallback_photo = as_profile_photo(&fallback_photo, false);
            if new_fallback_photo != stored.fallback_photo {
                stored.fallback_photo = new_fallback_photo;
                stored.is_changed = true;
            }
        }
        if !photo.is_empty() {
            self.register_user_photo(user_id, &photo);
        }
        if user_id == self.my_id && !fallback_photo.is_empty() {
            self.register_suggested_profile_photo(&fallback_photo);
        }
        if is_photo_empty {
            self.drop_user_photos(user_id, true, "on_get_user_full");
        }

        // User must be updated before UserFull
        if self.users.get(&user_id.get()).unwrap().is_changed {
            tdutils::log_error!("Receive inconsistent chatPhoto and chatPhotoInfo for {user_id:?}");
            self.update_user(env, user_id, false, false);
        }

        // The saved-music application (cpp:9295-9323) over the registry
        // stand-in.
        let first_saved_music_file_id = self
            .parse_saved_music_document(
                std::mem::take(&mut user_full.saved_music).unwrap_or_default(),
            )
            .unwrap_or_default();
        Self::on_update_user_full_first_saved_music_file_id(
            self.users_full.get_mut(&user_id.get()).unwrap(),
            first_saved_music_file_id,
        );
        if !first_saved_music_file_id.is_valid() {
            self.drop_user_saved_music(user_id, true, "on_get_user_full");
        } else {
            let cache_conflict = self
                .user_saved_music
                .get(&user_id.get())
                .map(|cache| {
                    cache.offset == 0
                        && (cache.saved_music_file_ids.is_empty()
                            || cache.saved_music_file_ids[0] != first_saved_music_file_id)
                })
                .unwrap_or(false);
            if cache_conflict {
                tdutils::log_info!("Drop cache of saved music of {user_id:?}");
                drop_user_saved_music_map(&mut self.user_saved_music, user_id, false);
            }
        }
        // on_get_peer_settings (cpp:9330-9331) — with MessagesManager
        self.users_full
            .get_mut(&user_id.get())
            .unwrap()
            .is_update_user_full_sent = true;
        self.update_user_full(env, user_id, "on_get_user_full", false);
    }

    /// `UserManager::invalidate_user_full` (UserManager.cpp:4809-4823).
    pub fn invalidate_user_full(&mut self, env: &mut UserEnv, user_id: UserId) {
        let loaded = self.get_user_full_force(env, user_id, "invalidate_user_full");
        if !loaded {
            return;
        }
        // DialogManager::on_dialog_info_full_invalidated — with its step
        let expired = {
            let ctx = env.time_ctx();
            self.users_full
                .get(&user_id.get())
                .unwrap()
                .is_expired(&ctx)
        };
        if !expired {
            let stored = self.users_full.get_mut(&user_id.get()).unwrap();
            stored.expires_at = 0.0;
            stored.need_save_to_database = true;
            self.update_user_full(env, user_id, "invalidate_user_full", false);
        }
    }

    /// `UserManager::drop_user_full` (UserManager.cpp:9552-9603):
    /// force-load, then reset the fields to their defaults and keep the
    /// record in the cache. GroupCallManager::on_update_dialog_about
    /// arrives with its step.
    pub fn drop_user_full(&mut self, env: &mut UserEnv, user_id: UserId) {
        let loaded = self.get_user_full_force(env, user_id, "drop_user_full");
        self.drop_user_photos(user_id, false, "drop_user_full");
        self.drop_user_saved_music(user_id, false, "drop_user_full");
        if !loaded {
            return;
        }
        {
            let user_full = self.users_full.get_mut(&user_id.get()).unwrap();
            user_full.expires_at = 0.0;
            user_full.photo = ProfilePhoto::default();
            user_full.personal_photo = ProfilePhoto::default();
            user_full.fallback_photo = ProfilePhoto::default();
            user_full.first_saved_music_file_id = FileId::default();
            // is_blocked / is_blocked_for_stories are kept (cpp:9560-9561)
            user_full.can_be_called = false;
            user_full.supports_video_calls = false;
            user_full.has_private_calls = false;
            user_full.need_phone_number_privacy_exception = false;
            user_full.wallpaper_overridden = false;
            user_full.about = String::new();
            // bot_info — with the BotInfo step
            user_full.gift_count = 0;
            user_full.common_chat_count = 0;
            user_full.personal_channel_id = 0;
            // main_profile_tab — with the ProfileTab step
            user_full.linked_community_id = CommunityId::default();
            // business_info / bot_verification / star_rating /
            // pending_star_rating — with their subsystems
            user_full.private_forward_name = String::new();
            user_full.voice_messages_forbidden = false;
            user_full.unofficial_security_risk = false;
            user_full.has_pinned_stories = false;
            user_full.read_dates_private = false;
            user_full.contact_require_premium = false;
            user_full.birthdate = Birthdate::default();
            user_full.gift_settings = crate::star_gift_settings::StarGiftSettings::allow_nothing();
            user_full.sponsored_enabled = false;
            user_full.has_preview_medias = false;
            user_full.can_view_revenue = false;
            user_full.can_manage_emoji_status = false;
            user_full.charge_paid_message_stars = 0;
            user_full.send_paid_message_stars = 0;
            // note — with the FormattedText step
            user_full.is_changed = true;
        }
        self.update_user_full(env, user_id, "drop_user_full", false);
    }

    /// `UserManager::add_user_full` (UserManager.cpp:8898-8906) — a new
    /// record retires the premium-contact price cache entry.
    pub fn add_user_full(&mut self, user_id: UserId) -> &mut UserFull {
        assert!(user_id.is_valid());
        if !self.users_full.contains_key(&user_id.get()) {
            self.user_full_contact_price.remove(&user_id.get());
        }
        self.users_full.entry(user_id.get()).or_default()
    }

    /// `UserManager::get_user_full_database_key`
    /// (UserManager.cpp:9356-9358).
    fn get_user_full_database_key(user_id: UserId) -> Vec<u8> {
        format!("usf{}", user_id.get()).into_bytes()
    }

    /// `UserManager::save_user_full` (UserManager.cpp:9345-9355): the
    /// C++ async pmc write collapses onto the synchronous kv (the
    /// UserLogEvent binlog mirror exists only for users).
    fn save_user_full(&mut self, env: &mut UserEnv, user_id: UserId) {
        if !env.global.use_chat_info_database() {
            return;
        }
        let value = {
            let Some(user_full) = self.users_full.get(&user_id.get()) else {
                return;
            };
            let ctx = env.time_ctx();
            let mut value = Vec::new();
            user_full.store_value(&mut value, &ctx);
            value
        };
        env.global
            .td_db()
            .common_kv()
            .set(&Self::get_user_full_database_key(user_id), &value);
    }

    /// `UserManager::get_user_full_force` (UserManager.cpp:8908-8928):
    /// `have_user_force` (cpp:5016-5018) is the C++ `get_user_force !=
    /// nullptr`, i.e. the special-user synthesis applies here too.
    /// Returns whether the full info is now in the cache.
    fn get_user_full_force(&mut self, env: &mut UserEnv, user_id: UserId, source: &str) -> bool {
        if !self.get_user_force(env, user_id, source) {
            return false;
        }
        if self.users_full.contains_key(&user_id.get()) {
            return true;
        }
        if !env.global.use_chat_info_database() {
            return false;
        }
        if !self.unavailable_user_fulls.insert(user_id.get()) {
            return false;
        }
        tdutils::log_info!("Trying to load full {user_id:?} from database from {source}");
        let value = env
            .global
            .td_db()
            .common_kv()
            .get(&Self::get_user_full_database_key(user_id));
        self.on_load_user_full_from_database(env, user_id, value);
        self.users_full.contains_key(&user_id.get())
    }

    /// `UserManager::on_load_user_full_from_database`
    /// (UserManager.cpp:9364-9438), collapsed onto the synchronous kv.
    fn on_load_user_full_from_database(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        value: Vec<u8>,
    ) {
        if self.users_full.contains_key(&user_id.get()) || value.is_empty() {
            return;
        }

        self.add_user_full(user_id);
        let parsed = {
            let ctx = env.time_ctx();
            UserFull::parse_value(&value, &ctx)
        };
        let user_full = match parsed {
            Ok(user_full) => user_full,
            Err(error) => {
                // can't happen unless the database is broken: clean all
                // known data about the user and pretend there was nothing
                tdutils::log_error!("Repair broken full {user_id:?}: {error}");
                self.users_full.remove(&user_id.get());
                env.global
                    .td_db()
                    .common_kv()
                    .erase(&Self::get_user_full_database_key(user_id));
                return;
            }
        };
        *self.users_full.get_mut(&user_id.get()).unwrap() = user_full;

        // Dependencies (cpp:9403-9413): the user itself is guaranteed by
        // the have_user_force gate; the linked-community /
        // personal-channel / bot-info / note loads arrive with
        // ChatManager and the subsystems. The pending_star_rating
        // promotion (cpp:9415-9418) — with StarRating.
        if self
            .users_full
            .get(&user_id.get())
            .unwrap()
            .need_phone_number_privacy_exception
            && self.is_user_contact_of(user_id, false)
        {
            self.users_full
                .get_mut(&user_id.get())
                .unwrap()
                .need_phone_number_privacy_exception = false;
        }

        let expected_photo_id = self.users.get(&user_id.get()).unwrap().photo.id;
        self.drop_user_full_photos(
            env,
            user_id,
            expected_photo_id,
            "on_load_user_full_from_database 2",
        );
        {
            let user_full = self.users_full.get_mut(&user_id.get()).unwrap();
            // register_user_photo (cpp:9421-9422) and the my suggested
            // profile photo (cpp:9423-9425) — with the Files step;
            // GroupCallManager::on_update_dialog_about (cpp:9428) — with
            // its step
            if user_full.noforwards_my_enabled || user_full.noforwards_peer_enabled {
                user_full.is_has_protected_content_changed = true;
            }
            user_full.is_update_user_full_sent = true;
        }

        // aa6d3aaf9 (cpp:9466-9473): the drop/reload now precede the
        // update_user_full send, so a deleted user doesn't emit a
        // duplicate updateUserFullInfo.
        let is_deleted = self
            .users
            .get(&user_id.get())
            .map(|u| u.is_deleted)
            .unwrap_or(false);
        if is_deleted {
            self.drop_user_full(env, user_id);
        } else if self
            .users_full
            .get(&user_id.get())
            .map(|user_full| user_full.expires_at == 0.0)
            .unwrap_or(false)
        {
            self.reload_user_full(
                env,
                user_id,
                Promise::default(),
                "on_load_user_full_from_database 4",
            );
        }
        self.update_user_full(env, user_id, "on_load_user_full_from_database 3", true);

        // 61e454f02 (cpp:9478-9485): sync the User-record cache from the
        // loaded full info — read after the branches, so a deleted user
        // syncs the dropped record's defaults like the C++ pointer.
        let (user_full_my, user_full_peer) = self
            .users_full
            .get(&user_id.get())
            .map(|user_full| {
                (
                    user_full.noforwards_my_enabled,
                    user_full.noforwards_peer_enabled,
                )
            })
            .unwrap_or((false, false));
        let user_changed = {
            let Some(u) = self.users.get_mut(&user_id.get()) else {
                return;
            };
            let changed = u.noforwards_my_enabled != user_full_my
                || u.noforwards_peer_enabled != user_full_peer
                || !u.is_noforwards_inited;
            if changed {
                u.noforwards_my_enabled = user_full_my;
                u.noforwards_peer_enabled = user_full_peer;
                u.is_noforwards_inited = true;
                u.need_save_to_database = true;
            }
            changed
        };
        if user_changed {
            self.update_user(env, user_id, false, false);
        }
    }

    /// `UserManager::load_user_full` (UserManager.cpp:8930-8953). The
    /// never-loaded and expired-bot bands park the promise on the merged
    /// `users.getFullUser` query (the GetUserFullCombiner); the
    /// expired-non-bot band refreshes in the background (the empty
    /// promise rides the combiner's delay window) and completes
    /// immediately. `StoryManager::on_view_dialog_active_stories` stays
    /// with its manager.
    pub fn load_user_full(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        force: bool,
        mut promise: Promise<()>,
        source: &str,
    ) {
        if !self.users.contains_key(&user_id.get()) {
            return promise.set_error(Status::error(400, "User not found"));
        }
        if !self.get_user_full_force(env, user_id, source) {
            // cpp:8936-8939 — `TRY_RESULT_PROMISE(input_user,
            // get_input_user(user_id))` then the combined query
            match self.get_input_user(env, user_id) {
                Ok(input_user) => {
                    self.send_get_user_full_query(user_id, input_user, promise, source)
                }
                Err(error) => promise.set_error(error),
            }
            return;
        }
        let expired = {
            let ctx = env.time_ctx();
            self.users_full
                .get(&user_id.get())
                .unwrap()
                .is_expired(&ctx)
        };
        if expired {
            // cpp:8941-8948 — the expired row triggers the background
            // refresh query; bots without force park the promise itself
            // on the query, the other forms refresh fire-and-forget
            let input_user = self.get_input_user_force(env, user_id);
            if env.auth_is_bot && !force {
                return self.send_get_user_full_query(
                    user_id,
                    input_user,
                    promise,
                    "load expired user_full",
                );
            }
            self.send_get_user_full_query(
                user_id,
                input_user,
                Promise::default(),
                "load expired user_full",
            );
        }
        promise.set_value(());
    }

    /// `GetUserFullInfoRequest::do_run` + `do_send_result`
    /// (Requests.cpp:235-249) — the two-tries RequestActor collapsed onto
    /// the load: the locally-answerable band (the full is loaded and not
    /// in the promise-riding window) builds the object immediately; the
    /// query bands park the answer on the GetUserFullCombiner and the
    /// completion builds the object (a still-missing full answers the
    /// tries-exhausted 500, the RequestActor ladder end).
    pub fn load_user_full_for_request(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        force: bool,
        mut answer: Promise<td_api::UserFullInfo>,
        source: &str,
    ) {
        if !self.users.contains_key(&user_id.get()) {
            // the load's first check (cpp:8932-8934)
            return answer.set_error(Status::error(400, "User not found"));
        }
        let loaded = self.get_user_full_force(env, user_id, source);
        let expired = loaded && {
            let ctx = env.time_ctx();
            self.users_full
                .get(&user_id.get())
                .unwrap()
                .is_expired(&ctx)
        };
        // cpp:8935-8948 — the promise rides the query only when the full
        // is missing, or for the expired-bot-without-force refresh; the
        // other expired forms still fire the background refresh
        // (fire-and-forget, the combiner's delay window) before the
        // local answer
        if loaded && !(expired && env.auth_is_bot && !force) {
            if expired {
                let input_user = self.get_input_user_force(env, user_id);
                self.send_get_user_full_query(
                    user_id,
                    input_user,
                    Promise::default(),
                    "load expired user_full",
                );
            }
            return answer.set_value(self.get_user_full_info_object_of(env, user_id));
        }
        let completion = match self.td_actor_id.clone() {
            Some(td_actor_id) => PromiseCreator::lambda_result(tdactor::promise_send_closure(
                td_actor_id,
                move |td: &mut crate::td::Td, result: tdutils::status::Result<()>| match result {
                    tdutils::status::Result::Ok(()) => {
                        let Some(options) = td.option_manager.as_mut() else {
                            return answer.set_error(Status::error(500, "Request aborted"));
                        };
                        let env = UserEnv {
                            global: &mut td.global,
                            options,
                            auth_is_bot: td.auth_manager.is_bot(),
                        };
                        td.user_manager
                            .finish_get_user_full_info_request(&env, user_id, answer);
                    }
                    tdutils::status::Result::Err(error) => answer.set_error(error),
                },
            )),
            None => {
                // the net-inert posture — the query bands park without the
                // actor machinery; the answer takes the C++ close-state
                // surface (the uncached-consumer convention) and the
                // completion absorbs the load's own band answers
                tdutils::log_info!("Get full {user_id:?} from {source} — [net-inert]");
                answer.set_error(Status::error(500, "Request aborted"));
                Promise::default()
            }
        };
        self.load_user_full(env, user_id, force, completion, source);
    }

    /// The completion half of [`UserManager::load_user_full_for_request`]
    /// — `GetUserFullInfoRequest::do_send_result`: the loaded full builds
    /// the object; a still-missing one answers the tries-exhausted 500
    /// (`RequestActor::loop`, RequestActor.h:56-60).
    fn finish_get_user_full_info_request(
        &mut self,
        env: &UserEnv,
        user_id: UserId,
        mut answer: Promise<td_api::UserFullInfo>,
    ) {
        if self.users_full.contains_key(&user_id.get()) {
            answer.set_value(self.get_user_full_info_object_of(env, user_id));
        } else {
            answer.set_error(Status::error(500, "Requested data is inaccessible"));
        }
    }

    /// `UserManager::reload_user_full` (UserManager.cpp:8954-8957) — the
    /// unconditional reload over the combined `users.getFullUser` query.
    pub fn reload_user_full(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        mut promise: Promise<()>,
        source: &str,
    ) {
        match self.get_input_user(env, user_id) {
            Ok(input_user) => self.send_get_user_full_query(user_id, input_user, promise, source),
            Err(error) => promise.set_error(error),
        }
    }

    /// `UserManager::send_get_user_full_query` (UserManager.cpp:8959-8972)
    /// — the QueryCombiner park: the first waiter supplies the send
    /// closure, the duplicates join; the combiner resolves the send
    /// closure with its result promise when the query actually goes out
    /// (the Td-actor hop of the closure runs the send half,
    /// [`UserManager::send_get_full_user_query`]).
    fn send_get_user_full_query(
        &mut self,
        user_id: UserId,
        input_user: telegram_api::InputUser,
        mut promise: Promise<()>,
        source: &str,
    ) {
        tdutils::log_info!("Get full {user_id:?} from {source}");
        if !user_id.is_valid() {
            return promise.set_error(Status::error(500, "Invalid user_id"));
        }
        let td_actor_id = match self.td_actor_id.clone() {
            Some(td_actor_id) => td_actor_id,
            None => {
                // the net-inert posture — the combiner is unwired without
                // the scheduler context; the C++ close-state surface
                // answers (the uncached-consumer convention)
                tdutils::log_info!("Get full {user_id:?} from {source} — [net-inert]");
                return promise.set_error(Status::error(500, "Request aborted"));
            }
        };
        let send_query = PromiseCreator::lambda_result(tdactor::promise_send_closure(
            td_actor_id,
            move |td: &mut crate::td::Td, result: tdutils::status::Result<Promise<()>>| {
                if let tdutils::status::Result::Ok(promise) = result
                    && !td.global.close_flag()
                {
                    // `td->create_handler<GetFullUserQuery>(
                    // promise.move_as_ok())->send(std::move(input_user))`
                    td.user_manager
                        .send_get_full_user_query(input_user, promise);
                }
            },
        ));
        match self.get_user_full_queries.clone() {
            Some(combiner) => tdactor::send_closure(
                combiner,
                crate::query_combiner::QueryCombinerMessage::AddQuery {
                    query_id: user_id.get(),
                    send_query,
                    promise,
                },
            ),
            None => {
                tdutils::log_info!("Get full {user_id:?} from {source} — [net-inert]");
                promise.set_error(Status::error(500, "Request aborted"));
            }
        }
    }

    /// `GetFullUserQuery::send` (cpp:1464-1466) — park the handler's
    /// result promise and dispatch the `users.getFullUser` query over
    /// the net handle.
    pub(crate) fn send_get_full_user_query(
        &mut self,
        input_user: telegram_api::InputUser,
        promise: Promise<()>,
    ) {
        self.pending_get_full_user_queries.push(promise);
        match self.net.clone() {
            Some(net) => net.dispatch(
                UserNetKind::GetFullUser,
                telegram_api::Function::users_getFullUser(telegram_api::users_getFullUser {
                    id: Some(input_user),
                }),
            ),
            None => tdutils::log_info!("Send users.getFullUser — [net-inert]"),
        }
    }

    /// `GetFullUserQuery::on_result`'s promise pop (cpp:1461-1481) — the
    /// head in-flight combiner result promise.
    pub(crate) fn take_get_full_user_query_promise(&mut self) -> Option<Promise<()>> {
        if self.pending_get_full_user_queries.is_empty() {
            None
        } else {
            Some(self.pending_get_full_user_queries.remove(0))
        }
    }

    /// `UserManager::drop_user_full_photos` (UserManager.cpp:9485-9520).
    fn drop_user_full_photos(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        expected_photo_id: i64,
        source: &str,
    ) {
        if !self.users_full.contains_key(&user_id.get()) {
            return;
        }
        tdutils::log_info!("Expect full photo {expected_photo_id} from {source}");
        {
            let user_full = self.users_full.get_mut(&user_id.get()).unwrap();
            let mut personal_photo = std::mem::take(&mut user_full.personal_photo);
            let mut photo = std::mem::take(&mut user_full.photo);
            let mut fallback_photo = std::mem::take(&mut user_full.fallback_photo);
            for photo_ptr in [&mut personal_photo, &mut photo, &mut fallback_photo] {
                if photo_ptr.is_empty() {
                    continue;
                }
                if expected_photo_id == 0 {
                    // if profile photo is empty, we must drop the full photo
                    *photo_ptr = ProfilePhoto::default();
                    user_full.is_changed = true;
                } else if expected_photo_id != photo_ptr.id {
                    // if full profile photo is unknown, we must drop it
                    *photo_ptr = ProfilePhoto::default();
                    user_full.is_changed = true;
                } else {
                    // nothing to drop
                    break;
                }
            }
            user_full.personal_photo = personal_photo;
            user_full.photo = photo;
            user_full.fallback_photo = fallback_photo;
            if expected_photo_id != Self::get_user_full_profile_photo_id(user_full) {
                user_full.expires_at = 0.0;
            }
        }
        if self
            .users_full
            .get(&user_id.get())
            .unwrap()
            .is_update_user_full_sent
        {
            self.update_user_full(env, user_id, "drop_user_full_photos", false);
        }
    }

    /// `UserManager::get_user_full_profile_photo_id`
    /// (UserManager.cpp:9472-9483).
    fn get_user_full_profile_photo_id(user_full: &UserFull) -> i64 {
        if !user_full.personal_photo.is_empty() {
            return user_full.personal_photo.id;
        }
        if !user_full.photo.is_empty() {
            return user_full.photo.id;
        }
        if !user_full.fallback_photo.is_empty() {
            return user_full.fallback_photo.id;
        }
        0
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
        td_api::Update::updateUser(td_api::updateUser { user: Some(user) })
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
                has_topic_sidebar: u.has_bot_forum_sidebar,
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

        let emoji_status = u
            .last_sent_emoji_status
            .as_ref()
            .map(make_emoji_status_object);
        let restricts_new_chats = u.contact_require_premium && !u.is_mutual_contact;
        let have_access = self.have_input_peer_user(env, user_id, AccessRights::Know);
        td_api::User {
            id: user_id.get(),
            first_name: u.first_name.clone(),
            last_name: u.last_name.clone(),
            // `u->usernames.get_usernames_object()` — null when empty
            // (Usernames.cpp:69-72)
            usernames: if u.usernames.is_empty() {
                None
            } else {
                Some(u.usernames.get_usernames_object())
            },
            phone_number: u.phone_number.clone(),
            status: Some(self.get_user_status_object(env, user_id)),
            // `get_profile_photo_object` is null while the stored
            // small_file_id is invalid (DialogPhoto.cpp:68-73) — the file
            // ids are placeholders until the Files step, so the field is
            // always null on this tree
            profile_photo: None,
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
            // `u->peer_color_collectible == nullptr ? nullptr : ...`
            // (UserManager.cpp:10329)
            upgraded_gift_colors: u
                .peer_color_collectible
                .as_ref()
                .map(|collectible| collectible.get_upgraded_gift_colors_object()),
            profile_accent_color_id: u.profile_accent_color_id.get(),
            profile_background_custom_emoji_id: u.profile_background_custom_emoji_id.get(),
            emoji_status,
            is_contact: self.is_user_contact_of(user_id, false),
            is_mutual_contact: self.is_user_contact_of(user_id, true),
            is_close_friend: u.is_close_friend,
            // `get_verification_status_object` — null when everything is
            // unset (VerificationStatus.cpp:11-17)
            verification_status: (u.is_verified
                || u.is_scam
                || u.is_fake
                || u.bot_verification_icon.is_valid())
            .then(|| td_api::VerificationStatus {
                is_verified: u.is_verified,
                is_scam: u.is_scam,
                is_fake: u.is_fake,
                bot_verification_icon_custom_emoji_id: u.bot_verification_icon.get(),
            }),
            is_premium: u.is_premium,
            is_support: u.is_support,
            // `get_restriction_info_object` — null when there is nothing
            // to restrict (RestrictionReason.cpp:77-91)
            restriction_info: get_restriction_info_object(
                &u.restriction_reasons,
                &env.restriction_options(),
            ),
            active_story_state: {
                // `get_active_story_state_object` — null without a server
                // story id (ActiveStoryState.cpp:16-19)
                let (max_active, max_read, has_live) = self.get_user_active_story_state(user_id);
                if !max_active.is_server() {
                    None
                } else if has_live {
                    Some(td_api::ActiveStoryState::activeStoryStateLive(
                        td_api::activeStoryStateLive {
                            story_id: max_active.get(),
                        },
                    ))
                } else if max_active.get() > max_read.get() {
                    Some(td_api::ActiveStoryState::activeStoryStateUnread(
                        td_api::activeStoryStateUnread,
                    ))
                } else {
                    Some(td_api::ActiveStoryState::activeStoryStateRead(
                        td_api::activeStoryStateRead,
                    ))
                }
            },
            restricts_new_chats,
            paid_message_star_count: u.paid_message_star_count,
            have_access,
            r#type: Some(r#type),
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
            personal_photo: Some(td_api::ChatPhoto::default()),
            photo: Some(td_api::ChatPhoto::default()),
            public_photo: Some(td_api::ChatPhoto::default()),
            community_id: user_full.linked_community_id.get(),
            block_list: Some(if user_full.is_blocked {
                td_api::BlockList::blockListMain(td_api::blockListMain)
            } else if user_full.is_blocked_for_stories {
                td_api::BlockList::blockListStories(td_api::blockListStories)
            } else {
                // the C++ emits null; the forced stand-in is blockListMain
                td_api::BlockList::blockListMain(td_api::blockListMain)
            }),
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
            bio: Some(bio),
            birthdate: Some(td_api::Birthdate {
                day: user_full.birthdate.get_day(),
                month: user_full.birthdate.get_month(),
                year: user_full.birthdate.get_year(),
            }),
            personal_chat_id: user_full.personal_channel_id,
            gift_count: user_full.gift_count,
            group_in_common_count: user_full.common_chat_count,
            incoming_paid_message_star_count: user_full.charge_paid_message_stars,
            outgoing_paid_message_star_count: user_full.send_paid_message_stars,
            gift_settings: Some(user_full.gift_settings.get_gift_settings_object()),
            bot_verification: Some(td_api::BotVerification {
                bot_user_id: 0,
                icon_custom_emoji_id: 0,
                custom_description: Some(td_api::FormattedText {
                    text: String::new(),
                    entities: Vec::new(),
                }),
            }),
            main_profile_tab: Some(crate::chat_manager::get_profile_tab_object(
                user_full.main_profile_tab,
            )),
            first_profile_audio: Some(td_api::Audio::default()),
            rating: Some(td_api::UserRating {
                level: 0,
                is_maximum_level_reached: false,
                rating: 0,
                ..td_api::UserRating::default()
            }),
            pending_rating: Some(td_api::UserRating {
                level: 0,
                is_maximum_level_reached: false,
                rating: 0,
                ..td_api::UserRating::default()
            }),
            pending_rating_date: 0,
            note: Some(td_api::FormattedText {
                text: String::new(),
                entities: Vec::new(),
            }),
            // The TON wallet card / profile share theme plane (userFull
            // wallet_card / share_theme, UserManager.cpp:9049-9052)
            // rides the profile-themes family follow-up.
            ton_wallet_card: None,
            share_theme: None,
            business_info: Some(td_api::BusinessInfo::default()),
            bot_info: Some(td_api::BotInfo::default()),
        }
    }

    /// `UserManager::get_me` (UserManager.cpp:5148-5157) — the local fast
    /// path; the C++ promise park on the GetUserMerger is covered by the
    /// request arm's own users.getUsers round-trip (the td.rs getMe
    /// walking dispatch), so the overload stays promise-less here.
    pub fn get_me(&mut self, env: &mut UserEnv) -> UserId {
        let my_id = self.my_id;
        if !self.get_user_force(env, my_id, "get_me") {
            return UserId::default();
        }
        my_id
    }

    /// `UserManager::get_user` (UserManager.cpp:5159-5192) — the local
    /// path with the synchronous database preload; the merged
    /// users.getUsers query tail parks at the request overload
    /// ([`UserManager::load_user`]). Returns whether the
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
    /// The `are_contacts_loaded_` gate for the request layer.
    pub(crate) fn are_contacts_loaded(&self) -> bool {
        self.are_contacts_loaded
    }

    /// Park a load-gated td_api request (see [`PendingUserRequest`]).
    pub(crate) fn park_pending_user_request(&mut self, request: PendingUserRequest) {
        self.pending_user_requests.push(request);
    }

    /// `UserManager::get_user(UserId, int, Promise)`
    /// (UserManager.cpp:5159-5192) — the request-facing overload. The
    /// asynchronous db-load park collapses onto the synchronous
    /// `get_user_force` re-check; the miss parks the answer on the
    /// GetUserMerger (the merged `users.getUsers` query, cpp:5186) and
    /// the merger resolution re-runs this load with the decremented
    /// tries (the RequestActor re-entry collapsed onto the completion
    /// closure). Every band completes `answer` — `GetUserRequest::
    /// do_send_result` builds the object (Requests.cpp:217-233). Returns
    /// whether the user is answerable locally.
    pub fn load_user(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        left_tries: i32,
        mut answer: Promise<td_api::User>,
    ) -> bool {
        if !user_id.is_valid() {
            answer.set_error(Status::error(400, "Invalid user identifier"));
            return false;
        }

        if user_id == UserManager::get_service_notifications_user_id()
            || user_id == UserManager::get_replies_bot_user_id(env.global.is_test_dc())
            || user_id == UserManager::get_verification_codes_bot_user_id()
            || user_id == UserManager::get_anonymous_bot_user_id(env.global.is_test_dc())
            || user_id == UserManager::get_channel_bot_user_id(env.global.is_test_dc())
            || user_id == UserManager::get_anti_spam_bot_user_id(env.global.is_test_dc())
        {
            self.get_user_force(env, user_id, "get_user");
        }

        let have = if env.auth_is_bot {
            self.have_accessible_user(user_id)
        } else {
            self.have_min_user(user_id)
        };
        if !have {
            // the C++ `left_tries > 2 && use_chat_info_database()` branch
            // parks the promise on the asynchronous db load; the load is
            // synchronous here, so the re-run collapses into this check
            if left_tries > 2 && env.global.use_chat_info_database() {
                self.get_user_force(env, user_id, "get_user");
            }
            let have = if env.auth_is_bot {
                self.have_accessible_user(user_id)
            } else {
                self.have_min_user(user_id)
            };
            if !have {
                let r_input_user = self.get_input_user(env, user_id);
                if left_tries == 1 || r_input_user.is_err() {
                    let error = match r_input_user {
                        Err(error) => error,
                        Ok(_) => Status::error(400, "User not found"),
                    };
                    answer.set_error(error);
                    return false;
                }

                // `get_user_queries_.add_query(user_id, promise,
                // "get_user")` — the merged users.getUsers query
                self.add_get_user_query(user_id, left_tries, answer);
                return false;
            }
        }

        answer.set_value(self.get_user_object_of(env, user_id));
        true
    }

    /// `UserManager::reload_user` (UserManager.cpp:5194-5204) — force the
    /// local halves, then park the promise on the GetUserMerger whatever
    /// the state (the server record replaces the local one).
    pub fn reload_user(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        mut promise: Promise<()>,
        source: &str,
    ) {
        if !user_id.is_valid() {
            return promise.set_error(Status::error(400, "Invalid user identifier"));
        }

        self.have_user_force(env, user_id, source);

        match self.get_input_user(env, user_id) {
            // `TRY_STATUS_PROMISE(promise, get_input_user(user_id))` —
            // the ok value drops; only the error rides
            Ok(_) => self.add_reload_user_query(user_id, promise),
            Err(error) => promise.set_error(error),
        }
    }

    /// The `get_user_queries_.add_query` park of the `get_user` request
    /// overload (cpp:5186): the answer rides the merger and the
    /// resolution re-runs `load_user` with the decremented tries — the
    /// RequestActor `raw_event` → `loop` re-entry (RequestActor.h:83-87).
    fn add_get_user_query(
        &mut self,
        user_id: UserId,
        left_tries: i32,
        mut answer: Promise<td_api::User>,
    ) {
        match (self.get_user_queries.clone(), self.td_actor_id.clone()) {
            (Some(merger), Some(td_actor_id)) => {
                let query_promise = PromiseCreator::lambda_result(tdactor::promise_send_closure(
                    td_actor_id,
                    move |td: &mut crate::td::Td, result: tdutils::status::Result<()>| match result
                    {
                        tdutils::status::Result::Ok(()) => {
                            let Some(options) = td.option_manager.as_mut() else {
                                return answer.set_error(Status::error(500, "Request aborted"));
                            };
                            let mut env = UserEnv {
                                global: &mut td.global,
                                options,
                                auth_is_bot: td.auth_manager.is_bot(),
                            };
                            td.user_manager
                                .load_user(&mut env, user_id, left_tries - 1, answer);
                        }
                        tdutils::status::Result::Err(error) => answer.set_error(error),
                    },
                ));
                tdactor::send_closure(
                    merger,
                    crate::query_merger::QueryMergerMessage::AddQuery {
                        query_id: user_id.get(),
                        promise: query_promise,
                    },
                );
            }
            _ => {
                // the net-inert posture: no scheduler context wired the
                // merger — the answer takes the C++ close-state surface
                // (the uncached-consumer convention)
                tdutils::log_info!("Get user {user_id:?} query — [net-inert]");
                answer.set_error(Status::error(500, "Request aborted"));
            }
        }
    }

    /// The `get_user_queries_.add_query` park of `reload_user`
    /// (cpp:5203) — the plain `()`-typed merger ride.
    fn add_reload_user_query(&mut self, user_id: UserId, mut promise: Promise<()>) {
        match self.get_user_queries.clone() {
            Some(merger) => tdactor::send_closure(
                merger,
                crate::query_merger::QueryMergerMessage::AddQuery {
                    query_id: user_id.get(),
                    promise,
                },
            ),
            None => {
                tdutils::log_info!("Reload user {user_id:?} — [net-inert]");
                promise.set_error(Status::error(500, "Request aborted"));
            }
        }
    }

    /// `GetUsersQuery::send` (cpp:1436-1438, reached through the
    /// GetUserMerger merge function cpp:2798-2802): park the handler's
    /// result promise and dispatch the merged `users.getUsers` query.
    pub(crate) fn send_get_users_query(
        &mut self,
        input_users: Vec<telegram_api::InputUser>,
        promise: Promise<()>,
    ) {
        self.pending_get_users_queries.push(promise);
        match self.net.clone() {
            Some(net) => net.dispatch(
                UserNetKind::GetUsers,
                telegram_api::Function::users_getUsers(telegram_api::users_getUsers {
                    id: input_users,
                }),
            ),
            None => tdutils::log_info!("Send users.getUsers — [net-inert]"),
        }
    }

    /// `GetUsersQuery::on_result`'s promise pop (cpp:1428-1445) — the
    /// head in-flight merger result promise.
    pub(crate) fn take_get_users_query_promise(&mut self) -> Option<Promise<()>> {
        if self.pending_get_users_queries.is_empty() {
            None
        } else {
            Some(self.pending_get_users_queries.remove(0))
        }
    }

    /// The `pending_user_requests` re-run (the request actors' post-load
    /// `do_run` re-entry). The loaded local halves answer synchronously;
    /// an import/change whose machine went asynchronous re-parks with its
    /// reserved `random_id` (the C++ actor re-runs on the task-promise
    /// completion and pops the reserved result then).
    pub(crate) fn drain_pending_user_requests(&mut self, env: &mut UserEnv) {
        let mut index = 0;
        while index < self.pending_user_requests.len() {
            let request = self.pending_user_requests.remove(index);
            match request {
                PendingUserRequest::GetCloseFriends { mut promise } => {
                    let user_ids = self.get_close_friends(env, Promise::default());
                    // GetCloseFriendsRequest::do_send_result — the
                    // close-friends list with the -1 total_count
                    let object = Object::users(self.get_users_object_of(env, -1, &user_ids));
                    promise.set_value(object);
                }
                PendingUserRequest::SearchContacts {
                    query,
                    limit,
                    mut promise,
                } => {
                    let (total_count, user_ids) =
                        self.search_contacts(env, &query, limit, Promise::default());
                    let object =
                        Object::users(self.get_users_object_of(env, total_count, &user_ids));
                    promise.set_value(object);
                }
                PendingUserRequest::ImportContacts {
                    contacts,
                    mut random_id,
                    mut promise,
                } => {
                    if random_id != 0 && !self.imported_contacts.contains_key(&random_id) {
                        // the ImportContactsTask is still awaiting its server
                        // answer — keep it parked until the finish hook
                        self.pending_user_requests.insert(
                            index,
                            PendingUserRequest::ImportContacts {
                                contacts,
                                random_id,
                                promise,
                            },
                        );
                        index += 1;
                        continue;
                    }
                    let result =
                        self.import_contacts(env, contacts, &mut random_id, Promise::default());
                    if random_id != 0 && self.import_contact_tasks.contains_key(&random_id) {
                        // the machine went asynchronous — wake on the
                        // on_import_contacts_finished re-run
                        self.pending_user_requests.insert(
                            index,
                            PendingUserRequest::ImportContacts {
                                contacts: Vec::new(),
                                random_id,
                                promise,
                            },
                        );
                        index += 1;
                    } else {
                        // ImportContactsRequest::do_send_result
                        let object = Object::importedContacts(
                            self.make_imported_contacts_object(env, &result.0, &result.1),
                        );
                        promise.set_value(object);
                    }
                }
                PendingUserRequest::ChangeImportedContacts {
                    mut contacts,
                    mut random_id,
                    mut promise,
                } => {
                    if random_id != 0 && self.import_contact_tasks.contains_key(&random_id) {
                        // the change task is still awaiting its server answer
                        self.pending_user_requests.insert(
                            index,
                            PendingUserRequest::ChangeImportedContacts {
                                contacts,
                                random_id,
                                promise,
                            },
                        );
                        index += 1;
                        continue;
                    }
                    let result = self.change_imported_contacts(
                        env,
                        &mut contacts,
                        &mut random_id,
                        Promise::default(),
                    );
                    if random_id != 0 && self.import_contact_tasks.contains_key(&random_id) {
                        // the machine went asynchronous (the import phase) —
                        // wake on the on_import_contacts_finished re-run
                        self.pending_user_requests.insert(
                            index,
                            PendingUserRequest::ChangeImportedContacts {
                                contacts: Vec::new(),
                                random_id,
                                promise,
                            },
                        );
                        index += 1;
                    } else if random_id != 0 {
                        // the machine completed within the call (the empty
                        // short-circuit) — the C++ actor's second run pops
                        // the member state and answers
                        let mut consumed = Vec::new();
                        let result = self.change_imported_contacts(
                            env,
                            &mut consumed,
                            &mut random_id,
                            Promise::default(),
                        );
                        let object = Object::importedContacts(
                            self.make_imported_contacts_object(env, &result.0, &result.1),
                        );
                        promise.set_value(object);
                    } else {
                        let object = Object::importedContacts(
                            self.make_imported_contacts_object(env, &result.0, &result.1),
                        );
                        promise.set_value(object);
                    }
                }
            }
        }
    }

    /// `UserManager::get_user_ids_object` (UserManager.cpp:10361-10366)
    /// folded into the `ImportContactsRequest::do_send_result` shape.
    fn make_imported_contacts_object(
        &mut self,
        env: &mut UserEnv,
        user_ids: &[UserId],
        unimported_contact_invites: &[i32],
    ) -> td_api::ImportedContacts {
        let user_ids: Vec<i64> = user_ids
            .iter()
            .map(|user_id| self.get_user_id_object(env, *user_id, "ImportContactsRequest"))
            .collect();
        td_api::ImportedContacts {
            user_ids,
            importer_count: unimported_contact_invites.to_vec(),
        }
    }

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
                    secret_chat: Some(make_unknown_secret_chat_object(SecretChatId::new(
                        secret_chat_id,
                    ))),
                }));
            }
        }

        let user_ids: Vec<i64> = self.users.keys().copied().collect();
        for user_id in user_ids {
            let user_object = self.get_user_object_of(env, UserId::new(user_id));
            updates.push(td_api::Update::updateUser(td_api::updateUser {
                user: Some(user_object),
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
                secret_chat: Some(object),
            }));
        }

        // users_full blocks and the contact birthdates/freeze-state
        // updates arrive with the UserFull store and the Contacts step
    }

    // ------------------------------------------------------------------
    // M.4.11: the read surface (UserManager.cpp:5196-5880) — InputUser/
    // InputPeer/inputEncryptedChat construction, bot data and the field
    // getters. `None`/`Err` stand-ins keep the C++ `nullptr`/400 surfaces.

    /// `UserManager::get_input_user` (UserManager.cpp:5206-5232). The
    /// `user_messages_` registry is in (M.4.15); the
    /// `inputUserFromMessage` construction (cpp:5215-5224) waits for
    /// `ChatManager::get_simple_input_peer` — the registry stays empty
    /// until ChatManager feeds `known_channels`, so the bot branch and
    /// the 400 errors are the reachable outcomes meanwhile.
    pub fn get_input_user(
        &self,
        env: &UserEnv,
        user_id: UserId,
    ) -> Result<telegram_api::InputUser, Status> {
        if user_id == self.my_id {
            return Ok(telegram_api::InputUser::inputUserSelf(
                telegram_api::inputUserSelf {},
            ));
        }
        let u = self.get_user(user_id);
        let inaccessible = match u {
            None => true,
            Some(u) => u.access_hash == -1 || u.is_min_access_hash,
        };
        if inaccessible {
            if env.auth_is_bot && user_id.is_valid() {
                return Ok(telegram_api::InputUser::inputUser(
                    telegram_api::inputUser {
                        user_id: user_id.get(),
                        access_hash: 0,
                    },
                ));
            }
            return Err(match u {
                None => Status::error(400, "User not found"),
                Some(_) => Status::error(400, "Have no access to the user"),
            });
        }
        Ok(telegram_api::InputUser::inputUser(
            telegram_api::inputUser {
                user_id: user_id.get(),
                access_hash: u.map(|u| u.access_hash).unwrap(),
            },
        ))
    }

    /// `UserManager::get_input_users` (UserManager.cpp:5234-5243).
    pub fn get_input_users(
        &self,
        env: &UserEnv,
        user_ids: &[UserId],
    ) -> Result<Vec<telegram_api::InputUser>, Status> {
        let mut input_users = Vec::with_capacity(user_ids.len());
        for user_id in user_ids {
            input_users.push(self.get_input_user(env, *user_id)?);
        }
        Ok(input_users)
    }

    /// `UserManager::get_input_user_force` (UserManager.cpp:5245-5252).
    pub fn get_input_user_force(&self, env: &UserEnv, user_id: UserId) -> telegram_api::InputUser {
        assert!(user_id.is_valid());
        self.get_input_user(env, user_id).unwrap_or_else(|_| {
            telegram_api::InputUser::inputUser(telegram_api::inputUser {
                user_id: user_id.get(),
                access_hash: 0,
            })
        })
    }

    /// `UserManager::get_input_users_force` (UserManager.cpp:5254-5265).
    pub fn get_input_users_force(
        &self,
        env: &UserEnv,
        user_ids: &[UserId],
    ) -> Vec<telegram_api::InputUser> {
        user_ids
            .iter()
            .filter_map(|user_id| self.get_input_user(env, *user_id).ok())
            .collect()
    }

    /// `UserManager::get_input_peer_user` (UserManager.cpp:5302-5325);
    /// `None` is the C++ `nullptr`. The registry admission is in
    /// (M.4.15); for a registry-only user the C++ builds
    /// `inputPeerUserFromMessage` (cpp:5312-5321) via
    /// `ChatManager::get_simple_input_peer`, which arrives with its
    /// slice — until then the registry stays empty and the branch is
    /// unreachable (`None` keeps the C++ nullptr surface).
    pub fn get_input_peer_user(
        &self,
        env: &UserEnv,
        user_id: UserId,
        access_rights: AccessRights,
    ) -> Option<telegram_api::InputPeer> {
        if user_id == self.my_id {
            return Some(telegram_api::InputPeer::inputPeerSelf(
                telegram_api::inputPeerSelf {},
            ));
        }
        let u = self.get_user(user_id);
        if !self.have_input_peer_user_for(u, user_id, access_rights, env.auth_is_bot) {
            return None;
        }
        let inaccessible = match u {
            None => true,
            Some(u) => u.access_hash == -1 || u.is_min_access_hash,
        };
        if inaccessible {
            if env.auth_is_bot && user_id.is_valid() {
                return Some(telegram_api::InputPeer::inputPeerUser(
                    telegram_api::inputPeerUser {
                        user_id: user_id.get(),
                        access_hash: 0,
                    },
                ));
            }
            // the C++ `user_messages_` CHECK + `inputPeerUserFromMessage`
            // — with ChatManager (see the doc comment)
            return None;
        }
        Some(telegram_api::InputPeer::inputPeerUser(
            telegram_api::inputPeerUser {
                user_id: user_id.get(),
                access_hash: u.map(|u| u.access_hash).unwrap(),
            },
        ))
    }

    /// `UserManager::have_input_encrypted_peer` (UserManager.cpp:5327-5343).
    pub fn have_input_encrypted_peer(
        &self,
        secret_chat_id: SecretChatId,
        access_rights: AccessRights,
    ) -> bool {
        match self.get_secret_chat(secret_chat_id) {
            None => false,
            Some(secret_chat) => match access_rights {
                AccessRights::Know | AccessRights::Read => true,
                AccessRights::Write | AccessRights::Edit => {
                    secret_chat.state == SecretChatState::Active
                }
            },
        }
    }

    /// `UserManager::get_input_encrypted_chat` (UserManager.cpp:5345-5353);
    /// `None` is the C++ `nullptr`.
    pub fn get_input_encrypted_chat(
        &self,
        secret_chat_id: SecretChatId,
        access_rights: AccessRights,
    ) -> Option<telegram_api::InputEncryptedChat> {
        let secret_chat = self.get_secret_chat(secret_chat_id)?;
        if !self.have_input_encrypted_peer(secret_chat_id, access_rights) {
            return None;
        }
        Some(telegram_api::InputEncryptedChat {
            chat_id: secret_chat_id.get(),
            access_hash: secret_chat.access_hash,
        })
    }

    /// `UserManager::get_bot_data` (UserManager.cpp:5403-5435).
    pub fn get_bot_data(&self, user_id: UserId) -> Result<BotData, Status> {
        let Some(u) = self.get_user(user_id) else {
            return Err(Status::error(400, "Bot not found"));
        };
        if !u.is_bot {
            return Err(Status::error(400, "User is not a bot"));
        }
        if u.is_deleted {
            return Err(Status::error(400, "Bot is deleted"));
        }
        if !u.is_received {
            return Err(Status::error(400, "Bot is inaccessible"));
        }
        Ok(BotData {
            username: u.usernames.get_first_username().to_owned(),
            can_be_edited: u.can_be_edited_bot,
            can_join_groups: u.can_join_groups,
            can_read_all_group_messages: u.can_read_all_group_messages,
            has_main_app: u.has_main_app,
            has_bot_forum_view: u.has_bot_forum_view,
            can_bot_create_topics: u.can_bot_create_topics,
            can_manage_bots: u.can_manage_bots,
            is_inline: u.is_inline_bot,
            is_guestchat: u.is_guestchat_bot,
            is_business: u.is_business_bot,
            is_guard: u.is_guard_bot,
            need_location: u.need_location_bot,
            can_be_added_to_attach_menu: u.can_be_added_to_attach_menu,
        })
    }

    /// `UserManager::get_user_dialog_photo` (UserManager.cpp:5484-5492).
    /// The read-only form skips the `apply_pending_user_photo` call
    /// (it needs the env); the callers that can hold the manager open
    /// use [`Self::get_user_dialog_photo_mut`]. `None` is the C++
    /// `nullptr`.
    pub fn get_user_dialog_photo(&self, user_id: UserId) -> Option<&ProfilePhoto> {
        self.get_user(user_id).map(|u| &u.photo)
    }

    /// `UserManager::get_user_dialog_photo` with the pending bot photo
    /// applied first (cpp:5489).
    pub fn get_user_dialog_photo_mut(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
    ) -> Option<&ProfilePhoto> {
        if self.get_user(user_id).is_some() {
            self.apply_pending_user_photo(env, user_id, "get_user_dialog_photo");
        }
        self.get_user(user_id).map(|u| &u.photo)
    }

    /// `UserManager::get_secret_chat_dialog_photo`
    /// (UserManager.cpp:5494-5500).
    pub fn get_secret_chat_dialog_photo(
        &self,
        secret_chat_id: SecretChatId,
    ) -> Option<&ProfilePhoto> {
        let c = self.get_secret_chat(secret_chat_id)?;
        self.get_user_dialog_photo(c.user_id)
    }

    /// `UserManager::get_user_accent_color_id_object`
    /// (UserManager.cpp:5502-5509) — the ThemeManager stand-in resolves
    /// invalid ids to the id-derived default (the `get_user_object`
    /// precedent).
    pub fn get_user_accent_color_id_object_of(&self, user_id: UserId) -> i32 {
        match self.get_user(user_id) {
            Some(u) if u.accent_color_id.is_valid() => u.accent_color_id.get(),
            _ => AccentColorId::from_peer_id(user_id.get()).get(),
        }
    }

    /// `UserManager::get_secret_chat_accent_color_id_object`
    /// (UserManager.cpp:5511-5517) — the C++ answers 5 for an unknown
    /// secret chat.
    pub fn get_secret_chat_accent_color_id_object_of(&self, secret_chat_id: SecretChatId) -> i32 {
        let Some(c) = self.get_secret_chat(secret_chat_id) else {
            return 5;
        };
        self.get_user_accent_color_id_object_of(c.user_id)
    }

    /// `UserManager::get_user_background_custom_emoji_id`
    /// (UserManager.cpp:5519-5526).
    pub fn get_user_background_custom_emoji_id_of(&self, user_id: UserId) -> CustomEmojiId {
        self.get_user(user_id)
            .map(|u| u.background_custom_emoji_id)
            .unwrap_or_default()
    }

    /// `UserManager::get_secret_chat_background_custom_emoji_id`
    /// (UserManager.cpp:5528-5534).
    pub fn get_secret_chat_background_custom_emoji_id_of(
        &self,
        secret_chat_id: SecretChatId,
    ) -> CustomEmojiId {
        let Some(c) = self.get_secret_chat(secret_chat_id) else {
            return CustomEmojiId::default();
        };
        self.get_user_background_custom_emoji_id_of(c.user_id)
    }

    /// `UserManager::get_user_upgraded_gift_colors_object`
    /// (UserManager.cpp:5536-5543); `None` is the C++ `nullptr`.
    pub fn get_user_upgraded_gift_colors_object_of(
        &self,
        user_id: UserId,
    ) -> Option<td_api::UpgradedGiftColors> {
        let u = self.get_user(user_id)?;
        u.peer_color_collectible
            .as_ref()
            .map(|collectible| collectible.get_upgraded_gift_colors_object())
    }

    /// `UserManager::get_secret_chat_upgraded_gift_colors_object`
    /// (UserManager.cpp:5545-5552).
    pub fn get_secret_chat_upgraded_gift_colors_object_of(
        &self,
        secret_chat_id: SecretChatId,
    ) -> Option<td_api::UpgradedGiftColors> {
        let c = self.get_secret_chat(secret_chat_id)?;
        self.get_user_upgraded_gift_colors_object_of(c.user_id)
    }

    /// `UserManager::get_user_profile_accent_color_id_object`
    /// (UserManager.cpp:5554-5561) — the ThemeManager stand-in passes
    /// the stored id through; unknown users answer -1 like the C++.
    pub fn get_user_profile_accent_color_id_object_of(&self, user_id: UserId) -> i32 {
        match self.get_user(user_id) {
            Some(u) => u.profile_accent_color_id.get(),
            None => -1,
        }
    }

    /// `UserManager::get_secret_chat_profile_accent_color_id_object`
    /// (UserManager.cpp:5563-5569).
    pub fn get_secret_chat_profile_accent_color_id_object_of(
        &self,
        secret_chat_id: SecretChatId,
    ) -> i32 {
        let Some(c) = self.get_secret_chat(secret_chat_id) else {
            return -1;
        };
        self.get_user_profile_accent_color_id_object_of(c.user_id)
    }

    /// `UserManager::get_user_profile_background_custom_emoji_id`
    /// (UserManager.cpp:5571-5578).
    pub fn get_user_profile_background_custom_emoji_id_of(&self, user_id: UserId) -> CustomEmojiId {
        self.get_user(user_id)
            .map(|u| u.profile_background_custom_emoji_id)
            .unwrap_or_default()
    }

    /// `UserManager::get_secret_chat_profile_background_custom_emoji_id`
    /// (UserManager.cpp:5580-5586).
    pub fn get_secret_chat_profile_background_custom_emoji_id_of(
        &self,
        secret_chat_id: SecretChatId,
    ) -> CustomEmojiId {
        let Some(c) = self.get_secret_chat(secret_chat_id) else {
            return CustomEmojiId::default();
        };
        self.get_user_profile_background_custom_emoji_id_of(c.user_id)
    }

    /// `UserManager::get_user_title` (UserManager.cpp:5588-5600).
    pub fn get_user_title_of(&self, user_id: UserId) -> String {
        let Some(u) = self.get_user(user_id) else {
            return String::new();
        };
        if u.last_name.is_empty() {
            return u.first_name.clone();
        }
        if u.first_name.is_empty() {
            return u.last_name.clone();
        }
        format!("{} {}", u.first_name, u.last_name)
    }

    /// `UserManager::get_secret_chat_title` (UserManager.cpp:5602-5607).
    pub fn get_secret_chat_title_of(&self, secret_chat_id: SecretChatId) -> String {
        let Some(c) = self.get_secret_chat(secret_chat_id) else {
            return String::new();
        };
        self.get_user_title_of(c.user_id)
    }

    /// `UserManager::get_user_default_permissions`
    /// (UserManager.cpp:5610-5618). The C++ `get_replies_bot_user_id`
    /// instance method reads the test-DC flag through `G()`, so the port
    /// takes it as a parameter. Lands with M.9.7 (its first consumer is
    /// `can_send_message_content`), not the ChatManager slice.
    pub fn get_user_default_permissions_of(
        &self,
        is_test_dc: bool,
        user_id: UserId,
    ) -> RestrictedRights {
        let u = self.get_user(user_id);
        if (u.is_none() && user_id != self.get_my_id())
            || user_id == Self::get_replies_bot_user_id(is_test_dc)
            || user_id == Self::get_verification_codes_bot_user_id()
        {
            return RestrictedRights::restrict_all();
        }
        RestrictedRights::from_bools(
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
            true,
            false,
            false,
            true,
            true,
            ChannelType::Unknown,
        )
    }

    /// `UserManager::get_secret_chat_default_permissions`
    /// (UserManager.cpp:5620-5627).
    pub fn get_secret_chat_default_permissions_of(
        &self,
        secret_chat_id: SecretChatId,
    ) -> RestrictedRights {
        if self.get_secret_chat(secret_chat_id).is_none() {
            return RestrictedRights::restrict_all();
        }
        RestrictedRights::from_bools(
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
            false,
            true,
            ChannelType::Unknown,
        )
    }

    /// `UserManager::get_user_emoji_status_object`
    /// (UserManager.cpp:5629-5635); `None` is the C++ `nullptr`.
    pub fn get_user_emoji_status_object_of(&self, user_id: UserId) -> Option<td_api::EmojiStatus> {
        let u = self.get_user(user_id)?;
        u.last_sent_emoji_status
            .as_ref()
            .map(make_emoji_status_object)
    }

    /// `UserManager::get_secret_chat_emoji_status_object`
    /// (UserManager.cpp:5637-5644).
    pub fn get_secret_chat_emoji_status_object_of(
        &self,
        secret_chat_id: SecretChatId,
    ) -> Option<td_api::EmojiStatus> {
        let c = self.get_secret_chat(secret_chat_id)?;
        self.get_user_emoji_status_object_of(c.user_id)
    }

    /// `UserManager::get_user_has_protected_content_force`
    /// (UserManager.cpp:5646-5652) — with the User-record cache consulted
    /// first (61e454f02).
    pub fn get_user_has_protected_content_force(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
    ) -> bool {
        self.get_user_force(env, user_id, "get_user_has_protected_content_force");
        if let Some(u) = self.users.get(&user_id.get())
            && u.is_noforwards_inited
        {
            return u.noforwards_my_enabled || u.noforwards_peer_enabled;
        }
        if self.get_user_full_force(env, user_id, "get_user_has_protected_content_force")
            && let Some(user_full) = self.get_user_full(user_id)
        {
            return user_full.noforwards_my_enabled || user_full.noforwards_peer_enabled;
        }
        false
    }

    /// `UserManager::get_user_has_protected_content`
    /// (UserManager.cpp:5654-5660) — with the User-record cache consulted
    /// first (61e454f02).
    pub fn get_user_has_protected_content_of(&self, user_id: UserId) -> bool {
        if let Some(u) = self.get_user(user_id)
            && u.is_noforwards_inited
        {
            return u.noforwards_my_enabled || u.noforwards_peer_enabled;
        }
        self.get_user_full(user_id)
            .map(|user_full| user_full.noforwards_my_enabled || user_full.noforwards_peer_enabled)
            .unwrap_or(false)
    }

    /// `UserManager::get_user_has_protected_content_force_by_me`
    /// (UserManager.cpp:5662-5668) — with the User-record cache consulted
    /// first (61e454f02).
    pub fn get_user_has_protected_content_force_by_me(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
    ) -> bool {
        self.get_user_force(env, user_id, "get_user_has_protected_content_force_by_me");
        if let Some(u) = self.users.get(&user_id.get())
            && u.is_noforwards_inited
        {
            return u.noforwards_my_enabled;
        }
        if self.get_user_full_force(env, user_id, "get_user_has_protected_content_force_by_me")
            && let Some(user_full) = self.get_user_full(user_id)
        {
            return user_full.noforwards_my_enabled;
        }
        false
    }

    /// `UserManager::get_user_has_protected_content_force_by_other`
    /// (UserManager.cpp:5670-5676) — with the User-record cache consulted
    /// first (61e454f02).
    pub fn get_user_has_protected_content_force_by_other(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
    ) -> bool {
        self.get_user_force(
            env,
            user_id,
            "get_user_has_protected_content_force_by_other",
        );
        if let Some(u) = self.users.get(&user_id.get())
            && u.is_noforwards_inited
        {
            return u.noforwards_peer_enabled;
        }
        if self.get_user_full_force(
            env,
            user_id,
            "get_user_has_protected_content_force_by_other",
        ) && let Some(user_full) = self.get_user_full(user_id)
        {
            return user_full.noforwards_peer_enabled;
        }
        false
    }

    /// `UserManager::get_user_stories_hidden` (UserManager.cpp:5678-5684).
    pub fn get_user_stories_hidden_of(&self, user_id: UserId) -> bool {
        self.get_user(user_id)
            .map(|u| u.stories_hidden)
            .unwrap_or(false)
    }

    /// `UserManager::can_poll_user_active_stories`
    /// (UserManager.cpp:5686-5689) with the
    /// `need_poll_user_active_stories` predicate (cpp:5691-5694) folded
    /// in; `now` is the caller's `Time::now()`. The story-ids intake
    /// keeps its reduced private stand-in.
    pub fn can_poll_user_active_stories_of(&self, user_id: UserId, now: f64) -> bool {
        let Some(u) = self.get_user(user_id) else {
            return false;
        };
        let need_poll = user_id != self.my_id
            && !self.is_user_contact_of(user_id, false)
            && !self.is_user_bot_of(user_id)
            && !self.is_user_support_of(user_id)
            && !self.is_user_deleted_of(user_id)
            && u.was_online != 0;
        need_poll && now >= u.max_active_story_id_next_reload_time
    }

    /// `UserManager::get_user_about` (UserManager.cpp:5696-5702).
    pub fn get_user_about(&mut self, env: &mut UserEnv, user_id: UserId) -> String {
        if self.get_user_full_force(env, user_id, "get_user_about")
            && let Some(user_full) = self.get_user_full(user_id)
        {
            return user_full.about.clone();
        }
        String::new()
    }

    /// `UserManager::get_secret_chat_about` (UserManager.cpp:5704-5710).
    pub fn get_secret_chat_about(
        &mut self,
        env: &mut UserEnv,
        secret_chat_id: SecretChatId,
    ) -> String {
        let Some(c) = self.get_secret_chat(secret_chat_id) else {
            return String::new();
        };
        let user_id = c.user_id;
        self.get_user_about(env, user_id)
    }

    /// `UserManager::get_user_private_forward_name`
    /// (UserManager.cpp:5712-5718).
    pub fn get_user_private_forward_name(&mut self, env: &mut UserEnv, user_id: UserId) -> String {
        if self.get_user_full_force(env, user_id, "get_user_private_forward_name")
            && let Some(user_full) = self.get_user_full(user_id)
        {
            return user_full.private_forward_name.clone();
        }
        String::new()
    }

    /// `UserManager::get_user_voice_messages_forbidden`
    /// (UserManager.cpp:5720-5729) — the C++ gates on premium before
    /// consulting the full.
    pub fn get_user_voice_messages_forbidden_of(&self, user_id: UserId) -> bool {
        if !self.is_user_premium_of(user_id) {
            return false;
        }
        self.get_user_full(user_id)
            .map(|user_full| user_full.voice_messages_forbidden)
            .unwrap_or(false)
    }

    /// `UserManager::get_my_sponsored_enabled`
    /// (UserManager.cpp:5731-5741) — non-premium users see sponsored
    /// messages; the full's flag answers when it is loaded.
    pub fn get_my_sponsored_enabled(&self) -> bool {
        let user_id = self.my_id;
        if !self.is_user_premium_of(user_id) {
            return true;
        }
        self.get_user_full(user_id)
            .map(|user_full| user_full.sponsored_enabled)
            .unwrap_or(true)
    }

    /// `UserManager::get_user_read_dates_private`
    /// (UserManager.cpp:5743-5749).
    pub fn get_user_read_dates_private(&mut self, env: &mut UserEnv, user_id: UserId) -> bool {
        if self.get_user_full_force(env, user_id, "get_user_read_dates_private")
            && let Some(user_full) = self.get_user_full(user_id)
        {
            return user_full.read_dates_private;
        }
        false
    }

    /// `UserManager::get_user_first_username`
    /// (UserManager.cpp:5773-5783) — an empty string for invalid or
    /// unknown ids (the C++ returns an empty `Slice`).
    pub fn get_user_first_username_of(&self, user_id: UserId) -> &str {
        if !user_id.is_valid() {
            return "";
        }
        self.get_user(user_id)
            .map(|u| u.usernames.get_first_username())
            .unwrap_or("")
    }

    /// `UserManager::get_secret_chat_id_object`
    /// (UserManager.cpp:10502-10511).
    pub fn get_secret_chat_id_object(&mut self, secret_chat_id: SecretChatId, source: &str) -> i32 {
        if secret_chat_id.is_valid()
            && self.get_secret_chat(secret_chat_id).is_none()
            && !self.unknown_secret_chats.contains(&secret_chat_id.get())
        {
            tdutils::log_error!("Have no information about {secret_chat_id:?} from {source}");
            self.unknown_secret_chats.insert(secret_chat_id.get());
            let secret_chat = make_unknown_secret_chat_object(secret_chat_id);
            self.send_update(td_api::Update::updateSecretChat(td_api::updateSecretChat {
                secret_chat: Some(secret_chat),
            }));
        }
        secret_chat_id.get()
    }

    /// `UserManager::get_secret_chat_object(SecretChatId)`
    /// (UserManager.cpp:10514-10521) — the request-facing builder; `None`
    /// (the C++ `nullptr` → 404 "Not Found") when the id is unknown.
    pub(crate) fn get_secret_chat_object_for_request(
        &mut self,
        env: &mut UserEnv,
        secret_chat_id: SecretChatId,
    ) -> Option<td_api::SecretChat> {
        let (user_id, state, is_outbound, key_hash, layer) = {
            let secret_chat = self.get_secret_chat(secret_chat_id)?;
            (
                secret_chat.user_id,
                secret_chat.state,
                secret_chat.is_outbound,
                secret_chat.key_hash.clone().into_bytes(),
                secret_chat.layer,
            )
        };
        self.get_user_force(env, user_id, "get_secret_chat_object");
        Some(td_api::SecretChat {
            id: secret_chat_id.get(),
            user_id: self.get_user_id_object(env, user_id, "secretChat"),
            state: Some(get_secret_chat_state_object(state)),
            is_outbound,
            key_hash,
            layer,
        })
    }

    // ------------------------------------------------------------------
    // M.4.12: profile-editing queries (UserManager.cpp:5882-6797) — the
    // validation state machines and the local success appliers. Every
    // `*_local` half answers what the C++ does before its net query; the
    // `UpdateProfileQuery`/username/color/... senders ride the net step.

    /// `UserManager::set_name` (UserManager.cpp:5882-5901) — returns the
    /// `ACCOUNT_UPDATE_*` flags for the (net) query; `Ok(0)` is the C++
    /// immediate `set_value`.
    pub fn set_name_local(&mut self, first_name: &str, last_name: &str) -> Result<i32, Status> {
        let new_first_name = crate::misc::clean_name(first_name, MAX_NAME_LENGTH);
        let new_last_name = crate::misc::clean_name(last_name, MAX_NAME_LENGTH);
        if new_first_name.is_empty() {
            return Err(Status::error(400, "First name must be non-empty"));
        }

        let my_id = self.get_my_id();
        let u = self.get_user(my_id);
        let mut flags = 0;
        if u.map(|u| u.first_name != new_first_name).unwrap_or(true) {
            flags |= ACCOUNT_UPDATE_FIRST_NAME;
        }
        if u.map(|u| u.last_name != new_last_name).unwrap_or(true) {
            flags |= ACCOUNT_UPDATE_LAST_NAME;
        }
        Ok(flags)
    }

    /// `UserManager::set_bio` (UserManager.cpp:5903-5917) — returns the
    /// sanitized bio for the (net) query; `Ok(None)` is the C++
    /// immediate `set_value`.
    pub fn set_bio_local(&mut self, env: &UserEnv, bio: &str) -> Result<Option<String>, Status> {
        let max_bio_length = env.options.get_option_integer("bio_length_max", 70).max(0) as usize;
        let mut new_bio = crate::misc::strip_empty_characters(bio, max_bio_length, false);
        crate::misc::replace_with_spaces(&mut new_bio, "\n");

        let user_full = self.get_user_full(self.get_my_id());
        if user_full
            .map(|user_full| user_full.about == new_bio)
            .unwrap_or(false)
        {
            return Ok(None);
        }
        Ok(Some(new_bio))
    }

    /// `UserManager::on_update_profile_success`
    /// (UserManager.cpp:5923-5945); the group-call about fan-out is a
    /// documented no-op until GroupCallManager lands.
    pub fn on_update_profile_success(
        &mut self,
        env: &mut UserEnv,
        flags: i32,
        first_name: &str,
        last_name: &str,
        about: &str,
    ) {
        assert!(flags != 0);

        let my_user_id = self.get_my_id();
        {
            let u = self.get_user(my_user_id);
            let Some(u) = u else {
                tdutils::log_error!("Doesn't receive info about me during update profile");
                return;
            };
            if (flags & ACCOUNT_UPDATE_FIRST_NAME) != 0 && u.first_name != first_name {
                tdutils::log_error!(
                    "Wrong first name \"{}\", expected \"{}\"",
                    u.first_name,
                    first_name
                );
            }
            if (flags & ACCOUNT_UPDATE_LAST_NAME) != 0 && u.last_name != last_name {
                tdutils::log_error!(
                    "Wrong last name \"{}\", expected \"{}\"",
                    u.last_name,
                    last_name
                );
            }
        }

        if (flags & ACCOUNT_UPDATE_ABOUT) != 0
            && self.get_user_full_force(env, my_user_id, "on_update_profile_success")
        {
            let user_full = self.users_full.get_mut(&my_user_id.get()).unwrap();
            user_full.about = about.to_owned();
            user_full.is_changed = true;
            self.update_user_full(env, my_user_id, "on_update_profile_success", false);
            // GroupCallManager::on_update_dialog_about
        }
    }

    /// `UserManager::set_username` (UserManager.cpp:6638-6643) — the
    /// validation half.
    pub fn set_username_local(&self, username: &str) -> Result<(), Status> {
        if !username.is_empty() && !crate::misc::is_allowed_username(username) {
            return Err(Status::error(400, "Username is invalid"));
        }
        Ok(())
    }

    /// `UserManager::toggle_username_is_active_impl`
    /// (UserManager.cpp:6655-6666) — the optimistic local apply; the
    /// `ToggleUsernameQuery` send rides the net step.
    pub fn toggle_username_is_active_local(
        &mut self,
        env: &mut UserEnv,
        username: &str,
        is_active: bool,
    ) -> Result<(), Status> {
        let user_id = self.get_my_id();
        let new_usernames = {
            let u = self
                .get_user(user_id)
                .expect("toggle_username_is_active needs the forced me");
            if !u.usernames.can_toggle(false, username) {
                return Err(Status::error(400, "Wrong username specified"));
            }
            u.usernames.toggle(false, username, is_active)
        };
        self.on_update_user_usernames(env, user_id, new_usernames);
        self.update_user(env, user_id, false, false);
        Ok(())
    }

    /// `UserManager::reorder_usernames_impl`
    /// (UserManager.cpp:6684-6699) — the optimistic local apply; the
    /// `ReorderUsernamesQuery` send rides the net step.
    pub fn reorder_usernames_local(
        &mut self,
        env: &mut UserEnv,
        usernames: &[String],
    ) -> Result<(), Status> {
        let user_id = self.get_my_id();
        let new_usernames = {
            let u = self
                .get_user(user_id)
                .expect("reorder_usernames needs the forced me");
            if !u.usernames.can_reorder_to(&usernames.to_vec()) {
                return Err(Status::error(400, "Invalid username order specified"));
            }
            if usernames.len() <= 1 {
                return Ok(());
            }
            u.usernames.reorder_to(usernames.to_vec())
        };
        self.on_update_user_usernames(env, user_id, new_usernames);
        self.update_user(env, user_id, false, false);
        Ok(())
    }

    /// `UserManager::on_update_username_is_active`
    /// (UserManager.cpp:6701-6711); the `reload_user` fallback for a
    /// stale local username is the net step.
    pub fn on_update_username_is_active(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        username: &str,
        is_active: bool,
    ) {
        let is_bot = self.is_user_bot_of(user_id);
        let new_usernames = {
            let u = self
                .get_user(user_id)
                .expect("the update flow forces the user");
            if !u.usernames.can_toggle(is_bot, username) {
                // `reload_user(user_id, std::move(promise),
                // "on_update_username_is_active")` (cpp:6575) — the flow
                // promise rides the ToggleUsername query family; the
                // empty-promise reload runs the refetch
                tdutils::log_info!("Need to reload {user_id:?} after the username toggle");
                self.reload_user(
                    env,
                    user_id,
                    Promise::default(),
                    "on_update_username_is_active",
                );
                return;
            }
            u.usernames.toggle(is_bot, username, is_active)
        };
        self.on_update_user_usernames(env, user_id, new_usernames);
        self.update_user(env, user_id, false, false);
    }

    /// `UserManager::on_update_active_usernames_order`
    /// (UserManager.cpp:6713-6723); the `reload_user` fallback for a
    /// stale local order is the net step.
    pub fn on_update_active_usernames_order(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        usernames: Vec<String>,
    ) {
        let new_usernames = {
            let u = self
                .get_user(user_id)
                .expect("the update flow forces the user");
            if !u.usernames.can_reorder_to(&usernames) {
                // `reload_user(user_id, std::move(promise),
                // "on_update_active_usernames_order")` (cpp:6587) — same
                // empty-promise form as the toggle twin
                tdutils::log_info!("Need to reload {user_id:?} after the username reorder");
                self.reload_user(
                    env,
                    user_id,
                    Promise::default(),
                    "on_update_active_usernames_order",
                );
                return;
            }
            u.usernames.reorder_to(usernames)
        };
        self.on_update_user_usernames(env, user_id, new_usernames);
        self.update_user(env, user_id, false, false);
    }

    /// `UserManager::toggle_bot_username_is_active`
    /// (UserManager.cpp:6725-6737) — the gates and the optimistic apply;
    /// the `ToggleBotUsernameQuery` send rides the net step.
    pub fn toggle_bot_username_is_active_local(
        &mut self,
        env: &mut UserEnv,
        bot_user_id: UserId,
        username: &str,
        is_active: bool,
    ) -> Result<(), Status> {
        let bot_data = self.get_bot_data(bot_user_id)?;
        if !bot_data.can_be_edited {
            return Err(Status::error(400, "The bot can't be edited"));
        }
        let new_usernames = {
            let u = self
                .get_user(bot_user_id)
                .expect("get_bot_data forced the bot");
            if !u.usernames.can_toggle(true, username) {
                return Err(Status::error(400, "Wrong username specified"));
            }
            u.usernames.toggle(true, username, is_active)
        };
        self.on_update_user_usernames(env, bot_user_id, new_usernames);
        self.update_user(env, bot_user_id, false, false);
        Ok(())
    }

    /// `UserManager::add_bot_username` (UserManager.cpp:6710-6726) —
    /// the gates and the optimistic apply; the `AddBotUsernameQuery`
    /// (bots.addUsername) send rides the net step.
    pub fn add_bot_username_local(
        &mut self,
        env: &mut UserEnv,
        bot_user_id: UserId,
        username: &str,
    ) -> Result<(), Status> {
        let bot_data = self.get_bot_data(bot_user_id)?;
        if !bot_data.can_be_edited {
            return Err(Status::error(400, "The bot can't be edited"));
        }
        if !env.options.get_option_boolean("is_premium", false) {
            return Err(Status::error(
                400,
                "The method is available only to Telegram Premium users",
            ));
        }
        let new_usernames = {
            let u = self
                .get_user(bot_user_id)
                .expect("get_bot_data forced the bot");
            if !u.usernames.can_add_secondary(username) {
                return Err(Status::error(400, "Can't add secondary username"));
            }
            u.usernames.add_secondary(username)
        };
        self.on_update_user_usernames(env, bot_user_id, new_usernames);
        self.update_user(env, bot_user_id, false, false);
        Ok(())
    }

    /// `UserManager::delete_bot_username` (UserManager.cpp:6728-6746) —
    /// deletes the first secondary username; the `RemoveBotUsernameQuery`
    /// (bots.removeUsername) send rides the net step.
    pub fn delete_bot_username_local(
        &mut self,
        env: &mut UserEnv,
        bot_user_id: UserId,
    ) -> Result<bool, Status> {
        let bot_data = self.get_bot_data(bot_user_id)?;
        if !bot_data.can_be_edited {
            return Err(Status::error(400, "The bot can't be edited"));
        }
        let new_usernames = {
            let u = self
                .get_user(bot_user_id)
                .expect("get_bot_data forced the bot");
            let deletable_usernames = u.usernames.get_deletable_usernames();
            if deletable_usernames.is_empty() {
                // the C++ promise.set_value(Unit()) — nothing to delete
                return Ok(false);
            }
            let username = deletable_usernames[0].clone();
            if !u.usernames.can_delete_secondary(&username) {
                return Err(Status::error(400, "Can't delete secondary username"));
            }
            u.usernames.delete_secondary(&username)
        };
        self.on_update_user_usernames(env, bot_user_id, new_usernames);
        self.update_user(env, bot_user_id, false, false);
        Ok(true)
    }

    /// `UserManager::reorder_bot_usernames`
    /// (UserManager.cpp:6739-6754) — the gates and the optimistic apply;
    /// the `ReorderBotUsernamesQuery` send rides the net step.
    pub fn reorder_bot_usernames_local(
        &mut self,
        env: &mut UserEnv,
        bot_user_id: UserId,
        usernames: &[String],
    ) -> Result<(), Status> {
        let bot_data = self.get_bot_data(bot_user_id)?;
        if !bot_data.can_be_edited {
            return Err(Status::error(400, "The bot can't be edited"));
        }
        let new_usernames = {
            let u = self
                .get_user(bot_user_id)
                .expect("get_bot_data forced the bot");
            if !u.usernames.can_reorder_to(&usernames.to_vec()) {
                return Err(Status::error(400, "Invalid username order specified"));
            }
            if usernames.len() <= 1 {
                return Ok(());
            }
            u.usernames.reorder_to(usernames.to_vec())
        };
        self.on_update_user_usernames(env, bot_user_id, new_usernames);
        self.update_user(env, bot_user_id, false, false);
        Ok(())
    }

    /// `UserManager::toggle_user_can_manage_emoji_status` (UserManager.cpp:6483-6491).
    pub fn toggle_user_can_manage_emoji_status(
        &mut self,
        env: &mut UserEnv,
        bot_user_id: UserId,
        can_manage_emoji_status: bool,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = self.get_input_user(env, bot_user_id) {
            promise.set_error(status);
            return;
        }
        if !self.is_user_bot_of(bot_user_id) {
            promise.set_error(Status::error(400, "The user must be a bot"));
            return;
        }
        let mut changed = false;
        if let Some(user_full) = self.users_full.get_mut(&bot_user_id.get())
            && user_full.can_manage_emoji_status != can_manage_emoji_status
        {
            user_full.can_manage_emoji_status = can_manage_emoji_status;
            changed = true;
        }
        if changed {
            self.update_user_full(
                env,
                bot_user_id,
                "on_update_bot_can_manage_emoji_status",
                false,
            );
        }
        promise.set_value(());
    }

    /// `UserManager::set_bot_profile_photo` (UserManager.cpp:5959-5979).
    pub fn set_bot_profile_photo(
        &mut self,
        _env: &mut UserEnv,
        is_auth_bot: bool,
        mut bot_user_id: UserId,
        _photo: Option<&crate::td_api::InputChatPhoto>,
        mut promise: Promise<()>,
    ) {
        if is_auth_bot {
            let my_id = self.get_my_id();
            if bot_user_id.is_valid() && bot_user_id != my_id {
                promise.set_error(Status::error(400, "Invalid bot user identifier specified"));
                return;
            }
            bot_user_id = my_id;
        } else {
            let bot_data = match self.get_bot_data(bot_user_id) {
                Ok(data) => data,
                Err(status) => {
                    promise.set_error(status);
                    return;
                }
            };
            if !bot_data.can_be_edited {
                promise.set_error(Status::error(400, "The bot can't be edited"));
                return;
            }
        }
        let _ = bot_user_id;
        promise.set_value(());
    }

    /// `UserManager::set_accent_color` (UserManager.cpp:6756-6765) — the
    /// local gate + the id-derived-default collapse, then the
    /// `UpdateColorQuery` send over the user net handle (the promise
    /// rides the family queue; the ok answer runs
    /// [`Self::on_update_accent_color_success`]).
    pub fn set_accent_color(
        &mut self,
        accent_color_id: AccentColorId,
        background_custom_emoji_id: CustomEmojiId,
        mut promise: Promise<()>,
    ) {
        if !accent_color_id.is_valid() {
            promise.set_error(Status::error(
                400,
                "Invalid accent color identifier specified",
            ));
            return;
        }
        let accent_color_id =
            if accent_color_id == AccentColorId::from_peer_id(self.get_my_id().get()) {
                AccentColorId::default()
            } else {
                accent_color_id
            };
        self.send_update_color_query(false, accent_color_id, background_custom_emoji_id, promise);
    }

    /// `UserManager::set_profile_accent_color` (UserManager.cpp:6767-6770)
    /// — no local gates (the C++ sends directly); the profile variant of
    /// the `UpdateColorQuery`.
    pub fn set_profile_accent_color(
        &mut self,
        profile_accent_color_id: AccentColorId,
        profile_background_custom_emoji_id: CustomEmojiId,
        promise: Promise<()>,
    ) {
        self.send_update_color_query(
            true,
            profile_accent_color_id,
            profile_background_custom_emoji_id,
            promise,
        );
    }

    /// `UpdateColorQuery::send` (UserManager.cpp:901-919) — the COLOR
    /// flag + the peerColor pair; the invalid accent id sends the bare
    /// (reset) form.
    fn send_update_color_query(
        &mut self,
        for_profile: bool,
        accent_color_id: AccentColorId,
        background_custom_emoji_id: CustomEmojiId,
        promise: Promise<()>,
    ) {
        use tdmtproto::telegram_api;
        let color = if accent_color_id.is_valid() {
            let mut color_flags = 1; // telegram_api::peerColor::COLOR_MASK
            if background_custom_emoji_id.is_valid() {
                color_flags |= 2; // peerColor::BACKGROUND_EMOJI_ID_MASK
            }
            Some(telegram_api::PeerColor::peerColor(
                telegram_api::peerColor {
                    flags: color_flags,
                    color: accent_color_id.get(),
                    background_emoji_id: background_custom_emoji_id.get(),
                },
            ))
        } else {
            None
        };
        self.pending_update_color_queries.push(promise);
        self.send_user_query(
            UserNetKind::UpdateColor {
                for_profile,
                accent_color_id,
                background_custom_emoji_id,
            },
            telegram_api::Function::account_updateColor(telegram_api::account_updateColor {
                flags: if color.is_some() { 4 } else { 0 }, // COLOR_MASK
                for_profile,
                color,
            }),
            "account.updateColor",
        );
    }

    /// `UserManager::on_update_accent_color_success`
    /// (UserManager.cpp:6776-6788).
    pub fn on_update_accent_color_success(
        &mut self,
        env: &mut UserEnv,
        for_profile: bool,
        accent_color_id: AccentColorId,
        background_custom_emoji_id: CustomEmojiId,
    ) {
        let user_id = self.get_my_id();
        if !self.get_user_force(env, user_id, "on_update_accent_color_success") {
            return;
        }
        if for_profile {
            self.on_update_user_profile_colors_impl(
                user_id,
                accent_color_id,
                background_custom_emoji_id,
            );
        } else {
            self.on_update_user_colors_impl(
                user_id,
                accent_color_id,
                background_custom_emoji_id,
                None,
            );
        }
        self.update_user(env, user_id, false, false);
    }

    /// `UserManager::on_update_user_is_blocked` +
    /// `on_update_user_full_is_blocked` (UserManager.cpp:4143-4168) —
    /// the full-record fan-in of the dialog block toggles.
    pub fn on_update_user_is_blocked(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        is_blocked: bool,
        is_blocked_for_stories: bool,
    ) {
        if !user_id.is_valid() {
            tdutils::log_error!("Receive invalid {user_id:?}");
            return;
        }
        if !self.get_user_full_force(env, user_id, "on_update_user_is_blocked") {
            return;
        }
        {
            let stored = self.users_full.get_mut(&user_id.get()).unwrap();
            if stored.is_blocked != is_blocked
                || stored.is_blocked_for_stories != is_blocked_for_stories
            {
                stored.is_blocked = is_blocked;
                stored.is_blocked_for_stories = is_blocked_for_stories;
                stored.is_changed = true;
            }
        }
        self.update_user_full(env, user_id, "on_update_user_is_blocked", false);
    }

    /// `UserManager::set_emoji_status` (UserManager.cpp:6746-6762) — the
    /// premium gate answers here, `add_recent_emoji_status` writes the
    /// recent registry, then the `UpdateEmojiStatusQuery` send rides the
    /// `net` handle (the cloned status and the promise ride the family
    /// queue; the ok answer runs [`Self::on_set_emoji_status`], the
    /// error band refetches the recent list first — cpp:1399-1401).
    pub fn set_emoji_status(
        &mut self,
        env: &mut UserEnv,
        emoji_status: Option<EmojiStatus>,
        mut promise: Promise<()>,
    ) {
        if !env.options.get_option_boolean("is_premium", false) {
            return promise.set_error(Status::error(
                400,
                "The method is available only to Telegram Premium users",
            ));
        }
        if let Some(status) = emoji_status.clone() {
            self.add_recent_emoji_status(env, status);
        }
        let input_status = EmojiStatus::get_input_emoji_status_of(emoji_status.as_ref());
        self.pending_update_emoji_status_queries
            .push((emoji_status, promise));
        self.send_user_query(
            UserNetKind::UpdateEmojiStatus,
            telegram_api::Function::account_updateEmojiStatus(
                telegram_api::account_updateEmojiStatus {
                    emoji_status: Some(input_status),
                },
            ),
            "account.updateEmojiStatus",
        );
    }

    /// `add_recent_emoji_status` (EmojiStatus.cpp:599-617) — the top of
    /// the recent registry (the themed skip is the DIVERGENCE at the
    /// body).
    fn add_recent_emoji_status(&mut self, env: &mut UserEnv, mut emoji_status: EmojiStatus) {
        if emoji_status.is_empty() {
            return;
        }
        // `StickersManager::is_default_emoji_status` themed skip —
        // DIVERGENCE(stickers plane): the special sticker sets are
        // unknown, so the band answers false and every status registers.
        emoji_status.clear_until_date();
        let mut statuses = crate::emoji_status::load_emoji_statuses(
            env.global.binlog_pmc(),
            crate::emoji_status::RECENT_EMOJI_STATUSES_DATABASE_KEY,
        );
        if statuses.emoji_statuses.first() == Some(&emoji_status) {
            return;
        }
        statuses.hash = 0;
        // add_to_top (td/utils/algorithm.h:115-137) — the server-side
        // list limit is 50.
        const MAX_RECENT_EMOJI_STATUSES: usize = 50;
        if let Some(pos) = statuses
            .emoji_statuses
            .iter()
            .position(|status| *status == emoji_status)
        {
            statuses.emoji_statuses.remove(pos);
        } else if statuses.emoji_statuses.len() >= MAX_RECENT_EMOJI_STATUSES {
            statuses.emoji_statuses.pop();
        }
        statuses.emoji_statuses.insert(0, emoji_status);
        let mut stored = Vec::new();
        statuses.store(&mut stored);
        crate::emoji_status::save_emoji_statuses_cache(
            env.global.binlog_pmc(),
            crate::emoji_status::RECENT_EMOJI_STATUSES_DATABASE_KEY,
            &stored,
        );
    }

    /// `UserManager::on_set_emoji_status` (UserManager.cpp:6765-6773)
    /// — the `UpdateEmojiStatusQuery` ok ack: the local-user apply.
    pub fn on_set_emoji_status(&mut self, env: &mut UserEnv, emoji_status: Option<EmojiStatus>) {
        let user_id = self.get_my_id();
        if self.get_user(user_id).is_some() {
            self.on_update_user_emoji_status(env, user_id, emoji_status);
            self.update_user(env, user_id, false, false);
        }
    }

    /// `UserManager::on_set_user_emoji_status`
    /// (UserManager.cpp:6568-6575) without the promise plumbing.
    pub fn on_set_user_emoji_status(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        emoji_status: Option<EmojiStatus>,
    ) {
        self.on_update_user_emoji_status(env, user_id, emoji_status);
        self.update_user(env, user_id, false, false);
    }

    /// `UserManager::on_set_birthdate` (UserManager.cpp:6790-6798)
    /// without the promise plumbing.
    pub fn on_set_birthdate(&mut self, env: &mut UserEnv, birthdate: Birthdate) {
        let my_user_id = self.get_my_id();
        if self.get_user_full_force(env, my_user_id, "on_set_birthdate") {
            let user_full = self.users_full.get_mut(&my_user_id.get()).unwrap();
            if user_full.birthdate != birthdate {
                user_full.birthdate = birthdate;
                user_full.is_changed = true;
                self.update_user_full(env, my_user_id, "on_set_birthdate", false);
            }
        }
    }

    /// `UserManager::on_toggle_sponsored_messages`
    /// (UserManager.cpp:6943-6953) without the promise plumbing; the
    /// `main_profile_tab`/`personal_channel` appliers carry the
    /// ProfileTab/ChannelId types into their subsystem steps.
    pub fn on_toggle_sponsored_messages(&mut self, env: &mut UserEnv, sponsored_enabled: bool) {
        let my_user_id = self.get_my_id();
        if self.get_user_full_force(env, my_user_id, "on_toggle_sponsored_messages") {
            let user_full = self.users_full.get_mut(&my_user_id.get()).unwrap();
            if user_full.sponsored_enabled != sponsored_enabled {
                user_full.sponsored_enabled = sponsored_enabled;
                user_full.is_changed = true;
                self.update_user_full(env, my_user_id, "on_toggle_sponsored_messages", false);
            }
        }
    }

    // ------------------------------------------------------------------
    // M.4 tails: the profile-note/tab/birthdate setters, the photo-set
    // family and the support-user query planes
    // (UserManager.cpp:5981-6835, 7178-7212). The setters' and
    // support-user sends and the profile-photo families ride the `net`
    // handle (the emoji-status planes since d5c9b66, the setters and
    // support-user since 43539e1, the §3.1 #9/#11-#14 plane here); the
    // upload tails stay the Files-slice seams.

    /// `UserManager::set_user_note` (UserManager.cpp:6440-6466). The
    /// contact/self gates and the formatted-text normalization answer
    /// here; the `UpdateContactNoteQuery` send rides the `net` handle
    /// — the user id, the note and the promise ride the family queue
    /// to the `on_set_user_note` ack. The entity-plane `user_manager`
    /// plumbing rides the MessageContentContext integration — the
    /// `None` input is the documented `()` stand-in.
    pub fn set_user_note(
        &mut self,
        env: &UserEnv,
        user_id: UserId,
        note: Option<td_api::FormattedText>,
        mut promise: Promise<()>,
    ) {
        let input_user = match self.get_input_user(env, user_id) {
            Ok(input_user) => input_user,
            Err(error) => return promise.set_error(error),
        };
        if !self.is_user_contact_of(user_id, false) {
            return promise.set_error(Status::error(400, "User isn't a contact"));
        }
        if user_id == self.get_my_id() {
            return promise.set_error(Status::error(400, "Can't set note to self"));
        }

        let note_text = match crate::message_entity::get_formatted_text_of_input::<()>(
            None,
            crate::message_entity::DialogId::None,
            note,
            false,
            true,
            true,
            false,
            None,
            env.options
                .get_option_boolean("always_parse_markdown", false),
            env.options
                .get_option_integer("session_count", 1)
                .try_into()
                .unwrap(),
            None,
            false,
        ) {
            Ok(mut note_text) => {
                crate::message_entity::remove_unallowed_quote_entities(&mut note_text);
                note_text
            }
            Err(error) => return promise.set_error(error),
        };
        self.pending_update_contact_note_queries
            .push((user_id, note_text.clone(), promise));
        self.send_user_query(
            UserNetKind::UpdateContactNote,
            telegram_api::Function::contacts_updateContactNote(
                telegram_api::contacts_updateContactNote {
                    id: Some(input_user),
                    note: Some(crate::message_entity::get_input_text_with_entities::<()>(
                        None,
                        &note_text,
                        "UpdateContactNoteQuery",
                    )),
                },
            ),
            "contacts.updateContactNote",
        );
    }

    /// `UserManager::on_set_user_note` (UserManager.cpp:6465-6469) —
    /// the `UpdateContactNoteQuery` ack: the note write is
    /// `on_update_user_note` (cpp:4657-4663) over the `UserFull::note`
    /// field, the FormattedText-subsystem store — the write lands with
    /// the formatted-text plane (the field is the store's `has_note`
    /// residue); the promise answers here.
    pub fn on_set_user_note(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        note: crate::message_entity::FormattedText,
        mut promise: Promise<()>,
    ) {
        // on_update_user_note (cpp:4657-4663) — the UserFull::note write
        // rides the FormattedText subsystem (§4).
        let _ = (env, user_id, note);
        promise.set_value(());
    }

    /// `UserManager::set_main_profile_tab` (UserManager.cpp:6688-6701)
    /// with the td_api `get_profile_tab` ladder (ProfileTab.cpp:61-95):
    /// the null gate and the `ChannelType::Unknown` allow-list answer
    /// here; the `SetMainProfileTabQuery` send rides the `net` handle
    /// (the tab and the promise ride the family queue; the ack is
    /// [`Self::on_set_main_profile_tab`], cpp:6702).
    pub fn set_main_profile_tab(
        &mut self,
        main_profile_tab: Option<td_api::ProfileTab>,
        mut promise: Promise<()>,
    ) {
        let Some(profile_tab) = main_profile_tab else {
            return promise.set_error(Status::error(400, "Profile tab must be non-empty"));
        };
        let profile_tab = match profile_tab {
            td_api::ProfileTab::profileTabPosts(_) => ProfileTab::Posts,
            td_api::ProfileTab::profileTabGifts(_) => ProfileTab::Gifts,
            _ => {
                return promise.set_error(Status::error(
                    400,
                    "Invalid profile tab specified for the chat",
                ));
            }
        };
        self.pending_set_main_profile_tab_queries
            .push((profile_tab, promise));
        self.send_user_query(
            UserNetKind::SetMainProfileTab,
            telegram_api::Function::account_setMainProfileTab(
                telegram_api::account_setMainProfileTab {
                    tab: Some(profile_tab.get_input_profile_tab()),
                },
            ),
            "account.setMainProfileTab",
        );
    }

    /// `UserManager::on_set_main_profile_tab` (UserManager.cpp:6702-6711)
    /// — the `SetMainProfileTabQuery` ack: the main-profile-tab write of
    /// my user_full record.
    pub fn on_set_main_profile_tab(
        &mut self,
        env: &mut UserEnv,
        main_profile_tab: ProfileTab,
        mut promise: Promise<()>,
    ) {
        let my_user_id = self.get_my_id();
        if self.get_user_full_force(env, my_user_id, "on_set_main_profile_tab") {
            let user_full = self.users_full.get_mut(&my_user_id.get()).unwrap();
            if user_full.main_profile_tab != main_profile_tab {
                user_full.main_profile_tab = main_profile_tab;
                user_full.is_changed = true;
                self.update_user_full(env, my_user_id, "on_set_main_profile_tab", false);
            }
        }
        promise.set_value(());
    }

    /// `UserManager::suggest_user_birthdate` (UserManager.cpp:6471-6480):
    /// the input/self/bot gates answer here; the
    /// `SuggestUserBirthdayQuery` send rides the `net` handle — the
    /// promise rides the family queue into the Updates answer's
    /// `on_get_updates` call (cpp:876-881).
    pub fn suggest_user_birthdate(
        &mut self,
        env: &UserEnv,
        user_id: UserId,
        birthdate: td_api::Birthdate,
        mut promise: Promise<()>,
    ) {
        let input_user = match self.get_input_user(env, user_id) {
            Ok(input_user) => input_user,
            Err(error) => return promise.set_error(error),
        };
        if user_id == self.get_my_id() {
            return promise.set_error(Status::error(400, "Can't suggest birthdate to self"));
        }
        if self.is_user_bot_of(user_id) {
            return promise.set_error(Status::error(400, "Can't suggest birthdate to bots"));
        }
        let birthdate = Birthdate::new(birthdate.day, birthdate.month, birthdate.year);
        self.pending_suggest_birthday_queries.push(promise);
        self.send_user_query(
            UserNetKind::SuggestBirthday,
            telegram_api::Function::users_suggestBirthday(telegram_api::users_suggestBirthday {
                id: Some(input_user),
                birthday: Some(birthdate.get_input_birthday()),
            }),
            "users.suggestBirthday",
        );
    }

    /// `UserManager::add_new_saved_music` (UserManager.cpp:7178-7216):
    /// the null/UTF-8 gates answer here, then the `get_input_file_id`
    /// resolution, the encrypted/web gates, the remote fast path
    /// (`add_saved_music`), the `create_audio` registration and the
    /// upload tail over the td-plane pair. The url variant skips the
    /// upload and runs `do_upload_saved_music` directly (cpp:7213).
    pub fn add_new_saved_music(
        &mut self,
        td: SavedMusicUploadTd<'_>,
        input_audio: Option<td_api::InputAudio>,
        mut promise: Promise<()>,
    ) {
        let SavedMusicUploadTd {
            files,
            fllm,
            upload_callback,
            audios,
            close_flag,
        } = td;
        let Some(input_audio) = input_audio else {
            return promise.set_error(Status::error(400, "Audio must be non-empty"));
        };
        if crate::misc::clean_input_string(&input_audio.title).is_none()
            || crate::misc::clean_input_string(&input_audio.performer).is_none()
        {
            return promise.set_error(Status::error(400, "Strings must be encoded in UTF-8"));
        }
        let file_id = match files.get_input_file_id(
            crate::files::FileType::Audio,
            input_audio.audio.as_ref(),
            crate::message_id::DialogId::default(),
            false,
            false,
            false,
            false,
            false,
            false,
        ) {
            Ok(file_id) => file_id,
            Err(error) => return promise.set_error(error),
        };
        debug_assert!(file_id.is_valid(), "CHECK(file_id.is_valid())");

        let (is_encrypted, is_web, has_remote, has_url) = files
            .get_file_view(file_id)
            .map(|view| {
                (
                    view.is_encrypted(),
                    view.get_main_remote_location().is_some_and(|l| l.is_web()),
                    view.get_main_remote_location().is_some(),
                    view.has_url(),
                )
            })
            .unwrap_or((false, false, false, false));
        if is_encrypted {
            return promise.set_error(Status::error(400, "Can't use encrypted file"));
        }
        if is_web {
            return promise.set_error(Status::error(
                400,
                "Can't use web file to create a profile audio",
            ));
        }

        if has_remote {
            return self.add_saved_music(file_id, FileId::default(), promise);
        }

        audios.create_audio(
            file_id,
            String::new(),
            crate::photo::PhotoSize::default(),
            String::new(),
            String::new(),
            input_audio.duration,
            input_audio.title,
            input_audio.performer,
            0,
            false,
        );
        if has_url {
            let file_upload_id = FileUploadId::new(file_id, files.get_internal_upload_id());
            let input_media = audios
                .get_input_media_with_files(files, file_id, None, None)
                .expect("CHECK(input_media != nullptr)");
            self.do_upload_saved_music(
                close_flag,
                files,
                fllm,
                file_upload_id,
                false,
                input_media,
                promise,
            );
        } else {
            self.upload_saved_music(files, fllm, upload_callback, file_id, promise);
        }
    }

    /// `UserManager::upload_saved_music` (UserManager.cpp:7220-7228) —
    /// park the promise on the upload id and ask the file plane
    /// (`upload_saved_music_callback_`, priority 2, order 0).
    fn upload_saved_music(
        &mut self,
        files: &mut crate::files::FileManager,
        fllm: &mut crate::files::FileLoadManager,
        upload_callback: &std::sync::Arc<parking_lot::Mutex<dyn crate::files::UploadCallback>>,
        file_id: FileId,
        promise: Promise<()>,
    ) {
        let file_upload_id = FileUploadId::new(file_id, files.get_internal_upload_id());
        debug_assert!(
            self.being_uploaded_saved_music_files
                .insert(file_upload_id, promise)
                .is_none(),
            "CHECK(is_inserted)"
        );
        tdutils::log_info!("Ask to upload profile audio {file_upload_id:?}");
        files.upload(file_upload_id, upload_callback.clone(), 2, 0, fllm);
    }

    /// `UserManager::on_upload_saved_music` (UserManager.cpp:7230-7244)
    /// — the `UploadSavedMusicCallback::on_upload_ok` hop: the parked
    /// promise pops and the flow continues in
    /// [`Self::do_upload_saved_music`]. The input media is resolved by
    /// the Td-side caller (the audios env locks the file manager; the
    /// C++ reaches both through the single actor); the close gate is
    /// the drain site's (the entry stays parked at close, like the C++
    /// early return).
    pub(crate) fn take_saved_music_upload_promise(
        &mut self,
        file_upload_id: FileUploadId,
    ) -> Promise<()> {
        self.being_uploaded_saved_music_files
            .remove(&file_upload_id)
            .expect("the callback fires for a registered upload")
    }

    /// `UserManager::on_upload_saved_music_error`
    /// (UserManager.cpp:7246-7261) — the error hop: the code band
    /// (positive codes pass, anything else 500). The close gate is the
    /// drain site's (the entry stays parked at close).
    pub(crate) fn on_upload_saved_music_error(
        &mut self,
        file_upload_id: FileUploadId,
        status: Status,
    ) {
        tdutils::log_warning!("Profile audio {file_upload_id:?} has upload error {status}");
        debug_assert!(status.is_error(), "CHECK(status.is_error())");
        let mut promise = self
            .being_uploaded_saved_music_files
            .remove(&file_upload_id)
            .expect("the callback fires for a registered upload");
        promise.set_error(if status.code() > 0 {
            status
        } else {
            Status::error(500, status.message())
        });
    }

    /// `UserManager::do_upload_saved_music` (UserManager.cpp:7263-7282)
    /// — the `messages.uploadMedia` send over the audios input media
    /// (`CHECK(input_media != nullptr)` is the caller's expect).
    /// `had_input_file` is the url-vs-upload split (`is_url_ =
    /// !had_input_file`). The `close_flag` is the
    /// `TRY_STATUS_PROMISE(promise, G()->close_status())` gate
    /// (cpp:7264).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn do_upload_saved_music(
        &mut self,
        close_flag: bool,
        files: &mut crate::files::FileManager,
        fllm: &mut crate::files::FileLoadManager,
        file_upload_id: FileUploadId,
        had_input_file: bool,
        input_media: telegram_api::InputMedia,
        mut promise: Promise<()>,
    ) {
        if close_flag {
            return promise.set_error(Status::error(500, "Request aborted"));
        }
        let was_uploaded = crate::files::extract_was_uploaded(&input_media);
        if had_input_file && !was_uploaded {
            // if we had InputFile, but has failed to use it for
            // input_media, then we need to immediately cancel file
            // upload so the next upload with the same file can succeed
            files.cancel_upload(file_upload_id, fllm);
        }

        self.pending_upload_saved_music_queries.push(promise);
        match &self.net {
            Some(net) => net.dispatch(
                UserNetKind::UploadSavedMusicFile {
                    file_upload_id,
                    is_url: !had_input_file,
                    was_uploaded,
                },
                telegram_api::Function::messages_uploadMedia(telegram_api::messages_uploadMedia {
                    flags: 0,
                    business_connection_id: String::new(),
                    peer: Some(Box::new(telegram_api::InputPeer::inputPeerSelf(
                        telegram_api::inputPeerSelf,
                    ))),
                    media: Some(Box::new(input_media)),
                }),
            ),
            None => tdutils::log_info!("Send messages.uploadMedia — [net-inert]"),
        }
    }

    /// `UserManager::get_support_user` (UserManager.cpp:6798-6815): the
    /// cached id answers here; the `GetSupportUserQuery` send rides the
    /// `net` handle and [`Self::on_get_support_user`] is its ack
    /// intake (the answer's `on_get_user` runs at the parse leg).
    pub fn get_support_user(&mut self, env: &UserEnv, mut promise: Promise<td_api::User>) {
        if self.support_user_id.is_valid() {
            promise.set_value(self.get_user_object_of(env, self.support_user_id));
            return;
        }
        self.pending_support_user_queries.push(promise);
        self.send_user_query(
            UserNetKind::GetSupport,
            telegram_api::Function::help_getSupport(telegram_api::help_getSupport),
            "help.getSupport",
        );
    }

    /// `GetSupportUserQuery::on_error` (cpp:1766-1769) — the query
    /// promise's error band fails the parked promises (the C++ fails
    /// the one promise the query carries).
    pub(crate) fn fail_pending_support_user_queries(&mut self, status: Status) {
        for mut promise in self.pending_support_user_queries.drain(..) {
            promise.set_error(status.clone());
        }
    }

    /// `UserManager::on_get_support_user` (UserManager.cpp:6817-6831) —
    /// the net-query ack intake; the parked promises resolve with the
    /// fresh object, like the C++ query promise.
    pub fn on_get_support_user(
        &mut self,
        env: &UserEnv,
        user_id: UserId,
        mut promise: Promise<td_api::User>,
    ) {
        let is_support = match self.get_user(user_id) {
            Some(u) => u.is_support,
            None => return promise.set_error(Status::error(500, "Can't find support user")),
        };
        if !is_support {
            tdutils::log_error!("Receive non-support {user_id:?}, but expected a support user");
        }
        self.support_user_id = user_id;
        let object = self.get_user_object_of(env, user_id);
        self.pending_support_user_queries.push(promise);
        for mut parked in self.pending_support_user_queries.drain(..) {
            parked.set_value(object.clone());
        }
    }

    /// `UserManager::set_profile_photo` (UserManager.cpp:5996-6002); the
    /// `UserpicSetup` suggested-action dismissal is the caller's seam
    /// (the Td-level arm hosts it like the Payments-side dismiss). The
    /// td-plane pair is the `td_` reach of the flows (the prev-photo
    /// file view, the upload arms' file plane, the sticker reads).
    pub fn set_profile_photo(
        &mut self,
        td: SetProfilePhotoTd<'_>,
        input_photo: Option<&td_api::InputChatPhoto>,
        is_fallback: bool,
        promise: Promise<()>,
    ) {
        let my_id = self.get_my_id();
        self.set_profile_photo_impl(td, my_id, input_photo, is_fallback, false, promise);
    }

    /// `UserManager::set_profile_photo_impl` (UserManager.cpp:6004-6070)
    /// — the validation bands answer here; the prev-photo arm rides the
    /// `UpdateProfilePhotoQuery` dispatcher (cpp:662) over the file
    /// view's input photo, the static/animation arms upload over the
    /// file plane then ride the `UploadProfilePhotoQuery` dispatcher
    /// (cpp:531), and the sticker arm resolves the
    /// `StickerPhotoSize` over the td_api input (the C++
    /// `get_sticker_photo_size`, StickerPhotoSize.cpp:14-69) before the
    /// same query.
    pub fn set_profile_photo_impl(
        &mut self,
        td: SetProfilePhotoTd<'_>,
        user_id: UserId,
        input_photo: Option<&td_api::InputChatPhoto>,
        is_fallback: bool,
        only_suggest: bool,
        mut promise: Promise<()>,
    ) {
        let Some(input_photo) = input_photo else {
            return promise.set_error(Status::error(400, "New profile photo must be non-empty"));
        };
        let SetProfilePhotoTd {
            env,
            files,
            fllm,
            upload_callback,
            stickers,
        } = td;
        let input_file: Option<&td_api::InputFile>;
        let mut main_frame_timestamp = 0.0;
        let mut is_animation = false;
        match input_photo {
            td_api::InputChatPhoto::inputChatPhotoPrevious(photo) => {
                if user_id != self.get_my_id() || env.auth_is_bot {
                    return promise
                        .set_error(Status::error(400, "Can't use inputChatPhotoPrevious"));
                }
                let photo_id = photo.chat_photo_id;
                let file_id = self.get_profile_photo_file_id(photo_id);
                // the C++ gate is `get_profile_photo_file_id().is_valid()`;
                // the map membership is the placeholder-world equivalent —
                // the registered photo file ids stay the invalid
                // placeholders until the Files step wires the registration
                if !self.my_photo_file_id.contains_key(&photo_id) {
                    return promise
                        .set_error(Status::error(400, "Unknown profile photo ID specified"));
                }
                // `send_update_profile_photo_query`'s file-view read
                // (cpp:6094-6107): the input photo comes from the
                // registered photo's main remote location
                let input_photo = files.get_file_view(file_id).and_then(|file_view| {
                    file_view.get_main_remote_location().map(|location| {
                        telegram_api::InputPhoto::inputPhoto(location.as_input_photo())
                    })
                });
                let Some(input_photo) = input_photo else {
                    return promise.set_error(Status::error(500, "Failed to upload the file"));
                };
                self.send_update_profile_photo_query(
                    env,
                    user_id,
                    photo_id,
                    is_fallback,
                    input_photo,
                    promise,
                );
                return;
            }
            td_api::InputChatPhoto::inputChatPhotoStatic(photo) => {
                input_file = photo.photo.as_ref();
            }
            td_api::InputChatPhoto::inputChatPhotoAnimation(photo) => {
                input_file = photo.animation.as_ref();
                main_frame_timestamp = photo.main_frame_timestamp;
                is_animation = true;
            }
            td_api::InputChatPhoto::inputChatPhotoSticker(photo) => {
                // `StickerPhotoSize::get_sticker_photo_size(td_, sticker)`
                // (StickerPhotoSize.cpp:14-69) — the null/type/fill bands
                // and the have_sticker/have_custom_emoji reads over the
                // stickers surface
                let sticker_photo_size =
                    match crate::photo::StickerPhotoSize::get_sticker_photo_size_td_api(
                        photo.sticker.as_ref(),
                        |sticker_set_id, sticker_id| {
                            stickers.have_sticker(
                                crate::stickers_manager::StickerSetId::new(sticker_set_id),
                                sticker_id,
                            )
                        },
                        |custom_emoji_id| {
                            stickers.have_custom_emoji(crate::stickers_manager::CustomEmojiId::new(
                                custom_emoji_id,
                            ))
                        },
                    ) {
                        Ok(sticker_photo_size) => sticker_photo_size,
                        Err(error) => return promise.set_error(error),
                    };
                self.send_upload_profile_photo_query_sticker(
                    env,
                    files,
                    fllm,
                    user_id,
                    stickers,
                    sticker_photo_size,
                    is_fallback,
                    only_suggest,
                    promise,
                );
                return;
            }
        }

        const MAX_ANIMATION_DURATION: f64 = 10.0;
        if !(0.0..=MAX_ANIMATION_DURATION).contains(&main_frame_timestamp) {
            return promise.set_error(Status::error(400, "Wrong main frame timestamp specified"));
        }

        let file_type = if is_animation {
            crate::files::FileType::Animation
        } else {
            crate::files::FileType::Photo
        };
        let file_id = match files.get_input_file_id(
            file_type,
            input_file,
            crate::message_id::DialogId::from_user_id(user_id),
            false,
            false,
            false,
            false,
            false,
            true,
        ) {
            Ok(file_id) => file_id,
            Err(error) => return promise.set_error(error),
        };

        let file_upload_id = FileUploadId::new(file_id, files.get_internal_upload_id());
        self.upload_profile_photo(
            env,
            files,
            fllm,
            upload_callback,
            user_id,
            file_upload_id,
            is_fallback,
            only_suggest,
            is_animation,
            main_frame_timestamp,
            promise,
            0,
            Vec::new(),
        );
    }

    /// `UserManager::set_user_profile_photo` (UserManager.cpp:6071-6097)
    /// — serves both setUserPersonalProfilePhoto and
    /// suggestUserProfilePhoto (`only_suggest`); the null-photo arm
    /// rides the `DeleteContactProfilePhotoQuery` dispatcher (cpp:766).
    pub fn set_user_profile_photo(
        &mut self,
        td: SetProfilePhotoTd<'_>,
        user_id: UserId,
        input_photo: Option<&td_api::InputChatPhoto>,
        only_suggest: bool,
        mut promise: Promise<()>,
    ) {
        let SetProfilePhotoTd { env, .. } = &td;
        let input_user = match self.get_input_user(env, user_id) {
            Ok(input_user) => input_user,
            Err(error) => return promise.set_error(error),
        };
        if !only_suggest && !self.is_user_contact_of(user_id, false) {
            return promise.set_error(Status::error(400, "User isn't a contact"));
        }
        if user_id == self.get_my_id() {
            return promise.set_error(Status::error(
                400,
                "Can't set personal or suggest photo to self",
            ));
        }
        if self.is_user_bot_of(user_id) {
            return promise.set_error(Status::error(
                400,
                "Can't set personal or suggest photo to bots",
            ));
        }
        let Some(input_photo) = input_photo else {
            if only_suggest {
                return promise.set_error(Status::error(400, "Photo must be non-empty"));
            }
            self.send_delete_contact_profile_photo_query(user_id, input_user, promise);
            return;
        };
        self.set_profile_photo_impl(td, user_id, Some(input_photo), false, only_suggest, promise);
    }

    /// `UserManager::get_profile_photo_file_id` (UserManager.cpp — the
    /// `my_photo_file_id_` lookup).
    fn get_profile_photo_file_id(&self, photo_id: i64) -> FileId {
        self.my_photo_file_id
            .get(&photo_id)
            .copied()
            .unwrap_or_default()
    }

    /// `UpdateProfilePhotoQuery::send` (cpp:671-691) over an already
    /// built input photo — the prev-photo arm reads the file view first
    /// (`send_update_profile_photo_query`, cpp:6094-6107), the
    /// `inputPhotoEmpty` reset arm of `delete_profile_photo` bypasses
    /// it (cpp:6418). The file-reference repair chain of the handler's
    /// `on_error` (cpp:696-718) rides the Files plane.
    fn send_update_profile_photo_query(
        &mut self,
        env: &UserEnv,
        user_id: UserId,
        old_photo_id: i64,
        is_fallback: bool,
        input_photo: telegram_api::InputPhoto,
        mut promise: Promise<()>,
    ) {
        // `TRY_STATUS_PROMISE(promise, G()->close_status())` (cpp:6096)
        if env.global.close_flag() {
            return promise.set_error(Status::error(500, "Request aborted"));
        }
        let bot = if user_id != self.get_my_id() {
            match self.get_input_user(env, user_id) {
                Ok(input_user) => Some(input_user),
                Err(error) => return promise.set_error(error),
            }
        } else {
            None
        };
        self.pending_update_profile_photo_queries.push(promise);
        let function = telegram_api::photos_updateProfilePhoto {
            // the bot field rides the flags bit 1 (the C++ BOT_MASK)
            flags: if bot.is_some() { 1 << 1 } else { 0 },
            fallback: is_fallback,
            bot,
            id: Some(input_photo),
        };
        match self.net.clone() {
            Some(net) => net.dispatch(
                UserNetKind::UpdateProfilePhoto {
                    user_id,
                    old_photo_id,
                    is_fallback,
                },
                telegram_api::Function::photos_updateProfilePhoto(function),
            ),
            None => tdutils::log_info!("Send photos.updateProfilePhoto — [net-inert]"),
        }
    }

    /// `UserManager::upload_profile_photo` (UserManager.cpp:6120-6143)
    /// — park the `UploadedProfilePhoto` members on the upload id and
    /// ask the file plane (`upload_profile_photo_callback_`, priority
    /// 32, order 0; the `[-1]` bad-parts re-entry of the force-reupload
    /// ladder rides `resume_upload`).
    #[allow(clippy::too_many_arguments)]
    fn upload_profile_photo(
        &mut self,
        env: &UserEnv,
        files: &mut crate::files::FileManager,
        fllm: &mut crate::files::FileLoadManager,
        upload_callback: &std::sync::Arc<parking_lot::Mutex<dyn crate::files::UploadCallback>>,
        user_id: UserId,
        file_upload_id: FileUploadId,
        is_fallback: bool,
        only_suggest: bool,
        is_animation: bool,
        main_frame_timestamp: f64,
        mut promise: Promise<()>,
        reupload_count: i32,
        bad_parts: Vec<i32>,
    ) {
        // `TRY_STATUS_PROMISE(promise, G()->close_status())` (cpp:6121)
        if env.global.close_flag() {
            return promise.set_error(Status::error(500, "Request aborted"));
        }
        debug_assert!(
            file_upload_id.is_valid(),
            "CHECK(file_upload_id.is_valid())"
        );
        let is_inserted = self
            .being_uploaded_profile_photos
            .insert(
                file_upload_id,
                UploadedProfilePhoto {
                    user_id,
                    is_fallback,
                    only_suggest,
                    main_frame_timestamp,
                    is_animation,
                    reupload_count,
                    promise,
                },
            )
            .is_none();
        debug_assert!(is_inserted, "CHECK(is_inserted)");
        tdutils::log_info!(
            "Ask to upload {} profile photo {file_upload_id:?} for user {user_id:?} with bad parts {bad_parts:?}",
            if is_animation { "animated" } else { "static" }
        );
        // TODO use force_reupload if reupload_count >= 1, replace
        // reupload_count with is_reupload (the C++ carries the same TODO)
        files.resume_upload(
            file_upload_id,
            bad_parts,
            upload_callback.clone(),
            32,
            0,
            false,
            false,
            fllm,
        );
    }

    /// `UserManager::on_upload_profile_photo` (UserManager.cpp:6145-6194)
    /// — the `UploadProfilePhotoCallback::on_upload_ok` hop: the
    /// reupload ladder over the main remote (the web band, the
    /// reupload-count cap, the file-reference drop) or the query send
    /// with the resolved `InputFile`. The close gate is the drain
    /// site's (the entry stays parked at close).
    pub(crate) fn on_upload_profile_photo(
        &mut self,
        td: SetProfilePhotoTd<'_>,
        file_upload_id: FileUploadId,
        input_file: Option<telegram_api::InputFile>,
    ) {
        let SetProfilePhotoTd {
            env,
            files,
            fllm,
            upload_callback,
            stickers,
        } = td;
        let UploadedProfilePhoto {
            user_id,
            is_fallback,
            only_suggest,
            main_frame_timestamp,
            is_animation,
            reupload_count,
            mut promise,
        } = self
            .being_uploaded_profile_photos
            .remove(&file_upload_id)
            .expect("the callback fires for a registered upload");
        tdutils::log_info!(
            "Uploaded {} profile photo {file_upload_id:?} for user {user_id:?} with reupload_count = {reupload_count}",
            if is_animation { "animated" } else { "static" }
        );
        let main_remote_location = files
            .get_file_view(file_upload_id.file_id)
            .and_then(|view| view.get_main_remote_location().cloned());
        if let (Some(main_remote_location), None) = (&main_remote_location, &input_file) {
            if main_remote_location.is_web() {
                return promise
                    .set_error(Status::error(400, "Can't use web photo as profile photo"));
            }
            if reupload_count == 3 {
                // upload, ForceReupload repair file reference, reupload
                return promise.set_error(Status::error(400, "Failed to reupload the file"));
            }

            // delete file reference and forcibly reupload the file
            // (the C++ LOG_CHECKs are the log-only collapse's)
            let file_reference = if is_animation {
                main_remote_location.as_input_document().file_reference
            } else {
                main_remote_location.as_input_photo().file_reference
            };
            files.delete_file_reference(file_upload_id.file_id, &file_reference);
            self.upload_profile_photo(
                env,
                files,
                fllm,
                upload_callback,
                user_id,
                file_upload_id,
                is_fallback,
                only_suggest,
                is_animation,
                main_frame_timestamp,
                promise,
                reupload_count + 1,
                vec![-1],
            );
            return;
        }
        let Some(input_file) = input_file else {
            // `CHECK(input_file != nullptr)` (cpp:6187) — the null file
            // without a main remote is the upload-canceled race
            return promise.set_error(Status::error(500, "Failed to upload the file"));
        };
        self.send_upload_profile_photo_query(
            env,
            files,
            fllm,
            user_id,
            file_upload_id,
            input_file,
            is_fallback,
            only_suggest,
            is_animation,
            main_frame_timestamp,
            promise,
        );
        let _ = stickers;
    }

    /// `UserManager::on_upload_profile_photo_error`
    /// (UserManager.cpp:6196-6211) — the error hop.
    pub(crate) fn on_upload_profile_photo_error(
        &mut self,
        file_upload_id: FileUploadId,
        status: Status,
    ) {
        tdutils::log_info!("Profile photo {file_upload_id:?} has upload error {status}");
        debug_assert!(status.is_error(), "CHECK(status.is_error())");
        let Some(UploadedProfilePhoto { mut promise, .. }) =
            self.being_uploaded_profile_photos.remove(&file_upload_id)
        else {
            return;
        };
        // TODO check that status has valid error code (the C++ carries
        // the same TODO)
        promise.set_error(status);
    }

    /// `UploadProfilePhotoQuery::send` — the file form (cpp:539-584):
    /// the own-photo ladder picks `photos.uploadProfilePhoto` over
    /// `photos.uploadContactProfilePhoto` (the personal/suggest save).
    /// The `files`/`fllm` pair serves the handler's `on_error` tail
    /// (cpp:628-633) on the `get_input_user` failure.
    #[allow(clippy::too_many_arguments)]
    fn send_upload_profile_photo_query(
        &mut self,
        env: &UserEnv,
        files: &mut crate::files::FileManager,
        fllm: &mut crate::files::FileLoadManager,
        user_id: UserId,
        file_upload_id: FileUploadId,
        input_file: telegram_api::InputFile,
        is_fallback: bool,
        only_suggest: bool,
        is_animation: bool,
        main_frame_timestamp: f64,
        mut promise: Promise<()>,
    ) {
        debug_assert!(
            file_upload_id.is_valid(),
            "CHECK(file_upload_id.is_valid())"
        );
        // the pinned scheme's flag bits (telegram_api.tl:2857-2860)
        const FILE_MASK: i32 = 1 << 0;
        const VIDEO_MASK: i32 = 1 << 1;
        const VIDEO_START_TS_MASK: i32 = 1 << 2;
        const BOT_MASK: i32 = 1 << 5;

        let mut flags = 0;
        let mut photo_input_file = None;
        let mut video_input_file = None;
        if is_animation {
            flags |= VIDEO_MASK;
            video_input_file = Some(input_file);
            if main_frame_timestamp != 0.0 {
                flags |= VIDEO_START_TS_MASK;
            }
        } else {
            flags |= FILE_MASK;
            photo_input_file = Some(input_file);
        }
        let is_me = user_id == self.get_my_id();
        let input_user = if !is_me {
            match self.get_input_user(env, user_id) {
                Ok(input_user) => Some(input_user),
                Err(error) => {
                    // the handler's `on_error` tail (cpp:628-633): the
                    // partial remote drops before the promise fails
                    if file_upload_id.is_valid() {
                        files.delete_partial_remote_location(file_upload_id, fllm);
                    }
                    return promise.set_error(error);
                }
            }
        } else {
            None
        };
        if self.is_user_bot_of(user_id) != env.auth_is_bot || is_me {
            if input_user.is_some() {
                flags |= BOT_MASK;
            }
            self.pending_upload_profile_photo_queries.push(promise);
            let function = telegram_api::photos_uploadProfilePhoto {
                flags,
                fallback: is_fallback,
                bot: input_user,
                file: photo_input_file,
                video: video_input_file,
                video_start_ts: main_frame_timestamp,
                video_emoji_markup: None,
            };
            match self.net.clone() {
                Some(net) => net.dispatch(
                    UserNetKind::UploadProfilePhoto {
                        user_id,
                        is_fallback,
                        only_suggest,
                        file_upload_id,
                    },
                    telegram_api::Function::photos_uploadProfilePhoto(function),
                ),
                None => tdutils::log_info!("Send photos.uploadProfilePhoto — [net-inert]"),
            }
        } else {
            self.pending_upload_profile_photo_queries.push(promise);
            // `suggest`/`save` ride their own flags bits (3/4) — the
            // serializer ORs them in; the FILE/VIDEO/VIDEO_START_TS bits
            // match the profile ctor's (the C++ static_asserts)
            let function = telegram_api::photos_uploadContactProfilePhoto {
                flags,
                suggest: only_suggest,
                save: !only_suggest,
                user_id: input_user,
                file: photo_input_file,
                video: video_input_file,
                video_start_ts: main_frame_timestamp,
                video_emoji_markup: None,
            };
            match self.net.clone() {
                Some(net) => net.dispatch(
                    UserNetKind::UploadProfilePhoto {
                        user_id,
                        is_fallback,
                        only_suggest,
                        file_upload_id,
                    },
                    telegram_api::Function::photos_uploadContactProfilePhoto(function),
                ),
                None => {
                    tdutils::log_info!("Send photos.uploadContactProfilePhoto — [net-inert]")
                }
            }
        }
    }

    /// `UploadProfilePhotoQuery::send` — the sticker form (cpp:586-611):
    /// the `VIDEO_EMOJI_MARKUP` flag pair over the resolved
    /// `StickerPhotoSize`. The `files`/`fllm` pair serves the
    /// `on_error` tail (the upload id is invalid on this form, so the
    /// drop is the no-op the C++ `is_valid` gate makes it).
    #[allow(clippy::too_many_arguments)]
    fn send_upload_profile_photo_query_sticker(
        &mut self,
        env: &UserEnv,
        files: &mut crate::files::FileManager,
        fllm: &mut crate::files::FileLoadManager,
        user_id: UserId,
        stickers: &crate::stickers_manager::StickersManager,
        sticker_photo_size: crate::photo::StickerPhotoSize,
        is_fallback: bool,
        only_suggest: bool,
        mut promise: Promise<()>,
    ) {
        // the sticker form's flag bits
        const VIDEO_EMOJI_MARKUP_MASK: i32 = 1 << 4;
        const BOT_MASK: i32 = 1 << 5;
        const CONTACT_VIDEO_EMOJI_MARKUP_MASK: i32 = 1 << 5;

        let file_upload_id = FileUploadId::default();
        let is_me = user_id == self.get_my_id();
        let input_user = if !is_me {
            match self.get_input_user(env, user_id) {
                Ok(input_user) => Some(input_user),
                Err(error) => {
                    // the handler's `on_error` tail (cpp:628-633)
                    if file_upload_id.is_valid() {
                        files.delete_partial_remote_location(file_upload_id, fllm);
                    }
                    return promise.set_error(error);
                }
            }
        } else {
            None
        };
        if self.is_user_bot_of(user_id) != env.auth_is_bot || is_me {
            let mut flags = VIDEO_EMOJI_MARKUP_MASK;
            if input_user.is_some() {
                flags |= BOT_MASK;
            }
            self.pending_upload_profile_photo_queries.push(promise);
            let function = telegram_api::photos_uploadProfilePhoto {
                flags,
                fallback: is_fallback,
                bot: input_user,
                file: None,
                video: None,
                video_start_ts: 0.0,
                video_emoji_markup: Some(sticker_photo_size.get_input_video_size_object(
                    |sticker_set_id| {
                        stickers.get_input_sticker_set(crate::stickers_manager::StickerSetId::new(
                            sticker_set_id,
                        ))
                    },
                )),
            };
            match self.net.clone() {
                Some(net) => net.dispatch(
                    UserNetKind::UploadProfilePhoto {
                        user_id,
                        is_fallback,
                        only_suggest,
                        file_upload_id,
                    },
                    telegram_api::Function::photos_uploadProfilePhoto(function),
                ),
                None => tdutils::log_info!("Send photos.uploadProfilePhoto — [net-inert]"),
            }
        } else {
            self.pending_upload_profile_photo_queries.push(promise);
            let function = telegram_api::photos_uploadContactProfilePhoto {
                flags: CONTACT_VIDEO_EMOJI_MARKUP_MASK,
                suggest: only_suggest,
                save: !only_suggest,
                user_id: input_user,
                file: None,
                video: None,
                video_start_ts: 0.0,
                video_emoji_markup: Some(sticker_photo_size.get_input_video_size_object(
                    |sticker_set_id| {
                        stickers.get_input_sticker_set(crate::stickers_manager::StickerSetId::new(
                            sticker_set_id,
                        ))
                    },
                )),
            };
            match self.net.clone() {
                Some(net) => net.dispatch(
                    UserNetKind::UploadProfilePhoto {
                        user_id,
                        is_fallback,
                        only_suggest,
                        file_upload_id,
                    },
                    telegram_api::Function::photos_uploadContactProfilePhoto(function),
                ),
                None => {
                    tdutils::log_info!("Send photos.uploadContactProfilePhoto — [net-inert]")
                }
            }
        }
    }

    /// `DeleteContactProfilePhotoQuery::send` (cpp:766-786).
    fn send_delete_contact_profile_photo_query(
        &mut self,
        user_id: UserId,
        input_user: telegram_api::InputUser,
        promise: Promise<()>,
    ) {
        self.pending_delete_contact_profile_photo_queries
            .push(promise);
        let function = telegram_api::photos_uploadContactProfilePhoto {
            flags: 0,
            suggest: false,
            save: true,
            user_id: Some(input_user),
            file: None,
            video: None,
            video_start_ts: 0.0,
            video_emoji_markup: None,
        };
        match self.net.clone() {
            Some(net) => net.dispatch(
                UserNetKind::DeleteContactProfilePhoto { user_id },
                telegram_api::Function::photos_uploadContactProfilePhoto(function),
            ),
            None => tdutils::log_info!("Send contacts.deleteContactProfilePhoto — [net-inert]"),
        }
    }

    /// `UserManager::delete_profile_photo` (UserManager.cpp:6395-6426)
    /// — the cache bands answer here; the unloaded-full reload ladder
    /// re-enters through the Td actor (the C++
    /// `PromiseCreator::lambda` chain, cpp:6405-6414), the reset arm
    /// refires the `UpdateProfilePhotoQuery` with `inputPhotoEmpty`
    /// (cpp:6418-6422) and the plain arm rides the
    /// `DeleteProfilePhotoQuery` (cpp:799-816).
    pub fn delete_profile_photo(
        &mut self,
        env: &mut UserEnv,
        profile_photo_id: i64,
        is_recursive: bool,
        mut promise: Promise<()>,
    ) {
        // `TRY_STATUS_PROMISE(promise, G()->close_status())` (cpp:6396)
        if env.global.close_flag() {
            return promise.set_error(Status::error(500, "Request aborted"));
        }
        let my_id = self.get_my_id();
        let have_user_full = self.get_user_full_force(env, my_id, "delete_profile_photo");
        if !have_user_full || (env.auth_is_bot && !is_recursive) {
            // must load UserFull first, because fallback photo can't be
            // deleted via DeleteProfilePhotoQuery
            if is_recursive {
                return promise.set_error(Status::error(500, "Failed to load UserFullInfo"));
            }
            // the reload ladder: `reload_user_full(my_id, lambda)` where
            // the lambda re-enters delete_profile_photo with
            // `is_recursive = true` (cpp:6405-6414); the Td-actor hop is
            // the re-entry path (the promise_send_closure form), the
            // net-inert None arm the uncached-consumer convention
            match self.td_actor_id.clone() {
                Some(td_actor_id) => {
                    let reload_promise =
                        PromiseCreator::lambda_result(tdactor::promise_send_closure(
                            td_actor_id,
                            move |td: &mut crate::td::Td, result: tdutils::status::Result<()>| {
                                let mut promise = promise;
                                match result {
                                    tdutils::status::Result::Ok(()) => {
                                        let Some(options) = td.option_manager.as_mut() else {
                                            return promise
                                                .set_error(Status::error(500, "Request aborted"));
                                        };
                                        let mut env = UserEnv {
                                            global: &mut td.global,
                                            options,
                                            auth_is_bot: td.auth_manager.is_bot(),
                                        };
                                        td.user_manager.delete_profile_photo(
                                            &mut env,
                                            profile_photo_id,
                                            true,
                                            promise,
                                        );
                                    }
                                    tdutils::status::Result::Err(error) => {
                                        promise.set_error(error);
                                    }
                                }
                            },
                        ));
                    self.reload_user_full(env, my_id, reload_promise, "delete_profile_photo");
                }
                None => {
                    tdutils::log_info!("Reload UserFullInfo — [net-inert]");
                    promise.set_error(Status::error(500, "Request aborted"));
                }
            }
            return;
        }
        let mut profile_photo_id = profile_photo_id;
        let (update_main_photo, update_fallback_photo) = {
            let user_full = self
                .users_full
                .get(&my_id.get())
                .expect("forced just above");
            if env.auth_is_bot {
                if user_full.photo.id == 0 {
                    promise.set_value(());
                    return;
                }
                profile_photo_id = user_full.photo.id;
            }
            (
                user_full.photo.id != 0 && user_full.photo.id == profile_photo_id,
                user_full.fallback_photo.id != 0 && user_full.fallback_photo.id == profile_photo_id,
            )
        };
        if update_main_photo || update_fallback_photo {
            self.send_update_profile_photo_query(
                env,
                my_id,
                profile_photo_id,
                update_fallback_photo,
                telegram_api::InputPhoto::inputPhotoEmpty(telegram_api::inputPhotoEmpty),
                promise,
            );
            return;
        }
        self.send_delete_profile_photo_query(profile_photo_id, promise);
    }

    /// `DeleteProfilePhotoQuery::send` (cpp:799-816) — the
    /// `photos.deletePhotos` query over the one-element id list.
    fn send_delete_profile_photo_query(&mut self, profile_photo_id: i64, promise: Promise<()>) {
        self.pending_delete_profile_photo_queries.push(promise);
        let function = telegram_api::photos_deletePhotos {
            id: vec![telegram_api::InputPhoto::inputPhoto(
                telegram_api::inputPhoto {
                    id: profile_photo_id,
                    access_hash: 0,
                    file_reference: Vec::new(),
                },
            )],
        };
        match self.net.clone() {
            Some(net) => net.dispatch(
                UserNetKind::DeleteProfilePhoto { profile_photo_id },
                telegram_api::Function::photos_deletePhotos(function),
            ),
            None => tdutils::log_info!("Send photos.deletePhotos — [net-inert]"),
        }
    }

    /// `UserManager::on_delete_profile_photo` (UserManager.cpp:6431-6438)
    /// — the `DeleteProfilePhotoQuery` ok tail: the cache delete, then
    /// the re-get ladder when the User record needs the new photo.
    pub fn on_delete_profile_photo(
        &mut self,
        env: &mut UserEnv,
        profile_photo_id: i64,
        mut promise: Promise<()>,
    ) {
        let need_reget_user = self.delete_my_profile_photo_from_cache(env, profile_photo_id);
        if need_reget_user && !env.global.close_flag() {
            let my_id = self.get_my_id();
            return self.reload_user(env, my_id, promise, "on_delete_profile_photo");
        }
        promise.set_value(());
    }

    /// `UserManager::on_set_profile_photo` (UserManager.cpp:6190-6214) —
    /// the `UpdateProfilePhotoQuery`/`DeleteContactProfilePhotoQuery`
    /// ok tail: the replaced-photo cache delete, the users intake, the
    /// set-photo cache add, then the promise (or the re-get when the
    /// answer carried no matching user).
    pub fn on_set_profile_photo(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        photo: telegram_api::photos_Photo,
        is_fallback: bool,
        old_photo_id: i64,
        mut promise: Promise<()>,
    ) {
        tdutils::log_info!("Changed profile photo to {}", photo.to_debug_string());
        let is_bot = self.is_user_bot_of(user_id);
        let is_my = user_id == self.get_my_id();
        if is_my && !is_fallback {
            self.delete_my_profile_photo_from_cache(env, old_photo_id);
        }
        let mut have_user = false;
        for user in &photo.users {
            if get_wire_user_id(user) == user_id {
                have_user = true;
            }
        }
        let telegram_api::photos_Photo {
            photo: photo_ptr,
            users,
        } = photo;
        self.on_get_users(env, users, "on_set_profile_photo");
        if !is_bot {
            // the C++ `get_photo(td_, std::move(photo->photo_), …)` — a
            // null payload (the DeleteContactProfilePhotoQuery band,
            // cpp:789) reads as the empty photo
            let photo = get_photo(photo_ptr.unwrap_or(telegram_api::Photo::photoEmpty(
                telegram_api::photoEmpty { id: 0 },
            )));
            self.add_set_profile_photo_to_cache(env, user_id, photo, is_fallback);
        }
        if have_user {
            promise.set_value(());
        } else {
            self.reload_user(env, user_id, promise, "on_set_profile_photo");
        }
    }

    /// `UserManager::add_set_profile_photo_to_cache`
    /// (UserManager.cpp:6216-6271). The `UserFull` photos store the
    /// reduced `as_profile_photo` form (module docs); the file-manager
    /// registration inside the C++ conversions is the Files step.
    fn add_set_profile_photo_to_cache(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        photo: Photo,
        is_fallback: bool,
    ) {
        if !self.get_user_force(env, user_id, "add_set_profile_photo_to_cache") {
            return;
        }
        tdutils::log_info!("Add profile photo {} to cache", photo.id);
        let is_me = user_id == self.get_my_id();

        // update photo list — we have subsequence of user photos in
        // user_photos_
        if is_me && !is_fallback && !photo.is_empty() {
            let mut registered: Option<Photo> = None;
            if let Some(user_photos) = self.user_photos.get_mut(&user_id.get())
                && user_photos.count != -1
            {
                if user_photos.offset == 0 {
                    if user_photos.photos.is_empty() || user_photos.photos[0].id != photo.id {
                        user_photos.photos.insert(0, photo.clone());
                        user_photos.count += 1;
                        registered = Some(user_photos.photos[0].clone());
                    }
                } else {
                    user_photos.count += 1;
                    user_photos.offset += 1;
                }
            }
            if let Some(back) = registered {
                self.register_user_photo(user_id, &back);
            }
        }

        // update ProfilePhoto in User
        let old_photo_id = self.users.get(&user_id.get()).unwrap().photo.id;
        if (!is_fallback || old_photo_id == 0) && !photo.is_empty() {
            self.do_update_user_photo(
                env,
                user_id,
                as_profile_photo(&photo, !is_me),
                false,
                "add_set_profile_photo_to_cache",
            );
            self.update_user(env, user_id, false, false);
        }

        // update Photo in UserFull
        if self.get_user_full_force(env, user_id, "add_set_profile_photo_to_cache") {
            let mut user_full = self.users_full.remove(&user_id.get()).unwrap();
            let mut current_photo_is_personal = false;
            let mut need_apply = Self::get_user_full_profile_photo_id(&user_full) > 0;
            if !is_me {
                current_photo_is_personal = true;
                if photo.is_empty() {
                    // always can apply empty personal photo
                    need_apply = true;
                }
            } else if !is_fallback {
                current_photo_is_personal = false;
                if photo.is_empty() {
                    // never can apply empty photo
                    need_apply = false;
                }
            } else if photo.is_empty() {
                // always can apply empty fallback photo
                need_apply = true;
            }
            let new_photo = as_profile_photo(&photo, current_photo_is_personal);
            let current_photo = if current_photo_is_personal {
                &mut user_full.personal_photo
            } else if is_me && !is_fallback {
                &mut user_full.photo
            } else {
                &mut user_full.fallback_photo
            };
            if *current_photo != new_photo && need_apply {
                tdutils::log_info!("Update full photo of {user_id:?} to {}", photo.id);
                *current_photo = new_photo;
                user_full.is_changed = true;
                if is_me && !photo.is_empty() {
                    if !is_fallback {
                        self.register_user_photo(user_id, &photo);
                    } else {
                        self.register_suggested_profile_photo(&photo);
                    }
                }
                // just in case — the expected id reads the User record the
                // ProfilePhoto update may have changed
                let expected_photo_id = self.users.get(&user_id.get()).unwrap().photo.id;
                self.drop_user_full_photos(
                    env,
                    user_id,
                    expected_photo_id,
                    "add_set_profile_photo_to_cache",
                );
            }
            if user_full.expires_at > 0.0 {
                user_full.expires_at = 0.0;
                user_full.need_save_to_database = true;
            }
            self.users_full.insert(user_id.get(), user_full);
            self.update_user_full(env, user_id, "add_set_profile_photo_to_cache", false);
            self.reload_user_full(
                env,
                user_id,
                Promise::default(),
                "add_set_profile_photo_to_cache",
            );
        }
    }

    /// `UserManager::delete_my_profile_photo_from_cache`
    /// (UserManager.cpp:6273-6393) — the photo-list surgery, the User
    /// photo repair and the UserFull photo repair; returns the
    /// `need_reget_user` flag.
    fn delete_my_profile_photo_from_cache(
        &mut self,
        env: &mut UserEnv,
        profile_photo_id: i64,
    ) -> bool {
        if profile_photo_id == 0 || profile_photo_id == -2 {
            return false;
        }
        // we have subsequence of user photos in user_photos_
        tdutils::log_info!("Delete profile photo {profile_photo_id} from cache");
        let user_id = self.get_my_id();
        let have_user = self.get_user_force(env, user_id, "delete_my_profile_photo_from_cache");
        let is_main_photo_deleted = have_user
            && self
                .users
                .get(&user_id.get())
                .is_some_and(|u| u.photo.id == profile_photo_id);

        // update photo list
        let mut user_photos = self.user_photos.remove(&user_id.get());
        if let Some(user_photos) = user_photos.as_mut()
            && user_photos.count > 0
        {
            let old_size = user_photos.photos.len();
            user_photos
                .photos
                .retain(|photo| photo.id != profile_photo_id);
            let removed_photos = old_size - user_photos.photos.len();
            if removed_photos > 0 {
                if removed_photos != 1 {
                    tdutils::log_error!("Had {removed_photos} photos with ID {profile_photo_id}");
                }
                user_photos.count -= removed_photos as i32;
                // offset was not changed
                debug_assert!(user_photos.count >= 0);
                if user_photos.count == 0 {
                    debug_assert!(user_photos.photos.is_empty());
                    user_photos.offset = 0;
                }
            } else if user_photos.offset == 0 && user_photos.count > user_photos.photos.len() as i32
            {
                // failed to find photo to remove from cache, but offset
                // doesn't need to be adjusted
                user_photos.count -= 1;
            } else {
                // failed to find photo to remove from cache — don't know
                // how to adjust offset, so drop the photos cache
                tdutils::log_info!("Drop photos of {user_id:?}");
                user_photos.photos.clear();
                user_photos.count = -1;
                user_photos.offset = -1;
            }
        }
        let have_new_photo = user_photos.as_ref().is_some_and(|photos| {
            photos.count != -1 && photos.offset == 0 && !photos.photos.is_empty()
        });

        let have_user_full =
            self.get_user_full_force(env, user_id, "delete_my_profile_photo_from_cache");

        // update ProfilePhoto in User
        let mut need_reget_user = false;
        if is_main_photo_deleted {
            if have_new_photo {
                let new_photo = as_profile_photo(&user_photos.as_ref().unwrap().photos[0], false);
                self.do_update_user_photo(
                    env,
                    user_id,
                    new_photo,
                    false,
                    "delete_my_profile_photo_from_cache",
                );
            } else {
                self.do_update_user_photo(
                    env,
                    user_id,
                    ProfilePhoto::default(),
                    false,
                    "delete_my_profile_photo_from_cache 2",
                );
                need_reget_user = user_photos.is_none() || user_photos.as_ref().unwrap().count != 0;
            }
            self.update_user(env, user_id, false, false);

            // update Photo in UserFull
            if have_user_full {
                let mut user_full = self.users_full.remove(&user_id.get()).unwrap();
                if !user_full.fallback_photo.is_empty()
                    && user_full.fallback_photo.id == profile_photo_id
                {
                    tdutils::log_info!("Drop full public photo of {user_id:?}");
                    user_full.fallback_photo = ProfilePhoto::default();
                    user_full.is_changed = true;
                } else if have_new_photo {
                    let new_photo =
                        as_profile_photo(&user_photos.as_ref().unwrap().photos[0], false);
                    if !user_full.photo.is_empty()
                        && user_full.photo.id == profile_photo_id
                        && user_full.photo != new_photo
                    {
                        tdutils::log_info!("Update full photo of {user_id:?}");
                        user_full.photo = new_photo;
                        user_full.is_changed = true;
                    }
                } else {
                    // repair UserFull photo
                    if !user_full.photo.is_empty() {
                        user_full.photo = ProfilePhoto::default();
                        user_full.is_changed = true;
                    }
                    if !user_full.fallback_photo.is_empty() {
                        user_full.fallback_photo = ProfilePhoto::default();
                        user_full.is_changed = true;
                    }
                }
                if user_full.expires_at > 0.0 {
                    user_full.expires_at = 0.0;
                    user_full.need_save_to_database = true;
                }
                self.users_full.insert(user_id.get(), user_full);
                self.reload_user_full(
                    env,
                    user_id,
                    Promise::default(),
                    "delete_my_profile_photo_from_cache",
                );
                self.update_user_full(env, user_id, "delete_my_profile_photo_from_cache", false);
            }
        }
        if let Some(user_photos) = user_photos {
            self.user_photos.insert(user_id.get(), user_photos);
        }
        need_reget_user
    }

    // --- the emoji-status query planes (EmojiStatus.cpp:571-637) —
    // the cached lists answer here; the hash-revalidation queries
    // ride the `net` handle (live since d5c9b66) ---

    // ------------------------------------------------------------------
    // The user-family net slice — the query sends ride the `net` handle
    // (the emoji-status cache planes since d5c9b66, the setters and the
    // support-user query since this slice); the intakes run the C++
    // on_result halves (the cache saves, the notModified 500 band, the
    // promise flushes, the ack-gated cache writes).

    /// The shared dispatch tail — `send_query(G()->net_query_creator()…)`
    /// narrowed to the handle; the None arm is the net-inert log.
    fn send_user_query(&self, kind: UserNetKind, function: telegram_api::Function, what: &str) {
        match &self.net {
            Some(net) => net.dispatch(kind, function),
            None => tdutils::log_info!("Send {what} — [net-inert]"),
        }
    }

    /// `get_default_emoji_statuses` (EmojiStatus.cpp:571-578).
    pub fn get_default_emoji_statuses(
        &mut self,
        env: &mut UserEnv,
        mut promise: Promise<td_api::EmojiStatusCustomEmojis>,
    ) {
        let statuses = crate::emoji_status::load_emoji_status_custom_emojis(
            env.global.binlog_pmc(),
            crate::emoji_status::DEFAULT_EMOJI_STATUSES_DATABASE_KEY,
        );
        if statuses.hash != -1 {
            promise.set_value(statuses.get_emoji_status_custom_emojis_object());
            return;
        }
        self.pending_default_emoji_statuses_queries.push(promise);
        self.send_user_query(
            UserNetKind::GetDefault,
            telegram_api::Function::account_getDefaultEmojiStatuses(
                telegram_api::account_getDefaultEmojiStatuses {
                    hash: statuses.hash,
                },
            ),
            "account.getDefaultEmojiStatuses",
        );
    }

    /// `get_default_channel_emoji_statuses` (EmojiStatus.cpp:580-588) —
    /// serves getDefaultChatEmojiStatuses.
    pub fn get_default_channel_emoji_statuses(
        &mut self,
        env: &mut UserEnv,
        mut promise: Promise<td_api::EmojiStatusCustomEmojis>,
    ) {
        let statuses = crate::emoji_status::load_emoji_status_custom_emojis(
            env.global.binlog_pmc(),
            crate::emoji_status::DEFAULT_CHANNEL_EMOJI_STATUSES_DATABASE_KEY,
        );
        if statuses.hash != -1 {
            promise.set_value(statuses.get_emoji_status_custom_emojis_object());
            return;
        }
        self.pending_channel_emoji_statuses_queries.push(promise);
        self.send_user_query(
            UserNetKind::GetChannelDefault,
            telegram_api::Function::account_getChannelDefaultEmojiStatuses(
                telegram_api::account_getChannelDefaultEmojiStatuses {
                    hash: statuses.hash,
                },
            ),
            "account.getChannelDefaultEmojiStatuses",
        );
    }

    /// `get_recent_emoji_statuses` (EmojiStatus.cpp:590-597).
    pub fn get_recent_emoji_statuses(
        &mut self,
        env: &mut UserEnv,
        mut promise: Promise<td_api::EmojiStatuses>,
    ) {
        let statuses = crate::emoji_status::load_emoji_statuses(
            env.global.binlog_pmc(),
            crate::emoji_status::RECENT_EMOJI_STATUSES_DATABASE_KEY,
        );
        if statuses.hash != -1 {
            promise.set_value(statuses.get_emoji_statuses_object());
            return;
        }
        self.pending_recent_emoji_statuses_queries.push(promise);
        self.send_user_query(
            UserNetKind::GetRecent,
            telegram_api::Function::account_getRecentEmojiStatuses(
                telegram_api::account_getRecentEmojiStatuses {
                    hash: statuses.hash,
                },
            ),
            "account.getRecentEmojiStatuses",
        );
    }

    /// `clear_recent_emoji_statuses` (EmojiStatus.cpp:621-624) — the
    /// cache clears here; the `ClearRecentEmojiStatusesQuery` ok tail
    /// re-saves the empty record and answers.
    pub fn clear_recent_emoji_statuses(&mut self, env: &mut UserEnv, promise: Promise<()>) {
        let empty = crate::emoji_status::EmojiStatusesCache::default();
        let mut stored = Vec::new();
        empty.store(&mut stored);
        crate::emoji_status::save_emoji_statuses_cache(
            env.global.binlog_pmc(),
            crate::emoji_status::RECENT_EMOJI_STATUSES_DATABASE_KEY,
            &stored,
        );
        self.pending_clear_recent_emoji_statuses_queries
            .push(promise);
        self.send_user_query(
            UserNetKind::ClearRecent,
            telegram_api::Function::account_clearRecentEmojiStatuses(
                telegram_api::account_clearRecentEmojiStatuses,
            ),
            "account.clearRecentEmojiStatuses",
        );
    }

    /// `get_upgraded_gift_emoji_statuses` (EmojiStatus.cpp:626-637) —
    /// the bot arm drops the promise like the C++ `CHECK(!promise)`.
    pub fn get_upgraded_gift_emoji_statuses(
        &mut self,
        env: &mut UserEnv,
        mut promise: Promise<td_api::EmojiStatuses>,
    ) {
        if env.auth_is_bot {
            return;
        }
        let statuses = crate::emoji_status::load_emoji_statuses(
            env.global.binlog_pmc(),
            crate::emoji_status::UPGRADED_GIFT_EMOJI_STATUSES_DATABASE_KEY,
        );
        if statuses.hash != -1 {
            promise.set_value(statuses.get_emoji_statuses_object());
            return;
        }
        self.pending_upgraded_gift_emoji_statuses_queries
            .push(promise);
        self.send_user_query(
            UserNetKind::GetCollectible,
            telegram_api::Function::account_getCollectibleEmojiStatuses(
                telegram_api::account_getCollectibleEmojiStatuses {
                    hash: statuses.hash,
                },
            ),
            "account.getCollectibleEmojiStatuses",
        );
    }

    /// `GetDefaultEmojiStatusesQuery::on_result` /
    /// `GetChannelDefaultEmojiStatusesQuery::on_result`
    /// (EmojiStatus.cpp:184-198, 228-242) — the notModified 500 band,
    /// the cache save, the promise flush; `channel` picks the family's
    /// queue and database key.
    pub fn on_emoji_status_custom_emojis(
        &mut self,
        env: &mut UserEnv,
        channel: bool,
        result: Result<telegram_api::account_EmojiStatuses, Status>,
    ) {
        let promise = if channel {
            self.pending_channel_emoji_statuses_queries.pop()
        } else {
            self.pending_default_emoji_statuses_queries.pop()
        };
        let Some(mut promise) = promise else {
            return;
        };
        let key = if channel {
            crate::emoji_status::DEFAULT_CHANNEL_EMOJI_STATUSES_DATABASE_KEY
        } else {
            crate::emoji_status::DEFAULT_EMOJI_STATUSES_DATABASE_KEY
        };
        match result {
            Err(status) => promise.set_error(status),
            Ok(telegram_api::account_EmojiStatuses::account_emojiStatusesNotModified(_)) => {
                promise.set_error(Status::error(500, "Receive wrong server response"));
            }
            Ok(telegram_api::account_EmojiStatuses::account_emojiStatuses(statuses)) => {
                let cache =
                    crate::emoji_status::EmojiStatusCustomEmojisCache::from_account_emoji_statuses(
                        statuses,
                    );
                let object = cache.get_emoji_status_custom_emojis_object();
                let mut stored = Vec::new();
                cache.store(&mut stored);
                crate::emoji_status::save_emoji_statuses_cache(
                    env.global.binlog_pmc(),
                    key,
                    &stored,
                );
                promise.set_value(object);
            }
        }
    }

    /// `GetRecentEmojiStatusesQuery::on_result` /
    /// `GetCollectibleEmojiStatusesQuery::on_result`
    /// (EmojiStatus.cpp:270-284, 338-352) — the statuses twin.
    pub fn on_emoji_statuses(
        &mut self,
        env: &mut UserEnv,
        upgraded_gift: bool,
        result: Result<telegram_api::account_EmojiStatuses, Status>,
    ) {
        let promise = if upgraded_gift {
            self.pending_upgraded_gift_emoji_statuses_queries.pop()
        } else {
            self.pending_recent_emoji_statuses_queries.pop()
        };
        let Some(mut promise) = promise else {
            return;
        };
        let key = if upgraded_gift {
            crate::emoji_status::UPGRADED_GIFT_EMOJI_STATUSES_DATABASE_KEY
        } else {
            crate::emoji_status::RECENT_EMOJI_STATUSES_DATABASE_KEY
        };
        match result {
            Err(status) => promise.set_error(status),
            Ok(telegram_api::account_EmojiStatuses::account_emojiStatusesNotModified(_)) => {
                promise.set_error(Status::error(500, "Receive wrong server response"));
            }
            Ok(telegram_api::account_EmojiStatuses::account_emojiStatuses(statuses)) => {
                let cache =
                    crate::emoji_status::EmojiStatusesCache::from_account_emoji_statuses(statuses);
                let object = cache.get_emoji_statuses_object();
                let mut stored = Vec::new();
                cache.store(&mut stored);
                crate::emoji_status::save_emoji_statuses_cache(
                    env.global.binlog_pmc(),
                    key,
                    &stored,
                );
                promise.set_value(object);
            }
        }
    }

    /// `ClearRecentEmojiStatusesQuery::on_result` (EmojiStatus.cpp:293-317)
    /// — the empty record re-save (the send already wrote it; the C++
    /// writes it again) and the ok answer.
    pub fn on_clear_recent_emoji_statuses(
        &mut self,
        env: &mut UserEnv,
        result: Result<(), Status>,
    ) {
        let Some(mut promise) = self.pending_clear_recent_emoji_statuses_queries.pop() else {
            return;
        };
        match result {
            Err(status) => promise.set_error(status),
            Ok(()) => {
                let empty = crate::emoji_status::EmojiStatusesCache::default();
                let mut stored = Vec::new();
                empty.store(&mut stored);
                crate::emoji_status::save_emoji_statuses_cache(
                    env.global.binlog_pmc(),
                    crate::emoji_status::RECENT_EMOJI_STATUSES_DATABASE_KEY,
                    &stored,
                );
                promise.set_value(());
            }
        }
    }

    // ------------------------------------------------------------------
    // M.4.13: profile photos + saved music (UserManager.cpp:6798-7753,
    // register_user_photo cpp:3706-3737, get_saved_music_document_id
    // cpp:4673-4693, drop_user_photos/drop_user_saved_music
    // cpp:9519-9553). The listing query senders ride the net step
    // (M.4.16); every file id is a placeholder until the Files step
    // (module docs).

    /// The FileReferenceManager stand-in behind the two file-source maps.
    fn create_file_source_id(&mut self) -> FileSourceId {
        self.next_file_source_id += 1;
        FileSourceId::new(self.next_file_source_id)
    }

    /// `UserManager::add_user_photos` (UserManager.cpp:7015-7022).
    fn add_user_photos(&mut self, user_id: UserId) -> &mut UserPhotos {
        assert!(user_id.is_valid());
        self.user_photos.entry(user_id.get()).or_default()
    }

    /// `UserManager::drop_user_photos` (UserManager.cpp:9519-9534).
    fn drop_user_photos(&mut self, user_id: UserId, is_empty: bool, source: &str) {
        if let Some(user_photos) = self.user_photos.get_mut(&user_id.get()) {
            let new_count = if is_empty { 0 } else { -1 };
            if user_photos.count == new_count {
                debug_assert!(user_photos.photos.is_empty());
                debug_assert_eq!(user_photos.offset, user_photos.count);
            } else {
                tdutils::log_info!(
                    "Drop photos of {user_id:?} to {} from {source}",
                    if is_empty { "empty" } else { "unknown" }
                );
                user_photos.photos.clear();
                user_photos.count = new_count;
                user_photos.offset = user_photos.count;
            }
        }
    }

    /// `UserManager::apply_pending_user_photo`
    /// (UserManager.cpp:7103-7113): apply the bot path's held photo.
    pub fn apply_pending_user_photo(&mut self, env: &mut UserEnv, user_id: UserId, source: &str) {
        let needs_apply = self
            .users
            .get(&user_id.get())
            .map(|u| !u.is_photo_inited)
            .unwrap_or(false)
            && self.pending_user_photos.contains_key(&user_id.get());
        if !needs_apply {
            return;
        }
        let photo = self.pending_user_photos.remove(&user_id.get()).unwrap();
        self.do_update_user_photo_from_telegram(env, user_id, photo, source);
        self.update_user(env, user_id, false, false);
    }

    /// The FIFO pop behind the `SaveMusic` answer (the C++ handler's
    /// `promise_`, UserManager.cpp:1542).
    pub(crate) fn take_add_saved_music_query_promise(&mut self) -> Option<Promise<()>> {
        if self.pending_add_saved_music_queries.is_empty() {
            None
        } else {
            Some(self.pending_add_saved_music_queries.remove(0))
        }
    }

    /// The FIFO pop behind the `UnsaveMusic` answer (cpp:1598).
    pub(crate) fn take_remove_saved_music_query_promise(&mut self) -> Option<Promise<()>> {
        if self.pending_remove_saved_music_queries.is_empty() {
            None
        } else {
            Some(self.pending_remove_saved_music_queries.remove(0))
        }
    }

    /// The FIFO pop behind the `GetUserSavedMusicById` answer — the
    /// caller promise (the C++ query-carried one, §5).
    pub(crate) fn take_reload_saved_music_by_id_query_promise(&mut self) -> Option<Promise<()>> {
        if self.pending_reload_saved_music_by_id_queries.is_empty() {
            None
        } else {
            Some(self.pending_reload_saved_music_by_id_queries.remove(0))
        }
    }

    /// `UpdateProfilePhotoQuery::on_result`'s promise pop (cpp:684-688) —
    /// the head in-flight query promise.
    pub(crate) fn take_update_profile_photo_query_promise(&mut self) -> Option<Promise<()>> {
        if self.pending_update_profile_photo_queries.is_empty() {
            None
        } else {
            Some(self.pending_update_profile_photo_queries.remove(0))
        }
    }

    /// The promise pop of the UploadProfilePhoto family queue (§5).
    pub(crate) fn take_upload_profile_photo_query_promise(&mut self) -> Option<Promise<()>> {
        if self.pending_upload_profile_photo_queries.is_empty() {
            None
        } else {
            Some(self.pending_upload_profile_photo_queries.remove(0))
        }
    }

    /// The promise pop of the UploadSavedMusicFile family queue (§5).
    pub(crate) fn take_upload_saved_music_query_promise(&mut self) -> Option<Promise<()>> {
        if self.pending_upload_saved_music_queries.is_empty() {
            None
        } else {
            Some(self.pending_upload_saved_music_queries.remove(0))
        }
    }

    /// `DeleteContactProfilePhotoQuery::on_result`'s promise pop
    /// (cpp:784-793).
    pub(crate) fn take_delete_contact_profile_photo_query_promise(
        &mut self,
    ) -> Option<Promise<()>> {
        if self.pending_delete_contact_profile_photo_queries.is_empty() {
            None
        } else {
            Some(self.pending_delete_contact_profile_photo_queries.remove(0))
        }
    }

    /// `DeleteProfilePhotoQuery::on_result`'s promise pop (cpp:808-815).
    pub(crate) fn take_delete_profile_photo_query_promise(&mut self) -> Option<Promise<()>> {
        if self.pending_delete_profile_photo_queries.is_empty() {
            None
        } else {
            Some(self.pending_delete_profile_photo_queries.remove(0))
        }
    }

    /// The offset `-1` `GetUserPhotosQuery` promise pop — the reload
    /// leg's query-carried promise (cpp:6990).
    pub(crate) fn take_get_user_photos_reload_promise(&mut self) -> Option<Promise<()>> {
        if self.pending_get_user_photos_reload_queries.is_empty() {
            None
        } else {
            Some(self.pending_get_user_photos_reload_queries.remove(0))
        }
    }

    /// `UserManager::get_user_profile_photos`
    /// (UserManager.cpp:6826-6885). The cache window answers here; a
    /// miss parks in the pending queue and the `photos.getUserPhotos`
    /// query rides the `net` handle.
    pub fn get_user_profile_photos(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        offset: i32,
        limit: i32,
        mut promise: Promise<td_api::ChatPhotos>,
    ) {
        if offset < 0 {
            promise.set_error(Status::error(400, "Parameter offset must be non-negative"));
            return;
        }
        if limit <= 0 {
            promise.set_error(Status::error(400, "Parameter limit must be positive"));
            return;
        }
        let mut limit = limit.min(MAX_GET_PROFILE_PHOTOS);

        if let Err(status) = self.get_input_user(env, user_id) {
            promise.set_error(status);
            return;
        }

        if self.get_user(user_id).is_none() {
            promise.set_error(Status::error(400, "User not found"));
            return;
        }

        self.apply_pending_user_photo(env, user_id, "get_user_profile_photos");

        self.add_user_photos(user_id);
        if let Some(user_photos) = self.user_photos.get(&user_id.get())
            && user_photos.count != -1
        {
            // know photo count
            debug_assert_ne!(user_photos.offset, -1);
            tdutils::log_info!(
                "Have {} cached user profile photos at offset {}",
                user_photos.count,
                user_photos.offset
            );
            if offset >= user_photos.count {
                // offset is too big
                promise.set_value(td_api::ChatPhotos {
                    total_count: user_photos.count,
                    photos: Vec::new(),
                });
                return;
            }
            if limit > user_photos.count - offset {
                limit = user_photos.count - offset;
            }
            let cache_begin = user_photos.offset;
            let cache_end = cache_begin + user_photos.photos.len() as i32;
            if cache_begin <= offset && offset + limit <= cache_end {
                // answer query from cache
                let photos = user_photos.photos
                    [(offset - cache_begin) as usize..(offset - cache_begin + limit) as usize]
                    .iter()
                    .map(get_chat_photo_object)
                    .collect();
                promise.set_value(td_api::ChatPhotos {
                    total_count: user_photos.count,
                    photos,
                });
                return;
            }
        }

        let pending_request = PendingGetPhotoRequest {
            offset,
            limit,
            retry_count: 0,
            promise,
        };
        let requests = self
            .pending_get_user_photos_requests
            .entry(user_id.get())
            .or_default();
        requests.push(pending_request);
        if requests.len() != 1 {
            return;
        }

        self.send_get_user_photos_query(env, user_id);
    }

    /// `UserManager::send_get_user_photos_query`
    /// (UserManager.cpp:6890-6911) — the cache-end offset adjustment is
    /// computed with the query; the `GetUserPhotosQuery` promise is the
    /// retry-machine routing lambda (the answer drain runs
    /// [`UserManager::on_get_user_profile_photos`]).
    fn send_get_user_photos_query(&mut self, env: &UserEnv, user_id: UserId) {
        let Some(requests) = self.pending_get_user_photos_requests.get(&user_id.get()) else {
            return;
        };
        debug_assert!(!requests.is_empty());
        let mut offset = requests[0].offset;
        let mut limit = requests[0].limit;
        if let Some(user_photos) = self.user_photos.get(&user_id.get())
            && user_photos.count != -1
            && offset >= user_photos.offset
        {
            let cache_end = user_photos.offset + user_photos.photos.len() as i32;
            if offset < cache_end {
                // adjust offset to the end of cache
                debug_assert!(offset + limit > cache_end); // otherwise the request has already been answered
                limit = offset + limit - cache_end;
                offset = cache_end;
            }
        }
        let limit = limit.max(MAX_GET_PROFILE_PHOTOS / 5);
        tdutils::log_info!(
            "Send photos.getUserPhotos for {user_id:?} at offset {offset} limit {limit}"
        );
        let input_user = self.get_input_user_force(env, user_id);
        match self.net.clone() {
            Some(net) => net.dispatch(
                UserNetKind::GetUserPhotos {
                    user_id,
                    offset,
                    limit,
                },
                telegram_api::Function::photos_getUserPhotos(telegram_api::photos_getUserPhotos {
                    user_id: Some(input_user),
                    offset,
                    max_id: 0,
                    limit,
                }),
            ),
            None => tdutils::log_info!("Send photos.getUserPhotos — [net-inert]"),
        }
    }

    /// `UserManager::on_get_user_profile_photos`
    /// (UserManager.cpp:6915-6980): the pending-request retry machine.
    /// `result` mirrors the query promise's `Result<Unit>`.
    pub fn on_get_user_profile_photos(
        &mut self,
        env: &UserEnv,
        user_id: UserId,
        result: Result<(), Status>,
    ) {
        if env.global.close_flag() {
            // ignore_result_if_closing — the parked answer is dropped
            return;
        }
        let Some(mut pending_requests) =
            self.pending_get_user_photos_requests.remove(&user_id.get())
        else {
            return;
        };
        debug_assert!(!pending_requests.is_empty());
        if let Err(status) = result {
            for request in &mut pending_requests {
                request.promise.set_error(status.clone());
            }
            return;
        }
        let count = self.add_user_photos(user_id).count;
        if count == -1 {
            debug_assert!(self.have_min_user(user_id));
            // received result has just been dropped; resend request
            pending_requests[0].retry_count += 1;
            if pending_requests[0].retry_count >= 3 {
                pending_requests[0]
                    .promise
                    .set_error(Status::error(500, "Failed to return profile photos"));
                pending_requests.remove(0);
                if pending_requests.is_empty() {
                    return;
                }
            }
            self.pending_get_user_photos_requests
                .insert(user_id.get(), pending_requests);
            self.send_get_user_photos_query(env, user_id);
            return;
        }

        let user_photos = self.user_photos.get(&user_id.get()).unwrap();
        debug_assert_ne!(user_photos.offset, -1);
        tdutils::log_info!(
            "Have {} cached user profile photos at offset {}",
            user_photos.count,
            user_photos.offset
        );
        let mut left_requests = Vec::new();
        for (request_index, request_slot) in pending_requests.iter_mut().enumerate() {
            let mut request = std::mem::replace(
                request_slot,
                PendingGetPhotoRequest {
                    offset: 0,
                    limit: 0,
                    retry_count: 0,
                    promise: Promise::default(),
                },
            );

            if request.offset >= user_photos.count {
                // offset is too big
                request.promise.set_value(td_api::ChatPhotos {
                    total_count: user_photos.count,
                    photos: Vec::new(),
                });
                continue;
            }

            if request.limit > user_photos.count - request.offset {
                request.limit = user_photos.count - request.offset;
            }

            let cache_begin = user_photos.offset;
            let cache_end = cache_begin + user_photos.photos.len() as i32;
            if cache_begin <= request.offset && request.offset + request.limit <= cache_end {
                // answer query from cache
                let photos = user_photos.photos[(request.offset - cache_begin) as usize
                    ..(request.offset - cache_begin + request.limit) as usize]
                    .iter()
                    .map(get_chat_photo_object)
                    .collect();
                request.promise.set_value(td_api::ChatPhotos {
                    total_count: user_photos.count,
                    photos,
                });
                continue;
            }

            request.retry_count += 1;
            if request_index == 0 && request.retry_count >= 3 {
                request
                    .promise
                    .set_error(Status::error(500, "Failed to get profile photos"));
                continue;
            }

            left_requests.push(request);
        }

        if !left_requests.is_empty() {
            let requests = self
                .pending_get_user_photos_requests
                .entry(user_id.get())
                .or_default();
            let need_send = requests.is_empty();
            requests.extend(left_requests);
            if need_send {
                self.send_get_user_photos_query(env, user_id);
            }
        }
    }

    /// `UserManager::on_get_user_photos` (UserManager.cpp:7024-7101):
    /// the photos.getUserPhotos answer intake and cache fill.
    pub fn on_get_user_photos(
        &mut self,
        user_id: UserId,
        offset: i32,
        limit: i32,
        mut total_count: i32,
        photos: Vec<telegram_api::Photo>,
    ) {
        let photo_count = photos.len() as i32;
        let min_total_count = if offset >= 0 && photo_count > 0 {
            offset
        } else {
            0
        } + photo_count;
        if total_count < min_total_count {
            tdutils::log_error!(
                "Receive wrong photos total_count {total_count} for user {user_id:?}: receive \
                 {photo_count} photos with offset {offset}"
            );
            total_count = min_total_count;
        }
        if limit < photo_count {
            tdutils::log_error!(
                "Requested not more than {limit} photos, but {photo_count} received"
            );
        }

        if !self.users.contains_key(&user_id.get()) {
            tdutils::log_error!("Can't find {user_id:?}");
            return;
        }

        if offset == -1 {
            // from reload_user_profile_photo
            debug_assert_eq!(limit, 1);
            for photo_ptr in photos {
                if let telegram_api::Photo::photo(server_photo) = photo_ptr {
                    let current_photo_id = self.users.get(&user_id.get()).unwrap().photo.id;
                    if server_photo.id == current_photo_id {
                        // convert_photo_to_profile_photo + the
                        // registering get_profile_photo (cpp:7047-7054) —
                        // a no-op until the Files step (module docs)
                    }
                    let photo = get_photo(telegram_api::Photo::photo(server_photo));
                    self.register_user_photo(user_id, &photo);
                }
            }
            return;
        }

        tdutils::log_info!(
            "Receive {photo_count} photos of {user_id:?} out of {total_count} with offset \
             {offset} and limit {limit}"
        );
        // add_user_photos + the cache fill — the record is taken out of
        // the map so register_user_photo can borrow the manager
        let mut user_photos = self.user_photos.remove(&user_id.get()).unwrap_or_default();
        user_photos.count = total_count;
        debug_assert!(
            self.pending_get_user_photos_requests
                .contains_key(&user_id.get())
        );

        if user_photos.offset == -1 {
            user_photos.offset = 0;
            debug_assert!(user_photos.photos.is_empty());
        }

        if offset != user_photos.photos.len() as i32 + user_photos.offset {
            tdutils::log_info!(
                "Inappropriate offset to append {user_id:?} profile photos to cache: offset = \
                 {offset}, current_offset = {}, photo_count = {}",
                user_photos.offset,
                user_photos.photos.len()
            );
            user_photos.photos.clear();
            user_photos.offset = offset;
        }

        for photo in photos {
            let user_photo = get_photo(photo);
            if user_photo.is_empty() {
                tdutils::log_error!(
                    "Receive empty profile photo in getUserPhotos request for {user_id:?} with \
                     offset {offset} and limit {limit}"
                );
                user_photos.count -= 1;
                debug_assert!(user_photos.count >= 0);
                continue;
            }

            user_photos.photos.push(user_photo);
            let back = user_photos.photos.last().unwrap().clone();
            self.register_user_photo(user_id, &back);
        }
        if user_photos.offset > user_photos.count {
            user_photos.offset = user_photos.count;
            user_photos.photos.clear();
        }

        let known_photo_count = user_photos.photos.len() as i32;
        if user_photos.offset + known_photo_count > user_photos.count {
            user_photos
                .photos
                .truncate((user_photos.count - user_photos.offset) as usize);
        }
        self.user_photos.insert(user_id.get(), user_photos);
    }

    /// `UserManager::reload_user_profile_photo`
    /// (UserManager.cpp:6988-6992) — the `photos.getUserPhotos` query
    /// with offset -1 rides the `net` handle; the caller promise rides
    /// the query (the §5 fidelity restore — the C++
    /// `GetUserPhotosQuery` carries it, the park used to drop it).
    /// This request will be needed only to download the photo, so
    /// there is no reason to combine different requests for a photo
    /// into one request.
    pub fn reload_user_profile_photo(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        photo_id: i64,
        mut promise: Promise<()>,
    ) {
        self.get_user_force(env, user_id, "reload_user_profile_photo");
        let input_user = match self.get_input_user(env, user_id) {
            Ok(input_user) => input_user,
            Err(status) => return promise.set_error(status),
        };
        tdutils::log_info!("Reload profile photo {photo_id} of {user_id:?}");
        self.pending_get_user_photos_reload_queries.push(promise);
        match self.net.clone() {
            Some(net) => net.dispatch(
                UserNetKind::GetUserPhotos {
                    user_id,
                    offset: -1,
                    limit: 1,
                },
                telegram_api::Function::photos_getUserPhotos(telegram_api::photos_getUserPhotos {
                    user_id: Some(input_user),
                    offset: -1,
                    max_id: photo_id,
                    limit: 1,
                }),
            ),
            None => tdutils::log_info!("Reload profile photo — [net-inert]"),
        }
    }

    /// `UserManager::get_user_profile_photo_file_source_id`
    /// (UserManager.cpp:6995-7013).
    pub fn get_user_profile_photo_file_source_id(
        &mut self,
        user_id: UserId,
        photo_id: i64,
    ) -> FileSourceId {
        if !user_id.is_valid() {
            return FileSourceId::default();
        }

        let have_photo_id = self
            .users
            .get(&user_id.get())
            .map(|u| u.photo_ids.contains(&photo_id))
            .unwrap_or(false);
        if have_photo_id {
            // photo was already added, source ID was registered and
            // shouldn't be needed
            return FileSourceId::default();
        }

        if let Some(&source_id) = self
            .user_profile_photo_file_source_ids
            .get(&(user_id.get(), photo_id))
            && source_id.is_valid()
        {
            return source_id;
        }
        let source_id = self.create_file_source_id();
        self.user_profile_photo_file_source_ids
            .insert((user_id.get(), photo_id), source_id);
        source_id
    }

    /// `UserManager::register_user_photo` (UserManager.cpp:3706-3737):
    /// the photo-id registration and the file-source bookkeeping. The
    /// file-type probe (`ProfilePhoto`-typed sizes return early) reads
    /// FileManager state the placeholders cannot carry — the listing
    /// registers `FileType::Photo` sizes only (module docs), so the
    /// pre-check reduces to the empty/files check.
    pub fn register_user_photo(&mut self, user_id: UserId, photo: &Photo) {
        let photo_file_ids = photo_get_file_ids(photo);
        if photo.is_empty() || photo_file_ids.is_empty() {
            return;
        }
        // file_type == FileType::ProfilePhoto return / CHECK(Photo) —
        // the Files step restores the probe
        let photo_id = photo.id;
        if photo_id != 0 && self.users.contains_key(&user_id.get()) {
            let inserted = self
                .users
                .get_mut(&user_id.get())
                .unwrap()
                .photo_ids
                .insert(photo_id);
            if inserted {
                if user_id == self.my_id {
                    self.my_photo_file_id.insert(photo_id, photo_file_ids[0]);
                }
                // the photo is immutable and fully received — its file
                // source is forgotten (cpp:3728-3733); the add_file_source
                // fan-out over the file ids is the Files step
                let had_source = self
                    .user_profile_photo_file_source_ids
                    .remove(&(user_id.get(), photo_id));
                if had_source.is_none() {
                    // create_user_photo_file_source — the Files step
                }
            }
        }
    }

    /// `UserManager::register_suggested_profile_photo`
    /// (UserManager.cpp:3692-3704).
    pub fn register_suggested_profile_photo(&mut self, photo: &Photo) {
        let photo_file_ids = photo_get_file_ids(photo);
        if photo.is_empty() || photo_file_ids.is_empty() {
            return;
        }
        let photo_id = photo.id;
        if photo_id != 0 {
            self.my_photo_file_id.insert(photo_id, photo_file_ids[0]);
        }
    }

    /// `UserManager::start_up`'s saved-music block
    /// (UserManager.cpp:2829-2845) — call only for the authorized
    /// non-bot session. An empty or broken value means the C++ reloads
    /// the list from the server (the net step) and the set stays
    /// un-inited.
    pub fn init_my_saved_music_ids(
        &mut self,
        binlog_pmc: &mut BinlogKeyValue,
        is_authorized_non_bot: bool,
    ) {
        if !is_authorized_non_bot {
            return;
        }
        let value = binlog_pmc.get_raw(Self::get_my_saved_music_ids_database_key().as_bytes());
        let ids = Self::parse_my_saved_music_ids(&value);
        let Some(ids) = ids else {
            // `reload_my_saved_music_list(Auto())` (cpp:2836) — the
            // fire-and-forget reload rides the dispatcher (net-drain 09)
            self.reload_my_saved_music_list(Promise::default());
            return;
        };
        for saved_music_id in ids {
            if saved_music_id != 0 {
                self.my_saved_music_ids.insert(saved_music_id);
            }
        }
        self.are_my_saved_music_ids_inited = true;
    }

    /// The `vector<int64>` log-event form: `int32` count, then raw
    /// `int64`s (tl_helpers.h:156-161).
    fn parse_my_saved_music_ids(value: &[u8]) -> Option<Vec<i64>> {
        if value.is_empty() {
            return None;
        }
        let mut parser = Parser::new(value);
        let size = match parser.i32_() {
            Ok(size) if size >= 0 => size as usize,
            _ => return None,
        };
        let mut ids = Vec::with_capacity(size);
        for _ in 0..size {
            ids.push(parser.i64_().ok()?);
        }
        // `log_event_parse`'s fetch_end — trailing bytes reject the value.
        parser.fetch_end().ok()?;
        Some(ids)
    }

    /// `UserManager::save_my_saved_music_ids`
    /// (UserManager.cpp:7744-7750). The C++ iterates a FlatHashSet in
    /// hash order; the Rust BTreeSet's ascending order is deterministic
    /// and round-trips the same set.
    pub fn save_my_saved_music_ids(&mut self, env: &mut UserEnv) {
        self.are_my_saved_music_ids_inited = true;
        let mut value = Vec::new();
        store_parse::store_i32(&mut value, self.my_saved_music_ids.len() as i32);
        for saved_music_id in &self.my_saved_music_ids {
            store_parse::store_i64(&mut value, *saved_music_id);
        }
        env.global.td_db().binlog_pmc().set_raw(
            Self::get_my_saved_music_ids_database_key().as_bytes(),
            &value,
        );
    }

    /// `UserManager::get_my_saved_music_ids_database_key`
    /// (UserManager.cpp:7740-7742).
    fn get_my_saved_music_ids_database_key() -> &'static str {
        "my_saved_music_ids"
    }

    /// `UserManager::check_is_saved_music`'s local half
    /// (UserManager.cpp:7168-7176): the close gate, then membership
    /// over the loaded set. The `is_saved_music` wrapper (net-drain 09)
    /// handles the not-inited reload; the FileId → document-id
    /// conversion goes through the location registry.
    pub fn check_is_saved_music(
        &mut self,
        env: &UserEnv,
        file_id: FileId,
        promise: &mut Promise<()>,
    ) {
        if env.global.close_flag() {
            promise.set_error(Status::error(500, "Request aborted"));
            return;
        }
        assert!(
            self.are_my_saved_music_ids_inited,
            "check_is_saved_music requires the loaded saved-music list"
        );
        let document_id = self.get_saved_music_document_id(file_id, false).0;
        if !self.my_saved_music_ids.contains(&document_id) {
            promise.set_error(Status::error(404, "Not Found"));
            return;
        }
        promise.set_value(());
    }

    /// `UserManager::get_saved_music_document_id`
    /// (UserManager.cpp:4673-4693) over the location registry (module
    /// docs): an unknown or non-audio file answers the empty pair; the
    /// `from_server` LOG(ERROR) surfaces fire for registry misses.
    fn get_saved_music_document_id(
        &self,
        saved_music_file_id: FileId,
        from_server: bool,
    ) -> (i64, i64) {
        match self
            .saved_music_file_locations
            .get(&saved_music_file_id.get())
        {
            Some(location) => *location,
            None => {
                if from_server {
                    tdutils::log_error!(
                        "Saved music {saved_music_file_id:?} has no remote location"
                    );
                }
                (0, 0)
            }
        }
    }

    /// The `documents_manager_->on_get_document` Audio-classification
    /// stand-in (the DocumentsManager step replaces it): a `document`
    /// carrying a non-voice `documentAttributeAudio` is saved music. It
    /// gets a placeholder FileId registered in the location registry.
    /// `AudiosManager::get_audio`'s document half over the registry
    /// stand-in — the audio-attribute check plus the location-registry
    /// insert. pub(crate) for the td-level netted round-trip seeds (the
    /// client got the audio from a listing in the C++).
    pub(crate) fn parse_saved_music_document(
        &mut self,
        document: telegram_api::Document,
    ) -> Option<FileId> {
        let telegram_api::Document::document(document) = document else {
            return None;
        };
        let is_audio = document.attributes.iter().any(|attribute| {
            matches!(
                attribute,
                telegram_api::DocumentAttribute::documentAttributeAudio(audio)
                    if !audio.voice
            )
        });
        if !is_audio {
            return None;
        }
        let file_id = FileId::new(self.next_saved_music_file_id + 1, 0);
        self.next_saved_music_file_id += 1;
        self.saved_music_file_locations
            .insert(file_id.get(), (document.id, document.access_hash));
        Some(file_id)
    }

    /// The registry-stand-in half of the C++ `check_saved_music_file_id`
    /// file-view read: an uploaded saved-music file registers its
    /// document location for the `add_saved_music` refire (the C++ reads
    /// the same pair off the merged file node,
    /// `on_uploaded_saved_music_file` → `add_saved_music`).
    pub(crate) fn register_saved_music_file_location(
        &mut self,
        file_id: FileId,
        document_id: i64,
        access_hash: i64,
    ) {
        self.saved_music_file_locations
            .insert(file_id.get(), (document_id, access_hash));
    }

    /// `UserManager::check_saved_music_file_id`
    /// (UserManager.cpp:7115-7145) over the registry stand-in: an
    /// invalid FileId answers "Invalid file identifier specified", a
    /// registry miss "Unknown file identifier specified" (the C++
    /// separates the empty file view from the missing audio object the
    /// same way). Returns the inputDocument pair.
    fn check_saved_music_file_id(
        &self,
        file_id: &mut FileId,
        allow_empty: bool,
    ) -> Result<(i64, i64), Status> {
        if allow_empty && !file_id.is_valid() {
            return Ok((0, 0));
        }
        if !file_id.is_valid() {
            return Err(Status::error(400, "Invalid file identifier specified"));
        }
        let location = self.saved_music_file_locations.get(&file_id.get());
        let Some(&(document_id, access_hash)) = location else {
            return Err(Status::error(400, "Unknown file identifier specified"));
        };
        Ok((document_id, access_hash))
    }

    /// `UserManager::add_saved_music` (UserManager.cpp:7318-7332): the
    /// validation is local; the `messages.saveMusic` send (the C++
    /// `SaveMusicQuery`, cpp:1540) rides the `net` handle (net-drain 09)
    /// — the answer feeds `on_add_saved_music` through the `SaveMusic`
    /// kind (net-drain 09).
    pub fn add_saved_music(
        &mut self,
        file_id: FileId,
        after_file_id: FileId,
        mut promise: Promise<()>,
    ) {
        let mut file_id = file_id;
        let mut after_file_id = after_file_id;
        let (document_id, access_hash) = match self.check_saved_music_file_id(&mut file_id, false) {
            Ok(location) => location,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        let (after_document_id, after_access_hash) =
            match self.check_saved_music_file_id(&mut after_file_id, true) {
                Ok(location) => location,
                Err(status) => {
                    promise.set_error(status);
                    return;
                }
            };
        if file_id == after_file_id {
            promise.set_error(Status::error(400, "Can't place saved music after self"));
            return;
        }
        let has_after = after_file_id.is_valid();
        self.pending_add_saved_music_queries.push(promise);
        // `telegram_api::account_saveMusic::AFTER_ID_MASK`
        const SAVE_MUSIC_AFTER_ID_MASK: i32 = 1 << 1;
        let input_document =
            telegram_api::InputDocument::inputDocument(telegram_api::inputDocument {
                id: document_id,
                access_hash,
                file_reference: Vec::new(),
            });
        let after_input_document = has_after.then(|| {
            telegram_api::InputDocument::inputDocument(telegram_api::inputDocument {
                id: after_document_id,
                access_hash: after_access_hash,
                file_reference: Vec::new(),
            })
        });
        match &self.net {
            Some(net) => net.dispatch(
                UserNetKind::SaveMusic {
                    file_id,
                    after_file_id,
                },
                telegram_api::Function::account_saveMusic(telegram_api::account_saveMusic {
                    flags: if has_after {
                        SAVE_MUSIC_AFTER_ID_MASK
                    } else {
                        0
                    },
                    unsave: false,
                    id: Some(input_document),
                    after_id: after_input_document,
                }),
            ),
            None => tdutils::log_info!("Send account.saveMusic — [net-inert]"),
        }
    }

    /// `UserManager::on_add_saved_music` (UserManager.cpp:7334-7370).
    pub fn on_add_saved_music(
        &mut self,
        env: &mut UserEnv,
        file_id: FileId,
        after_file_id: FileId,
        promise: &mut Promise<()>,
    ) {
        let document_id = self.get_saved_music_document_id(file_id, true).0;
        if document_id != 0 && !self.my_saved_music_ids.contains(&document_id) {
            self.my_saved_music_ids.insert(document_id);
            self.save_my_saved_music_ids(env);
        }

        let user_id = self.my_id;
        let cache_live = self
            .user_saved_music
            .get(&user_id.get())
            .map(|cache| cache.count != -1 && cache.offset != -1)
            .unwrap_or(false);
        if cache_live {
            // td::remove(saved_music_file_ids, file_id) first...
            let existing = {
                let cache = self.user_saved_music.get(&user_id.get()).unwrap();
                cache
                    .saved_music_file_ids
                    .iter()
                    .position(|&fid| fid == file_id)
            };
            if let Some(index) = existing {
                self.user_saved_music
                    .get_mut(&user_id.get())
                    .unwrap()
                    .saved_music_file_ids
                    .remove(index);
            } else {
                self.user_saved_music.get_mut(&user_id.get()).unwrap().count += 1;
            }
            if !after_file_id.is_valid() {
                let cache = self.user_saved_music.get_mut(&user_id.get()).unwrap();
                if cache.offset > 0 {
                    cache.offset += 1;
                } else {
                    cache.saved_music_file_ids.insert(0, file_id);
                }
            } else {
                // ...then the anchor lookup over the mutated list, like
                // the C++ std::find
                let after = {
                    let cache = self.user_saved_music.get(&user_id.get()).unwrap();
                    cache
                        .saved_music_file_ids
                        .iter()
                        .position(|&fid| fid == after_file_id)
                };
                match after {
                    None => {
                        // can't find place to insert new saved music,
                        // drop cache
                        drop_user_saved_music_map(&mut self.user_saved_music, user_id, false);
                    }
                    Some(index) => {
                        self.user_saved_music
                            .get_mut(&user_id.get())
                            .unwrap()
                            .saved_music_file_ids
                            .insert(index + 1, file_id);
                    }
                }
            }
        }
        if !after_file_id.is_valid() {
            let loaded = self.get_user_full_force(env, user_id, "on_add_saved_music");
            if loaded {
                Self::on_update_user_full_first_saved_music_file_id(
                    self.users_full.get_mut(&user_id.get()).unwrap(),
                    file_id,
                );
                self.update_user_full(env, user_id, "on_add_saved_music", false);
            }
        }
        promise.set_value(());
    }

    /// `UserManager::remove_saved_music` (UserManager.cpp:7372-7378) —
    /// the validation is local; the `account.saveMusic(unsave)` send
    /// (the C++ `UnsaveMusicQuery`, cpp:1596) rides the `net` handle
    /// (net-drain 09).
    pub fn remove_saved_music(&mut self, file_id: FileId, mut promise: Promise<()>) {
        let mut file_id = file_id;
        let (document_id, access_hash) = match self.check_saved_music_file_id(&mut file_id, false) {
            Ok(location) => location,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        self.pending_remove_saved_music_queries.push(promise);
        match &self.net {
            Some(net) => net.dispatch(
                UserNetKind::UnsaveMusic { file_id },
                telegram_api::Function::account_saveMusic(telegram_api::account_saveMusic {
                    flags: 0,
                    unsave: true,
                    id: Some(telegram_api::InputDocument::inputDocument(
                        telegram_api::inputDocument {
                            id: document_id,
                            access_hash,
                            file_reference: Vec::new(),
                        },
                    )),
                    after_id: None,
                }),
            ),
            None => tdutils::log_info!("Send account.saveMusic(unsave) — [net-inert]"),
        }
    }

    /// `UserManager::on_remove_saved_music` (UserManager.cpp:7380-7420).
    /// The `reload_user_full` fallback (cpp:7415) rides the net step —
    /// the promise completes there (M.4.16).
    pub fn on_remove_saved_music(
        &mut self,
        env: &mut UserEnv,
        file_id: FileId,
        promise: &mut Promise<()>,
    ) {
        let document_id = self.get_saved_music_document_id(file_id, true).0;
        if self.my_saved_music_ids.remove(&document_id) {
            self.save_my_saved_music_ids(env);
        }

        let user_id = self.my_id;
        let cache_live = self
            .user_saved_music
            .get(&user_id.get())
            .map(|cache| cache.count != -1 && cache.offset != -1)
            .unwrap_or(false);
        let mut removed_from_cache = false;
        if cache_live {
            let (position, offset) = {
                let cache = self.user_saved_music.get(&user_id.get()).unwrap();
                (
                    cache
                        .saved_music_file_ids
                        .iter()
                        .position(|&fid| fid == file_id),
                    cache.offset,
                )
            };
            if let Some(index) = position {
                self.user_saved_music
                    .get_mut(&user_id.get())
                    .unwrap()
                    .saved_music_file_ids
                    .remove(index);
                removed_from_cache = true;
            }
            if removed_from_cache || offset == 0 {
                let cache = self.user_saved_music.get_mut(&user_id.get()).unwrap();
                if cache.count > 0 {
                    cache.count -= 1;
                    if cache.offset > cache.count {
                        cache.offset = cache.count;
                    }
                }
            } else {
                // can't find the removed saved music; drop cache, because
                // offset could have been changed
                drop_user_saved_music_map(&mut self.user_saved_music, user_id, false);
            }
        }
        let loaded = self.get_user_full_force(env, user_id, "on_remove_saved_music");
        let first_saved_matches = self
            .users_full
            .get(&user_id.get())
            .map(|user_full| user_full.first_saved_music_file_id == file_id)
            .unwrap_or(false);
        if loaded && first_saved_matches {
            let cache_head = self
                .user_saved_music
                .get(&user_id.get())
                .map(|cache| {
                    cache.count != -1 && cache.offset == 0 && !cache.saved_music_file_ids.is_empty()
                })
                .unwrap_or(false);
            let cache_empty = self
                .user_saved_music
                .get(&user_id.get())
                .map(|cache| cache.count == 0)
                .unwrap_or(false);
            if cache_head {
                let new_first = self
                    .user_saved_music
                    .get(&user_id.get())
                    .unwrap()
                    .saved_music_file_ids[0];
                Self::on_update_user_full_first_saved_music_file_id(
                    self.users_full.get_mut(&user_id.get()).unwrap(),
                    new_first,
                );
                self.update_user_full(env, user_id, "on_remove_saved_music 1", false);
            } else if cache_empty {
                Self::on_update_user_full_first_saved_music_file_id(
                    self.users_full.get_mut(&user_id.get()).unwrap(),
                    FileId::default(),
                );
                self.update_user_full(env, user_id, "on_remove_saved_music 2", false);
            } else {
                // `reload_user_full(user_id, std::move(promise),
                // "on_remove_saved_music")` (cpp:7408) — the promise
                // rides the reload
                return self.reload_user_full(
                    env,
                    user_id,
                    std::mem::take(promise),
                    "on_remove_saved_music",
                );
            }
        }
        promise.set_value(());
    }

    /// `UserManager::add_user_saved_music` (UserManager.cpp:7566-7573).
    fn add_user_saved_music(&mut self, user_id: UserId) -> &mut UserSavedMusic {
        assert!(user_id.is_valid());
        self.user_saved_music.entry(user_id.get()).or_default()
    }

    /// `UserManager::drop_user_saved_music` (UserManager.cpp:9535-9550);
    /// the map mutation lives in the free fn so the add/remove state
    /// machines stay borrow-clean.
    fn drop_user_saved_music(&mut self, user_id: UserId, is_empty: bool, source: &str) {
        let Some(cache) = self.user_saved_music.get(&user_id.get()) else {
            return;
        };
        let new_count = if is_empty { 0 } else { -1 };
        if cache.count == new_count {
            debug_assert!(cache.saved_music_file_ids.is_empty());
            debug_assert_eq!(cache.offset, cache.count);
            return;
        }
        tdutils::log_info!(
            "Drop saved music of {user_id:?} to {} from {source}",
            if is_empty { "empty" } else { "unknown" }
        );
        drop_user_saved_music_map(&mut self.user_saved_music, user_id, is_empty);
    }

    /// `UserManager::get_user_saved_music` (UserManager.cpp:7422-7490).
    /// The `users.getSavedMusic` send is [blocked: net] (M.4.16). The
    /// audio objects are the codegen-forced empty payloads until the
    /// AudiosManager step.
    pub fn get_user_saved_music(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        offset: i32,
        limit: i32,
        mut promise: Promise<td_api::Audios>,
    ) {
        if offset < 0 {
            promise.set_error(Status::error(400, "Parameter offset must be non-negative"));
            return;
        }
        if limit <= 0 {
            promise.set_error(Status::error(400, "Parameter limit must be positive"));
            return;
        }
        let mut limit = limit.min(MAX_GET_SAVED_MUSIC);

        if let Err(status) = self.get_input_user(env, user_id) {
            promise.set_error(status);
            return;
        }

        if self.get_user(user_id).is_none() {
            promise.set_error(Status::error(400, "User not found"));
            return;
        }

        self.add_user_saved_music(user_id);
        if let Some(user_saved_music) = self.user_saved_music.get(&user_id.get())
            && user_saved_music.count != -1
        {
            // know saved music count
            debug_assert_ne!(user_saved_music.offset, -1);
            tdutils::log_info!(
                "Have {} cached saved music files at offset {}",
                user_saved_music.count,
                user_saved_music.offset
            );
            if offset >= user_saved_music.count {
                // offset is too big
                promise.set_value(td_api::Audios {
                    total_count: user_saved_music.count,
                    audios: Vec::new(),
                });
                return;
            }
            if limit > user_saved_music.count - offset {
                limit = user_saved_music.count - offset;
            }
            let cache_begin = user_saved_music.offset;
            let cache_end = cache_begin + user_saved_music.saved_music_file_ids.len() as i32;
            if cache_begin <= offset && offset + limit <= cache_end {
                // answer query from cache
                let audios = user_saved_music.saved_music_file_ids
                    [(offset - cache_begin) as usize..(offset - cache_begin + limit) as usize]
                    .iter()
                    .map(|_| empty_audio_object())
                    .collect();
                promise.set_value(td_api::Audios {
                    total_count: user_saved_music.count,
                    audios,
                });
                return;
            }
        }

        let pending_request = PendingGetSavedMusicRequest {
            offset,
            limit,
            retry_count: 0,
            promise,
        };
        let requests = self
            .pending_get_user_saved_music_requests
            .entry(user_id.get())
            .or_default();
        requests.push(pending_request);
        if requests.len() != 1 {
            return;
        }

        self.send_get_user_saved_music_query(env, user_id);
    }

    /// `UserManager::send_get_user_saved_music_query`
    /// (UserManager.cpp:7492-7518) — the `users.getSavedMusic` send
    /// (the C++ `GetUserSavedMusicQuery`, cpp:1630) rides the `net`
    /// handle (net-drain 09); the answer feeds `on_get_user_saved_music`
    /// and the query promise lambda routes
    /// `finish_get_user_saved_music`.
    fn send_get_user_saved_music_query(&mut self, env: &UserEnv, user_id: UserId) {
        let Some(requests) = self
            .pending_get_user_saved_music_requests
            .get(&user_id.get())
        else {
            return;
        };
        debug_assert!(!requests.is_empty());
        let mut offset = requests[0].offset;
        let mut limit = requests[0].limit;
        if let Some(user_saved_music) = self.user_saved_music.get(&user_id.get())
            && user_saved_music.count != -1
            && offset >= user_saved_music.offset
        {
            let cache_end =
                user_saved_music.offset + user_saved_music.saved_music_file_ids.len() as i32;
            if offset < cache_end {
                // adjust offset to the end of cache
                debug_assert!(offset + limit > cache_end);
                limit = offset + limit - cache_end;
                offset = cache_end;
            }
        }
        let limit = limit.max(MAX_GET_SAVED_MUSIC / 5);
        let input_user = self.get_input_user_force(env, user_id);
        match &self.net {
            Some(net) => net.dispatch(
                UserNetKind::GetUserSavedMusic {
                    user_id,
                    offset,
                    limit,
                },
                telegram_api::Function::users_getSavedMusic(telegram_api::users_getSavedMusic {
                    id: Some(input_user),
                    offset,
                    limit,
                    hash: 0,
                }),
            ),
            None => tdutils::log_info!(
                "Send users.getSavedMusic for {user_id:?} at offset {offset} limit {limit} — [net-inert]"
            ),
        }
    }

    /// `UserManager::finish_get_user_saved_music`
    /// (UserManager.cpp:7520-7564): the pending-request retry machine.
    pub fn finish_get_user_saved_music(
        &mut self,
        env: &UserEnv,
        user_id: UserId,
        result: Result<(), Status>,
    ) {
        if env.global.close_flag() {
            // ignore_result_if_closing — the parked answer is dropped
            return;
        }
        let Some(mut pending_requests) = self
            .pending_get_user_saved_music_requests
            .remove(&user_id.get())
        else {
            return;
        };
        debug_assert!(!pending_requests.is_empty());
        if let Err(status) = result {
            for request in &mut pending_requests {
                request.promise.set_error(status.clone());
            }
            return;
        }
        let count = self.add_user_saved_music(user_id).count;
        if count == -1 {
            debug_assert!(self.have_min_user(user_id));
            // received result has just been dropped; resend request
            pending_requests[0].retry_count += 1;
            if pending_requests[0].retry_count >= 3 {
                pending_requests[0]
                    .promise
                    .set_error(Status::error(500, "Failed to return saved music"));
                pending_requests.remove(0);
                if pending_requests.is_empty() {
                    return;
                }
            }
            self.pending_get_user_saved_music_requests
                .insert(user_id.get(), pending_requests);
            self.send_get_user_saved_music_query(env, user_id);
            return;
        }

        let user_saved_music = self.user_saved_music.get(&user_id.get()).unwrap();
        debug_assert_ne!(user_saved_music.offset, -1);
        tdutils::log_info!(
            "Have {} cached saved music files at offset {}",
            user_saved_music.count,
            user_saved_music.offset
        );
        let mut left_requests = Vec::new();
        for (request_index, request_slot) in pending_requests.iter_mut().enumerate() {
            let mut request = std::mem::replace(
                request_slot,
                PendingGetSavedMusicRequest {
                    offset: 0,
                    limit: 0,
                    retry_count: 0,
                    promise: Promise::default(),
                },
            );

            if request.offset >= user_saved_music.count {
                // offset is too big
                request.promise.set_value(td_api::Audios {
                    total_count: user_saved_music.count,
                    audios: Vec::new(),
                });
                continue;
            }

            if request.limit > user_saved_music.count - request.offset {
                request.limit = user_saved_music.count - request.offset;
            }

            let cache_begin = user_saved_music.offset;
            let cache_end = cache_begin + user_saved_music.saved_music_file_ids.len() as i32;
            if cache_begin <= request.offset && request.offset + request.limit <= cache_end {
                // answer query from cache
                let audios = user_saved_music.saved_music_file_ids[(request.offset - cache_begin)
                    as usize
                    ..(request.offset - cache_begin + request.limit) as usize]
                    .iter()
                    .map(|_| empty_audio_object())
                    .collect();
                request.promise.set_value(td_api::Audios {
                    total_count: user_saved_music.count,
                    audios,
                });
                continue;
            }

            request.retry_count += 1;
            if request_index == 0 && request.retry_count >= 3 {
                request
                    .promise
                    .set_error(Status::error(500, "Failed to get saved music"));
                continue;
            }

            left_requests.push(request);
        }

        if !left_requests.is_empty() {
            let requests = self
                .pending_get_user_saved_music_requests
                .entry(user_id.get())
                .or_default();
            let need_send = requests.is_empty();
            requests.extend(left_requests);
            if need_send {
                self.send_get_user_saved_music_query(env, user_id);
            }
        }
    }

    /// `UserManager::on_get_user_saved_music`
    /// (UserManager.cpp:7575-7657): the users.getSavedMusic answer
    /// intake and cache fill. The document parse goes through the
    /// registry stand-in (`parse_saved_music_document`).
    pub fn on_get_user_saved_music(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        offset: i32,
        limit: i32,
        mut total_count: i32,
        documents: Vec<telegram_api::Document>,
    ) {
        let saved_music_count = documents.len() as i32;
        let min_total_count = if saved_music_count > 0 { offset } else { 0 } + saved_music_count;
        if total_count < min_total_count {
            tdutils::log_error!(
                "Receive wrong saved music total_count {total_count} for user {user_id:?}: \
                 receive {saved_music_count} saved music files with offset {offset}"
            );
            total_count = min_total_count;
        }
        if limit < saved_music_count {
            tdutils::log_error!(
                "Requested not more than {limit} saved music files, but {saved_music_count} \
                 received"
            );
        }

        assert!(self.users.contains_key(&user_id.get()));

        tdutils::log_info!(
            "Receive {saved_music_count} saved music files of {user_id:?} out of {total_count} \
             with offset {offset} and limit {limit}"
        );
        // add_user_saved_music + the cache fill — the record is taken out
        // of the map so the registry writes can borrow the manager
        let mut user_saved_music = self
            .user_saved_music
            .remove(&user_id.get())
            .unwrap_or_default();
        user_saved_music.count = total_count;
        debug_assert!(
            self.pending_get_user_saved_music_requests
                .contains_key(&user_id.get())
        );

        if user_saved_music.offset == -1 {
            user_saved_music.offset = 0;
            debug_assert!(user_saved_music.saved_music_file_ids.is_empty());
        }

        if offset != user_saved_music.saved_music_file_ids.len() as i32 + user_saved_music.offset {
            tdutils::log_info!(
                "Inappropriate offset to append {user_id:?} saved music to cache: offset = \
                 {offset}, current_offset = {}, saved_music_count = {}",
                user_saved_music.offset,
                user_saved_music.saved_music_file_ids.len()
            );
            user_saved_music.saved_music_file_ids.clear();
            user_saved_music.offset = offset;
        }

        let need_update_my_saved_music_ids =
            user_id == self.my_id && self.are_my_saved_music_ids_inited;
        let mut need_save_my_saved_music_ids = false;
        for document in documents {
            let parsed_file_id = self.parse_saved_music_document(document);
            let Some(file_id) = parsed_file_id else {
                user_saved_music.count -= 1;
                debug_assert!(user_saved_music.count >= 0);
                continue;
            };
            let document_id = self.get_saved_music_document_id(file_id, true).0;
            if document_id == 0 {
                user_saved_music.count -= 1;
                debug_assert!(user_saved_music.count >= 0);
                continue;
            }
            if need_update_my_saved_music_ids && !self.my_saved_music_ids.contains(&document_id) {
                self.my_saved_music_ids.insert(document_id);
                need_save_my_saved_music_ids = true;
            }
            user_saved_music.saved_music_file_ids.push(file_id);
            self.register_user_saved_music(env, user_id, file_id);
        }
        if user_saved_music.offset > user_saved_music.count {
            user_saved_music.offset = user_saved_music.count;
            user_saved_music.saved_music_file_ids.clear();
        }
        if need_save_my_saved_music_ids {
            self.save_my_saved_music_ids(env);
        }

        let known_saved_music_count = user_saved_music.saved_music_file_ids.len() as i32;
        if user_saved_music.offset + known_saved_music_count > user_saved_music.count {
            user_saved_music
                .saved_music_file_ids
                .truncate((user_saved_music.count - user_saved_music.offset) as usize);
        }
        let cache_offset_zero = user_saved_music.offset == 0;
        let cache_head = if user_saved_music.saved_music_file_ids.is_empty() {
            FileId::default()
        } else {
            user_saved_music.saved_music_file_ids[0]
        };
        self.user_saved_music
            .insert(user_id.get(), user_saved_music);

        if cache_offset_zero {
            let loaded = self.get_user_full_force(env, user_id, "on_get_user_saved_music");
            if loaded {
                Self::on_update_user_full_first_saved_music_file_id(
                    self.users_full.get_mut(&user_id.get()).unwrap(),
                    cache_head,
                );
                self.update_user_full(env, user_id, "on_get_user_saved_music", false);
            }
        }
    }

    /// `UserManager::reload_user_saved_music`
    /// (UserManager.cpp:7659-7666) — the `users.getSavedMusicByID` send
    /// (the C++ `GetUserSavedMusicByIdQuery`, cpp:1672) rides the `net`
    /// handle (net-drain 09); the caller promise parks in the by-id
    /// family queue until the answer (the C++ query carrier, §5). The
    /// caller is the FileReferenceManager repair leg (Files plane).
    pub fn reload_user_saved_music(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        document_id: i64,
        access_hash: i64,
        mut promise: Promise<()>,
    ) {
        self.get_user_force(env, user_id, "reload_user_saved_music");
        let input_user = match self.get_input_user(env, user_id) {
            Ok(input_user) => input_user,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };
        self.pending_reload_saved_music_by_id_queries.push(promise);
        match &self.net {
            Some(net) => net.dispatch(
                UserNetKind::GetUserSavedMusicById { user_id },
                telegram_api::Function::users_getSavedMusicByID(
                    telegram_api::users_getSavedMusicByID {
                        id: Some(input_user),
                        documents: vec![telegram_api::InputDocument::inputDocument(
                            telegram_api::inputDocument {
                                id: document_id,
                                access_hash,
                                file_reference: Vec::new(),
                            },
                        )],
                    },
                ),
            ),
            None => tdutils::log_info!(
                "Reload saved music document {document_id} (hash {access_hash}) of {user_id:?} — [net-inert]"
            ),
        }
    }

    /// `UserManager::get_user_saved_music_file_source_id`
    /// (UserManager.cpp:7668-7681).
    pub fn get_user_saved_music_file_source_id(
        &mut self,
        user_id: UserId,
        document_id: i64,
        access_hash: i64,
    ) -> FileSourceId {
        if !user_id.is_valid() {
            return FileSourceId::default();
        }

        if let Some(&source_id) =
            self.user_saved_music_file_source_ids
                .get(&(user_id.get(), document_id, access_hash))
            && source_id.is_valid()
        {
            return source_id;
        }
        let source_id = self.create_file_source_id();
        self.user_saved_music_file_source_ids
            .insert((user_id.get(), document_id, access_hash), source_id);
        source_id
    }

    /// `UserManager::register_user_saved_music`
    /// (UserManager.cpp:4696-4712).
    fn register_user_saved_music(&mut self, env: &mut UserEnv, user_id: UserId, file_id: FileId) {
        if !file_id.is_valid() || env.auth_is_bot {
            return;
        }
        let document_id = self.get_saved_music_document_id(file_id, true);
        if document_id.0 == 0 {
            return;
        }
        // the file-source registration + add_file_source fan-out — the
        // Files step
        let _ = self.get_user_saved_music_file_source_id(user_id, document_id.0, document_id.1);
    }

    /// `UserManager::on_update_user_full_first_saved_music_file_id`
    /// (UserManager.cpp:4648-4654).
    fn on_update_user_full_first_saved_music_file_id(
        user_full: &mut UserFull,
        first_saved_music_file_id: FileId,
    ) {
        if user_full.first_saved_music_file_id != first_saved_music_file_id {
            user_full.first_saved_music_file_id = first_saved_music_file_id;
            user_full.is_first_saved_music_file_id_changed = true;
            user_full.is_changed = true;
        }
    }

    /// `UserManager::reload_my_saved_music_list`
    /// (UserManager.cpp:7683-7699): the promise queue is local; the
    /// first queued caller sends `account.getSavedMusicIds` over the
    /// `net` handle (net-drain 09) — the answer feeds
    /// `on_get_my_saved_music_list` through the `GetSavedMusicIds` kind.
    pub fn reload_my_saved_music_list(&mut self, promise: Promise<()>) {
        self.reload_my_saved_music_queries.push(promise);
        if self.reload_my_saved_music_queries.len() != 1 {
            return;
        }
        self.send_reload_my_saved_music_list_query();
    }

    /// The `account.getSavedMusicIds` send leg (cpp:7702-7706) — the
    /// vector hash over the sorted id set (the BTreeSet iterates
    /// ascending; the C++ sorts its copy).
    fn send_reload_my_saved_music_list_query(&mut self) {
        let numbers: Vec<u64> = self
            .my_saved_music_ids
            .iter()
            .map(|&id| id as u64)
            .collect();
        let hash = get_vector_hash(&numbers);
        match &self.net {
            Some(net) => net.dispatch(
                UserNetKind::GetSavedMusicIds,
                telegram_api::Function::account_getSavedMusicIds(
                    telegram_api::account_getSavedMusicIds { hash },
                ),
            ),
            None => tdutils::log_info!("Send account.getSavedMusicIds — [net-inert]"),
        }
    }

    /// `UserManager::is_saved_music` (UserManager.cpp:7154-7166): the
    /// loaded set answers locally; the not-inited list queues the
    /// follow-up check (the C++ lambda) and rides the reload.
    pub fn is_saved_music(&mut self, env: &mut UserEnv, file_id: FileId, mut promise: Promise<()>) {
        if !self.are_my_saved_music_ids_inited {
            self.pending_saved_music_checks.push((file_id, promise));
            self.reload_my_saved_music_list(Promise::default());
            return;
        }
        self.check_is_saved_music(env, file_id, &mut promise);
    }

    /// `UserManager::on_get_my_saved_music_list`
    /// (UserManager.cpp:7701-7738). The close-path abort applies when
    /// the result is ok; the error arm fails the queue and the
    /// `is_saved_music` follow-ups; the not-modified form keeps the set;
    /// the ok tail re-runs the queued checks (the collapsed C++ lambda
    /// hop).
    pub fn on_get_my_saved_music_list(
        &mut self,
        env: &mut UserEnv,
        saved_music_ids: Result<telegram_api::account_SavedMusicIds, Status>,
    ) {
        let saved_music_ids = if env.global.close_flag() && saved_music_ids.is_ok() {
            // the C++ turns an ok result into request_aborted_error
            Err(Status::error(500, "Request aborted"))
        } else {
            saved_music_ids
        };
        let mut promises = std::mem::take(&mut self.reload_my_saved_music_queries);
        if let Err(status) = saved_music_ids {
            for promise in &mut promises {
                promise.set_error(status.clone());
            }
            for (_, mut promise) in std::mem::take(&mut self.pending_saved_music_checks) {
                promise.set_error(status.clone());
            }
            return;
        }
        let saved_music_ids = saved_music_ids.as_ref().ok().unwrap();
        match saved_music_ids {
            telegram_api::account_SavedMusicIds::account_savedMusicIdsNotModified(_) => {}
            telegram_api::account_SavedMusicIds::account_savedMusicIds(ids) => {
                self.my_saved_music_ids.clear();
                for saved_music_id in &ids.ids {
                    if *saved_music_id == 0 || self.my_saved_music_ids.contains(saved_music_id) {
                        tdutils::log_error!("Receive {saved_music_id}");
                        continue;
                    }
                    self.my_saved_music_ids.insert(*saved_music_id);
                }
                self.save_my_saved_music_ids(env);
            }
        }
        for promise in &mut promises {
            promise.set_value(());
        }
        // the `is_saved_music` not-inited follow-ups (the C++ lambda hop
        // send_closure(actor_id, &UserManager::check_is_saved_music))
        for (file_id, mut promise) in std::mem::take(&mut self.pending_saved_music_checks) {
            self.check_is_saved_music(env, file_id, &mut promise);
        }
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

    // ------------------------------------------------------------------
    // Contacts (UserManager.cpp:7910-8889). The query senders ride the
    // `net` handle: `reload_contacts` (GetContactsQuery), `add_contact`
    // (AddContactQuery), `do_import_contacts` (ImportContactsQuery),
    // `remove_contacts`/`remove_contacts_by_phone_number`
    // (DeleteContacts/DeleteContactsByPhoneNumber),
    // `clear_imported_contacts` (ResetContactsQuery), the close-friends
    // editor (EditCloseFriendsQuery), `search_user_by_phone_number`
    // (ResolvePhoneQuery), `reload_contact_birthdates`
    // (GetContactsBirthdaysQuery) and `hide_contact_birthdays`
    // (DismissContactBirthdaysSuggestionQuery).
    // The load-gated entry points follow the C++ caller-retry contract:
    // they park the promise in the load queue and return the empty
    // result; `add_contact` and `get_imported_contact_count` re-dispatch
    // internally, so they park in `pending_load_contacts_calls` (the
    // C++ actor-message analogue).

    /// The `UserManager` constructor's contacts block
    /// (UserManager.cpp:2748-2759).
    pub fn init_contacts_state(
        &mut self,
        binlog_pmc: &mut BinlogKeyValue,
        unix_time: i32,
        use_chat_info_database: bool,
        auth_is_bot: bool,
    ) {
        if use_chat_info_database {
            let next_contacts_sync_date = binlog_pmc.get("next_contacts_sync_date");
            if !next_contacts_sync_date.is_empty() {
                let next_contacts_sync_date: i32 = next_contacts_sync_date.parse().unwrap_or(0);
                self.next_contacts_sync_date = next_contacts_sync_date.min(unix_time + 100_000);
            }

            let saved_contact_count = binlog_pmc.get("saved_contact_count");
            if !saved_contact_count.is_empty() {
                self.saved_contact_count = saved_contact_count.parse().unwrap_or(0);
            }
        } else if !auth_is_bot {
            binlog_pmc.erase("next_contacts_sync_date");
            binlog_pmc.erase("saved_contact_count");
        }
    }

    /// `UserManager::load_contacts` (UserManager.cpp:7910-7934).
    pub fn load_contacts(&mut self, env: &mut UserEnv, mut promise: Promise<()>) {
        if env.auth_is_bot {
            self.are_contacts_loaded = true;
            self.saved_contact_count = 0;
        }
        if self.are_contacts_loaded && self.saved_contact_count != -1 {
            tdutils::log_info!("Contacts are already loaded");
            promise.set_value(());
            return;
        }
        self.load_contacts_queries.push(promise);
        if self.load_contacts_queries.len() == 1 {
            if env.global.use_chat_info_database()
                && self.next_contacts_sync_date > 0
                && self.saved_contact_count != -1
            {
                tdutils::log_info!("Load contacts from database");
                let value = env.global.td_db().common_kv().get(b"user_contacts");
                self.on_load_contacts_from_database(env, value);
            } else {
                tdutils::log_info!("Load contacts from server");
                self.reload_contacts(env, true);
            }
        } else {
            tdutils::log_info!("Load contacts request has already been sent");
        }
    }

    /// `UserManager::get_contacts_hash` (UserManager.cpp:7936-7956).
    pub fn get_contacts_hash(&mut self, env: &mut UserEnv) -> i64 {
        if !self.are_contacts_loaded {
            return 0;
        }

        let mut user_ids = self.contacts_hints.search_empty(100_000).1;
        debug_assert!(user_ids.windows(2).all(|w| w[0] <= w[1]));
        let my_id = self.get_my_id();
        if self.get_user_force(env, my_id, "get_contacts_hash") {
            let u = self.get_user(my_id).expect("force-loaded above");
            if u.is_contact {
                let pos = user_ids.partition_point(|&id| id < my_id.get());
                user_ids.insert(pos, my_id.get());
            }
        }

        let mut numbers = Vec::with_capacity(user_ids.len() + 1);
        numbers.push(self.saved_contact_count as u64);
        for user_id in user_ids {
            numbers.push(user_id as u64);
        }
        get_vector_hash(&numbers)
    }

    /// `UserManager::reload_contacts` (UserManager.cpp:7958-7965) — the
    /// `GetContactsQuery` send over the `net` handle; the sync-date gate
    /// advances to `i32::MAX` exactly like the C++ before the send (the
    /// C++ `create(…, {{DialogId(user_id)}}` chain-id arg rides the
    /// sequencing plane the walking-skeleton dispatch drops).
    pub fn reload_contacts(&mut self, env: &mut UserEnv, force: bool) {
        if !env.global.close_flag()
            && !env.auth_is_bot
            && self.next_contacts_sync_date != i32::MAX
            && (self.next_contacts_sync_date < env.unix_time() || force)
        {
            self.next_contacts_sync_date = i32::MAX;
            let hash = self.get_contacts_hash(env);
            self.send_user_query(
                UserNetKind::GetContacts,
                telegram_api::Function::contacts_getContacts(telegram_api::contacts_getContacts {
                    hash,
                }),
                "contacts.getContacts",
            );
        }
    }

    /// `UserManager::save_next_contacts_sync_date`
    /// (UserManager.cpp:7967-7975).
    pub fn save_next_contacts_sync_date(&mut self, env: &mut UserEnv) {
        if env.global.close_flag() || !env.global.use_chat_info_database() {
            return;
        }
        env.global.binlog_pmc().set(
            "next_contacts_sync_date",
            &self.next_contacts_sync_date.to_string(),
        );
    }

    /// `UserManager::save_contacts_to_database` (UserManager.cpp:7977-7999)
    /// — the binlog force-sync + async sqlite set collapse onto the
    /// synchronous pmc + kv writes (the M.4.5 divergence).
    pub fn save_contacts_to_database(&mut self, env: &mut UserEnv) {
        if !env.global.use_chat_info_database() || !self.are_contacts_loaded {
            return;
        }

        tdutils::log_info!("Schedule save contacts to database");
        let user_ids = self.contacts_hints.search_empty(100_000).1;

        env.global
            .binlog_pmc()
            .set("saved_contact_count", &self.saved_contact_count.to_string());
        let mut value = Vec::with_capacity(8 * (user_ids.len() + 2));
        store_user_ids_log_event(&mut value, &user_ids);
        env.global.td_db().common_kv().set(b"user_contacts", &value);
        self.save_next_contacts_sync_date(env);
    }

    /// `UserManager::on_get_contacts` (UserManager.cpp:8001-8054) — the
    /// notModified arm re-sends `GetContactsStatusesQuery` (fire-and-
    /// forget, like the C++ handler).
    pub fn on_get_contacts(
        &mut self,
        env: &mut UserEnv,
        new_contacts: telegram_api::contacts_Contacts,
    ) {
        self.next_contacts_sync_date = env.unix_time() + random_fast(70_000, 100_000);

        match new_contacts {
            telegram_api::contacts_Contacts::contacts_contactsNotModified(
                telegram_api::contacts_contactsNotModified,
            ) => {
                if self.saved_contact_count == -1 {
                    self.saved_contact_count = 0;
                }
                self.on_get_contacts_finished(env, self.contacts_hints.size());
                self.send_user_query(
                    UserNetKind::GetContactsStatuses,
                    telegram_api::Function::contacts_getStatuses(
                        telegram_api::contacts_getStatuses,
                    ),
                    "contacts.getStatuses",
                );
            }
            telegram_api::contacts_Contacts::contacts_contacts(contacts) => {
                let mut contact_user_ids: BTreeSet<i64> = BTreeSet::new();
                for user in &contacts.users {
                    let user_id = get_wire_user_id(user);
                    if !user_id.is_valid() {
                        tdutils::log_error!("Receive invalid {user_id:?}");
                        continue;
                    }
                    contact_user_ids.insert(user_id.get());
                }
                self.on_get_users(env, contacts.users, "on_get_contacts");

                let my_id = self.get_my_id();
                let user_ids: Vec<i64> = self.users.keys().copied().collect();
                for user_id in user_ids {
                    let is_contact = self
                        .users
                        .get(&user_id)
                        .expect("collected above")
                        .is_contact;
                    let should_be_contact = contact_user_ids.contains(&user_id);
                    if is_contact != should_be_contact {
                        if is_contact {
                            tdutils::log_info!("Drop contact with {user_id}");
                            if user_id != my_id.get() {
                                assert!(
                                    self.contacts_hints.has_key(user_id),
                                    "{} {} {}",
                                    my_id.get(),
                                    user_id,
                                    self.get_user_search_text_of(UserId::new(user_id))
                                );
                            }
                            self.on_update_user_is_contact_impl(
                                env,
                                UserId::new(user_id),
                                false,
                                false,
                                false,
                            );
                            {
                                let u = self.users.get_mut(&user_id).expect("collected above");
                                assert!(u.is_is_contact_changed);
                                u.cache_version = 0;
                                u.is_repaired = false;
                            }
                            self.update_user(env, UserId::new(user_id), false, false);
                            assert!(
                                !self
                                    .users
                                    .get(&user_id)
                                    .expect("collected above")
                                    .is_contact
                            );
                            if user_id != my_id.get() {
                                assert!(!self.contacts_hints.has_key(user_id));
                            }
                        } else {
                            tdutils::log_error!(
                                "Receive non-contact {user_id} in the list of contacts"
                            );
                        }
                    }
                }

                self.saved_contact_count = contacts.saved_count;
                self.on_get_contacts_finished(env, usize::MAX);
            }
        }
    }

    /// `UserManager::on_get_contacts_failed` (UserManager.cpp:8056-8060)
    /// — the load failure errors the parked promises (the request
    /// actors' `do_send_error`) and the parked load-gated td_api
    /// requests.
    pub fn on_get_contacts_failed(&mut self, env: &mut UserEnv, error: Status) {
        self.next_contacts_sync_date = env.unix_time() + random_fast(5, 10);
        fail_promises(&mut self.load_contacts_queries, &error);
        for request in std::mem::take(&mut self.pending_user_requests) {
            let mut promise = match request {
                PendingUserRequest::GetCloseFriends { promise }
                | PendingUserRequest::SearchContacts { promise, .. }
                | PendingUserRequest::ImportContacts { promise, .. }
                | PendingUserRequest::ChangeImportedContacts { promise, .. } => promise,
            };
            promise.set_error(error.clone());
        }
        for call in std::mem::take(&mut self.pending_load_contacts_calls) {
            match call {
                PendingLoadContactsCall::AddContact { mut promise, .. } => {
                    promise.set_error(error.clone());
                }
                PendingLoadContactsCall::GetImportedContactCount { mut promise } => {
                    promise.set_error(error.clone());
                }
                PendingLoadContactsCall::SharePhoneNumber { mut promise, .. } => {
                    promise.set_error(error.clone());
                }
            }
        }
    }

    /// `UserManager::on_load_contacts_from_database`
    /// (UserManager.cpp:8062-8103) — the
    /// `load_contact_users_multipromise_` fan-in collapses onto the
    /// synchronous per-user loads; a user that fails to load (a broken
    /// row) falls back to `reload_contacts(true)` like the C++ multipromise
    /// error path.
    fn on_load_contacts_from_database(&mut self, env: &mut UserEnv, value: Vec<u8>) {
        if env.global.close_flag() {
            return;
        }
        if value.is_empty() {
            self.reload_contacts(env, true);
            return;
        }

        let user_ids = match parse_user_ids_log_event(&value) {
            Ok(user_ids) => user_ids,
            Err(_) => {
                tdutils::log_error!("Failed to load contacts from database");
                self.reload_contacts(env, true);
                return;
            }
        };

        let version = Parser::new_log_event(&value)
            .map(|parser| parser.version())
            .unwrap_or(0);
        if version < store_parse::version::ADD_USER_FLAGS2 {
            self.next_contacts_sync_date = 0;
            self.save_next_contacts_sync_date(env);
            self.reload_contacts(env, true);
        }

        tdutils::log_info!(
            "Successfully loaded {} contacts from database",
            user_ids.len()
        );
        let expected_contact_count = user_ids.len();
        for user_id in user_ids {
            if !self.get_user_force(env, UserId::new(user_id), "load contacts from database") {
                tdutils::log_info!("Failed to load contact users from database");
                self.reload_contacts(env, true);
                return;
            }
        }
        self.on_get_contacts_finished(env, expected_contact_count);
    }

    /// `UserManager::on_get_contacts_finished` (UserManager.cpp:8105-8112)
    /// — the `set_promises(load_contacts_queries_)` completion re-runs
    /// the parked load-gated td_api requests (the C++ request actors'
    /// `do_run` re-run on the resolved load promise).
    fn on_get_contacts_finished(&mut self, env: &mut UserEnv, expected_contact_count: usize) {
        tdutils::log_info!(
            "Finished to get {} contacts out of expected {}",
            self.contacts_hints.size(),
            expected_contact_count
        );
        self.are_contacts_loaded = true;
        set_promises(&mut self.load_contacts_queries);
        if expected_contact_count != self.contacts_hints.size() {
            self.save_contacts_to_database(env);
        }
        for call in std::mem::take(&mut self.pending_load_contacts_calls) {
            match call {
                PendingLoadContactsCall::AddContact {
                    user_id,
                    contact,
                    share_phone_number,
                    promise,
                } => self.add_contact(env, user_id, contact, share_phone_number, promise),
                PendingLoadContactsCall::GetImportedContactCount { promise } => {
                    self.get_imported_contact_count(env, promise)
                }
                PendingLoadContactsCall::SharePhoneNumber { user_id, promise } => {
                    self.share_phone_number(env, user_id, promise)
                }
            }
        }
        self.drain_pending_user_requests(env);
    }

    /// `UserManager::on_get_contacts_statuses` (UserManager.cpp:8114-8123).
    pub fn on_get_contacts_statuses(
        &mut self,
        env: &mut UserEnv,
        statuses: Vec<telegram_api::ContactStatus>,
    ) {
        let my_user_id = self.get_my_id();
        for status in statuses {
            let user_id = UserId::new(status.user_id);
            if user_id != my_user_id {
                self.on_update_user_online(env, user_id, status.status.unwrap_or_default());
            }
        }
        self.save_next_contacts_sync_date(env);
    }

    /// `UserManager::add_contact` (UserManager.cpp:8125-8153) — the
    /// promise rides the family queue onto the `AddContactQuery` (the §5
    /// restore); the note branch (`edit_note_` → the `UpdateContactNote`
    /// ack chain, #4) is the formatted-text plane — `Contact` never
    /// carries `edit_note_`/`note_`, so flags stays 0 on the wire form.
    pub fn add_contact(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        contact: Contact,
        share_phone_number: bool,
        mut promise: Promise<()>,
    ) {
        let input_user = match self.get_input_user(env, user_id) {
            Ok(input_user) => input_user,
            Err(error) => return promise.set_error(error),
        };

        if !self.are_contacts_loaded {
            self.pending_load_contacts_calls
                .push(PendingLoadContactsCall::AddContact {
                    user_id,
                    contact,
                    share_phone_number,
                    promise,
                });
            self.load_contacts(env, Promise::default());
            return;
        }

        tdutils::log_info!(
            "Add contact {contact:?} for {user_id:?} with share_phone_number = {share_phone_number}"
        );
        self.pending_add_contact_queries.push((user_id, promise));
        self.send_user_query(
            UserNetKind::AddContact,
            telegram_api::Function::contacts_addContact(telegram_api::contacts_addContact {
                flags: 0,
                add_phone_privacy_exception: share_phone_number,
                id: Some(input_user),
                first_name: contact.get_first_name().to_owned(),
                last_name: contact.get_last_name().to_owned(),
                phone: contact.get_phone_number().to_owned(),
                note: None,
            }),
            "contacts.addContact",
        );
    }

    /// `UserManager::import_contacts` (UserManager.cpp:8155-8181) — the
    /// `ImportContactsQuery` send is [blocked: net]; the caller-retry
    /// contract keeps the promise in the load queue when the contacts
    /// aren't loaded yet.
    pub fn import_contacts(
        &mut self,
        env: &mut UserEnv,
        contacts: Vec<Contact>,
        random_id: &mut i64,
        mut promise: Promise<()>,
    ) -> (Vec<UserId>, Vec<i32>) {
        if !self.are_contacts_loaded {
            self.load_contacts(env, promise);
            return (Vec::new(), Vec::new());
        }

        tdutils::log_info!(
            "Asked to import {} contacts with random_id = {}",
            contacts.len(),
            *random_id
        );
        if *random_id != 0 {
            // request has already been sent before
            let Some(result) = self.imported_contacts.remove(random_id) else {
                panic!("reserved import result slot is missing");
            };
            promise.set_value(());
            return result;
        }

        loop {
            *random_id = tdutils::random::secure_int64();
            if *random_id != 0 && *random_id != 1 && !self.imported_contacts.contains_key(random_id)
            {
                break;
            }
        }
        self.imported_contacts
            .insert(*random_id, (Vec::new(), Vec::new())); // reserve place for result

        self.do_import_contacts(env, contacts, *random_id, promise);
        (Vec::new(), Vec::new())
    }

    /// `UserManager::do_import_contacts` (UserManager.cpp:8183-8206) —
    /// the task registers (the promise rides it, exactly like the C++)
    /// and the `ImportContactsQuery` rides the `net` handle with the
    /// `inputPhoneContact` payloads.
    fn do_import_contacts(
        &mut self,
        env: &mut UserEnv,
        contacts: Vec<Contact>,
        random_id: i64,
        mut promise: Promise<()>,
    ) {
        let size = contacts.len();
        if size == 0 {
            self.on_import_contacts_finished(env, random_id, Vec::new(), Vec::new());
            promise.set_value(());
            return;
        }

        let task = ImportContactsTask {
            promise,
            input_contacts: contacts,
            imported_user_ids: vec![UserId::default(); size],
            unimported_contact_invites: vec![0; size],
        };
        let is_added = self.import_contact_tasks.insert(random_id, task).is_none();
        assert!(is_added);

        // the C++ builds the payloads before registering the task; both
        // orders are unobservable (the query answers after the turn)
        let input_phone_contacts = self
            .import_contact_tasks
            .get(&random_id)
            .expect("the task registered above")
            .input_contacts
            .iter()
            .enumerate()
            .map(|(i, contact)| contact.get_input_phone_contact(i as i64))
            .collect();
        self.send_import_contacts_query(random_id, input_phone_contacts);
    }

    /// The `ImportContactsQuery` send (cpp:353-360) — the kind carries
    /// the handler's `random_id_`/`sent_size_` members.
    fn send_import_contacts_query(
        &mut self,
        random_id: i64,
        input_phone_contacts: Vec<telegram_api::InputContact>,
    ) {
        let sent_size = input_phone_contacts.len();
        self.send_user_query(
            UserNetKind::ImportContacts {
                random_id,
                sent_size,
            },
            telegram_api::Function::contacts_importContacts(
                telegram_api::contacts_importContacts {
                    contacts: input_phone_contacts,
                },
            ),
            "contacts.importContacts",
        );
    }

    /// `UserManager::on_imported_contacts` (UserManager.cpp:8208-8268) —
    /// the retry round re-sends the `ImportContactsQuery` with the retry
    /// client ids and keeps the task registered, exactly like the C++.
    pub fn on_imported_contacts(
        &mut self,
        env: &mut UserEnv,
        random_id: i64,
        result: Result<telegram_api::contacts_ImportedContacts, Status>,
    ) {
        if let Err(error) = result {
            let mut task = self
                .import_contact_tasks
                .remove(&random_id)
                .expect("import task exists");
            task.promise.set_error(error.clone());
            // the parked td_api import/change request of the task errors
            // out (the C++ request-actor promise chain's `do_send_error`)
            self.fail_pending_import_request(random_id, &error);
            return;
        }
        let imported_contacts = result.expect("checked above");

        self.on_get_users(env, imported_contacts.users, "on_imported_contacts");

        {
            let task = self
                .import_contact_tasks
                .get_mut(&random_id)
                .expect("import task exists");
            for imported_contact in &imported_contacts.imported {
                let client_id = imported_contact.client_id;
                if client_id < 0 || client_id >= task.imported_user_ids.len() as i64 {
                    tdutils::log_error!("Wrong client_id {client_id} returned");
                    continue;
                }
                task.imported_user_ids[client_id as usize] = UserId::new(imported_contact.user_id);
            }
            for popular_contact in &imported_contacts.popular_invites {
                let client_id = popular_contact.client_id;
                if client_id < 0 || client_id >= task.unimported_contact_invites.len() as i64 {
                    tdutils::log_error!("Wrong client_id {client_id} returned");
                    continue;
                }
                if popular_contact.importers < 0 {
                    tdutils::log_error!(
                        "Wrong number of importers {} returned",
                        popular_contact.importers
                    );
                    continue;
                }
                task.unimported_contact_invites[client_id as usize] = popular_contact.importers;
            }
        }

        if !imported_contacts.retry_contacts.is_empty() {
            // the C++ re-sends the retry client ids with the same
            // random_id and keeps the task
            let input_phone_contacts = {
                let task = self
                    .import_contact_tasks
                    .get(&random_id)
                    .expect("import task exists");
                let total_size = task.input_contacts.len() as i64;
                let mut input_phone_contacts =
                    Vec::with_capacity(imported_contacts.retry_contacts.len());
                for client_id in &imported_contacts.retry_contacts {
                    if *client_id < 0 || *client_id >= total_size {
                        tdutils::log_error!("Wrong client_id {client_id} returned");
                        continue;
                    }
                    input_phone_contacts.push(
                        task.input_contacts[*client_id as usize]
                            .get_input_phone_contact(*client_id),
                    );
                }
                input_phone_contacts
            };
            self.send_import_contacts_query(random_id, input_phone_contacts);
            return;
        }

        let mut task = self
            .import_contact_tasks
            .remove(&random_id)
            .expect("checked above");
        self.on_import_contacts_finished(
            env,
            random_id,
            std::mem::take(&mut task.imported_user_ids),
            std::mem::take(&mut task.unimported_contact_invites),
        );
        task.promise.set_value(());
    }

    /// The parked td_api import/change request of an errored task —
    /// the C++ request-actor promise chain's `do_send_error` (the task
    /// registry keyed the random id).
    fn fail_pending_import_request(&mut self, random_id: i64, error: &Status) {
        let mut index = 0;
        while index < self.pending_user_requests.len() {
            let matches = match &self.pending_user_requests[index] {
                PendingUserRequest::ImportContacts { random_id: id, .. }
                | PendingUserRequest::ChangeImportedContacts { random_id: id, .. } => {
                    *id == random_id
                }
                _ => false,
            };
            if matches {
                let mut promise = match self.pending_user_requests.remove(index) {
                    PendingUserRequest::ImportContacts { promise, .. }
                    | PendingUserRequest::ChangeImportedContacts { promise, .. } => promise,
                    _ => unreachable!("the match above filtered the kind"),
                };
                promise.set_error(error.clone());
            } else {
                index += 1;
            }
        }
    }

    /// `UserManager::on_import_contacts_finished`
    /// (UserManager.cpp:8270-8328).
    pub fn on_import_contacts_finished(
        &mut self,
        env: &mut UserEnv,
        random_id: i64,
        imported_contact_user_ids: Vec<UserId>,
        unimported_contact_invites: Vec<i32>,
    ) {
        tdutils::log_info!(
            "Contacts import with random_id {random_id} has finished: {imported_contact_user_ids:?}"
        );
        if random_id == 1 {
            // import from change_imported_contacts
            self.all_imported_contacts = std::mem::take(&mut self.next_all_imported_contacts);

            let result_size = self.imported_contacts_unique_id.len();
            let unique_size = self.all_imported_contacts.len();
            let add_size = self.imported_contacts_pos.len();

            self.imported_contact_user_ids
                .resize(result_size, UserId::default());
            self.unimported_contact_invites.resize(result_size, 0);

            assert!(imported_contact_user_ids.len() == add_size);
            assert!(unimported_contact_invites.len() == add_size);
            assert!(self.imported_contacts_unique_id.len() == result_size);

            let mut unique_id_to_unimported_contact_invites: HashMap<i64, i32> = HashMap::new();
            for i in 0..add_size {
                let unique_id = self.imported_contacts_pos[i];
                // to ensure updateUser
                self.get_user_id_object(
                    env,
                    imported_contact_user_ids[i],
                    "on_import_contacts_finished",
                );
                self.all_imported_contacts[unique_id].set_user_id(imported_contact_user_ids[i]);
                unique_id_to_unimported_contact_invites
                    .insert(unique_id as i64, unimported_contact_invites[i]);
            }

            if env.global.use_chat_info_database() {
                // the binlog force-sync collapses onto the synchronous kv
                // write (the M.4.5 divergence)
                let mut value = Vec::new();
                store_contacts_log_event(&mut value, &self.all_imported_contacts);
                env.global
                    .td_db()
                    .common_kv()
                    .set(b"user_imported_contacts", &value);
            }

            for i in 0..result_size {
                let unique_id = self.imported_contacts_unique_id[i];
                assert!(unique_id < unique_size);
                self.imported_contact_user_ids[i] =
                    self.all_imported_contacts[unique_id].get_user_id();
                match unique_id_to_unimported_contact_invites.get(&(unique_id as i64)) {
                    Some(invites) => self.unimported_contact_invites[i] = *invites,
                    None => self.unimported_contact_invites[i] = 0,
                }
            }
            // the parked change requests re-run now (the C++ request actor
            // wakes on the task promise and pops the member state)
            self.drain_pending_user_requests(env);
            return;
        }

        let entry = self
            .imported_contacts
            .get_mut(&random_id)
            .expect("reserved import result slot exists");
        assert!(entry.0.is_empty());
        assert!(entry.1.is_empty());
        *entry = (imported_contact_user_ids, unimported_contact_invites);
        // the parked import/change requests re-run now (the C++ request
        // actor wakes on the task promise and pops the reserved result)
        self.drain_pending_user_requests(env);
    }

    /// `UserManager::remove_contacts` (UserManager.cpp:8330-8353) — the
    /// promise rides the family queue onto the `DeleteContactsQuery`
    /// (the §5 restore).
    pub fn remove_contacts(
        &mut self,
        env: &mut UserEnv,
        user_ids: Vec<UserId>,
        mut promise: Promise<()>,
    ) {
        tdutils::log_info!("Delete contacts: {user_ids:?}");
        if !self.are_contacts_loaded {
            self.load_contacts(env, promise);
            return;
        }

        let mut input_users: Vec<telegram_api::InputUser> = Vec::new();
        for user_id in user_ids {
            if self.get_user(user_id).is_some_and(|u| u.is_contact)
                && let Ok(input_user) = self.get_input_user(env, user_id)
            {
                input_users.push(input_user);
            }
        }

        if input_users.is_empty() {
            promise.set_value(());
            return;
        }

        self.pending_delete_contacts_queries.push(promise);
        self.send_user_query(
            UserNetKind::DeleteContacts,
            telegram_api::Function::contacts_deleteContacts(
                telegram_api::contacts_deleteContacts { id: input_users },
            ),
            "contacts.deleteContacts",
        );
    }

    /// `UserManager::remove_contacts_by_phone_number`
    /// (UserManager.cpp:8355-8365) — the `DeleteContactsByPhoneNumber
    /// Query` send; the empty-phones early answer is the C++ handler
    /// `send`'s promise resolution.
    pub fn remove_contacts_by_phone_number(
        &mut self,
        env: &mut UserEnv,
        user_phone_numbers: Vec<String>,
        user_ids: Vec<UserId>,
        mut promise: Promise<()>,
    ) {
        tdutils::log_info!("Delete contacts by phone number: {user_phone_numbers:?}");
        if !self.are_contacts_loaded {
            self.load_contacts(env, promise);
            return;
        }

        if user_phone_numbers.is_empty() {
            return promise.set_value(());
        }

        self.pending_delete_contacts_by_phones_queries
            .push((user_ids, promise));
        self.send_user_query(
            UserNetKind::DeleteContactsByPhones,
            telegram_api::Function::contacts_deleteByPhones(
                telegram_api::contacts_deleteByPhones {
                    phones: user_phone_numbers,
                },
            ),
            "contacts.deleteByPhones",
        );
    }

    /// `UserManager::on_deleted_contacts` (UserManager.cpp:8367-8386).
    pub fn on_deleted_contacts(&mut self, env: &mut UserEnv, deleted_contact_user_ids: &[UserId]) {
        tdutils::log_info!("Contacts deletion has finished for {deleted_contact_user_ids:?}");

        for user_id in deleted_contact_user_ids {
            let is_contact = self
                .users
                .get(&user_id.get())
                .expect("contact user exists")
                .is_contact;
            if !is_contact {
                continue;
            }

            tdutils::log_info!("Drop contact with {user_id:?}");
            self.on_update_user_is_contact_impl(env, *user_id, false, false, false);
            {
                let u = self
                    .users
                    .get_mut(&user_id.get())
                    .expect("contact user exists");
                assert!(u.is_is_contact_changed);
                u.cache_version = 0;
                u.is_repaired = false;
            }
            self.update_user(env, *user_id, false, false);
            assert!(
                !self
                    .users
                    .get(&user_id.get())
                    .expect("contact user exists")
                    .is_contact
            );
            assert!(!self.contacts_hints.has_key(user_id.get()));
        }
    }

    /// `UserManager::get_imported_contact_count` (UserManager.cpp:8388-8404).
    pub fn get_imported_contact_count(&mut self, env: &mut UserEnv, mut promise: Promise<i32>) {
        if !self.are_contacts_loaded || self.saved_contact_count == -1 {
            self.pending_load_contacts_calls
                .push(PendingLoadContactsCall::GetImportedContactCount { promise });
            self.load_contacts(env, Promise::default());
            return;
        }
        self.reload_contacts(env, false);

        promise.set_value(self.saved_contact_count);
    }

    /// `UserManager::load_imported_contacts` (UserManager.cpp:8406-8431).
    pub fn load_imported_contacts(&mut self, env: &mut UserEnv, mut promise: Promise<()>) {
        if env.auth_is_bot {
            self.are_imported_contacts_loaded = true;
        }
        if self.are_imported_contacts_loaded {
            tdutils::log_info!("Imported contacts are already loaded");
            promise.set_value(());
            return;
        }
        self.load_imported_contacts_queries.push(promise);
        if self.load_imported_contacts_queries.len() == 1 {
            if env.global.use_chat_info_database() {
                tdutils::log_info!("Load imported contacts from database");
                let value = env
                    .global
                    .td_db()
                    .common_kv()
                    .get(b"user_imported_contacts");
                self.on_load_imported_contacts_from_database(env, value);
            } else {
                tdutils::log_info!("Have no previously imported contacts");
                self.on_load_imported_contacts_from_database(env, Vec::new());
            }
        } else {
            tdutils::log_info!("Load imported contacts request has already been sent");
        }
    }

    /// `UserManager::on_load_imported_contacts_from_database`
    /// (UserManager.cpp:8433-8471) — the
    /// `load_imported_contact_users_multipromise_` fan-in collapses onto
    /// the synchronous per-user loads.
    fn on_load_imported_contacts_from_database(&mut self, env: &mut UserEnv, value: Vec<u8>) {
        if env.global.close_flag() {
            return;
        }

        assert!(!self.are_imported_contacts_loaded);
        let mut value = value;
        if self.need_clear_imported_contacts {
            self.need_clear_imported_contacts = false;
            value.clear();
        }
        if value.is_empty() {
            assert!(self.all_imported_contacts.is_empty());
        } else if let Ok(contacts) = parse_contacts_log_event(&value) {
            tdutils::log_info!(
                "Successfully loaded {} imported contacts from database",
                contacts.len()
            );
            self.all_imported_contacts = contacts;
        } else {
            tdutils::log_error!("Failed to load all imported contacts from database");
            self.all_imported_contacts.clear();
        }

        // the C++ multipromise leaves the load pending on a user-load
        // failure; the synchronous load is infallible except for a broken
        // row, which reproduces that stall
        let user_ids: Vec<UserId> = self
            .all_imported_contacts
            .iter()
            .map(|contact| contact.get_user_id())
            .filter(|user_id| user_id.is_valid())
            .collect();
        for user_id in user_ids {
            if !self.get_user_force(env, user_id, "load imported contacts") {
                tdutils::log_info!("Failed to load imported contact users from database");
                return;
            }
        }
        self.on_load_imported_contacts_finished(env);
    }

    /// `UserManager::on_load_imported_contacts_finished`
    /// (UserManager.cpp:8473-8486).
    fn on_load_imported_contacts_finished(&mut self, env: &mut UserEnv) {
        tdutils::log_info!(
            "Finished to load {} imported contacts",
            self.all_imported_contacts.len()
        );

        let user_ids: Vec<UserId> = self
            .all_imported_contacts
            .iter()
            .map(|contact| contact.get_user_id())
            .collect();
        for user_id in user_ids {
            // to ensure updateUser
            self.get_user_id_object(env, user_id, "on_load_imported_contacts_finished");
        }

        if self.need_clear_imported_contacts {
            self.need_clear_imported_contacts = false;
            self.all_imported_contacts.clear();
        }
        self.are_imported_contacts_loaded = true;
        set_promises(&mut self.load_imported_contacts_queries);
    }

    /// `UserManager::change_imported_contacts`
    /// (UserManager.cpp:8488-8598) — the dedup machine, the
    /// to-delete/to-add diff and the `are_imported_contacts_changing_`
    /// gate are ported; the removal continuation into
    /// `on_clear_imported_contacts` rides the change-continuation
    /// message slice.
    pub fn change_imported_contacts(
        &mut self,
        env: &mut UserEnv,
        contacts: &mut [Contact],
        random_id: &mut i64,
        mut promise: Promise<()>,
    ) -> (Vec<UserId>, Vec<i32>) {
        if !self.are_contacts_loaded {
            self.load_contacts(env, promise);
            return (Vec::new(), Vec::new());
        }
        if !self.are_imported_contacts_loaded {
            self.load_imported_contacts(env, promise);
            return (Vec::new(), Vec::new());
        }

        tdutils::log_info!(
            "Asked to change imported contacts to a list of {} contacts with random_id = {}",
            contacts.len(),
            *random_id
        );
        if *random_id != 0 {
            // request has already been sent before
            if self.need_clear_imported_contacts {
                self.need_clear_imported_contacts = false;
                self.all_imported_contacts.clear();
                if env.global.use_chat_info_database() {
                    env.global
                        .td_db()
                        .common_kv()
                        .erase(b"user_imported_contacts");
                }
                self.reload_contacts(env, true);
            }

            assert!(self.are_imported_contacts_changing);
            self.are_imported_contacts_changing = false;

            let unimported_contact_invites = std::mem::take(&mut self.unimported_contact_invites);
            let imported_contact_user_ids = std::mem::take(&mut self.imported_contact_user_ids);

            promise.set_value(());
            return (imported_contact_user_ids, unimported_contact_invites);
        }

        if self.are_imported_contacts_changing {
            promise.set_error(Status::error(
                400,
                "ChangeImportedContacts can be called only once at the same time",
            ));
            return (Vec::new(), Vec::new());
        }

        let mut new_contacts_unique_id: Vec<usize> = Vec::with_capacity(contacts.len());
        let mut unique_new_contacts: Vec<Contact> = Vec::with_capacity(contacts.len());
        // the C++ unordered_map<Contact, size_t, ContactHash, ContactEqual>
        // — keyed on (phone number, first name, last name)
        let mut different_new_contacts: HashMap<(String, String, String), (Contact, usize)> =
            HashMap::new();
        let mut different_new_phone_numbers: BTreeSet<String> = BTreeSet::new();
        let mut unique_size = 0usize;
        for contact in contacts.iter().cloned() {
            let key = contact.get_dedup_key();
            let unique_id = match different_new_contacts.entry(key.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.get().1,
                std::collections::hash_map::Entry::Vacant(entry) => {
                    unique_new_contacts.push(contact.clone());
                    different_new_phone_numbers.insert(contact.get_phone_number().to_owned());
                    entry.insert((contact, unique_size));
                    unique_size += 1;
                    unique_size - 1
                }
            };
            new_contacts_unique_id.push(unique_id);
        }

        let mut to_delete: Vec<String> = Vec::new();
        let mut to_delete_user_ids: Vec<UserId> = Vec::new();
        for old_contact in &self.all_imported_contacts {
            let user_id = old_contact.get_user_id();
            let key = old_contact.get_dedup_key();
            if !different_new_contacts.contains_key(&key) {
                let phone_number = old_contact.get_phone_number().to_owned();
                if !different_new_phone_numbers.contains(&phone_number) {
                    to_delete.push(phone_number);
                    if user_id.is_valid() {
                        to_delete_user_ids.push(user_id);
                    }
                }
            } else {
                let (_, unique_id) = different_new_contacts.remove(&key).expect("checked above");
                unique_new_contacts[unique_id].set_user_id(user_id);
            }
        }
        let to_add: (Vec<usize>, Vec<Contact>) = different_new_contacts
            .into_values()
            .map(|(contact, unique_id)| (unique_id, contact))
            .unzip();

        if to_add.0.is_empty() && to_delete.is_empty() {
            for i in 0..contacts.len() {
                let unique_id = new_contacts_unique_id[i];
                contacts[i].set_user_id(unique_new_contacts[unique_id].get_user_id());
            }

            promise.set_value(());
            return (
                contacts
                    .iter()
                    .map(|contact| contact.get_user_id())
                    .collect(),
                vec![0; contacts.len()],
            );
        }

        self.are_imported_contacts_changing = true;
        *random_id = 1;

        self.remove_contacts_by_phone_number(
            env,
            to_delete,
            to_delete_user_ids,
            Promise::default(),
        );
        // the C++ chains the removal query's promise into
        // on_clear_imported_contacts → do_import_contacts(1)
        // (cpp:8573-8597); the chain's actor hop needs an in-flight marker
        // the drain can see (the parked request would otherwise answer
        // between the removal answer and the chain arrival) — it rides
        // the change-continuation message slice, and the continuation
        // stays driven by the callers (the tests) until then
        (Vec::new(), Vec::new())
    }

    /// `UserManager::clear_imported_contacts` (UserManager.cpp:8600-8609)
    /// — the `ResetContactsQuery` send with the promise on the family
    /// queue.
    pub fn clear_imported_contacts(&mut self, _env: &mut UserEnv, mut promise: Promise<()>) {
        tdutils::log_info!("Delete imported contacts");

        if self.saved_contact_count == 0 {
            promise.set_value(());
            return;
        }

        self.pending_reset_contacts_queries.push(promise);
        self.send_user_query(
            UserNetKind::ResetContacts,
            telegram_api::Function::contacts_resetSaved(telegram_api::contacts_resetSaved),
            "contacts.resetSaved",
        );
    }

    /// `UserManager::on_clear_imported_contacts`
    /// (UserManager.cpp:8611-8620).
    pub fn on_clear_imported_contacts(
        &mut self,
        env: &mut UserEnv,
        contacts: Vec<Contact>,
        contacts_unique_id: Vec<usize>,
        to_add: (Vec<usize>, Vec<Contact>),
        promise: Promise<()>,
    ) {
        tdutils::log_info!("Add {} contacts", to_add.0.len());
        self.next_all_imported_contacts = contacts;
        self.imported_contacts_unique_id = contacts_unique_id;
        self.imported_contacts_pos = to_add.0;

        self.do_import_contacts(env, to_add.1, 1, promise);
    }

    /// `UserManager::on_update_contacts_reset`
    /// (UserManager.cpp:8622-8668).
    pub fn on_update_contacts_reset(&mut self, env: &mut UserEnv) {
        self.saved_contact_count = 0;
        if env.global.use_chat_info_database() {
            env.global.binlog_pmc().set("saved_contact_count", "0");
            env.global
                .td_db()
                .common_kv()
                .erase(b"user_imported_contacts");
        }
        if !self.are_imported_contacts_loaded {
            if self.load_imported_contacts_queries.is_empty() {
                assert!(self.all_imported_contacts.is_empty());
                tdutils::log_info!("Imported contacts were never loaded, just clear them");
            } else {
                tdutils::log_info!(
                    "Imported contacts are being loaded, clear them after they will be loaded"
                );
                self.need_clear_imported_contacts = true;
            }
        } else if !self.are_imported_contacts_changing {
            tdutils::log_info!(
                "Imported contacts were loaded, but aren't changing now, just clear them"
            );
            self.all_imported_contacts.clear();
        } else {
            tdutils::log_info!(
                "Imported contacts are changing now, clear them after they will be changed"
            );
            self.need_clear_imported_contacts = true;
        }
        self.reload_contacts(env, true);
    }

    /// `UserManager::update_contacts_hints` (UserManager.cpp:8670-8701).
    pub fn update_contacts_hints(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        from_database: bool,
    ) {
        let is_contact = self.is_user_contact_of(user_id, false);
        if env.auth_is_bot {
            if is_contact {
                tdutils::log_error!("Bot has {user_id:?} as a contact");
            }
            return;
        }

        let key = user_id.get();
        let old_value = self.contacts_hints.key_to_string(key);
        let new_value = if is_contact {
            self.get_user_search_text_of(user_id)
        } else {
            String::new()
        };

        if new_value != old_value {
            if is_contact {
                self.contacts_hints.add(key, &new_value);
            } else {
                self.contacts_hints.remove(key);
            }
        }

        if env.global.use_chat_info_database() {
            // update contacts database
            if !self.are_contacts_loaded {
                if !from_database
                    && self.load_contacts_queries.is_empty()
                    && is_contact
                    && self
                        .users
                        .get(&key)
                        .is_some_and(|u| u.is_is_contact_changed)
                {
                    self.search_contacts(env, "", i32::MAX, Promise::default());
                }
            } else if old_value.is_empty() == is_contact {
                self.save_contacts_to_database(env);
            }
        }
    }

    /// `UserManager::search_contacts` (UserManager.cpp:8703-8733).
    pub fn search_contacts(
        &mut self,
        env: &mut UserEnv,
        query: &str,
        limit: i32,
        mut promise: Promise<()>,
    ) -> (i32, Vec<UserId>) {
        tdutils::log_info!("Search contacts with query = \"{query}\" and limit = {limit}");

        if limit < 0 {
            promise.set_error(Status::error(400, "Limit must be non-negative"));
            return (0, Vec::new());
        }

        if !self.are_contacts_loaded {
            self.load_contacts(env, promise);
            return (0, Vec::new());
        }
        self.reload_contacts(env, false);

        let result = if query.is_empty() {
            self.contacts_hints.search_empty(limit)
        } else {
            self.contacts_hints.search(query, limit, false)
        };

        let user_ids: Vec<UserId> = result.1.iter().map(|&key| UserId::new(key)).collect();

        promise.set_value(());
        (result.0 as i32, user_ids)
    }

    /// `UserManager::reload_contact_birthdates`
    /// (UserManager.cpp:8735-8749) — the `GetContactsBirthdaysQuery`
    /// send over the `net` handle (fire-and-forget; the error band runs
    /// the `None` intake that re-arms the sync timer).
    pub fn reload_contact_birthdates(&mut self, env: &mut UserEnv, force: bool) {
        if env
            .options
            .get_option_boolean("dismiss_birthday_contact_today", false)
        {
            self.contact_birthdates.need_drop = true;
            if !self.contact_birthdates.is_being_synced {
                self.contact_birthdates.is_being_synced = true;
                self.on_get_contact_birthdates(env, None);
            }
            return;
        }
        if !env.global.close_flag()
            && !env.auth_is_bot
            && !self.contact_birthdates.is_being_synced
            && (self.contact_birthdates.next_sync_time < tdutils::time::now() || force)
        {
            self.contact_birthdates.is_being_synced = true;
            self.send_user_query(
                UserNetKind::GetBirthdays,
                telegram_api::Function::contacts_getBirthdays(telegram_api::contacts_getBirthdays),
                "contacts.getBirthdays",
            );
        }
    }

    /// `UserManager::on_get_contact_birthdates`
    /// (UserManager.cpp:8751-8787).
    pub fn on_get_contact_birthdates(
        &mut self,
        env: &mut UserEnv,
        birthdays: Option<telegram_api::contacts_ContactBirthdays>,
    ) {
        assert!(self.contact_birthdates.is_being_synced);
        self.contact_birthdates.is_being_synced = false;
        let mut birthdays = birthdays;
        if self.contact_birthdates.need_drop {
            // the C++ swaps in an empty contacts_contactBirthdays object
            // and falls through — the users list clears and the update
            // fires; a `nullptr` without the drop only resets the timer
            birthdays = Some(telegram_api::contacts_ContactBirthdays {
                contacts: Vec::new(),
                users: Vec::new(),
            });
            self.contact_birthdates.need_drop = false;
        }
        if birthdays.is_none() {
            self.contact_birthdates.next_sync_time =
                tdutils::time::now() + f64::from(random_fast(120, 180));
            return;
        }
        let birthdays = birthdays.expect("checked above");
        self.contact_birthdates.next_sync_time =
            tdutils::time::now() + f64::from(random_fast(86_400 / 4, 86_400 / 3));

        self.on_get_users(env, birthdays.users, "on_get_contact_birthdates");
        let mut users: Vec<(UserId, Birthdate)> = Vec::new();
        for contact in &birthdays.contacts {
            let user_id = UserId::new(contact.contact_id);
            if self.is_user_contact_of(user_id, false) {
                let birthdate = contact
                    .birthday
                    .as_ref()
                    .map(|birthday| Birthdate::new(birthday.day, birthday.month, birthday.year))
                    .unwrap_or_default();
                let is_changed = {
                    let mut is_changed = false;
                    if self.get_user_full_force(env, user_id, "on_get_contact_birthdates") {
                        let user_full = self
                            .users_full
                            .get_mut(&user_id.get())
                            .expect("force-loaded above");
                        if user_full.birthdate != birthdate {
                            user_full.birthdate = birthdate;
                            user_full.is_changed = true;
                            is_changed = true;
                        }
                    }
                    is_changed
                };
                if is_changed {
                    self.update_user_full(env, user_id, "on_get_contact_birthdates", false);
                }
                if !birthdate.is_empty() {
                    users.push((user_id, birthdate));
                }
            }
        }
        if self.contact_birthdates.users != users {
            self.contact_birthdates.users = users;
            let update = self.get_update_contact_close_birthdays(env);
            self.send_update(update);
        }
        // there is no need to save them between restarts
    }

    /// `UserManager::get_update_contact_close_birthdays`
    /// (UserManager.cpp:10465-10472).
    fn get_update_contact_close_birthdays(&mut self, env: &mut UserEnv) -> td_api::Update {
        let users = self.contact_birthdates.users.clone();
        let close_birthday_users = users
            .iter()
            .map(|(user_id, birthdate)| td_api::CloseBirthdayUser {
                user_id: self.get_user_id_object(env, *user_id, "closeBirthdayUser"),
                birthdate: Some(
                    birthdate
                        .get_birthdate_object()
                        .expect("contact_birthdates.users_ holds only non-empty birthdates"),
                ),
            })
            .collect();
        td_api::Update::updateContactCloseBirthdays(td_api::updateContactCloseBirthdays {
            close_birthday_users,
        })
    }

    /// `UserManager::hide_contact_birthdays` (UserManager.cpp:8789-8791)
    /// — the `DismissContactBirthdaysSuggestionQuery` send (the
    /// `help.dismissSuggestion` form with `inputPeerEmpty` and the
    /// `BIRTHDAY_CONTACTS_TODAY` action); the promise rides the family
    /// queue (the §5 restore).
    pub fn hide_contact_birthdays(&mut self, promise: Promise<()>) {
        self.pending_dismiss_birthdays_queries.push(promise);
        self.send_user_query(
            UserNetKind::DismissBirthdays,
            telegram_api::Function::help_dismissSuggestion(telegram_api::help_dismissSuggestion {
                peer: Some(Box::new(telegram_api::InputPeer::inputPeerEmpty(
                    telegram_api::inputPeerEmpty,
                ))),
                suggestion: "BIRTHDAY_CONTACTS_TODAY".to_owned(),
            }),
            "help.dismissSuggestion",
        );
    }

    /// `UserManager::get_close_friends` (UserManager.cpp:8793-8813).
    pub fn get_close_friends(
        &mut self,
        env: &mut UserEnv,
        mut promise: Promise<()>,
    ) -> Vec<UserId> {
        if !self.are_contacts_loaded {
            self.load_contacts(env, promise);
            return Vec::new();
        }
        self.reload_contacts(env, false);

        let result = self.contacts_hints.search_empty(10_000);

        let mut user_ids = Vec::new();
        for key in result.1 {
            let user_id = UserId::new(key);
            if self.get_user(user_id).is_some_and(|u| u.is_close_friend) {
                user_ids.push(user_id);
            }
        }

        promise.set_value(());
        user_ids
    }

    /// `UserManager::set_close_friends` (UserManager.cpp:8815-8823) —
    /// the `EditCloseFriendsQuery` send; the user ids (the
    /// `on_set_close_friends` intake) and the promise ride the family
    /// queue (the §5 restore).
    pub fn set_close_friends(&mut self, user_ids: Vec<UserId>, mut promise: Promise<()>) {
        for user_id in &user_ids {
            if !self.have_min_user(*user_id) {
                return promise.set_error(Status::error(400, "User not found"));
            }
        }

        self.pending_edit_close_friends_queries
            .push((user_ids.clone(), promise));
        let ids = user_ids.iter().map(|user_id| user_id.get()).collect();
        self.send_user_query(
            UserNetKind::EditCloseFriends,
            telegram_api::Function::contacts_editCloseFriends(
                telegram_api::contacts_editCloseFriends { id: ids },
            ),
            "contacts.editCloseFriends",
        );
    }

    /// `UserManager::on_set_close_friends` (UserManager.cpp:8825-8839).
    pub fn on_set_close_friends(
        &mut self,
        env: &mut UserEnv,
        user_ids: Vec<UserId>,
        mut promise: Promise<()>,
    ) {
        let close_friend_user_ids: BTreeSet<i64> = user_ids
            .iter()
            .map(|user_id| {
                assert!(user_id.is_valid());
                user_id.get()
            })
            .collect();
        let known_user_ids: Vec<i64> = self.users.keys().copied().collect();
        for user_id in known_user_ids {
            let (is_contact, is_mutual_contact, is_close_friend) = {
                let u = self.users.get(&user_id).expect("collected above");
                (u.is_contact, u.is_mutual_contact, u.is_close_friend)
            };
            if is_contact && is_close_friend != close_friend_user_ids.contains(&user_id) {
                self.on_update_user_is_contact_impl(
                    env,
                    UserId::new(user_id),
                    is_contact,
                    is_mutual_contact,
                    !is_close_friend,
                );
                self.update_user(env, UserId::new(user_id), false, false);
            }
        }
        promise.set_value(());
    }

    /// `UserManager::search_user_by_phone_number`
    /// (UserManager.cpp:8841-8860) — the miss arm parks the phone and
    /// the promise on the `ResolvePhoneQuery` family queue (the §5
    /// restore) and sends the query.
    pub fn search_user_by_phone_number(
        &mut self,
        phone_number: String,
        only_local: bool,
        mut promise: Promise<()>,
    ) -> UserId {
        let phone_number = clean_phone_number(phone_number);
        if phone_number.is_empty() {
            promise.set_error(Status::error(200, "Phone number is invalid"));
            return UserId::default();
        }

        if let Some(user_id) = self.resolved_phone_numbers.get(&phone_number) {
            promise.set_value(());
            return *user_id;
        }

        if only_local {
            promise.set_value(());
        } else {
            self.send_resolve_phone_query(phone_number, promise);
        }
        UserId::default()
    }

    /// The `ResolvePhoneQuery` send (cpp:278-290) — the handler's
    /// `phone_number_` member and the promise park on the family queue.
    fn send_resolve_phone_query(&mut self, phone_number: String, promise: Promise<()>) {
        self.pending_resolve_phone_queries
            .push((phone_number.clone(), promise));
        self.send_user_query(
            UserNetKind::ResolvePhone,
            telegram_api::Function::contacts_resolvePhone(telegram_api::contacts_resolvePhone {
                phone: phone_number,
            }),
            "contacts.resolvePhone",
        );
    }

    /// The request-facing `search_user_by_phone_number`
    /// (SearchUserByPhoneNumberRequest, Requests.cpp:364-390) — the local
    /// arms answer through `answer_search_phone_request` (the
    /// `do_send_result` form); the miss leg parks the request on the
    /// resolve queue (the RequestActor do_run/do_send_result re-run
    /// collapse: the resolve answer caches the phone, the drain answers
    /// from the cache).
    pub(crate) fn search_user_by_phone_number_for_request(
        &mut self,
        env: &UserEnv,
        phone_number: String,
        only_local: bool,
        mut promise: Promise<Object>,
    ) {
        let phone_number = clean_phone_number(phone_number);
        if phone_number.is_empty() {
            return promise.set_error(Status::error(200, "Phone number is invalid"));
        }

        if let Some(user_id) = self.resolved_phone_numbers.get(&phone_number) {
            let user_id = *user_id;
            return self.answer_search_phone_request(env, user_id, promise);
        }

        if only_local {
            return self.answer_search_phone_request(env, UserId::default(), promise);
        }

        self.pending_search_phone_requests
            .push((phone_number.clone(), promise));
        self.send_resolve_phone_query(phone_number, Promise::default());
    }

    /// The `SearchUserByPhoneNumberRequest::do_send_result` half — the
    /// user object of the resolution, or the `send_result(nullptr)` 404
    /// when the user is unknown (the negative-cache default included).
    fn answer_search_phone_request(
        &self,
        env: &UserEnv,
        user_id: UserId,
        mut promise: Promise<Object>,
    ) {
        match self.get_user(user_id) {
            Some(_) => promise.set_value(Object::user(self.get_user_object_of(env, user_id))),
            None => promise.set_value(Object::error(td_api::Error {
                code: 404,
                message: "Not Found".to_owned(),
            })),
        }
    }

    /// Drain the parked `searchUserByPhoneNumber` requests whose phone a
    /// resolve answer just cached (the RequestActor do_run/do_send_result
    /// re-run); the rest stay parked for their own answers — the drain
    /// is phone-exact on the ok path.
    pub(crate) fn drain_search_phone_requests(&mut self, env: &UserEnv) {
        let mut index = 0;
        while index < self.pending_search_phone_requests.len() {
            let phone = self.pending_search_phone_requests[index].0.clone();
            if let Some(user_id) = self.resolved_phone_numbers.get(&phone).copied() {
                let (_, promise) = self.pending_search_phone_requests.remove(index);
                self.answer_search_phone_request(env, user_id, promise);
            } else {
                index += 1;
            }
        }
    }

    /// Fail the parked `searchUserByPhoneNumber` requests of one phone —
    /// the resolve error band (the FIFO-popped pair's phone, the §5
    /// pairing).
    pub(crate) fn fail_search_phone_requests(&mut self, phone_number: &str, status: &Status) {
        self.pending_search_phone_requests =
            std::mem::take(&mut self.pending_search_phone_requests)
                .into_iter()
                .filter_map(|(phone, mut promise)| {
                    if phone == phone_number {
                        promise.set_error(status.clone());
                        None
                    } else {
                        Some((phone, promise))
                    }
                })
                .collect();
    }

    /// The request-facing `search_user_by_phone_number`
    /// (SearchUserByPhoneNumberRequest, Requests.cpp:364-390) —
    /// `UserManager::on_resolved_phone_number` (UserManager.cpp:8862-8888).
    pub fn on_resolved_phone_number(&mut self, phone_number: String, user_id: UserId) {
        if !user_id.is_valid() {
            // the C++ `emplace` keeps a previously resolved phone — the
            // negative cache never downgrades it (cpp:8865-8867)
            self.resolved_phone_numbers.entry(phone_number).or_default(); // negative cache
            return;
        }

        if let Some(known_user_id) = self.resolved_phone_numbers.get(&phone_number) {
            if *known_user_id != user_id {
                tdutils::log_warning!(
                    "Resolve phone number \"{phone_number}\" to {user_id:?}, but have it in {known_user_id:?}"
                );
                self.resolved_phone_numbers.insert(phone_number, user_id);
            }
            return;
        }

        match self.get_user(user_id) {
            None => tdutils::log_error!(
                "Resolve phone number \"{phone_number}\" to unknown {user_id:?}"
            ),
            Some(u) if !u.phone_number.is_empty() => tdutils::log_error!(
                "Resolve phone number \"{phone_number}\" to {user_id:?} with phone number {}",
                u.phone_number
            ),
            // the user's phone number can be hidden by privacy settings,
            // despite the user can be found by the phone number
            Some(_) => {}
        }
        self.resolved_phone_numbers.insert(phone_number, user_id); // always update cached value
    }
}

/// `EmojiStatus::get_emoji_status_object` — the reduced payload; the
/// collectible upgraded-gift form lands with the Gifts step (the C++
/// emits `emojiStatusUpgradedGift` there).
pub(crate) fn make_emoji_status_object(status: &EmojiStatus) -> td_api::EmojiStatus {
    td_api::EmojiStatus {
        r#type: Some(td_api::EmojiStatusType::emojiStatusTypeCustomEmoji(
            td_api::emojiStatusTypeCustomEmoji {
                custom_emoji_id: status.get_custom_emoji_id().get(),
            },
        )),
        expiration_date: status.get_until_date(),
    }
}

/// `UserManager::get_update_unknown_secret_chat_object`
/// (UserManager.cpp:10496-10500).
fn make_unknown_secret_chat_object(secret_chat_id: SecretChatId) -> td_api::SecretChat {
    td_api::SecretChat {
        id: secret_chat_id.get(),
        user_id: 0,
        state: Some(td_api::SecretChatState::secretChatStateClosed(
            td_api::secretChatStateClosed,
        )),
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
        state: Some(get_secret_chat_state_object(secret_chat.state)),
        is_outbound: secret_chat.is_outbound,
        key_hash: secret_chat.key_hash.clone().into_bytes(),
        layer: secret_chat.layer,
    }
}

/// `UserManager::get_secret_chat_state_object` (UserManager.cpp:10284-10296).
fn get_secret_chat_state_object(state: SecretChatState) -> td_api::SecretChatState {
    match state {
        SecretChatState::Waiting => {
            td_api::SecretChatState::secretChatStatePending(td_api::secretChatStatePending)
        }
        SecretChatState::Active => {
            td_api::SecretChatState::secretChatStateReady(td_api::secretChatStateReady)
        }
        SecretChatState::Closed | SecretChatState::Unknown => {
            td_api::SecretChatState::secretChatStateClosed(td_api::secretChatStateClosed)
        }
    }
}

/// `UserManager::get_update_unknown_user_object`'s payload
/// (UserManager.cpp:10269-10276) — the C++ passes nullptr for the
/// usernames/photo/collectible/emoji-status/verification/restriction/
/// story-state object fields and the JSON plane omits them.
fn make_unknown_user_object(user_id: UserId, my_id: UserId) -> td_api::User {
    let have_access = user_id == my_id;
    td_api::User {
        id: user_id.get(),
        first_name: String::new(),
        last_name: String::new(),
        usernames: None,
        phone_number: String::new(),
        status: Some(td_api::UserStatus::userStatusEmpty(td_api::userStatusEmpty)),
        profile_photo: None,
        accent_color_id: AccentColorId::from_peer_id(user_id.get()).get(),
        background_custom_emoji_id: 0,
        upgraded_gift_colors: None,
        profile_accent_color_id: -1,
        profile_background_custom_emoji_id: 0,
        emoji_status: None,
        is_contact: false,
        is_mutual_contact: false,
        is_close_friend: false,
        verification_status: None,
        is_premium: false,
        is_support: false,
        restriction_info: None,
        active_story_state: None,
        restricts_new_chats: false,
        paid_message_star_count: 0,
        have_access,
        r#type: Some(td_api::UserType::userTypeUnknown(td_api::userTypeUnknown)),
        language_code: String::new(),
        added_to_attachment_menu: false,
    }
}

// ------------------------------------------------------------------
// Contact policy + registries (UserManager.cpp:7754-7909 + the policy
// intakes) — M.4.15. The message-users registry needs `MessageFullId`
// (crate::message_id, the C++ DialogId/MessageId/MessageFullId subset);
// the premium-required-to-contact QueryMerger actor is created by the Td
// init and stored in `get_is_premium_required_to_contact_queries`.

impl UserManager {
    /// `UserManager::register_message_users` (UserManager.cpp:7754-7769).
    /// The `ChatManager::have_channel` gate is the `known_channels`
    /// stand-in — empty (no channel admitted) until the ChatManager slice
    /// feeds it. Users without a usable access hash get an `updateUser`
    /// when their registry entry appears.
    pub fn register_message_users(
        &mut self,
        env: &UserEnv,
        message_full_id: MessageFullId,
        user_ids: &[UserId],
    ) {
        let dialog_id = message_full_id.get_dialog_id();
        assert!(
            dialog_id.get_type() == DialogType::Channel,
            "register_message_users for a non-channel dialog"
        );
        if !self
            .known_channels
            .contains(&dialog_id.get_channel_id().get())
        {
            return;
        }
        for user_id in user_ids {
            assert!(user_id.is_valid());
            let has_access_hash = self
                .get_user(*user_id)
                .is_some_and(|u| u.access_hash != -1 && !u.is_min_access_hash);
            if !has_access_hash {
                let user_messages = self.user_messages.entry(user_id.get()).or_default();
                let need_update = user_messages.is_empty();
                user_messages.insert(message_full_id);
                if need_update {
                    let user_object = self.get_user_object_of(env, *user_id);
                    self.send_update(td_api::Update::updateUser(td_api::updateUser {
                        user: Some(user_object),
                    }));
                }
            }
        }
    }

    /// `UserManager::unregister_message_users` (UserManager.cpp:7771-7791) —
    /// the fast path and the last-reference `updateUser` re-emission.
    pub fn unregister_message_users(
        &mut self,
        env: &UserEnv,
        message_full_id: MessageFullId,
        user_ids: &[UserId],
    ) {
        if self.user_messages.is_empty() {
            // fast path
            return;
        }
        for user_id in user_ids {
            let entry_empty = match self.user_messages.get_mut(&user_id.get()) {
                Some(user_messages) => {
                    user_messages.remove(&message_full_id);
                    user_messages.is_empty()
                }
                None => continue,
            };
            if !entry_empty {
                continue;
            }
            self.user_messages.remove(&user_id.get());
            let has_access_hash = self
                .get_user(*user_id)
                .is_some_and(|u| u.access_hash != -1 && !u.is_min_access_hash);
            if !has_access_hash {
                let user_object = self.get_user_object_of(env, *user_id);
                self.send_update(td_api::Update::updateUser(td_api::updateUser {
                    user: Some(user_object),
                }));
            }
        }
    }

    /// `UserManager::can_send_message_to_user` (UserManager.cpp:7795-7852).
    /// The uncached, non-force path parks in the premium-required-to-contact
    /// QueryMerger; its merged query re-dispatches this call with
    /// `force = true` through the Td actor. The C++ `u->paid_message_star_
    /// count` dereference in the premium disjunct is UB when `u == nullptr`;
    /// the Rust port treats a missing user as `false` there (the earlier
    /// `have_input_peer_user` check already answered the deleted/null forms).
    pub fn can_send_message_to_user(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        force: bool,
        mut promise: Promise<td_api::CanSendMessageToUserResult>,
    ) {
        if env.global.close_flag() {
            // the C++ `G()->close_status()`; the codebase's close answer
            // (client.rs) is 500 "Request aborted"
            return promise.set_error(Status::error(500, "Request aborted"));
        }
        if user_id == self.my_id {
            return promise.set_value(
                td_api::CanSendMessageToUserResult::canSendMessageToUserResultOk(
                    td_api::canSendMessageToUserResultOk,
                ),
            );
        }
        let u = self.get_user(user_id);
        if !self.have_input_peer_user_for(u, user_id, AccessRights::Write, env.auth_is_bot) {
            return promise.set_value(
                td_api::CanSendMessageToUserResult::canSendMessageToUserResultUserIsDeleted(
                    td_api::canSendMessageToUserResultUserIsDeleted,
                ),
            );
        }
        assert!(user_id.is_valid());
        let u_accepts = u.is_some_and(|u| {
            (!u.contact_require_premium && u.paid_message_star_count == 0) || u.is_mutual_contact
        });
        let premium_accepts = u.is_some_and(|u| {
            env.options.get_option_boolean("is_premium", false) && u.paid_message_star_count == 0
        });
        if u_accepts || premium_accepts {
            return promise.set_value(
                td_api::CanSendMessageToUserResult::canSendMessageToUserResultOk(
                    td_api::canSendMessageToUserResultOk,
                ),
            );
        }
        if self.get_user_full_force(env, user_id, "can_send_message_to_user") {
            let user_full = self.users_full.get(&user_id.get()).unwrap();
            if user_full.send_paid_message_stars > 0 {
                return promise.set_value(
                td_api::CanSendMessageToUserResult::canSendMessageToUserResultUserHasPaidMessages(
                    td_api::canSendMessageToUserResultUserHasPaidMessages {
                        outgoing_paid_message_star_count: user_full.send_paid_message_stars,
                    },
                ),
            );
            }
            if user_full.contact_require_premium {
                return promise.set_value(
                td_api::CanSendMessageToUserResult::canSendMessageToUserResultUserRestrictsNewChats(
                    td_api::canSendMessageToUserResultUserRestrictsNewChats,
                ),
            );
            }
            return promise.set_value(
                td_api::CanSendMessageToUserResult::canSendMessageToUserResultOk(
                    td_api::canSendMessageToUserResultOk,
                ),
            );
        }
        match self.user_full_contact_price.get(&user_id.get()).copied() {
            Some(price) if price > 0 => {
                return promise.set_value(
                td_api::CanSendMessageToUserResult::canSendMessageToUserResultUserHasPaidMessages(
                    td_api::canSendMessageToUserResultUserHasPaidMessages {
                        outgoing_paid_message_star_count: price,
                    },
                ),
            );
            }
            Some(-1) => {
                return promise.set_value(
                td_api::CanSendMessageToUserResult::canSendMessageToUserResultUserRestrictsNewChats(
                    td_api::canSendMessageToUserResultUserRestrictsNewChats,
                ),
            );
            }
            Some(_) => {
                return promise.set_value(
                    td_api::CanSendMessageToUserResult::canSendMessageToUserResultOk(
                        td_api::canSendMessageToUserResultOk,
                    ),
                );
            }
            None => {}
        }
        if force {
            return promise.set_value(
                td_api::CanSendMessageToUserResult::canSendMessageToUserResultOk(
                    td_api::canSendMessageToUserResultOk,
                ),
            );
        }

        let Some(merger) = self.get_is_premium_required_to_contact_queries.clone() else {
            tdutils::log_error!("Premium-required-to-contact merger not wired");
            return promise.set_error(Status::error(500, "Request aborted"));
        };
        let td_actor_id = self
            .td_actor_id
            .clone()
            .expect("the Td actor id is wired by the Td init");
        let query_promise = PromiseCreator::lambda_result(tdactor::promise_send_closure(
            td_actor_id,
            move |td: &mut crate::td::Td, result: tdutils::status::Result<()>| {
                match result {
                    tdutils::status::Result::Ok(()) => {
                        // the merged query resolved — re-run with force
                        let Some(options) = td.option_manager.as_mut() else {
                            return promise.set_error(Status::error(500, "Request aborted"));
                        };
                        let mut env = UserEnv {
                            global: &mut td.global,
                            options,
                            auth_is_bot: td.auth_manager.is_bot(),
                        };
                        td.user_manager
                            .can_send_message_to_user(&mut env, user_id, true, promise);
                    }
                    tdutils::status::Result::Err(error) => {
                        promise.set_error(error);
                    }
                }
            },
        ));
        tdactor::send_closure(
            merger,
            crate::query_merger::QueryMergerMessage::AddQuery {
                query_id: user_id.get(),
                promise: query_promise,
            },
        );
    }

    /// `GetIsPremiumRequiredToContactQuery::send` (cpp:1783-1786, reached
    /// through the merger's merge function cpp:2803-2811): park the
    /// handler's `user_ids_`/`promise_` pair and dispatch the merged
    /// `users.getRequirementsToContact` query.
    pub(crate) fn send_get_requirements_to_contact_query(
        &mut self,
        user_ids: Vec<UserId>,
        input_users: Vec<telegram_api::InputUser>,
        promise: Promise<()>,
    ) {
        self.pending_get_requirements_to_contact_queries
            .push((user_ids, promise));
        match self.net.clone() {
            Some(net) => net.dispatch(
                UserNetKind::GetRequirementsToContact,
                telegram_api::Function::users_getRequirementsToContact(
                    telegram_api::users_getRequirementsToContact { id: input_users },
                ),
            ),
            None => tdutils::log_info!("Send users.getRequirementsToContact — [net-inert]"),
        }
    }

    /// `GetIsPremiumRequiredToContactQuery::on_result`'s in-flight pop
    /// (cpp:1788-1798) — the head `user_ids_`/`promise_` pair.
    pub(crate) fn take_get_requirements_to_contact_query(
        &mut self,
    ) -> Option<(Vec<UserId>, Promise<()>)> {
        if self.pending_get_requirements_to_contact_queries.is_empty() {
            None
        } else {
            Some(self.pending_get_requirements_to_contact_queries.remove(0))
        }
    }

    /// Pop the head `UpdateContactNoteQuery` state (the handler's
    /// promise lambda members, cpp:844-857) — the ok answer runs
    /// `on_set_user_note` with them.
    pub(crate) fn take_update_contact_note_query(
        &mut self,
    ) -> Option<(UserId, crate::message_entity::FormattedText, Promise<()>)> {
        if self.pending_update_contact_note_queries.is_empty() {
            None
        } else {
            Some(self.pending_update_contact_note_queries.remove(0))
        }
    }

    /// Pop the head `SetMainProfileTabQuery` state (cpp:1304-1318) —
    /// the Bool answer runs `on_set_main_profile_tab` with them.
    pub(crate) fn take_set_main_profile_tab_query(&mut self) -> Option<(ProfileTab, Promise<()>)> {
        if self.pending_set_main_profile_tab_queries.is_empty() {
            None
        } else {
            Some(self.pending_set_main_profile_tab_queries.remove(0))
        }
    }

    /// Pop the head `SuggestUserBirthdayQuery` promise (cpp:870-883) —
    /// the Updates answer's `on_get_updates` call consumes it.
    pub(crate) fn take_suggest_birthday_query(&mut self) -> Option<Promise<()>> {
        if self.pending_suggest_birthday_queries.is_empty() {
            None
        } else {
            Some(self.pending_suggest_birthday_queries.remove(0))
        }
    }

    /// Pop the head `UpdateEmojiStatusQuery` state (cpp:1380-1397) —
    /// the cloned status and the promise.
    pub(crate) fn take_update_emoji_status_query(
        &mut self,
    ) -> Option<(Option<EmojiStatus>, Promise<()>)> {
        if self.pending_update_emoji_status_queries.is_empty() {
            None
        } else {
            Some(self.pending_update_emoji_status_queries.remove(0))
        }
    }

    /// Pop the head `UpdateColorQuery` promise (UserManager.cpp:926-931)
    /// — the Bool answer runs `on_update_accent_color_success` then
    /// flushes it.
    pub(crate) fn take_update_color_query(&mut self) -> Option<Promise<()>> {
        if self.pending_update_color_queries.is_empty() {
            None
        } else {
            Some(self.pending_update_color_queries.remove(0))
        }
    }

    /// The test read of `saved_contact_count_` (the reset band assert).
    #[cfg(test)]
    pub(crate) fn saved_contact_count_for_test(&self) -> i32 {
        self.saved_contact_count
    }

    /// Pop the head `AddContactQuery` state (cpp:209-240) — the handler's
    /// `user_id_` member and the query-carried promise (the §5 restore).
    pub(crate) fn take_add_contact_query(&mut self) -> Option<(UserId, Promise<()>)> {
        if self.pending_add_contact_queries.is_empty() {
            None
        } else {
            Some(self.pending_add_contact_queries.remove(0))
        }
    }

    /// Pop the head `DeleteContactsQuery` promise (cpp:383-406, the §5
    /// restore).
    pub(crate) fn take_delete_contacts_query(&mut self) -> Option<Promise<()>> {
        if self.pending_delete_contacts_queries.is_empty() {
            None
        } else {
            Some(self.pending_delete_contacts_queries.remove(0))
        }
    }

    /// Pop the head `DeleteContactsByPhoneNumberQuery` state (cpp:411-432)
    /// — the handler's `user_ids_` member and the promise.
    pub(crate) fn take_delete_contacts_by_phones_query(
        &mut self,
    ) -> Option<(Vec<UserId>, Promise<()>)> {
        if self.pending_delete_contacts_by_phones_queries.is_empty() {
            None
        } else {
            Some(self.pending_delete_contacts_by_phones_queries.remove(0))
        }
    }

    /// Pop the head `ResetContactsQuery` promise (cpp:448-472).
    pub(crate) fn take_reset_contacts_query(&mut self) -> Option<Promise<()>> {
        if self.pending_reset_contacts_queries.is_empty() {
            None
        } else {
            Some(self.pending_reset_contacts_queries.remove(0))
        }
    }

    /// Pop the head `DismissContactBirthdaysSuggestionQuery` promise
    /// (cpp:161-180).
    pub(crate) fn take_dismiss_birthdays_query(&mut self) -> Option<Promise<()>> {
        if self.pending_dismiss_birthdays_queries.is_empty() {
            None
        } else {
            Some(self.pending_dismiss_birthdays_queries.remove(0))
        }
    }

    /// Pop the head `EditCloseFriendsQuery` state (cpp:250-266) — the
    /// handler's `user_ids_` member and the promise (the §5 restore).
    pub(crate) fn take_edit_close_friends_query(&mut self) -> Option<(Vec<UserId>, Promise<()>)> {
        if self.pending_edit_close_friends_queries.is_empty() {
            None
        } else {
            Some(self.pending_edit_close_friends_queries.remove(0))
        }
    }

    /// Pop the head `ResolvePhoneQuery` state (cpp:278-316) — the
    /// handler's `phone_number_` member and the promise (the §5
    /// restore).
    pub(crate) fn take_resolve_phone_query(&mut self) -> Option<(String, Promise<()>)> {
        if self.pending_resolve_phone_queries.is_empty() {
            None
        } else {
            Some(self.pending_resolve_phone_queries.remove(0))
        }
    }

    /// Pop the head `AcceptContactQuery` state (cpp:322-351) — the
    /// handler's `user_id_` member and the promise (the §5 restore).
    pub(crate) fn take_accept_contact_query(&mut self) -> Option<(UserId, Promise<()>)> {
        if self.pending_accept_contact_queries.is_empty() {
            None
        } else {
            Some(self.pending_accept_contact_queries.remove(0))
        }
    }

    /// `UserManager::on_get_is_premium_required_to_contact_users`
    /// (UserManager.cpp:7854-7882) — the merged-query answer: cache the
    /// price per user that has no `UserFull` yet.
    pub fn on_get_is_premium_required_to_contact_users(
        &mut self,
        user_ids: Vec<UserId>,
        requirements: Vec<telegram_api::RequirementToContact>,
        mut promise: Promise<()>,
    ) {
        if user_ids.len() != requirements.len() {
            tdutils::log_error!(
                "Receive {} flags instead of {}",
                requirements.len(),
                user_ids.len()
            );
            return promise.set_error(Status::error(500, "Receive invalid response"));
        }
        for (user_id, requirement) in user_ids.iter().zip(requirements.iter()) {
            assert!(user_id.is_valid());
            if self.get_user_full(*user_id).is_none() {
                let price = match requirement {
                    telegram_api::RequirementToContact::requirementToContactEmpty(_) => 0,
                    telegram_api::RequirementToContact::requirementToContactPremium(_) => -1,
                    telegram_api::RequirementToContact::requirementToContactPaidMessages(r) => {
                        get_star_count(r.stars_amount)
                    }
                };
                self.user_full_contact_price.insert(user_id.get(), price);
            }
        }
        promise.set_value(());
    }

    /// `UserManager::allow_send_message_to_user` (UserManager.cpp:7884-7892).
    pub fn allow_send_message_to_user(&mut self, env: &UserEnv, user_id: UserId) {
        assert!(!env.auth_is_bot);
        if self.get_user_full(user_id).is_none() {
            assert!(user_id.is_valid());
            if let Some(price) = self.user_full_contact_price.get_mut(&user_id.get())
                && *price == -1
            {
                *price = 0;
            }
        }
    }

    /// `UserManager::share_phone_number` (UserManager.cpp:7896-7909). The
    /// load-gated re-dispatch rides the parked-call queue (the C++ actor
    /// message); the `AcceptContactQuery` send rides the `net` handle
    /// with the promise on the family queue (the §5 restore); the
    /// `hide_dialog_action_bar` fan-out is MessagesManager's.
    pub fn share_phone_number(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        mut promise: Promise<()>,
    ) {
        if env.global.close_flag() {
            // the C++ TRY_STATUS_PROMISE(promise, G()->close_status())
            return promise.set_error(Status::error(500, "Request aborted"));
        }
        if !self.are_contacts_loaded {
            self.pending_load_contacts_calls
                .push(PendingLoadContactsCall::SharePhoneNumber { user_id, promise });
            self.load_contacts(env, Promise::default());
            return;
        }
        tdutils::log_info!("Share phone number with {user_id:?}");
        let input_user = match self.get_input_user(env, user_id) {
            Ok(input_user) => input_user,
            Err(error) => return promise.set_error(error),
        };
        // MessagesManager::hide_dialog_action_bar — with its step
        tdutils::log_info!("Hide dialog action bar — the MessagesManager step");
        self.pending_accept_contact_queries.push((user_id, promise));
        self.send_user_query(
            UserNetKind::AcceptContact,
            telegram_api::Function::contacts_acceptContact(telegram_api::contacts_acceptContact {
                id: Some(input_user),
            }),
            "contacts.acceptContact",
        );
    }

    /// `UserManager::register_noforwards_request` (UserManager.cpp:9438-9450)
    /// — arm the noforwards deadline when the request window is still open
    /// and the message is not scheduled.
    pub fn register_noforwards_request(
        &mut self,
        env: &UserEnv,
        message_full_id: MessageFullId,
        message_date: i32,
    ) {
        let message_id = message_full_id.get_message_id();
        let duration = env
            .options
            .get_option_integer("has_protected_content_disable_request_duration", 86400);
        let left_time = message_date as i64 + duration - env.unix_time() as i64;
        if left_time > 0 && !message_id.is_scheduled() {
            assert_ne!(message_id.get(), 0);
            assert!(
                !self.noforwards_request_ids.contains_key(&message_full_id),
                "a second noforwards request for one message"
            );
            self.current_noforwards_request_id += 1;
            let request_id = self.current_noforwards_request_id;
            self.noforwards_request_ids
                .insert(message_full_id, request_id);
            self.noforwards_request_message_full_ids
                .insert(request_id, message_full_id);
            self.noforwards_request_timeouts
                .insert(request_id, tdutils::time::now() + left_time as f64 + 1.0);
        }
    }

    /// `UserManager::unregister_noforwards_request`
    /// (UserManager.cpp:9452-9462).
    pub fn unregister_noforwards_request(&mut self, message_full_id: MessageFullId) {
        let Some(request_id) = self.noforwards_request_ids.remove(&message_full_id) else {
            return;
        };
        assert_ne!(request_id, 0);
        self.noforwards_request_message_full_ids.remove(&request_id);
        self.noforwards_request_timeouts.remove(&request_id);
    }

    /// `UserManager::on_noforwards_request_timeout`
    /// (UserManager.cpp:2935-2947) — the deadline fire: drop the registration
    /// and let MessagesManager refresh the message (its step).
    fn on_noforwards_request_timeout(&mut self, env: &UserEnv, request_id: i32) {
        if env.global.close_flag() {
            return;
        }
        let Some(message_full_id) = self
            .noforwards_request_message_full_ids
            .get(&request_id)
            .copied()
        else {
            return;
        };
        self.unregister_noforwards_request(message_full_id);
        // MessagesManager::on_external_update_message_content — with its step
    }

    /// `UserManager::get_web_app_placeholder` (UserManager.cpp:9464-9469).
    /// `bot_info` is absent until its subsystem, so the placeholder-path
    /// branch is unreachable and the answer is always the C++ null.
    pub fn get_web_app_placeholder(
        &mut self,
        env: &mut UserEnv,
        user_id: UserId,
        mut promise: Promise<Option<td_api::Outline>>,
    ) {
        if !self.get_user_full_force(env, user_id, "get_web_app_placeholder") {
            return promise.set_value(None);
        }
        // the C++ reads user_full->bot_info->placeholder_path and builds the
        // outline (`get_outline_object`); both arrive with the BotInfo step
        promise.set_value(None);
    }

    /// `UserManager::get_update_freeze_state_object` (UserManager.cpp:4786-4789).
    fn get_update_freeze_state_object(&self) -> td_api::updateFreezeState {
        td_api::updateFreezeState {
            is_frozen: self.freeze_since_date > 0,
            freezing_date: self.freeze_since_date,
            deletion_date: self.freeze_until_date,
            appeal_link: self.freeze_appeal_url.clone(),
        }
    }

    /// `UserManager::on_update_freeze_state` (UserManager.cpp:4791-4806) —
    /// the no-op guard, the `updateFreezeState` emission and the binlog-pmc
    /// persistence (erase when the freeze ended).
    pub fn on_update_freeze_state(
        &mut self,
        env: &mut UserEnv,
        freeze_since_date: i32,
        freeze_until_date: i32,
        freeze_appeal_url: String,
    ) {
        if freeze_since_date == self.freeze_since_date
            && freeze_until_date == self.freeze_until_date
            && freeze_appeal_url == self.freeze_appeal_url
        {
            return;
        }
        self.freeze_since_date = freeze_since_date;
        self.freeze_until_date = freeze_until_date;
        self.freeze_appeal_url = freeze_appeal_url;
        let update = self.get_update_freeze_state_object();
        self.send_update(td_api::Update::updateFreezeState(update));
        if self.freeze_since_date > 0 {
            env.global.binlog_pmc().set(
                "freeze_state",
                &format!(
                    "{} {} {}",
                    self.freeze_since_date, self.freeze_until_date, self.freeze_appeal_url
                ),
            );
        } else {
            env.global.binlog_pmc().erase("freeze_state");
        }
    }

    /// `UserManager::on_update_online_status_privacy`
    /// (UserManager.cpp:4768-4770) — the `GetContactsStatusesQuery`
    /// re-fetch (fire-and-forget, like the C++ handler).
    pub fn on_update_online_status_privacy(&mut self, _env: &mut UserEnv) {
        self.send_user_query(
            UserNetKind::GetContactsStatuses,
            telegram_api::Function::contacts_getStatuses(telegram_api::contacts_getStatuses),
            "contacts.getStatuses",
        );
    }

    /// `UserManager::on_update_phone_number_privacy`
    /// (UserManager.cpp:4772-4778) — every cached `UserFull`'s
    /// `need_phone_number_privacy_exception` can be outdated, so all expire.
    pub fn on_update_phone_number_privacy(&mut self, env: &UserEnv) {
        assert!(!env.auth_is_bot);
        tdutils::log_info!("Updated phone number privacy settings");
        for user_full in self.users_full.values_mut() {
            user_full.expires_at = 0.0;
        }
    }

    /// `UserManager::on_ignored_restriction_reasons_changed`
    /// (UserManager.cpp:4780-4784) — re-emit every restricted user (the
    /// C++ null-user form is the Rust unknown-user object).
    pub fn on_ignored_restriction_reasons_changed(&mut self, env: &UserEnv) {
        let user_ids: Vec<i64> = self.restricted_user_ids.iter().copied().collect();
        for user_id in user_ids {
            let user_object = self.get_user_object_of(env, UserId::new(user_id));
            self.send_update(td_api::Update::updateUser(td_api::updateUser {
                user: Some(user_object),
            }));
        }
    }

    /// `UserManager::on_user_rating_timeout` (UserManager.cpp:2905-2923) —
    /// the deadline plumbing is in (M.4.15); the body reads the UserFull
    /// star-rating fields that arrive with the StarRating subsystem, so until
    /// then the timeout never arms and a fire is a no-op.
    fn on_user_rating_timeout(&mut self, env: &UserEnv, user_id: UserId) {
        if env.global.close_flag() {
            return;
        }
        if self.users_full.contains_key(&user_id.get()) {
            // the pending-star-rating application and the update_user_full
            // re-run (cpp:2909-2923) — with the StarRating fields
        }
        // else: the C++ CHECK(user_full != nullptr); the map stays empty
        // until the StarRating fields exist, so a fire is unreachable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdutils::promise::PromiseCreator;

    /// The recording slot behind [`unit_promise_slot`].
    type UnitPromiseSlot = std::sync::Arc<std::sync::Mutex<Option<tdutils::status::Result<()>>>>;

    /// A `Promise<()>` recording slot (the promise-slot shape of the
    /// td.rs netted tests).
    fn unit_promise_slot() -> (tdutils::promise::Promise<()>, UnitPromiseSlot) {
        struct Rec(UnitPromiseSlot);
        impl tdutils::promise::PromiseInterface<()> for Rec {
            fn set_value(&mut self, _value: ()) {
                *self.0.lock().unwrap() = Some(tdutils::status::Result::Ok(()));
            }
            fn set_error(&mut self, error: tdutils::status::Status) {
                *self.0.lock().unwrap() = Some(tdutils::status::Result::Err(error));
            }
        }
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            tdutils::promise::Promise::from_interface(Rec(slot.clone())),
            slot,
        )
    }

    fn test_env() -> (TempEnv, UserManager) {
        TempEnv::new("unit")
    }

    /// A file-load factory whose nets never run (the band tests answer
    /// before any upload starts).
    struct PanicNetFactory;

    impl crate::files::FileLoadNetFactory for PanicNetFactory {
        fn download_net(&mut self) -> Box<dyn crate::files::FileDownloaderNet> {
            unimplemented!("the band tests start no downloads")
        }
        fn upload_net(&mut self) -> Box<dyn crate::files::FileUploaderNet> {
            unimplemented!("the band tests start no uploads")
        }
        fn hash_net(&mut self) -> Box<dyn crate::files::FileHashUploaderNet> {
            unimplemented!("the band tests start no hash uploads")
        }
        fn utils_net(&mut self) -> Box<dyn crate::files::FileDownloaderNet> {
            unimplemented!("the band tests touch no file io")
        }
    }

    /// The null upload callback of the band tests.
    struct NoopUserUploadCallback;

    impl crate::files::UploadCallback for NoopUserUploadCallback {
        fn on_upload_ok(
            &mut self,
            _file_upload_id: FileUploadId,
            _input_file: Option<telegram_api::InputFile>,
        ) {
        }
        fn on_upload_error(&mut self, _file_upload_id: FileUploadId, _status: Status) {}
    }

    /// A Global + OptionManager pair over a throwaway database.
    struct TempEnv {
        global: Global,
        options: OptionManager,
        dir: String,
    }

    impl TempEnv {
        fn new(name: &str) -> (TempEnv, UserManager) {
            Self::with_chat_info_db(name, true)
        }

        /// The bot photo-hold path needs a database without the chat
        /// info (`TdDb::open` forces chat info back on when the message
        /// database is requested, so both go off together).
        fn with_chat_info_db(name: &str, chat_info_db: bool) -> (TempEnv, UserManager) {
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
                use_message_database: chat_info_db,
                use_chat_info_database: chat_info_db,
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

    /// A received user with an access hash — passes `get_input_user`.
    fn seed_received_user(temp: &mut TempEnv, um: &mut UserManager, id: i64) -> UserId {
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(id, "Alice", USER_ACCESS_HASH_MASK | (1 << 1))),
            "test",
        )
    }

    /// A `telegram_api::Photo` with the 'a'/'c' pair (the
    /// `as_profile_photo` selection) plus an 'm' size.
    fn telegram_photo(id: i64) -> telegram_api::Photo {
        let size = |t: &str, dim: i32| {
            telegram_api::PhotoSize::photoSize(telegram_api::photoSize {
                r#type: t.to_owned(),
                w: dim,
                h: dim,
                size: dim * dim,
            })
        };
        telegram_api::Photo::photo(telegram_api::photo {
            flags: 0,
            has_stickers: false,
            id,
            access_hash: 7,
            file_reference: Vec::new(),
            date: 1000,
            sizes: vec![size("a", 160), size("c", 800), size("m", 320)],
            video_sizes: Vec::new(),
            dc_id: 2,
        })
    }

    fn photo_with_id(id: i64) -> Photo {
        get_photo(telegram_photo(id))
    }

    /// A `telegram_api::Document` with a non-voice audio attribute.
    fn audio_document(id: i64, access_hash: i64) -> telegram_api::Document {
        telegram_api::Document::document(telegram_api::document {
            flags: 0,
            id,
            access_hash,
            file_reference: Vec::new(),
            date: 1234,
            mime_type: "audio/mpeg".to_owned(),
            size: 100_000,
            thumbs: Vec::new(),
            video_thumbs: Vec::new(),
            dc_id: 2,
            attributes: vec![telegram_api::DocumentAttribute::documentAttributeAudio(
                telegram_api::documentAttributeAudio {
                    flags: 0,
                    voice: false,
                    duration: 180,
                    title: "Song".to_owned(),
                    performer: "Artist".to_owned(),
                    waveform: Vec::new(),
                },
            )],
        })
    }

    /// A document that classifies as nothing (no audio attribute).
    fn document_without_audio(id: i64) -> telegram_api::Document {
        telegram_api::Document::document(telegram_api::document {
            flags: 0,
            id,
            access_hash: 5,
            file_reference: Vec::new(),
            date: 1234,
            mime_type: "application/pdf".to_owned(),
            size: 500,
            thumbs: Vec::new(),
            video_thumbs: Vec::new(),
            dc_id: 2,
            attributes: Vec::new(),
        })
    }

    /// The recording side of [`recording_promise`].
    type Recorded<T> = std::sync::Arc<std::sync::Mutex<Vec<Result<T, String>>>>;

    /// A promise recording its outcome — the test stand-in for the
    /// actor-side promise plumbing.
    struct RecordingPromise<T> {
        outcomes: Recorded<T>,
    }

    impl<T: Send + 'static> tdutils::promise::PromiseInterface<T> for RecordingPromise<T> {
        fn set_value(&mut self, value: T) {
            self.outcomes.lock().unwrap().push(Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            self.outcomes
                .lock()
                .unwrap()
                .push(Err(error.message().to_owned()));
        }
    }

    fn recording_promise<T: Send + 'static>() -> (Promise<T>, Recorded<T>) {
        let outcomes: Recorded<T> = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Promise::from_interface(RecordingPromise {
                outcomes: outcomes.clone(),
            }),
            outcomes,
        )
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
            updates.iter().any(|update| matches!(
                update,
                td_api::Update::updateUser(u)
                    if u.user.as_ref().is_some_and(|user| user.id == 5 && user.first_name == "Alice")
            )),
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
        // the C++ `User::parse` cleans the phone too (cpp:1860s) —
        // `clean_phone_number` is digits-only (misc.cpp:58-60), the '+'
        // does not survive a store/parse round-trip
        assert_eq!(parsed.phone_number, "15551234567");
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

    /// 61e454f02: the noforwards cache rides the flags2 tail (bits 24-26,
    /// after has_linked_community_id) — byte-pinned against the C++
    /// `User::store`/`User::parse`.
    #[test]
    fn user_store_value_round_trips_the_noforwards_cache_flags() {
        let (mut temp, _um) = test_env();
        let ctx = temp.env(false).time_ctx();
        let mut user = User::new();
        user.first_name = "Nf".to_owned();
        user.is_received = true;
        user.is_bot = false;
        user.is_deleted = false;
        user.is_noforwards_inited = true;
        user.noforwards_my_enabled = true;
        user.noforwards_peer_enabled = false;

        let mut bytes = Vec::new();
        user.store_value(&mut bytes, &ctx);
        // The second stored u32 is the flags2 word; the three new flags
        // are its bits 24, 25 and 26.
        let flags2 = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        assert_eq!(flags2 >> 24 & 1, 1, "is_noforwards_inited at bit 24");
        assert_eq!(flags2 >> 25 & 1, 1, "noforwards_my_enabled at bit 25");
        assert_eq!(flags2 >> 26 & 1, 0, "noforwards_peer_enabled at bit 26");

        let parsed = User::parse_value(&bytes, &ctx).unwrap();
        assert!(parsed.is_noforwards_inited);
        assert!(parsed.noforwards_my_enabled);
        assert!(!parsed.noforwards_peer_enabled);

        // A blob written by the pre-61e454f02 format (no new bits set)
        // still parses with the cache uninited.
        let mut legacy = User::new();
        legacy.first_name = "Old".to_owned();
        legacy.is_bot = false;
        legacy.is_deleted = false;
        let mut legacy_bytes = Vec::new();
        legacy.store_value(&mut legacy_bytes, &ctx);
        let parsed = User::parse_value(&legacy_bytes, &ctx).unwrap();
        assert!(!parsed.is_noforwards_inited);
    }

    /// 61e454f02: `on_get_user_full` seeds the User-record cache and the
    /// has_protected_content accessors answer from it, even with the
    /// UserFull record dropped.
    #[test]
    fn on_get_user_full_caches_noforwards_in_the_user_record() {
        let (mut temp, mut um) = test_env();
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", USER_ACCESS_HASH_MASK | (1 << 1))),
            "t",
        );
        um.take_updates();

        let user_full = telegram_api::UserFull {
            id: 5,
            flags2: 1 << 23, // noforwards_my_enabled
            ..telegram_api::UserFull::default()
        };
        um.on_get_user_full(&mut temp.env(false), user_full);
        {
            let u = um.get_user(UserId::new(5)).unwrap();
            assert!(u.is_noforwards_inited);
            assert!(u.noforwards_my_enabled);
            assert!(!u.noforwards_peer_enabled);
        }

        // The accessors answer from the User cache after the UserFull
        // record resets (drop_user_full clears the full-info side only).
        um.drop_user_full(&mut temp.env(false), UserId::new(5));
        assert!(um.get_user_has_protected_content_of(UserId::new(5)));
        assert!(!um.get_user_has_protected_content_of(UserId::new(6)));

        // A second delivery flips the cached bits.
        let user_full = telegram_api::UserFull {
            id: 5,
            flags2: (1 << 23) | (1 << 24),
            ..telegram_api::UserFull::default()
        };
        um.on_get_user_full(&mut temp.env(false), user_full);
        {
            let u = um.get_user(UserId::new(5)).unwrap();
            assert!(u.noforwards_my_enabled);
            assert!(u.noforwards_peer_enabled);
        }
    }

    /// 61e454f02: `on_load_user_full_from_database` seeds the User-record
    /// cache from the stored full info.
    #[test]
    fn on_load_user_full_from_database_syncs_the_noforwards_cache() {
        let (mut temp, mut um) = test_env();
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", USER_ACCESS_HASH_MASK | (1 << 1))),
            "t",
        );
        um.take_updates();
        {
            let mut user_full = UserFull::new();
            user_full.noforwards_peer_enabled = true;
            let ctx = temp.env(false).time_ctx();
            let mut bytes = Vec::new();
            user_full.store_value(&mut bytes, &ctx);
            let key = UserManager::get_user_full_database_key(UserId::new(5));
            temp.env(false).global.td_db().common_kv().set(&key, &bytes);
        }

        um.get_user_full_force(&mut temp.env(false), UserId::new(5), "test");
        let u = um.get_user(UserId::new(5)).unwrap();
        assert!(u.is_noforwards_inited);
        assert!(!u.noforwards_my_enabled);
        assert!(u.noforwards_peer_enabled);
        assert!(um.get_user_has_protected_content_of(UserId::new(5)));
    }

    /// aa6d3aaf9: a deleted user forces the phone-number privacy
    /// exception off, like a contact or self.
    #[test]
    fn deleted_user_suppresses_the_phone_number_privacy_exception() {
        let (mut temp, mut um) = test_env();
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "gone", USER_ACCESS_HASH_MASK | (1 << 1))),
            "t",
        );
        um.users.get_mut(&5).unwrap().is_deleted = true;
        um.take_updates();

        let user_full = telegram_api::UserFull {
            id: 5,
            settings: Some(telegram_api::PeerSettings {
                flags: 1 << 4,
                need_contacts_exception: true,
                ..telegram_api::PeerSettings::default()
            }),
            ..telegram_api::UserFull::default()
        };
        um.on_get_user_full(&mut temp.env(false), user_full);
        assert!(
            !um.get_user_full(UserId::new(5))
                .unwrap()
                .need_phone_number_privacy_exception
        );
    }

    #[test]
    fn cache_version_mismatch_latches_the_repair_once() {
        let (mut temp, mut um) = test_env();
        let user_id = UserId::new(500);
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(
                user_id.get(),
                "Stale",
                USER_ACCESS_HASH_MASK | (1 << 1),
            )),
            "test",
        );
        // Simulate a row written by an older cache version (the C++
        // on_get_user stamps the current version; a blob migrated from a
        // previous release parses back with the stored one).
        um.users.get_mut(&user_id.get()).unwrap().cache_version = USER_CACHE_VERSION - 1;
        um.users.get_mut(&user_id.get()).unwrap().is_repaired = false;

        // The repair trigger (UserManager.cpp:10065-10071): one-shot.
        um.update_user(&mut temp.env(false), user_id, false, false);
        assert!(um.users.get(&user_id.get()).unwrap().is_repaired);

        // The latch holds — a second update never re-triggers (and the
        // parked `reload_user` sender cannot loop).
        um.update_user(&mut temp.env(false), user_id, false, false);
        assert!(um.users.get(&user_id.get()).unwrap().is_repaired);
    }

    #[test]
    fn cache_version_repair_needs_read_access_and_a_live_td() {
        let (mut temp, mut um) = test_env();

        // A min user: `have_input_peer_user(Read)` is false without an
        // access hash, so the trigger does not fire even though the
        // version is stale.
        let mut min_user = telegram_user(501, "Min", USER_ACCESS_HASH_MASK | (1 << 1));
        min_user.min = true;
        let user_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(min_user),
            "test",
        );
        um.users.get_mut(&user_id.get()).unwrap().cache_version = USER_CACHE_VERSION - 1;
        um.update_user(&mut temp.env(false), user_id, false, false);
        assert!(!um.users.get(&user_id.get()).unwrap().is_repaired);

        // A closing Td holds the trigger off too.
        let reachable_id = UserId::new(502);
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(
                reachable_id.get(),
                "Reachable",
                USER_ACCESS_HASH_MASK | (1 << 1),
            )),
            "test",
        );
        um.users.get_mut(&reachable_id.get()).unwrap().cache_version = USER_CACHE_VERSION - 1;
        temp.global.set_close_flag();
        um.update_user(&mut temp.env(false), reachable_id, false, false);
        assert!(!um.users.get(&reachable_id.get()).unwrap().is_repaired);
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
    fn secret_chat_log_event_round_trips() {
        // `SecretChatLogEvent` framing: version prefix, the int32 secret
        // chat id (SecretChatId stores as `store_int`), then the record.
        let mut secret_chat = SecretChat::new();
        secret_chat.access_hash = -4242;
        secret_chat.user_id = UserId::new(42);
        secret_chat.state = SecretChatState::Closed;
        secret_chat.ttl = 17;
        secret_chat.date = 1700000001;
        secret_chat.key_hash = String::new();
        secret_chat.layer = SECRET_CHAT_DEFAULT_LAYER;
        secret_chat.initial_folder_id = 1; // FolderId::archive()

        let mut bytes = Vec::new();
        store_secret_chat_log_event(&mut bytes, SecretChatId::new(7), &secret_chat);
        let (secret_chat_id, parsed) = parse_secret_chat_log_event(&bytes).unwrap();
        assert_eq!(secret_chat_id.get(), 7);
        assert_eq!(parsed.access_hash, -4242);
        assert_eq!(parsed.user_id.get(), 42);
        assert_eq!(parsed.state, SecretChatState::Closed);
        assert_eq!(parsed.ttl, 17);
        assert_eq!(parsed.date, 1700000001);
        assert_eq!(parsed.initial_folder_id, 1, "FolderId::archive");
        // the default-layer chat omits the field and restores it
        secret_chat.layer = 0;
        secret_chat.initial_folder_id = 0;
        let mut bytes = Vec::new();
        store_secret_chat_log_event(&mut bytes, SecretChatId::new(7), &secret_chat);
        let parsed = parse_secret_chat_log_event(&bytes).unwrap().1;
        assert_eq!(parsed.layer, SECRET_CHAT_DEFAULT_LAYER);
        assert_eq!(parsed.initial_folder_id, 0);
    }

    /// The M.4.10 intake: field deltas with the C++ change-flag matrix,
    /// the `updateSecretChat` emission, the `secret_chats_with_user` index
    /// maintenance and the read getters.
    #[test]
    fn on_update_secret_chat_intake_runs_the_diff_machine() {
        let (mut temp, mut um) = test_env();
        let secret_chat_id = SecretChatId::new(1);

        // First delivery: everything is new (`is_changed` starts true on a
        // fresh record), the diff machine emits and saves.
        um.on_update_secret_chat(
            &mut temp.env(false),
            secret_chat_id,
            777,
            UserId::new(42),
            SecretChatState::Waiting,
            true,
            60,
            1700000000,
            "khash".to_owned(),
            101,
            1,
        );
        let updates = um.take_updates();
        assert_eq!(updates.len(), 1, "updateSecretChat on the first intake");
        match &updates[0] {
            td_api::Update::updateSecretChat(update) => {
                let Some(secret_chat) = update.secret_chat.as_ref() else {
                    panic!("the secret chat payload must be present");
                };
                assert_eq!(secret_chat.id, 1);
                assert_eq!(secret_chat.user_id, 42);
                assert!(
                    matches!(
                        secret_chat.state,
                        Some(td_api::SecretChatState::secretChatStatePending(_))
                    ),
                    "Waiting converts to secretChatStatePending"
                );
                assert!(secret_chat.is_outbound);
                assert_eq!(secret_chat.layer, 101);
            }
            other => panic!("unexpected update {other:?}"),
        }
        let c = um.get_secret_chat(secret_chat_id).unwrap();
        assert_eq!(c.access_hash, 777);
        assert_eq!(c.ttl, 60);
        assert_eq!(c.date, 1700000000);
        assert_eq!(c.key_hash, "khash");
        assert!(c.is_saved, "the synchronous save completed");
        // the read getters
        assert_eq!(um.get_secret_chat_date(secret_chat_id), 1700000000);
        assert_eq!(um.get_secret_chat_ttl(secret_chat_id), 60);
        assert_eq!(um.get_secret_chat_user_id(secret_chat_id).get(), 42);
        assert!(um.get_secret_chat_is_outbound(secret_chat_id));
        assert_eq!(
            um.get_secret_chat_state(secret_chat_id),
            SecretChatState::Waiting
        );
        assert_eq!(um.get_secret_chat_layer(secret_chat_id), 101);
        assert_eq!(um.get_secret_chat_initial_folder_id(secret_chat_id), 1);
        assert_eq!(
            um.for_each_secret_chat_with_user(UserId::new(42)),
            vec![secret_chat_id]
        );

        // Second delivery: state advances (Unknown sentinels are skipped —
        // ttl -1, date 0, empty key_hash, layer 0, folder 0 keep theirs).
        um.on_update_secret_chat(
            &mut temp.env(false),
            secret_chat_id,
            777,
            UserId::new(42),
            SecretChatState::Active,
            true,
            -1,
            0,
            String::new(),
            0,
            0,
        );
        let updates = um.take_updates();
        assert_eq!(updates.len(), 1, "the state change emits again");
        let c = um.get_secret_chat(secret_chat_id).unwrap();
        assert_eq!(c.state, SecretChatState::Active);
        assert_eq!(c.ttl, 60, "ttl -1 is the keep-sentinel");
        assert_eq!(c.date, 1700000000);
        assert_eq!(c.layer, 101, "layer 0 is the keep-sentinel");
        assert!(!c.is_state_changed, "consumed by the diff machine");
        assert!(!c.is_changed);

        // The user changed: the index moves the chat, the error path logs.
        um.on_update_secret_chat(
            &mut temp.env(false),
            secret_chat_id,
            777,
            UserId::new(43),
            SecretChatState::Active,
            true,
            -1,
            0,
            String::new(),
            0,
            0,
        );
        assert_eq!(um.take_updates().len(), 1);
        assert!(
            um.for_each_secret_chat_with_user(UserId::new(42))
                .is_empty()
        );
        assert_eq!(
            um.for_each_secret_chat_with_user(UserId::new(43)),
            vec![secret_chat_id]
        );

        // A no-op delivery emits nothing.
        um.on_update_secret_chat(
            &mut temp.env(false),
            secret_chat_id,
            777,
            UserId::new(43),
            SecretChatState::Active,
            true,
            -1,
            0,
            String::new(),
            0,
            0,
        );
        assert!(um.take_updates().is_empty());

        // The unknown-id getters fall back to the C++ defaults.
        let missing = SecretChatId::new(99);
        assert_eq!(um.get_secret_chat_date(missing), 0);
        assert_eq!(um.get_secret_chat_ttl(missing), 0);
        assert!(!um.get_secret_chat_user_id(missing).is_valid());
        assert!(!um.get_secret_chat_is_outbound(missing));
        assert_eq!(um.get_secret_chat_state(missing), SecretChatState::Unknown);
        assert_eq!(um.get_secret_chat_layer(missing), 0);
        assert_eq!(um.get_secret_chat_initial_folder_id(missing), 0);
    }

    /// The `sc<id>` row survives reopen and loads through
    /// `get_secret_chat_force` — which also force-loads the chat's user.
    #[test]
    fn secret_chat_database_value_round_trips_across_reopen() {
        let dir = std::env::temp_dir()
            .join(format!(
                "td-rs-m410-scrow-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let user_id = UserId::new(42);
        let secret_chat_id = SecretChatId::new(1);

        // Phase 1: the user and the secret chat save through the normal
        // intake path (the binlog mirror retires once the row is durable).
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                ..Default::default()
            })
            .unwrap();
            let mut global = Global::new();
            global.init(opened.database);
            global.binlog_pmc().set("auth", "ok");
            let mut options = OptionManager::new(global.is_test_dc());
            let mut um = UserManager::new();
            {
                let mut env = UserEnv {
                    global: &mut global,
                    options: &mut options,
                    auth_is_bot: false,
                };
                let mut user = telegram_user(42, "Owner", USER_ACCESS_HASH_MASK | (1 << 1));
                user.access_hash = 4242;
                um.on_get_user(&mut env, telegram_api::User::user(user), "test");
                um.on_update_secret_chat(
                    &mut env,
                    secret_chat_id,
                    777,
                    user_id,
                    SecretChatState::Active,
                    false,
                    60,
                    1700000000,
                    "khash".to_owned(),
                    101,
                    0,
                );
            }
            assert!(!global.td_db().common_kv().get(b"sc1").is_empty());
            let _ = um.take_updates(); // drain the updateUser/updateSecretChat pair
            global.close_all(false);
        }

        // Phase 2: no binlog mirror at rest; the row loads through the
        // force path and pulls the user row in with it. A second, unknown
        // secret chat stays absent after its (empty) load.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                ..Default::default()
            })
            .unwrap();
            assert!(opened.secret_chat_events.is_empty(), "the mirror retired");
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
                assert!(um.get_secret_chat_force(&mut env, secret_chat_id, "test"));
                let c = um.get_secret_chat(secret_chat_id).unwrap();
                assert_eq!(c.user_id, user_id);
                assert_eq!(c.access_hash, 777);
                assert_eq!(c.state, SecretChatState::Active);
                assert_eq!(c.ttl, 60);
                assert_eq!(c.layer, 101);
                assert_eq!(c.key_hash, "khash");
                assert!(c.is_saved);
                assert!(um.have_secret_chat_force(&mut env, secret_chat_id, "test"));
                assert_eq!(um.get_user(user_id).unwrap().first_name, "Owner");
                assert!(!um.get_secret_chat_force(&mut env, SecretChatId::new(2), "test"));
                assert!(!um.have_secret_chat(SecretChatId::new(2)));
            }
            global.close_all(false);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The M.4.10 crash-window arc: a `SecretChatLogEvent` left in the
    /// binlog (the C++ state between `binlog_add` and the async sqlite
    /// flush) replays on reopen, re-saves to the "common" kv, and retires
    /// its binlog mirror.
    #[test]
    fn secret_chat_log_event_replays_across_reopen() {
        let dir = std::env::temp_dir()
            .join(format!(
                "td-rs-m410-sclog-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let secret_chat_id = SecretChatId::new(9);

        // Phase 1: the binlog mirror was written, the sqlite row never
        // landed.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                ..Default::default()
            })
            .unwrap();
            let mut global = Global::new();
            global.init(opened.database);
            global.binlog_pmc().set("auth", "ok");
            let mut secret_chat = SecretChat::new();
            secret_chat.access_hash = 11;
            secret_chat.user_id = UserId::new(42);
            secret_chat.state = SecretChatState::Waiting;
            secret_chat.ttl = 33;
            secret_chat.date = 1700000002;
            secret_chat.key_hash = "crash".to_owned();
            secret_chat.is_outbound = true;
            let mut data = Vec::new();
            store_secret_chat_log_event(&mut data, secret_chat_id, &secret_chat);
            global
                .td_db()
                .binlog()
                .add(tddb::td_db::LOG_EVENT_HANDLER_TYPE_SECRET_CHAT_INFOS, &data);
            // no sqlite write, straight to close — the crash
            global.close_all(false);
        }

        // Phase 2: reopen — TdDb collected the SecretChatInfos event;
        // replay restores the secret chat, emits updateSecretChat, saves
        // the sqlite row and erases the mirror.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                ..Default::default()
            })
            .unwrap();
            assert_eq!(
                opened.secret_chat_events.len(),
                1,
                "the SecretChatInfos event replays"
            );
            let event = opened.secret_chat_events[0].clone();

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
                um.on_binlog_secret_chat_event(&mut env, &event);
            }
            let c = um
                .get_secret_chat(secret_chat_id)
                .expect("replayed secret chat");
            assert_eq!(c.access_hash, 11);
            assert_eq!(c.user_id.get(), 42);
            assert_eq!(c.state, SecretChatState::Waiting);
            assert_eq!(c.ttl, 33);
            assert_eq!(c.key_hash, "crash");
            assert!(c.is_outbound);
            assert_eq!(c.log_event_id, 0, "the save retired the mirror");

            let row = global.td_db().common_kv().get(b"sc9");
            assert!(!row.is_empty(), "the replay saved the sqlite row");
            let parsed = SecretChat::parse_value(&row).unwrap();
            assert_eq!(parsed.state, SecretChatState::Waiting);
            let updates = um.take_updates();
            assert!(
                updates.iter().any(
                    |u| matches!(u, td_api::Update::updateSecretChat(u) if u.secret_chat.as_ref().is_some_and(|secret_chat| secret_chat.id == 9))
                ),
                "updateSecretChat on the replay"
            );
            global.close_all(false);
        }

        // Phase 3: reopen again — no SecretChatInfos events left; the row
        // loads through the manager's database path.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                ..Default::default()
            })
            .unwrap();
            assert!(
                opened.secret_chat_events.is_empty(),
                "the mirror was retired"
            );
            let mut global = Global::new();
            global.init(opened.database);
            let mut options = OptionManager::new(global.is_test_dc());
            let mut um = UserManager::new();
            assert!(um.get_secret_chat_force(
                &mut UserEnv {
                    global: &mut global,
                    options: &mut options,
                    auth_is_bot: false,
                },
                secret_chat_id,
                "test"
            ));
            assert_eq!(
                um.get_secret_chat_state(secret_chat_id),
                SecretChatState::Waiting
            );
            global.close_all(false);
        }
        let _ = std::fs::remove_dir_all(&dir);
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
        user.status = Some(telegram_api::UserStatus::userStatusOnline(
            telegram_api::userStatusOnline { expires },
        ));
        um.on_get_user(&mut temp.env(false), telegram_api::User::user(user), "t");
        let updates = um.take_updates();
        // A new user folds the status into its updateUser — a separate
        // updateUserStatus fires only for a status-only change (the C++
        // clears is_status_changed inside the is_changed block).
        assert!(
            updates
                .iter()
                .any(|update| matches!(update, td_api::Update::updateUser(u) if u.user.as_ref().is_some_and(|user| user.id == 5))),
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
        user.status = Some(telegram_api::UserStatus::userStatusOnline(
            telegram_api::userStatusOnline { expires },
        ));
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
                matches!(s.status, Some(td_api::UserStatus::userStatusOffline(_)))
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
            td_api::Update::updateUserFullInfo(f) if f.user_id == 5 && f.user_full_info.as_ref().is_some_and(|full| full.bio.as_ref().is_some_and(|bio| bio.text == "the bio"))
        )));
    }

    #[test]
    fn on_get_user_full_applies_settings_calls_and_private_forward_name() {
        let (mut temp, mut um) = test_env();
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1);
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        um.take_updates();

        // phone_calls_available together with phone_calls_private: the
        // C++ conjunct suppresses both call flags (cpp:9108-9110);
        // voice_messages_forbidden is gated on the user's premium flag
        // (cpp:9112); the privacy exception survives for a non-contact
        // (hook cpp:4446-4459).
        let mut user_full = telegram_api::UserFull {
            id: 5,
            flags: (1 << 4) | (1 << 5) | (1 << 20), // calls available+private, voice forbidden
            settings: Some(telegram_api::PeerSettings {
                flags: (1 << 4) | (1 << 14), // need_contacts_exception, charge_paid_message_stars
                need_contacts_exception: true,
                charge_paid_message_stars: 1i64 << 52, // clamps to the maximum
                ..telegram_api::PeerSettings::default()
            }),
            private_forward_name: "pf".to_owned(),
            birthday: Some(telegram_api::Birthday {
                day: 7,
                month: 11,
                year: 1990,
                ..telegram_api::Birthday::default()
            }),
            ..telegram_api::UserFull::default()
        };
        user_full.flags2 = (1 << 5) | (1 << 24); // birthday, noforwards_peer_enabled
        um.on_get_user_full(&mut temp.env(false), user_full);
        let stored = um.get_user_full(UserId::new(5)).unwrap();
        assert!(!stored.can_be_called);
        assert!(!stored.supports_video_calls);
        assert!(stored.has_private_calls);
        assert!(!stored.voice_messages_forbidden);
        assert!(stored.need_phone_number_privacy_exception);
        assert_eq!(stored.charge_paid_message_stars, 1i64 << 51);
        assert_eq!(stored.private_forward_name, "pf");
        assert!(!stored.birthdate.is_empty());
        assert_eq!(stored.birthdate.get_day(), 7);
        assert_eq!(stored.birthdate.get_month(), 11);
        assert_eq!(stored.birthdate.get_year(), 1990);

        // The privacy exception is suppressed once the user becomes a
        // contact (cpp:3378-3392 re-runs the hook from update_user; here
        // the direct hook parity via a re-delivery with is_contact set).
        um.users.get_mut(&5).unwrap().is_contact = true;
        let user_full = telegram_api::UserFull {
            id: 5,
            settings: Some(telegram_api::PeerSettings {
                flags: 1 << 4,
                need_contacts_exception: true,
                ..telegram_api::PeerSettings::default()
            }),
            private_forward_name: "pf".to_owned(),
            ..telegram_api::UserFull::default()
        };
        um.on_get_user_full(&mut temp.env(false), user_full);
        let stored = um.get_user_full(UserId::new(5)).unwrap();
        assert!(!stored.need_phone_number_privacy_exception);
        // The same-name re-delivery keeps the value (only the emptiness
        // crossing sets is_changed, cpp:9150-9156).
        assert_eq!(stored.private_forward_name, "pf");
        assert!(!stored.can_be_called);
    }

    #[test]
    fn user_full_pmc_round_trips_through_the_common_kv() {
        let (mut temp, mut um) = test_env();
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1);
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        um.take_updates();
        let mut user_full = telegram_api::UserFull {
            id: 5,
            flags: 1 << 1, // about
            common_chats_count: 3,
            stargifts_count: 7,
            ..telegram_api::UserFull::default()
        };
        user_full.about = "the bio".to_owned();
        um.on_get_user_full(&mut temp.env(false), user_full);

        let saved = temp.env(false).global.td_db().common_kv().get(b"usf5");
        assert!(!saved.is_empty(), "the usf row must be written");

        // A fresh manager over the same database force-loads the user and
        // then the full info; the parse-into-default flags emit the
        // updateUserFullInfo exactly like the C++ (cpp:9429-9431).
        let mut um2 = UserManager::new();
        um2.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        um2.take_updates();
        assert!(um2.get_user_full_force(&mut temp.env(false), UserId::new(5), "round-trip"));
        let stored = um2.get_user_full(UserId::new(5)).unwrap();
        assert_eq!(stored.about, "the bio");
        assert_eq!(stored.common_chat_count, 3);
        assert_eq!(stored.gift_count, 7);
        assert!(stored.is_update_user_full_sent);
        assert!(um2.take_updates().iter().any(|update| matches!(
            update,
            td_api::Update::updateUserFullInfo(f) if f.user_id == 5
        )));
    }

    #[test]
    fn user_full_unavailable_and_repair_paths() {
        let (mut temp, mut um) = test_env();
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1);
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        um.take_updates();

        // No row: the id is remembered as unavailable and is not re-read.
        assert!(!um.get_user_full_force(&mut temp.env(false), UserId::new(5), "t1"));
        assert!(um.unavailable_user_fulls.contains(&5));

        // A broken row: the repair path drops the record and the key.
        um.unavailable_user_fulls.clear();
        temp.env(false)
            .global
            .td_db()
            .common_kv()
            .set(b"usf5", &[0xff, 0xff, 0xff]);
        assert!(!um.get_user_full_force(&mut temp.env(false), UserId::new(5), "t2"));
        assert!(um.get_user_full(UserId::new(5)).is_none());
        assert!(
            temp.env(false)
                .global
                .td_db()
                .common_kv()
                .get(b"usf5")
                .is_empty()
        );
    }

    #[test]
    fn on_update_bot_has_preview_medias_bands() {
        let (mut temp, mut um) = test_env();
        // the bot wire bits: an access hash, the contact flag and the
        // bot bit (flags.14 + the explicit bool)
        let mut bot_user = telegram_user(5, "Bot", USER_ACCESS_HASH_MASK | (1 << 1) | (1 << 14));
        bot_user.bot = true;
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(bot_user),
            "t",
        );
        um.take_updates();
        // a plain user twin
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(6, "Human", USER_ACCESS_HASH_MASK | (1 << 1))),
            "t",
        );
        um.take_updates();

        // the invalid id logs and returns before any cache work
        um.on_update_bot_has_preview_medias(&mut temp.env(false), UserId::new(0), true);
        assert!(um.get_user_full(UserId::new(0)).is_none());

        // the unknown user and the non-bot twin return before the full
        // read (no record materializes)
        um.on_update_bot_has_preview_medias(&mut temp.env(false), UserId::new(999), true);
        um.on_update_bot_has_preview_medias(&mut temp.env(false), UserId::new(6), true);
        assert!(um.get_user_full(UserId::new(6)).is_none());

        // the bot-mode auth guard: the record stays untouched even with
        // the cache seeded
        um.add_user_full(UserId::new(5));
        um.on_update_bot_has_preview_medias(&mut temp.env(true), UserId::new(5), true);
        assert!(!um.get_user_full(UserId::new(5)).unwrap().has_preview_medias);

        // the ok tail flips the flag and broadcasts updateUserFullInfo
        um.on_update_bot_has_preview_medias(&mut temp.env(false), UserId::new(5), true);
        assert!(um.get_user_full(UserId::new(5)).unwrap().has_preview_medias);
        assert!(um.take_updates().iter().any(|update| matches!(
            update,
            td_api::Update::updateUserFullInfo(f) if f.user_id == 5
        )));

        // the same value again is change-gated (no second broadcast)
        um.on_update_bot_has_preview_medias(&mut temp.env(false), UserId::new(5), true);
        assert!(
            !um.take_updates()
                .iter()
                .any(|update| matches!(update, td_api::Update::updateUserFullInfo(_)))
        );

        // and the false value flips back
        um.on_update_bot_has_preview_medias(&mut temp.env(false), UserId::new(5), false);
        assert!(!um.get_user_full(UserId::new(5)).unwrap().has_preview_medias);
        assert!(um.take_updates().iter().any(|update| matches!(
            update,
            td_api::Update::updateUserFullInfo(f) if f.user_id == 5
        )));
    }

    #[test]
    fn user_full_expiry_and_load_user_full_paths() {
        let (mut temp, mut um) = test_env();
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1);
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        um.take_updates();
        // An unknown user is the C++ 400 "User not found".
        let (promise, result) = promise_unit_result();
        um.load_user_full(&mut temp.env(false), UserId::new(999), false, promise, "t");
        match &*result.lock().unwrap() {
            Some(tdutils::status::Result::Err(status)) => {
                assert_eq!((status.code(), status.message()), (400, "User not found"));
            }
            other => panic!("the unknown user: {other:?}"),
        }
        // No cached full and no row: the query band parks on the combined
        // users.getFullUser query — the net-inert posture answers the C++
        // close-state surface (the combiner is unwired without the
        // scheduler).
        let (promise, result) = promise_unit_result();
        um.load_user_full(&mut temp.env(false), UserId::new(5), false, promise, "t");
        match &*result.lock().unwrap() {
            Some(tdutils::status::Result::Err(status)) => {
                assert_eq!((status.code(), status.message()), (500, "Request aborted"));
            }
            other => panic!("the query band: {other:?}"),
        }

        // An expired row still force-loads (the C++ reload decision is
        // the caller's, cpp:8936-8948).
        let ctx = TimeCtx {
            now: tdutils::time::now() - 120.0,
            server_time: 0.0,
        };
        let mut user_full = UserFull::new();
        user_full.about = "old".to_owned();
        user_full.expires_at = ctx.now;
        let mut bytes = Vec::new();
        user_full.store_value(&mut bytes, &ctx);
        um.unavailable_user_fulls.clear();
        temp.env(false)
            .global
            .td_db()
            .common_kv()
            .set(b"usf5", &bytes);
        assert!(um.get_user_full_force(&mut temp.env(false), UserId::new(5), "t"));
        let stored = um.get_user_full(UserId::new(5)).unwrap();
        assert_eq!(stored.about, "old");
        assert!(stored.is_expired(&TimeCtx {
            now: tdutils::time::now(),
            server_time: 0.0
        }));
        // The expired-non-bot band refreshes in the background and
        // completes immediately (cpp:8941-8949)
        let (promise, result) = promise_unit_result();
        um.load_user_full(&mut temp.env(false), UserId::new(5), false, promise, "t");
        assert!(matches!(
            &*result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(()))
        ));
    }

    /// The shared completion slot of a user-full-info answer.
    type UserFullInfoSlot =
        std::sync::Arc<std::sync::Mutex<Option<tdutils::status::Result<td_api::UserFullInfo>>>>;

    /// A `Promise<td_api::UserFullInfo>` capturing its `Result` into a
    /// shared slot.
    fn promise_user_full_info() -> (Promise<td_api::UserFullInfo>, UserFullInfoSlot) {
        let result = std::sync::Arc::new(std::sync::Mutex::new(None));
        let result_clone = result.clone();
        (
            PromiseCreator::lambda_result(move |r| *result_clone.lock().unwrap() = Some(r)),
            result,
        )
    }

    #[test]
    fn load_user_full_for_request_answers_the_expired_cache() {
        let (mut temp, mut um) = test_env();
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1);
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        um.take_updates();

        // The unknown user keeps the load's 400 (cpp:8932-8934)
        let (answer, slot) = promise_user_full_info();
        um.load_user_full_for_request(&mut temp.env(false), UserId::new(999), false, answer, "t");
        match &*slot.lock().unwrap() {
            Some(tdutils::status::Result::Err(status)) => {
                assert_eq!((status.code(), status.message()), (400, "User not found"));
            }
            other => panic!("the unknown user: {other:?}"),
        }

        // An expired cached full answers the object immediately — the
        // non-bot band's background refresh rides the (net-inert)
        // combiner park, the answer does not wait for it (cpp:8941-8949
        // + the GetUserFullInfoRequest do_send_result)
        let ctx = TimeCtx {
            now: tdutils::time::now() - 120.0,
            server_time: 0.0,
        };
        let mut user_full = UserFull::new();
        user_full.about = "old".to_owned();
        user_full.expires_at = ctx.now;
        let mut bytes = Vec::new();
        user_full.store_value(&mut bytes, &ctx);
        um.unavailable_user_fulls.clear();
        temp.env(false)
            .global
            .td_db()
            .common_kv()
            .set(b"usf5", &bytes);
        let (answer, slot) = promise_user_full_info();
        um.load_user_full_for_request(&mut temp.env(false), UserId::new(5), false, answer, "t");
        assert!(
            matches!(&*slot.lock().unwrap(), Some(tdutils::status::Result::Ok(_))),
            "the expired cache answers the object"
        );
    }

    #[test]
    fn reload_user_bands_match_the_cpp_errors() {
        let (mut temp, mut um) = test_env();
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1);
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        um.take_updates();

        // the invalid id is the C++ 400 (cpp:5196-5198)
        let (promise, result) = promise_unit_result();
        um.reload_user(&mut temp.env(false), UserId::new(-5), promise, "t");
        match &*result.lock().unwrap() {
            Some(tdutils::status::Result::Err(status)) => {
                assert_eq!(
                    (status.code(), status.message()),
                    (400, "Invalid user identifier")
                );
            }
            other => panic!("the invalid id: {other:?}"),
        }

        // an unknown user: `get_input_user` errors the promise
        // (cpp:5201, TRY_STATUS_PROMISE)
        let (promise, result) = promise_unit_result();
        um.reload_user(&mut temp.env(false), UserId::new(999), promise, "t");
        match &*result.lock().unwrap() {
            Some(tdutils::status::Result::Err(status)) => {
                assert_eq!((status.code(), status.message()), (400, "User not found"));
            }
            other => panic!("the unknown user: {other:?}"),
        }

        // the known user parks on the GetUserMerger — the net-inert
        // posture answers the close-state surface (cpp:5203)
        let (promise, result) = promise_unit_result();
        um.reload_user(&mut temp.env(false), UserId::new(5), promise, "t");
        match &*result.lock().unwrap() {
            Some(tdutils::status::Result::Err(status)) => {
                assert_eq!((status.code(), status.message()), (500, "Request aborted"));
            }
            other => panic!("the reload park: {other:?}"),
        }
    }

    #[test]
    fn reload_user_full_bands_match_the_cpp_errors() {
        let (mut temp, mut um) = test_env();
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1);
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        um.take_updates();

        // an unknown user: `get_input_user` errors the promise (cpp:8955)
        let (promise, result) = promise_unit_result();
        um.reload_user_full(&mut temp.env(false), UserId::new(999), promise, "t");
        match &*result.lock().unwrap() {
            Some(tdutils::status::Result::Err(status)) => {
                assert_eq!((status.code(), status.message()), (400, "User not found"));
            }
            other => panic!("the unknown user: {other:?}"),
        }

        // the known user rides the combined query — the net-inert
        // posture answers the close-state surface
        let (promise, result) = promise_unit_result();
        um.reload_user_full(&mut temp.env(false), UserId::new(5), promise, "t");
        match &*result.lock().unwrap() {
            Some(tdutils::status::Result::Err(status)) => {
                assert_eq!((status.code(), status.message()), (500, "Request aborted"));
            }
            other => panic!("the reload park: {other:?}"),
        }
    }

    #[test]
    fn drop_user_full_resets_and_keeps_the_record() {
        let (mut temp, mut um) = test_env();
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1);
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        um.take_updates();
        let mut user_full = telegram_api::UserFull {
            id: 5,
            flags: (1 << 1) | (1 << 4),
            common_chats_count: 3,
            ..telegram_api::UserFull::default()
        };
        user_full.about = "the bio".to_owned();
        um.on_get_user_full(&mut temp.env(false), user_full);
        um.take_updates();

        um.drop_user_full(&mut temp.env(false), UserId::new(5));
        // The record stays with its default fields (cpp:9552-9602), and
        // the reset is persisted through the diff machine.
        let stored = um.get_user_full(UserId::new(5)).unwrap();
        assert_eq!(stored.about, "");
        assert_eq!(stored.common_chat_count, 0);
        assert_eq!(stored.expires_at, 0.0);
        assert!(!stored.can_be_called);
        let saved = temp.env(false).global.td_db().common_kv().get(b"usf5");
        assert!(!saved.is_empty());
        let parsed = UserFull::parse_value(
            &saved,
            &TimeCtx {
                now: tdutils::time::now(),
                server_time: 0.0,
            },
        )
        .unwrap();
        assert_eq!(parsed.about, "");
        assert!(um.take_updates().iter().any(|update| matches!(
            update,
            td_api::Update::updateUserFullInfo(f) if f.user_id == 5
        )));
    }

    #[test]
    fn drop_user_full_photos_matches_the_expected_id() {
        let (mut temp, mut um) = test_env();
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1);
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        um.take_updates();
        let seed = |um: &mut UserManager| {
            let stored = um.add_user_full(UserId::new(5));
            stored.personal_photo.id = 42;
            stored.photo.id = 43;
            stored.fallback_photo.id = 44;
            stored.expires_at = 123.0;
            stored.is_update_user_full_sent = true;
        };

        // Expected 0: everything non-empty is dropped; the resulting
        // profile-photo id is 0 == expected, so the expiry stays
        // (cpp:9508-9513).
        seed(&mut um);
        um.drop_user_full_photos(&mut temp.env(false), UserId::new(5), 0, "t");
        let stored = um.get_user_full(UserId::new(5)).unwrap();
        assert!(stored.personal_photo.is_empty());
        assert!(stored.photo.is_empty());
        assert!(stored.fallback_photo.is_empty());
        assert_eq!(stored.expires_at, 123.0);

        // A mismatched expected id drops all three and zeroes the expiry.
        seed(&mut um);
        um.drop_user_full_photos(&mut temp.env(false), UserId::new(5), 99, "t");
        let stored = um.get_user_full(UserId::new(5)).unwrap();
        assert!(stored.personal_photo.is_empty());
        assert!(stored.photo.is_empty());
        assert!(stored.fallback_photo.is_empty());
        assert_eq!(stored.expires_at, 0.0);

        // A matching expected id keeps the leading photo untouched.
        seed(&mut um);
        um.drop_user_full_photos(&mut temp.env(false), UserId::new(5), 42, "t");
        let stored = um.get_user_full(UserId::new(5)).unwrap();
        assert_eq!(stored.personal_photo.id, 42);
        assert_eq!(stored.photo.id, 43);
        assert_eq!(stored.fallback_photo.id, 44);
        assert_eq!(stored.expires_at, 123.0);
    }

    #[test]
    fn on_load_drops_the_full_of_a_deleted_user() {
        let (mut temp, mut um) = test_env();
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1);
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        um.take_updates();
        um.users.get_mut(&5).unwrap().is_deleted = true;

        let ctx = temp.env(false).time_ctx();
        let mut user_full = UserFull::new();
        user_full.about = "the bio".to_owned();
        user_full.expires_at = ctx.now + 60.0;
        let mut bytes = Vec::new();
        user_full.store_value(&mut bytes, &ctx);
        temp.env(false)
            .global
            .td_db()
            .common_kv()
            .set(b"usf5", &bytes);

        assert!(um.get_user_full_force(&mut temp.env(false), UserId::new(5), "t"));
        let stored = um.get_user_full(UserId::new(5)).unwrap();
        assert_eq!(stored.about, "");
        assert_eq!(stored.expires_at, 0.0);
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
            td_api::Update::updateUser(u) if u.user.as_ref().is_some_and(|user| user.id == 5)
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
        user.status = Some(telegram_api::UserStatus::userStatusOnline(
            telegram_api::userStatusOnline { expires },
        ));
        um.on_get_user(&mut temp.env(false), telegram_api::User::user(user), "t");
        let updates = um.take_updates();
        assert!(
            updates
                .iter()
                .any(|u| matches!(u, td_api::Update::updateUser(x) if x.user.as_ref().is_some_and(|user| user.id == 5))),
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

    #[test]
    fn system_user_ids_match_the_cpp_constants() {
        // UserManager.cpp:3019-3049.
        assert_eq!(
            UserManager::get_service_notifications_user_id().get(),
            777000
        );
        assert_eq!(
            UserManager::get_verification_codes_bot_user_id().get(),
            489000
        );
        for (is_test_dc, replies, anonymous, channel, anti_spam) in [
            (false, 1271266957, 1087968824, 136817688, 5434988373),
            (true, 708513, 552888, 936174, 2200583762),
        ] {
            assert_eq!(
                UserManager::get_replies_bot_user_id(is_test_dc).get(),
                replies
            );
            assert_eq!(
                UserManager::get_anonymous_bot_user_id(is_test_dc).get(),
                anonymous
            );
            assert_eq!(
                UserManager::get_channel_bot_user_id(is_test_dc).get(),
                channel
            );
            assert_eq!(
                UserManager::get_anti_spam_bot_user_id(is_test_dc).get(),
                anti_spam
            );
        }
    }

    #[test]
    fn add_special_users_synthesize_the_canned_users() {
        let (mut temp, mut um) = test_env();
        let env = &mut temp.env(false);

        let service_id = um.add_service_notifications_user(env);
        assert_eq!(service_id.get(), 777000);
        let service = um.get_user(service_id).unwrap();
        assert_eq!(service.first_name, "Telegram");
        assert!(service.is_verified);
        assert!(service.is_support);
        assert_eq!(service.phone_number, "42777");
        assert!(
            service.is_received,
            "the synthesis goes through on_get_user"
        );

        let anonymous_id = um.add_anonymous_bot_user(env);
        assert_eq!(anonymous_id.get(), 1087968824);
        let anonymous = um.get_user(anonymous_id).unwrap();
        assert_eq!(anonymous.first_name, "Group");
        assert_eq!(
            anonymous.usernames.get_first_username(),
            "GroupAnonymousBot"
        );
        assert!(anonymous.is_bot);

        let channel_id = um.add_channel_bot_user(env);
        assert_eq!(channel_id.get(), 136817688);
        let channel = um.get_user(channel_id).unwrap();
        assert_eq!(channel.first_name, "Channel");
        assert!(channel.is_bot);

        // the second call is served from the cache
        assert_eq!(um.add_service_notifications_user(env).get(), 777000);
    }

    #[test]
    fn set_my_id_publishes_the_option_and_guards_mismatches() {
        let (mut temp, mut um) = test_env();
        um.init_my_id(temp.global.binlog_pmc());
        assert!(!um.my_id.is_valid(), "a fresh database has no my id");

        um.set_my_id(&mut temp.env(false), UserId::new(4242));
        assert_eq!(um.get_my_id().get(), 4242);
        assert_eq!(temp.global.binlog_pmc().get("my_id"), "4242");
        assert_eq!(
            temp.options.get_option_integer("my_id", 0),
            4242,
            "the C++ publishes the my_id option"
        );

        // a conflicting userSelf is ignored (UserManager.cpp:3001-3004)
        um.set_my_id(&mut temp.env(false), UserId::new(7));
        assert_eq!(um.get_my_id().get(), 4242);
        assert_eq!(temp.global.binlog_pmc().get("my_id"), "4242");

        // an invalid id is ignored (UserManager.cpp:3005-3008)
        um.set_my_id(&mut temp.env(false), UserId::default());
        assert_eq!(um.get_my_id().get(), 4242);
    }

    #[test]
    fn my_id_restores_across_a_database_reopen() {
        let dir = std::env::temp_dir()
            .join(format!(
                "td-rs-m48-myid-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Phase 1: a fresh database, then a userSelf arrival stores the id.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                ..Default::default()
            })
            .unwrap();
            let mut global = Global::new();
            global.init(opened.database);
            // authorized marker travels in the binlog pmc — later reopens
            // must not destroy the sqlite file
            global.binlog_pmc().set("auth", "ok");
            let mut options = OptionManager::new(global.is_test_dc());
            let mut um = UserManager::new();
            um.init_my_id(global.binlog_pmc());
            assert!(!um.my_id.is_valid());
            let mut env = UserEnv {
                global: &mut global,
                options: &mut options,
                auth_is_bot: false,
            };
            um.set_my_id(&mut env, UserId::new(4242));
            drop(options);
            global.close_all(false);
        }

        // Phase 2: reopen — the binlog pmc restores my id.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                ..Default::default()
            })
            .unwrap();
            let mut global = Global::new();
            global.init(opened.database);
            let mut um = UserManager::new();
            um.init_my_id(global.binlog_pmc());
            assert_eq!(um.get_my_id().get(), 4242);
            global.close_all(false);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ----- M.4.11: the read surface -----

    fn received_user(id: i64) -> telegram_api::user {
        telegram_user(
            id,
            "Alice",
            USER_ACCESS_HASH_MASK | (1 << 1), // access_hash, first_name
        )
    }

    fn bot_user(id: i64, extra_flags: i32) -> telegram_api::user {
        let mut user = telegram_user(
            id,
            "HelperBot",
            USER_ACCESS_HASH_MASK | (1 << 1) | extra_flags,
        );
        user.bot = true;
        user.username = "helper_bot".to_owned(); // flags.3
        user.flags |= 1 << 3;
        user
    }

    #[test]
    fn input_user_and_input_peer_follow_the_access_hash_rules() {
        let (mut temp, mut um) = test_env();
        let my_id = UserId::new(4242);
        um.set_my_id(&mut temp.env(false), my_id);

        // Self collapses to the inputUserSelf/inputPeerSelf pair.
        assert!(matches!(
            um.get_input_user(&temp.env(false), my_id),
            Ok(telegram_api::InputUser::inputUserSelf(_))
        ));
        assert!(matches!(
            um.get_input_peer_user(&temp.env(false), my_id, AccessRights::Write),
            Some(telegram_api::InputPeer::inputPeerSelf(_))
        ));

        // A received user keeps its access hash for every right.
        let user_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(received_user(5)),
            "test",
        );
        assert!(matches!(
            um.get_input_user(&temp.env(false), user_id),
            Ok(telegram_api::InputUser::inputUser(
                telegram_api::inputUser {
                    user_id: 5,
                    access_hash: 123
                }
            ))
        ));
        for rights in [AccessRights::Know, AccessRights::Read, AccessRights::Write] {
            assert!(matches!(
                um.get_input_peer_user(&temp.env(false), user_id, rights),
                Some(telegram_api::InputPeer::inputPeerUser(
                    telegram_api::inputPeerUser {
                        user_id: 5,
                        access_hash: 123
                    }
                ))
            ));
        }

        // A min user: 400 without the bot fallback, the zero-hash input
        // with it; the input peer is refused for non-bots.
        let mut min_user = received_user(6);
        min_user.min = true;
        let min_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(min_user),
            "test",
        );
        assert!(matches!(
            um.get_input_user(&temp.env(false), min_id),
            Err(status) if status.code() == 400
        ));
        assert!(
            um.get_input_peer_user(&temp.env(false), min_id, AccessRights::Know)
                .is_none()
        );
        assert!(matches!(
            um.get_input_user(&temp.env(true), min_id),
            Ok(telegram_api::InputUser::inputUser(
                telegram_api::inputUser {
                    user_id: 6,
                    access_hash: 0
                }
            ))
        ));
        assert!(matches!(
            um.get_input_peer_user(&temp.env(true), min_id, AccessRights::Write),
            Some(telegram_api::InputPeer::inputPeerUser(_))
        ));

        // An unknown user: "User not found", and the force form falls
        // back to the zero-hash input (the C++ CHECK keeps it valid).
        let unknown = UserId::new(777);
        assert!(matches!(
            um.get_input_user(&temp.env(false), unknown),
            Err(status) if status.code() == 400
        ));
        assert!(matches!(
            um.get_input_user_force(&temp.env(false), unknown),
            telegram_api::InputUser::inputUser(telegram_api::inputUser {
                user_id: 777,
                access_hash: 0
            })
        ));
        assert!(
            um.get_input_peer_user(&temp.env(false), unknown, AccessRights::Know)
                .is_none()
        );

        // get_input_users propagates the first error; the force variant
        // skips the failures instead.
        assert!(
            um.get_input_users(&temp.env(false), &[user_id, unknown])
                .is_err()
        );
        let forced = um.get_input_users_force(&temp.env(false), &[user_id, unknown]);
        assert_eq!(forced.len(), 1);
    }

    #[test]
    fn input_encrypted_chat_requires_the_active_state() {
        let (mut temp, mut um) = test_env();
        let sc_id = SecretChatId::new(33);

        // The waiting chat: Know/Read see it; Write does not.
        um.on_update_secret_chat(
            &mut temp.env(false),
            sc_id,
            111,
            UserId::new(5),
            SecretChatState::Waiting,
            false,
            60,
            1700000000,
            "khash".to_owned(),
            SECRET_CHAT_DEFAULT_LAYER,
            0,
        );
        assert!(um.have_input_encrypted_peer(sc_id, AccessRights::Know));
        assert!(!um.have_input_encrypted_peer(sc_id, AccessRights::Write));
        assert!(
            um.get_input_encrypted_chat(sc_id, AccessRights::Read)
                .is_some()
        );
        assert!(
            um.get_input_encrypted_chat(sc_id, AccessRights::Write)
                .is_none()
        );

        // Only the Active state admits Write, carrying the access hash.
        um.on_update_secret_chat(
            &mut temp.env(false),
            sc_id,
            4321,
            UserId::new(5),
            SecretChatState::Active,
            false,
            -1,
            0,
            String::new(),
            0,
            0,
        );
        assert!(matches!(
            um.get_input_encrypted_chat(sc_id, AccessRights::Write),
            Some(telegram_api::InputEncryptedChat {
                chat_id: 33,
                access_hash: 4321
            })
        ));

        // An unknown secret chat is refused everywhere.
        assert!(!um.have_input_encrypted_peer(SecretChatId::new(99), AccessRights::Know));
        assert!(
            um.get_input_encrypted_chat(SecretChatId::new(99), AccessRights::Know)
                .is_none()
        );
    }

    #[test]
    fn get_bot_data_gates_and_surfaces() {
        let (mut temp, mut um) = test_env();

        // The four C++ error surfaces in order.
        assert!(matches!(
            um.get_bot_data(UserId::new(9)),
            Err(status) if status.message() == "Bot not found"
        ));
        let plain_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(received_user(5)),
            "test",
        );
        assert!(matches!(
            um.get_bot_data(plain_id),
            Err(status) if status.message() == "User is not a bot"
        ));

        // A min bot is not received yet.
        let mut min_bot = bot_user(6, 0);
        min_bot.min = true;
        let min_bot_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(min_bot),
            "test",
        );
        assert!(matches!(
            um.get_bot_data(min_bot_id),
            Err(status) if status.message() == "Bot is inaccessible"
        ));

        // A deleted bot is refused even when received.
        um.users.get_mut(&plain_id.get()).unwrap().is_bot = true;
        um.users.get_mut(&plain_id.get()).unwrap().is_deleted = true;
        assert!(matches!(
            um.get_bot_data(plain_id),
            Err(status) if status.message() == "Bot is deleted"
        ));

        let bot_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(bot_user(7, 0)),
            "test",
        );
        let bot_data = um.get_bot_data(bot_id).unwrap();
        assert_eq!(bot_data.username, "helper_bot");
        assert!(bot_data.can_join_groups, "default: nochats unset");
        assert!(!bot_data.can_be_edited);
        assert!(!bot_data.is_inline);
        assert!(!bot_data.need_location);
        assert!(!bot_data.can_be_added_to_attach_menu);

        // The inline placeholder rides the flags.19 bit; the attach-menu
        // flag is a struct field (the intake reads `bot_attach_menu`).
        let mut inline_bot = bot_user(8, 1 << 19);
        inline_bot.bot_attach_menu = true;
        let inline_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(inline_bot),
            "test",
        );
        let inline_data = um.get_bot_data(inline_id).unwrap();
        assert!(inline_data.is_inline);
        assert!(inline_data.can_be_added_to_attach_menu);
    }

    #[test]
    fn golden_user_object_json_matches_the_cpp_field_order() {
        let (mut temp, mut um) = test_env();

        let mut user = received_user(5);
        user.first_name = "Alice".to_owned();
        user.last_name = "Liddell".to_owned(); // flags.2
        user.flags |= 1 << 2;
        user.username = "alice".to_owned(); // flags.3
        user.flags |= 1 << 3;
        user.phone = "42777".to_owned(); // flags.4
        user.flags |= 1 << 4;
        let user_id = um.on_get_user(&mut temp.env(false), telegram_api::User::user(user), "test");
        let env = temp.env(false);
        let json = um.get_user_object_of(&env, user_id).to_json();
        // The S12 gate proved `to_json` byte-identical to td's converter;
        // the pinned string below pins the get_user_object field mapping
        // (UserManager.cpp:10301-10336) in the td_json order: int64 ids
        // ride the td_json string form and the nullable object fields
        // the C++ leaves null (photo — the stored small_file_id is
        // invalid, collectible colors, emoji status, verification,
        // restriction info, story state) are omitted exactly like the
        // C++ JSON plane.
        assert_eq!(
            json,
            r#"{"@type":"user","id":5,"first_name":"Alice","last_name":"Liddell","usernames":{"@type":"usernames","active_usernames":["alice"],"disabled_usernames":[],"editable_username":"alice","collectible_usernames":[],"secondary_username":""},"phone_number":"42777","status":{"@type":"userStatusEmpty"},"accent_color_id":5,"background_custom_emoji_id":"0","profile_accent_color_id":-1,"profile_background_custom_emoji_id":"0","is_contact":false,"is_mutual_contact":false,"is_close_friend":false,"is_premium":false,"is_support":false,"restricts_new_chats":false,"paid_message_star_count":0,"have_access":true,"type":{"@type":"userTypeRegular"},"language_code":"","added_to_attachment_menu":false}"#
        );
    }

    /// The live S76 residue regression: a min user applied through
    /// `on_get_user` emits one `updateUser` whose object carries only the
    /// fields the min application stored — byte-equal to the C++ record
    /// (the echo intake of the S76 corpus, UserManager.cpp:3205/:3397
    /// guards + the null-omitting object builder).
    #[test]
    fn min_user_emission_matches_the_cpp_record() {
        let (mut temp, mut um) = test_env();

        let mut user = received_user(2202815082);
        user.min = true;
        user.first_name = "..".to_owned();
        user.lang_code = "en".to_owned();
        let user_id = um.on_get_user(
            &mut temp.env(true),
            telegram_api::User::user(user),
            "Updates",
        );
        assert_eq!(user_id.get(), 2202815082);

        // exactly one updateUser, with the min-shaped object
        let updates = um.take_updates();
        assert_eq!(updates.len(), 1, "one emission for a new min user");
        let json = match &updates[0] {
            td_api::Update::updateUser(update) => update
                .user
                .as_ref()
                .expect("updateUser carries the user object")
                .to_json(),
            other => panic!("unexpected update: {other:?}"),
        };
        assert_eq!(
            json,
            r#"{"@type":"user","id":2202815082,"first_name":"..","last_name":"","phone_number":"","status":{"@type":"userStatusEmpty"},"accent_color_id":6,"background_custom_emoji_id":"0","profile_accent_color_id":-1,"profile_background_custom_emoji_id":"0","is_contact":false,"is_mutual_contact":false,"is_close_friend":false,"is_premium":false,"is_support":false,"restricts_new_chats":false,"paid_message_star_count":0,"have_access":true,"type":{"@type":"userTypeRegular"},"language_code":"en","added_to_attachment_menu":false}"#
        );

        // the unchanged re-application is silent (no is_changed)
        let mut again = received_user(2202815082);
        again.min = true;
        again.first_name = "..".to_owned();
        again.flags |= 1;
        again.lang_code = "en".to_owned();
        um.on_get_user(
            &mut temp.env(true),
            telegram_api::User::user(again),
            "Updates",
        );
        assert!(um.take_updates().is_empty(), "no emission without a change");
    }

    #[test]
    fn golden_secret_chat_object_json_matches_the_cpp_field_order() {
        let mut secret_chat = SecretChat::new();
        secret_chat.user_id = UserId::new(5);
        secret_chat.state = SecretChatState::Active;
        secret_chat.is_outbound = true;
        secret_chat.key_hash = "abc".to_owned();
        secret_chat.layer = SECRET_CHAT_DEFAULT_LAYER;
        let json = make_secret_chat_object(SecretChatId::new(33), &secret_chat).to_json();
        // td_json carries byte fields in the base64 form.
        assert_eq!(
            json,
            "{\"@type\":\"secretChat\",\"id\":33,\"user_id\":5,\"state\":{\"@type\":\"secretChatStateReady\"},\
             \"is_outbound\":true,\"key_hash\":\"YWJj\",\"layer\":73}"
        );
        // The unknown-chat stand-in answers closed with zeroed fields
        // (UserManager.cpp:10496-10500).
        assert_eq!(
            make_unknown_secret_chat_object(SecretChatId::new(34)).to_json(),
            "{\"@type\":\"secretChat\",\"id\":34,\"user_id\":0,\"state\":{\"@type\":\"secretChatStateClosed\"},\
             \"is_outbound\":false,\"key_hash\":\"\",\"layer\":0}"
        );
    }

    #[test]
    fn field_getters_cover_the_read_surface() {
        let (mut temp, mut um) = test_env();

        // Title composition.
        let mut named = received_user(5);
        named.first_name = "Alice".to_owned();
        named.last_name = "Liddell".to_owned();
        named.flags |= 1 << 2;
        let both_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(named),
            "test",
        );
        assert_eq!(um.get_user_title_of(both_id), "Alice Liddell");

        let mut first_only = received_user(6);
        first_only.first_name = "Bob".to_owned();
        let first_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(first_only),
            "test",
        );
        assert_eq!(um.get_user_title_of(first_id), "Bob");

        let mut last_only = received_user(7);
        last_only.first_name = String::new();
        last_only.last_name = "Brown".to_owned();
        last_only.flags = (last_only.flags | (1 << 2)) & !(1 << 1);
        let last_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(last_only),
            "test",
        );
        assert_eq!(um.get_user_title_of(last_id), "Brown");
        assert_eq!(um.get_user_title_of(UserId::new(1)), "");
        assert_eq!(um.get_secret_chat_title_of(SecretChatId::new(3)), "");

        // First username: the legacy username field feeds it; invalid
        // and unknown ids answer empty.
        assert_eq!(um.get_user_first_username_of(both_id), "");
        let mut with_username = received_user(8);
        with_username.username = "carol".to_owned();
        with_username.flags |= 1 << 3;
        let username_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(with_username),
            "test",
        );
        assert_eq!(um.get_user_first_username_of(username_id), "carol");
        assert_eq!(um.get_user_first_username_of(UserId::new(0)), "");
        assert_eq!(um.get_user_first_username_of(UserId::new(424242)), "");

        // Accent colors: the stored id wins; the invalid id falls back
        // to the id-derived default; unknown secret chats answer 5.
        let accents = um.users.get_mut(&both_id.get()).unwrap();
        accents.accent_color_id = AccentColorId::new(7);
        accents.profile_accent_color_id = AccentColorId::new(9);
        accents.background_custom_emoji_id = CustomEmojiId::new(11);
        accents.profile_background_custom_emoji_id = CustomEmojiId::new(13);
        assert_eq!(um.get_user_accent_color_id_object_of(both_id), 7);
        assert_eq!(um.get_user_profile_accent_color_id_object_of(both_id), 9);
        assert_eq!(um.get_user_background_custom_emoji_id_of(both_id).get(), 11);
        assert_eq!(
            um.get_user_profile_background_custom_emoji_id_of(both_id)
                .get(),
            13
        );
        assert_eq!(
            um.get_user_accent_color_id_object_of(first_id),
            AccentColorId::from_peer_id(first_id.get()).get()
        );
        assert_eq!(
            um.get_user_profile_accent_color_id_object_of(UserId::new(1)),
            -1
        );
        assert_eq!(
            um.get_secret_chat_accent_color_id_object_of(SecretChatId::new(3)),
            5
        );
        assert!(
            um.get_secret_chat_background_custom_emoji_id_of(SecretChatId::new(3))
                .get()
                == 0
        );

        // Upgraded gift colors exist only with the collectible.
        assert!(
            um.get_user_upgraded_gift_colors_object_of(both_id)
                .is_none()
        );
        um.users
            .get_mut(&both_id.get())
            .unwrap()
            .peer_color_collectible = Some(crate::peer_color::PeerColorCollectible::default());
        assert!(
            um.get_user_upgraded_gift_colors_object_of(both_id)
                .is_some()
        );

        // Emoji status object: None without one, the reduced payload with.
        assert!(um.get_user_emoji_status_object_of(both_id).is_none());
        um.users
            .get_mut(&both_id.get())
            .unwrap()
            .last_sent_emoji_status = Some(EmojiStatus::from_telegram(
            telegram_api::EmojiStatus::emojiStatus(telegram_api::emojiStatus {
                document_id: 77,
                ..Default::default()
            }),
        ));
        let status_object = um.get_user_emoji_status_object_of(both_id).unwrap();
        assert!(matches!(
            status_object.r#type,
            Some(td_api::EmojiStatusType::emojiStatusTypeCustomEmoji(
                td_api::emojiStatusTypeCustomEmoji {
                    custom_emoji_id: 77
                }
            ))
        ));

        // Stories and the poll predicate.
        assert!(!um.get_user_stories_hidden_of(both_id));
        um.users.get_mut(&both_id.get()).unwrap().stories_hidden = true;
        assert!(um.get_user_stories_hidden_of(both_id));

        // Dialog photos: present with the user, absent for strangers,
        // and proxied through the secret chat's user.
        assert!(um.get_user_dialog_photo(both_id).is_some());
        assert!(um.get_user_dialog_photo(UserId::new(1)).is_none());
        assert!(
            um.get_secret_chat_dialog_photo(SecretChatId::new(3))
                .is_none()
        );
    }

    #[test]
    fn can_poll_user_active_stories_follows_the_cpp_predicate() {
        let (mut temp, mut um) = test_env();
        um.set_my_id(&mut temp.env(false), UserId::new(4242));

        let user_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(received_user(5)),
            "test",
        );
        // was_online == 0 disqualifies.
        assert!(!um.can_poll_user_active_stories_of(user_id, 100.0));

        {
            let u = um.users.get_mut(&user_id.get()).unwrap();
            u.was_online = 90;
            u.max_active_story_id_next_reload_time = 50.0;
        }
        assert!(um.can_poll_user_active_stories_of(user_id, 100.0));
        assert!(
            !um.can_poll_user_active_stories_of(user_id, 49.0),
            "gated on the reload time"
        );

        // Myself, a bot, support and deleted users never poll.
        assert!(!um.can_poll_user_active_stories_of(UserId::new(4242), 100.0));
        let bot_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(bot_user(6, 0)),
            "test",
        );
        um.users.get_mut(&bot_id.get()).unwrap().was_online = 90;
        assert!(!um.can_poll_user_active_stories_of(bot_id, 100.0));
        um.users.get_mut(&user_id.get()).unwrap().is_support = true;
        assert!(!um.can_poll_user_active_stories_of(user_id, 100.0));
        um.users.get_mut(&user_id.get()).unwrap().is_support = false;
        um.users.get_mut(&user_id.get()).unwrap().is_deleted = true;
        assert!(!um.can_poll_user_active_stories_of(user_id, 100.0));
    }

    #[test]
    fn user_full_field_getters_force_and_plain_paths() {
        let (mut temp, mut um) = test_env();

        // Unknown users: the force-load fails and the answers are the
        // C++ defaults.
        let env = &mut temp.env(false);
        assert_eq!(um.get_user_about(env, UserId::new(9)), "");
        assert_eq!(um.get_user_private_forward_name(env, UserId::new(9)), "");
        assert!(!um.get_user_read_dates_private(env, UserId::new(9)));
        assert!(!um.get_user_has_protected_content_force(env, UserId::new(9)));

        // A loaded user full answers every getter from the record.
        let user_id = um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(received_user(5)),
            "test",
        );
        let env = &mut temp.env(false);
        {
            let user_full = um.add_user_full(user_id);
            user_full.about = "curiouser".to_owned();
            user_full.private_forward_name = "via bob".to_owned();
            user_full.read_dates_private = true;
            user_full.noforwards_my_enabled = true;
            user_full.voice_messages_forbidden = true;
            user_full.sponsored_enabled = false;
        }
        assert_eq!(um.get_user_about(env, user_id), "curiouser");
        assert_eq!(um.get_user_private_forward_name(env, user_id), "via bob");
        assert!(um.get_user_read_dates_private(env, user_id));
        assert!(um.get_user_has_protected_content_force(env, user_id));
        assert!(um.get_user_has_protected_content_of(user_id));
        assert!(um.get_user_has_protected_content_force_by_me(env, user_id));
        assert!(!um.get_user_has_protected_content_force_by_other(env, user_id));

        // Sponsored: non-premium me sees them; a premium me with the
        // flag off does not.
        assert!(um.get_my_sponsored_enabled());
        um.set_my_id(env, user_id);
        um.users.get_mut(&user_id.get()).unwrap().is_premium = true;
        assert!(
            !um.get_my_sponsored_enabled(),
            "sponsored_enabled=false and premium"
        );

        // The premium gate also covers the voice-messages getter.
        assert!(
            um.get_user_voice_messages_forbidden_of(user_id),
            "premium + flag"
        );
        um.users.get_mut(&user_id.get()).unwrap().is_premium = false;
        assert!(!um.get_user_voice_messages_forbidden_of(user_id));
    }

    #[test]
    fn secret_chat_id_object_registers_unknown_and_emits_once() {
        let (mut temp, mut um) = test_env();
        let env = &mut temp.env(false);

        let id = um.get_secret_chat_id_object(SecretChatId::new(66), "test");
        assert_eq!(id, 66);
        let updates = um.take_updates();
        assert_eq!(
            updates.len(),
            1,
            "the unknown secret chat must emit exactly one updateSecretChat"
        );
        assert!(matches!(
            &updates[0],
            td_api::Update::updateSecretChat(u) if u.secret_chat.as_ref().is_some_and(|secret_chat| secret_chat.id == 66
                && secret_chat.user_id == 0)
        ));
        assert!(um.unknown_secret_chats.contains(&66));

        // The registry suppresses the repeat; a known id never emits.
        let id = um.get_secret_chat_id_object(SecretChatId::new(66), "test");
        assert_eq!(id, 66);
        assert!(um.take_updates().is_empty());

        // A delivered secret chat answers without a new update (its own
        // intake emission is consumed first).
        let sc_id = SecretChatId::new(67);
        um.on_update_secret_chat(
            env,
            sc_id,
            1,
            UserId::new(5),
            SecretChatState::Active,
            false,
            60,
            1700000000,
            "khash".to_owned(),
            SECRET_CHAT_DEFAULT_LAYER,
            0,
        );
        um.take_updates();
        let id = um.get_secret_chat_id_object(sc_id, "test");
        assert_eq!(id, 67);
        assert!(um.take_updates().is_empty());
    }

    // ----- M.4.12: profile-editing local halves -----

    fn me_with_name(um: &mut UserManager, env: &mut UserEnv, first: &str, last: &str) -> UserId {
        um.set_my_id(env, UserId::new(5));
        let mut user = telegram_user(5, first, USER_ACCESS_HASH_MASK | (1 << 1)); // access_hash, first_name
        if !last.is_empty() {
            user.last_name = last.to_owned();
            user.flags |= 1 << 2;
        }
        um.on_get_user(env, telegram_api::User::user(user), "test")
    }

    #[test]
    fn set_name_and_bio_validate_and_compute_the_flag_matrix() {
        let (mut temp, mut um) = test_env();
        let user_id = me_with_name(&mut um, &mut temp.env(false), "Alice", "");

        // The empty cleaned first name is the one hard error.
        assert!(matches!(
            um.set_name_local("   ", "Smith"),
            Err(status) if status.message() == "First name must be non-empty"
        ));

        // Only the actually-changed fields carry their flag; cleaned
        // whitespace collapses before the comparison.
        assert_eq!(
            um.set_name_local("  Alice  ", "Liddell").unwrap(),
            ACCOUNT_UPDATE_LAST_NAME,
            "the first name is unchanged after cleaning"
        );
        assert_eq!(
            um.set_name_local("Alicia", "Liddell").unwrap(),
            ACCOUNT_UPDATE_FIRST_NAME | ACCOUNT_UPDATE_LAST_NAME
        );
        let _ = user_id;

        // bio: sanitized (strip + newline replacement) and compared
        // against the stored about.
        let env = &mut temp.env(false);
        um.add_user_full(user_id).about = "curiouser".to_owned();
        assert_eq!(um.set_bio_local(env, "curiouser").unwrap(), None);
        let bio = um
            .set_bio_local(env, "curiouser\nand\n\n")
            .unwrap()
            .unwrap();
        assert_eq!(
            bio, "curiouser and",
            "newlines collapse; trailing empties go away"
        );
    }

    #[test]
    fn on_update_profile_success_applies_the_about() {
        let (mut temp, mut um) = test_env();
        let user_id = me_with_name(&mut um, &mut temp.env(false), "Old", "");
        let env = &mut temp.env(false);
        um.add_user_full(user_id);

        um.on_update_profile_success(
            env,
            ACCOUNT_UPDATE_FIRST_NAME | ACCOUNT_UPDATE_ABOUT,
            "New",
            "",
            "the bio",
        );
        // The names are only cross-checked against the server answer
        // (LOG_IF(ERROR) fires for the mismatch); they are NOT applied
        // here — the user update carries them.
        assert_eq!(um.get_user(user_id).unwrap().first_name, "Old");
        assert_eq!(um.get_user_full(user_id).unwrap().about, "the bio");
        let updates = um.take_updates();
        assert!(
            updates
                .iter()
                .any(|update| matches!(update, td_api::Update::updateUserFullInfo(f) if f.user_full_info.as_ref().is_some_and(|full| full.bio.as_ref().is_some_and(|bio| bio.text == "the bio")))),
            "the about change must emit updateUserFullInfo"
        );
    }

    #[test]
    fn username_toggle_and_reorder_apply_optimistically() {
        let (mut temp, mut um) = test_env();
        let env = &mut temp.env(false);
        um.set_my_id(env, UserId::new(5));
        let mut user = telegram_user(5, "Alice", USER_ACCESS_HASH_MASK | (1 << 1));
        user.flags2 = 1; // usernames:flags2.0
        user.usernames = vec![
            telegram_api::Username {
                flags: 0,
                editable: true,
                active: true,
                deletable: false,
                expired: false,
                username: "alice".to_owned(),
            },
            telegram_api::Username {
                flags: 0,
                editable: false,
                active: true,
                deletable: false,
                expired: false,
                username: "wonder".to_owned(),
            },
        ];
        um.on_get_user(env, telegram_api::User::user(user), "test");
        um.take_updates();

        // Toggle a non-editable username off (the editable one is
        // protected for non-bots); the diff emits.
        um.toggle_username_is_active_local(env, "wonder", false)
            .unwrap();
        assert!(
            um.get_user(UserId::new(5))
                .unwrap()
                .usernames
                .get_active_usernames()
                .iter()
                .all(|name| name != "wonder"),
            "wonder must leave the active set"
        );
        assert!(
            !um.take_updates().is_empty(),
            "the change must emit updateUser"
        );

        // The editable username can't be toggled by a user.
        assert!(matches!(
            um.toggle_username_is_active_local(env, "alice", false),
            Err(status) if status.message() == "Wrong username specified"
        ));
        assert!(matches!(
            um.reorder_usernames_local(env, &["alice".to_owned(), "wonder".to_owned()]),
            Err(status) if status.message() == "Invalid username order specified"
        ));
        um.take_updates();

        // Reorder the remaining actives; the same order resolves at once.
        um.reorder_usernames_local(env, &["alice".to_owned()])
            .unwrap();
        um.take_updates();

        // The update-intake forms share the same rules.
        um.on_update_active_usernames_order(env, UserId::new(5), vec!["alice".to_owned()]);
        um.on_update_username_is_active(env, UserId::new(5), "wonder", true);
        assert!(
            um.get_user(UserId::new(5))
                .unwrap()
                .usernames
                .get_active_usernames()
                .contains(&"wonder".to_owned()),
            "the update reactivates wonder"
        );
    }

    #[test]
    fn bot_username_ops_gate_on_the_editable_flag() {
        let (mut temp, mut um) = test_env();
        let env = &mut temp.env(false);

        let mut bot = bot_user(7, 0);
        bot.bot_can_edit = true;
        // drop the legacy username so only the usernames vector matters
        bot.username = String::new();
        bot.flags &= !(1 << 3);
        bot.usernames = vec![
            telegram_api::Username {
                flags: 0,
                editable: true,
                active: true,
                deletable: false,
                expired: false,
                username: "helper_bot".to_owned(),
            },
            telegram_api::Username {
                flags: 0,
                editable: false,
                active: true,
                deletable: false,
                expired: false,
                username: "wonder_bot".to_owned(),
            },
        ];
        bot.flags2 = 1; // usernames:flags2.0
        let bot_id = um.on_get_user(env, telegram_api::User::user(bot), "test");

        // The bot toggles a non-editable username optimistically; with a
        // single active username even a bot couldn't drop its editable.
        um.toggle_bot_username_is_active_local(env, bot_id, "wonder_bot", false)
            .unwrap();
        assert!(!um.take_updates().is_empty());
        assert!(matches!(
            um.toggle_bot_username_is_active_local(env, bot_id, "unknown", true),
            Err(status) if status.message() == "Wrong username specified"
        ));

        // A non-editable bot is refused before anything else.
        let plain_bot_id = um.on_get_user(env, telegram_api::User::user(bot_user(8, 0)), "test");
        assert!(matches!(
            um.reorder_bot_usernames_local(env, plain_bot_id, &[]),
            Err(status) if status.message() == "The bot can't be edited"
        ));
        um.reorder_bot_usernames_local(env, bot_id, &["helper_bot".to_owned()])
            .unwrap();
    }

    #[test]
    fn accent_color_and_emoji_status_appliers_update_me() {
        let (mut temp, mut um) = test_env();
        let user_id = me_with_name(&mut um, &mut temp.env(false), "Alice", "");
        let env = &mut temp.env(false);

        // The invalid id is the hard error; the id-derived default
        // collapses to the invalid marker (the net send halves park on
        // the family queue — the net-inert test env holds no handle, so
        // only the gate bands and the park shape run here).
        let (promise, invalid_slot) = unit_promise_slot();
        um.set_accent_color(AccentColorId::new(-1), CustomEmojiId::default(), promise);
        assert!(matches!(
            invalid_slot.lock().unwrap().take(),
            Some(tdutils::status::Result::Err(ref status))
                if status.message() == "Invalid accent color identifier specified"
        ));
        let (promise, collapsed_slot) = unit_promise_slot();
        um.set_accent_color(
            AccentColorId::from_peer_id(5),
            CustomEmojiId::new(9),
            promise,
        );
        let (promise, passed_slot) = unit_promise_slot();
        um.set_accent_color(AccentColorId::new(3), CustomEmojiId::new(9), promise);
        // the two valid sends parked their promises on the family queue
        // (the id-derived default collapse happens inside the first)
        assert_eq!(um.pending_update_color_queries.len(), 2);
        assert!(collapsed_slot.lock().unwrap().is_none());
        assert!(passed_slot.lock().unwrap().is_none());

        // The success appliers mutate me and emit.
        um.on_update_accent_color_success(env, false, AccentColorId::new(3), CustomEmojiId::new(9));
        um.on_update_accent_color_success(env, true, AccentColorId::new(4), CustomEmojiId::new(10));
        let u = um.get_user(user_id).unwrap();
        assert_eq!(u.accent_color_id.get(), 3);
        assert_eq!(u.background_custom_emoji_id.get(), 9);
        assert_eq!(u.profile_accent_color_id.get(), 4);
        assert_eq!(u.profile_background_custom_emoji_id.get(), 10);
        assert!(!um.take_updates().is_empty());

        // The emoji-status setter needs the premium option.
        let (promise, result) = promise_result();
        um.set_emoji_status(env, None, promise);
        assert!(matches!(
            &*result.lock().unwrap(),
            Some(tdutils::status::Result::Err(status)) if status.message().contains("Premium")
        ));
        env.options.set_option_boolean("is_premium", true);
        let (promise, result) = promise_result();
        um.set_emoji_status(env, None, promise);
        assert!(
            (*result.lock().unwrap()).is_none(),
            "the promise rides the query"
        );
        assert_eq!(um.pending_update_emoji_status_queries.len(), 1);
        assert!(um.pending_update_emoji_status_queries[0].0.is_none());
        um.pending_update_emoji_status_queries.clear();

        let status = Some(EmojiStatus::from_telegram(
            telegram_api::EmojiStatus::emojiStatus(telegram_api::emojiStatus {
                document_id: 77,
                ..Default::default()
            }),
        ));
        um.on_set_emoji_status(env, status.clone());
        um.on_set_user_emoji_status(env, user_id, status);
        let updates = um.take_updates();
        assert!(
            updates.iter().any(
                |update| matches!(update, td_api::Update::updateUser(u) if u.user.as_ref().is_some_and(
                    |user| user.emoji_status.as_ref().is_some_and(|emoji_status| matches!(
                        emoji_status.r#type,
                        Some(td_api::EmojiStatusType::emojiStatusTypeCustomEmoji(_))
                    ))
                ))
            ),
            "the emoji status must surface in updateUser"
        );
    }

    #[test]
    fn birthdate_and_sponsored_appliers_write_the_full() {
        let (mut temp, mut um) = test_env();
        let user_id = me_with_name(&mut um, &mut temp.env(false), "Alice", "");
        let env = &mut temp.env(false);
        um.add_user_full(user_id);

        let birthdate = Birthdate::new(5, 3, 1990);
        um.on_set_birthdate(env, birthdate);
        assert_eq!(um.get_user_full(user_id).unwrap().birthdate, birthdate);
        let first_updates = um.take_updates();
        assert!(first_updates.iter().any(|update| matches!(
            update,
            td_api::Update::updateUserFullInfo(f)
                if f.user_full_info.as_ref().is_some_and(|full| full.birthdate.as_ref().is_some_and(|birthdate| birthdate.day == 5))
        )));

        // The same birthdate again is a silent no-op.
        um.on_set_birthdate(env, birthdate);
        assert!(um.take_updates().is_empty());

        // Sponsored messages start disabled by default (the C++ field
        // default); with premium me and the server-set true, the flag
        // answers; flip it off and then silence.
        um.add_user_full(user_id).sponsored_enabled = true;
        um.users.get_mut(&user_id.get()).unwrap().is_premium = true;
        assert!(
            um.get_my_sponsored_enabled(),
            "the flag answers while the full is loaded"
        );
        um.on_toggle_sponsored_messages(env, false);
        assert!(!um.get_user_full(user_id).unwrap().sponsored_enabled);
        assert!(!um.get_my_sponsored_enabled());
        assert!(!um.take_updates().is_empty());
        um.on_toggle_sponsored_messages(env, false);
        assert!(um.take_updates().is_empty());
    }

    // ----- M.4.13: the saved-music registry and photos params -----

    #[test]
    fn my_saved_music_ids_round_trip_across_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "td-rs-m13-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Phase 1: an empty database leaves the set un-inited (the C++
        // reloads it from the server); the save writes the vector form.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone().to_string_lossy().into_owned(),
                ..Default::default()
            })
            .unwrap();
            let mut global = Global::new();
            global.init(opened.database);
            global.binlog_pmc().set("auth", "ok");
            let mut options = OptionManager::new(global.is_test_dc());
            let mut um = UserManager::new();
            um.init_my_saved_music_ids(global.binlog_pmc(), true);
            assert!(!um.are_my_saved_music_ids_inited);

            um.my_saved_music_ids.insert(11);
            um.my_saved_music_ids.insert(22);
            {
                let mut env = UserEnv {
                    global: &mut global,
                    options: &mut options,
                    auth_is_bot: false,
                };
                um.save_my_saved_music_ids(&mut env);
            }
            global.close_all(false);
        }

        // Phase 2: reopen — the set restores, zero ids skipped like the
        // C++ start_up.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone().to_string_lossy().into_owned(),
                ..Default::default()
            })
            .unwrap();
            let mut global = Global::new();
            global.init(opened.database);
            let mut um = UserManager::new();
            um.init_my_saved_music_ids(global.binlog_pmc(), true);
            assert!(um.are_my_saved_music_ids_inited);
            assert!(um.my_saved_music_ids.contains(&11));
            assert!(um.my_saved_music_ids.contains(&22));
            assert!(!um.my_saved_music_ids.contains(&33));
            global.close_all(false);
        }

        // Phase 3: a broken value keeps the set un-inited (the net
        // reload half), and bots/never-authorized sessions skip the
        // load entirely.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone().to_string_lossy().into_owned(),
                ..Default::default()
            })
            .unwrap();
            let mut global = Global::new();
            global.init(opened.database);
            global
                .binlog_pmc()
                .set_raw(b"my_saved_music_ids", &[0xff, 0xff]);

            let mut um = UserManager::new();
            um.init_my_saved_music_ids(global.binlog_pmc(), true);
            assert!(!um.are_my_saved_music_ids_inited);

            // A valid vector followed by trailing bytes fails the C++
            // log_event_parse fetch_end, so the set stays un-inited too.
            global.binlog_pmc().set_raw(
                b"my_saved_music_ids",
                &[
                    0x01, 0x00, 0x00, 0x00, 0x2b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xde,
                ],
            );
            let mut um = UserManager::new();
            um.init_my_saved_music_ids(global.binlog_pmc(), true);
            assert!(!um.are_my_saved_music_ids_inited);

            um.init_my_saved_music_ids(global.binlog_pmc(), false);
            assert!(!um.are_my_saved_music_ids_inited);
            global.close_all(false);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn profile_photos_params_validate_and_clamp() {
        let (mut temp, mut um) = test_env();
        let user_id = seed_received_user(&mut temp, &mut um, 5);

        // The validation errors fire before the input-user check.
        let (p1, a1) = recording_promise::<td_api::ChatPhotos>();
        um.get_user_profile_photos(&mut temp.env(false), user_id, -1, 10, p1);
        assert_eq!(
            a1.lock().unwrap().as_slice(),
            [Err("Parameter offset must be non-negative".to_owned())]
        );
        let (p2, a2) = recording_promise::<td_api::ChatPhotos>();
        um.get_user_profile_photos(&mut temp.env(false), user_id, 0, 0, p2);
        assert_eq!(
            a2.lock().unwrap().as_slice(),
            [Err("Parameter limit must be positive".to_owned())]
        );

        // An unknown user answers the input-user error.
        let (p3, a3) = recording_promise::<td_api::ChatPhotos>();
        um.get_user_profile_photos(&mut temp.env(false), UserId::new(999), 0, 10, p3);
        assert_eq!(
            a3.lock().unwrap().as_slice(),
            [Err("User not found".to_owned())]
        );

        // A valid request parks in the pending queue with the clamped
        // limit (the send is the net step).
        let (p4, _a4) = recording_promise::<td_api::ChatPhotos>();
        um.get_user_profile_photos(&mut temp.env(false), user_id, 3, 1000, p4);
        let requests = um.pending_get_user_photos_requests.get(&5).unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].offset, 3);
        assert_eq!(requests[0].limit, MAX_GET_PROFILE_PHOTOS);

        // The record answers from the cache after the intake.
        um.on_get_user_photos(
            user_id,
            3,
            100,
            6,
            vec![telegram_photo(31), telegram_photo(32), telegram_photo(33)],
        );
        um.on_get_user_profile_photos(&temp.env(false), user_id, Ok(()));
        assert_eq!(
            _a4.lock().unwrap().as_slice(),
            [Ok(td_api::ChatPhotos {
                total_count: 6,
                photos: vec![
                    get_chat_photo_object(&photo_with_id(31)),
                    get_chat_photo_object(&photo_with_id(32)),
                    get_chat_photo_object(&photo_with_id(33)),
                ],
            })]
        );
    }

    #[test]
    fn photos_cache_answers_pagination_from_cache() {
        let (mut temp, mut um) = test_env();
        let user_id = seed_received_user(&mut temp, &mut um, 5);

        // Fill the cache window [0, 3) out of 5 (a parked request keeps
        // the intake's pending-request check honest).
        let (p0, _a0) = recording_promise::<td_api::ChatPhotos>();
        um.get_user_profile_photos(&mut temp.env(false), user_id, 0, 3, p0);
        um.on_get_user_photos(
            user_id,
            0,
            3,
            5,
            vec![telegram_photo(11), telegram_photo(12), telegram_photo(13)],
        );
        um.on_get_user_profile_photos(&temp.env(false), user_id, Ok(()));
        assert_eq!(_a0.lock().unwrap().len(), 1, "the parked request answered");

        // A cache-covered request answers without parking.
        let (p1, a1) = recording_promise::<td_api::ChatPhotos>();
        um.get_user_profile_photos(&mut temp.env(false), user_id, 1, 2, p1);
        assert_eq!(
            a1.lock().unwrap().as_slice(),
            [Ok(td_api::ChatPhotos {
                total_count: 5,
                photos: vec![
                    get_chat_photo_object(&photo_with_id(12)),
                    get_chat_photo_object(&photo_with_id(13)),
                ],
            })]
        );
        assert!(!um.pending_get_user_photos_requests.contains_key(&5));

        // A limit reaching past the count clamps to the count — [2, 5)
        // is not covered by [0, 3), so the request parks.
        let (p2, a2) = recording_promise::<td_api::ChatPhotos>();
        um.get_user_profile_photos(&mut temp.env(false), user_id, 2, 10, p2);
        assert_eq!(
            um.pending_get_user_photos_requests.get(&5).unwrap().len(),
            1
        );

        // The offset-3 intake appends to the cache window...
        um.on_get_user_photos(
            user_id,
            3,
            10,
            5,
            vec![telegram_photo(14), telegram_photo(15)],
        );
        // ...and the completion callback answers [2, 5) from it.
        um.on_get_user_profile_photos(&temp.env(false), user_id, Ok(()));
        assert_eq!(
            a2.lock().unwrap().as_slice(),
            [Ok(td_api::ChatPhotos {
                total_count: 5,
                photos: vec![
                    get_chat_photo_object(&photo_with_id(13)),
                    get_chat_photo_object(&photo_with_id(14)),
                    get_chat_photo_object(&photo_with_id(15)),
                ],
            })]
        );

        // An offset past the count answers empty with the total count.
        let (p3, a3) = recording_promise::<td_api::ChatPhotos>();
        um.get_user_profile_photos(&mut temp.env(false), user_id, 7, 2, p3);
        assert_eq!(
            a3.lock().unwrap().as_slice(),
            [Ok(td_api::ChatPhotos {
                total_count: 5,
                photos: Vec::new(),
            })]
        );

        // The photos register into User::photo_ids, and the registered
        // photo answers no file source (cpp:7003-7006).
        let u = um.get_user(user_id).unwrap();
        assert!(u.photo_ids.contains(&11) && u.photo_ids.contains(&15));
        assert_eq!(
            um.get_user_profile_photo_file_source_id(user_id, 11),
            FileSourceId::default()
        );
        // An unregistered photo id allocates a stable source id.
        let first = um.get_user_profile_photo_file_source_id(user_id, 999);
        assert!(first.is_valid());
        assert_eq!(
            um.get_user_profile_photo_file_source_id(user_id, 999),
            first
        );
    }

    #[test]
    fn photos_cache_offset_mismatch_resets_and_trim() {
        let (mut temp, mut um) = test_env();
        let user_id = seed_received_user(&mut temp, &mut um, 5);

        // Park a request so the intake's CHECK holds.
        let (p, _a) = recording_promise::<td_api::ChatPhotos>();
        um.get_user_profile_photos(&mut temp.env(false), user_id, 2, 4, p);
        um.on_get_user_photos(
            user_id,
            2,
            4,
            9,
            vec![telegram_photo(21), telegram_photo(22)],
        );
        let cache = um.user_photos.get(&5).unwrap();
        assert_eq!(cache.count, 9);
        assert_eq!(cache.offset, 2);
        assert_eq!(cache.photos.len(), 2);

        // A subsequent intake at an offset that cannot append resets the
        // window to the new offset.
        um.on_get_user_profile_photos(&temp.env(false), user_id, Ok(()));
        let (p2, _a2) = recording_promise::<td_api::ChatPhotos>();
        um.get_user_profile_photos(&mut temp.env(false), user_id, 8, 4, p2);
        um.on_get_user_photos(user_id, 8, 4, 9, vec![telegram_photo(29)]);
        let cache = um.user_photos.get(&5).unwrap();
        assert_eq!(cache.offset, 8);
        assert_eq!(cache.photos.len(), 1);

        // An empty photo drops the total count; an offset beyond the
        // count collapses the window (cpp:7099-7101).
        um.on_get_user_profile_photos(&temp.env(false), user_id, Ok(()));
        let (p3, _a3) = recording_promise::<td_api::ChatPhotos>();
        um.get_user_profile_photos(&mut temp.env(false), user_id, 0, 10, p3);
        um.on_get_user_photos(
            user_id,
            0,
            10,
            2,
            vec![
                telegram_api::Photo::photoEmpty(telegram_api::photoEmpty { id: 1 }),
                telegram_photo(41),
            ],
        );
        let cache = um.user_photos.get(&5).unwrap();
        // photoEmpty → is_empty → count-- and skipped
        assert_eq!(cache.count, 1);
        assert_eq!(cache.photos.len(), 1);
        assert_eq!(cache.photos[0].id, 41);

        // drop_user_photos(false) resets to unknown, (true) to empty.
        um.drop_user_photos(user_id, false, "test");
        assert_eq!(um.user_photos.get(&5).unwrap().count, -1);
        let (p4, _a4) = recording_promise::<td_api::ChatPhotos>();
        um.get_user_profile_photos(&mut temp.env(false), user_id, 0, 1, p4);
        um.on_get_user_photos(user_id, 0, 1, 1, vec![telegram_photo(51)]);
        um.drop_user_photos(user_id, true, "test");
        let cache = um.user_photos.get(&5).unwrap();
        assert_eq!(cache.count, 0);
        assert_eq!(cache.offset, 0);
        assert!(cache.photos.is_empty());
    }

    #[test]
    fn photos_retry_machine_errors_after_three_attempts() {
        let (mut temp, mut um) = test_env();
        let user_id = seed_received_user(&mut temp, &mut um, 5);

        // Request [0, 5) of a count-5 listing whose intake only fills
        // [0, 2) — never answerable from the cache.
        let (p, a) = recording_promise::<td_api::ChatPhotos>();
        um.get_user_profile_photos(&mut temp.env(false), user_id, 0, 5, p);
        um.on_get_user_photos(
            user_id,
            0,
            5,
            5,
            vec![telegram_photo(61), telegram_photo(62)],
        );
        um.on_get_user_profile_photos(&temp.env(false), user_id, Ok(()));
        assert!(a.lock().unwrap().is_empty(), "no answer yet");
        assert_eq!(
            um.pending_get_user_photos_requests.get(&5).unwrap().len(),
            1
        );

        um.on_get_user_profile_photos(&temp.env(false), user_id, Ok(()));
        assert!(a.lock().unwrap().is_empty());
        um.on_get_user_profile_photos(&temp.env(false), user_id, Ok(()));
        assert_eq!(
            a.lock().unwrap().as_slice(),
            [Err("Failed to get profile photos".to_owned())]
        );
        assert!(!um.pending_get_user_photos_requests.contains_key(&5));

        // The dropped-result branch: an unknown count resends; the third
        // resend failure answers "Failed to return profile photos". The
        // request must park first (offset 3 is outside the [0, 2)
        // cache window).
        let (p2, a2) = recording_promise::<td_api::ChatPhotos>();
        um.get_user_profile_photos(&mut temp.env(false), user_id, 3, 1, p2);
        // force the cache to unknown between completions (the C++ drops
        // the received result when the cache was reset meanwhile)
        um.drop_user_photos(user_id, false, "test");
        um.on_get_user_profile_photos(&temp.env(false), user_id, Ok(()));
        assert!(a2.lock().unwrap().is_empty());
        um.drop_user_photos(user_id, false, "test");
        um.on_get_user_profile_photos(&temp.env(false), user_id, Ok(()));
        assert!(a2.lock().unwrap().is_empty());
        um.drop_user_photos(user_id, false, "test");
        um.on_get_user_profile_photos(&temp.env(false), user_id, Ok(()));
        assert_eq!(
            a2.lock().unwrap().as_slice(),
            [Err("Failed to return profile photos".to_owned())]
        );

        // An error result fans out to every parked request (offsets
        // outside the [0, 2) cache window park).
        let (p3, a3) = recording_promise::<td_api::ChatPhotos>();
        let (p4, a4) = recording_promise::<td_api::ChatPhotos>();
        um.get_user_profile_photos(&mut temp.env(false), user_id, 3, 1, p3);
        um.get_user_profile_photos(&mut temp.env(false), user_id, 4, 1, p4);
        um.on_get_user_profile_photos(
            &temp.env(false),
            user_id,
            Err(Status::error(400, "PHOTOS_INVALID")),
        );
        assert_eq!(
            a3.lock().unwrap().as_slice(),
            [Err("PHOTOS_INVALID".to_owned())]
        );
        assert_eq!(
            a4.lock().unwrap().as_slice(),
            [Err("PHOTOS_INVALID".to_owned())]
        );
    }

    #[test]
    fn bot_pending_user_photo_holds_and_applies() {
        let (mut temp, mut um) = TempEnv::with_chat_info_db("unit", false);
        assert!(!temp.global.use_chat_info_database());
        // A bare user — `on_get_user` would apply the (empty) photo and
        // set is_photo_inited already; the hold applies to users whose
        // photo was never applied (e.g. database-loaded records).
        let user_id = UserId::new(5);
        um.add_user(user_id);

        // The bot-without-chat-info-database path holds the photo.
        um.on_update_user_photo(
            &mut temp.env(true),
            user_id,
            telegram_api::UserProfilePhoto::userProfilePhoto(telegram_api::userProfilePhoto {
                flags: 0,
                has_video: false,
                photo_id: 400,
                stripped_thumb: vec![1, 2, 3],
                dc_id: 2,
                personal: false,
            }),
            "test",
        );
        assert!(!um.get_user(user_id).unwrap().is_photo_inited);
        assert!(um.pending_user_photos.contains_key(&5));
        // drop_user_photos on a missing cache entry is a no-op (cpp:9521).
        assert!(!um.user_photos.contains_key(&5));

        // A duplicate photo id does not clobber the held one.
        um.on_update_user_photo(
            &mut temp.env(true),
            user_id,
            telegram_api::UserProfilePhoto::userProfilePhoto(telegram_api::userProfilePhoto {
                flags: 0,
                has_video: false,
                photo_id: 400,
                stripped_thumb: vec![9, 9],
                dc_id: 2,
                personal: false,
            }),
            "test",
        );
        assert_eq!(
            um.pending_user_photos
                .get(&5)
                .map(get_profile_photo_id)
                .unwrap(),
            400
        );

        // The first listing/getter applies the held photo and sends
        // updateUser.
        um.apply_pending_user_photo(&mut temp.env(true), user_id, "test");
        let u = um.get_user(user_id).unwrap();
        assert!(u.is_photo_inited);
        assert_eq!(u.photo.id, 400);
        assert!(um.pending_user_photos.is_empty());
        assert!(
            um.take_updates()
                .iter()
                .any(|update| matches!(update, td_api::Update::updateUser(u) if u.user.as_ref().is_some_and(|user| user.id == 5)))
        );
    }

    #[test]
    fn saved_music_list_cache_and_machines() {
        let (mut temp, mut um) = test_env();
        // My user must be known (on_get_user self sets my_id).
        let mut me = telegram_user(77, "Me", USER_ACCESS_HASH_MASK | (1 << 1) | USER_PHONE_MASK);
        me.phone = "+15550001111".to_owned();
        me.self_ = true;
        um.on_get_user(&mut temp.env(false), telegram_api::User::user(me), "test");
        let me_id = um.get_my_id();
        assert_eq!(me_id.get(), 77);

        // start_up's saved-music block stand-in: the authorized session
        // has the (empty) persisted list, so the set is inited. The
        // UserFull record pre-exists (an absent `usf` row creates none,
        // cpp:9368-9370 — the first-saved-music update needs the record).
        um.save_my_saved_music_ids(&mut temp.env(false));
        assert!(um.are_my_saved_music_ids_inited);
        um.add_user_full(me_id);

        // A listing request parks; the intake fills the cache and the
        // registry, updates my saved-music ids (persisted) and the
        // UserFull first-saved-music field.
        let (p, a) = recording_promise::<td_api::Audios>();
        um.get_user_saved_music(&mut temp.env(false), me_id, 0, 10, p);
        assert_eq!(
            um.pending_get_user_saved_music_requests
                .get(&77)
                .unwrap()
                .len(),
            1
        );

        um.on_get_user_saved_music(
            &mut temp.env(false),
            me_id,
            0,
            10,
            3,
            vec![
                audio_document(101, 11),
                audio_document(102, 12),
                document_without_audio(103),
            ],
        );
        um.finish_get_user_saved_music(&temp.env(false), me_id, Ok(()));
        // The non-audio document dropped the count by one; the cache
        // holds the two audio files; the answer carries the count.
        let cache = um.user_saved_music.get(&77).unwrap();
        assert_eq!(cache.count, 2);
        assert_eq!(cache.offset, 0);
        assert_eq!(cache.saved_music_file_ids.len(), 2);
        let answer = a.lock().unwrap().pop().unwrap();
        assert!(
            matches!(answer, Ok(audios) if audios.total_count == 2 && audios.audios.len() == 2)
        );

        // my_saved_music_ids got the document ids and were persisted.
        assert!(um.my_saved_music_ids.contains(&101));
        assert!(um.my_saved_music_ids.contains(&102));
        assert!(!um.my_saved_music_ids.contains(&103));
        assert!(um.are_my_saved_music_ids_inited);
        let raw = temp
            .global
            .binlog_pmc()
            .get_raw(b"my_saved_music_ids")
            .to_vec();
        assert_eq!(raw.len(), 4 + 8 * 2);

        // The UserFull first-saved-music points at the cache head.
        let file_ids = cache.saved_music_file_ids.clone();
        let user_full = um.users_full.get(&77).unwrap();
        assert_eq!(user_full.first_saved_music_file_id, file_ids[0]);
        assert!(!user_full.is_first_saved_music_file_id_changed);

        // on_add_saved_music with no anchor inserts at the head.
        let new_file = um
            .parse_saved_music_document(audio_document(104, 13))
            .unwrap();
        let (mut pa, aa) = recording_promise::<()>();
        um.on_add_saved_music(&mut temp.env(false), new_file, FileId::default(), &mut pa);
        assert!(matches!(aa.lock().unwrap().as_slice()[0], Ok(())));
        let cache = um.user_saved_music.get(&77).unwrap();
        assert_eq!(cache.count, 3);
        assert_eq!(cache.saved_music_file_ids[0], new_file);
        assert!(um.my_saved_music_ids.contains(&104));

        // Removing the current head (the newly added file) re-points
        // first_saved_music to the next entry.
        let (mut pr, ar) = recording_promise::<()>();
        um.on_remove_saved_music(&mut temp.env(false), new_file, &mut pr);
        assert!(matches!(ar.lock().unwrap().as_slice()[0], Ok(())));
        let cache = um.user_saved_music.get(&77).unwrap();
        assert_eq!(cache.count, 2);
        assert_eq!(cache.saved_music_file_ids[0], file_ids[0]);
        assert!(!um.my_saved_music_ids.contains(&104));
        let user_full = um.users_full.get(&77).unwrap();
        assert_eq!(user_full.first_saved_music_file_id, file_ids[0]);

        // An anchored add inserts after the anchor; a missing anchor
        // drops the cache.
        let another = um
            .parse_saved_music_document(audio_document(105, 14))
            .unwrap();
        let (mut pa2, _aa2) = recording_promise::<()>();
        um.on_add_saved_music(&mut temp.env(false), another, file_ids[0], &mut pa2);
        let cache = um.user_saved_music.get(&77).unwrap();
        assert_eq!(cache.saved_music_file_ids[1], another);

        let stray = um
            .parse_saved_music_document(audio_document(106, 15))
            .unwrap();
        let (mut pa3, _aa3) = recording_promise::<()>();
        um.on_add_saved_music(&mut temp.env(false), stray, FileId::new(9999, 0), &mut pa3);
        let cache = um.user_saved_music.get(&77).unwrap();
        assert_eq!(cache.count, -1, "missing anchor drops the cache");
        assert_eq!(cache.offset, -1);
    }

    #[test]
    fn saved_music_validation_and_check() {
        let (mut temp, mut um) = test_env();
        let mut me = telegram_user(77, "Me", USER_ACCESS_HASH_MASK | (1 << 1));
        me.self_ = true;
        um.on_get_user(&mut temp.env(false), telegram_api::User::user(me), "test");

        // A registered audio document passes; invalid/unknown ids answer
        // the exact errors.
        let file_id = um
            .parse_saved_music_document(audio_document(201, 21))
            .unwrap();
        let (p1, a1) = recording_promise::<()>();
        um.add_saved_music(FileId::default(), FileId::default(), p1);
        assert_eq!(
            a1.lock().unwrap().as_slice(),
            [Err("Invalid file identifier specified".to_owned())]
        );
        let (p2, a2) = recording_promise::<()>();
        um.add_saved_music(FileId::new(999, 0), FileId::default(), p2);
        assert_eq!(
            a2.lock().unwrap().as_slice(),
            [Err("Unknown file identifier specified".to_owned())]
        );
        let (p3, a3) = recording_promise::<()>();
        um.add_saved_music(file_id, file_id, p3);
        assert_eq!(
            a3.lock().unwrap().as_slice(),
            [Err("Can't place saved music after self".to_owned())]
        );

        // check_is_saved_music answers over the loaded set.
        um.my_saved_music_ids.insert(201);
        um.are_my_saved_music_ids_inited = true;
        let (mut p4, a4) = recording_promise::<()>();
        um.check_is_saved_music(&temp.env(false), file_id, &mut p4);
        assert!(matches!(a4.lock().unwrap().as_slice()[0], Ok(())));
        let other = um
            .parse_saved_music_document(audio_document(202, 22))
            .unwrap();
        let (mut p5, a5) = recording_promise::<()>();
        um.check_is_saved_music(&temp.env(false), other, &mut p5);
        assert_eq!(a5.lock().unwrap().as_slice(), [Err("Not Found".to_owned())]);

        // reload_my_saved_music_list parks the promise (the send is
        // net-inert at the manager level); the ok intake replaces the
        // set and completes it.
        let (p6, a6) = recording_promise::<()>();
        um.reload_my_saved_music_list(p6);
        um.on_get_my_saved_music_list(
            &mut temp.env(false),
            Ok(telegram_api::account_SavedMusicIds::account_savedMusicIds(
                telegram_api::account_savedMusicIds {
                    ids: vec![0, 202, 202, 203],
                },
            )),
        );
        assert!(matches!(a6.lock().unwrap().as_slice()[0], Ok(())));
        assert_eq!(
            um.my_saved_music_ids.iter().copied().collect::<Vec<i64>>(),
            vec![202, 203],
            "zero ids and duplicates are dropped"
        );
        assert!(
            !temp
                .global
                .binlog_pmc()
                .get_raw(b"my_saved_music_ids")
                .is_empty()
        );

        // The not-modified form keeps the set.
        let (p7, a7) = recording_promise::<()>();
        um.reload_my_saved_music_list(p7);
        um.on_get_my_saved_music_list(
            &mut temp.env(false),
            Ok(
                telegram_api::account_SavedMusicIds::account_savedMusicIdsNotModified(
                    telegram_api::account_savedMusicIdsNotModified {},
                ),
            ),
        );
        assert!(matches!(a7.lock().unwrap().as_slice()[0], Ok(())));
        assert_eq!(um.my_saved_music_ids.len(), 2);

        // The error arm fails the queue (the C++ on_error →
        // fail_promises hop).
        let (p8, a8) = recording_promise::<()>();
        um.reload_my_saved_music_list(p8);
        um.on_get_my_saved_music_list(
            &mut temp.env(false),
            Err(Status::error(429, "Too Many Requests")),
        );
        assert_eq!(
            a8.lock().unwrap().as_slice(),
            [Err("Too Many Requests".to_owned())]
        );
        assert!(um.reload_my_saved_music_queries.is_empty(), "drained");
    }

    #[test]
    fn on_get_user_full_applies_photos_and_saved_music() {
        let (mut temp, mut um) = test_env();
        let user_id = seed_received_user(&mut temp, &mut um, 5);

        um.on_get_user_full(
            &mut temp.env(false),
            telegram_api::UserFull {
                id: 5,
                flags: 1 << 2,   // profile_photo
                flags2: 1 << 21, // saved_music
                profile_photo: Some(telegram_photo(55)),
                personal_photo: Some(telegram_api::Photo::photoEmpty(telegram_api::photoEmpty {
                    id: 0,
                })),
                fallback_photo: Some(telegram_api::Photo::photoEmpty(telegram_api::photoEmpty {
                    id: 0,
                })),
                saved_music: Some(audio_document(300, 30)),
                ..telegram_api::UserFull::default()
            },
        );

        // The User photo and UserFull photo carry the id; the photo id
        // registers.
        assert_eq!(um.get_user(user_id).unwrap().photo.id, 55);
        assert!(um.get_user(user_id).unwrap().photo_ids.contains(&55));
        assert_eq!(um.users_full.get(&5).unwrap().photo.id, 55);
        // The saved-music document became the first-saved-music file id.
        let first = um.users_full.get(&5).unwrap().first_saved_music_file_id;
        assert!(first.is_valid());
        assert_eq!(um.get_saved_music_document_id(first, false).0, 300);

        // An empty userFull photo set drops the photos cache to empty
        // and the saved-music cache too (cpp:9299-9310).
        um.user_photos.entry(5).or_default();
        um.user_saved_music.entry(5).or_default();
        um.on_get_user_full(
            &mut temp.env(false),
            telegram_api::UserFull {
                id: 5,
                ..telegram_api::UserFull::default()
            },
        );
        assert_eq!(um.user_photos.get(&5).unwrap().count, 0);
        assert_eq!(um.user_saved_music.get(&5).unwrap().count, 0);
        assert!(
            !um.users_full
                .get(&5)
                .unwrap()
                .first_saved_music_file_id
                .is_valid()
        );
    }

    // ------------------------------------------------------------------
    // Contacts (M.4.14)

    fn telegram_contact(id: i64, first_name: &str, last_name: &str) -> telegram_api::user {
        telegram_api::user {
            // access_hash, first_name, last_name, contact
            flags: USER_ACCESS_HASH_MASK | (1 << 1) | (1 << 2) | (1 << 11),
            contact: true,
            id,
            access_hash: 123,
            first_name: first_name.to_owned(),
            last_name: last_name.to_owned(),
            ..telegram_api::user::default()
        }
    }

    fn contacts_list(
        saved_count: i32,
        users: Vec<telegram_api::user>,
    ) -> telegram_api::contacts_Contacts {
        telegram_api::contacts_Contacts::contacts_contacts(telegram_api::contacts_contacts {
            contacts: Vec::new(),
            saved_count,
            users: users.into_iter().map(telegram_api::User::user).collect(),
        })
    }

    /// A `Promise<()>` that flips an observable flag on completion (the
    /// `PromiseCreator` closures are `'static`).
    fn promise_flag() -> (Promise<()>, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag_clone = flag.clone();
        (
            PromiseCreator::lambda(move |_: ()| {
                flag_clone.store(true, std::sync::atomic::Ordering::SeqCst)
            }),
            flag,
        )
    }

    /// The `promise_result` slots (a type-complexity clippy alias).
    type Slots = (
        Promise<()>,
        std::sync::Arc<std::sync::Mutex<Option<tdutils::status::Result<()>>>>,
    );
    /// A `Promise<()>` capturing its `Result` into a shared slot.
    fn promise_result() -> Slots {
        let result = std::sync::Arc::new(std::sync::Mutex::new(None));
        let result_clone = result.clone();
        (
            PromiseCreator::lambda_result(move |r| *result_clone.lock().unwrap() = Some(r)),
            result,
        )
    }

    /// The typed `promise_result` slots (a type-complexity clippy
    /// alias, generic over the promise payload).
    type TypedSlots<T> = (
        Promise<T>,
        std::sync::Arc<std::sync::Mutex<Option<tdutils::status::Result<T>>>>,
    );
    /// The typed `promise_result`: a `Promise<T>` capturing its `Result`
    /// into a shared slot.
    fn typed_promise_result<T: Send + 'static>() -> TypedSlots<T> {
        let result = std::sync::Arc::new(std::sync::Mutex::new(None));
        let result_clone = result.clone();
        (
            PromiseCreator::lambda_result(move |r| *result_clone.lock().unwrap() = Some(r)),
            result,
        )
    }

    /// Loads the contacts (marking `are_contacts_loaded_`) through the
    /// `on_get_contacts` intake, like the first server answer would.
    fn load_contacts_via_on_get_contacts(
        um: &mut UserManager,
        env: &mut UserEnv,
        users: Vec<telegram_api::user>,
        saved_count: i32,
    ) {
        let unix_time = env.unix_time();
        um.on_get_contacts(env, contacts_list(saved_count, users));
        assert!(um.are_contacts_loaded);
        assert_eq!(um.saved_contact_count, saved_count);
        assert!(
            (unix_time + 70_000..=unix_time + 100_000).contains(&um.next_contacts_sync_date),
            "the next sync date is a Random::fast(70000, 100000) draw"
        );
    }

    #[test]
    fn contacts_constructor_block_restores_the_binlog_pmc_pair() {
        let (mut temp, mut um) = test_env();

        // a fresh manager with empty keys keeps the defaults
        let unix_time = temp.global.unix_time();
        let mut um2 = UserManager::new();
        um2.init_contacts_state(temp.global.binlog_pmc(), unix_time, true, false);
        assert_eq!(um2.next_contacts_sync_date, 0);
        assert_eq!(um2.saved_contact_count, -1);

        temp.global.binlog_pmc().set(
            "next_contacts_sync_date",
            &(unix_time + 200_000).to_string(),
        );
        temp.global.binlog_pmc().set("saved_contact_count", "42");

        um.init_contacts_state(temp.global.binlog_pmc(), unix_time, true, false);
        // the C++ clamps to unix_time + 100000
        assert_eq!(um.next_contacts_sync_date, unix_time + 100_000);
        assert_eq!(um.saved_contact_count, 42);
        assert!(!um.are_contacts_loaded);
    }

    #[test]
    fn on_get_contacts_syncs_contact_flags_and_rebuilds_the_hints() {
        let (mut temp, mut um) = test_env();
        um.set_my_id(&mut temp.env(false), UserId::new(1));

        // user 2 is a known contact, user 3 a known non-contact; the
        // server now answers that only 2 (with a fresh name) and the new
        // 4 are contacts
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_contact(2, "Old", "Name")),
            "test",
        );
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(3, "Nobody", USER_ACCESS_HASH_MASK | (1 << 1))),
            "test",
        );

        load_contacts_via_on_get_contacts(
            &mut um,
            &mut temp.env(false),
            vec![
                telegram_contact(2, "Alice", "Cooper"),
                telegram_contact(4, "Bob", "Dylan"),
            ],
            2,
        );

        assert!(um.get_user(UserId::new(2)).unwrap().is_contact);
        assert_eq!(um.get_user(UserId::new(2)).unwrap().first_name, "Alice");
        assert!(um.get_user(UserId::new(4)).unwrap().is_contact);
        assert!(!um.get_user(UserId::new(3)).unwrap().is_contact);
        assert!(um.contacts_hints.has_key(2));
        assert!(um.contacts_hints.has_key(4));
        assert!(!um.contacts_hints.has_key(3));
        // the search text carries the updated name
        assert_eq!(um.contacts_hints.key_to_string(2), "Alice Cooper ");
        assert!(
            um.take_updates()
                .iter()
                .any(|update| matches!(update, td_api::Update::updateUser(u) if u.user.as_ref().is_some_and(|user| user.id == 2)))
        );
    }

    #[test]
    fn contacts_hints_search_names_usernames_and_the_hash() {
        let (mut temp, mut um) = test_env();
        um.set_my_id(&mut temp.env(false), UserId::new(1));

        let users = vec![
            telegram_contact(2, "Alice", "Cooper"),
            telegram_contact(3, "Bob", "Dylan"),
            telegram_contact(4, "Alice", "Bobson"),
        ];
        for user in &users {
            um.on_get_user(
                &mut temp.env(false),
                telegram_api::User::user(user.clone()),
                "test",
            );
        }

        load_contacts_via_on_get_contacts(&mut um, &mut temp.env(false), users, 3);

        // the usernames arrive after the contacts list (an update, not a
        // user re-application, so the list can't wipe them)
        um.on_update_user_usernames(
            &mut temp.env(false),
            UserId::new(2),
            Usernames::from_telegram("alice_cooper".to_owned(), Vec::new()),
        );

        // the usernames join the search text (the C++ implode — no
        // trailing separator after the last username)
        assert_eq!(
            um.contacts_hints.key_to_string(2),
            "Alice Cooper alice_cooper"
        );

        let (count, ids) =
            um.search_contacts(&mut temp.env(false), "alice", 10, Promise::default());
        assert_eq!(count, 2, "Alice Cooper and Alice Bobson");
        assert_eq!(ids, vec![UserId::new(2), UserId::new(4)]);

        // the username-only word finds Alice Cooper
        let (_, ids) = um.search_contacts(&mut temp.env(false), "cooper", 10, Promise::default());
        assert_eq!(ids, vec![UserId::new(2)]);

        // the empty query lists every contact, truncated by the limit
        let (count, ids) = um.search_contacts(&mut temp.env(false), "", 2, Promise::default());
        assert_eq!(count, 3);
        assert_eq!(ids.len(), 2);

        // the close friends are the contacts with the close-friend flag
        um.on_update_user_is_contact_impl(&mut temp.env(false), UserId::new(3), true, true, true);
        um.update_user(&mut temp.env(false), UserId::new(3), false, false);
        let close_friends = um.get_close_friends(&mut temp.env(false), Promise::default());
        assert_eq!(close_friends, vec![UserId::new(3)]);

        // the hash folds the saved count and the sorted contact ids
        let hash = um.get_contacts_hash(&mut temp.env(false));
        let numbers: Vec<u64> = vec![3, 2, 3, 4];
        assert_eq!(hash, get_vector_hash(&numbers));
    }

    #[test]
    fn contacts_persist_and_restore_across_a_database_reopen() {
        let dir = std::env::temp_dir()
            .join(format!(
                "td-rs-m414-contacts-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Phase 1: contacts arrive, then the save lands in the binlog pmc
        // and the `user_contacts` row.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                ..Default::default()
            })
            .unwrap();
            let mut global = Global::new();
            global.init(opened.database);
            global.binlog_pmc().set("auth", "ok");
            let mut options = OptionManager::new(global.is_test_dc());
            let mut um = UserManager::new();
            um.init_my_id(global.binlog_pmc());
            let unix_time = global.unix_time();
            um.init_contacts_state(global.binlog_pmc(), unix_time, true, false);
            let mut env = UserEnv {
                global: &mut global,
                options: &mut options,
                auth_is_bot: false,
            };
            um.set_my_id(&mut env, UserId::new(1));
            let contact_2 = telegram_contact(2, "Alice", "Cooper");
            let contact_3 = telegram_contact(3, "Bob", "Dylan");
            um.on_get_user(
                &mut env,
                telegram_api::User::user(contact_2.clone()),
                "test",
            );
            um.on_get_user(
                &mut env,
                telegram_api::User::user(contact_3.clone()),
                "test",
            );
            load_contacts_via_on_get_contacts(&mut um, &mut env, vec![contact_2, contact_3], 2);
            um.save_contacts_to_database(&mut env);
            assert_eq!(
                global.binlog_pmc().get("saved_contact_count"),
                "2",
                "the count rides the binlog pmc"
            );
            assert!(!global.td_db().common_kv().get(b"user_contacts").is_empty());
            drop(options);
            global.close_all(false);
        }

        // Phase 2: reopen — the pmc pair restores and `load_contacts` reads
        // the row, reindexes the hints and resolves the queue.
        {
            let opened = tddb::td_db::TdDb::open(tddb::td_db::Parameters {
                database_directory: dir.clone(),
                ..Default::default()
            })
            .unwrap();
            let mut global = Global::new();
            global.init(opened.database);
            let mut options = OptionManager::new(global.is_test_dc());
            let mut um = UserManager::new();
            um.init_my_id(global.binlog_pmc());
            let unix_time = global.unix_time();
            um.init_contacts_state(global.binlog_pmc(), unix_time, true, false);
            assert_eq!(um.saved_contact_count, 2);
            assert!(um.next_contacts_sync_date > 0);
            let mut env = UserEnv {
                global: &mut global,
                options: &mut options,
                auth_is_bot: false,
            };
            let (promise, loaded) = promise_flag();
            um.load_contacts(&mut env, promise);
            assert!(
                loaded.load(std::sync::atomic::Ordering::SeqCst),
                "the load resolves synchronously from the row"
            );
            assert!(um.are_contacts_loaded);
            assert_eq!(um.contacts_hints.size(), 2);
            assert!(um.contacts_hints.has_key(2));
            assert!(um.contacts_hints.has_key(3));
            assert!(um.get_user(UserId::new(2)).unwrap().is_contact);
            drop(options);
            global.close_all(false);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_contacts_state_machine_reserves_replays_and_retries() {
        let (mut temp, mut um) = test_env();
        um.set_my_id(&mut temp.env(false), UserId::new(1));
        load_contacts_via_on_get_contacts(&mut um, &mut temp.env(false), Vec::new(), 0);

        let contact_a = Contact::new(
            "+123".to_owned(),
            "A".to_owned(),
            String::new(),
            String::new(),
            UserId::default(),
        );
        let contact_b = Contact::new(
            "+456".to_owned(),
            "B".to_owned(),
            String::new(),
            String::new(),
            UserId::default(),
        );
        let mut random_id = 0i64;
        let (promise, finished) = promise_flag();
        let (user_ids, invites) = um.import_contacts(
            &mut temp.env(false),
            vec![contact_a.clone(), contact_b.clone()],
            &mut random_id,
            promise,
        );
        assert!(user_ids.is_empty());
        assert!(invites.is_empty());
        assert_ne!(random_id, 0, "the slot id is generated");
        assert!(um.imported_contacts.contains_key(&random_id));
        assert!(um.import_contact_tasks.contains_key(&random_id));
        assert!(
            !finished.load(std::sync::atomic::Ordering::SeqCst),
            "the query result has not arrived yet"
        );

        // a retry answer keeps the task alive
        um.on_imported_contacts(
            &mut temp.env(false),
            random_id,
            Ok(telegram_api::contacts_ImportedContacts {
                imported: vec![telegram_api::ImportedContact {
                    user_id: 10,
                    client_id: 0,
                }],
                popular_invites: Vec::new(),
                retry_contacts: vec![1],
                users: Vec::new(),
            }),
        );
        assert!(um.import_contact_tasks.contains_key(&random_id));

        // the final answer fills the buffers and the slot
        um.on_imported_contacts(
            &mut temp.env(false),
            random_id,
            Ok(telegram_api::contacts_ImportedContacts {
                imported: vec![
                    telegram_api::ImportedContact {
                        user_id: 10,
                        client_id: 0,
                    },
                    telegram_api::ImportedContact {
                        user_id: 20,
                        client_id: 1,
                    },
                ],
                popular_invites: vec![telegram_api::PopularContact {
                    client_id: 1,
                    importers: 5,
                }],
                retry_contacts: Vec::new(),
                users: Vec::new(),
            }),
        );
        assert!(!um.import_contact_tasks.contains_key(&random_id));
        assert!(finished.load(std::sync::atomic::Ordering::SeqCst));

        // the replay returns the stored result
        let (promise, replay_done) = promise_flag();
        let (user_ids, invites) =
            um.import_contacts(&mut temp.env(false), Vec::new(), &mut random_id, promise);
        assert_eq!(user_ids, vec![UserId::new(10), UserId::new(20)]);
        assert_eq!(invites, vec![0, 5]);
        assert!(replay_done.load(std::sync::atomic::Ordering::SeqCst));

        // an error answer fails the promise and drops the task
        let mut random_id2 = 0i64;
        let (promise, error) = promise_result();
        um.import_contacts(
            &mut temp.env(false),
            vec![contact_a],
            &mut random_id2,
            promise,
        );
        um.on_imported_contacts(
            &mut temp.env(false),
            random_id2,
            Err(Status::error(500, "SERVICE_UNAVAILABLE")),
        );
        let error = error.lock().unwrap().take().expect("the promise failed");
        assert!(error.is_error());
        assert_eq!(error.error().message(), "SERVICE_UNAVAILABLE");
        assert!(!um.import_contact_tasks.contains_key(&random_id2));
    }

    #[test]
    fn change_imported_contacts_dedups_and_replays_the_change_result() {
        let (mut temp, mut um) = test_env();
        um.set_my_id(&mut temp.env(false), UserId::new(1));
        load_contacts_via_on_get_contacts(&mut um, &mut temp.env(false), Vec::new(), 0);

        // the imported users exist (their rows feed the force-load below)
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(
                11,
                "Old A",
                USER_ACCESS_HASH_MASK | (1 << 1) | (1 << 2),
            )),
            "test",
        );
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(
                12,
                "Old B",
                USER_ACCESS_HASH_MASK | (1 << 1) | (1 << 2),
            )),
            "test",
        );

        // the persisted imported-contacts row: two old contacts
        let old_a = Contact::new(
            "+1".to_owned(),
            "Old A".to_owned(),
            String::new(),
            String::new(),
            UserId::new(11),
        );
        let old_b = Contact::new(
            "+2".to_owned(),
            "Old B".to_owned(),
            String::new(),
            String::new(),
            UserId::new(12),
        );
        let mut blob = Vec::new();
        store_contacts_log_event(&mut blob, &[old_a.clone(), old_b.clone()]);
        temp.global
            .td_db()
            .common_kv()
            .set(b"user_imported_contacts", &blob);
        let (promise, loaded) = promise_flag();
        um.load_imported_contacts(&mut temp.env(false), promise);
        assert!(loaded.load(std::sync::atomic::Ordering::SeqCst));
        assert!(um.are_imported_contacts_loaded);
        assert_eq!(um.all_imported_contacts.len(), 2);

        // the change: a duplicated pair, one match (keeps the old user id),
        // one new contact and old B deleted
        let keep_a = Contact::new(
            "+1".to_owned(),
            "Old A".to_owned(),
            String::new(),
            String::new(),
            UserId::default(),
        );
        let new_c = Contact::new(
            "+3".to_owned(),
            "New C".to_owned(),
            String::new(),
            String::new(),
            UserId::default(),
        );
        let mut contacts = vec![keep_a.clone(), keep_a.clone(), new_c.clone()];
        let mut random_id = 0i64;
        let (user_ids, invites) = um.change_imported_contacts(
            &mut temp.env(false),
            &mut contacts,
            &mut random_id,
            Promise::default(),
        );
        assert!(
            user_ids.is_empty(),
            "the change needs the removal + import roundtrip"
        );
        assert!(invites.is_empty());
        assert_eq!(random_id, 1);
        assert!(um.are_imported_contacts_changing);

        // a concurrent change is rejected
        let mut contacts2 = vec![new_c.clone()];
        let mut random_id2 = 0i64;
        let (promise, error) = promise_result();
        um.change_imported_contacts(
            &mut temp.env(false),
            &mut contacts2,
            &mut random_id2,
            promise,
        );
        let error = error.lock().unwrap().take().expect("the promise failed");
        let error = error.error().clone();
        assert_eq!(error.code(), 400);
        assert_eq!(
            error.message(),
            "ChangeImportedContacts can be called only once at the same time"
        );

        // the removal continuation hands the unique contacts to the import
        let unique_contacts = vec![keep_a.clone(), new_c.clone()];
        let contacts_unique_id = vec![0usize, 0usize, 1usize];
        let to_add = (vec![0usize, 1usize], unique_contacts);
        let (promise, change_done) = promise_flag();
        um.on_clear_imported_contacts(
            &mut temp.env(false),
            vec![keep_a.clone(), new_c.clone()],
            contacts_unique_id,
            to_add,
            promise,
        );
        assert!(um.import_contact_tasks.contains_key(&1i64));
        assert!(
            !change_done.load(std::sync::atomic::Ordering::SeqCst),
            "the import query has not answered yet"
        );

        // the import answer fills the task buffers, resolves the promise
        // and finishes the change bookkeeping
        um.on_imported_contacts(
            &mut temp.env(false),
            1,
            Ok(telegram_api::contacts_ImportedContacts {
                imported: vec![
                    telegram_api::ImportedContact {
                        user_id: 11,
                        client_id: 0,
                    },
                    telegram_api::ImportedContact {
                        user_id: 31,
                        client_id: 1,
                    },
                ],
                popular_invites: Vec::new(),
                retry_contacts: Vec::new(),
                users: Vec::new(),
            }),
        );
        assert!(change_done.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(um.all_imported_contacts.len(), 2);
        assert_eq!(um.all_imported_contacts[0].get_user_id(), UserId::new(11));
        assert_eq!(um.all_imported_contacts[1].get_user_id(), UserId::new(31));
        assert_eq!(um.imported_contact_user_ids.len(), 3);
        assert_eq!(
            um.imported_contact_user_ids,
            vec![UserId::new(11), UserId::new(11), UserId::new(31)]
        );
        assert_eq!(um.unimported_contact_invites, vec![0, 0, 0]);
        assert!(
            !temp
                .global
                .td_db()
                .common_kv()
                .get(b"user_imported_contacts")
                .is_empty()
        );

        // the replay returns the change results and clears the gate
        let mut contacts3 = Vec::new();
        let (promise, replay_done) = promise_flag();
        let (user_ids, invites) = um.change_imported_contacts(
            &mut temp.env(false),
            &mut contacts3,
            &mut random_id,
            promise,
        );
        assert_eq!(
            user_ids,
            vec![UserId::new(11), UserId::new(11), UserId::new(31)]
        );
        assert_eq!(invites, vec![0, 0, 0]);
        assert!(replay_done.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!um.are_imported_contacts_changing);

        // a fully-matching change is a no-op that answers immediately
        let mut contacts4 = vec![keep_a.clone(), new_c.clone()];
        let mut random_id4 = 0i64;
        let (promise, noop_done) = promise_flag();
        let (user_ids, invites) = um.change_imported_contacts(
            &mut temp.env(false),
            &mut contacts4,
            &mut random_id4,
            promise,
        );
        assert_eq!(user_ids, vec![UserId::new(11), UserId::new(31)]);
        assert_eq!(invites, vec![0, 0]);
        assert!(noop_done.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(random_id4, 0, "no roundtrip was started");
    }

    #[test]
    fn contact_birthdates_update_the_full_and_emit_close_birthdays() {
        let (mut temp, mut um) = test_env();
        um.set_my_id(&mut temp.env(false), UserId::new(1));
        let contact = telegram_contact(2, "Alice", "Cooper");
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(contact.clone()),
            "test",
        );
        load_contacts_via_on_get_contacts(&mut um, &mut temp.env(false), vec![contact], 1);
        um.add_user_full(UserId::new(2));

        um.reload_contact_birthdates(&mut temp.env(false), true);
        assert!(um.contact_birthdates.is_being_synced);

        let before = um.contact_birthdates.next_sync_time;
        um.on_get_contact_birthdates(
            &mut temp.env(false),
            Some(telegram_api::contacts_ContactBirthdays {
                contacts: vec![telegram_api::ContactBirthday {
                    contact_id: 2,
                    birthday: Some(telegram_api::Birthday {
                        flags: 1,
                        day: 3,
                        month: 4,
                        year: 1990,
                    }),
                }],
                users: Vec::new(),
            }),
        );
        assert!(!um.contact_birthdates.is_being_synced);
        assert!(um.contact_birthdates.next_sync_time > before);
        let user_full = um
            .get_user_full(UserId::new(2))
            .expect("force-loaded above");
        assert_eq!(user_full.birthdate, Birthdate::new(3, 4, 1990));
        assert!(um.take_updates().iter().any(|update| matches!(
            update,
            td_api::Update::updateContactCloseBirthdays(u)
                if u.close_birthday_users.len() == 1 && u.close_birthday_users[0].user_id == 2
                && u.close_birthday_users[0].birthdate.as_ref().is_some_and(|birthdate| birthdate.day == 3)
        )));

        // the dismiss option swaps the next answer for an empty one — the
        // users list clears and an empty update fires
        temp.options
            .set_option_boolean("dismiss_birthday_contact_today", true);
        um.reload_contact_birthdates(&mut temp.env(false), true);
        assert!(
            !um.contact_birthdates.need_drop,
            "the drop ran through the nullptr intake"
        );
        assert!(!um.contact_birthdates.is_being_synced);
        assert!(um.contact_birthdates.users.is_empty());
        assert!(
            um.take_updates()
                .iter()
                .any(|update| matches!(update, td_api::Update::updateContactCloseBirthdays(u) if u.close_birthday_users.is_empty()))
        );
    }

    #[test]
    fn search_user_by_phone_number_uses_the_resolved_cache() {
        let (mut temp, mut um) = test_env();
        um.set_my_id(&mut temp.env(false), UserId::new(1));

        // the digits-only cleaning
        let (promise, done) = promise_flag();
        let found = um.search_user_by_phone_number("+1 (23) 45-6".to_owned(), true, promise);
        assert_eq!(found, UserId::default());
        assert!(done.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            um.resolved_phone_numbers.is_empty(),
            "only_local never caches"
        );

        // the negative cache
        um.on_resolved_phone_number("123456".to_owned(), UserId::default());
        let (promise, done) = promise_flag();
        let found = um.search_user_by_phone_number("123456".to_owned(), true, promise);
        assert_eq!(found, UserId::default());
        assert!(done.load(std::sync::atomic::Ordering::SeqCst));

        // a resolved user answers from the cache
        um.on_resolved_phone_number("777".to_owned(), UserId::new(42));
        let (promise, done) = promise_flag();
        let found = um.search_user_by_phone_number("777".to_owned(), false, promise);
        assert_eq!(found, UserId::new(42));
        assert!(done.load(std::sync::atomic::Ordering::SeqCst));

        // the negative cache never downgrades a resolved phone (the C++
        // `emplace` keep-first)
        um.on_resolved_phone_number("777".to_owned(), UserId::default());
        let (promise, done) = promise_flag();
        let found = um.search_user_by_phone_number("777".to_owned(), false, promise);
        assert_eq!(found, UserId::new(42));
        assert!(done.load(std::sync::atomic::Ordering::SeqCst));

        // the empty number is an error
        let (promise, error) = promise_result();
        let found = um.search_user_by_phone_number("+".to_owned(), false, promise);
        assert_eq!(found, UserId::default());
        let error = error.lock().unwrap().take().expect("the promise failed");
        let error = error.error().clone();
        assert_eq!(error.code(), 200);
        assert_eq!(error.message(), "Phone number is invalid");
    }

    #[test]
    fn on_get_contacts_statuses_feeds_the_online_machinery() {
        let (mut temp, mut um) = test_env();
        um.set_my_id(&mut temp.env(false), UserId::new(1));
        let contact = telegram_contact(2, "Alice", "Cooper");
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(contact.clone()),
            "test",
        );
        load_contacts_via_on_get_contacts(&mut um, &mut temp.env(false), vec![contact], 1);

        let expires = temp.global.unix_time() + 60;
        um.on_get_contacts_statuses(
            &mut temp.env(false),
            vec![telegram_api::ContactStatus {
                user_id: 2,
                status: Some(telegram_api::UserStatus::userStatusOnline(
                    telegram_api::userStatusOnline { expires },
                )),
            }],
        );
        assert!(um.get_user(UserId::new(2)).unwrap().was_online > 0);
        assert!(
            !temp
                .global
                .binlog_pmc()
                .get("next_contacts_sync_date")
                .is_empty(),
            "the statuses complete the sync and save the date"
        );
    }

    #[test]
    fn on_update_contacts_reset_clears_the_imported_contacts() {
        let (mut temp, mut um) = test_env();
        um.set_my_id(&mut temp.env(false), UserId::new(1));
        load_contacts_via_on_get_contacts(&mut um, &mut temp.env(false), Vec::new(), 7);

        // the imported contacts are being loaded — the reset defers the clear
        temp.global
            .td_db()
            .common_kv()
            .set(b"user_imported_contacts", &[0x00]);
        let (promise, loaded) = promise_flag();
        um.load_imported_contacts(&mut temp.env(false), promise);
        // a broken blob clears the list, so the load still finishes
        assert!(loaded.load(std::sync::atomic::Ordering::SeqCst));
        assert!(um.are_imported_contacts_loaded);

        um.on_update_contacts_reset(&mut temp.env(false));
        assert_eq!(um.saved_contact_count, 0);
        assert_eq!(
            temp.global.binlog_pmc().get("saved_contact_count"),
            "0",
            "the reset count rides the binlog pmc"
        );
        assert!(
            temp.global
                .td_db()
                .common_kv()
                .get(b"user_imported_contacts")
                .is_empty(),
            "the imported-contacts row is erased"
        );
    }

    // ------------------------------------------------------------------
    // Contact policy + registries (M.4.15)

    /// A `MessageFullId` of a channel message: the C++ dialog id is
    /// `ZERO_CHANNEL_ID - channel_id`.
    fn channel_message_full_id(channel_id: i64, message_id: i64) -> MessageFullId {
        MessageFullId::new(
            crate::message_id::DialogId::new(
                crate::message_id::DialogId::ZERO_CHANNEL_ID - channel_id,
            ),
            crate::message_id::MessageId::new(message_id),
        )
    }

    /// A min (no-phone) cached user — access hash present but not usable.
    fn add_min_user(um: &mut UserManager, env: &mut UserEnv, id: i64, name: &str) -> UserId {
        let mut min_user = telegram_user(id, name, USER_ACCESS_HASH_MASK | (1 << 1));
        min_user.min = true;
        um.on_get_user(env, telegram_api::User::user(min_user), "test")
    }

    /// A received cached user with a usable access hash.
    fn add_received_user(um: &mut UserManager, env: &mut UserEnv, id: i64, name: &str) -> UserId {
        um.on_get_user(
            env,
            telegram_api::User::user(telegram_user(id, name, USER_ACCESS_HASH_MASK | (1 << 1))),
            "test",
        )
    }

    #[test]
    fn message_users_registry_gates_on_known_channels_and_reemits() {
        let (mut temp, mut um) = test_env();
        let mut env = temp.env(false);
        let min_user_id = add_min_user(&mut um, &mut env, 5, "Min");
        um.take_updates(); // the seeding's own updateUser

        let message_full_id = channel_message_full_id(7, 1 << 20);
        // an unknown channel: the ChatManager stand-in gate drops the call
        um.register_message_users(&env, message_full_id, &[min_user_id]);
        assert!(um.user_messages.is_empty());
        assert!(um.take_updates().is_empty());

        // a known channel admits; the min user gets an updateUser
        um.known_channels.insert(7);
        um.register_message_users(&env, message_full_id, &[min_user_id]);
        assert_eq!(um.user_messages.get(&5).unwrap().len(), 1);
        let updates = um.take_updates();
        assert!(
            matches!(&updates[0], td_api::Update::updateUser(u) if u.user.as_ref().is_some_and(|user| user.id == 5)),
            "the registry appearance emits updateUser for the min user"
        );

        // a second message for the same user does not re-emit
        um.register_message_users(&env, channel_message_full_id(7, 2 << 20), &[min_user_id]);
        assert!(um.take_updates().is_empty());

        // a received user never enters the registry
        let full_user_id = add_received_user(&mut um, &mut env, 6, "Full");
        um.take_updates(); // the seeding's own updateUser
        um.register_message_users(&env, message_full_id, &[full_user_id]);
        assert!(!um.user_messages.contains_key(&6));

        // unregistering one of two messages keeps the entry (no re-emit)
        um.unregister_message_users(&env, message_full_id, &[min_user_id]);
        assert!(um.take_updates().is_empty());
        assert_eq!(um.user_messages.get(&5).unwrap().len(), 1);

        // the last reference removes the entry and re-emits
        um.unregister_message_users(&env, channel_message_full_id(7, 2 << 20), &[min_user_id]);
        assert!(!um.user_messages.contains_key(&5));
        let updates = um.take_updates();
        assert!(
            matches!(&updates[0], td_api::Update::updateUser(u) if u.user.as_ref().is_some_and(|user| user.id == 5)),
            "the last registry reference emits updateUser for the min user"
        );

        // the fast path: nothing registered, nothing emitted
        um.unregister_message_users(&env, message_full_id, &[min_user_id]);
        assert!(um.take_updates().is_empty());
    }

    #[test]
    fn noforwards_request_registry_arms_fires_and_cancels() {
        let (mut temp, mut um) = test_env();
        let mut env = temp.env(false);
        let unix_time = env.unix_time();
        let message_full_id = channel_message_full_id(7, 1 << 20);

        // a message date inside the window arms the deadline
        um.register_noforwards_request(&env, message_full_id, unix_time - 10);
        assert_eq!(
            um.noforwards_request_ids.get(&message_full_id).copied(),
            Some(1)
        );
        let deadline = *um.noforwards_request_timeouts.get(&1).unwrap();
        assert!(deadline > tdutils::time::now());

        // a scheduled message never arms — C++ checks is_scheduled() before
        // any map access, and probing the ordered map with a scheduled key
        // CHECK-fails there, so assert through the sizes instead
        let scheduled = MessageFullId::new(
            message_full_id.get_dialog_id(),
            crate::message_id::MessageId::new((1 << 21) | 4),
        );
        um.register_noforwards_request(&env, scheduled, unix_time - 10);
        assert_eq!(um.noforwards_request_ids.len(), 1);
        assert_eq!(um.noforwards_request_message_full_ids.len(), 1);
        assert_eq!(um.noforwards_request_timeouts.len(), 1);
        assert_eq!(
            um.noforwards_request_message_full_ids.get(&1),
            Some(&message_full_id)
        );

        // a message outside the window never arms
        let expired = channel_message_full_id(7, 2 << 20);
        um.register_noforwards_request(&env, expired, unix_time - 1_000_000);
        assert!(!um.noforwards_request_ids.contains_key(&expired));

        // the fire path: drag the deadline into the past and tick
        *um.noforwards_request_timeouts.get_mut(&1).unwrap() = tdutils::time::now() - 1.0;
        um.tick_timeouts(&mut env);
        assert!(um.noforwards_request_ids.is_empty());
        assert!(um.noforwards_request_message_full_ids.is_empty());
        assert!(!um.noforwards_request_timeouts.contains_key(&1));

        // unregister cancels the deadline; unknown unregister is a no-op
        um.register_noforwards_request(&env, message_full_id, unix_time - 10);
        assert_eq!(
            um.noforwards_request_ids.get(&message_full_id).copied(),
            Some(2)
        );
        um.unregister_noforwards_request(message_full_id);
        assert!(um.noforwards_request_timeouts.is_empty());
        assert!(um.noforwards_request_ids.is_empty());
        um.unregister_noforwards_request(message_full_id);
    }

    /// The shared completion slot of a unit-typed promise.
    type UnitResultSlot = std::sync::Arc<std::sync::Mutex<Option<tdutils::status::Result<()>>>>;

    /// A `Promise<()>` capturing its `Result` into a shared slot.
    fn promise_unit_result() -> (Promise<()>, UnitResultSlot) {
        let result = std::sync::Arc::new(std::sync::Mutex::new(None));
        let result_clone = result.clone();
        (
            PromiseCreator::lambda_result(move |r| *result_clone.lock().unwrap() = Some(r)),
            result,
        )
    }

    /// The shared completion slot of the can-send deferred promise.
    type CanSendResultSlot = std::sync::Arc<
        std::sync::Mutex<Option<tdutils::status::Result<td_api::CanSendMessageToUserResult>>>,
    >;

    /// A `Promise<CanSendMessageToUserResult>` capturing its `Result` into
    /// a shared slot.
    fn promise_can_send_result() -> (
        Promise<td_api::CanSendMessageToUserResult>,
        CanSendResultSlot,
    ) {
        let result = std::sync::Arc::new(std::sync::Mutex::new(None));
        let result_clone = result.clone();
        (
            PromiseCreator::lambda_result(move |r| *result_clone.lock().unwrap() = Some(r)),
            result,
        )
    }

    fn is_can_send_ok(
        result: &Option<tdutils::status::Result<td_api::CanSendMessageToUserResult>>,
    ) -> bool {
        matches!(
            result,
            Some(tdutils::status::Result::Ok(
                td_api::CanSendMessageToUserResult::canSendMessageToUserResultOk(_)
            ))
        )
    }

    #[test]
    fn can_send_message_to_user_answers_the_cpp_matrix() {
        let (mut temp, mut um) = test_env();
        let mut env = temp.env(false);

        // self: Ok
        um.my_id = UserId::new(7);
        let (promise, result) = promise_can_send_result();
        um.can_send_message_to_user(&mut env, UserId::new(7), false, promise);
        assert!(is_can_send_ok(&result.lock().unwrap()));

        // an unknown (non-bot) user: UserIsDeleted
        let (promise, result) = promise_can_send_result();
        um.can_send_message_to_user(&mut env, UserId::new(99), false, promise);
        assert!(matches!(
            &*result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(
                td_api::CanSendMessageToUserResult::canSendMessageToUserResultUserIsDeleted(_)
            ))
        ));

        // a received user without restrictions: Ok
        let user_id = add_received_user(&mut um, &mut env, 5, "Alice");
        let (promise, result) = promise_can_send_result();
        um.can_send_message_to_user(&mut env, user_id, false, promise);
        assert!(is_can_send_ok(&result.lock().unwrap()));

        // a deleted user: UserIsDeleted (the Write gate)
        um.users.get_mut(&5).unwrap().is_deleted = true;
        let (promise, result) = promise_can_send_result();
        um.can_send_message_to_user(&mut env, user_id, false, promise);
        assert!(matches!(
            &*result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(
                td_api::CanSendMessageToUserResult::canSendMessageToUserResultUserIsDeleted(_)
            ))
        ));

        // mutual contact bypasses the premium requirement
        let mutual_user_id = add_received_user(&mut um, &mut env, 8, "Bob");
        let u = um.users.get_mut(&8).unwrap();
        u.is_deleted = false;
        u.contact_require_premium = true;
        u.paid_message_star_count = 5;
        u.is_mutual_contact = true;
        let (promise, result) = promise_can_send_result();
        um.can_send_message_to_user(&mut env, mutual_user_id, false, promise);
        assert!(is_can_send_ok(&result.lock().unwrap()));

        // the premium option does not bypass a non-zero paid-message price
        // (the C++ premium disjunct requires paid_message_star_count == 0):
        // the ladder falls through to the merger, which is unwired in the
        // unit env and answers the "Request aborted" surface
        env.options.set_option_boolean("is_premium", true);
        um.users.get_mut(&8).unwrap().is_mutual_contact = false;
        let (promise, result) = promise_can_send_result();
        um.can_send_message_to_user(&mut env, mutual_user_id, false, promise);
        assert!(matches!(
            &*result.lock().unwrap(),
            Some(tdutils::status::Result::Err(status)) if status.code() == 500
        ));
        env.options.set_option_boolean("is_premium", false);

        // the cached contact price answers (paid / premium / none)
        um.user_full_contact_price.insert(8, 12);
        let (promise, result) = promise_can_send_result();
        um.can_send_message_to_user(&mut env, mutual_user_id, false, promise);
        assert!(matches!(
            &*result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(
                td_api::CanSendMessageToUserResult::canSendMessageToUserResultUserHasPaidMessages(v)
            )) if v.outgoing_paid_message_star_count == 12
        ));
        um.user_full_contact_price.insert(8, -1);
        let (promise, result) = promise_can_send_result();
        um.can_send_message_to_user(&mut env, mutual_user_id, false, promise);
        assert!(matches!(
            &*result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(
                td_api::CanSendMessageToUserResult::canSendMessageToUserResultUserRestrictsNewChats(
                    _
                )
            ))
        ));
        um.user_full_contact_price.insert(8, 0);
        let (promise, result) = promise_can_send_result();
        um.can_send_message_to_user(&mut env, mutual_user_id, false, promise);
        assert!(is_can_send_ok(&result.lock().unwrap()));

        // the cached UserFull answers (paid / premium / none)
        um.user_full_contact_price.clear();
        um.add_user_full(UserId::new(8));
        um.users_full.get_mut(&8).unwrap().send_paid_message_stars = 7;
        let (promise, result) = promise_can_send_result();
        um.can_send_message_to_user(&mut env, mutual_user_id, false, promise);
        assert!(matches!(
            &*result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(
                td_api::CanSendMessageToUserResult::canSendMessageToUserResultUserHasPaidMessages(v)
            )) if v.outgoing_paid_message_star_count == 7
        ));
        um.users_full.get_mut(&8).unwrap().send_paid_message_stars = 0;
        um.users_full.get_mut(&8).unwrap().contact_require_premium = true;
        let (promise, result) = promise_can_send_result();
        um.can_send_message_to_user(&mut env, mutual_user_id, false, promise);
        assert!(matches!(
            &*result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(
                td_api::CanSendMessageToUserResult::canSendMessageToUserResultUserRestrictsNewChats(
                    _
                )
            ))
        ));
        um.users_full.get_mut(&8).unwrap().contact_require_premium = false;
        let (promise, result) = promise_can_send_result();
        um.can_send_message_to_user(&mut env, mutual_user_id, false, promise);
        assert!(is_can_send_ok(&result.lock().unwrap()));

        // an uncached user with force: Ok (the merged-query half)
        let forced_user_id = add_received_user(&mut um, &mut env, 9, "Eve");
        let u = um.users.get_mut(&9).unwrap();
        u.contact_require_premium = true;
        u.paid_message_star_count = 3;
        let (promise, result) = promise_can_send_result();
        um.can_send_message_to_user(&mut env, forced_user_id, true, promise);
        assert!(is_can_send_ok(&result.lock().unwrap()));
    }

    #[test]
    fn share_phone_number_parks_until_contacts_load_and_errors_on_failure() {
        let (mut temp, mut um) = test_env();
        let mut env = temp.env(false);
        let user_id = add_received_user(&mut um, &mut env, 5, "Alice");

        // parked while the contacts are unloaded; the failure drain errors
        // the parked promise
        let (promise, result) = promise_result();
        um.share_phone_number(&mut env, user_id, promise);
        assert_eq!(um.pending_load_contacts_calls.len(), 1);
        um.on_get_contacts_failed(&mut env, Status::error(400, "contact load failed"));
        assert!(matches!(
            *result.lock().unwrap(),
            Some(tdutils::status::Result::Err(_))
        ));
        assert!(um.pending_load_contacts_calls.is_empty());

        // the loaded path re-dispatches through the parked-call queue
        let (promise, _flag) = promise_flag();
        um.share_phone_number(&mut env, user_id, promise);
        assert_eq!(um.pending_load_contacts_calls.len(), 1);
        load_contacts_via_on_get_contacts(
            &mut um,
            &mut env,
            vec![telegram_user(5, "Alice", USER_ACCESS_HASH_MASK | (1 << 1))],
            1,
        );
        assert!(um.pending_load_contacts_calls.is_empty());

        // the loaded path errors on an unknown user (the get_input_user 400)
        let (promise, result) = promise_result();
        um.share_phone_number(&mut env, UserId::new(99), promise);
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.code() == 400)
        );
    }

    #[test]
    fn freeze_state_restores_updates_and_persists() {
        let (mut temp, mut um) = test_env();
        let mut env = temp.env(false);
        env.global.binlog_pmc().set(
            "freeze_state",
            "1700000000 1800000000 https://appeal.example/x y",
        );

        // the constructor restore emits updateFreezeState
        um.init_freeze_state(env.global.binlog_pmc());
        assert_eq!(um.freeze_since_date, 1700000000);
        assert_eq!(um.freeze_until_date, 1800000000);
        assert_eq!(um.freeze_appeal_url, "https://appeal.example/x y");
        let updates = um.take_updates();
        assert!(matches!(&updates[0], td_api::Update::updateFreezeState(u)
                if u.is_frozen
                    && u.freezing_date == 1700000000
                    && u.deletion_date == 1800000000
                    && u.appeal_link == "https://appeal.example/x y"));

        // the no-op guard keeps state and storage untouched
        um.on_update_freeze_state(
            &mut env,
            1700000000,
            1800000000,
            "https://appeal.example/x y".to_owned(),
        );
        assert!(um.take_updates().is_empty());
        assert_eq!(
            env.global.binlog_pmc().get("freeze_state"),
            "1700000000 1800000000 https://appeal.example/x y"
        );

        // a change emits and re-persists
        um.on_update_freeze_state(&mut env, 1700000001, 1800000000, String::new());
        let updates = um.take_updates();
        assert!(matches!(&updates[0], td_api::Update::updateFreezeState(u)
                if u.freezing_date == 1700000001 && u.appeal_link.is_empty()));
        assert_eq!(
            env.global.binlog_pmc().get("freeze_state"),
            "1700000001 1800000000 "
        );

        // the end of the freeze erases the pmc value
        um.on_update_freeze_state(&mut env, 0, 0, String::new());
        assert!(env.global.binlog_pmc().get("freeze_state").is_empty());
        let updates = um.take_updates();
        assert!(matches!(&updates[0], td_api::Update::updateFreezeState(u) if !u.is_frozen));

        // an absent pmc value restores nothing
        let (mut temp2, mut um2) = test_env();
        um2.init_freeze_state(temp2.global.binlog_pmc());
        assert!(um2.take_updates().is_empty());
        assert_eq!(um2.freeze_since_date, 0);
    }

    #[test]
    fn privacy_updates_expire_user_fulls_and_reemit_restricted_users() {
        let (mut temp, mut um) = test_env();
        let mut env = temp.env(false);
        um.my_id = UserId::new(1);

        // the user record precedes its full: a fresh full is born
        // need_send_update, and the C++ CHECK(u == nullptr ||
        // u->is_update_user_sent) forbids announcing a full before its user
        add_received_user(&mut um, &mut env, 5, "Alice");
        um.take_updates();

        // the phone-number privacy update expires every cached UserFull
        um.add_user_full(UserId::new(5));
        um.add_user_full(UserId::new(6));
        um.users_full.get_mut(&5).unwrap().expires_at = 123.0;
        um.users_full.get_mut(&6).unwrap().expires_at = 456.0;
        um.on_update_phone_number_privacy(&env);
        assert_eq!(um.users_full.get(&5).unwrap().expires_at, 0.0);
        assert_eq!(um.users_full.get(&6).unwrap().expires_at, 0.0);

        // the ignored-restrictions update re-emits every restricted user
        um.restricted_user_ids.insert(5);
        um.restricted_user_ids.insert(999); // the C++ null-user form
        um.on_ignored_restriction_reasons_changed(&env);
        let updates = um.take_updates();
        assert_eq!(updates.len(), 2);
        assert!(
            matches!(&updates[0], td_api::Update::updateUser(u) if u.user.as_ref().is_some_and(|user| user.id == 5))
        );
        assert!(
            matches!(&updates[1], td_api::Update::updateUser(u) if u.user.as_ref().is_some_and(|user| user.id == 999))
        );
    }

    #[test]
    fn premium_contact_price_intake_caches_and_allow_clears() {
        let (mut temp, mut um) = test_env();
        let env = temp.env(false);
        let (promise, flag) = promise_flag();
        um.on_get_is_premium_required_to_contact_users(
            vec![UserId::new(5), UserId::new(6), UserId::new(7)],
            vec![
                telegram_api::RequirementToContact::requirementToContactEmpty(
                    telegram_api::requirementToContactEmpty,
                ),
                telegram_api::RequirementToContact::requirementToContactPremium(
                    telegram_api::requirementToContactPremium,
                ),
                telegram_api::RequirementToContact::requirementToContactPaidMessages(
                    telegram_api::requirementToContactPaidMessages { stars_amount: 15 },
                ),
            ],
            promise,
        );
        assert!(flag.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(um.user_full_contact_price.get(&5), Some(&0));
        assert_eq!(um.user_full_contact_price.get(&6), Some(&-1));
        assert_eq!(um.user_full_contact_price.get(&7), Some(&15));

        // a user with a cached UserFull is skipped
        um.add_user_full(UserId::new(8));
        let (promise, _flag) = promise_flag();
        um.on_get_is_premium_required_to_contact_users(
            vec![UserId::new(8)],
            vec![
                telegram_api::RequirementToContact::requirementToContactPremium(
                    telegram_api::requirementToContactPremium,
                ),
            ],
            promise,
        );
        assert!(!um.user_full_contact_price.contains_key(&8));

        // the size mismatch errors with the C++ message
        let (promise, result) = promise_result();
        um.on_get_is_premium_required_to_contact_users(vec![UserId::new(5)], Vec::new(), promise);
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.code() == 500)
        );

        // allow_send_message_to_user clears the premium flag only
        um.allow_send_message_to_user(&env, UserId::new(6));
        assert_eq!(um.user_full_contact_price.get(&6), Some(&0));
        um.allow_send_message_to_user(&env, UserId::new(7));
        assert_eq!(um.user_full_contact_price.get(&7), Some(&15));
    }

    #[test]
    fn set_main_profile_tab_gates_null_and_the_unknown_channel_allow_list() {
        let (_temp, mut um) = test_env();

        // the null gate
        let (promise, result) = promise_result();
        um.set_main_profile_tab(None, promise);
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "Profile tab must be non-empty")
        );

        // ProfileTab.cpp:61-95 with ChannelType::Unknown: only Posts/Gifts
        let (promise, result) = promise_result();
        um.set_main_profile_tab(
            Some(td_api::ProfileTab::profileTabMedia(
                td_api::profileTabMedia {},
            )),
            promise,
        );
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "Invalid profile tab specified for the chat")
        );

        // the allowed tabs park on the SetMainProfileTabQuery (the
        // family queue — the net-inert send logs)
        let (promise, result) = promise_result();
        um.set_main_profile_tab(
            Some(td_api::ProfileTab::profileTabPosts(
                td_api::profileTabPosts {},
            )),
            promise,
        );
        assert_eq!(um.pending_set_main_profile_tab_queries.len(), 1);
        assert_eq!(
            um.pending_set_main_profile_tab_queries[0].0,
            crate::chat_manager::ProfileTab::Posts
        );
        assert!((*result.lock().unwrap()).is_none());
        um.pending_set_main_profile_tab_queries.clear();
    }

    #[test]
    fn set_user_note_runs_the_input_user_and_contact_gates() {
        let (mut temp, mut um) = test_env();
        let mut env = temp.env(false);
        let user_id = add_received_user(&mut um, &mut env, 5, "Alice");

        // the unknown user is the get_input_user 400
        let (promise, result) = promise_result();
        um.set_user_note(&env, UserId::new(99), None, promise);
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.code() == 400)
        );

        // the known non-contact is the contact-gate 400
        let (promise, result) = promise_result();
        um.set_user_note(&env, user_id, None, promise);
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "User isn't a contact")
        );

        // my own id fails the same contact gate first, like the C++
        let (promise, result) = promise_result();
        um.set_user_note(&env, um.get_my_id(), None, promise);
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "User isn't a contact")
        );
    }

    #[test]
    fn suggest_user_birthdate_gates_self_bots_and_parks() {
        let (mut temp, mut um) = test_env();
        let mut env = temp.env(false);
        let user_id = add_received_user(&mut um, &mut env, 5, "Alice");
        let birthdate = td_api::Birthdate {
            day: 10,
            month: 6,
            year: 1990,
        };

        // self
        let (promise, result) = promise_result();
        um.suggest_user_birthdate(&env, um.get_my_id(), birthdate.clone(), promise);
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "Can't suggest birthdate to self")
        );

        // the known non-contact parks on the SuggestUserBirthdayQuery
        // (the family queue — the net-inert send logs; the C++ has no
        // contact gate here)
        let (promise, result) = promise_result();
        um.suggest_user_birthdate(&env, user_id, birthdate, promise);
        assert_eq!(um.pending_suggest_birthday_queries.len(), 1);
        assert!((*result.lock().unwrap()).is_none());
        um.pending_suggest_birthday_queries.clear();
    }

    #[test]
    fn get_support_user_parks_until_the_ack_then_answers_from_the_cache() {
        let (mut temp, mut um) = test_env();
        let mut env = temp.env(false);
        let user_id = add_received_user(&mut um, &mut env, 5, "Support");

        // the uncached call parks on the GetSupportUserQuery
        let (promise, result) = typed_promise_result();
        um.get_support_user(&env, promise);
        assert_eq!(um.pending_support_user_queries.len(), 1);
        assert!((*result.lock().unwrap()).is_none());

        // the ack caches the id and answers the parked promise
        let (promise, ack_result) = typed_promise_result();
        um.on_get_support_user(&env, user_id, promise);
        assert!(matches!(
            *ack_result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(_))
        ));

        // the cached id answers immediately without parking
        let (promise, result) = typed_promise_result();
        um.get_support_user(&env, promise);
        assert!(um.pending_support_user_queries.is_empty());
        assert!(matches!(
            *result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(_))
        ));
    }

    #[test]
    fn user_full_store_round_trips_the_main_profile_tab() {
        // the flags2 slot (cpp:2215/2272) + the raw int32
        // (cpp:2376-2377/2595-2596) — Default stores the flag off
        let ctx = TimeCtx {
            now: 1.0,
            server_time: 0.0,
        };
        let mut user_full = UserFull::new();
        let mut bytes = Vec::new();
        user_full.store_value(&mut bytes, &ctx);
        let parsed = UserFull::parse_value(&bytes, &ctx).unwrap();
        assert_eq!(parsed.main_profile_tab, ProfileTab::Default);

        user_full.main_profile_tab = ProfileTab::Gifts;
        let mut bytes = Vec::new();
        user_full.store_value(&mut bytes, &ctx);
        let parsed = UserFull::parse_value(&bytes, &ctx).unwrap();
        assert_eq!(parsed.main_profile_tab, ProfileTab::Gifts);
        let mut bytes2 = Vec::new();
        parsed.store_value(&mut bytes2, &ctx);
        assert_eq!(bytes, bytes2, "the re-store is byte-identical");
    }

    #[test]
    fn on_get_user_full_applies_the_wire_main_tab() {
        let (mut temp, mut um) = test_env();
        let full_flags = USER_ACCESS_HASH_MASK | (1 << 1);
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(5, "A", full_flags)),
            "t",
        );
        um.take_updates();
        let user_full = telegram_api::UserFull {
            id: 5,
            main_tab: Some(telegram_api::ProfileTab::profileTabGifts(
                telegram_api::profileTabGifts,
            )),
            ..telegram_api::UserFull::default()
        };
        um.on_get_user_full(&mut temp.env(false), user_full);
        assert_eq!(
            um.users_full.get(&5).unwrap().main_profile_tab,
            ProfileTab::Gifts
        );

        // the disallowed band (Unknown answers only Posts/Gifts) logs
        // and keeps the record untouched (ProfileTab.cpp:47-50)
        let user_full = telegram_api::UserFull {
            id: 5,
            main_tab: Some(telegram_api::ProfileTab::profileTabMedia(
                telegram_api::profileTabMedia,
            )),
            ..telegram_api::UserFull::default()
        };
        um.on_get_user_full(&mut temp.env(false), user_full);
        assert_eq!(
            um.users_full.get(&5).unwrap().main_profile_tab,
            ProfileTab::Default
        );
    }

    #[test]
    fn on_set_main_profile_tab_writes_my_user_full_and_answers() {
        let (mut temp, mut um) = test_env();
        um.set_my_id(&mut temp.env(false), UserId::new(700000001));
        let my_id = um.get_my_id();
        um.on_get_user(
            &mut temp.env(false),
            telegram_api::User::user(telegram_user(
                my_id.get(),
                "Me",
                USER_ACCESS_HASH_MASK | (1 << 1),
            )),
            "t",
        );
        um.on_get_user_full(
            &mut temp.env(false),
            telegram_api::UserFull {
                id: my_id.get(),
                ..telegram_api::UserFull::default()
            },
        );
        um.take_updates();

        let (promise, result) = promise_result();
        um.on_set_main_profile_tab(&mut temp.env(false), ProfileTab::Gifts, promise);
        assert_eq!(
            um.users_full.get(&my_id.get()).unwrap().main_profile_tab,
            ProfileTab::Gifts
        );
        assert!(matches!(
            *result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(()))
        ));
    }

    #[test]
    fn add_recent_emoji_status_bands() {
        let (mut temp, mut um) = test_env();
        let mut env = temp.env(false);

        // the empty status never registers
        um.add_recent_emoji_status(&mut env, EmojiStatus::default());
        let statuses = crate::emoji_status::load_emoji_statuses(
            env.global.binlog_pmc(),
            crate::emoji_status::RECENT_EMOJI_STATUSES_DATABASE_KEY,
        );
        assert!(statuses.emoji_statuses.is_empty());
        assert_eq!(statuses.hash, -1);

        // a status with an expiry registers without it (the
        // clear_until_date band, EmojiStatus.cpp:609)
        let wire_status = |document_id: i64, until: i32| {
            EmojiStatus::from_telegram(telegram_api::EmojiStatus::emojiStatus(
                telegram_api::emojiStatus {
                    flags: if until != 0 { 1 } else { 0 },
                    document_id,
                    until,
                },
            ))
        };
        let status = wire_status(7, 1000);
        assert_eq!(status.get_until_date(), 1000);
        um.add_recent_emoji_status(&mut env, status.clone());
        let statuses = crate::emoji_status::load_emoji_statuses(
            env.global.binlog_pmc(),
            crate::emoji_status::RECENT_EMOJI_STATUSES_DATABASE_KEY,
        );
        assert_eq!(statuses.emoji_statuses.len(), 1);
        assert_eq!(statuses.hash, 0);
        assert_eq!(statuses.emoji_statuses[0].get_until_date(), 0);
        assert_eq!(statuses.emoji_statuses[0].get_custom_emoji_id().get(), 7);

        // the head dedupe short-circuits (cpp:611-613)
        um.add_recent_emoji_status(&mut env, status.clone());
        let statuses = crate::emoji_status::load_emoji_statuses(
            env.global.binlog_pmc(),
            crate::emoji_status::RECENT_EMOJI_STATUSES_DATABASE_KEY,
        );
        assert_eq!(statuses.emoji_statuses.len(), 1);

        // a different status moves to the top (add_to_top, cpp:615-616)
        let other = wire_status(9, 0);
        um.add_recent_emoji_status(&mut env, other);
        let statuses = crate::emoji_status::load_emoji_statuses(
            env.global.binlog_pmc(),
            crate::emoji_status::RECENT_EMOJI_STATUSES_DATABASE_KEY,
        );
        assert_eq!(statuses.emoji_statuses.len(), 2);
        assert_eq!(statuses.emoji_statuses[0].get_custom_emoji_id().get(), 9);
    }

    #[test]
    fn set_user_note_parks_the_query_with_the_normalized_note() {
        let (mut temp, mut um) = test_env();
        let mut env = temp.env(false);
        let mut wire = telegram_user(5, "Alice", USER_ACCESS_HASH_MASK | (1 << 1) | (1 << 11));
        wire.contact = true;
        um.on_get_user(&mut env, telegram_api::User::user(wire), "test");
        um.take_updates();

        let (promise, result) = promise_result();
        um.set_user_note(
            &env,
            UserId::new(5),
            Some(td_api::FormattedText {
                text: "the note".to_owned(),
                entities: Vec::new(),
            }),
            promise,
        );
        assert_eq!(um.pending_update_contact_note_queries.len(), 1);
        let (parked_user, parked_note, _) = &um.pending_update_contact_note_queries[0];
        assert_eq!(*parked_user, UserId::new(5));
        assert_eq!(parked_note.text, "the note");
        assert!((*result.lock().unwrap()).is_none());
        um.pending_update_contact_note_queries.clear();
    }

    #[test]
    fn set_profile_photo_impl_runs_the_validation_bands() {
        let (mut temp, mut um) = test_env();
        let env = temp.env(false);
        let my_id = um.get_my_id();
        let mut files = crate::files::FileManager::new();
        let mut fllm = crate::files::FileLoadManager::new(
            Box::new(PanicNetFactory),
            crate::files::FileLoadManagerOptions::default(),
        );
        let upload_callback: std::sync::Arc<parking_lot::Mutex<dyn crate::files::UploadCallback>> =
            std::sync::Arc::new(parking_lot::Mutex::new(NoopUserUploadCallback));
        let stickers = crate::stickers_manager::StickersManager::new();
        macro_rules! mk_td {
            () => {
                SetProfilePhotoTd {
                    env: &env,
                    files: &mut files,
                    fllm: &mut fllm,
                    upload_callback: &upload_callback,
                    stickers: &stickers,
                }
            };
        }

        // the null gate
        let (promise, result) = promise_result();
        um.set_profile_photo_impl(mk_td!(), my_id, None, false, false, promise);
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "New profile photo must be non-empty")
        );

        // inputChatPhotoPrevious for another user is the C++ 400
        let previous =
            td_api::InputChatPhoto::inputChatPhotoPrevious(td_api::inputChatPhotoPrevious {
                chat_photo_id: 7,
            });
        let (promise, result) = promise_result();
        um.set_profile_photo_impl(
            mk_td!(),
            UserId::new(5),
            Some(&previous),
            false,
            false,
            promise,
        );
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "Can't use inputChatPhotoPrevious")
        );

        // an unknown photo id is the cache-miss 400
        let (promise, result) = promise_result();
        um.set_profile_photo_impl(mk_td!(), my_id, Some(&previous), false, false, promise);
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "Unknown profile photo ID specified")
        );

        // a registered photo whose file view is missing is the C++ 500
        // (send_update_profile_photo_query's file-view gate, cpp:6100)
        um.my_photo_file_id.insert(7, FileId::new(0, 0));
        let (promise, result) = promise_result();
        um.set_profile_photo_impl(mk_td!(), my_id, Some(&previous), false, false, promise);
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "Failed to upload the file")
        );

        // the animation timestamp band
        let animation =
            td_api::InputChatPhoto::inputChatPhotoAnimation(td_api::inputChatPhotoAnimation {
                animation: None,
                main_frame_timestamp: 42.0,
            });
        let (promise, result) = promise_result();
        um.set_profile_photo_impl(mk_td!(), my_id, Some(&animation), false, false, promise);
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "Wrong main frame timestamp specified")
        );

        // a static photo without a file is the get_input_file_id 400
        // (the input gates answer before the upload flow)
        let static_photo =
            td_api::InputChatPhoto::inputChatPhotoStatic(td_api::inputChatPhotoStatic {
                photo: None,
            });
        let (promise, result) = promise_result();
        um.set_profile_photo_impl(mk_td!(), my_id, Some(&static_photo), false, false, promise);
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "InputFile is not specified")
        );
    }

    #[test]
    fn delete_profile_photo_bands_the_unloaded_user_full() {
        let (mut temp, mut um) = test_env();
        {
            let mut env = temp.env(false);

            // no cached UserFull and no Td-actor hop: the net-inert
            // posture answers the C++ close-state surface (the
            // uncached-consumer convention — the ladder's combiner ride
            // needs the scheduler context)
            let (promise, result) = promise_result();
            um.delete_profile_photo(&mut env, 7, false, promise);
            assert!(
                matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "Request aborted")
            );

            // the re-entry arm answers the C++ 500 (cpp:6405)
            let (promise, result) = promise_result();
            um.delete_profile_photo(&mut env, 7, true, promise);
            assert!(
                matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "Failed to load UserFullInfo")
            );
        }

        // the close gate answers the C++ synchronous 500 (cpp:6396)
        temp.global.close_all(false);
        let mut env = temp.env(false);
        let (promise, result) = promise_result();
        um.delete_profile_photo(&mut env, 7, false, promise);
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "Request aborted")
        );
    }

    #[test]
    fn on_set_profile_photo_updates_the_caches() {
        let (mut temp, mut um) = test_env();
        let my_id = UserId::new(7);
        um.set_my_id(&mut temp.env(false), my_id);
        seed_received_user(&mut temp, &mut um, 7);
        um.take_updates();

        // the photos cache through the natural retry-machine drive (one
        // parked request, the listing answer, the completion)
        let (p, _a) = recording_promise::<td_api::ChatPhotos>();
        um.get_user_profile_photos(&mut temp.env(false), my_id, 0, 10, p);
        um.on_get_user_photos(my_id, 0, 10, 1, vec![telegram_photo(31)]);
        um.on_get_user_profile_photos(&temp.env(false), my_id, Ok(()));
        assert_eq!(um.user_photos[&7].photos.len(), 1);

        // the full record with the same photo (the profile_photo apply
        // path needs a known full photo)
        um.on_get_user_full(
            &mut temp.env(false),
            telegram_api::UserFull {
                id: 7,
                flags: 1 << 2, // profile_photo present
                profile_photo: Some(telegram_photo(31)),
                ..telegram_api::UserFull::default()
            },
        );
        assert_eq!(um.users_full[&7].photo.id, 31);

        // on_set_profile_photo: the main-photo set takes the cache head
        // (cpp:6190-6214 + 6216-6271; the replaced-id delete arm is the
        // re-add band of the delete test). The answer's user carries
        // the pre-change photo — the photo-less form would wipe the
        // User photo first (the C++ drop path).
        let answer_user_with_photo = |photo_id: i64| {
            telegram_api::User::user(telegram_api::user {
                flags: USER_ACCESS_HASH_MASK | (1 << 1) | (1 << 5) | (1 << 20),
                access_hash: 123,
                id: 7,
                first_name: "Alice".to_owned(),
                photo: Some(telegram_api::UserProfilePhoto::userProfilePhoto(
                    telegram_api::userProfilePhoto {
                        flags: 0,
                        photo_id,
                        dc_id: 2,
                        ..telegram_api::userProfilePhoto::default()
                    },
                )),
                ..telegram_api::user::default()
            })
        };
        let (promise, result) = promise_result();
        um.on_set_profile_photo(
            &mut temp.env(false),
            my_id,
            telegram_api::photos_Photo {
                photo: Some(telegram_photo(34)),
                users: vec![answer_user_with_photo(31)],
            },
            false,
            0,
            promise,
        );
        assert!(matches!(
            *result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(()))
        ));
        let user_photos = &um.user_photos[&7];
        assert_eq!(user_photos.count, 2);
        assert_eq!(user_photos.photos[0].id, 34);
        // the User photo change dropped the full photos (the C++
        // `do_update_user_photo` mismatch drop, cpp:4065-4069) — the
        // full slot re-syncs on the next full load
        assert!(um.users_full[&7].photo.is_empty());
        assert!(um.users.get(&7).unwrap().photo_ids.contains(&34));

        // the fallback arm reaches the full-slot apply (the User-photo
        // arm is skipped for fallbacks — no drop precedes it)
        um.on_get_user_full(
            &mut temp.env(false),
            telegram_api::UserFull {
                id: 7,
                flags: 1 << 2, // profile_photo present
                profile_photo: Some(telegram_photo(31)),
                ..telegram_api::UserFull::default()
            },
        );
        let answer_user_with_photo = |photo_id: i64| {
            telegram_api::User::user(telegram_api::user {
                flags: USER_ACCESS_HASH_MASK | (1 << 1) | (1 << 5) | (1 << 20),
                access_hash: 123,
                id: 7,
                first_name: "Alice".to_owned(),
                photo: Some(telegram_api::UserProfilePhoto::userProfilePhoto(
                    telegram_api::userProfilePhoto {
                        flags: 0,
                        photo_id,
                        dc_id: 2,
                        ..telegram_api::userProfilePhoto::default()
                    },
                )),
                ..telegram_api::user::default()
            })
        };
        let (promise, result) = promise_result();
        um.on_set_profile_photo(
            &mut temp.env(false),
            my_id,
            telegram_api::photos_Photo {
                photo: Some(telegram_photo(35)),
                users: vec![answer_user_with_photo(31)],
            },
            true, // is_fallback
            0,
            promise,
        );
        assert!(matches!(
            *result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(()))
        ));
        assert_eq!(um.users_full[&7].fallback_photo.id, 35);
        assert_eq!(um.users_full[&7].photo.id, 31);
        assert!(um.my_photo_file_id.contains_key(&35));

        // the answer without the user rides the re-get (the merger is
        // unwired without the scheduler — the net-inert band)
        let (promise, result) = promise_result();
        um.on_set_profile_photo(
            &mut temp.env(false),
            my_id,
            telegram_api::photos_Photo {
                photo: Some(telegram_photo(35)),
                users: Vec::new(),
            },
            false,
            0,
            promise,
        );
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "Request aborted")
        );
    }

    #[test]
    fn delete_my_profile_photo_from_cache_runs_the_list_bands() {
        let (mut temp, mut um) = test_env();
        let my_id = UserId::new(7);
        um.set_my_id(&mut temp.env(false), my_id);
        seed_received_user(&mut temp, &mut um, 7);
        um.take_updates();

        // the 0/-2 ids never touch the caches
        assert!(!um.delete_my_profile_photo_from_cache(&mut temp.env(false), 0));
        assert!(!um.delete_my_profile_photo_from_cache(&mut temp.env(false), -2));

        // a known photo with an unknown cache id decrements the count
        // (the offset-0 partial window, cpp:6300-6305)
        um.user_photos.insert(
            7,
            UserPhotos {
                photos: vec![photo_with_id(31)],
                count: 2,
                offset: 0,
            },
        );
        assert!(!um.delete_my_profile_photo_from_cache(&mut temp.env(false), 99));
        let user_photos = &um.user_photos[&7];
        assert_eq!((user_photos.count, user_photos.photos.len()), (1, 1));

        // the main photo delete empties the User photo and answers the
        // re-get flag only when photos remain unknown (cpp:6345-6360) —
        // here the cache drains to the known-empty state
        um.user_photos.get_mut(&7).unwrap().count = 1;
        let need_reget = um.delete_my_profile_photo_from_cache(&mut temp.env(false), 31);
        assert!(!need_reget);
        let user_photos = &um.user_photos[&7];
        assert_eq!(
            (
                user_photos.count,
                user_photos.offset,
                user_photos.photos.len()
            ),
            (0, 0, 0)
        );

        // the main photo delete with a non-empty remainder answers the
        // re-get flag (the count stays above the window)
        um.user_photos.insert(
            7,
            UserPhotos {
                photos: vec![photo_with_id(41)],
                count: 3,
                offset: 1,
            },
        );
        um.users.get_mut(&7).unwrap().photo = as_profile_photo(&photo_with_id(41), false);
        let need_reget = um.delete_my_profile_photo_from_cache(&mut temp.env(false), 41);
        assert!(need_reget);
        let user_photos = &um.user_photos[&7];
        assert_eq!(user_photos.count, 2);

        // an unknown id with the full window known drops the photos
        // cache entirely (cpp:6306-6310)
        um.user_photos.insert(
            7,
            UserPhotos {
                photos: vec![photo_with_id(51)],
                count: 1,
                offset: 0,
            },
        );
        assert!(!um.delete_my_profile_photo_from_cache(&mut temp.env(false), 99));
        let user_photos = &um.user_photos[&7];
        assert_eq!(
            (
                user_photos.count,
                user_photos.offset,
                user_photos.photos.len()
            ),
            (-1, -1, 0)
        );
    }

    #[test]
    fn on_delete_profile_photo_answers_the_reget_ladder() {
        let (mut temp, mut um) = test_env();
        let my_id = UserId::new(7);
        um.set_my_id(&mut temp.env(false), my_id);
        seed_received_user(&mut temp, &mut um, 7);
        um.take_updates();

        // the known-empty cache completes the promise (cpp:6431-6438)
        um.user_photos.insert(
            7,
            UserPhotos {
                photos: vec![photo_with_id(31)],
                count: 1,
                offset: 0,
            },
        );
        let (promise, result) = promise_result();
        um.on_delete_profile_photo(&mut temp.env(false), 31, promise);
        assert!(matches!(
            *result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(()))
        ));

        // the unknown-remainder state rides the reload (the merger is
        // unwired without the scheduler — the net-inert band)
        um.user_photos.insert(
            7,
            UserPhotos {
                photos: vec![photo_with_id(41)],
                count: 3,
                offset: 1,
            },
        );
        um.users.get_mut(&7).unwrap().photo = as_profile_photo(&photo_with_id(41), false);
        let (promise, result) = promise_result();
        um.on_delete_profile_photo(&mut temp.env(false), 41, promise);
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "Request aborted")
        );
    }

    #[test]
    fn add_new_saved_music_runs_the_input_gates() {
        let (_temp, mut um) = test_env();
        let mut files = crate::files::FileManager::new();
        let mut fllm = crate::files::FileLoadManager::new(
            Box::new(PanicNetFactory),
            crate::files::FileLoadManagerOptions::default(),
        );
        let upload_callback: std::sync::Arc<parking_lot::Mutex<dyn crate::files::UploadCallback>> =
            std::sync::Arc::new(parking_lot::Mutex::new(NoopUserUploadCallback));
        let mut audios = crate::audios_manager::AudiosManager::new();

        // the null gate
        let (promise, result) = promise_result();
        um.add_new_saved_music(
            SavedMusicUploadTd {
                files: &mut files,
                fllm: &mut fllm,
                upload_callback: &upload_callback,
                audios: &mut audios,
                close_flag: false,
            },
            None,
            promise,
        );
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "Audio must be non-empty")
        );

        // the title/performer UTF-8 gate is the C++ raw-string band: a
        // typed Rust String is always valid UTF-8, so it is exercised at
        // the JSON plane, not here

        // a clean input without a file is the get_input_file_id 400
        // (the input gates answer before the upload flow)
        let (promise, result) = promise_result();
        um.add_new_saved_music(
            SavedMusicUploadTd {
                files: &mut files,
                fllm: &mut fllm,
                upload_callback: &upload_callback,
                audios: &mut audios,
                close_flag: false,
            },
            Some(td_api::InputAudio {
                audio: None,
                album_cover_thumbnail: None,
                duration: 10,
                title: "Title".to_owned(),
                performer: "Performer".to_owned(),
            }),
            promise,
        );
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "InputFile is not specified")
        );
    }

    #[test]
    fn on_upload_saved_music_error_runs_the_code_band() {
        let (_temp, mut um) = test_env();
        let file_upload_id = FileUploadId::new(FileId::new(9, 0), 1);

        // a coded status passes through
        let (promise, result) = promise_result();
        um.being_uploaded_saved_music_files
            .insert(file_upload_id, promise);
        um.on_upload_saved_music_error(file_upload_id, Status::error(420, "FLOOD_WAIT_5"));
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.code() == 420 && s.message() == "FLOOD_WAIT_5")
        );

        // a codeless status answers the C++ 500 band
        let (promise, result) = promise_result();
        um.being_uploaded_saved_music_files
            .insert(file_upload_id, promise);
        um.on_upload_saved_music_error(file_upload_id, Status::error_message("local error"));
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.code() == 500 && s.message() == "local error")
        );
    }

    #[test]
    fn on_upload_profile_photo_bands_the_web_remote_and_the_reupload_cap() {
        let (mut temp, mut um) = test_env();
        let env = temp.env(false);
        let mut files = crate::files::FileManager::new();
        let mut fllm = crate::files::FileLoadManager::new(
            Box::new(PanicNetFactory),
            crate::files::FileLoadManagerOptions::default(),
        );
        let upload_callback: std::sync::Arc<parking_lot::Mutex<dyn crate::files::UploadCallback>> =
            std::sync::Arc::new(parking_lot::Mutex::new(NoopUserUploadCallback));
        let stickers = crate::stickers_manager::StickersManager::new();

        // a web main remote — the early-complete (null InputFile) hop is
        // the C++ web band (cpp:6163-6165)
        let file_id = files.register_remote(
            crate::files::FullRemoteFileLocation::from_web(
                crate::files::FileType::Photo,
                "https://example.com/x.jpg".to_owned(),
                1,
            ),
            crate::files::FileLocationSource::FromServer,
            crate::message_id::DialogId::default(),
            0,
            0,
            String::new(),
        );
        let file_upload_id = FileUploadId::new(file_id, 1);
        let (promise, result) = promise_result();
        um.being_uploaded_profile_photos.insert(
            file_upload_id,
            UploadedProfilePhoto {
                user_id: UserId::new(7),
                is_fallback: false,
                only_suggest: false,
                main_frame_timestamp: 0.0,
                is_animation: false,
                reupload_count: 0,
                promise,
            },
        );
        um.on_upload_profile_photo(
            SetProfilePhotoTd {
                env: &env,
                files: &mut files,
                fllm: &mut fllm,
                upload_callback: &upload_callback,
                stickers: &stickers,
            },
            file_upload_id,
            None,
        );
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "Can't use web photo as profile photo")
        );

        // a common photo remote — the force-reupload ladder re-arms until
        // the count cap answers the C++ 400 (cpp:6166-6168 + 6178-6185)
        let file_id = files.register_remote(
            crate::files::FullRemoteFileLocation::from_photo(
                crate::files::PhotoSizeSource::StickerSetThumbnailVersion {
                    sticker_set_id: 0,
                    sticker_set_access_hash: 0,
                    version: 0,
                },
                7,
                77,
                crate::chat_manager::DcId::main(),
                "ref".to_owned(),
            ),
            crate::files::FileLocationSource::FromServer,
            crate::message_id::DialogId::default(),
            0,
            0,
            String::new(),
        );
        let file_upload_id = FileUploadId::new(file_id, 1);
        let (promise, result) = promise_result();
        um.being_uploaded_profile_photos.insert(
            file_upload_id,
            UploadedProfilePhoto {
                user_id: UserId::new(7),
                is_fallback: false,
                only_suggest: false,
                main_frame_timestamp: 0.0,
                is_animation: false,
                reupload_count: 0,
                promise,
            },
        );
        for _ in 0..3 {
            um.on_upload_profile_photo(
                SetProfilePhotoTd {
                    env: &env,
                    files: &mut files,
                    fllm: &mut fllm,
                    upload_callback: &upload_callback,
                    stickers: &stickers,
                },
                file_upload_id,
                None,
            );
            // each round re-arms the upload (the ForceUploadActor [-1]
            // deferral consumes the callback without a result — the
            // entry re-parks with the incremented count)
            assert!(
                um.being_uploaded_profile_photos
                    .contains_key(&file_upload_id),
                "the ladder re-arms"
            );
        }
        um.on_upload_profile_photo(
            SetProfilePhotoTd {
                env: &env,
                files: &mut files,
                fllm: &mut fllm,
                upload_callback: &upload_callback,
                stickers: &stickers,
            },
            file_upload_id,
            None,
        );
        assert!(
            matches!(*result.lock().unwrap(), Some(tdutils::status::Result::Err(ref s)) if s.message() == "Failed to reupload the file")
        );
    }

    #[test]
    fn emoji_status_queries_answer_from_the_pmc_cache_and_park_when_empty() {
        let (mut temp, mut um) = test_env();
        let mut env = temp.env(false);

        // no cache: the get arms park, the cache stays untouched
        let (promise, result) = typed_promise_result();
        um.get_recent_emoji_statuses(&mut env, promise);
        assert_eq!(um.pending_recent_emoji_statuses_queries.len(), 1);
        assert!((*result.lock().unwrap()).is_none());
        um.pending_recent_emoji_statuses_queries.clear();

        // seed the recent cache (the C++ pmc framing) and re-query
        let cache = crate::emoji_status::EmojiStatusesCache {
            hash: 5,
            emoji_statuses: vec![crate::emoji_status::EmojiStatus::from_telegram(
                telegram_api::EmojiStatus::emojiStatus(telegram_api::emojiStatus {
                    flags: 0,
                    document_id: 42,
                    until: 0,
                }),
            )],
        };
        let mut stored = Vec::new();
        cache.store(&mut stored);
        env.global.binlog_pmc().set_raw(
            crate::emoji_status::RECENT_EMOJI_STATUSES_DATABASE_KEY,
            &stored,
        );

        let (promise, result) = typed_promise_result();
        um.get_recent_emoji_statuses(&mut env, promise);
        assert!(um.pending_recent_emoji_statuses_queries.is_empty());
        match &*result.lock().unwrap() {
            Some(tdutils::status::Result::Ok(statuses)) => {
                assert_eq!(statuses.emoji_statuses.len(), 1);
            }
            _ => panic!("the cached list must answer"),
        }

        // clear_recent_emoji_statuses empties the cache and parks the ok
        let (promise, result) = promise_result();
        um.clear_recent_emoji_statuses(&mut env, promise);
        assert_eq!(um.pending_clear_recent_emoji_statuses_queries.len(), 1);
        let cleared = env
            .global
            .binlog_pmc()
            .get_raw(crate::emoji_status::RECENT_EMOJI_STATUSES_DATABASE_KEY);
        let parsed = crate::emoji_status::EmojiStatusesCache::parse(&cleared).unwrap();
        assert_eq!(parsed.hash, 0);
        assert!(parsed.emoji_statuses.is_empty());
        assert!((*result.lock().unwrap()).is_none());
        um.pending_clear_recent_emoji_statuses_queries.clear();
    }

    #[test]
    fn the_emoji_status_intakes_run_the_answer_bands() {
        let (mut temp, mut um) = test_env();
        let mut env = temp.env(false);

        // the default family: the server list saves under its key and
        // flushes its queue (EmojiStatus.cpp:184-198)
        let (promise, result): (
            _,
            std::sync::Arc<
                std::sync::Mutex<Option<tdutils::status::Result<td_api::EmojiStatusCustomEmojis>>>,
            >,
        ) = typed_promise_result();
        um.pending_default_emoji_statuses_queries.push(promise);
        um.on_emoji_status_custom_emojis(
            &mut env,
            false,
            Ok(telegram_api::account_EmojiStatuses::account_emojiStatuses(
                telegram_api::account_emojiStatuses {
                    hash: 7,
                    statuses: vec![telegram_api::EmojiStatus::emojiStatus(
                        telegram_api::emojiStatus {
                            flags: 0,
                            document_id: 42,
                            until: 0,
                        },
                    )],
                },
            )),
        );
        match &*result.lock().unwrap() {
            Some(tdutils::status::Result::Ok(list)) => {
                assert_eq!(list.custom_emoji_ids, vec![42]);
            }
            _ => panic!("the server list must flush the queue"),
        }
        let stored = env
            .global
            .binlog_pmc()
            .get_raw(crate::emoji_status::DEFAULT_EMOJI_STATUSES_DATABASE_KEY);
        assert_eq!(
            crate::emoji_status::EmojiStatusCustomEmojisCache::parse(&stored)
                .unwrap()
                .custom_emoji_ids
                .len(),
            1
        );

        // the notModified band — the C++ 500
        let (promise, result): (
            _,
            std::sync::Arc<
                std::sync::Mutex<Option<tdutils::status::Result<td_api::EmojiStatusCustomEmojis>>>,
            >,
        ) = typed_promise_result();
        um.pending_default_emoji_statuses_queries.push(promise);
        um.on_emoji_status_custom_emojis(
            &mut env,
            false,
            Ok(
                telegram_api::account_EmojiStatuses::account_emojiStatusesNotModified(
                    telegram_api::account_emojiStatusesNotModified,
                ),
            ),
        );
        assert!(
            matches!(&*result.lock().unwrap(), Some(tdutils::status::Result::Err(s)) if s.message() == "Receive wrong server response")
        );

        // the channel family flushes its own queue under its key
        let (promise, result): (
            _,
            std::sync::Arc<
                std::sync::Mutex<Option<tdutils::status::Result<td_api::EmojiStatusCustomEmojis>>>,
            >,
        ) = typed_promise_result();
        um.pending_channel_emoji_statuses_queries.push(promise);
        um.on_emoji_status_custom_emojis(
            &mut env,
            true,
            Ok(telegram_api::account_EmojiStatuses::account_emojiStatuses(
                telegram_api::account_emojiStatuses {
                    hash: 9,
                    statuses: Vec::new(),
                },
            )),
        );
        assert!(matches!(
            &*result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(_))
        ));
        assert!(
            !env.global
                .binlog_pmc()
                .get_raw(crate::emoji_status::DEFAULT_CHANNEL_EMOJI_STATUSES_DATABASE_KEY)
                .is_empty()
        );

        // the upgraded-gift family — the until-date band drops the expiry
        let (promise, result): (
            _,
            std::sync::Arc<
                std::sync::Mutex<Option<tdutils::status::Result<td_api::EmojiStatuses>>>,
            >,
        ) = typed_promise_result();
        um.pending_upgraded_gift_emoji_statuses_queries
            .push(promise);
        um.on_emoji_statuses(
            &mut env,
            true,
            Ok(telegram_api::account_EmojiStatuses::account_emojiStatuses(
                telegram_api::account_emojiStatuses {
                    hash: 3,
                    statuses: vec![telegram_api::EmojiStatus::emojiStatus(
                        telegram_api::emojiStatus {
                            flags: 0,
                            document_id: 5,
                            until: 100,
                        },
                    )],
                },
            )),
        );
        match &*result.lock().unwrap() {
            Some(tdutils::status::Result::Ok(statuses)) => {
                assert_eq!(statuses.emoji_statuses.len(), 1);
            }
            _ => panic!("the collectible list must flush the queue"),
        }
        let stored = env
            .global
            .binlog_pmc()
            .get_raw(crate::emoji_status::UPGRADED_GIFT_EMOJI_STATUSES_DATABASE_KEY);
        assert_eq!(
            crate::emoji_status::EmojiStatusesCache::parse(&stored)
                .unwrap()
                .emoji_statuses[0]
                .get_until_date(),
            0,
            "the temporary band drops the expiry"
        );

        // the clear ok tail re-saves the empty record
        let (promise, result) = promise_result();
        um.pending_clear_recent_emoji_statuses_queries.push(promise);
        um.on_clear_recent_emoji_statuses(&mut env, Ok(()));
        assert!(matches!(
            &*result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(()))
        ));

        // the error tail forwards
        let (promise, result) = promise_result();
        um.pending_clear_recent_emoji_statuses_queries.push(promise);
        um.on_clear_recent_emoji_statuses(&mut env, Err(Status::error(420, "FLOOD_WAIT_5")));
        assert!(
            matches!(&*result.lock().unwrap(), Some(tdutils::status::Result::Err(s)) if s.message() == "FLOOD_WAIT_5")
        );
    }

    #[test]
    fn contacts_query_families_answer_the_local_bands() {
        let (mut temp, mut um) = test_env();
        let mut env = temp.env(false);
        um.set_my_id(&mut env, UserId::new(1));

        // remove_contacts before the load parks on the load promise
        let (promise, result) = promise_result();
        um.remove_contacts(&mut env, vec![UserId::new(5)], promise);
        assert!((*result.lock().unwrap()).is_none(), "the load gate parks");
        assert!(!um.are_contacts_loaded);

        // the loaded tree over the empty store
        load_contacts_via_on_get_contacts(&mut um, &mut env, Vec::new(), 0);

        // remove_contacts with no contacts among the ids answers ok
        // with no query out (the empty input_users band)
        let (promise, result) = promise_result();
        um.remove_contacts(&mut env, vec![UserId::new(7)], promise);
        assert!(matches!(
            *result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(()))
        ));
        assert!(
            um.pending_delete_contacts_queries.is_empty(),
            "no query parked"
        );

        // remove_contacts_by_phone_number with no phones answers ok (the
        // C++ handler send's early promise resolution)
        let (promise, result) = promise_result();
        um.remove_contacts_by_phone_number(&mut env, Vec::new(), Vec::new(), promise);
        assert!(matches!(
            *result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(()))
        ));
        assert!(um.pending_delete_contacts_by_phones_queries.is_empty());

        // clear_imported_contacts over the empty store answers ok
        let (promise, result) = promise_result();
        um.clear_imported_contacts(&mut env, promise);
        assert!(matches!(
            *result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(()))
        ));
        assert!(um.pending_reset_contacts_queries.is_empty());

        // set_close_friends with an unknown user answers the C++ 400
        let (promise, result) = promise_result();
        um.set_close_friends(vec![UserId::new(999)], promise);
        let error = result.lock().unwrap().take().expect("the promise failed");
        let error = error.error().clone();
        assert_eq!((error.code(), error.message()), (400, "User not found"));
        assert!(um.pending_edit_close_friends_queries.is_empty());

        // add_contact with an inaccessible user answers the input error
        let (promise, result) = promise_result();
        um.add_contact(
            &mut env,
            UserId::new(999),
            Contact::new(
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                UserId::default(),
            ),
            false,
            promise,
        );
        let error = result.lock().unwrap().take().expect("the promise failed");
        let error = error.error().clone();
        assert_eq!(error.code(), 400);
        assert!(um.pending_add_contact_queries.is_empty());

        // search_user_by_phone_number with the empty number answers the
        // 200 band
        let (promise, result) = promise_result();
        let found = um.search_user_by_phone_number("+#".to_owned(), false, promise);
        assert_eq!(found, UserId::default());
        let error = result.lock().unwrap().take().expect("the promise failed");
        let error = error.error().clone();
        assert_eq!(
            (error.code(), error.message()),
            (200, "Phone number is invalid")
        );
        assert!(um.pending_resolve_phone_queries.is_empty());
    }

    #[test]
    fn contacts_query_families_park_the_promises_and_handler_state() {
        let (mut temp, mut um) = test_env();
        let mut env = temp.env(false);
        um.set_my_id(&mut env, UserId::new(1));
        // the contact pre-seed — the helper's walk refires the load when
        // the contact arrives fresh (is_is_contact_changed)
        let contact = telegram_contact(2, "Alice", "Cooper");
        um.on_get_user(&mut env, telegram_api::User::user(contact.clone()), "test");
        load_contacts_via_on_get_contacts(&mut um, &mut env, vec![contact], 1);

        // remove_contacts parks the query-carried promise (the §5
        // restore) and pops FIFO
        let (promise, result) = promise_result();
        um.remove_contacts(&mut env, vec![UserId::new(2)], promise);
        assert!(
            (*result.lock().unwrap()).is_none(),
            "the answer completes it"
        );
        assert_eq!(um.pending_delete_contacts_queries.len(), 1);
        // the pop shape: the take fn drains head-first
        let _parked = um
            .take_delete_contacts_query()
            .expect("the parked promise pops");
        assert!(um.take_delete_contacts_query().is_none(), "drained");

        // remove_contacts_by_phone_number parks the user ids with the
        // promise (the on_deleted_contacts intake state)
        let (promise, _result) = promise_result();
        um.remove_contacts_by_phone_number(
            &mut env,
            vec!["+123".to_owned()],
            vec![UserId::new(2)],
            promise,
        );
        let (user_ids, _promise) = um
            .take_delete_contacts_by_phones_query()
            .expect("the pair pops");
        assert_eq!(user_ids, vec![UserId::new(2)]);

        // clear_imported_contacts over a non-empty store parks (the
        // ResetContacts promise)
        let (promise, _result) = promise_result();
        um.clear_imported_contacts(&mut env, promise);
        assert!(
            um.take_reset_contacts_query().is_some(),
            "the reset promise parks"
        );

        // set_close_friends over the seeded contact parks the ids
        let (promise, _result) = promise_result();
        um.set_close_friends(vec![UserId::new(2)], promise);
        let (user_ids, _promise) = um
            .take_edit_close_friends_query()
            .expect("the close-friends pair pops");
        assert_eq!(user_ids, vec![UserId::new(2)]);

        // hide_contact_birthdays parks the dismiss promise
        let (promise, _result) = promise_result();
        um.hide_contact_birthdays(promise);
        assert!(
            um.take_dismiss_birthdays_query().is_some(),
            "the dismiss promise parks"
        );

        // share_phone_number over the loaded tree parks the accept pair
        let (promise, _result) = promise_result();
        um.share_phone_number(&mut env, UserId::new(2), promise);
        let (user_id, _promise) = um
            .take_accept_contact_query()
            .expect("the accept pair pops");
        assert_eq!(user_id, UserId::new(2));

        // add_contact parks the user id with the promise
        let (promise, _result) = promise_result();
        um.add_contact(
            &mut env,
            UserId::new(2),
            Contact::new(
                "+123".to_owned(),
                "Alice".to_owned(),
                "Cooper".to_owned(),
                String::new(),
                UserId::default(),
            ),
            false,
            promise,
        );
        let (user_id, _promise) = um.take_add_contact_query().expect("the add pair pops");
        assert_eq!(user_id, UserId::new(2));

        // the miss leg of the phone search parks the cleaned phone
        let (promise, _result) = promise_result();
        let found = um.search_user_by_phone_number("+9 (555)".to_owned(), false, promise);
        assert_eq!(found, UserId::default());
        let (phone, _promise) = um
            .take_resolve_phone_query()
            .expect("the resolve pair pops");
        assert_eq!(phone, "9555", "the digits-only cleaning");
    }
}
