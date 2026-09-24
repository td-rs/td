//! Port of the `td/telegram/StickersManager.{h,cpp}` request plane (the
//! M.st slice) — the 38 td_api sticker arms over the manager's local
//! validation and decision tables. The C++ manager is a 10.4k-line actor
//! whose state (sticker sets, found-sticker caches, emoji-group lists)
//! loads over the network and the database; offline the caches answer
//! their not-loaded bands, which this port keeps exact: the load/query
//! tails park at the `[blocked: net]`/`[blocked: Files]` seams (the
//! promise lands unanswered, the translation-manager convention), the
//! synchronous validation bands and the cache-hit tables are ported
//! C++-exact and covered by the inline tests.
//!
//! Seams:
//! - every `create_handler<...>` query tail — `[blocked: net]`
//! - the sqlite-pmc/database loads (`load_custom_emoji_sticker_from_database`,
//!   the found-stickers/emoji-group database keys) — `[blocked: net]` class
//! - the special-sticker-set loads (animated emoji, the emoji-status and
//!   topic-icon sets, the premium gift set) — `[blocked: net]`
//! - the file/upload half of `create_new_sticker_set`/`upload_sticker_file`
//!   — `[blocked: Files]` (the M.12 upload orchestration)
//! - the recorded null-response divergences (`get_sticker_outline_object`
//!   over an unknown sticker answers JSON null in C++) park like the M.dp
//!   `send_null_user` band.

use crate::documents_manager::StickerFormat;
use crate::files::FileType;
use crate::messages_manager::MessageId;
use crate::misc::{
    clean_input_string, clean_username, replace_with_spaces, strip_empty_characters,
};
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Result;

/// `StickersManager::MAX_FOUND_STICKERS` (StickersManager.h:481).
pub(crate) const MAX_FOUND_STICKERS: i32 = 100;

/// `StickersManager::MAX_STICKER_SET_TITLE_LENGTH` (StickersManager.h:482).
pub(crate) const MAX_STICKER_SET_TITLE_LENGTH: usize = 64;

/// `StickersManager::MAX_STICKER_SET_SHORT_NAME_LENGTH`
/// (StickersManager.h:483).
pub(crate) const MAX_STICKER_SET_SHORT_NAME_LENGTH: usize = 64;

/// `StickersManager::MAX_GET_CUSTOM_EMOJI_STICKERS`
/// (StickersManager.h:484).
pub(crate) const MAX_GET_CUSTOM_EMOJI_STICKERS: usize = 200;

/// `StickersManager::GREAT_MINDS_SET_ID` (StickersManager.h:374).
pub(crate) const GREAT_MINDS_SET_ID: i64 = 1258616945789056;

/// `StickersManager::GREAT_MINDS_COLOR_SET_ID`
/// (StickersManager.cpp:4984) — the repainted Great Minds twin.
pub(crate) const GREAT_MINDS_COLOR_SET_ID: i64 = 151353307481243663;

/// `StickerType` (StickerType.h:14-18).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum StickerType {
    #[default]
    Regular = 0,
    Mask,
    CustomEmoji,
}

/// `MAX_STICKER_TYPE` (StickersManager.h:444).
pub(crate) const MAX_STICKER_TYPE: usize = 3;

/// `get_sticker_type` (StickerType.cpp:23-39) — the td_api table; `None`
/// maps to `Regular`.
pub fn get_sticker_type(td_api_type: Option<&td_api::StickerType>) -> StickerType {
    let Some(td_api_type) = td_api_type else {
        return StickerType::Regular;
    };
    match td_api_type {
        td_api::StickerType::stickerTypeRegular(_) => StickerType::Regular,
        td_api::StickerType::stickerTypeMask(_) => StickerType::Mask,
        td_api::StickerType::stickerTypeCustomEmoji(_) => StickerType::CustomEmoji,
        #[allow(unreachable_patterns)]
        _ => {
            tracing::error!("Receive unsupported sticker type");
            StickerType::Regular
        }
    }
}

/// `get_sticker_format` (StickerFormat.cpp:13-26) — `None` maps to
/// `Unknown`.
pub fn get_sticker_format(td_api_format: Option<&td_api::StickerFormat>) -> StickerFormat {
    let Some(td_api_format) = td_api_format else {
        return StickerFormat::Unknown;
    };
    match td_api_format {
        td_api::StickerFormat::stickerFormatWebp(_) => StickerFormat::Webp,
        td_api::StickerFormat::stickerFormatTgs(_) => StickerFormat::Tgs,
        td_api::StickerFormat::stickerFormatWebm(_) => StickerFormat::Webm,
        #[allow(unreachable_patterns)]
        _ => {
            tracing::error!("Receive unsupported sticker format");
            StickerFormat::Unknown
        }
    }
}

/// `EmojiGroupType` (EmojiGroupType.h:14-19).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum EmojiGroupType {
    #[default]
    Default = 0,
    EmojiStatus,
    ProfilePhoto,
    RegularStickers,
}

/// `get_emoji_group_type` (EmojiGroupType.cpp:15-30) — the td_api table.
pub fn get_emoji_group_type(td_api_type: Option<&td_api::EmojiCategoryType>) -> EmojiGroupType {
    let Some(td_api_type) = td_api_type else {
        return EmojiGroupType::Default;
    };
    match td_api_type {
        td_api::EmojiCategoryType::emojiCategoryTypeDefault(_) => EmojiGroupType::Default,
        td_api::EmojiCategoryType::emojiCategoryTypeEmojiStatus(_) => EmojiGroupType::EmojiStatus,
        td_api::EmojiCategoryType::emojiCategoryTypeChatPhoto(_) => EmojiGroupType::ProfilePhoto,
        td_api::EmojiCategoryType::emojiCategoryTypeRegularStickers(_) => {
            EmojiGroupType::RegularStickers
        }
        #[allow(unreachable_patterns)]
        _ => {
            tracing::error!("Receive unsupported emoji category type");
            EmojiGroupType::Default
        }
    }
}

/// `StickerListType` (StickerListType.h:14).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum StickerListType {
    DialogPhoto = 0,
    UserProfilePhoto,
    Background,
    DisallowedChannelEmojiStatus,
}

/// `MAX_STICKER_LIST_TYPE` (StickerListType.h:16).
pub(crate) const MAX_STICKER_LIST_TYPE: usize = 4;

/// `CustomEmojiId` (CustomEmojiId.h:18-38).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CustomEmojiId(i64);

impl CustomEmojiId {
    pub fn new(id: i64) -> CustomEmojiId {
        CustomEmojiId(id)
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// `CustomEmojiId::is_valid` (CustomEmojiId.h:29-31) — the C++ id
    /// range check.
    pub fn is_valid(self) -> bool {
        (self.0 >= (1 << 20)) && (self.0 <= (i64::from(i32::MAX) - (1 << 19)))
    }

    /// `CustomEmojiId::get_custom_emoji_ids`
    /// (CustomEmojiId.h:33-37) — the raw-id vector passthrough.
    pub fn get_custom_emoji_ids(ids: &[i64]) -> Vec<CustomEmojiId> {
        ids.iter().map(|&id| CustomEmojiId::new(id)).collect()
    }
}

/// `StickerSetId` (StickerSetId.h:18-38) — the stickers-family id (the
/// chat-manager's `StickerSetId` is a different plane).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StickerSetId(i64);

impl StickerSetId {
    pub fn new(id: i64) -> StickerSetId {
        StickerSetId(id)
    }

    pub fn get(self) -> i64 {
        self.0
    }

    pub fn is_valid(self) -> bool {
        self.0 != 0
    }

    /// `StickerSetId::get_sticker_set_ids` (StickerSetId.h:33-37).
    pub fn get_sticker_set_ids(ids: &[i64]) -> Vec<StickerSetId> {
        ids.iter().map(|&id| StickerSetId::new(id)).collect()
    }
}

/// `StickersManager::CheckStickerSetNameResult`
/// (StickersManager.h:454-462).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckStickerSetNameResult {
    Ok,
    Invalid,
    Occupied,
}

/// `get_check_sticker_set_name_result_object`
/// (StickersManager.cpp:8093-8102).
pub fn get_check_sticker_set_name_result_object(
    result: CheckStickerSetNameResult,
) -> td_api::CheckStickerSetNameResult {
    match result {
        CheckStickerSetNameResult::Ok => {
            td_api::CheckStickerSetNameResult::checkStickerSetNameResultOk(
                td_api::checkStickerSetNameResultOk {},
            )
        }
        CheckStickerSetNameResult::Invalid => {
            td_api::CheckStickerSetNameResult::checkStickerSetNameResultNameInvalid(
                td_api::checkStickerSetNameResultNameInvalid {},
            )
        }
        CheckStickerSetNameResult::Occupied => {
            td_api::CheckStickerSetNameResult::checkStickerSetNameResultNameOccupied(
                td_api::checkStickerSetNameResultNameOccupied {},
            )
        }
    }
}

/// `SpecialStickerSetType` (StickersManager.h:298-342) — the six
/// special sets the request plane reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SpecialStickerSetType {
    AnimatedEmoji,
    GenericAnimations,
    DefaultTopicIcons,
    DefaultStatuses,
    DefaultChannelStatuses,
    PremiumGifts,
}

/// `StickersManager::get_max_sticker_file_size`
/// (StickerFormat.cpp:151-166).
pub fn get_max_sticker_file_size(
    sticker_format: StickerFormat,
    sticker_type: StickerType,
    for_thumbnail: bool,
) -> i64 {
    let is_custom_emoji = sticker_type == StickerType::CustomEmoji;
    match sticker_format {
        StickerFormat::Unknown | StickerFormat::Webp => {
            if for_thumbnail || is_custom_emoji {
                1 << 17
            } else {
                1 << 19
            }
        }
        StickerFormat::Tgs => {
            if for_thumbnail {
                1 << 15
            } else {
                1 << 16
            }
        }
        StickerFormat::Webm => {
            if for_thumbnail {
                1 << 15
            } else if is_custom_emoji {
                1 << 16
            } else {
                1 << 18
            }
        }
        #[allow(unreachable_patterns)]
        _ => {
            tracing::error!("Have unknown sticker format");
            0
        }
    }
}

/// The `FileView` bands the sticker request plane reads, flattened out
/// of the C++ `FileView` (the `Td` impl fills it over the FileManager
/// registry; the offline default is the all-false unknown view).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StickerFileView {
    pub is_encrypted: bool,
    /// `get_main_remote_location()` non-null and a non-web document —
    /// the C++ band collapses both checks into the `Option`.
    pub main_remote_document_id: Option<i64>,
    pub has_url: bool,
    pub has_full_local_location: bool,
    pub expected_size: i64,
    /// `get_full_remote_location()` non-null and a document.
    pub full_remote_document_id: Option<i64>,
}

/// The `G()`/manager slice the collapsed StickersManager reads through.
pub trait StickersManagerEnv {
    /// `td_->auth_manager_->is_bot()`.
    fn auth_is_bot(&self) -> bool;
    /// `G()->close_flag()`.
    fn close_flag(&self) -> bool;
    /// `G()->unix_time()`.
    fn unix_time(&self) -> i32;
    /// `Time::now()` — the process-monotonic clock.
    fn now(&self) -> f64;
    /// `td_->option_manager_->get_option_boolean(name, default)`.
    fn get_option_boolean(&self, name: &str, default_value: bool) -> bool {
        let _ = (name, default_value);
        false
    }

    // -- the user/file planes (real over `Td`)
    /// `user_manager_->get_my_id()`.
    fn get_my_id(&self) -> UserId;
    /// `user_manager_->get_input_user(user_id)`.
    fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser>;
    /// `file_manager_->get_input_file_id(file_type, input_file, DialogId(),
    /// expect_thumbnail, false)` — real over the FileManager registry.
    fn file_get_input_file_id(
        &mut self,
        file_type: FileType,
        input_file: Option<&td_api::InputFile>,
        expect_thumbnail: bool,
    ) -> Result<crate::files::FileId>;
    /// the `FileManager::get_file_view` read flattened into
    /// [`StickerFileView`] — real over the FileManager registry.
    fn file_get_view(&mut self, file_id: crate::files::FileId) -> StickerFileView;
    /// `messages_manager_->get_file_message_object(file_id)` — the
    /// `finish_upload_sticker_file` answer over the already-remote band.
    fn file_get_file_object(&mut self, _file_id: crate::files::FileId) -> Option<td_api::File> {
        None
    }

    // -- the `[blocked: net]`/`[blocked: Files]` query tails; each one
    // -- owns the promise, which keeps the C++ request unanswered offline
    /// `load_special_sticker_set` — the animated-emoji / emoji-status /
    /// topic-icon / premium-gift set loads (the special-set plane).
    fn load_special_sticker_set(&mut self, _set_type: SpecialStickerSetType) {}
    /// `CheckStickerSetShortNameQuery` (StickersManager.cpp:8125-8132);
    /// the SHORT_NAME_INVALID/OCCUPIED result mapping rides the net step.
    fn send_check_sticker_set_short_name_query(
        &mut self,
        _name: String,
        _promise: Promise<CheckStickerSetNameResult>,
    ) {
    }
    /// `GetStickerSetNameQuery` (the name-load-query continuation).
    fn send_get_sticker_set_name_query(&mut self, _set_id: StickerSetId) {}
    /// `SuggestStickerSetShortNameQuery`.
    fn send_suggest_sticker_set_short_name_query(
        &mut self,
        _title: String,
        _promise: Promise<String>,
    ) {
    }
    /// `GetEmojiUrlQuery`.
    fn send_get_emoji_suggestions_url_query(
        &mut self,
        _language_code: String,
        _promise: Promise<String>,
    ) {
    }
    /// the found-stickers cache miss: the sqlite-pmc database load or the
    /// `SearchStickersQuery` reload (StickersManager.cpp:4590-4610).
    fn load_found_stickers(
        &mut self,
        _sticker_type: StickerType,
        _emoji: String,
        _promise: Promise<td_api::Stickers>,
    ) {
    }
    /// the found-stickers cache refresh after a fresh hit
    /// (`reload_found_stickers`).
    fn reload_found_stickers(&mut self, _sticker_type: StickerType, _emoji: String) {}
    /// the emoji-group database load or `getAvailableReactions`-style
    /// reload (StickersManager.cpp:10271-10283).
    fn load_emoji_groups(
        &mut self,
        _group_type: EmojiGroupType,
        _used_language_codes: String,
        _promise: Promise<td_api::EmojiCategories>,
    ) {
    }
    /// `load_default_custom_emoji_ids` (the default-list plane).
    fn load_default_custom_emoji_ids(&mut self, _list_type: StickerListType, _force_reload: bool) {}
    /// `load_custom_emoji_sticker_from_database` (the pmc half).
    fn load_custom_emoji_sticker_from_database(&mut self, _custom_emoji_id: CustomEmojiId) {}
    /// `GetCustomEmojiDocumentsQuery`.
    fn send_get_custom_emoji_documents_query(&mut self, _custom_emoji_ids: Vec<CustomEmojiId>) {}
    /// `GetMyStickersQuery`.
    fn send_get_my_stickers_query(
        &mut self,
        _offset_sticker_set_id: StickerSetId,
        _limit: i32,
        _promise: Promise<td_api::StickerSets>,
    ) {
    }
    /// `do_reload_sticker_set` over `inputStickerSetShortName` — the
    /// add/thumbnail continuation load.
    fn reload_sticker_set_by_name(&mut self, _short_name: String) {}
    /// `ReorderStickerSetsQuery` — rides the local reorder answer.
    fn send_reorder_sticker_sets_query(
        &mut self,
        _sticker_type: StickerType,
        _sticker_set_ids: Vec<StickerSetId>,
    ) {
    }
    /// `DeleteStickerFromSetQuery`.
    fn send_delete_sticker_from_set_query(
        &mut self,
        _input_document: telegram_api::InputDocument,
        _promise: Promise<()>,
    ) {
    }
    /// `ChangeStickerQuery` — the setSticker{Emojis,Keywords,MaskPosition}
    /// funnel.
    #[allow(clippy::too_many_arguments)]
    fn send_change_sticker_query(
        &mut self,
        _input_document: telegram_api::InputDocument,
        _change_emojis: bool,
        _emojis: String,
        _mask_position: Option<td_api::MaskPosition>,
        _change_keywords: bool,
        _keywords: String,
        _promise: Promise<()>,
    ) {
    }
    /// `SetStickerPositionQuery`.
    fn send_set_sticker_position_query(
        &mut self,
        _input_document: telegram_api::InputDocument,
        _position: i32,
        _promise: Promise<()>,
    ) {
    }
    /// `SetStickerSetTitleQuery`.
    fn send_set_sticker_set_title_query(
        &mut self,
        _short_name: String,
        _title: String,
        _promise: Promise<()>,
    ) {
    }
    /// `DeleteStickerSetQuery`.
    fn send_delete_sticker_set_query(&mut self, _short_name: String, _promise: Promise<()>) {}
    /// the `PendingNewStickerSet` upload machinery of
    /// `create_new_sticker_set` — `[blocked: Files]`; the promise lands
    /// unanswered offline.
    #[allow(clippy::too_many_arguments)]
    fn start_new_sticker_set_upload(
        &mut self,
        _user_id: UserId,
        _title: String,
        _short_name: String,
        _sticker_type: StickerType,
        _needs_repainting: bool,
        _software: String,
        _file_ids: Vec<crate::files::FileId>,
        _promise: Promise<td_api::StickerSet>,
    ) {
    }
    /// the `do_add_sticker_to_set` continuation after the set reload —
    /// `[blocked: Files]`/net.
    fn start_add_sticker_to_set(
        &mut self,
        _user_id: UserId,
        _short_name: String,
        _sticker: td_api::NewSticker,
        _old_sticker: Option<td_api::InputFile>,
        _promise: Promise<()>,
    ) {
    }
    /// the `do_set_sticker_set_thumbnail` continuation — `[blocked: Files]`.
    fn start_set_sticker_set_thumbnail(
        &mut self,
        _user_id: UserId,
        _short_name: String,
        _thumbnail: Option<td_api::InputFile>,
        _format: StickerFormat,
        _promise: Promise<()>,
    ) {
    }
    /// the `do_set_custom_emoji_sticker_set_thumbnail` continuation —
    /// `[blocked: Files]`.
    fn start_set_custom_emoji_sticker_set_thumbnail(
        &mut self,
        _short_name: String,
        _custom_emoji_id: CustomEmojiId,
        _promise: Promise<()>,
    ) {
    }
    /// the sticker-file upload leg (`upload_sticker_file` over a local or
    /// URL input) — `[blocked: Files]`.
    fn start_sticker_file_upload(&mut self, _file_id: crate::files::FileId) {}
    /// the composite `get_premium_stickers` load — the MultiPromise over
    /// the found-sticker + installed-sticker planes.
    fn load_premium_stickers(&mut self, _limit: i32, _promise: Promise<td_api::Stickers>) {}
    /// `view_featured_sticker_sets`' delayed server read
    /// (`read_featured_sticker_sets` over the views timeout).
    fn send_read_featured_sticker_sets(&mut self) {}
}

/// `StickersManager` — the collapsed state the request plane reads and
/// mutates. The caches start empty (offline the sets never load) and the
/// tests pre-seed them to drive the cache-hit tables.
#[derive(Default)]
pub struct StickersManager {
    /// `are_installed_sticker_sets_loaded_[type]`.
    are_installed_sticker_sets_loaded: [bool; MAX_STICKER_TYPE],
    /// `installed_sticker_set_ids_[type]`.
    installed_sticker_set_ids: [Vec<StickerSetId>; MAX_STICKER_TYPE],
    /// `custom_emoji_to_sticker_id_` (the WaitFreeHashMap collapses to a
    /// plain map).
    custom_emoji_to_sticker_id: std::collections::HashMap<CustomEmojiId, MessageId>,
    /// `found_stickers_[type][emoji]` — the `(sticker_ids,
    /// next_reload_time)` cache.
    found_stickers: [std::collections::HashMap<String, (Vec<MessageId>, f64)>; MAX_STICKER_TYPE],
    /// `are_default_custom_emoji_ids_loaded_[index]`.
    are_default_custom_emoji_ids_loaded: [bool; MAX_STICKER_LIST_TYPE],
    /// `default_custom_emoji_ids_[index]`.
    default_custom_emoji_ids: [Vec<CustomEmojiId>; MAX_STICKER_LIST_TYPE],
}

/// The found-stickers cache entry key builder — the C++ rekeys
/// `emoji'\xFF'query'\xFF'langs'\xFF'offset'\xFF'limit`
/// (StickersManager.cpp:4567-4570).
fn search_cache_key(
    emoji: &str,
    query: &str,
    language_codes: &[String],
    offset: i32,
    limit: i32,
) -> String {
    if !query.is_empty() || offset > 0 {
        format!(
            "{}\u{ff}{}\u{ff}{}\u{ff}{}\u{ff}{}",
            emoji,
            query,
            language_codes.join(" "),
            offset,
            limit
        )
    } else {
        emoji.to_owned()
    }
}

impl StickersManager {
    pub fn new() -> StickersManager {
        StickersManager::default()
    }

    /// `get_stickers_object` over the collapsed cache — offline the id
    /// vector resolves to no sticker objects, so the answer is the empty
    /// `stickers` (the C++ builds per-id objects; the unknown-id band
    /// drops them).
    fn get_stickers_object(sticker_ids: &[MessageId]) -> td_api::Stickers {
        let _ = sticker_ids;
        td_api::Stickers {
            stickers: Vec::new(),
        }
    }

    /// `get_custom_emoji_stickers_object` over the collapsed cache —
    /// same class as [`Self::get_stickers_object`].
    fn get_custom_emoji_stickers_object(custom_emoji_ids: &[CustomEmojiId]) -> td_api::Stickers {
        let _ = custom_emoji_ids;
        td_api::Stickers {
            stickers: Vec::new(),
        }
    }

    /// `search_stickers` (StickersManager.cpp:4548-4610).
    #[allow(clippy::too_many_arguments)]
    pub fn search_stickers(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_type: StickerType,
        emoji: String,
        query: String,
        input_language_codes: Vec<String>,
        offset: i32,
        limit: i32,
        mut promise: Promise<td_api::Stickers>,
    ) {
        if offset < 0 {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Parameter offset must be non-negative",
            ));
            return;
        }
        if limit == 0 {
            promise.set_value(Self::get_stickers_object(&[]));
            return;
        }
        if limit < 0 {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Parameter limit must be positive",
            ));
            return;
        }
        let limit = limit.min(MAX_FOUND_STICKERS);

        let emoji = tdutils::emoji::remove_emoji_modifiers(&emoji, false);
        let key = search_cache_key(&emoji, &query, &input_language_codes, offset, limit);
        if emoji.is_empty() || sticker_type == StickerType::Mask {
            promise.set_value(Self::get_stickers_object(&[]));
            return;
        }

        let type_index = sticker_type as usize;
        if let Some((sticker_ids, next_reload_time)) = self.found_stickers[type_index].get(&key) {
            let result_size = (limit as usize).min(sticker_ids.len());
            promise.set_value(Self::get_stickers_object(&sticker_ids[..result_size]));
            if env.now() < *next_reload_time {
                return;
            }
            // the fresh-cache band: the answer went out, the reload
            // re-queries with limit 0 (the promise is dropped here, the
            // C++ moves an empty one in)
            env.reload_found_stickers(sticker_type, key);
            return;
        }

        // the cache miss: the database load (offset 0) or the server
        // query parks at the seam
        env.load_found_stickers(sticker_type, key, promise);
    }

    /// `get_animated_emoji` (StickersManager.cpp:6162-6189) and the four
    /// same-shaped special-set readers — the not-loaded band parks at
    /// the load seam; the `is_recursive` empty answers are the internal
    /// re-entry only.
    pub fn get_animated_emoji(
        &mut self,
        env: &mut impl StickersManagerEnv,
        emoji: String,
        is_recursive: bool,
        mut promise: Promise<td_api::AnimatedEmoji>,
    ) {
        let _ = &emoji;
        if env.close_flag() {
            promise.set_error(tdutils::status::Status::error(500, "Request aborted"));
            return;
        }
        if !is_recursive {
            // the C++ queues the promise on the special-set load and
            // parks; the is_recursive re-entry answers null over the
            // never-loaded set — the recorded null-response divergence,
            // equally a park here
            env.load_special_sticker_set(SpecialStickerSetType::AnimatedEmoji);
            return;
        }
        // the C++ promise.set_value(nullptr) is inexpressible — park
        let _ = promise;
    }

    /// `get_custom_emoji_reaction_generic_animations`
    /// (StickersManager.cpp:6221-6246).
    pub fn get_custom_emoji_reaction_generic_animations(
        &mut self,
        env: &mut impl StickersManagerEnv,
        is_recursive: bool,
        mut promise: Promise<td_api::Stickers>,
    ) {
        if env.close_flag() {
            promise.set_error(tdutils::status::Status::error(500, "Request aborted"));
            return;
        }
        if is_recursive {
            promise.set_value(Self::get_stickers_object(&[]));
            return;
        }
        env.load_special_sticker_set(SpecialStickerSetType::GenericAnimations);
    }

    /// `get_default_topic_icons` (StickersManager.cpp:6345-6390).
    pub fn get_default_topic_icons(
        &mut self,
        env: &mut impl StickersManagerEnv,
        is_recursive: bool,
        mut promise: Promise<td_api::Stickers>,
    ) {
        if env.close_flag() {
            promise.set_error(tdutils::status::Status::error(500, "Request aborted"));
            return;
        }
        if is_recursive {
            promise.set_value(Self::get_stickers_object(&[]));
            return;
        }
        env.load_special_sticker_set(SpecialStickerSetType::DefaultTopicIcons);
    }

    /// `get_default_emoji_statuses` (StickersManager.cpp:6248-6305) —
    /// the loaded band answers the first 8 custom emoji ids of the set.
    pub fn get_default_emoji_statuses(
        &mut self,
        env: &mut impl StickersManagerEnv,
        is_recursive: bool,
        promise: Promise<td_api::EmojiStatusCustomEmojis>,
    ) {
        self.get_special_emoji_statuses(
            env,
            SpecialStickerSetType::DefaultStatuses,
            is_recursive,
            promise,
        );
    }

    /// `get_default_channel_emoji_statuses`
    /// (StickersManager.cpp:6307-6343).
    pub fn get_default_channel_emoji_statuses(
        &mut self,
        env: &mut impl StickersManagerEnv,
        is_recursive: bool,
        promise: Promise<td_api::EmojiStatusCustomEmojis>,
    ) {
        self.get_special_emoji_statuses(
            env,
            SpecialStickerSetType::DefaultChannelStatuses,
            is_recursive,
            promise,
        );
    }

    /// the shared body of the two emoji-status special-set readers.
    fn get_special_emoji_statuses(
        &mut self,
        env: &mut impl StickersManagerEnv,
        set_type: SpecialStickerSetType,
        is_recursive: bool,
        mut promise: Promise<td_api::EmojiStatusCustomEmojis>,
    ) {
        if env.close_flag() {
            promise.set_error(tdutils::status::Status::error(500, "Request aborted"));
            return;
        }
        if is_recursive {
            promise.set_value(td_api::EmojiStatusCustomEmojis {
                custom_emoji_ids: Vec::new(),
            });
            return;
        }
        env.load_special_sticker_set(set_type);
    }

    /// `get_custom_emoji_stickers` (StickersManager.cpp:6532-6606).
    pub fn get_custom_emoji_stickers(
        &mut self,
        env: &mut impl StickersManagerEnv,
        mut custom_emoji_ids: Vec<CustomEmojiId>,
        use_database: bool,
        mut promise: Promise<td_api::Stickers>,
    ) {
        if env.close_flag() {
            promise.set_error(tdutils::status::Status::error(500, "Request aborted"));
            return;
        }
        if custom_emoji_ids.len() > MAX_GET_CUSTOM_EMOJI_STICKERS {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Too many custom emoji identifiers specified",
            ));
            return;
        }

        let mut unique = std::collections::HashSet::new();
        custom_emoji_ids.retain(|custom_emoji_id| {
            custom_emoji_id.is_valid() && unique.insert(*custom_emoji_id)
        });

        let unknown_custom_emoji_ids: Vec<CustomEmojiId> = custom_emoji_ids
            .iter()
            .copied()
            .filter(|custom_emoji_id| {
                !self
                    .custom_emoji_to_sticker_id
                    .contains_key(custom_emoji_id)
            })
            .collect();

        if unknown_custom_emoji_ids.is_empty() {
            promise.set_value(Self::get_custom_emoji_stickers_object(&custom_emoji_ids));
            return;
        }

        if use_database && env.get_option_boolean("use_sqlite_pmc", false) {
            for custom_emoji_id in unknown_custom_emoji_ids {
                env.load_custom_emoji_sticker_from_database(custom_emoji_id);
            }
            return;
        }

        env.send_get_custom_emoji_documents_query(unknown_custom_emoji_ids);
    }

    /// `get_default_custom_emoji_stickers`
    /// (StickersManager.cpp:6608-6618).
    pub fn get_default_custom_emoji_stickers(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_list_type: StickerListType,
        force_reload: bool,
        mut promise: Promise<td_api::Stickers>,
    ) {
        let index = sticker_list_type as usize;
        if self.are_default_custom_emoji_ids_loaded[index] && !force_reload {
            let ids = self.default_custom_emoji_ids[index].clone();
            promise.set_value(Self::get_custom_emoji_stickers_object(&ids));
            return;
        }
        let _ = promise;
        env.load_default_custom_emoji_ids(sticker_list_type, force_reload);
    }

    /// `get_sticker_list_emoji_statuses` (StickersManager.cpp:6620-6630).
    pub fn get_sticker_list_emoji_statuses(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_list_type: StickerListType,
        force_reload: bool,
        mut promise: Promise<td_api::EmojiStatusCustomEmojis>,
    ) {
        let index = sticker_list_type as usize;
        if self.are_default_custom_emoji_ids_loaded[index] && !force_reload {
            promise.set_value(td_api::EmojiStatusCustomEmojis {
                custom_emoji_ids: self.default_custom_emoji_ids[index]
                    .iter()
                    .map(|id| id.get())
                    .collect(),
            });
            return;
        }
        let _ = promise;
        env.load_default_custom_emoji_ids(sticker_list_type, force_reload);
    }

    /// `get_emoji_suggestions_url` (StickersManager.cpp:10248-10250).
    pub fn get_emoji_suggestions_url(
        &mut self,
        env: &mut impl StickersManagerEnv,
        language_code: String,
        promise: Promise<String>,
    ) {
        env.send_get_emoji_suggestions_url_query(language_code, promise);
    }

    /// `get_emoji_groups` (StickersManager.cpp:10256-10284) — the
    /// used-language-codes cache key collapses to the option set; the
    /// hit band answers and the expired/miss bands park.
    pub fn get_emoji_groups(
        &mut self,
        env: &mut impl StickersManagerEnv,
        group_type: EmojiGroupType,
        promise: Promise<td_api::EmojiCategories>,
    ) {
        let used_language_codes = String::new();
        let _ = used_language_codes;
        // the collapsed emoji_group_list_ starts empty — the hit band is
        // unreachable offline; the load parks
        let _ = promise;
        env.load_emoji_groups(group_type, String::new(), promise);
    }

    /// `load_premium_gift_sticker` (StickersManager.cpp:2648-2662) — the
    /// premium-gift set never loads offline; the pending queue parks.
    pub fn load_premium_gift_sticker(
        &mut self,
        env: &mut impl StickersManagerEnv,
        month_count: i32,
        star_count: i64,
        mut promise: Promise<td_api::Sticker>,
    ) {
        let _ = (month_count, star_count, &mut promise);
        env.load_special_sticker_set(SpecialStickerSetType::PremiumGifts);
    }

    /// `get_premium_stickers` (StickersManager.cpp:4853-4883) — the
    /// limit bands are local; the MultiPromise composite parks.
    pub fn get_premium_stickers(
        &mut self,
        env: &mut impl StickersManagerEnv,
        limit: i32,
        mut promise: Promise<td_api::Stickers>,
    ) {
        if env.close_flag() {
            promise.set_error(tdutils::status::Status::error(500, "Request aborted"));
            return;
        }
        if limit == 0 {
            promise.set_value(Self::get_stickers_object(&[]));
            return;
        }
        let limit = limit.min(MAX_FOUND_STICKERS);
        env.load_premium_stickers(limit, promise);
    }

    /// `get_created_sticker_sets` (StickersManager.cpp:8785-8799).
    pub fn get_created_sticker_sets(
        &mut self,
        env: &mut impl StickersManagerEnv,
        offset_sticker_set_id: StickerSetId,
        limit: i32,
        mut promise: Promise<td_api::StickerSets>,
    ) {
        if limit <= 0 {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Parameter limit must be positive",
            ));
            return;
        }
        env.send_get_my_stickers_query(offset_sticker_set_id, limit, promise);
    }

    /// `get_sticker_set_name` (StickersManager.cpp:4982-4999).
    pub fn get_sticker_set_name(
        &mut self,
        env: &mut impl StickersManagerEnv,
        set_id: StickerSetId,
        mut promise: Promise<String>,
    ) {
        if set_id.get() == GREAT_MINDS_SET_ID || set_id.get() == GREAT_MINDS_COLOR_SET_ID {
            promise.set_value("TelegramGreatMinds".to_owned());
            return;
        }

        // the sticker_set == nullptr band — the 400 error
        promise.set_error(tdutils::status::Status::error(400, "Sticker set not found"));
        let _ = env;
    }

    /// `get_suggested_sticker_set_name` (StickersManager.cpp:8095-8102).
    pub fn get_suggested_sticker_set_name(
        &mut self,
        env: &mut impl StickersManagerEnv,
        title: String,
        mut promise: Promise<String>,
    ) {
        let title = strip_empty_characters(&title, MAX_STICKER_SET_TITLE_LENGTH, false);
        if title.is_empty() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Sticker set title must be non-empty",
            ));
            return;
        }
        env.send_suggest_sticker_set_short_name_query(title, promise);
    }

    /// `check_sticker_set_name` (StickersManager.cpp:8104-8123).
    pub fn check_sticker_set_name(
        &mut self,
        env: &mut impl StickersManagerEnv,
        name: String,
        mut promise: Promise<CheckStickerSetNameResult>,
    ) {
        if name.is_empty() {
            promise.set_value(CheckStickerSetNameResult::Invalid);
            return;
        }
        env.send_check_sticker_set_short_name_query(name, promise);
    }

    /// `view_featured_sticker_sets` (StickersManager.cpp:7165-7191) —
    /// over the collapsed state the set lookups all miss, so the body is
    /// the no-op + the scheduled server read (which parks).
    pub fn view_featured_sticker_sets(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_set_ids: &[StickerSetId],
    ) {
        let _ = sticker_set_ids;
        env.send_read_featured_sticker_sets();
    }

    /// `reorder_installed_sticker_sets` (StickersManager.cpp:7885-7901).
    pub fn reorder_installed_sticker_sets(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_type: StickerType,
        sticker_set_ids: Vec<StickerSetId>,
        mut promise: Promise<()>,
    ) {
        let result = self.apply_installed_sticker_sets_order(sticker_type, &sticker_set_ids);
        if result < 0 {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Wrong sticker set list",
            ));
            return;
        }
        if result > 0 {
            let type_index = sticker_type as usize;
            let ids = self.installed_sticker_set_ids[type_index].clone();
            env.send_reorder_sticker_sets_query(sticker_type, ids);
        }
        promise.set_value(());
    }

    /// `apply_installed_sticker_sets_order`
    /// (StickersManager.cpp:7787-7847) — returns -1 wrong / 0 no-op /
    /// 1 reordered; the appended-tail logic is exact.
    fn apply_installed_sticker_sets_order(
        &mut self,
        sticker_type: StickerType,
        sticker_set_ids: &[StickerSetId],
    ) -> i32 {
        let type_index = sticker_type as usize;
        if !self.are_installed_sticker_sets_loaded[type_index] {
            return -1;
        }

        let current = &self.installed_sticker_set_ids[type_index];
        if sticker_set_ids == current.as_slice() {
            return 0;
        }

        let mut valid: std::collections::HashSet<StickerSetId> = current.iter().copied().collect();

        let mut new_sticker_set_ids = Vec::with_capacity(sticker_set_ids.len());
        for &sticker_set_id in sticker_set_ids {
            if valid.remove(&sticker_set_id) {
                new_sticker_set_ids.push(sticker_set_id);
            } else {
                return -1;
            }
        }
        if new_sticker_set_ids.is_empty() {
            return 0;
        }
        if !valid.is_empty() {
            let mut missed = Vec::new();
            for &sticker_set_id in current {
                if valid.remove(&sticker_set_id) {
                    missed.push(sticker_set_id);
                }
            }
            missed.extend(new_sticker_set_ids);
            new_sticker_set_ids = missed;
        }

        self.installed_sticker_set_ids[type_index] = new_sticker_set_ids;
        1
    }

    /// `get_sticker_input_document` (StickersManager.cpp:8711-8736) —
    /// the file-manager probe + the remote-document band.
    fn get_sticker_input_document(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker: Option<&td_api::InputFile>,
    ) -> Result<telegram_api::InputDocument> {
        let file_id = match env.file_get_input_file_id(FileType::Sticker, sticker, false) {
            Result::Ok(file_id) => file_id,
            Result::Err(error) => return Result::Err(error),
        };
        let file_view = env.file_get_view(file_id);
        let Some(document_id) = file_view.main_remote_document_id else {
            return Result::from_status(tdutils::status::Status::error(
                400,
                "Wrong sticker file specified",
            ));
        };
        Result::Ok(telegram_api::InputDocument::inputDocument(
            telegram_api::inputDocument {
                id: document_id,
                access_hash: 0,
                file_reference: Vec::new(),
            },
        ))
    }

    /// `set_sticker_emojis` (StickersManager.cpp:8755-8762).
    pub fn set_sticker_emojis(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker: Option<&td_api::InputFile>,
        emojis: String,
        mut promise: Promise<()>,
    ) {
        let input_document = match self.get_sticker_input_document(env, sticker) {
            Result::Ok(input_document) => input_document,
            Result::Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        env.send_change_sticker_query(
            input_document,
            true,
            emojis,
            None,
            false,
            String::new(),
            promise,
        );
    }

    /// `set_sticker_keywords` (StickersManager.cpp:8764-8774).
    pub fn set_sticker_keywords(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker: Option<&td_api::InputFile>,
        mut keywords: Vec<String>,
        mut promise: Promise<()>,
    ) {
        let input_document = match self.get_sticker_input_document(env, sticker) {
            Result::Ok(input_document) => input_document,
            Result::Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        for keyword in &mut keywords {
            replace_with_spaces(keyword, ",\n");
        }
        env.send_change_sticker_query(
            input_document,
            false,
            String::new(),
            None,
            true,
            keywords.join(","),
            promise,
        );
    }

    /// `set_sticker_mask_position` (StickersManager.cpp:8776-8783).
    pub fn set_sticker_mask_position(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker: Option<&td_api::InputFile>,
        mask_position: Option<td_api::MaskPosition>,
        mut promise: Promise<()>,
    ) {
        let input_document = match self.get_sticker_input_document(env, sticker) {
            Result::Ok(input_document) => input_document,
            Result::Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        env.send_change_sticker_query(
            input_document,
            false,
            String::new(),
            mask_position,
            false,
            String::new(),
            promise,
        );
    }

    /// `set_sticker_position_in_set` (StickersManager.cpp:8738-8745).
    pub fn set_sticker_position_in_set(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker: Option<&td_api::InputFile>,
        position: i32,
        mut promise: Promise<()>,
    ) {
        if position < 0 {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Wrong sticker position specified",
            ));
            return;
        }
        let input_document = match self.get_sticker_input_document(env, sticker) {
            Result::Ok(input_document) => input_document,
            Result::Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        env.send_set_sticker_position_query(input_document, position, promise);
    }

    /// `remove_sticker_from_set` (StickersManager.cpp:8747-8753).
    pub fn remove_sticker_from_set(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker: Option<&td_api::InputFile>,
        mut promise: Promise<()>,
    ) {
        let input_document = match self.get_sticker_input_document(env, sticker) {
            Result::Ok(input_document) => input_document,
            Result::Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        env.send_delete_sticker_from_set_query(input_document, promise);
    }

    /// `set_sticker_set_title` (StickersManager.cpp:8688-8700).
    pub fn set_sticker_set_title(
        &mut self,
        env: &mut impl StickersManagerEnv,
        short_name: String,
        title: String,
        mut promise: Promise<()>,
    ) {
        let short_name = clean_username(&strip_empty_characters(
            &short_name,
            MAX_STICKER_SET_SHORT_NAME_LENGTH,
            false,
        ));
        if short_name.is_empty() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Sticker set name must be non-empty",
            ));
            return;
        }
        let title = strip_empty_characters(&title, MAX_STICKER_SET_TITLE_LENGTH, false);
        if title.is_empty() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Sticker set title must be non-empty",
            ));
            return;
        }
        env.send_set_sticker_set_title_query(short_name, title, promise);
    }

    /// `delete_sticker_set` (StickersManager.cpp:8702-8709).
    pub fn delete_sticker_set(
        &mut self,
        env: &mut impl StickersManagerEnv,
        short_name: String,
        mut promise: Promise<()>,
    ) {
        let short_name = clean_username(&strip_empty_characters(
            &short_name,
            MAX_STICKER_SET_SHORT_NAME_LENGTH,
            false,
        ));
        if short_name.is_empty() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Sticker set name must be non-empty",
            ));
            return;
        }
        env.send_delete_sticker_set_query(short_name, promise);
    }

    /// `set_custom_emoji_sticker_set_thumbnail`
    /// (StickersManager.cpp:8648-8686) — the loaded band parks at the
    /// do-continuation, the not-loaded band at the reload.
    pub fn set_custom_emoji_sticker_set_thumbnail(
        &mut self,
        env: &mut impl StickersManagerEnv,
        short_name: String,
        _custom_emoji_id: CustomEmojiId,
        mut promise: Promise<()>,
    ) {
        let short_name = clean_username(&strip_empty_characters(
            &short_name,
            MAX_STICKER_SET_SHORT_NAME_LENGTH,
            false,
        ));
        if short_name.is_empty() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Sticker set name must be non-empty",
            ));
            return;
        }
        // the collapsed short_name_to_sticker_set_id_ is always a miss
        env.reload_sticker_set_by_name(short_name);
    }

    /// `set_sticker_set_thumbnail` (StickersManager.cpp:8535-8580).
    pub fn set_sticker_set_thumbnail(
        &mut self,
        env: &mut impl StickersManagerEnv,
        user_id: UserId,
        short_name: String,
        _thumbnail: Option<td_api::InputFile>,
        _format: StickerFormat,
        mut promise: Promise<()>,
    ) {
        let is_bot = env.auth_is_bot();
        let user_id = if !is_bot { env.get_my_id() } else { user_id };
        if let Result::Err(error) = env.get_input_user(user_id) {
            promise.set_error(error);
            return;
        }
        let short_name = clean_username(&strip_empty_characters(
            &short_name,
            MAX_STICKER_SET_SHORT_NAME_LENGTH,
            false,
        ));
        if short_name.is_empty() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Sticker set name must be non-empty",
            ));
            return;
        }
        // the collapsed set lookup misses — the reload parks
        env.reload_sticker_set_by_name(short_name);
    }

    /// `add_sticker_to_set` (StickersManager.cpp:8422-8455).
    pub fn add_sticker_to_set(
        &mut self,
        env: &mut impl StickersManagerEnv,
        user_id: UserId,
        short_name: String,
        _sticker: Option<td_api::NewSticker>,
        _old_sticker: Option<td_api::InputFile>,
        mut promise: Promise<()>,
    ) {
        let is_bot = env.auth_is_bot();
        let user_id = if !is_bot { env.get_my_id() } else { user_id };
        if let Result::Err(error) = env.get_input_user(user_id) {
            promise.set_error(error);
            return;
        }
        let short_name = clean_username(&strip_empty_characters(
            &short_name,
            MAX_STICKER_SET_SHORT_NAME_LENGTH,
            false,
        ));
        if short_name.is_empty() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Sticker set name must be non-empty",
            ));
            return;
        }
        // the collapsed set lookup misses — the reload parks
        env.reload_sticker_set_by_name(short_name);
    }

    /// `prepare_input_sticker` (StickersManager.cpp:7942-7960).
    fn prepare_input_sticker(
        env: &mut impl StickersManagerEnv,
        sticker: &td_api::NewSticker,
        sticker_type: StickerType,
    ) -> Result<(crate::files::FileId, bool, bool)> {
        let Some(emojis) = clean_input_string(&sticker.emojis) else {
            return Result::from_status(tdutils::status::Status::error(
                400,
                "Emojis must be encoded in UTF-8",
            ));
        };
        let _ = emojis;
        let mut keywords = sticker.keywords.clone();
        for keyword in &mut keywords {
            let Some(cleaned) = clean_input_string(keyword) else {
                return Result::from_status(tdutils::status::Status::error(
                    400,
                    "Keywords must be encoded in UTF-8",
                ));
            };
            *keyword = cleaned;
            replace_with_spaces(keyword, ",\n");
        }
        let sticker_format = get_sticker_format(sticker.format.as_ref());
        Self::prepare_input_file(
            env,
            sticker.sticker.as_ref(),
            sticker_format,
            sticker_type,
            false,
        )
    }

    /// `prepare_input_file` (StickersManager.cpp:7962-8020) — the
    /// create_sticker/create_document record registration collapses to a
    /// no-op (the records feed the parked tails only).
    fn prepare_input_file(
        env: &mut impl StickersManagerEnv,
        input_file: Option<&td_api::InputFile>,
        sticker_format: StickerFormat,
        sticker_type: StickerType,
        for_thumbnail: bool,
    ) -> Result<(crate::files::FileId, bool, bool)> {
        if sticker_format == StickerFormat::Unknown {
            return Result::from_status(tdutils::status::Status::error(
                400,
                "Sticker format must be non-empty",
            ));
        }

        let file_type = if sticker_format == StickerFormat::Tgs {
            FileType::Sticker
        } else {
            FileType::Document
        };
        let file_id = match env.file_get_input_file_id(file_type, input_file, for_thumbnail) {
            Result::Ok(file_id) => file_id,
            Result::Err(error) => return Result::Err(error),
        };
        if !file_id.is_valid() {
            return Result::Ok((crate::files::FileId::new(0, 0), false, false));
        }

        let file_view = env.file_get_view(file_id);
        if file_view.is_encrypted {
            return Result::from_status(tdutils::status::Status::error(
                400,
                "Can't use encrypted file",
            ));
        }
        if file_view.main_remote_document_id.is_some() {
            // CHECK(main_remote_location->is_document()) — the env
            // flattens the band into the Option
        } else if file_view.has_url {
            if sticker_format == StickerFormat::Tgs {
                return Result::from_status(tdutils::status::Status::error(
                    400,
                    "Animated stickers can't be uploaded by URL",
                ));
            }
            if sticker_format == StickerFormat::Webm {
                return Result::from_status(tdutils::status::Status::error(
                    400,
                    "Video stickers can't be uploaded by URL",
                ));
            }
            return Result::Ok((file_id, true, false));
        } else {
            if file_view.has_full_local_location
                && file_view.expected_size
                    > get_max_sticker_file_size(sticker_format, sticker_type, for_thumbnail)
            {
                return Result::from_status(tdutils::status::Status::error(400, "File is too big"));
            }
            return Result::Ok((file_id, false, true));
        }
        Result::Ok((file_id, false, false))
    }

    /// `create_new_sticker_set` (StickersManager.cpp:8142-8240) — the
    /// validation prefix is exact; the pending-upload machinery parks at
    /// the `[blocked: Files]` seam.
    #[allow(clippy::too_many_arguments)]
    pub fn create_new_sticker_set(
        &mut self,
        env: &mut impl StickersManagerEnv,
        user_id: UserId,
        title: String,
        short_name: String,
        sticker_type: StickerType,
        needs_repainting: bool,
        stickers: Vec<td_api::NewSticker>,
        software: String,
        mut promise: Promise<td_api::StickerSet>,
    ) {
        let is_bot = env.auth_is_bot();
        let user_id = if !is_bot { env.get_my_id() } else { user_id };
        if let Result::Err(error) = env.get_input_user(user_id) {
            promise.set_error(error);
            return;
        }

        let title = strip_empty_characters(&title, MAX_STICKER_SET_TITLE_LENGTH, false);
        if title.is_empty() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Sticker set title must be non-empty",
            ));
            return;
        }

        let short_name =
            strip_empty_characters(&short_name, MAX_STICKER_SET_SHORT_NAME_LENGTH, false);
        if short_name.is_empty() && is_bot {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Sticker set name must be non-empty",
            ));
            return;
        }

        if stickers.is_empty() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "At least 1 sticker must be specified",
            ));
            return;
        }

        if needs_repainting && sticker_type != StickerType::CustomEmoji {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Only custom emoji stickers support repainting",
            ));
            return;
        }

        let mut file_ids = Vec::with_capacity(stickers.len());
        for sticker in &stickers {
            match Self::prepare_input_sticker(env, sticker, sticker_type) {
                Result::Ok((file_id, _is_url, _is_local)) => file_ids.push(file_id),
                Result::Err(error) => {
                    promise.set_error(error);
                    return;
                }
            }
        }

        env.start_new_sticker_set_upload(
            user_id,
            title,
            short_name,
            sticker_type,
            needs_repainting,
            software,
            file_ids,
            promise,
        );
    }

    /// `upload_sticker_file` (StickersManager.cpp:8022-8063) — the
    /// already-remote band answers through the file object; the URL and
    /// local bands park at the `[blocked: Files]` upload.
    pub fn upload_sticker_file(
        &mut self,
        env: &mut impl StickersManagerEnv,
        user_id: UserId,
        sticker_format: StickerFormat,
        input_file: Option<&td_api::InputFile>,
        mut promise: Promise<td_api::File>,
    ) {
        let is_bot = env.auth_is_bot();
        let user_id = if !is_bot { env.get_my_id() } else { user_id };
        if let Result::Err(error) = env.get_input_user(user_id) {
            promise.set_error(error);
            return;
        }

        // StickerType::Regular has less restrictions
        let (file_id, is_url, is_local) = match Self::prepare_input_file(
            env,
            input_file,
            sticker_format,
            StickerType::Regular,
            false,
        ) {
            Result::Ok(prepared) => prepared,
            Result::Err(error) => {
                promise.set_error(error);
                return;
            }
        };

        // the C++ classify: URL → the internal-upload leg, local → the
        // upload leg, already-remote → the immediate answer. The first
        // two legs are the [blocked: Files] seam; the third is real.
        if is_url || is_local {
            env.start_sticker_file_upload(file_id);
            return;
        }
        if let Some(file_object) = env.file_get_file_object(file_id) {
            promise.set_value(file_object);
            return;
        }
        env.start_sticker_file_upload(file_id);
    }

    /// `get_sticker_outline_object` (StickersManager.cpp:2105-2132) —
    /// the unknown-sticker band answers JSON null in C++ (the recorded
    /// null-response divergence): the promise parks.
    pub fn get_sticker_outline_object(
        &mut self,
        env: &mut impl StickersManagerEnv,
        _file_id: crate::files::FileId,
        _for_animated_emoji: bool,
        _for_clicked_animated_emoji: bool,
        _promise: Promise<td_api::Outline>,
    ) {
        let _ = env;
        // park: the C++ promise.set_value(nullptr) is inexpressible
    }

    /// `get_sticker_outline_svg_path` (StickersManager.cpp:2134-2163) —
    /// the unknown-sticker band answers the empty string (the known
    /// path needs the sticker record).
    pub fn get_sticker_outline_svg_path(
        &mut self,
        env: &mut impl StickersManagerEnv,
        _file_id: crate::files::FileId,
        _for_animated_emoji: bool,
        _for_clicked_animated_emoji: bool,
        mut promise: Promise<String>,
    ) {
        // sticker == nullptr over the collapsed cache → the empty string
        let _ = env;
        promise.set_value(String::new());
    }
}

impl std::fmt::Debug for StickersManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StickersManager")
            .field(
                "are_installed_sticker_sets_loaded",
                &self.are_installed_sticker_sets_loaded,
            )
            .field(
                "installed_sticker_set_ids",
                &self
                    .installed_sticker_set_ids
                    .iter()
                    .map(Vec::len)
                    .collect::<Vec<_>>(),
            )
            .field(
                "custom_emoji_to_sticker_id",
                &self.custom_emoji_to_sticker_id.len(),
            )
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests — the local validation bands, the cache tables and the enum
// converters over a recording env.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use tdutils::promise::PromiseInterface;

    /// The result-capturing promise interface (the call_manager pattern).
    struct StRecording<T>(std::sync::Arc<std::sync::Mutex<Option<Result<T>>>>);

    impl<T: Send + 'static> PromiseInterface<T> for StRecording<T> {
        fn set_value(&mut self, value: T) {
            *self.0.lock().unwrap() = Some(Result::Ok(value));
        }

        fn set_error(&mut self, error: tdutils::status::Status) {
            *self.0.lock().unwrap() = Some(Result::Err(error));
        }
    }

    type StCell<T> = std::sync::Arc<std::sync::Mutex<Option<Result<T>>>>;

    fn record<T: Send + 'static>() -> (Promise<T>, StCell<T>) {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(StRecording(recorded.clone())),
            recorded,
        )
    }

    fn recorded_error<T: Send + 'static + std::fmt::Debug>(cell: &StCell<T>) -> (i32, String) {
        match &*cell.lock().unwrap() {
            Some(Result::Err(status)) => (status.code(), status.message().to_owned()),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    /// The offline env: records every seam call, drives `now`, and owns
    /// the seeded file-view answer.
    #[derive(Clone)]
    struct TestEnv {
        is_bot: bool,
        now: f64,
        use_sqlite_pmc: bool,
        input_user_ok: bool,
        file_view: StickerFileView,
        queries: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                is_bot: false,
                now: 100.0,
                use_sqlite_pmc: false,
                input_user_ok: true,
                file_view: StickerFileView::default(),
                queries: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            }
        }
    }

    impl TestEnv {
        fn recorded(&self, prefix: &str) -> usize {
            self.queries
                .borrow()
                .iter()
                .filter(|name| name.starts_with(prefix))
                .count()
        }
    }

    impl StickersManagerEnv for TestEnv {
        fn auth_is_bot(&self) -> bool {
            self.is_bot
        }
        fn close_flag(&self) -> bool {
            false
        }
        fn unix_time(&self) -> i32 {
            1_700_000_000
        }
        fn now(&self) -> f64 {
            self.now
        }
        fn get_option_boolean(&self, name: &str, default_value: bool) -> bool {
            if name == "use_sqlite_pmc" {
                self.use_sqlite_pmc
            } else {
                default_value
            }
        }
        fn get_my_id(&self) -> UserId {
            UserId::new(1)
        }
        fn get_input_user(&mut self, _user_id: UserId) -> Result<telegram_api::InputUser> {
            if self.input_user_ok {
                Result::Ok(telegram_api::InputUser::inputUserSelf(
                    telegram_api::inputUserSelf {},
                ))
            } else {
                Result::from_status(tdutils::status::Status::error(400, "User not found"))
            }
        }
        fn file_get_input_file_id(
            &mut self,
            _file_type: FileType,
            input_file: Option<&td_api::InputFile>,
            expect_thumbnail: bool,
        ) -> Result<crate::files::FileId> {
            if input_file.is_none() && expect_thumbnail {
                return Result::Ok(crate::files::FileId::new(0, 0));
            }
            Result::Ok(crate::files::FileId::new(7, 0))
        }
        fn file_get_view(&mut self, _file_id: crate::files::FileId) -> StickerFileView {
            self.file_view
        }

        // The seam-recording overrides follow (same impl block); the
        // untouched tails keep their default park bodies.
        fn send_check_sticker_set_short_name_query(
            &mut self,
            _name: String,
            _promise: Promise<CheckStickerSetNameResult>,
        ) {
            self.queries
                .borrow_mut()
                .push("CheckStickerSetShortNameQuery".to_owned());
        }
        fn send_suggest_sticker_set_short_name_query(
            &mut self,
            _title: String,
            _promise: Promise<String>,
        ) {
            self.queries
                .borrow_mut()
                .push("SuggestStickerSetShortNameQuery".to_owned());
        }
        fn load_found_stickers(
            &mut self,
            _sticker_type: StickerType,
            _emoji: String,
            _promise: Promise<td_api::Stickers>,
        ) {
            self.queries
                .borrow_mut()
                .push("LoadFoundStickers".to_owned());
        }
        fn reload_found_stickers(&mut self, _sticker_type: StickerType, _emoji: String) {
            self.queries
                .borrow_mut()
                .push("ReloadFoundStickers".to_owned());
        }
        fn load_special_sticker_set(&mut self, set_type: SpecialStickerSetType) {
            self.queries
                .borrow_mut()
                .push(format!("LoadSpecialStickerSet({set_type:?})"));
        }
        fn load_custom_emoji_sticker_from_database(&mut self, _custom_emoji_id: CustomEmojiId) {
            self.queries
                .borrow_mut()
                .push("LoadCustomEmojiFromDatabase".to_owned());
        }
        fn send_get_custom_emoji_documents_query(&mut self, _custom_emoji_ids: Vec<CustomEmojiId>) {
            self.queries
                .borrow_mut()
                .push("GetCustomEmojiDocumentsQuery".to_owned());
        }
        fn load_default_custom_emoji_ids(
            &mut self,
            _list_type: StickerListType,
            _force_reload: bool,
        ) {
            self.queries
                .borrow_mut()
                .push("LoadDefaultCustomEmojiIds".to_owned());
        }
        fn load_emoji_groups(
            &mut self,
            _group_type: EmojiGroupType,
            _used_language_codes: String,
            _promise: Promise<td_api::EmojiCategories>,
        ) {
            self.queries.borrow_mut().push("LoadEmojiGroups".to_owned());
        }
        fn send_get_my_stickers_query(
            &mut self,
            _offset_sticker_set_id: StickerSetId,
            _limit: i32,
            _promise: Promise<td_api::StickerSets>,
        ) {
            self.queries
                .borrow_mut()
                .push("GetMyStickersQuery".to_owned());
        }
        fn send_reorder_sticker_sets_query(
            &mut self,
            _sticker_type: StickerType,
            _sticker_set_ids: Vec<StickerSetId>,
        ) {
            self.queries
                .borrow_mut()
                .push("ReorderStickerSetsQuery".to_owned());
        }
        fn send_set_sticker_set_title_query(
            &mut self,
            _short_name: String,
            _title: String,
            _promise: Promise<()>,
        ) {
            self.queries
                .borrow_mut()
                .push("SetStickerSetTitleQuery".to_owned());
        }
        fn send_set_sticker_position_query(
            &mut self,
            _input_document: telegram_api::InputDocument,
            _position: i32,
            _promise: Promise<()>,
        ) {
            self.queries
                .borrow_mut()
                .push("SetStickerPositionQuery".to_owned());
        }
        fn send_change_sticker_query(
            &mut self,
            _input_document: telegram_api::InputDocument,
            _change_emojis: bool,
            _emojis: String,
            _mask_position: Option<td_api::MaskPosition>,
            _change_keywords: bool,
            _keywords: String,
            _promise: Promise<()>,
        ) {
            self.queries
                .borrow_mut()
                .push("ChangeStickerQuery".to_owned());
        }
        fn send_delete_sticker_set_query(&mut self, _short_name: String, _promise: Promise<()>) {
            self.queries
                .borrow_mut()
                .push("DeleteStickerSetQuery".to_owned());
        }
        fn reload_sticker_set_by_name(&mut self, _short_name: String) {
            self.queries
                .borrow_mut()
                .push("ReloadStickerSetByName".to_owned());
        }
        fn start_new_sticker_set_upload(
            &mut self,
            _user_id: UserId,
            _title: String,
            _short_name: String,
            _sticker_type: StickerType,
            _needs_repainting: bool,
            _software: String,
            _file_ids: Vec<crate::files::FileId>,
            _promise: Promise<td_api::StickerSet>,
        ) {
            self.queries
                .borrow_mut()
                .push("StartNewStickerSetUpload".to_owned());
        }
        fn start_sticker_file_upload(&mut self, _file_id: crate::files::FileId) {
            self.queries
                .borrow_mut()
                .push("StartStickerFileUpload".to_owned());
        }
        fn send_read_featured_sticker_sets(&mut self) {
            self.queries
                .borrow_mut()
                .push("ReadFeaturedStickerSets".to_owned());
        }
        fn load_premium_stickers(&mut self, _limit: i32, _promise: Promise<td_api::Stickers>) {
            self.queries
                .borrow_mut()
                .push("LoadPremiumStickers".to_owned());
        }
    }

    #[test]
    fn the_search_bands_follow_the_cpp() {
        let mut manager = StickersManager::new();
        let mut env = TestEnv::default();

        // negative offsets are rejected
        let (promise, cell) = record::<td_api::Stickers>();
        manager.search_stickers(
            &mut env,
            StickerType::Regular,
            "x".to_owned(),
            String::new(),
            Vec::new(),
            -1,
            10,
            promise,
        );
        assert_eq!(
            recorded_error(&cell),
            (400, "Parameter offset must be non-negative".to_owned())
        );

        // zero limit answers empty, negative is rejected
        let (promise, cell) = record::<td_api::Stickers>();
        manager.search_stickers(
            &mut env,
            StickerType::Regular,
            "x".to_owned(),
            String::new(),
            Vec::new(),
            0,
            0,
            promise,
        );
        assert!(matches!(&*cell.lock().unwrap(), Some(Result::Ok(_))));
        let (promise, cell) = record::<td_api::Stickers>();
        manager.search_stickers(
            &mut env,
            StickerType::Regular,
            "x".to_owned(),
            String::new(),
            Vec::new(),
            0,
            -5,
            promise,
        );
        assert_eq!(
            recorded_error(&cell),
            (400, "Parameter limit must be positive".to_owned())
        );

        // the empty emoji and the Mask type answer empty without a load
        let (promise, cell) = record::<td_api::Stickers>();
        manager.search_stickers(
            &mut env,
            StickerType::Regular,
            String::new(),
            String::new(),
            Vec::new(),
            0,
            10,
            promise,
        );
        assert!(matches!(&*cell.lock().unwrap(), Some(Result::Ok(_))));
        assert_eq!(env.recorded("LoadFoundStickers"), 0);
        let (promise, cell) = record::<td_api::Stickers>();
        manager.search_stickers(
            &mut env,
            StickerType::Mask,
            "x".to_owned(),
            String::new(),
            Vec::new(),
            0,
            10,
            promise,
        );
        assert!(matches!(&*cell.lock().unwrap(), Some(Result::Ok(_))));

        // a live emoji parks at the found-stickers load
        let (promise, _) = record::<td_api::Stickers>();
        manager.search_stickers(
            &mut env,
            StickerType::Regular,
            "\u{2b50}".to_owned(),
            String::new(),
            Vec::new(),
            0,
            10,
            promise,
        );
        assert_eq!(env.recorded("LoadFoundStickers"), 1);
    }

    #[test]
    fn the_found_stickers_cache_table_answers_and_refreshes() {
        let mut manager = StickersManager::new();
        let mut env = TestEnv::default();
        manager.found_stickers[StickerType::Regular as usize].insert(
            "\u{2b50}".to_owned(),
            (vec![MessageId::new(2000001)], 150.0),
        );

        // a fresh cache hit answers and skips the reload
        let (promise, cell) = record::<td_api::Stickers>();
        manager.search_stickers(
            &mut env,
            StickerType::Regular,
            "\u{2b50}".to_owned(),
            String::new(),
            Vec::new(),
            0,
            10,
            promise,
        );
        assert!(matches!(&*cell.lock().unwrap(), Some(Result::Ok(_))));
        assert_eq!(env.recorded("ReloadFoundStickers"), 0);

        // a stale hit answers and fires the reload
        manager.found_stickers[StickerType::Regular as usize]
            .get_mut("\u{2b50}")
            .unwrap()
            .1 = 90.0;
        let (promise, _) = record::<td_api::Stickers>();
        manager.search_stickers(
            &mut env,
            StickerType::Regular,
            "\u{2b50}".to_owned(),
            String::new(),
            Vec::new(),
            0,
            10,
            promise,
        );
        assert_eq!(env.recorded("ReloadFoundStickers"), 1);
    }

    #[test]
    fn the_custom_emoji_bands_follow_the_cpp() {
        let mut manager = StickersManager::new();
        let mut env = TestEnv::default();

        // the count band
        let ids: Vec<CustomEmojiId> = (0..201).map(CustomEmojiId::new).collect();
        let (promise, cell) = record::<td_api::Stickers>();
        manager.get_custom_emoji_stickers(&mut env, ids, true, promise);
        assert_eq!(
            recorded_error(&cell),
            (
                400,
                "Too many custom emoji identifiers specified".to_owned()
            )
        );

        // invalid + duplicate ids drop; unknown ids ride the database
        // load under use_sqlite_pmc, else the documents query
        let mut ids = vec![
            CustomEmojiId::new(5),
            CustomEmojiId::new(1 << 21),
            CustomEmojiId::new(1 << 21),
        ];
        ids.push(CustomEmojiId::new(0));
        env.use_sqlite_pmc = true;
        manager.get_custom_emoji_stickers(&mut env, ids.clone(), true, promise_unused());
        assert_eq!(env.recorded("LoadCustomEmojiFromDatabase"), 1);
        env.use_sqlite_pmc = false;
        let (promise, _) = record::<td_api::Stickers>();
        manager.get_custom_emoji_stickers(&mut env, ids, false, promise);
        assert_eq!(env.recorded("GetCustomEmojiDocumentsQuery"), 1);
    }

    fn promise_unused() -> Promise<td_api::Stickers> {
        Promise::default()
    }

    #[test]
    fn the_sticker_set_name_plane_follows_the_cpp() {
        let mut manager = StickersManager::new();
        let mut env = TestEnv::default();

        // the Great Minds special cases answer locally
        for set_id in [GREAT_MINDS_SET_ID, GREAT_MINDS_COLOR_SET_ID] {
            let (promise, cell) = record::<String>();
            manager.get_sticker_set_name(&mut env, StickerSetId::new(set_id), promise);
            match &*cell.lock().unwrap() {
                Some(Result::Ok(name)) => assert_eq!(name, "TelegramGreatMinds"),
                other => panic!("expected the special name, got {other:?}"),
            }
        }

        // unknown sets are the 400 band
        let (promise, cell) = record::<String>();
        manager.get_sticker_set_name(&mut env, StickerSetId::new(42), promise);
        assert_eq!(
            recorded_error(&cell),
            (400, "Sticker set not found".to_owned())
        );
    }

    #[test]
    fn the_check_and_suggest_name_bands_follow_the_cpp() {
        let mut manager = StickersManager::new();
        let mut env = TestEnv::default();

        // the empty name answers Invalid locally
        let (promise, cell) = record::<CheckStickerSetNameResult>();
        manager.check_sticker_set_name(&mut env, String::new(), promise);
        assert!(matches!(
            &*cell.lock().unwrap(),
            Some(Result::Ok(CheckStickerSetNameResult::Invalid))
        ));
        assert_eq!(env.recorded("CheckStickerSetShortNameQuery"), 0);

        // a real name parks at the check query
        let (promise, _) = record::<CheckStickerSetNameResult>();
        manager.check_sticker_set_name(&mut env, "name".to_owned(), promise);
        assert_eq!(env.recorded("CheckStickerSetShortNameQuery"), 1);

        // the empty/stripped titles are rejected
        let (promise, cell) = record::<String>();
        manager.get_suggested_sticker_set_name(&mut env, String::new(), promise);
        assert_eq!(
            recorded_error(&cell),
            (400, "Sticker set title must be non-empty".to_owned())
        );
        let (promise, cell) = record::<String>();
        manager.get_suggested_sticker_set_name(&mut env, "  \u{a0} ".to_owned(), promise);
        assert_eq!(
            recorded_error(&cell),
            (400, "Sticker set title must be non-empty".to_owned())
        );

        // a live title parks at the suggestion query
        let (promise, _) = record::<String>();
        manager.get_suggested_sticker_set_name(&mut env, "Dogs".to_owned(), promise);
        assert_eq!(env.recorded("SuggestStickerSetShortNameQuery"), 1);
    }

    #[test]
    fn the_reorder_bands_follow_the_cpp() {
        let mut manager = StickersManager::new();
        let mut env = TestEnv::default();

        // the not-loaded band is the 400
        let (promise, cell) = record::<()>();
        manager.reorder_installed_sticker_sets(
            &mut env,
            StickerType::Regular,
            vec![StickerSetId::new(1)],
            promise,
        );
        assert_eq!(
            recorded_error(&cell),
            (400, "Wrong sticker set list".to_owned())
        );

        // loaded + empty list: the no-op answer without a query
        manager.are_installed_sticker_sets_loaded[StickerType::Regular as usize] = true;
        let (promise, cell) = record::<()>();
        manager.reorder_installed_sticker_sets(&mut env, StickerType::Regular, Vec::new(), promise);
        assert!(matches!(&*cell.lock().unwrap(), Some(Result::Ok(()))));
        assert_eq!(env.recorded("ReorderStickerSetsQuery"), 0);

        // loaded + a permutation of one known set: reorder + query
        manager.installed_sticker_set_ids[StickerType::Regular as usize] =
            vec![StickerSetId::new(1), StickerSetId::new(2)];
        let (promise, cell) = record::<()>();
        manager.reorder_installed_sticker_sets(
            &mut env,
            StickerType::Regular,
            vec![StickerSetId::new(2), StickerSetId::new(1)],
            promise,
        );
        assert!(matches!(&*cell.lock().unwrap(), Some(Result::Ok(()))));
        assert_eq!(env.recorded("ReorderStickerSetsQuery"), 1);
        // the missed tails append after the new order
        assert_eq!(
            manager.installed_sticker_set_ids[StickerType::Regular as usize],
            vec![StickerSetId::new(2), StickerSetId::new(1)]
        );

        // an unknown set id is the 400
        let (promise, cell) = record::<()>();
        manager.reorder_installed_sticker_sets(
            &mut env,
            StickerType::Regular,
            vec![StickerSetId::new(9)],
            promise,
        );
        assert_eq!(
            recorded_error(&cell),
            (400, "Wrong sticker set list".to_owned())
        );
    }

    #[test]
    fn the_mutation_bands_follow_the_cpp() {
        let mut manager = StickersManager::new();
        let mut env = TestEnv::default();

        // the negative position band
        let (promise, cell) = record::<()>();
        manager.set_sticker_position_in_set(&mut env, None, -1, promise);
        assert_eq!(
            recorded_error(&cell),
            (400, "Wrong sticker position specified".to_owned())
        );

        // the wrong-sticker-file band (the collapsed view has no remote
        // document)
        let (promise, cell) = record::<()>();
        manager.set_sticker_position_in_set(&mut env, None, 5, promise);
        assert_eq!(
            recorded_error(&cell),
            (400, "Wrong sticker file specified".to_owned())
        );

        // with a remote document the query parks
        env.file_view.main_remote_document_id = Some(777);
        let (promise, _) = record::<()>();
        manager.set_sticker_position_in_set(&mut env, None, 5, promise);
        assert_eq!(env.recorded("SetStickerPositionQuery"), 1);

        // the name/title strips of set_sticker_set_title
        let (promise, cell) = record::<()>();
        manager.set_sticker_set_title(&mut env, String::new(), "T".to_owned(), promise);
        assert_eq!(
            recorded_error(&cell),
            (400, "Sticker set name must be non-empty".to_owned())
        );
        let (promise, cell) = record::<()>();
        manager.set_sticker_set_title(&mut env, "dogs".to_owned(), String::new(), promise);
        assert_eq!(
            recorded_error(&cell),
            (400, "Sticker set title must be non-empty".to_owned())
        );
        let (promise, _) = record::<()>();
        manager.set_sticker_set_title(&mut env, "dogs".to_owned(), "Dogs".to_owned(), promise);
        assert_eq!(env.recorded("SetStickerSetTitleQuery"), 1);

        // delete_sticker_set rides the same name band
        let (promise, cell) = record::<()>();
        manager.delete_sticker_set(&mut env, String::new(), promise);
        assert_eq!(
            recorded_error(&cell),
            (400, "Sticker set name must be non-empty".to_owned())
        );
        let (promise, _) = record::<()>();
        manager.delete_sticker_set(&mut env, "dogs".to_owned(), promise);
        assert_eq!(env.recorded("DeleteStickerSetQuery"), 1);
    }

    #[test]
    fn the_create_new_sticker_set_bands_follow_the_cpp() {
        let mut manager = StickersManager::new();
        let mut env = TestEnv {
            input_user_ok: false,
            ..TestEnv::default()
        };
        // the input-user failure band fires first
        let (promise, cell) = record::<td_api::StickerSet>();
        manager.create_new_sticker_set(
            &mut env,
            UserId::new(9),
            "Dogs".to_owned(),
            "dogs".to_owned(),
            StickerType::Regular,
            false,
            Vec::new(),
            String::new(),
            promise,
        );
        assert_eq!(recorded_error(&cell), (400, "User not found".to_owned()));
        env.input_user_ok = true;

        // the empty title band
        let (promise, cell) = record::<td_api::StickerSet>();
        manager.create_new_sticker_set(
            &mut env,
            UserId::new(9),
            String::new(),
            "dogs".to_owned(),
            StickerType::Regular,
            false,
            Vec::new(),
            String::new(),
            promise,
        );
        assert_eq!(
            recorded_error(&cell),
            (400, "Sticker set title must be non-empty".to_owned())
        );

        // the user-side empty name is fine (the bot band is the 400)
        env.is_bot = true;
        let (promise, cell) = record::<td_api::StickerSet>();
        manager.create_new_sticker_set(
            &mut env,
            UserId::new(9),
            "Dogs".to_owned(),
            String::new(),
            StickerType::Regular,
            false,
            Vec::new(),
            String::new(),
            promise,
        );
        assert_eq!(
            recorded_error(&cell),
            (400, "Sticker set name must be non-empty".to_owned())
        );
        env.is_bot = false;

        // the empty sticker list and the repainting bands
        let (promise, cell) = record::<td_api::StickerSet>();
        manager.create_new_sticker_set(
            &mut env,
            UserId::new(9),
            "Dogs".to_owned(),
            "dogs".to_owned(),
            StickerType::Regular,
            false,
            Vec::new(),
            String::new(),
            promise,
        );
        assert_eq!(
            recorded_error(&cell),
            (400, "At least 1 sticker must be specified".to_owned())
        );
        let repainting_sticker = || td_api::NewSticker {
            sticker: Some(td_api::InputFile::inputFileLocal(td_api::inputFileLocal {
                path: "sticker.png".to_owned(),
            })),
            format: Some(td_api::StickerFormat::stickerFormatWebp(
                td_api::stickerFormatWebp {},
            )),
            emojis: "\u{1f436}".to_owned(),
            mask_position: None,
            keywords: Vec::new(),
        };
        let (promise, cell) = record::<td_api::StickerSet>();
        manager.create_new_sticker_set(
            &mut env,
            UserId::new(9),
            "Dogs".to_owned(),
            "dogs".to_owned(),
            StickerType::Regular,
            true,
            vec![repainting_sticker()],
            String::new(),
            promise,
        );
        assert_eq!(
            recorded_error(&cell),
            (
                400,
                "Only custom emoji stickers support repainting".to_owned()
            )
        );

        // with a sticker the prepare runs and the upload parks
        env.file_view.has_full_local_location = true;
        env.file_view.expected_size = 1 << 18;
        let (promise, _) = record::<td_api::StickerSet>();
        manager.create_new_sticker_set(
            &mut env,
            UserId::new(9),
            "Dogs".to_owned(),
            "dogs".to_owned(),
            StickerType::Regular,
            false,
            vec![repainting_sticker()],
            String::new(),
            promise,
        );
        assert_eq!(env.recorded("StartNewStickerSetUpload"), 1);
    }

    #[test]
    fn the_prepare_bands_follow_the_cpp() {
        let mut env = TestEnv::default();

        // the unknown-format band fires before the input probe
        let result = StickersManager::prepare_input_file(
            &mut env,
            None,
            StickerFormat::Unknown,
            StickerType::Regular,
            false,
        );
        assert!(matches!(
            result,
            Result::Err(ref status) if status.message() == "Sticker format must be non-empty"
        ));

        // the empty-input + expect_thumbnail band answers the empty tuple
        let (file_id, is_url, is_local) = match StickersManager::prepare_input_file(
            &mut env,
            None,
            StickerFormat::Webp,
            StickerType::Regular,
            true,
        ) {
            Result::Ok(prepared) => prepared,
            Result::Err(error) => panic!("unexpected error {error:?}"),
        };
        assert!(!file_id.is_valid());
        assert!(!is_url && !is_local);

        // a real file id rides the view bands
        env.file_view.has_full_local_location = true;
        env.file_view.expected_size = 1 << 19;
        let result = StickersManager::prepare_input_file(
            &mut env,
            None,
            StickerFormat::Webp,
            StickerType::Regular,
            false,
        );
        assert!(matches!(result, Result::Ok((_, false, true))));

        // the too-big local band (custom emoji webp caps at 1 << 17)
        env.file_view.expected_size = (1 << 17) + 1;
        let result = StickersManager::prepare_input_file(
            &mut env,
            None,
            StickerFormat::Webp,
            StickerType::CustomEmoji,
            false,
        );
        assert!(matches!(
            result,
            Result::Err(ref status) if status.message() == "File is too big"
        ));

        // the encrypted band
        env.file_view.is_encrypted = true;
        let result = StickersManager::prepare_input_file(
            &mut env,
            None,
            StickerFormat::Webp,
            StickerType::Regular,
            false,
        );
        assert!(matches!(
            result,
            Result::Err(ref status) if status.message() == "Can't use encrypted file"
        ));
    }

    #[test]
    fn the_park_through_planes_record_their_seams() {
        let mut manager = StickersManager::new();
        let mut env = TestEnv::default();

        // the special-set readers park on their loads
        let (promise, _) = record::<td_api::AnimatedEmoji>();
        manager.get_animated_emoji(&mut env, "\u{1f600}".to_owned(), false, promise);
        let (promise, _) = record::<td_api::Stickers>();
        manager.get_custom_emoji_reaction_generic_animations(&mut env, false, promise);
        let (promise, _) = record::<td_api::Stickers>();
        manager.get_default_topic_icons(&mut env, false, promise);
        let (promise, _) = record::<td_api::EmojiStatusCustomEmojis>();
        manager.get_default_emoji_statuses(&mut env, false, promise);
        let (promise, _) = record::<td_api::EmojiStatusCustomEmojis>();
        manager.get_default_channel_emoji_statuses(&mut env, false, promise);
        assert_eq!(
            env.recorded("LoadSpecialStickerSet"),
            5,
            "the five special-set loads"
        );

        // the recursive re-entries answer the empty bands
        let (promise, cell) = record::<td_api::Stickers>();
        manager.get_custom_emoji_reaction_generic_animations(&mut env, true, promise);
        assert!(matches!(&*cell.lock().unwrap(), Some(Result::Ok(_))));

        // the premium planes park
        let (promise, _) = record::<td_api::Sticker>();
        manager.load_premium_gift_sticker(&mut env, 3, 0, promise);
        let (promise, _) = record::<td_api::Stickers>();
        manager.get_premium_stickers(&mut env, 10, promise);
        assert_eq!(env.recorded("LoadPremiumStickers"), 1);

        // the premium limit bands
        let (promise, cell) = record::<td_api::Stickers>();
        manager.get_premium_stickers(&mut env, 0, promise);
        assert!(matches!(&*cell.lock().unwrap(), Some(Result::Ok(_))));

        // the owned-sets limit band
        let (promise, cell) = record::<td_api::StickerSets>();
        manager.get_created_sticker_sets(&mut env, StickerSetId::default(), 0, promise);
        assert_eq!(
            recorded_error(&cell),
            (400, "Parameter limit must be positive".to_owned())
        );
        let (promise, _) = record::<td_api::StickerSets>();
        manager.get_created_sticker_sets(&mut env, StickerSetId::default(), 10, promise);
        assert_eq!(env.recorded("GetMyStickersQuery"), 1);

        // view_trending records the delayed read
        manager.view_featured_sticker_sets(&mut env, &[StickerSetId::new(1)]);
        assert_eq!(env.recorded("ReadFeaturedStickerSets"), 1);
    }

    #[test]
    fn the_enum_tables_follow_the_cpp() {
        // the sticker-type table
        assert_eq!(get_sticker_type(None), StickerType::Regular);
        assert_eq!(
            get_sticker_type(Some(&td_api::StickerType::stickerTypeMask(
                td_api::stickerTypeMask {}
            ))),
            StickerType::Mask
        );
        assert_eq!(
            get_sticker_type(Some(&td_api::StickerType::stickerTypeCustomEmoji(
                td_api::stickerTypeCustomEmoji {}
            ))),
            StickerType::CustomEmoji
        );

        // the sticker-format table
        assert_eq!(get_sticker_format(None), StickerFormat::Unknown);
        assert_eq!(
            get_sticker_format(Some(&td_api::StickerFormat::stickerFormatTgs(
                td_api::stickerFormatTgs {}
            ))),
            StickerFormat::Tgs
        );

        // the emoji-group-type table
        assert_eq!(get_emoji_group_type(None), EmojiGroupType::Default);
        assert_eq!(
            get_emoji_group_type(Some(
                &td_api::EmojiCategoryType::emojiCategoryTypeChatPhoto(
                    td_api::emojiCategoryTypeChatPhoto {}
                )
            )),
            EmojiGroupType::ProfilePhoto
        );
        assert_eq!(
            get_emoji_group_type(Some(
                &td_api::EmojiCategoryType::emojiCategoryTypeRegularStickers(
                    td_api::emojiCategoryTypeRegularStickers {}
                )
            )),
            EmojiGroupType::RegularStickers
        );

        // the check-result object table
        assert!(matches!(
            get_check_sticker_set_name_result_object(CheckStickerSetNameResult::Occupied),
            td_api::CheckStickerSetNameResult::checkStickerSetNameResultNameOccupied(_)
        ));
    }

    #[test]
    fn the_max_sticker_file_size_table_follows_the_cpp() {
        assert_eq!(
            get_max_sticker_file_size(StickerFormat::Webp, StickerType::Regular, false),
            1 << 19
        );
        assert_eq!(
            get_max_sticker_file_size(StickerFormat::Webp, StickerType::CustomEmoji, false),
            1 << 17
        );
        assert_eq!(
            get_max_sticker_file_size(StickerFormat::Webp, StickerType::Regular, true),
            1 << 17
        );
        assert_eq!(
            get_max_sticker_file_size(StickerFormat::Tgs, StickerType::Regular, false),
            1 << 16
        );
        assert_eq!(
            get_max_sticker_file_size(StickerFormat::Tgs, StickerType::Regular, true),
            1 << 15
        );
        assert_eq!(
            get_max_sticker_file_size(StickerFormat::Webm, StickerType::Regular, false),
            1 << 18
        );
        assert_eq!(
            get_max_sticker_file_size(StickerFormat::Webm, StickerType::CustomEmoji, false),
            1 << 16
        );
        assert_eq!(
            get_max_sticker_file_size(StickerFormat::Webm, StickerType::Regular, true),
            1 << 15
        );
    }

    #[test]
    fn the_custom_emoji_id_bands_follow_the_cpp() {
        assert!(!CustomEmojiId::new(0).is_valid());
        assert!(!CustomEmojiId::new(1 << 19).is_valid());
        assert!(CustomEmojiId::new(1 << 20).is_valid());
        assert!(!CustomEmojiId::new(i64::from(i32::MAX)).is_valid());
        assert_eq!(
            CustomEmojiId::get_custom_emoji_ids(&[1 << 21, 2 << 21]),
            vec![CustomEmojiId::new(1 << 21), CustomEmojiId::new(2 << 21)]
        );
    }
}
