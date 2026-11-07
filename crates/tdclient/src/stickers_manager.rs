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
//!   topic-icon sets, the premium gift set) ride the
//!   `ReloadSpecialStickerSetQuery` plane since net-drain 59 (the six
//!   special-set readers' misses park on the per-type lots; the dice /
//!   click / ton-gift twins stay with their planes)
//! - the file/upload half of `create_new_sticker_set`/`upload_sticker_file`
//!   — `[blocked: Files]` (the M.12 upload orchestration)
//! - the recorded null-response divergences (`get_sticker_outline_object`
//!   over an unknown sticker answers JSON null in C++) park like the M.dp
//!   `send_null_user` band.

use crate::documents_manager::StickerFormat;
use crate::files::FileType;
use crate::messages_manager::MessageId;
use crate::misc::{
    clean_input_string, clean_name, clean_username, replace_with_spaces, strip_empty_characters,
};
use crate::store_parse::{Parser, store_i32, store_log_event_version, store_string_vec};
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;
use tdutils::promise::{Promise, PromiseCreator};
use tdutils::status::Result;
use tdutils::translit::get_word_transliterations;
use tdutils::utf8::{check_utf8, utf8_get_search_words, utf8_to_lower};

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

/// `StickersManager::Sticker` (StickersManager.h:499-545) — the M.15
/// record core the net-drain 59 intakes build (`on_get_sticker_document`
/// → `create_sticker` → `on_get_sticker`, StickersManager.cpp:3376-3435 /
/// :2845-2950); only the fields the answer builders read are carried
/// (the database/secret-media halves stay with their planes).
#[derive(Clone, Debug, Default, PartialEq)]
struct Sticker {
    /// `file_id_`.
    file_id: crate::files::FileId,
    /// `dimensions_`.
    dimensions: crate::photo::Dimensions,
    /// `minithumbnail_` — empty over the bot band
    /// (`!td_->auth_manager_->is_bot()`).
    minithumbnail: String,
    /// `s_thumbnail_` — the `'s'`/`'t'` thumbnail.
    s_thumbnail: crate::photo::PhotoSize,
    /// `m_thumbnail_` — the `'m'` thumbnail.
    m_thumbnail: crate::photo::PhotoSize,
    /// `premium_animation_file_id_`.
    premium_animation_file_id: crate::files::FileId,
    /// `is_premium_`.
    is_premium: bool,
    /// `set_id_`.
    set_id: StickerSetId,
    /// `alt_`.
    alt: String,
    /// `mask_position_` — the raw `StickerMaskPosition` (the C++ holds
    /// the parsed `mask_coords_`; only the object build reads it).
    mask_position: Option<td_api::MaskPosition>,
    /// `emoji_receive_date_` — the 86400s freshness watermark
    /// (`G()->unix_time()` at the custom-emoji create).
    emoji_receive_date: i32,
    /// `is_being_reloaded_` — the one-shot reload guard.
    is_being_reloaded: bool,
    /// `type_` (`StickerType::Regular` default).
    sticker_type: StickerType,
    /// `has_text_color_` (the custom-emoji attribute).
    has_text_color: bool,
    /// `format_`.
    format: StickerFormat,
}

impl Sticker {
    /// `StickersManager::get_sticker_id` (StickersManager.cpp:2088-2093)
    /// — the full-remote document id over the file view; the caller's
    /// `is_document()` guard is the [`StickerFileView`] read.
    fn get_sticker_id(&self, view: &StickerFileView) -> i64 {
        view.full_remote_document_id.unwrap_or(0)
    }
}

/// `StickersManager::StickerSet` (StickersManager.h:388-440) — the
/// record core the set intakes build. The database/save halves stay
/// with their planes; only the fields the answer builders and the
/// special-set machinery read are carried.
#[derive(Clone, Debug, Default, PartialEq)]
struct StickerSet {
    /// `id_`.
    id: StickerSetId,
    /// `access_hash_`.
    access_hash: i64,
    /// `title_`.
    title: String,
    /// `short_name_`.
    short_name: String,
    /// `sticker_type_`.
    sticker_type: StickerType,
    /// `sticker_ids_`.
    sticker_ids: Vec<crate::files::FileId>,
    /// `sticker_emojis_map_` — `sticker_id → emojis` (the premium-gift
    /// option pick reads it, StickersManager.cpp:2583-2595).
    sticker_emojis_map: std::collections::HashMap<crate::files::FileId, Vec<String>>,
    /// `was_loaded_` — the full-record marker (`is_loaded_` covers the
    /// id/hash half only).
    was_loaded: bool,
    /// `is_loaded_`.
    is_loaded: bool,
    /// `is_inited_` — the record carries the short name.
    is_inited: bool,
    /// `is_installed_` / `is_archived_` / `is_official_` /
    /// `is_viewed_` / `is_created_`.
    is_installed: bool,
    is_archived: bool,
    is_official: bool,
    is_viewed: bool,
    is_created: bool,
    /// `has_text_color_` / `channel_emoji_status_`.
    has_text_color: bool,
    channel_emoji_status: bool,
    /// `sticker_count_` — the unloaded-set total.
    sticker_count: i32,
    /// `hash_`.
    hash: i64,
    /// `minithumbnail_`.
    minithumbnail: String,
    /// `thumbnail_`.
    thumbnail: crate::photo::PhotoSize,
    /// `thumbnail_document_id_`.
    thumbnail_document_id: i64,
}

/// `StickersManager::SpecialStickerSet` (StickersManager.h:596-603) — the
/// per-type load state the special-set plane keeps beside the shared
/// `sticker_sets_` records (the type is the map key, matching the C++
/// `special_sticker_sets_[type]`).
#[derive(Clone, Debug, Default, PartialEq)]
struct SpecialStickerSet {
    /// `id_`.
    id: StickerSetId,
    /// `access_hash_`.
    access_hash: i64,
    /// `short_name_`.
    short_name: String,
    /// `is_being_loaded_`.
    is_being_loaded: bool,
    /// `is_being_reloaded_`.
    is_being_reloaded: bool,
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

/// `get_sticker_format_by_mime_type` (StickerFormat.cpp:27-37) — the
/// `on_get_sticker_document` mime read (StickersManager.cpp:3032).
pub(crate) fn get_sticker_format_by_mime_type(mime_type: &str) -> StickerFormat {
    match mime_type {
        "application/x-tgsticker" => StickerFormat::Tgs,
        "image/webp" => StickerFormat::Webp,
        "video/webm" => StickerFormat::Webm,
        _ => StickerFormat::Unknown,
    }
}

/// `get_sticker_format_by_extension` (StickerFormat.cpp:39-49) — the
/// `create_sticker` format-guess fallback (cpp:3385-3390).
pub(crate) fn get_sticker_format_by_extension(extension: &str) -> StickerFormat {
    match extension {
        "tgs" => StickerFormat::Tgs,
        "webp" => StickerFormat::Webp,
        "webm" => StickerFormat::Webm,
        _ => StickerFormat::Unknown,
    }
}

/// `is_sticker_format_vector` (StickerFormat.cpp:123-135) — the TGS-only
/// band (the `create_sticker` dimension default, cpp:3392-3395).
pub(crate) fn is_sticker_format_vector(sticker_format: StickerFormat) -> bool {
    sticker_format == StickerFormat::Tgs
}

/// `get_sticker_format_extension` (StickerFormat.cpp:82-93) — the
/// `PSTRING() << document_id << extension` join half
/// (StickersManager.cpp:3036-3040).
pub(crate) fn get_sticker_format_extension(sticker_format: StickerFormat) -> &'static str {
    match sticker_format {
        StickerFormat::Unknown => "",
        StickerFormat::Webp => ".webp",
        StickerFormat::Tgs => ".tgs",
        StickerFormat::Webm => ".webm",
    }
}

/// `get_sticker_format_object` (StickerFormat.cpp:51-65) — the
/// `Unknown` band LOG(ERROR)s and answers Webp.
pub(crate) fn get_sticker_format_object(sticker_format: StickerFormat) -> td_api::StickerFormat {
    match sticker_format {
        StickerFormat::Tgs => td_api::StickerFormat::stickerFormatTgs(td_api::stickerFormatTgs {}),
        StickerFormat::Webm => {
            td_api::StickerFormat::stickerFormatWebm(td_api::stickerFormatWebm {})
        }
        StickerFormat::Unknown => {
            tdutils::log_error!("Have a sticker of unknown format");
            td_api::StickerFormat::stickerFormatWebp(td_api::stickerFormatWebp {})
        }
        StickerFormat::Webp => {
            td_api::StickerFormat::stickerFormatWebp(td_api::stickerFormatWebp {})
        }
    }
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
    /// `FileView::empty()` — the unknown-file band ("File not found").
    pub empty: bool,
    pub is_encrypted: bool,
    /// `get_main_remote_location()` non-null and a non-web document —
    /// the C++ band collapses both checks into the `Option`.
    pub main_remote_document_id: Option<i64>,
    pub has_url: bool,
    pub has_full_local_location: bool,
    pub expected_size: i64,
    /// `get_full_remote_location()` non-null and a document.
    pub full_remote_document_id: Option<i64>,
    /// `get_full_remote_location()->is_photo()`.
    pub full_remote_is_photo: bool,
    /// `get_full_remote_location()->is_web()`.
    pub full_remote_is_web: bool,
}

impl StickersManager {
    /// `save_recent_stickers_to_database` (StickersManager.cpp:9342-9349)
    /// — the local rewrite of the `ssr0`/`ssr1` pmc records; the caller
    /// gates `use_sqlite_pmc`/`close_flag`.
    fn save_recent_stickers_list(&mut self, env: &mut impl StickersManagerEnv, is_attached: bool) {
        if !env.use_sqlite_pmc() || env.close_flag() {
            return;
        }
        let mut out = Vec::new();
        store_sticker_list(&mut out, &self.recent_sticker_ids[is_attached as usize]);
        env.sqlite_pmc_set_raw(if is_attached { b"ssr1" } else { b"ssr0" }, &out);
    }

    /// `save_favorite_stickers_to_database`
    /// (StickersManager.cpp:9702-9708) — the `ssfav` record twin.
    fn save_favorite_stickers_list(&mut self, env: &mut impl StickersManagerEnv) {
        if !env.use_sqlite_pmc() || env.close_flag() {
            return;
        }
        let mut out = Vec::new();
        store_sticker_list(&mut out, &self.favorite_sticker_ids);
        env.sqlite_pmc_set_raw(b"ssfav", &out);
    }
}

/// `td::full_split` (misc.h) — the no-empty-parts split.
fn full_split(s: &str, delimiter: char) -> Vec<&str> {
    s.split(delimiter).filter(|part| !part.is_empty()).collect()
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
    /// `td_->file_manager_->get_file_view(file_id).suggested_path()` —
    /// the `get_sticker_object` JPEG-thumbnail band
    /// (StickersManager.cpp:2180-2186).
    fn file_suggested_path(&mut self, _file_id: crate::files::FileId) -> String {
        String::new()
    }
    /// `file_manager_->register_remote(FullRemoteFileLocation(FileType::Sticker,
    /// …), FromServer, DialogId(), size, 0, document_id + extension)`
    /// (StickersManager.cpp:3036-3040).
    #[allow(clippy::too_many_arguments)]
    fn file_register_sticker(
        &mut self,
        _document_id: i64,
        _access_hash: i64,
        _dc_id: crate::chat_manager::DcId,
        _file_reference: String,
        _size: i64,
        _format: StickerFormat,
    ) -> crate::files::FileId {
        crate::files::FileId::default()
    }
    /// `get_photo_size(file_manager, PhotoSizeSource::thumbnail(FileType::Thumbnail,
    /// 0), …)` (StickersManager.cpp:3043-3061) — the full registered form.
    #[allow(clippy::too_many_arguments)]
    fn photo_size_registered(
        &mut self,
        _document_id: i64,
        _access_hash: i64,
        _file_reference: String,
        _dc_id: crate::chat_manager::DcId,
        _size_ptr: telegram_api::PhotoSize,
        _format: crate::photo::PhotoFormat,
    ) -> crate::photo::PhotoSizeOrMinithumbnail {
        crate::photo::PhotoSizeOrMinithumbnail::Size(crate::photo::PhotoSize::default())
    }
    /// `register_photo_size(file_manager, PhotoSizeSource::thumbnail(FileType::Thumbnail,
    /// 'f'), …)` (StickersManager.cpp:3065-3073) — the premium-animation
    /// registration.
    #[allow(clippy::too_many_arguments)]
    fn register_photo_size(
        &mut self,
        _document_id: i64,
        _access_hash: i64,
        _file_reference: String,
        _file_size: i32,
        _dc_id: crate::chat_manager::DcId,
        _format: crate::photo::PhotoFormat,
    ) -> crate::files::FileId {
        crate::files::FileId::default()
    }

    // -- the `[blocked: net]`/`[blocked: Files]` query tails; each one
    // -- owns the promise, which keeps the C++ request unanswered offline
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

    // -- the residue-ladder surfaces (the recent/favorite/hashtag/emoji
    // -- planes); the pmc and option reads are real over `Td`
    /// `td_->option_manager_->get_option_integer(name, default)`.
    fn get_option_integer(&self, _name: &str, default_value: i64) -> i64 {
        default_value
    }
    /// `G()->use_sqlite_pmc()`.
    fn use_sqlite_pmc(&self) -> bool {
        false
    }
    /// `G()->td_db()->get_sqlite_pmc()->get` (the async kv get collapses
    /// to the synchronous read, the autosave-manager convention).
    fn sqlite_pmc_get_raw(&mut self, _key: &[u8]) -> Vec<u8> {
        Vec::new()
    }
    /// `G()->td_db()->get_sqlite_pmc()->set`.
    fn sqlite_pmc_set_raw(&mut self, _key: &[u8], _value: &[u8]) {}
    /// `G()->td_db()->get_sqlite_sync_pmc()->get_by_prefix` — the
    /// callback receives the key suffix after the prefix (the C++
    /// `strip_key_prefix`), like the keyword-record scan.
    fn sqlite_pmc_get_by_prefix<F>(&mut self, _prefix: &[u8], mut _callback: F)
    where
        F: FnMut(&[u8], &[u8]),
    {
    }
    /// `G()->td_db()->get_binlog_pmc()->get` (the emoji-status planes).
    fn binlog_pmc_get_raw(&mut self, _key: &[u8]) -> Vec<u8> {
        Vec::new()
    }
    /// `G()->td_db()->get_binlog_pmc()->set`.
    fn binlog_pmc_set_raw(&mut self, _key: &[u8], _value: &[u8]) {}
    /// `td_->language_pack_manager_->get_used_language_codes()`.
    fn get_language_pack_used_language_codes(&self) -> Vec<String> {
        Vec::new()
    }
    /// `G()->mtproto_header().get_system_language_code()`.
    fn get_system_language_code(&self) -> String {
        String::new()
    }
    /// `td_->user_manager_->get_secret_chat_layer` — the C++ CHECKs
    /// `have_secret_chat` before the read; over the collapsed chat store
    /// a secret-chat id is unknown, so the default answers the current
    /// layer (the early-empty band never fires).
    fn get_secret_chat_layer(&self, _secret_chat_id: crate::user_manager::SecretChatId) -> i32 {
        144 // SecretChatLayer::SpoilerAndCustomEmojiEntities
    }

    // -- the `[blocked: net]` query tails of the residue ladder; each one
    // -- owns the caller's promise (the M.31 convention — the promise is
    // -- parked unanswered offline). The generic carries whatever result
    // -- the parked request would have answered.
    /// `load_installed_sticker_sets` (the GetAllStickersQuery load).
    fn load_installed_sticker_sets<T: Send>(
        &mut self,
        _sticker_type: StickerType,
        _promise: Promise<T>,
    ) {
    }
    /// `load_featured_sticker_sets` (the GetFeaturedStickerSetsQuery load).
    fn load_featured_sticker_sets<T: Send>(
        &mut self,
        _sticker_type: StickerType,
        _promise: Promise<T>,
    ) {
    }
    /// `load_old_featured_sticker_sets`.
    fn load_old_featured_sticker_sets<T: Send>(
        &mut self,
        _sticker_type: StickerType,
        _promise: Promise<T>,
    ) {
    }
    /// `load_sticker_sets` — the per-set load fan (the LoadStickerSets
    /// MultiPromise over `messages.getStickerSets`).
    fn load_sticker_sets<T: Send>(&mut self, _set_ids: Vec<StickerSetId>, _promise: Promise<T>) {}
    /// `GetArchivedStickerSetsQuery`.
    fn send_get_archived_sticker_sets_query<T: Send>(
        &mut self,
        _sticker_type: StickerType,
        _offset_sticker_set_id: StickerSetId,
        _limit: i32,
        _promise: Promise<T>,
    ) {
    }
    /// `GetAttachedStickerSetsQuery`.
    fn send_get_attached_stickers_query<T: Send>(
        &mut self,
        _file_id: crate::files::FileId,
        _promise: Promise<T>,
    ) {
    }
    /// `SearchStickerSetsQuery`.
    fn send_search_sticker_sets_query<T: Send>(
        &mut self,
        _sticker_type: StickerType,
        _query: String,
        _promise: Promise<T>,
    ) {
    }
    /// `do_reload_sticker_set` over `inputStickerSetShortName` — the
    /// `search_sticker_set` tail (the promise-owning twin of
    /// [`StickersManagerEnv::reload_sticker_set_by_name`]).
    fn send_search_sticker_set_query<T: Send>(
        &mut self,
        _short_name: String,
        _promise: Promise<T>,
    ) {
    }
    /// `do_reload_sticker_set` over `inputStickerSetID` — the Great Minds
    /// band of `get_sticker_set`.
    fn send_reload_sticker_set_by_id_query<T: Send>(
        &mut self,
        _set_id: StickerSetId,
        _promise: Promise<T>,
    ) {
    }
    /// `InstallStickerSetQuery`.
    fn send_install_sticker_set_query<T: Send>(
        &mut self,
        _set_id: StickerSetId,
        _is_archived: bool,
        _promise: Promise<T>,
    ) {
    }
    /// `UninstallStickerSetQuery`.
    fn send_uninstall_sticker_set_query<T: Send>(
        &mut self,
        _set_id: StickerSetId,
        _promise: Promise<T>,
    ) {
    }
    /// `GetRecentStickersQuery` — the load tail (the queued
    /// load-recent-stickers promise rides the query result).
    fn send_get_recent_stickers_query<T: Send>(
        &mut self,
        _is_attached: bool,
        _promise: Promise<T>,
    ) {
    }
    /// `GetFavedStickersQuery` — the load tail.
    fn send_get_favorite_stickers_query<T: Send>(&mut self, _promise: Promise<T>) {}
    /// `SaveRecentStickerQuery`.
    fn send_save_recent_sticker_query<T: Send>(
        &mut self,
        _is_attached: bool,
        _sticker_id: crate::files::FileId,
        _unsave: bool,
        _promise: Promise<T>,
    ) {
    }
    /// `FaveStickerQuery`.
    fn send_fave_sticker_query<T: Send>(
        &mut self,
        _sticker_id: crate::files::FileId,
        _unsave: bool,
        _promise: Promise<T>,
    ) {
    }
    /// `ClearRecentStickersQuery`.
    fn send_clear_recent_stickers_query<T: Send>(
        &mut self,
        _is_attached: bool,
        _promise: Promise<T>,
    ) {
    }
    /// `GetEmojiKeywordsLanguageQuery`.
    fn send_get_emoji_keywords_language_query<T: Send>(
        &mut self,
        _language_codes: Vec<String>,
        _promise: Promise<T>,
    ) {
    }
    /// `GetEmojiKeywordsQuery`.
    fn send_get_emoji_keywords_query<T: Send>(
        &mut self,
        _language_code: String,
        _promise: Promise<T>,
    ) {
    }
    /// `GetEmojiKeywordsDifferenceQuery` — the fire-and-forget freshness
    /// tail (`Auto()` promise in the C++).
    fn send_get_emoji_keywords_difference_query(
        &mut self,
        _language_code: String,
        _from_version: i32,
    ) {
    }

    // -- the background freshness reloads (the `Auto()`-promise queries of
    // -- the loaded bands)
    /// `reload_installed_sticker_sets` (GetAllStickersQuery, Auto()).
    fn reload_installed_sticker_sets(&mut self, _sticker_type: StickerType, _force: bool) {}
    /// `reload_featured_sticker_sets` (GetFeaturedStickerSetsQuery, Auto()).
    fn reload_featured_sticker_sets(&mut self, _sticker_type: StickerType, _force: bool) {}
    /// `reload_recent_stickers` (GetRecentStickersQuery, Auto()).
    fn reload_recent_stickers(&mut self, _is_attached: bool, _force: bool) {}
    /// `reload_favorite_stickers` (GetFavedStickersQuery, Auto()).
    fn reload_favorite_stickers(&mut self, _force: bool) {}
}

/// `HashtagHints` (HashtagHints.{h,cpp}) — the collapsed hints actor.
/// Three instances back the tag registries: the recent hashtags
/// (`"text"`, '#'), the searched-for tags (`"search"`, '#') and the
/// cashtag twin (`"cashtag_search"`, '$') — Td.cpp:1344-1347. The
/// `Hints` rating (`-++counter_`, the newest tag first) collapses to a
/// newest-first `Vec<String>`; the word/translit prefix search is
/// ported over `utf8_get_search_words` + `get_word_transliterations`.
pub struct HashtagHints {
    mode: &'static str,
    first_character: char,
    /// `sync_with_db_` — false until the `start_up` pmc replay runs.
    synced: bool,
    /// `hints_` in the rating order (newest first).
    names: Vec<String>,
    /// `counter_` — the C++ rating watermark; the collapsed newest-first
    /// `names` order already encodes the rating, so nothing reads it.
    #[allow(dead_code)]
    counter: i64,
}

/// The persisted hints size — `HashtagHints::hashtag_used` stores the
/// top 101 rated names (`hints_.search_empty(101)`).
const HASHTAG_HINTS_STORED: usize = 101;

impl Default for HashtagHints {
    /// The inert instance — the real mode/first-character pair arrives
    /// with `init_hashtag_hints` (the `StickersManager::default()`
    /// derive needs a placeholder; `start_up` never runs on it).
    fn default() -> HashtagHints {
        HashtagHints {
            mode: "",
            first_character: '#',
            synced: false,
            names: Vec::new(),
            counter: 0,
        }
    }
}

impl HashtagHints {
    pub(crate) fn new(mode: &'static str, first_character: char) -> HashtagHints {
        HashtagHints {
            mode,
            first_character,
            synced: false,
            names: Vec::new(),
            counter: 0,
        }
    }

    /// `HashtagHints::get_key`.
    fn key(&self) -> String {
        format!("hashtag_hints#{}", self.mode)
    }

    /// `HashtagHints::start_up` + `from_db` (HashtagHints.cpp:27-37,
    /// 69-96) — the async sqlite pmc get collapses to the synchronous
    /// read. `sync_with_db_` turns true under `use_sqlite_pmc` even when
    /// the store is empty or unparsable; the replay walks the stored
    /// names in reverse so the last stored name ends up the newest.
    pub(crate) fn start_up(&mut self, env: &mut impl StickersManagerEnv) {
        if !env.use_sqlite_pmc() {
            return;
        }
        self.synced = true;
        let value = env.sqlite_pmc_get_raw(self.key().as_bytes());
        if value.is_empty() {
            return;
        }
        let mut parser = match Parser::new_log_event(&value) {
            std::result::Result::Ok(parser) => parser,
            std::result::Result::Err(_) => {
                // "Failed to unserialize hashtag hints" — the empty hints
                tracing::error!("Failed to unserialize hashtag hints");
                return;
            }
        };
        let hashtags = match parser.string_vec() {
            std::result::Result::Ok(hashtags) => hashtags,
            std::result::Result::Err(_) => {
                tracing::error!("Failed to unserialize hashtag hints");
                return;
            }
        };
        for hashtag in hashtags.iter().rev() {
            self.hashtag_used_impl(hashtag);
        }
    }

    /// `HashtagHints::hashtag_used_impl` (HashtagHints.cpp:98-108) — the
    /// rating refresh collapses to a move-to-front.
    fn hashtag_used_impl(&mut self, hashtag: &str) {
        if !check_utf8(hashtag.as_bytes()) {
            tracing::error!("Trying to add invalid UTF-8 hashtag \"{hashtag}\"");
            return;
        }
        self.counter += 1;
        self.names.retain(|name| name != hashtag);
        self.names.insert(0, hashtag.to_owned());
    }

    /// `HashtagHints::hashtag_used` (HashtagHints.cpp:39-47) — the
    /// recording no-ops before the pmc replay; every accepted use
    /// rewrites the stored top-101 list. The C++ callers are the
    /// message-viewing planes (MessageContent/MessageQueryManager),
    /// still ported — nothing reachable records a use yet.
    #[allow(dead_code)]
    pub(crate) fn hashtag_used(&mut self, env: &mut impl StickersManagerEnv, hashtag: String) {
        if !self.synced {
            return;
        }
        self.hashtag_used_impl(&hashtag);
        self.save(env);
    }

    /// `HashtagHints::remove_hashtag` (HashtagHints.cpp:49-62) — the
    /// leading `first_character` strip, then the remove-or-ignore; both
    /// bands answer ok.
    pub(crate) fn remove_hashtag(
        &mut self,
        env: &mut impl StickersManagerEnv,
        mut hashtag: String,
        mut promise: Promise<()>,
    ) {
        if !self.synced {
            promise.set_value(());
            return;
        }
        if hashtag.starts_with(self.first_character) {
            hashtag.remove(0);
        }
        if let Some(position) = self.names.iter().position(|name| *name == hashtag) {
            self.names.remove(position);
            self.save(env);
        }
        // the promise is set explicitly, because the sqlite pmc waits for
        // too long before setting it (the C++ comment)
        promise.set_value(());
    }

    /// `HashtagHints::clear` (HashtagHints.cpp:64-68).
    pub(crate) fn clear(&mut self, env: &mut impl StickersManagerEnv, mut promise: Promise<()>) {
        if !self.synced {
            promise.set_value(());
            return;
        }
        self.names.clear();
        self.save(env);
        promise.set_value(());
    }

    /// `HashtagHints::query` (HashtagHints.cpp:70-78) — the prefix strip
    /// rides the same `first_character` rule as the removal.
    pub(crate) fn query(&self, prefix: &str, limit: i32, mut promise: Promise<Vec<String>>) {
        if !self.synced {
            promise.set_value(Vec::new());
            return;
        }
        let query = prefix.strip_prefix(self.first_character).unwrap_or(prefix);
        promise.set_value(self.search(query, limit));
    }

    /// `Hints::search`/`search_empty` over the rating order — the
    /// result is the rating-sorted (newest-first) limited match list.
    fn search(&self, query: &str, limit: i32) -> Vec<String> {
        if limit < 0 {
            return Vec::new();
        }
        // `Hints::search` over an empty query with
        // return_all_for_empty_query — every key in the rating order
        let mut matched: Vec<&String> = Vec::new();
        let words = utf8_get_search_words(query);
        if words.is_empty() {
            matched.extend(self.names.iter().take(HASHTAG_HINTS_STORED));
        } else {
            for name in &self.names {
                if Self::name_matches(name, &words) {
                    matched.push(name);
                }
            }
        }
        matched
            .into_iter()
            .take(limit.max(0) as usize)
            .cloned()
            .collect()
    }

    /// The `Hints::search_word` match: every query word must hit some
    /// name word by prefix, directly or through either side's
    /// transliterations.
    fn name_matches(name: &str, query_words: &[String]) -> bool {
        let name_words = utf8_get_search_words(name);
        query_words.iter().all(|query_word| {
            name_words.iter().any(|name_word| {
                name_word.starts_with(query_word.as_str())
                    || get_word_transliterations(name_word, false)
                        .iter()
                        .any(|translit| translit.starts_with(query_word.as_str()))
                    || get_word_transliterations(query_word, true)
                        .iter()
                        .any(|translit| name_word.starts_with(translit.as_str()))
            })
        })
    }

    /// The stored list write (`hashtag_used`/`clear` tails) — the
    /// serialized `keys_to_strings(hints_.search_empty(101).second)`.
    fn save(&self, env: &mut impl StickersManagerEnv) {
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        store_string_vec(
            &mut out,
            &self.names[..self.names.len().min(HASHTAG_HINTS_STORED)],
        );
        env.sqlite_pmc_set_raw(self.key().as_bytes(), &out);
    }
}

/// The `StickerListLogEvent` collapse (StickersManager.cpp:1614-1645) —
/// the C++ stores full sticker records through `store_sticker`; over the
/// collapsed sticker store the record carries the `(id, remote_id)`
/// pairs, which is all the recent/favorite registries read back.
fn store_sticker_list(out: &mut Vec<u8>, sticker_ids: &[crate::files::FileId]) {
    // `log_event_store` framing — `parse_sticker_list` reads it back
    // through `Parser::new_log_event`.
    store_log_event_version(out);
    store_i32(out, sticker_ids.len() as i32);
    for sticker_id in sticker_ids {
        store_i32(out, sticker_id.get());
        store_i32(out, sticker_id.get_remote());
    }
}

fn parse_sticker_list(
    input: &[u8],
) -> std::result::Result<Vec<crate::files::FileId>, &'static str> {
    let mut parser = Parser::new_log_event(input)?;
    let size = parser.i32_()?;
    let mut sticker_ids = Vec::with_capacity(size.max(0) as usize);
    for _ in 0..size {
        let id = parser.i32_()?;
        let remote_id = parser.i32_()?;
        sticker_ids.push(crate::files::FileId::new(id, remote_id));
    }
    parser.fetch_end()?;
    Ok(sticker_ids)
}

/// The in-flight query kinds of the net-drain 59 families — the C++
/// `Td::ResultHandler` query classes of StickersManager.cpp collapsed
/// onto the dispatcher tag; the answers drain through
/// `Td::on_stickers_net_answer` into the manager intakes. The kinds stay
/// `Copy`: the C++ handler members the answer needs beyond the kind ride
/// the per-family parked lots (the §5 shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StickersNetKind {
    /// `GetCustomEmojiDocumentsQuery` (cpp:1432-1459) —
    /// `messages.getCustomEmojiDocuments`; the request's deduped id
    /// vector + promise park in the family lot (the C++ promise lambda
    /// captures both, cpp:6570-6579), the answer re-entering
    /// [`StickersManager::on_get_custom_emoji_documents`].
    GetCustomEmojiDocuments,
    /// `ReloadSpecialStickerSetQuery` (cpp:794-832) —
    /// `messages.getStickerSet` over the special set's wire input; the
    /// kind carries the query's `sticker_set_id_` member (cpp:797, the
    /// `stickerSetNotModified` branch's key) and its `type_` (cpp:798,
    /// the `on_get_special_sticker_set` re-entry key).
    ReloadSpecialStickerSet {
        set_type: SpecialStickerSetType,
        sticker_set_id: StickerSetId,
    },
}

/// `StickersManager` — the collapsed state the request plane reads and
/// mutates. The caches start empty (offline the sets never load) and the
/// tests pre-seed them to drive the cache-hit tables.
#[derive(Default)]
pub struct StickersManager {
    /// The net-drain 59 dispatch surface — `None` keeps the net-inert
    /// posture (the pre-init windows and the net-inert tests); wired at
    /// the dispatcher-creation block through `Td::stickers_net` (the
    /// manager-held `BotRecommendationNetHandle` shape).
    pub(crate) net: Option<crate::td::StickersNetHandle>,
    /// `stickers_` — the `FileId → Sticker` record store (the M.15 core
    /// the net-drain 59 intakes build).
    stickers: std::collections::HashMap<crate::files::FileId, Sticker>,
    /// `sticker_sets_` — the `StickerSetId → StickerSet` record store.
    sticker_sets: std::collections::HashMap<StickerSetId, StickerSet>,
    /// `next_sticker_set_access_hash_` is not needed: the access hash
    /// rides the record.
    ///
    /// the parked `GetCustomEmojiDocumentsQuery` requests — the C++
    /// promise lambda's captures (cpp:6570-6579); FIFO per the C++
    /// one-query-per-request shape.
    pending_get_custom_emoji_documents: Vec<(Vec<CustomEmojiId>, Promise<td_api::Stickers>)>,
    /// `special_sticker_sets_[type]` (StickersManager.h:596-603 — the
    /// C++ unique_ptr map collapses to the plain map keyed by the type).
    special_sticker_sets: std::collections::HashMap<SpecialStickerSetType, SpecialStickerSet>,
    /// `pending_get_animated_emoji_queries_` (the re-entry pair's second
    /// half rides the stored `emoji`, cpp:6171-6179).
    pending_get_animated_emoji_queries: Vec<(String, Promise<td_api::AnimatedEmoji>)>,
    /// `pending_get_generic_animations_queries_` (cpp:6235-6243).
    pending_get_generic_animations_queries: Vec<Promise<td_api::Stickers>>,
    /// `pending_get_default_topic_icons_queries_` (cpp:6361-6369).
    pending_get_default_topic_icons_queries: Vec<Promise<td_api::Stickers>>,
    /// `pending_get_default_statuses_queries_` (cpp:6272-6280).
    pending_get_default_statuses_queries: Vec<Promise<td_api::EmojiStatusCustomEmojis>>,
    /// `pending_get_default_channel_statuses_queries_` (cpp:6331-6339).
    pending_get_default_channel_statuses_queries: Vec<Promise<td_api::EmojiStatusCustomEmojis>>,
    /// `pending_get_premium_gift_option_sticker_queries_` — the C++
    /// promise lambda's captures (cpp:2653-2660); the C++ lot also
    /// carries the `load_premium_gift_sticker_set(Promise<Unit>)`
    /// fire-and-forget riders (cpp:2618-2624, the getStarTransactions
    /// prefetch — the star-plane seam keeps its park, so the triples
    /// are all reader promises).
    pending_get_premium_gift_option_sticker_queries:
        Vec<(i32, i64, Promise<Option<td_api::Sticker>>)>,
    /// `are_installed_sticker_sets_loaded_[type]`.
    are_installed_sticker_sets_loaded: [bool; MAX_STICKER_TYPE],
    /// `installed_sticker_set_ids_[type]`.
    installed_sticker_set_ids: [Vec<StickerSetId>; MAX_STICKER_TYPE],
    /// `custom_emoji_to_sticker_id_` (the WaitFreeHashMap collapses to a
    /// plain map). `pub(crate)` for the td-level test seeds (the wire
    /// intakes fill it at their slices).
    pub(crate) custom_emoji_to_sticker_id:
        std::collections::HashMap<CustomEmojiId, crate::files::FileId>,
    /// `found_stickers_[type][emoji]` — the `(sticker_ids,
    /// next_reload_time)` cache.
    found_stickers: [std::collections::HashMap<String, (Vec<MessageId>, f64)>; MAX_STICKER_TYPE],
    /// `are_default_custom_emoji_ids_loaded_[index]`.
    are_default_custom_emoji_ids_loaded: [bool; MAX_STICKER_LIST_TYPE],
    /// `default_custom_emoji_ids_[index]`.
    default_custom_emoji_ids: [Vec<CustomEmojiId>; MAX_STICKER_LIST_TYPE],
    /// `are_recent_stickers_loaded_[is_attached]`.
    are_recent_stickers_loaded: [bool; 2],
    /// `recent_sticker_ids_[is_attached]`.
    recent_sticker_ids: [Vec<crate::files::FileId>; 2],
    /// `are_favorite_stickers_loaded_`.
    are_favorite_stickers_loaded: bool,
    /// `favorite_sticker_ids_`.
    favorite_sticker_ids: Vec<crate::files::FileId>,
    /// `attached_sticker_sets_` (the WaitFreeHashMap collapses to a map).
    attached_sticker_sets: std::collections::HashMap<crate::files::FileId, Vec<StickerSetId>>,
    /// `archived_sticker_set_ids_[type]` (the trailing `StickerSetId()`
    /// marks the known end of the list).
    archived_sticker_set_ids: [Vec<StickerSetId>; MAX_STICKER_TYPE],
    /// `total_archived_sticker_set_count_[type]` — `-1` until known.
    total_archived_sticker_set_count: [i32; MAX_STICKER_TYPE],
    /// `are_featured_sticker_sets_loaded_[type]`.
    are_featured_sticker_sets_loaded: [bool; MAX_STICKER_TYPE],
    /// `featured_sticker_set_ids_[type]`.
    featured_sticker_set_ids: [Vec<StickerSetId>; MAX_STICKER_TYPE],
    /// `old_featured_sticker_set_ids_[type]`.
    old_featured_sticker_set_ids: [Vec<StickerSetId>; MAX_STICKER_TYPE],
    /// `old_featured_sticker_set_count_[type]` — the C++ defaults are
    /// `{-1, 0, 0}` (StickersManager.h:1061); `-1` means "unknown".
    old_featured_sticker_set_count: [i32; MAX_STICKER_TYPE],
    /// `are_old_featured_sticker_sets_invalidated_[type]`.
    are_old_featured_sticker_sets_invalidated: [bool; MAX_STICKER_TYPE],
    /// `short_name_to_sticker_set_id_` — the set-name cache; over the
    /// collapsed record-less sets it is an always-miss (see
    /// `Self::search_sticker_set`), filled only by the reload planes.
    #[allow(dead_code)]
    short_name_to_sticker_set_id: std::collections::HashMap<String, StickerSetId>,
    /// `found_sticker_sets_[type][query]`.
    found_sticker_sets: [std::collections::HashMap<String, Vec<StickerSetId>>; MAX_STICKER_TYPE],
    /// `emoji_language_code_versions_[language_code]`.
    emoji_language_code_versions: std::collections::HashMap<String, i32>,
    /// `emoji_language_code_last_difference_times_[language_code]`.
    emoji_language_code_last_difference_times: std::collections::HashMap<String, f64>,
    /// `emoji_language_codes_[key]` — the `emojilc$...` cache.
    emoji_language_codes: std::collections::HashMap<String, Vec<String>>,
    /// `reloaded_emoji_keywords_` — the one-shot background reload guard.
    reloaded_emoji_keywords: std::collections::HashSet<String>,
    /// the three `HashtagHints` actors (Td.cpp:1344-1347): the recent
    /// hashtags ("text"/'#'), the searched-for tags ("search"/'#') and
    /// the cashtag twin ("cashtag_search"/'$').
    pub(crate) hashtag_hints: HashtagHints,
    pub(crate) hashtag_search_hints: HashtagHints,
    pub(crate) cashtag_search_hints: HashtagHints,
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

/// The `get_stickers` outcome — `Ready` answers the request (the
/// actor's `do_send_result`), `Parked` rides the named load seam and
/// `Failed` is the request error.
enum StickerListOutcome {
    Ready(Vec<crate::files::FileId>),
    Parked(StickerListPark),
    Failed(tdutils::status::Status),
}

/// The load the C++ `get_stickers` parks on before re-entry.
enum StickerListPark {
    Installed(StickerType),
    Recent(bool),
    Favorite,
    Featured(StickerType),
    Sets(Vec<StickerSetId>),
}

/// The `search_emojis`/`get_keyword_emojis` outcome — `Answered` is the
/// empty/unsupported band, `Parked` rides the emoji-keywords queries and
/// `Ready` carries the prepared query into the pmc keyword-record read
/// (`prepare_search_emoji_query`, StickersManager.cpp:10168-10211).
enum EmojiSearchOutcome {
    Answered,
    Parked(EmojiSearchPark),
    Ready {
        text: String,
        language_codes: Vec<String>,
    },
}

/// The load the C++ `prepare_search_emoji_query` parks on before
/// re-entry.
enum EmojiSearchPark {
    /// the `emojilc$` cache miss — the GetEmojiKeywordsLanguageQuery
    /// tail over the used-language union.
    LanguageCodes(Vec<String>),
    /// the per-language GetEmojiKeywordsQuery fan (the MultiPromise
    /// keeps only the first promise).
    Keywords(Vec<String>),
}

/// `get_mask_point_object` (StickerMaskPosition.cpp:11-25) — the wire
/// `maskCoords.n` to the `td_api::maskPoint*` table; the C++ `default:`
/// arm is `UNREACHABLE()`, reached only over the 0-3 range guard the
/// ctor enforces (cpp:32-34).
fn get_mask_point_object(point: i32) -> Option<td_api::MaskPoint> {
    match point {
        0 => Some(td_api::MaskPoint::maskPointForehead(
            td_api::maskPointForehead {},
        )),
        1 => Some(td_api::MaskPoint::maskPointEyes(td_api::maskPointEyes {})),
        2 => Some(td_api::MaskPoint::maskPointMouth(td_api::maskPointMouth {})),
        3 => Some(td_api::MaskPoint::maskPointChin(td_api::maskPointChin {})),
        _ => None,
    }
}

/// `StickerMaskPosition(mask_coords)` (StickerMaskPosition.cpp:27-43) —
/// the 0-3 point guard folds the C++ `point_ = -1` sentinel away
/// (an out-of-range `n` answers no `maskPosition`).
fn get_mask_position_object(
    mask_coords: &telegram_api::MaskCoords,
) -> Option<td_api::MaskPosition> {
    if !(0..=3).contains(&mask_coords.n) {
        return None;
    }
    Some(td_api::MaskPosition {
        point: get_mask_point_object(mask_coords.n),
        x_shift: mask_coords.x,
        y_shift: mask_coords.y,
        scale: mask_coords.zoom,
    })
}

/// `SpecialStickerSetType(InputStickerSet)` (SpecialStickerSetType.cpp:55-99)
/// — the wire special-set constructors the manager's special-set store
/// keys on. `inputStickerSetAnimatedEmojiAnimations` (the
/// `animated_emoji_click` type), `inputStickerSetDice` (the per-emoji
/// `animated_dice` type) and `inputStickerSetTonGifts` (the `ton_gifts`
/// type) answer `None` — none of the six request-plane readers owns
/// them, and `add_special_sticker_set`'s id for them stays the C++'s
/// empty `StickerSetId` until their own slices land.
fn special_sticker_set_type(
    input_sticker_set: &telegram_api::InputStickerSet,
) -> Option<SpecialStickerSetType> {
    match input_sticker_set {
        telegram_api::InputStickerSet::inputStickerSetAnimatedEmoji(_) => {
            Some(SpecialStickerSetType::AnimatedEmoji)
        }
        telegram_api::InputStickerSet::inputStickerSetEmojiDefaultTopicIcons(_) => {
            Some(SpecialStickerSetType::DefaultTopicIcons)
        }
        telegram_api::InputStickerSet::inputStickerSetEmojiDefaultStatuses(_) => {
            Some(SpecialStickerSetType::DefaultStatuses)
        }
        telegram_api::InputStickerSet::inputStickerSetEmojiChannelDefaultStatuses(_) => {
            Some(SpecialStickerSetType::DefaultChannelStatuses)
        }
        telegram_api::InputStickerSet::inputStickerSetPremiumGifts(_) => {
            Some(SpecialStickerSetType::PremiumGifts)
        }
        _ => None,
    }
}

/// `SpecialStickerSetType::type_` — the string key the binlog-pmc row
/// and the per-type map share (SpecialStickerSetType.cpp:17-53).
fn special_sticker_set_type_key(set_type: SpecialStickerSetType) -> &'static str {
    match set_type {
        SpecialStickerSetType::AnimatedEmoji => "animated_emoji_sticker_set",
        SpecialStickerSetType::GenericAnimations => "generic_animations_sticker_set",
        SpecialStickerSetType::DefaultTopicIcons => "default_topic_icons_sticker_set",
        SpecialStickerSetType::DefaultStatuses => "default_statuses_sticker_set",
        SpecialStickerSetType::DefaultChannelStatuses => "default_channel_statuses_sticker_set",
        SpecialStickerSetType::PremiumGifts => "premium_gifts_sticker_set",
    }
}

/// `SpecialStickerSetType::get_input_sticker_set()`
/// (SpecialStickerSetType.cpp:101-135) — the six request-plane special
/// sets' wire inputs (the `ReloadSpecialStickerSetQuery` body,
/// StickersManager.cpp:802).
fn get_special_sticker_set_input(set_type: SpecialStickerSetType) -> telegram_api::InputStickerSet {
    match set_type {
        SpecialStickerSetType::AnimatedEmoji => {
            telegram_api::InputStickerSet::inputStickerSetAnimatedEmoji(
                telegram_api::inputStickerSetAnimatedEmoji {},
            )
        }
        SpecialStickerSetType::GenericAnimations => {
            telegram_api::InputStickerSet::inputStickerSetEmojiGenericAnimations(
                telegram_api::inputStickerSetEmojiGenericAnimations {},
            )
        }
        SpecialStickerSetType::DefaultTopicIcons => {
            telegram_api::InputStickerSet::inputStickerSetEmojiDefaultTopicIcons(
                telegram_api::inputStickerSetEmojiDefaultTopicIcons {},
            )
        }
        SpecialStickerSetType::DefaultStatuses => {
            telegram_api::InputStickerSet::inputStickerSetEmojiDefaultStatuses(
                telegram_api::inputStickerSetEmojiDefaultStatuses {},
            )
        }
        SpecialStickerSetType::DefaultChannelStatuses => {
            telegram_api::InputStickerSet::inputStickerSetEmojiChannelDefaultStatuses(
                telegram_api::inputStickerSetEmojiChannelDefaultStatuses {},
            )
        }
        SpecialStickerSetType::PremiumGifts => {
            telegram_api::InputStickerSet::inputStickerSetPremiumGifts(
                telegram_api::inputStickerSetPremiumGifts {},
            )
        }
    }
}

/// `StickersManager::get_emoji_number` (StickersManager.cpp:6826-6834) —
/// the `'0'-'9' + U+20E3` keycap read the premium-gift option pick uses.
fn get_emoji_number(emoji: &str) -> i32 {
    let bytes = emoji.as_bytes();
    if bytes.len() != 4
        || !bytes[0].is_ascii_digit()
        || bytes[1] != 0xE2
        || bytes[2] != 0x83
        || bytes[3] != 0xA3
    {
        return -1;
    }
    i32::from(bytes[0] - b'0')
}

/// `InputStickerSet` id read — the `inputStickerSetID` band of the
/// special-set fold (`add_sticker_set` needs the id).
fn input_sticker_set_id(input_sticker_set: &telegram_api::InputStickerSet) -> Option<StickerSetId> {
    match input_sticker_set {
        telegram_api::InputStickerSet::inputStickerSetID(set) => Some(StickerSetId::new(set.id)),
        _ => None,
    }
}

/// `InputStickerSet` access-hash read.
fn input_sticker_set_access_hash(input_sticker_set: &telegram_api::InputStickerSet) -> i64 {
    match input_sticker_set {
        telegram_api::InputStickerSet::inputStickerSetID(set) => set.access_hash,
        _ => 0,
    }
}

impl StickersManager {
    pub fn new() -> StickersManager {
        // StickersManager.h:1061 — `old_featured_sticker_set_count_ =
        // {-1, 0, 0}` (only the Regular slot starts unknown).
        StickersManager {
            old_featured_sticker_set_count: [-1, 0, 0],
            ..StickersManager::default()
        }
    }

    /// `StickersManager::get_stickers_object` (StickersManager.cpp:2231-2234)
    /// — the per-id [`Self::get_sticker_object`] vector; the C++ keeps
    /// `nullptr` entries for invalid ids, which the td_api writer
    /// answers as JSON null — the collapsed port drops them (the
    /// unknown-id band, the M.15 record-less convention).
    fn get_stickers_object(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_ids: &[crate::files::FileId],
    ) -> td_api::Stickers {
        td_api::Stickers {
            stickers: sticker_ids
                .iter()
                .filter_map(|&sticker_id| self.get_sticker_object(env, sticker_id))
                .collect(),
        }
    }

    /// `get_custom_emoji_stickers_object` (StickersManager.cpp:6473-6502)
    /// — the id-map walk; an unknown id contributes nothing (the C++
    /// `file_id.is_valid()` guard), the 86400s-stale records queue a
    /// background reload (cpp:6486-6499) and the answer is the per-id
    /// [`Self::get_sticker_object`] vector.
    fn get_custom_emoji_stickers_object(
        &mut self,
        env: &mut impl StickersManagerEnv,
        custom_emoji_ids: &[CustomEmojiId],
    ) -> td_api::Stickers {
        let update_before_date = env.unix_time() - 86400;
        let mut reload_custom_emoji_ids = Vec::new();
        let mut stickers = Vec::new();
        for &custom_emoji_id in custom_emoji_ids {
            if let Some(&file_id) = self.custom_emoji_to_sticker_id.get(&custom_emoji_id) {
                if let Some(sticker) = self.stickers.get_mut(&file_id)
                    && sticker.emoji_receive_date < update_before_date
                    && !sticker.is_being_reloaded
                {
                    sticker.is_being_reloaded = true;
                    reload_custom_emoji_ids.push(custom_emoji_id);
                }
                if let Some(sticker) = self.get_sticker_object(env, file_id) {
                    stickers.push(sticker);
                }
            }
        }
        if !reload_custom_emoji_ids.is_empty() {
            // cpp:6494-6499 — the fire-and-forget reload (an empty
            // promise + id vector at the answer half)
            self.pending_get_custom_emoji_documents
                .push((Vec::new(), Promise::default()));
            match self.net.clone() {
                Some(net) => net.dispatch(
                    StickersNetKind::GetCustomEmojiDocuments,
                    telegram_api::Function::messages_getCustomEmojiDocuments(
                        telegram_api::messages_getCustomEmojiDocuments {
                            document_id: reload_custom_emoji_ids
                                .iter()
                                .map(|custom_emoji_id| custom_emoji_id.get())
                                .collect(),
                        },
                    ),
                ),
                None => {
                    tdutils::log_info!("Send messages.getCustomEmojiDocuments — [net-inert]")
                }
            }
        }
        td_api::Stickers { stickers }
    }

    /// `StickersManager::get_sticker_object` (StickersManager.cpp:2163-2226)
    /// — the `td_api::sticker` build over the record + the file plane;
    /// an invalid id or a missing record answers `None` (the C++ `nullptr`
    /// over `documentEmpty`).
    fn get_sticker_object(
        &self,
        env: &mut impl StickersManagerEnv,
        file_id: crate::files::FileId,
    ) -> Option<td_api::Sticker> {
        if !file_id.is_valid() {
            return None;
        }
        let sticker = self.stickers.get(&file_id)?;
        // cpp:2172-2188 — the `m_thumbnail_` wins, then the `s_thumbnail_`; the
        // JPEG band reads the thumbnail file's suggested path (the
        // encrypted-upload convention)
        let thumbnail = if sticker.m_thumbnail.file_id.is_valid() {
            &sticker.m_thumbnail
        } else {
            &sticker.s_thumbnail
        };
        let mut thumbnail_format = crate::photo::PhotoFormat::Webp;
        if !sticker.set_id.is_valid() {
            let view = env.file_get_view(file_id);
            let is_jpeg = view.is_encrypted
                || (thumbnail.file_id.is_valid()
                    && env.file_suggested_path(thumbnail.file_id).ends_with(".jpg"));
            if is_jpeg {
                thumbnail_format = crate::photo::PhotoFormat::Jpeg;
            }
        }
        let thumbnail_object = crate::photo::get_thumbnail_object(thumbnail, thumbnail_format);
        // cpp:2189-2203 — the zoom ladder is `for_animated_emoji`-only;
        // both flags are false at every current caller, so the plain copy
        // lands (the `Dimensions` u16 → the td_api i32)
        let width = i32::from(sticker.dimensions.width);
        let height = i32::from(sticker.dimensions.height);
        let full_type = match sticker.sticker_type {
            StickerType::Regular => {
                td_api::StickerFullType::stickerFullTypeRegular(td_api::stickerFullTypeRegular {
                    premium_animation: if sticker.premium_animation_file_id.is_valid() {
                        env.file_get_file_object(sticker.premium_animation_file_id)
                    } else {
                        None
                    },
                })
            }
            StickerType::Mask => {
                td_api::StickerFullType::stickerFullTypeMask(td_api::stickerFullTypeMask {
                    mask_position: sticker.mask_position.clone(),
                })
            }
            StickerType::CustomEmoji => td_api::StickerFullType::stickerFullTypeCustomEmoji(
                td_api::stickerFullTypeCustomEmoji {
                    // the wire `documentAttributeCustomEmoji.text_color`
                    // lands in the td_api `needs_repainting` slot (the
                    // pinned-scheme naming drift, cpp:2214-2216)
                    custom_emoji_id: sticker.get_sticker_id(&env.file_get_view(file_id)),
                    needs_repainting: sticker.has_text_color,
                },
            ),
        };
        let mut sticker_id = 0;
        if sticker.set_id.is_valid() {
            sticker_id = sticker.get_sticker_id(&env.file_get_view(file_id));
        }
        Some(td_api::Sticker {
            id: sticker_id,
            set_id: sticker.set_id.get(),
            width,
            height,
            emoji: sticker.alt.clone(),
            format: Some(get_sticker_format_object(sticker.format)),
            full_type: Some(full_type),
            thumbnail: thumbnail_object,
            sticker: env.file_get_file_object(file_id),
        })
    }

    /// `StickersManager::add_special_sticker_set(const
    /// SpecialStickerSetType&)` (cpp:1799-1815) — the per-type entry.
    fn special_sticker_set_mut(
        &mut self,
        set_type: SpecialStickerSetType,
    ) -> &mut SpecialStickerSet {
        self.special_sticker_sets.entry(set_type).or_default()
    }

    /// `StickersManager::load_special_sticker_set(SpecialStickerSet&)`
    /// (cpp:1869-1892) — the id/hash ladder: a loaded record reloads by
    /// its hash, and an unknown id reloads from the wire input. The
    /// valid-but-never-was-loaded middle band rides the
    /// `load_sticker_sets({id})` fan (the `GetStickerSetQuery` over
    /// `inputStickerSetID`) — unreachable in the collapsed port (the id
    /// only becomes valid through `on_get_special_sticker_set`, which
    /// always lands a was-loaded record; the C++ reaches it over the
    /// binlog-pmc startup read), so the fan parks here until that slice.
    fn load_special_sticker_set(
        &mut self,
        env: &mut impl StickersManagerEnv,
        set_type: SpecialStickerSetType,
    ) {
        if env.auth_is_bot() && set_type != SpecialStickerSetType::DefaultTopicIcons {
            // the C++ CHECK band (cpp:1870) — the collapsed port keeps the
            // no-op (the readers gate the bot band before the load)
            return;
        }
        let id = {
            let data = self.special_sticker_set_mut(set_type);
            if data.is_being_loaded {
                return;
            }
            data.is_being_loaded = true;
            data.id
        };
        if id.is_valid() {
            match self.sticker_sets.get(&id) {
                Some(record) if record.was_loaded => {
                    let hash = if record.is_loaded { record.hash } else { 0 };
                    self.reload_special_sticker_set(set_type, hash as i32);
                    return;
                }
                _ => {
                    // the `load_sticker_sets({id}, promise)` fan
                    // (cpp:1886-1892) — the GetStickerSetQuery slice
                    tdutils::log_info!("load_sticker_sets({}) — [blocked: net]", id.get());
                    return;
                }
            }
        }
        self.reload_special_sticker_set(set_type, 0);
    }

    /// `StickersManager::reload_special_sticker_set` (cpp:1926-1932) — the
    /// `ReloadSpecialStickerSetQuery` send (cpp:801-805).
    fn reload_special_sticker_set(&mut self, set_type: SpecialStickerSetType, hash: i32) {
        let data = self.special_sticker_set_mut(set_type);
        if data.is_being_reloaded {
            return;
        }
        data.is_being_reloaded = true;
        let sticker_set_id = data.id;
        match self.net.clone() {
            Some(net) => net.dispatch(
                StickersNetKind::ReloadSpecialStickerSet {
                    set_type,
                    sticker_set_id,
                },
                telegram_api::Function::messages_getStickerSet(
                    telegram_api::messages_getStickerSet {
                        stickerset: Some(get_special_sticker_set_input(set_type)),
                        hash,
                    },
                ),
            ),
            None => {
                tdutils::log_info!("Send messages.getStickerSet — [net-inert]");
            }
        }
    }

    /// `StickersManager::on_load_special_sticker_set`
    /// (cpp:1934-2010) — the load half's settle: the reloaded flag clears,
    /// the loaded flag clears only on success, and the per-type pending
    /// lots settle on success only. The error band (cpp:1988-2005) is the
    /// C++ divergence point kept exact: only the premium-gift lot
    /// settles — `set_promises` sets them OK, so the parked
    /// `return_premium_gift_sticker` lambdas re-enter over the still
    /// unloaded set and answer the null sticker (the ton-gift twin parks
    /// with its plane) — every other lot stays parked for the 300-600s
    /// `SleepActor` retry, which parks at the timer plane.
    pub(crate) fn on_load_special_sticker_set(
        &mut self,
        env: &mut impl StickersManagerEnv,
        set_type: SpecialStickerSetType,
        result: Result<()>,
    ) {
        if env.close_flag() {
            return;
        }
        {
            let data = self.special_sticker_set_mut(set_type);
            data.is_being_reloaded = false;
            if !data.is_being_loaded {
                return;
            }
        }
        match result {
            Result::Err(error) => {
                tdutils::log_info!(
                    "Failed to load special sticker set {:?}: {}",
                    set_type,
                    error.message()
                );
                if set_type == SpecialStickerSetType::PremiumGifts {
                    let pending =
                        std::mem::take(&mut self.pending_get_premium_gift_option_sticker_queries);
                    for (month_count, star_count, promise) in pending {
                        self.return_premium_gift_sticker(env, month_count, star_count, promise);
                    }
                }
                // cpp:1998-2005 — the 300-600s retry SleepActor parks at
                // the timer plane; the other lots stay parked until it
                // refires
            }
            Result::Ok(()) => {
                self.special_sticker_set_mut(set_type).is_being_loaded = false;
                self.settle_special_sticker_set_queries(env, set_type);
            }
        }
    }

    /// the success band of the per-type pending lots — the C++
    /// re-entry closures (cpp:1968-1995: `set_promises` runs the lambdas,
    /// which re-enter the reader with `is_recursive = true`); the
    /// `try_update_animated_emoji_messages`/
    /// `try_update_premium_gift_messages` tails (cpp:1977-1978/:1983-1984)
    /// park at the messages plane.
    fn settle_special_sticker_set_queries(
        &mut self,
        env: &mut impl StickersManagerEnv,
        set_type: SpecialStickerSetType,
    ) {
        match set_type {
            SpecialStickerSetType::AnimatedEmoji => {
                let pending = std::mem::take(&mut self.pending_get_animated_emoji_queries);
                for (emoji, promise) in pending {
                    self.get_animated_emoji(env, emoji, true, promise);
                }
            }
            SpecialStickerSetType::PremiumGifts => {
                let pending =
                    std::mem::take(&mut self.pending_get_premium_gift_option_sticker_queries);
                for (month_count, star_count, promise) in pending {
                    self.return_premium_gift_sticker(env, month_count, star_count, promise);
                }
            }
            SpecialStickerSetType::GenericAnimations => {
                for promise in std::mem::take(&mut self.pending_get_generic_animations_queries) {
                    self.get_custom_emoji_reaction_generic_animations(env, true, promise);
                }
            }
            SpecialStickerSetType::DefaultStatuses => {
                for promise in std::mem::take(&mut self.pending_get_default_statuses_queries) {
                    self.get_special_emoji_statuses(
                        env,
                        SpecialStickerSetType::DefaultStatuses,
                        true,
                        promise,
                    );
                }
            }
            SpecialStickerSetType::DefaultChannelStatuses => {
                for promise in
                    std::mem::take(&mut self.pending_get_default_channel_statuses_queries)
                {
                    self.get_special_emoji_statuses(
                        env,
                        SpecialStickerSetType::DefaultChannelStatuses,
                        true,
                        promise,
                    );
                }
            }
            SpecialStickerSetType::DefaultTopicIcons => {
                for promise in std::mem::take(&mut self.pending_get_default_topic_icons_queries) {
                    self.get_default_topic_icons(env, true, promise);
                }
            }
        }
    }

    /// `StickersManager::on_get_special_sticker_set` (cpp:4025-4054) — the
    /// `ReloadSpecialStickerSetQuery` ok half's tail: the record lands on
    /// the per-type entry, the binlog pmc row parks at the database plane.
    pub(crate) fn on_get_special_sticker_set(
        &mut self,
        env: &mut impl StickersManagerEnv,
        set_type: SpecialStickerSetType,
        sticker_set_id: StickerSetId,
    ) {
        let Some((record_access_hash, record_short_name)) = self
            .sticker_sets
            .get(&sticker_set_id)
            .map(|record| (record.access_hash, record.short_name.clone()))
        else {
            // the C++ CHECK(s != nullptr && is_inited_ && is_loaded_)
            // (cpp:4029-4031) — the record always exists at the callers
            return;
        };
        let new_short_name = clean_username(&record_short_name);
        {
            let data = self.special_sticker_set_mut(set_type);
            if sticker_set_id == data.id
                && record_access_hash == data.access_hash
                && new_short_name == data.short_name
                && !new_short_name.is_empty()
            {
                self.on_load_special_sticker_set(env, set_type, Result::Ok(()));
                return;
            }
            data.id = sticker_set_id;
            data.access_hash = record_access_hash;
            data.short_name = new_short_name.clone();
        }
        if !env.auth_is_bot() {
            // the binlog-pmc row (cpp:4043-4048) — `type → "id hash
            // short_name"`; the startup read
            // (`load_special_sticker_set_info_from_binlog`, cpp:1826-1866)
            // rides the start-up plane
            env.binlog_pmc_set_raw(
                special_sticker_set_type_key(set_type).as_bytes(),
                format!(
                    "{} {} {}",
                    sticker_set_id.get(),
                    record_access_hash,
                    new_short_name
                )
                .as_bytes(),
            );
        }
        self.special_sticker_set_mut(set_type).is_being_loaded = true;
        self.on_load_special_sticker_set(env, set_type, Result::Ok(()));
    }

    /// `StickersManager::on_get_messages_sticker_set` (cpp:3797-3950) —
    /// the `messages_StickerSet` intake: the set record build
    /// (`on_get_sticker_set`), the `stickerSetNotModified` band, the
    /// document walk and the packs walk into `sticker_emojis_map_`. The
    /// update fan-outs (`update_sticker_set`/`send_update_installed_
    /// sticker_sets`, the animated-emoji/premium-gift message refresh),
    /// the `load_requests_` promise fan, the keyword maps walk, the
    /// `emoji_stickers_map_`/`premium_sticker_positions_` tables and the
    /// `expires_at` freshness watermark park at their planes (messages,
    /// load-fan, search, premium-stickers and the bot freshness reload
    /// respectively); the expected-id mismatch's
    /// `on_load_sticker_set_fail` tail is the load-fan park.
    pub(crate) fn on_get_messages_sticker_set(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_set_id: StickerSetId,
        set_ptr: telegram_api::messages_StickerSet,
        is_changed: bool,
    ) -> StickerSetId {
        if let telegram_api::messages_StickerSet::messages_stickerSet(_) = set_ptr {
            return self.on_get_messages_sticker_set_full(env, sticker_set_id, set_ptr, is_changed);
        }
        if !sticker_set_id.is_valid() {
            tdutils::log_error!("Receive unexpected stickerSetNotModified");
            return sticker_set_id;
        }
        let Some(record) = self.sticker_sets.get_mut(&sticker_set_id) else {
            return sticker_set_id;
        };
        record.is_loaded = true;
        sticker_set_id
    }

    /// the `messages_stickerSet` arm of
    /// [`Self::on_get_messages_sticker_set`] (cpp:3816-3948).
    fn on_get_messages_sticker_set_full(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_set_id: StickerSetId,
        set_ptr: telegram_api::messages_StickerSet,
        is_changed: bool,
    ) -> StickerSetId {
        let telegram_api::messages_StickerSet::messages_stickerSet(set) = set_ptr else {
            return StickerSetId::default();
        };
        let Some(wire_set) = set.set else {
            return StickerSetId::default();
        };
        let set_id = self.on_get_sticker_set(env, wire_set, is_changed);
        if !set_id.is_valid() {
            return StickerSetId::default();
        }
        if sticker_set_id.is_valid() && sticker_set_id != set_id {
            tdutils::log_error!(
                "Expected {}, but receive {}",
                sticker_set_id.get(),
                set_id.get()
            );
            return StickerSetId::default();
        }
        if self
            .sticker_sets
            .get(&set_id)
            .is_some_and(|record| record.is_loaded)
        {
            // the already-loaded band (cpp:3835-3839): the record is up to
            // date, only the update fan-outs would run
            return set_id;
        }
        {
            let record = self.sticker_sets.entry(set_id).or_default();
            record.was_loaded = true;
            record.is_loaded = true;
            record.sticker_ids.clear();
            record.sticker_emojis_map.clear();
        }

        let is_bot = env.auth_is_bot();
        let mut document_id_to_sticker_id =
            std::collections::HashMap::<i64, crate::files::FileId>::new();
        for document in set.documents {
            let (document_id, sticker_file_id) =
                self.on_get_sticker_document(env, document, StickerFormat::Unknown);
            if !sticker_file_id.is_valid() || document_id == 0 {
                continue;
            }
            self.sticker_sets
                .entry(set_id)
                .or_default()
                .sticker_ids
                .push(sticker_file_id);
            if !is_bot {
                document_id_to_sticker_id.insert(document_id, sticker_file_id);
            }
        }
        let mut sticker_emojis_map: std::collections::HashMap<crate::files::FileId, Vec<String>> =
            std::collections::HashMap::new();
        if !is_bot {
            for pack in &set.packs {
                let cleaned_emoji = tdutils::emoji::remove_emoji_modifiers(&pack.emoticon, false);
                if cleaned_emoji.is_empty() {
                    tdutils::log_error!("Receive empty emoji in {}", set_id.get());
                    continue;
                }
                for &document_id in &pack.documents {
                    let Some(&sticker_file_id) = document_id_to_sticker_id.get(&document_id) else {
                        tdutils::log_error!(
                            "Can't find document with ID {document_id} in {}",
                            set_id.get()
                        );
                        continue;
                    };
                    sticker_emojis_map
                        .entry(sticker_file_id)
                        .or_default()
                        .push(pack.emoticon.clone());
                }
            }
        }
        let record = self.sticker_sets.entry(set_id).or_default();
        record.sticker_emojis_map = sticker_emojis_map;
        if i32::try_from(record.sticker_ids.len()).unwrap_or(i32::MAX) != record.sticker_count {
            record.sticker_count = i32::try_from(record.sticker_ids.len()).unwrap_or(i32::MAX);
        }
        set_id
    }

    /// `StickersManager::on_get_sticker_set` (cpp:3574-3795) — the
    /// `stickerSet` record build. Only the init half's fields and the
    /// `stickerSet` record build. Only the init half's fields and the
    /// guarded else-branch updates the answer builders read land (the
    /// installed-sets hints, the set-thumbnail registration and the
    /// database save park at their planes). The count/hash change band
    /// (cpp:3671-3683) resets `is_loaded` so a hash-changed reload
    /// re-runs the document walk.
    fn on_get_sticker_set(
        &mut self,
        env: &mut impl StickersManagerEnv,
        set: telegram_api::StickerSet,
        _is_changed: bool,
    ) -> StickerSetId {
        let set_id = StickerSetId::new(set.id);
        let sticker_type = if set.emojis {
            StickerType::CustomEmoji
        } else if set.masks {
            StickerType::Mask
        } else {
            StickerType::Regular
        };
        self.add_sticker_set(set_id, set.access_hash);
        let record = self.sticker_sets.entry(set_id).or_default();
        if !record.is_inited {
            record.is_inited = true;
            record.title = set.title;
            record.short_name = set.short_name;
            record.sticker_count = set.count;
            record.hash = i64::from(set.hash);
            record.is_official = set.official;
            record.sticker_type = sticker_type;
            record.has_text_color = set.emojis && set.text_color;
            record.channel_emoji_status = set.emojis && set.channel_emoji_status;
            record.is_created = set.creator;
        } else {
            if record.access_hash != set.access_hash {
                record.access_hash = set.access_hash;
            }
            if record.title != set.title {
                record.title = set.title;
            }
            if record.short_name != set.short_name {
                record.short_name = set.short_name;
            }
            if record.sticker_count != set.count || record.hash != i64::from(set.hash) {
                // cpp:3671-3683 — the count/hash change marks the record
                // stale so the caller re-runs the document walk
                record.is_loaded = false;
                record.sticker_count = set.count;
                record.hash = i64::from(set.hash);
            }
            if record.is_official != set.official {
                record.is_official = set.official;
            }
            if record.sticker_type != sticker_type {
                record.sticker_type = sticker_type;
            }
            record.has_text_color = set.emojis && set.text_color;
            record.channel_emoji_status = set.emojis && set.channel_emoji_status;
            if record.is_created != set.creator {
                record.is_created = set.creator;
            }
        }
        let _ = env;
        set_id
    }

    /// `StickersManager::get_premium_gift_sticker_set` (cpp:2528-2546) —
    /// the loaded-set read (a miss triggers the load and answers `None`).
    fn get_premium_gift_sticker_set(
        &mut self,
        env: &mut impl StickersManagerEnv,
    ) -> Option<StickerSetId> {
        if env.auth_is_bot() {
            return None;
        }
        let set_type = SpecialStickerSetType::PremiumGifts;
        let id = self.special_sticker_set_mut(set_type).id;
        if !id.is_valid() {
            self.load_special_sticker_set(env, set_type);
            return None;
        }
        let was_loaded = self.sticker_sets.get(&id)?.was_loaded;
        if !was_loaded {
            self.load_special_sticker_set(env, set_type);
            return None;
        }
        Some(id)
    }

    /// `StickersManager::get_premium_gift_sticker_object` (cpp:2496-2507)
    /// — the month/star ladder over the premium-gift set (the §6
    /// gift-options sticker leg). The `premium_gift_messages_` cache
    /// (cpp:2499-2504, the try_update_premium_gift_messages plane) parks
    /// with the messages plane — the ladder runs the option-sticker id
    /// read directly.
    pub fn get_premium_gift_sticker_object(
        &mut self,
        env: &mut impl StickersManagerEnv,
        month_count: i32,
        star_count: i64,
    ) -> Option<td_api::Sticker> {
        let month_count = if month_count == 0 {
            crate::star_manager::get_months_by_star_count(star_count)
        } else {
            month_count
        };
        let file_id = self.get_premium_gift_option_sticker_id(env, month_count)?;
        self.get_sticker_object(env, file_id)
    }

    /// `StickersManager::get_premium_gift_option_sticker_id` (cpp:2570-2601)
    /// — the month table walk over the set's `sticker_emojis_map_`.
    fn get_premium_gift_option_sticker_id(
        &mut self,
        env: &mut impl StickersManagerEnv,
        month_count: i32,
    ) -> Option<crate::files::FileId> {
        let set_id = self.get_premium_gift_sticker_set(env)?;
        let record = self.sticker_sets.get(&set_id)?;
        if record.sticker_ids.is_empty() || month_count <= 0 {
            return None;
        }
        let number = match month_count {
            1 => 1,
            3 => 2,
            6 => 3,
            12 => 4,
            24 => 5,
            _ => -1,
        };
        for &sticker_id in &record.sticker_ids {
            if let Some(emojis) = record.sticker_emojis_map.get(&sticker_id) {
                for emoji in emojis {
                    if get_emoji_number(emoji) == number {
                        return Some(sticker_id);
                    }
                }
            }
        }
        // there is no match; return the first sticker
        record.sticker_ids.first().copied()
    }

    /// `StickersManager::return_premium_gift_sticker` (cpp:2677-2680).
    fn return_premium_gift_sticker(
        &mut self,
        env: &mut impl StickersManagerEnv,
        month_count: i32,
        star_count: i64,
        mut promise: Promise<Option<td_api::Sticker>>,
    ) {
        promise.set_value(self.get_premium_gift_sticker_object(env, month_count, star_count));
    }

    /// Pop the head of the parked `GetCustomEmojiDocumentsQuery` lot —
    /// the `GetCustomEmojiDocumentsQuery` answer half's re-entry pair
    /// (the C++ promise lambda's captures, cpp:6570-6579); FIFO per the
    /// C++ one-query-per-request shape.
    pub(crate) fn take_pending_get_custom_emoji_documents(
        &mut self,
    ) -> Option<(Vec<CustomEmojiId>, Promise<td_api::Stickers>)> {
        if self.pending_get_custom_emoji_documents.is_empty() {
            None
        } else {
            Some(self.pending_get_custom_emoji_documents.remove(0))
        }
    }

    /// `on_get_custom_emoji_documents` (StickersManager.cpp:6587-6606) —
    /// the `GetCustomEmojiDocumentsQuery` answer half: the close band,
    /// the per-document `on_get_sticker_document` walk and the parked
    /// request's object answer.
    pub fn on_get_custom_emoji_documents(
        &mut self,
        env: &mut impl StickersManagerEnv,
        r_documents: tdutils::status::Result<Vec<telegram_api::Document>>,
        custom_emoji_ids: Vec<CustomEmojiId>,
        mut promise: Promise<td_api::Stickers>,
    ) {
        if env.close_flag() {
            promise.set_error(tdutils::status::Status::error(500, "Request aborted"));
            return;
        }
        let documents = match r_documents {
            Result::Err(status) => return promise.set_error(status),
            Result::Ok(documents) => documents,
        };
        for document in documents {
            if let telegram_api::Document::documentEmpty(_) = document {
                continue;
            }
            self.on_get_sticker_document(env, document, StickerFormat::Unknown);
        }
        promise.set_value(self.get_custom_emoji_stickers_object(env, &custom_emoji_ids));
    }

    /// `on_get_sticker_document` (StickersManager.cpp:2972-3067) — the
    /// sticker-document record machine: the empty/dc/attribute bands, the
    /// mime-format guard, the remote registration, the thumbnail walk and
    /// the `create_sticker` tail. Returns the `(document_id, file_id)`
    /// pair (`{}` on every drop band).
    fn on_get_sticker_document(
        &mut self,
        env: &mut impl StickersManagerEnv,
        document_ptr: telegram_api::Document,
        expected_format: StickerFormat,
    ) -> (i64, crate::files::FileId) {
        let telegram_api::Document::document(document) = document_ptr else {
            // the documentEmpty band is filtered at the callers; the
            // default arm (no other Document constructor exists)
            return (0, crate::files::FileId::default());
        };
        if !crate::chat_manager::DcId::is_valid_raw(document.dc_id) {
            tdutils::log_error!(
                "Wrong dc_id = {} in document {}",
                document.dc_id,
                document.id
            );
            return (0, crate::files::FileId::default());
        }
        let dc_id = crate::chat_manager::DcId::internal(document.dc_id);

        let mut dimensions = crate::photo::Dimensions::default();
        let mut sticker: Option<telegram_api::documentAttributeSticker> = None;
        let mut custom_emoji: Option<telegram_api::documentAttributeCustomEmoji> = None;
        for attribute in document.attributes {
            match attribute {
                telegram_api::DocumentAttribute::documentAttributeVideo(video) => {
                    dimensions = crate::photo::get_dimensions(
                        video.w,
                        video.h,
                        "sticker documentAttributeVideo",
                    );
                }
                telegram_api::DocumentAttribute::documentAttributeImageSize(image_size) => {
                    dimensions = crate::photo::get_dimensions(
                        image_size.w,
                        image_size.h,
                        "sticker documentAttributeImageSize",
                    );
                }
                telegram_api::DocumentAttribute::documentAttributeSticker(value) => {
                    sticker = Some(value);
                }
                telegram_api::DocumentAttribute::documentAttributeCustomEmoji(value) => {
                    custom_emoji = Some(value);
                }
                _ => continue,
            }
        }
        if sticker.is_none() && custom_emoji.is_none() {
            if document.mime_type != "application/x-bad-tgsticker" {
                tdutils::log_error!("Have no attributeSticker in {}", document.id);
            }
            return (0, crate::files::FileId::default());
        }

        let format = get_sticker_format_by_mime_type(&document.mime_type);
        if format == StickerFormat::Unknown
            || (expected_format != StickerFormat::Unknown && format != expected_format)
        {
            tdutils::log_error!(
                "Expected sticker of the type {:?}, but received of the type {:?}",
                expected_format,
                format
            );
            return (0, crate::files::FileId::default());
        }
        let document_id = document.id;
        let file_reference = String::from_utf8_lossy(&document.file_reference).into_owned();
        let sticker_id = env.file_register_sticker(
            document_id,
            document.access_hash,
            dc_id,
            file_reference.clone(),
            document.size,
            format,
        );

        let mut thumbnail = crate::photo::PhotoSize::default();
        let mut minithumbnail = String::new();
        let thumbnail_format = if crate::documents_manager::has_webp_thumbnail(&document.thumbs) {
            crate::photo::PhotoFormat::Webp
        } else {
            crate::photo::PhotoFormat::Jpeg
        };
        let mut premium_animation_file_id = crate::files::FileId::default();
        for thumbnail_ptr in &document.thumbs {
            let photo_size = env.photo_size_registered(
                document_id,
                document.access_hash,
                file_reference.clone(),
                dc_id,
                thumbnail_ptr.clone(),
                thumbnail_format,
            );
            match photo_size {
                crate::photo::PhotoSizeOrMinithumbnail::Size(size) => {
                    if !thumbnail.file_id.is_valid() {
                        thumbnail = size;
                    }
                    break;
                }
                crate::photo::PhotoSizeOrMinithumbnail::Minithumbnail(packed) => {
                    if thumbnail_format == crate::photo::PhotoFormat::Webp {
                        minithumbnail = packed;
                    }
                }
            }
        }
        for thumbnail_ptr in &document.video_thumbs {
            let telegram_api::VideoSize::videoSize(video_size) = thumbnail_ptr else {
                continue;
            };
            if video_size.r#type == "f" && !premium_animation_file_id.is_valid() {
                premium_animation_file_id = env.register_photo_size(
                    document_id,
                    document.access_hash,
                    file_reference.clone(),
                    video_size.size,
                    dc_id,
                    crate::documents_manager::get_sticker_format_photo_format(format),
                );
            }
        }

        self.create_sticker(
            env,
            sticker_id,
            premium_animation_file_id,
            minithumbnail,
            thumbnail,
            dimensions,
            sticker,
            custom_emoji,
            format,
        );
        (document_id, sticker_id)
    }

    /// `StickersManager::create_sticker` (StickersManager.cpp:3376-3435) —
    /// the record build over the parsed attributes (the
    /// `load_data_multipromise_ptr` half is the database-load plane and
    /// drops with it).
    #[allow(clippy::too_many_arguments)]
    fn create_sticker(
        &mut self,
        env: &mut impl StickersManagerEnv,
        file_id: crate::files::FileId,
        premium_animation_file_id: crate::files::FileId,
        minithumbnail: String,
        thumbnail: crate::photo::PhotoSize,
        mut dimensions: crate::photo::Dimensions,
        sticker: Option<telegram_api::documentAttributeSticker>,
        custom_emoji: Option<telegram_api::documentAttributeCustomEmoji>,
        mut format: StickerFormat,
    ) {
        if format == StickerFormat::Unknown && sticker.is_none() {
            format = match self.stickers.get(&file_id) {
                Some(old_sticker) => old_sticker.format,
                None => {
                    // guess the format by the file extension
                    // (cpp:3385-3390), defaulting to Webp
                    let suggested_path = env.file_suggested_path(file_id);
                    let extension = suggested_path
                        .rsplit('.')
                        .find(|part| !part.is_empty() && part.len() <= 4)
                        .unwrap_or("");
                    let guessed = get_sticker_format_by_extension(extension);
                    if guessed == StickerFormat::Unknown {
                        StickerFormat::Webp
                    } else {
                        guessed
                    }
                }
            };
        }
        if is_sticker_format_vector(format) && dimensions.width == 0 {
            dimensions.width = if custom_emoji.is_some() { 100 } else { 512 };
            dimensions.height = if custom_emoji.is_some() { 100 } else { 512 };
        }

        let mut s = Sticker {
            file_id,
            dimensions,
            format,
            ..Sticker::default()
        };
        if !env.auth_is_bot() {
            s.minithumbnail = minithumbnail;
        }
        Self::add_sticker_thumbnail(&mut s, thumbnail);
        s.is_premium = premium_animation_file_id.is_valid();
        s.premium_animation_file_id = premium_animation_file_id;
        let replace = sticker.is_some() || custom_emoji.is_some();
        if let Some(sticker) = sticker {
            s.set_id = self.on_get_input_sticker_set(sticker.stickerset);
            s.alt = sticker.alt;
            if sticker.mask {
                s.sticker_type = StickerType::Mask;
            }
            s.mask_position = sticker
                .mask_coords
                .as_ref()
                .and_then(get_mask_position_object);
        } else if let Some(custom_emoji) = custom_emoji {
            s.set_id = self.on_get_input_sticker_set(custom_emoji.stickerset);
            s.alt = custom_emoji.alt;
            s.sticker_type = StickerType::CustomEmoji;
            s.is_premium = !custom_emoji.free;
            s.has_text_color = custom_emoji.text_color;
            s.emoji_receive_date = env.unix_time();
        }
        self.on_get_sticker(env, s, replace);
    }

    /// `StickersManager::add_sticker_thumbnail` (cpp:3361-3374).
    fn add_sticker_thumbnail(s: &mut Sticker, thumbnail: crate::photo::PhotoSize) {
        if !thumbnail.file_id.is_valid() {
            return;
        }
        match thumbnail.size_type as u8 as char {
            'm' => s.m_thumbnail = thumbnail,
            's' | 't' => s.s_thumbnail = thumbnail,
            other => {
                tdutils::log_error!("Receive sticker thumbnail of unsupported type {other}");
            }
        }
    }

    /// `StickersManager::on_get_input_sticker_set` (cpp:3292-3334) — the
    /// attribute's `InputStickerSet` to `StickerSetId` fold over the
    /// empty/id/special-set bands. The `ShortName` half (cpp:3306-3325)
    /// is the database/search plane and parks at its seam.
    fn on_get_input_sticker_set(
        &mut self,
        set_ptr: Option<telegram_api::InputStickerSet>,
    ) -> StickerSetId {
        let Some(set_ptr) = set_ptr else {
            return StickerSetId::default();
        };
        match set_ptr {
            telegram_api::InputStickerSet::inputStickerSetEmpty(_) => StickerSetId::default(),
            telegram_api::InputStickerSet::inputStickerSetID(set) => {
                let set_id = StickerSetId::new(set.id);
                self.add_sticker_set(set_id, set.access_hash);
                set_id
            }
            telegram_api::InputStickerSet::inputStickerSetShortName(set) => {
                tdutils::log_error!("Receive sticker set {} by its short name", set.short_name);
                StickerSetId::default()
            }
            special => self.add_special_sticker_set(special),
        }
    }

    /// `StickersManager::add_sticker_set` (cpp:3082-3110) — the id/hash
    /// record insert (the database save half parks at its plane).
    fn add_sticker_set(&mut self, set_id: StickerSetId, access_hash: i64) {
        let sticker_set = self.sticker_sets.entry(set_id).or_default();
        if sticker_set.id.is_valid() {
            if sticker_set.access_hash != access_hash {
                sticker_set.access_hash = access_hash;
            }
        } else {
            sticker_set.id = set_id;
            sticker_set.access_hash = access_hash;
            sticker_set.is_created = true;
            sticker_set.is_inited = true;
        }
    }

    /// `StickersManager::add_special_sticker_set` (cpp:3112-3145) — the
    /// `SpecialStickerSetType`-keyed band of `on_get_input_sticker_set`
    /// (cpp:3323-3331); over the collapsed special-set store the
    /// `type_.get_input_sticker_set()` id is the record seed.
    fn add_special_sticker_set(
        &mut self,
        input_sticker_set: telegram_api::InputStickerSet,
    ) -> StickerSetId {
        if special_sticker_set_type(&input_sticker_set).is_none() {
            return StickerSetId::default();
        }
        let set_id = input_sticker_set_id(&input_sticker_set).unwrap_or_default();
        self.add_sticker_set(set_id, input_sticker_set_access_hash(&input_sticker_set));
        set_id
    }

    /// `StickersManager::on_get_sticker` (cpp:2845-2950) — the insert or
    /// the guarded replace walk, then the custom-emoji id-map tail
    /// (cpp:2929-2943) over the final record; the database save and the
    /// message-update fan-out park at their planes.
    fn on_get_sticker(
        &mut self,
        env: &mut impl StickersManagerEnv,
        new_sticker: Sticker,
        replace: bool,
    ) -> crate::files::FileId {
        let file_id = new_sticker.file_id;
        match self.stickers.get_mut(&file_id) {
            None => {
                self.stickers.insert(file_id, new_sticker);
            }
            Some(s) if replace => {
                let mut updated_custom_emoji_id = CustomEmojiId::default();
                if s.sticker_type == StickerType::CustomEmoji {
                    let view = env.file_get_view(file_id);
                    let custom_emoji_id = CustomEmojiId::new(s.get_sticker_id(&view));
                    if custom_emoji_id.is_valid()
                        && self.custom_emoji_to_sticker_id.get(&custom_emoji_id) == Some(&file_id)
                    {
                        self.custom_emoji_to_sticker_id.remove(&custom_emoji_id);
                        updated_custom_emoji_id = custom_emoji_id;
                    }
                }
                if new_sticker.dimensions.width != 0 {
                    s.dimensions = new_sticker.dimensions;
                }
                if new_sticker.set_id.is_valid() {
                    if s.set_id.is_valid() && s.set_id != new_sticker.set_id {
                        tdutils::log_error!("Sticker {} set_id has changed", file_id.get());
                    }
                    s.set_id = new_sticker.set_id;
                }
                if !new_sticker.alt.is_empty() {
                    s.alt = new_sticker.alt;
                }
                s.minithumbnail = new_sticker.minithumbnail;
                if new_sticker.s_thumbnail.file_id.is_valid() {
                    s.s_thumbnail = new_sticker.s_thumbnail;
                }
                if new_sticker.m_thumbnail.file_id.is_valid() {
                    s.m_thumbnail = new_sticker.m_thumbnail;
                }
                s.is_premium = new_sticker.is_premium;
                if new_sticker.premium_animation_file_id.is_valid() {
                    s.premium_animation_file_id = new_sticker.premium_animation_file_id;
                }
                if new_sticker.format != StickerFormat::Unknown {
                    s.format = new_sticker.format;
                }
                if new_sticker.sticker_type != StickerType::Regular {
                    s.sticker_type = new_sticker.sticker_type;
                }
                s.mask_position = new_sticker.mask_position.clone();
                if s.emoji_receive_date < new_sticker.emoji_receive_date {
                    s.emoji_receive_date = new_sticker.emoji_receive_date;
                }
                let _ = updated_custom_emoji_id;
            }
            Some(_) => {}
        }
        // cpp:2929-2943 — the custom-emoji tail over the final record
        let view = env.file_get_view(file_id);
        self.apply_custom_emoji_id(&view, file_id);
        file_id
    }

    /// the `on_get_sticker` custom-emoji map tail (cpp:2914-2943) — the
    /// id key follows the record's `get_sticker_id` and the id map keeps
    /// the last writer.
    fn apply_custom_emoji_id(&mut self, view: &StickerFileView, file_id: crate::files::FileId) {
        let Some(sticker) = self.stickers.get(&file_id) else {
            return;
        };
        if sticker.sticker_type != StickerType::CustomEmoji {
            return;
        }
        let custom_emoji_id = CustomEmojiId::new(sticker.get_sticker_id(view));
        if !custom_emoji_id.is_valid() {
            return;
        }
        if let Some(&old_file_id) = self.custom_emoji_to_sticker_id.get(&custom_emoji_id)
            && old_file_id != file_id
        {
            tdutils::log_info!(
                "Change custom emoji {} sticker from {} to {}",
                custom_emoji_id.get(),
                old_file_id.get(),
                file_id.get()
            );
        }
        self.custom_emoji_to_sticker_id
            .insert(custom_emoji_id, file_id);
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
            promise.set_value(self.get_stickers_object(env, &[]));
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
            promise.set_value(self.get_stickers_object(env, &[]));
            return;
        }

        let type_index = sticker_type as usize;
        if let Some((sticker_ids, next_reload_time)) = self.found_stickers[type_index].get(&key) {
            let result_size = (limit as usize).min(sticker_ids.len());
            let _ = &sticker_ids[..result_size];
            // the found-stickers ids are `MessageId`s over the collapsed
            // search cache — the per-id object build rides the
            // search/found-stickers plane (SearchStickersQuery), so the
            // hit answers the record-less empty band
            promise.set_value(td_api::Stickers {
                stickers: Vec::new(),
            });
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
        if env.close_flag() {
            promise.set_error(tdutils::status::Status::error(500, "Request aborted"));
            return;
        }
        let set_type = SpecialStickerSetType::AnimatedEmoji;
        let id = self.special_sticker_set_mut(set_type).id;
        let was_loaded = self.sticker_sets.get(&id).is_some_and(|s| s.was_loaded);
        if !was_loaded {
            if is_recursive {
                // the C++ promise.set_value(nullptr) is inexpressible — the
                // recorded null-response divergence stays parked
                return;
            }
            // cpp:6171-6179 — the re-entry lambda parks on the per-type lot
            self.pending_get_animated_emoji_queries
                .push((emoji, promise));
            self.load_special_sticker_set(env, set_type);
            return;
        }
        // the animated-emoji object build rides the M.15 sticker seam over
        // the per-emoji set walk (`get_animated_emoji_sticker`,
        // cpp:6188-6189) — the recorded null-response divergence parks
        let _ = &mut promise;
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
        let set_type = SpecialStickerSetType::GenericAnimations;
        let id = self.special_sticker_set_mut(set_type).id;
        let was_loaded = self.sticker_sets.get(&id).is_some_and(|s| s.was_loaded);
        if !was_loaded {
            if is_recursive {
                promise.set_value(self.get_stickers_object(env, &[]));
                return;
            }
            // cpp:6235-6243 — the re-entry lambda parks on the per-type lot
            self.pending_get_generic_animations_queries.push(promise);
            self.load_special_sticker_set(env, set_type);
            return;
        }
        let sticker_ids = self
            .sticker_sets
            .get(&id)
            .map(|record| record.sticker_ids.clone())
            .unwrap_or_default();
        promise.set_value(self.get_stickers_object(env, &sticker_ids));
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
        let set_type = SpecialStickerSetType::DefaultTopicIcons;
        let id = self.special_sticker_set_mut(set_type).id;
        let was_loaded = self.sticker_sets.get(&id).is_some_and(|s| s.was_loaded);
        if !was_loaded {
            if is_recursive {
                promise.set_value(self.get_stickers_object(env, &[]));
                return;
            }
            // cpp:6361-6369 — the re-entry lambda parks on the per-type lot
            self.pending_get_default_topic_icons_queries.push(promise);
            self.load_special_sticker_set(env, set_type);
            return;
        }
        let sticker_ids = self
            .sticker_sets
            .get(&id)
            .map(|record| record.sticker_ids.clone())
            .unwrap_or_default();
        // the bot freshness reload (cpp:6377-6389 — `!is_recursive &&
        // is_bot && unix_time >= expires_at_` → `do_reload_sticker_set`)
        // parks with the `expires_at_` watermark: the collapsed record
        // carries no expiry, so the loaded band always answers
        promise.set_value(self.get_stickers_object(env, &sticker_ids));
    }

    /// `get_default_emoji_statuses` (StickersManager.cpp:6248-6305) —
    /// the themed-statuses reader; the loaded band answers the first 8
    /// custom emoji ids of the set.
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
        let id = self.special_sticker_set_mut(set_type).id;
        let was_loaded = self.sticker_sets.get(&id).is_some_and(|s| s.was_loaded);
        if !was_loaded {
            if is_recursive {
                promise.set_value(td_api::EmojiStatusCustomEmojis {
                    custom_emoji_ids: Vec::new(),
                });
                return;
            }
            // cpp:6272-6280 / :6331-6339 — the re-entry lambda parks on the
            // per-type lot
            match set_type {
                SpecialStickerSetType::DefaultStatuses => {
                    self.pending_get_default_statuses_queries.push(promise)
                }
                SpecialStickerSetType::DefaultChannelStatuses => self
                    .pending_get_default_channel_statuses_queries
                    .push(promise),
                _ => return,
            }
            self.load_special_sticker_set(env, set_type);
            return;
        }
        // cpp:6294-6305 / :6353-6364 — the first 8 custom emoji ids of the
        // set (`get_custom_emoji_id` per sticker)
        let sticker_ids = self
            .sticker_sets
            .get(&id)
            .map(|record| record.sticker_ids.clone())
            .unwrap_or_default();
        let mut custom_emoji_ids = Vec::new();
        for sticker_id in sticker_ids {
            let custom_emoji_id = CustomEmojiId::new(
                self.stickers
                    .get(&sticker_id)
                    .map(|sticker| sticker.get_sticker_id(&env.file_get_view(sticker_id)))
                    .unwrap_or(0),
            );
            if !custom_emoji_id.is_valid() {
                tdutils::log_error!("Ignore wrong sticker {}", sticker_id.get());
                continue;
            }
            custom_emoji_ids.push(custom_emoji_id.get());
            if custom_emoji_ids.len() >= 8 {
                break;
            }
        }
        promise.set_value(td_api::EmojiStatusCustomEmojis { custom_emoji_ids });
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
            promise.set_value(self.get_custom_emoji_stickers_object(env, &custom_emoji_ids));
            return;
        }

        if use_database && env.get_option_boolean("use_sqlite_pmc", false) {
            for custom_emoji_id in unknown_custom_emoji_ids {
                env.load_custom_emoji_sticker_from_database(custom_emoji_id);
            }
            return;
        }

        // `GetCustomEmojiDocumentsQuery::send` (cpp:1437-1441) — the C++
        // promise lambda captures the deduped id vector and the request
        // promise (cpp:6570-6579); the kind rides the dispatcher and the
        // pair parks in the family lot
        self.pending_get_custom_emoji_documents
            .push((custom_emoji_ids, promise));
        match self.net.clone() {
            Some(net) => net.dispatch(
                StickersNetKind::GetCustomEmojiDocuments,
                telegram_api::Function::messages_getCustomEmojiDocuments(
                    telegram_api::messages_getCustomEmojiDocuments {
                        document_id: unknown_custom_emoji_ids
                            .iter()
                            .map(|custom_emoji_id| custom_emoji_id.get())
                            .collect(),
                    },
                ),
            ),
            None => tdutils::log_info!("Send messages.getCustomEmojiDocuments — [net-inert]"),
        }
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
            promise.set_value(self.get_custom_emoji_stickers_object(env, &ids));
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

    /// `load_premium_gift_sticker` (StickersManager.cpp:2648-2666) — the
    /// §6 gift-options sticker leg: the loaded set answers immediately,
    /// else the (month, star, promise) triple parks on the per-type lot
    /// and the premium-gift special set loads.
    pub fn load_premium_gift_sticker(
        &mut self,
        env: &mut impl StickersManagerEnv,
        month_count: i32,
        star_count: i64,
        promise: Promise<Option<td_api::Sticker>>,
    ) {
        if self.get_premium_gift_sticker_set(env).is_some() {
            return self.return_premium_gift_sticker(env, month_count, star_count, promise);
        }
        self.pending_get_premium_gift_option_sticker_queries.push((
            month_count,
            star_count,
            promise,
        ));
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
            promise.set_value(self.get_stickers_object(env, &[]));
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

    /// `StickersManager::have_sticker` (StickersManager.cpp:2075-2086) —
    /// the set record's sticker walk. The record cache itself is the
    /// stickers-plane seam (§3.9): with no records loaded, the
    /// regular-or-mask photo-sticker band answers the C++ "Sticker not
    /// found" 400 exactly like an unloaded C++ set.
    pub(crate) fn have_sticker(&self, _sticker_set_id: StickerSetId, _sticker_id: i64) -> bool {
        false
    }

    /// `StickersManager::have_custom_emoji`
    /// (StickersManager.cpp:2088-2090) — the id map read.
    pub(crate) fn have_custom_emoji(&self, custom_emoji_id: CustomEmojiId) -> bool {
        self.custom_emoji_to_sticker_id
            .contains_key(&custom_emoji_id)
    }

    /// `StickersManager::get_input_sticker_set(StickerSetId)`
    /// (StickersManager.cpp:2806-2812) — the record's `inputStickerSetID`.
    /// The record cache is the stickers-plane seam (§3.9): a miss is
    /// reachable only past `have_sticker`, which guarantees the record.
    pub(crate) fn get_input_sticker_set(
        &self,
        _sticker_set_id: StickerSetId,
    ) -> Option<telegram_api::InputStickerSet> {
        None
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

    // ------------------------------------------------------------------
    // The stickers residue ladder (the 31-arm slice): the recent/favorite
    // registries, the sets ladder, the emoji-search planes, the emoji
    // statuses and the hashtag hints.
    // ------------------------------------------------------------------

    /// `get_stickers` (StickersManager.cpp:4259-4542) over the
    /// `GetStickersRequest` actor (Requests.cpp:1361-1388) — the local
    /// decision ladder is exact; the per-set record search collapses
    /// (see `Self::get_stickers_bands`).
    #[allow(clippy::too_many_arguments)]
    pub fn get_stickers(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_type: StickerType,
        query: String,
        limit: i32,
        dialog_id: crate::dialog_id::DialogId,
        force: bool,
        mut promise: Promise<td_api::Stickers>,
    ) {
        match self.get_stickers_bands(env, sticker_type, query, limit, dialog_id, force) {
            StickerListOutcome::Ready(sticker_ids) => {
                // `get_stickers_object` — the record-less ids build the
                // empty `stickers` (the M.st collapse)
                let _ = sticker_ids;
                promise.set_value(td_api::Stickers {
                    stickers: Vec::new(),
                });
            }
            StickerListOutcome::Parked(park) => match park {
                StickerListPark::Installed(sticker_type) => {
                    env.load_installed_sticker_sets(sticker_type, promise)
                }
                StickerListPark::Recent(is_attached) => {
                    if !self.try_load_recent_stickers_inline(env, is_attached) {
                        env.send_get_recent_stickers_query(is_attached, promise);
                        return;
                    }
                    // the C++ re-run answers over the just-loaded list
                    promise.set_value(td_api::Stickers {
                        stickers: Vec::new(),
                    });
                }
                StickerListPark::Favorite => {
                    if !self.try_load_favorite_stickers_inline(env) {
                        env.send_get_favorite_stickers_query(promise);
                        return;
                    }
                    promise.set_value(td_api::Stickers {
                        stickers: Vec::new(),
                    });
                }
                StickerListPark::Featured(sticker_type) => {
                    env.load_featured_sticker_sets(sticker_type, promise)
                }
                StickerListPark::Sets(set_ids) => env.load_sticker_sets(set_ids, promise),
            },
            StickerListOutcome::Failed(error) => promise.set_error(error),
        }
    }

    /// `get_sticker_emojis_object` over the collapsed cache — the
    /// record-less ids build the empty `emojis`
    /// (StickersManager.cpp:2236-2259, the CHECK-ed record walk is
    /// unreachable over the never-loaded registry).
    fn get_sticker_emojis_object(
        &self,
        sticker_ids: &[crate::files::FileId],
        return_only_main_emoji: bool,
    ) -> td_api::Emojis {
        let _ = (sticker_ids, return_only_main_emoji);
        td_api::Emojis { emojis: Vec::new() }
    }

    /// `getAllStickerEmojisRequest` (Requests.cpp:1390-1417) — the same
    /// `get_stickers` ladder with the fixed 1,000,000 limit, answered
    /// with the `emojis` object.
    #[allow(clippy::too_many_arguments)]
    pub fn get_all_sticker_emojis(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_type: StickerType,
        query: String,
        dialog_id: crate::dialog_id::DialogId,
        return_only_main_emoji: bool,
        mut promise: Promise<td_api::Emojis>,
    ) {
        match self.get_stickers_bands(env, sticker_type, query, 1_000_000, dialog_id, false) {
            StickerListOutcome::Ready(sticker_ids) => {
                promise.set_value(
                    self.get_sticker_emojis_object(&sticker_ids, return_only_main_emoji),
                );
            }
            StickerListOutcome::Parked(park) => match park {
                StickerListPark::Installed(sticker_type) => {
                    env.load_installed_sticker_sets(sticker_type, promise)
                }
                StickerListPark::Recent(is_attached) => {
                    if !self.try_load_recent_stickers_inline(env, is_attached) {
                        env.send_get_recent_stickers_query(is_attached, promise);
                        return;
                    }
                    // the C++ re-run answers over the just-loaded list
                    promise.set_value(td_api::Emojis { emojis: Vec::new() });
                }
                StickerListPark::Favorite => {
                    if !self.try_load_favorite_stickers_inline(env) {
                        env.send_get_favorite_stickers_query(promise);
                        return;
                    }
                    promise.set_value(td_api::Emojis { emojis: Vec::new() });
                }
                StickerListPark::Featured(sticker_type) => {
                    env.load_featured_sticker_sets(sticker_type, promise)
                }
                StickerListPark::Sets(set_ids) => env.load_sticker_sets(set_ids, promise),
            },
            StickerListOutcome::Failed(error) => promise.set_error(error),
        }
    }

    /// The `get_stickers` body (StickersManager.cpp:4261-4541). The local
    /// gates (the close/limit bands, the installed/recent/favorite/featured
    /// load gates, the old-secret-chat early empty) are exact; the
    /// search half walks the loaded set records — over the collapsed
    /// state no set ever loads, so the examined registry contributes
    /// nothing and the record-less prepend ids drop (the C++
    /// `CHECK(s != nullptr)` is unreachable over a loaded registry).
    #[allow(clippy::too_many_arguments)]
    fn get_stickers_bands(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_type: StickerType,
        mut query: String,
        limit: i32,
        dialog_id: crate::dialog_id::DialogId,
        force: bool,
    ) -> StickerListOutcome {
        if env.close_flag() {
            return StickerListOutcome::Failed(tdutils::status::Status::error(
                500,
                "Request aborted",
            ));
        }
        if limit <= 0 {
            return StickerListOutcome::Failed(tdutils::status::Status::error(
                400,
                "Parameter limit must be positive",
            ));
        }

        let type_index = sticker_type as usize;
        if !self.are_installed_sticker_sets_loaded[type_index] {
            // CHECK(!force) — the actor's retry force never arrives
            // before the load
            return StickerListOutcome::Parked(StickerListPark::Installed(sticker_type));
        }

        let return_all_installed = query.is_empty();

        tdutils::emoji::remove_emoji_modifiers_in_place(&mut query, true);
        let mut emojis = full_split(&query, ' ');
        if emojis.iter().any(|emoji| !tdutils::emoji::is_emoji(emoji)) {
            emojis.clear();
        }
        let _ = emojis;

        if !return_all_installed {
            if sticker_type == StickerType::Regular {
                if !self.are_recent_stickers_loaded[0] {
                    return StickerListOutcome::Parked(StickerListPark::Recent(false));
                }
                if !self.are_favorite_stickers_loaded {
                    return StickerListOutcome::Parked(StickerListPark::Favorite);
                }
            } else if sticker_type == StickerType::CustomEmoji
                && !self.are_featured_sticker_sets_loaded[type_index]
            {
                return StickerListOutcome::Parked(StickerListPark::Featured(sticker_type));
            }
        }

        let mut allow_premium = false;
        if sticker_type == StickerType::CustomEmoji {
            match dialog_id.get_type() {
                crate::messages_manager::DialogType::User
                    if dialog_id.get_user_id() == env.get_my_id() =>
                {
                    allow_premium = true;
                }
                crate::messages_manager::DialogType::SecretChat
                    if env.get_secret_chat_layer(dialog_id.get_secret_chat_id()) < 144 =>
                {
                    // SecretChatLayer::SpoilerAndCustomEmojiEntities — the
                    // early empty answer
                    return StickerListOutcome::Ready(Vec::new());
                }
                _ => {}
            }
        }
        // `allow_premium` only feeds the record-dependent premium
        // interleave below
        let _ = allow_premium;

        let mut examined_sticker_set_ids = self.installed_sticker_set_ids[type_index].clone();
        if !return_all_installed && sticker_type == StickerType::CustomEmoji {
            examined_sticker_set_ids.extend(self.featured_sticker_set_ids[type_index].iter());
        }

        let prepend_sticker_ids = if !return_all_installed && sticker_type == StickerType::Regular {
            let mut prepend_sticker_ids = self.recent_sticker_ids[0].clone();
            for sticker_id in &self.favorite_sticker_ids {
                if !prepend_sticker_ids.contains(sticker_id) {
                    prepend_sticker_ids.push(*sticker_id);
                }
            }
            // the animated-first stable sort is over the sticker records
            prepend_sticker_ids
        } else {
            Vec::new()
        };
        let _ = prepend_sticker_ids;

        // the sets-to-load walk: no set record ever loads over the
        // collapsed state, so every examined id needs the load and
        // forces it (the C++ CHECK(get_sticker_set(id) != nullptr) and
        // the CHECK-ed prepend walk are unreachable over a loaded
        // registry)
        if !examined_sticker_set_ids.is_empty() {
            if !force {
                return StickerListOutcome::Parked(StickerListPark::Sets(examined_sticker_set_ids));
            }
            // the force retry: the load is fire-and-forget (`Auto()`)
            // and the search proceeds
            env.load_sticker_sets(examined_sticker_set_ids, Promise::<()>::default());
        }

        // the search over the (never-loaded) set records and the
        // prepend match both read sticker records: they contribute
        // nothing over the collapsed state
        StickerListOutcome::Ready(Vec::new())
    }

    /// `get_installed_sticker_sets` (StickersManager.cpp:4921-4931).
    pub fn get_installed_sticker_sets(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_type: StickerType,
        mut promise: Promise<td_api::StickerSets>,
    ) {
        let type_index = sticker_type as usize;
        if !self.are_installed_sticker_sets_loaded[type_index] {
            env.load_installed_sticker_sets(sticker_type, promise);
            return;
        }
        env.reload_installed_sticker_sets(sticker_type, false);

        // `get_sticker_sets_object(-1, ids, 1)` — the record-less ids
        // build the empty list and lift the -1 total to the size
        let _ = self.installed_sticker_set_ids[type_index].len();
        promise.set_value(td_api::StickerSets {
            total_count: 0,
            sets: Vec::new(),
        });
    }

    /// `get_sticker_set` (StickersManager.cpp:4961-4980) — the public
    /// set-by-id read.
    pub fn get_sticker_set(
        &mut self,
        env: &mut impl StickersManagerEnv,
        set_id: StickerSetId,
        mut promise: Promise<td_api::StickerSet>,
    ) {
        let _ = env;
        // the collapsed set lookup: unknown ids ride the Great Minds
        // reload seam (the do_reload_sticker_set tail) or the 400 band
        if set_id.get() == GREAT_MINDS_SET_ID {
            env.send_reload_sticker_set_by_id_query(set_id, promise);
            return;
        }
        promise.set_error(tdutils::status::Status::error(400, "Sticker set not found"));
    }

    /// `search_sticker_set` (StickersManager.cpp:5002-5019) — over the
    /// collapsed name cache the lookup always misses and the reload
    /// tail owns the promise.
    pub fn search_sticker_set(
        &mut self,
        env: &mut impl StickersManagerEnv,
        short_name_to_search: String,
        ignore_cache: bool,
        promise: Promise<td_api::StickerSet>,
    ) {
        let short_name = clean_username(&short_name_to_search);
        let _ = short_name;
        let _ = ignore_cache;
        // sticker_set == nullptr → the do_reload_sticker_set tail over
        // inputStickerSetShortName
        let short_name = clean_username(&short_name_to_search);
        env.send_search_sticker_set_query(short_name, promise);
    }

    /// `search_installed_sticker_sets` (StickersManager.cpp:5021-5042).
    pub fn search_installed_sticker_sets(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_type: StickerType,
        _query: String,
        limit: i32,
        mut promise: Promise<td_api::StickerSets>,
    ) {
        if limit < 0 {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Limit must be non-negative",
            ));
            return;
        }
        let type_index = sticker_type as usize;
        if !self.are_installed_sticker_sets_loaded[type_index] {
            env.load_installed_sticker_sets(sticker_type, promise);
            return;
        }
        env.reload_installed_sticker_sets(sticker_type, false);

        // the installed_sticker_sets_hints_ title index collapses to the
        // empty registry (set titles never load offline): the search
        // answers {0, {}} — `get_sticker_sets_object(0, {}, 5)`
        promise.set_value(td_api::StickerSets {
            total_count: 0,
            sets: Vec::new(),
        });
    }

    /// `search_sticker_sets` (StickersManager.cpp:5044-5072).
    pub fn search_sticker_sets(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_type: StickerType,
        query: String,
        mut promise: Promise<td_api::StickerSets>,
    ) {
        if sticker_type == StickerType::Mask {
            promise.set_value(td_api::StickerSets {
                total_count: 0,
                sets: Vec::new(),
            });
            return;
        }
        let type_index = sticker_type as usize;
        let q = clean_name(&query, 1000);
        if let Some(sticker_set_ids) = self.found_sticker_sets[type_index].get(&q) {
            // the found-cache hit filters the installed sets (the C++
            // CHECKs the found records exist; over the collapsed state a
            // seeded found list filters by the installed registry)
            let installed: std::collections::HashSet<StickerSetId> = self.installed_sticker_set_ids
                [type_index]
                .iter()
                .copied()
                .collect();
            let result: Vec<StickerSetId> = sticker_set_ids
                .iter()
                .copied()
                .filter(|set_id| !installed.contains(set_id))
                .collect();
            let total_count = i32::try_from(result.len()).unwrap_or(-1);
            promise.set_value(td_api::StickerSets {
                total_count,
                sets: Vec::new(),
            });
            return;
        }

        env.send_search_sticker_sets_query(sticker_type, q, promise);
    }

    /// `change_sticker_set` (StickersManager.cpp:5134-5171) — over the
    /// collapsed state the set lookup always misses: the two 400 bands
    /// are the reachable behavior.
    pub fn change_sticker_set(
        &mut self,
        env: &mut impl StickersManagerEnv,
        set_id: StickerSetId,
        is_installed: bool,
        is_archived: bool,
        mut promise: Promise<()>,
    ) {
        let _ = env;
        if is_installed && is_archived {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Sticker set can't be installed and archived simultaneously",
            ));
            return;
        }
        // sticker_set == nullptr → the 400 band
        let _ = set_id;
        promise.set_error(tdutils::status::Status::error(400, "Sticker set not found"));
    }

    /// `get_archived_sticker_sets` (StickersManager.cpp:7208-7251) — the
    /// seeded-list walk (with the trailing-sentinel check) is exact.
    pub fn get_archived_sticker_sets(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_type: StickerType,
        offset_sticker_set_id: StickerSetId,
        limit: i32,
        force: bool,
        mut promise: Promise<td_api::StickerSets>,
    ) {
        if limit <= 0 {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Parameter limit must be positive",
            ));
            return;
        }

        let type_index = sticker_type as usize;
        let sticker_set_ids = &self.archived_sticker_set_ids[type_index];
        let total_count = self.total_archived_sticker_set_count[type_index];
        if total_count >= 0 {
            let mut offset_it = 0usize;
            if offset_sticker_set_id.is_valid() {
                match sticker_set_ids
                    .iter()
                    .position(|&set_id| set_id == offset_sticker_set_id)
                {
                    Some(position) => offset_it = position + 1,
                    None => offset_it = 0,
                }
            }
            let mut result = Vec::new();
            while result.len() < limit as usize {
                if offset_it == sticker_set_ids.len() {
                    break;
                }
                let sticker_set_id = sticker_set_ids[offset_it];
                offset_it += 1;
                if !sticker_set_id.is_valid() {
                    // end of the list (the trailing StickerSetId())
                    promise.set_value(td_api::StickerSets {
                        total_count,
                        sets: Vec::new(),
                    });
                    return;
                }
                result.push(sticker_set_id);
            }
            if result.len() == limit as usize || force {
                promise.set_value(td_api::StickerSets {
                    total_count,
                    sets: Vec::new(),
                });
                return;
            }
        }

        env.send_get_archived_sticker_sets_query(
            sticker_type,
            offset_sticker_set_id,
            limit,
            promise,
        );
    }

    /// `get_featured_sticker_sets` (StickersManager.cpp:7296-7362).
    pub fn get_featured_sticker_sets(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_type: StickerType,
        mut offset: i32,
        limit: i32,
        mut promise: Promise<td_api::TrendingStickerSets>,
    ) {
        if offset < 0 {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Parameter offset must be non-negative",
            ));
            return;
        }
        if limit < 0 {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Parameter limit must be non-negative",
            ));
            return;
        }
        if limit == 0 {
            offset = 0;
        }

        if sticker_type == StickerType::Mask {
            promise.set_value(self.get_trending_sticker_sets_object(sticker_type, &[]));
            return;
        }
        let type_index = sticker_type as usize;

        if !self.are_featured_sticker_sets_loaded[type_index] {
            env.load_featured_sticker_sets(sticker_type, promise);
            return;
        }
        env.reload_featured_sticker_sets(sticker_type, false);

        let set_count = self.featured_sticker_set_ids[type_index].len() as i32;
        if offset < set_count {
            let limit = limit.min(set_count - offset);
            let begin = offset as usize;
            let end = begin + limit as usize;
            promise.set_value(self.get_trending_sticker_sets_object(
                sticker_type,
                &self.featured_sticker_set_ids[type_index][begin..end],
            ));
            return;
        }

        if offset == set_count && self.are_old_featured_sticker_sets_invalidated[type_index] {
            // invalidate_old_featured_sticker_sets — the pmc erase and
            // the generation bump ride the updates plane
            self.are_old_featured_sticker_sets_invalidated[type_index] = false;
            self.old_featured_sticker_set_ids[type_index].clear();
        }

        let old_featured_sticker_set_count = self.old_featured_sticker_set_count[type_index];
        let total_count = set_count
            + if old_featured_sticker_set_count == -1 {
                1
            } else {
                old_featured_sticker_set_count
            };
        if offset < total_count || old_featured_sticker_set_count == -1 {
            offset -= set_count;
            let set_count = self.old_featured_sticker_set_ids[type_index].len() as i32;
            if offset < set_count {
                let limit = limit.min(set_count - offset);
                let begin = offset as usize;
                let end = begin + limit as usize;
                promise.set_value(self.get_trending_sticker_sets_object(
                    sticker_type,
                    &self.old_featured_sticker_set_ids[type_index][begin..end],
                ));
                return;
            }
            if offset > set_count {
                promise.set_error(tdutils::status::Status::error(
                    400,
                    "Too big offset specified; trending sticker sets can be received only consequently",
                ));
                return;
            }

            env.load_old_featured_sticker_sets(sticker_type, promise);
            return;
        }

        promise.set_value(self.get_trending_sticker_sets_object(sticker_type, &[]));
    }

    /// `get_trending_sticker_sets_object` (StickersManager.cpp:7491-7517)
    /// — the total-count formula and the premium flag are exact; the
    /// per-set info build collapses over the never-loaded registry.
    fn get_trending_sticker_sets_object(
        &self,
        sticker_type: StickerType,
        sticker_set_ids: &[StickerSetId],
    ) -> td_api::TrendingStickerSets {
        let type_index = sticker_type as usize;
        let old_count = self.old_featured_sticker_set_count[type_index];
        let total_count = self.featured_sticker_set_ids[type_index].len() as i32
            + if old_count == -1 { 1 } else { old_count };
        let _ = sticker_set_ids;
        td_api::TrendingStickerSets {
            total_count,
            sets: Vec::new(),
            is_premium: false,
        }
    }

    /// `get_attached_sticker_sets` (StickersManager.cpp:7723-7764).
    pub fn get_attached_sticker_sets(
        &mut self,
        env: &mut impl StickersManagerEnv,
        file_id: crate::files::FileId,
        mut promise: Promise<td_api::StickerSets>,
    ) {
        if !file_id.is_valid() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Wrong file_id specified",
            ));
            return;
        }

        if let Some(sticker_set_ids) = self.attached_sticker_sets.get(&file_id) {
            let _ = sticker_set_ids;
            promise.set_value(td_api::StickerSets {
                total_count: 0,
                sets: Vec::new(),
            });
            return;
        }

        // send_get_attached_stickers_query: the empty view is the 400
        // band, the non-document/non-photo remote location answers empty
        // (the local bands), the document/photo query parks
        let file_view = env.file_get_view(file_id);
        if file_view.empty {
            promise.set_error(tdutils::status::Status::error(400, "File not found"));
            return;
        }
        if file_view.full_remote_is_photo {
            env.send_get_attached_stickers_query(file_id, promise);
            return;
        }
        if file_view.full_remote_document_id.is_some() && !file_view.full_remote_is_web {
            env.send_get_attached_stickers_query(file_id, promise);
            return;
        }
        promise.set_value(td_api::StickerSets {
            total_count: 0,
            sets: Vec::new(),
        });
    }

    /// `get_recent_stickers` (StickersManager.cpp:9018-9027).
    pub fn get_recent_stickers(
        &mut self,
        env: &mut impl StickersManagerEnv,
        is_attached: bool,
        mut promise: Promise<td_api::Stickers>,
    ) {
        if !self.try_load_recent_stickers_inline(env, is_attached) {
            env.send_get_recent_stickers_query(is_attached, promise);
            return;
        }
        env.reload_recent_stickers(is_attached, false);

        // get_stickers_object(recent_sticker_ids_[is_attached]) — the
        // record-less ids build the empty `stickers`
        let _ = self.recent_sticker_ids[is_attached as usize].len();
        promise.set_value(td_api::Stickers {
            stickers: Vec::new(),
        });
    }

    /// `load_recent_stickers` (StickersManager.cpp:9029-9051) — the
    /// sqlite pmc read collapses inline. Returns true when the lists
    /// became loaded inline (the caller continues with its loaded-band
    /// answer — the C++ request actor re-runs after the unit promise);
    /// on false the caller hands its promise to
    /// [`StickersManagerEnv::send_get_recent_stickers_query`], which
    /// parks it ([blocked: net]).
    fn try_load_recent_stickers_inline(
        &mut self,
        env: &mut impl StickersManagerEnv,
        is_attached: bool,
    ) -> bool {
        if env.auth_is_bot() {
            self.are_recent_stickers_loaded[is_attached as usize] = true;
        }
        if self.are_recent_stickers_loaded[is_attached as usize] {
            return true;
        }
        if env.use_sqlite_pmc() {
            let key: &[u8] = if is_attached { b"ssr1" } else { b"ssr0" };
            let value = env.sqlite_pmc_get_raw(key);
            if !value.is_empty() {
                match parse_sticker_list(&value) {
                    Ok(mut sticker_ids) => {
                        self.on_load_recent_stickers_finished(env, is_attached, &mut sticker_ids);
                        return true;
                    }
                    Err(_) => {
                        // "Can't load recent stickers" — the broken
                        // record falls through to the server reload
                        tracing::error!("Can't load recent stickers");
                    }
                }
            }
        }
        false
    }

    /// `on_load_recent_stickers_finished` (StickersManager.cpp:9078-9087)
    /// — the registry fill (the update emission rides the updates plane).
    fn on_load_recent_stickers_finished(
        &mut self,
        env: &mut impl StickersManagerEnv,
        is_attached: bool,
        sticker_ids: &mut Vec<crate::files::FileId>,
    ) {
        let recent_stickers_limit =
            i32::try_from(env.get_option_integer("recent_stickers_limit", 200)).unwrap_or(200);
        if sticker_ids.len() > recent_stickers_limit.max(0) as usize {
            sticker_ids.truncate(recent_stickers_limit.max(0) as usize);
        }
        self.recent_sticker_ids[is_attached as usize] = std::mem::take(sticker_ids);
        self.are_recent_stickers_loaded[is_attached as usize] = true;
    }

    /// `add_recent_sticker` (StickersManager.cpp:9163-9173).
    pub fn add_recent_sticker(
        &mut self,
        env: &mut impl StickersManagerEnv,
        is_attached: bool,
        input_file: Option<&td_api::InputFile>,
        mut promise: Promise<()>,
    ) {
        if !self.try_load_recent_stickers_inline(env, is_attached) {
            env.send_get_recent_stickers_query(is_attached, promise);
            return;
        }

        let file_id = match env.file_get_input_file_id(FileType::Sticker, input_file, false) {
            Result::Ok(file_id) => file_id,
            Result::Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        self.add_recent_sticker_impl(env, is_attached, file_id, true, promise);
    }

    /// `add_recent_sticker_impl` (StickersManager.cpp:9195-9261) — the
    /// head-equality and the sticker-record bands; over the collapsed
    /// state every non-duplicate add hits the "Sticker not found" band
    /// (the record walk needs the loaded sticker store).
    fn add_recent_sticker_impl(
        &mut self,
        env: &mut impl StickersManagerEnv,
        is_attached: bool,
        sticker_id: crate::files::FileId,
        add_on_server: bool,
        mut promise: Promise<()>,
    ) {
        let _ = add_on_server;
        if !self.try_load_recent_stickers_inline(env, is_attached) {
            env.send_get_recent_stickers_query(is_attached, promise);
            return;
        }

        let is_equal = |file_id: crate::files::FileId| {
            file_id == sticker_id
                || (file_id.get_remote() == sticker_id.get_remote() && sticker_id.get_remote() != 0)
        };

        let sticker_ids = &mut self.recent_sticker_ids[is_attached as usize];
        if !sticker_ids.is_empty() && is_equal(sticker_ids[0]) {
            // the add_to_top of an already-first sticker — the C++
            // upgrades the head's remote id and rewrites the pmc record
            // (StickersManager.cpp:9216-9225)
            if sticker_ids[0].get_remote() == 0 && sticker_id.get_remote() != 0 {
                sticker_ids[0] = sticker_id;
                self.save_recent_stickers_list(env, is_attached);
            }
            promise.set_value(());
            return;
        }

        // get_sticker(sticker_id) == nullptr → the 400 band
        promise.set_error(tdutils::status::Status::error(400, "Sticker not found"));
        let _ = env;
    }

    /// `remove_recent_sticker` (StickersManager.cpp:9263-9289) — the
    /// registry removal is local and exact; the server tail owns the
    /// promise. Like the C++, a removed-but-unknown sticker keeps the
    /// local removal and answers the 400.
    pub fn remove_recent_sticker(
        &mut self,
        env: &mut impl StickersManagerEnv,
        is_attached: bool,
        input_file: Option<&td_api::InputFile>,
        mut promise: Promise<()>,
    ) {
        if !self.try_load_recent_stickers_inline(env, is_attached) {
            env.send_get_recent_stickers_query(is_attached, promise);
            return;
        }

        let file_id = match env.file_get_input_file_id(FileType::Sticker, input_file, false) {
            Result::Ok(file_id) => file_id,
            Result::Err(error) => {
                promise.set_error(error);
                return;
            }
        };

        let is_equal = |candidate: crate::files::FileId| {
            candidate == file_id
                || (candidate.get_remote() == file_id.get_remote() && file_id.get_remote() != 0)
        };
        let sticker_ids = &mut self.recent_sticker_ids[is_attached as usize];
        if let Some(position) = sticker_ids
            .iter()
            .position(|&candidate| is_equal(candidate))
        {
            sticker_ids.remove(position);
            // get_sticker(file_id) == nullptr → the 400 band
            promise.set_error(tdutils::status::Status::error(400, "Sticker not found"));
            return;
        }

        promise.set_value(());
    }

    /// `clear_recent_stickers` (StickersManager.cpp:9291-9308) — the
    /// local clear is immediate; the server tail owns the promise.
    pub fn clear_recent_stickers(
        &mut self,
        env: &mut impl StickersManagerEnv,
        is_attached: bool,
        mut promise: Promise<()>,
    ) {
        if !self.try_load_recent_stickers_inline(env, is_attached) {
            env.send_get_recent_stickers_query(is_attached, promise);
            return;
        }

        let sticker_ids = &mut self.recent_sticker_ids[is_attached as usize];
        if sticker_ids.is_empty() {
            promise.set_value(());
            return;
        }

        env.send_clear_recent_stickers_query(is_attached, promise);
        // the C++ clears after the handler send; the update emission
        // rides the updates plane
        sticker_ids.clear();
    }

    /// `get_favorite_stickers` (StickersManager.cpp:9416-9425).
    pub fn get_favorite_stickers(
        &mut self,
        env: &mut impl StickersManagerEnv,
        mut promise: Promise<td_api::Stickers>,
    ) {
        if !self.try_load_favorite_stickers_inline(env) {
            env.send_get_favorite_stickers_query(promise);
            return;
        }
        env.reload_favorite_stickers(false);

        let _ = self.favorite_sticker_ids.len();
        promise.set_value(td_api::Stickers {
            stickers: Vec::new(),
        });
    }

    /// The inline half — true = the list is loaded (the pmc record
    /// filled it); false = the caller parks its promise via
    /// [`StickersManagerEnv::send_get_favorite_stickers_query`].
    fn try_load_favorite_stickers_inline(&mut self, env: &mut impl StickersManagerEnv) -> bool {
        if env.auth_is_bot() {
            self.are_favorite_stickers_loaded = true;
        }
        if self.are_favorite_stickers_loaded {
            return true;
        }
        if env.use_sqlite_pmc() {
            let value = env.sqlite_pmc_get_raw(b"ssfav");
            if !value.is_empty() {
                match parse_sticker_list(&value) {
                    Ok(mut sticker_ids) => {
                        self.on_load_favorite_stickers_finished(env, &mut sticker_ids);
                        return true;
                    }
                    Err(_) => {
                        tracing::error!("Can't load favorite stickers");
                    }
                }
            }
        }
        false
    }

    /// `on_load_favorite_stickers_finished`
    /// (StickersManager.cpp:9475-9483).
    fn on_load_favorite_stickers_finished(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_ids: &mut Vec<crate::files::FileId>,
    ) {
        let favorite_stickers_limit =
            i32::try_from(env.get_option_integer("favorite_stickers_limit", 5)).unwrap_or(5);
        if sticker_ids.len() > favorite_stickers_limit.max(0) as usize {
            sticker_ids.truncate(favorite_stickers_limit.max(0) as usize);
        }
        self.favorite_sticker_ids = std::mem::take(sticker_ids);
        self.are_favorite_stickers_loaded = true;
    }

    /// `add_favorite_sticker` (StickersManager.cpp:9553-9564).
    pub fn add_favorite_sticker(
        &mut self,
        env: &mut impl StickersManagerEnv,
        input_file: Option<&td_api::InputFile>,
        mut promise: Promise<()>,
    ) {
        if !self.try_load_favorite_stickers_inline(env) {
            env.send_get_favorite_stickers_query(promise);
            return;
        }

        let file_id = match env.file_get_input_file_id(FileType::Sticker, input_file, false) {
            Result::Ok(file_id) => file_id,
            Result::Err(error) => {
                promise.set_error(error);
                return;
            }
        };
        self.add_favorite_sticker_impl(env, file_id, true, promise);
    }

    /// `add_favorite_sticker_impl` (StickersManager.cpp:9584-9647) —
    /// the favorite twin of [`Self::add_recent_sticker_impl`].
    fn add_favorite_sticker_impl(
        &mut self,
        env: &mut impl StickersManagerEnv,
        sticker_id: crate::files::FileId,
        add_on_server: bool,
        mut promise: Promise<()>,
    ) {
        let _ = add_on_server;
        if !self.try_load_favorite_stickers_inline(env) {
            env.send_get_favorite_stickers_query(promise);
            return;
        }

        let is_equal = |file_id: crate::files::FileId| {
            file_id == sticker_id
                || (file_id.get_remote() == sticker_id.get_remote() && sticker_id.get_remote() != 0)
        };

        if !self.favorite_sticker_ids.is_empty() && is_equal(self.favorite_sticker_ids[0]) {
            if self.favorite_sticker_ids[0].get_remote() == 0 && sticker_id.get_remote() != 0 {
                self.favorite_sticker_ids[0] = sticker_id;
                self.save_favorite_stickers_list(env);
            }
            promise.set_value(());
            return;
        }

        // get_sticker(sticker_id) == nullptr → the 400 band
        promise.set_error(tdutils::status::Status::error(400, "Sticker not found"));
        let _ = env;
    }

    /// `remove_favorite_sticker` (StickersManager.cpp:9649-9674).
    pub fn remove_favorite_sticker(
        &mut self,
        env: &mut impl StickersManagerEnv,
        input_file: Option<&td_api::InputFile>,
        mut promise: Promise<()>,
    ) {
        if !self.try_load_favorite_stickers_inline(env) {
            env.send_get_favorite_stickers_query(promise);
            return;
        }

        let file_id = match env.file_get_input_file_id(FileType::Sticker, input_file, false) {
            Result::Ok(file_id) => file_id,
            Result::Err(error) => {
                promise.set_error(error);
                return;
            }
        };

        let is_equal = |candidate: crate::files::FileId| {
            candidate == file_id
                || (candidate.get_remote() == file_id.get_remote() && file_id.get_remote() != 0)
        };
        if let Some(position) = self
            .favorite_sticker_ids
            .iter()
            .position(|&candidate| is_equal(candidate))
        {
            self.favorite_sticker_ids.remove(position);
            // get_sticker(file_id) == nullptr → the 400 band
            promise.set_error(tdutils::status::Status::error(400, "Sticker not found"));
            return;
        }

        promise.set_value(());
    }

    /// `get_sticker_emojis` (StickersManager.cpp:9710-9757) — the
    /// input-file probe is real; the sticker-record bands collapse to
    /// the empty answer (the store never loads offline).
    pub fn get_sticker_emojis(
        &mut self,
        env: &mut impl StickersManagerEnv,
        input_file: Option<&td_api::InputFile>,
        mut promise: Promise<td_api::Emojis>,
    ) {
        let _file_id = match env.file_get_input_file_id(FileType::Sticker, input_file, false) {
            Result::Ok(file_id) => file_id,
            Result::Err(error) => {
                promise.set_error(error);
                return;
            }
        };

        // sticker == nullptr (and the set/view bands behind it) → the
        // empty `emojis`
        promise.set_value(td_api::Emojis { emojis: Vec::new() });
    }

    /// `search_emojis` (StickersManager.cpp:10213-10229) over the
    /// `SearchEmojisRequest` actor — the empty/unsupported bands answer
    /// the empty `emojiKeywords`; the prepared query reads the sqlite
    /// pmc keyword records.
    pub fn search_emojis(
        &mut self,
        env: &mut impl StickersManagerEnv,
        text: String,
        input_language_codes: Vec<String>,
        force: bool,
        mut promise: Promise<td_api::EmojiKeywords>,
    ) {
        match self.prepare_search_emoji_query(env, text, input_language_codes, force) {
            EmojiSearchOutcome::Answered => {
                promise.set_value(td_api::EmojiKeywords {
                    emoji_keywords: Vec::new(),
                });
            }
            EmojiSearchOutcome::Parked(park) => match park {
                EmojiSearchPark::LanguageCodes(language_codes) => {
                    env.send_get_emoji_keywords_language_query(language_codes, promise)
                }
                EmojiSearchPark::Keywords(language_codes) => {
                    // the MultiPromise keeps only the lock promise — the
                    // first query answers, the rest run detached
                    let mut promise = Some(promise);
                    for language_code in language_codes.iter() {
                        let promise = promise.take().unwrap_or_default();
                        env.send_get_emoji_keywords_query(language_code.clone(), promise);
                    }
                }
            },
            EmojiSearchOutcome::Ready {
                text,
                language_codes,
            } => {
                let mut result: Vec<(String, String)> = Vec::new();
                for language_code in &language_codes {
                    result.extend(self.search_language_emojis(env, language_code, &text));
                }
                // td::unique — sort + dedup over the (emoji, keyword)
                // pairs
                result.sort();
                result.dedup();
                promise.set_value(td_api::EmojiKeywords {
                    emoji_keywords: result
                        .into_iter()
                        .map(|(emoji, keyword)| td_api::EmojiKeyword { emoji, keyword })
                        .collect(),
                });
            }
        }
    }

    /// `get_keyword_emojis` (StickersManager.cpp:10231-10246) — the
    /// keyword twin of [`Self::search_emojis`].
    pub fn get_keyword_emojis(
        &mut self,
        env: &mut impl StickersManagerEnv,
        text: String,
        input_language_codes: Vec<String>,
        force: bool,
        mut promise: Promise<td_api::Emojis>,
    ) {
        match self.prepare_search_emoji_query(env, text, input_language_codes, force) {
            EmojiSearchOutcome::Answered => {
                promise.set_value(td_api::Emojis { emojis: Vec::new() });
            }
            EmojiSearchOutcome::Parked(park) => match park {
                EmojiSearchPark::LanguageCodes(language_codes) => {
                    env.send_get_emoji_keywords_language_query(language_codes, promise)
                }
                EmojiSearchPark::Keywords(language_codes) => {
                    // the MultiPromise keeps only the lock promise — the
                    // first query answers, the rest run detached
                    let mut promise = Some(promise);
                    for language_code in language_codes.iter() {
                        let promise = promise.take().unwrap_or_default();
                        env.send_get_emoji_keywords_query(language_code.clone(), promise);
                    }
                }
            },
            EmojiSearchOutcome::Ready {
                text,
                language_codes,
            } => {
                let mut result: Vec<String> = Vec::new();
                for language_code in &language_codes {
                    result.extend(self.get_keyword_language_emojis(env, language_code, &text));
                }
                result.sort();
                result.dedup();
                promise.set_value(td_api::Emojis { emojis: result });
            }
        }
    }

    /// `prepare_search_emoji_query` (StickersManager.cpp:10168-10211)
    /// plus the `get_emoji_language_codes` cache walk
    /// (StickersManager.cpp:9926-9960).
    fn prepare_search_emoji_query(
        &mut self,
        env: &mut impl StickersManagerEnv,
        text: String,
        input_language_codes: Vec<String>,
        force: bool,
    ) -> EmojiSearchOutcome {
        if text.is_empty() || !env.use_sqlite_pmc() {
            return EmojiSearchOutcome::Answered;
        }

        // get_emoji_language_codes: the used-language union, then the
        // `emojilc$` cache; the empty cache band parks at
        // load_language_codes with the used list (the promise rides the
        // GetEmojiKeywordsLanguageQuery)
        let used_language_codes = self.get_used_language_codes(env, input_language_codes, &text);
        let key = format!("emojilc${}", used_language_codes.join("$"));
        let language_codes = if let Some(cached) = self.emoji_language_codes.get(&key) {
            cached.clone()
        } else {
            let loaded = full_split(
                &String::from_utf8_lossy(&env.sqlite_pmc_get_raw(key.as_bytes())),
                '$',
            )
            .into_iter()
            .filter(|language_code| !(language_code.is_empty() || language_code.contains('$')))
            .map(ToOwned::to_owned)
            .collect::<Vec<String>>();
            self.emoji_language_codes
                .insert(key.clone(), loaded.clone());
            loaded
        };
        if language_codes.is_empty() {
            // load_language_codes(std::move(language_codes), key, promise)
            return EmojiSearchOutcome::Parked(EmojiSearchPark::LanguageCodes(used_language_codes));
        }

        // the freshness difference queries (fire-and-forget)
        for language_code in &language_codes {
            let last_difference_time =
                self.get_emoji_language_code_last_difference_time(env, language_code);
            if last_difference_time < env.now() - 3600.0
                && self.get_emoji_language_code_version(env, language_code) != 0
            {
                // EMOJI_KEYWORDS_UPDATE_DELAY = 3600
                // (StickersManager.h:486); load_emoji_keywords_difference
                // pre-bumps the freshness time to prevent simultaneous
                // requests
                self.emoji_language_code_last_difference_times
                    .insert(language_code.clone(), env.now() + 1e9);
                let from_version = self.get_emoji_language_code_version(env, language_code);
                env.send_get_emoji_keywords_difference_query(language_code.clone(), from_version);
            }
        }
        if self.reloaded_emoji_keywords.insert(key) {
            env.send_get_emoji_keywords_language_query(
                language_codes.clone(),
                Promise::<()>::default(),
            );
        }

        let languages_to_load: Vec<String> = language_codes
            .iter()
            .filter(|language_code| self.get_emoji_language_code_version(env, language_code) == 0)
            .cloned()
            .collect();
        if !languages_to_load.is_empty() {
            if !force {
                // the LoadEmojiLanguagesMultiPromiseActor over the
                // per-language GetEmojiKeywordsQuery fan
                return EmojiSearchOutcome::Parked(EmojiSearchPark::Keywords(languages_to_load));
            }
            tracing::error!("Have no {languages_to_load:?} emoji keywords");
        }

        EmojiSearchOutcome::Ready {
            text: utf8_to_lower(&text),
            language_codes,
        }
    }

    /// `get_used_language_codes` (StickersManager.cpp:9885-9920) — the
    /// language-pack + system + input + cyrillic union.
    fn get_used_language_codes(
        &self,
        env: &mut impl StickersManagerEnv,
        input_language_codes: Vec<String>,
        text: &str,
    ) -> Vec<String> {
        let mut language_codes = env.get_language_pack_used_language_codes();
        let system_language_code = env.get_system_language_code();
        if system_language_code.len() >= 2
            && !system_language_code.contains('$')
            && (system_language_code.len() == 2 || system_language_code.as_bytes()[2] == b'-')
        {
            language_codes.push(system_language_code[..2].to_owned());
        }
        for input_language_code in &input_language_codes {
            if input_language_code.len() >= 2
                && !input_language_code.contains('$')
                && (input_language_code.len() == 2 || input_language_code.as_bytes()[2] == b'-')
            {
                language_codes.push(input_language_code[..2].to_owned());
            }
        }
        if let Some(first_letter) = text.chars().next() {
            let code = first_letter as u32;
            if (0x410..=0x44F).contains(&code) || code == 0x401 || code == 0x451 {
                // the first letter is cyrillic
                let known_cyrillic = [
                    "ru", "uk", "bg", "be", "mk", "sr", "mn", "ky", "kk", "uz", "tk",
                ];
                if !known_cyrillic
                    .iter()
                    .any(|code| language_codes.iter().any(|have| have == code))
                {
                    language_codes.push("ru".to_owned());
                }
            }
        }
        language_codes.sort();
        language_codes.dedup();

        if language_codes.is_empty() {
            language_codes.push("en".to_owned());
        }
        language_codes
    }

    /// `get_emoji_language_code_version` (StickersManager.cpp:9763-9775).
    fn get_emoji_language_code_version(
        &mut self,
        env: &mut impl StickersManagerEnv,
        language_code: &str,
    ) -> i32 {
        if let Some(version) = self.emoji_language_code_versions.get(language_code) {
            return *version;
        }
        if language_code.is_empty() {
            return 0;
        }
        let value = env.sqlite_pmc_get_raw(format!("emojiv${language_code}").as_bytes());
        // to_integer<int32> over the stored decimal string; a broken
        // value reads as 0
        let version: i32 = String::from_utf8_lossy(&value).trim().parse().unwrap_or(0);
        self.emoji_language_code_versions
            .insert(language_code.to_owned(), version);
        version
    }

    /// `get_emoji_language_code_last_difference_time`
    /// (StickersManager.cpp:9781-9795).
    fn get_emoji_language_code_last_difference_time(
        &mut self,
        env: &mut impl StickersManagerEnv,
        language_code: &str,
    ) -> f64 {
        if let Some(last_difference_time) = self
            .emoji_language_code_last_difference_times
            .get(language_code)
        {
            return *last_difference_time;
        }
        if language_code.is_empty() {
            return env.now() - f64::from(env.unix_time());
        }
        let value = env.sqlite_pmc_get_raw(format!("emojid${language_code}").as_bytes());
        let old_unix_time: i32 = String::from_utf8_lossy(&value).trim().parse().unwrap_or(0);
        let passed_time = (env.unix_time() - old_unix_time).max(0);
        let last_difference_time = env.now() - f64::from(passed_time);
        self.emoji_language_code_last_difference_times
            .insert(language_code.to_owned(), last_difference_time);
        last_difference_time
    }

    /// `search_language_emojis` (StickersManager.cpp:9801-9813) — the
    /// `emoji$lc$text` prefix scan; the callback key is the suffix after
    /// the prefix (the C++ `strip_key_prefix`), so the keyword is
    /// `text + suffix`.
    fn search_language_emojis(
        &mut self,
        env: &mut impl StickersManagerEnv,
        language_code: &str,
        text: &str,
    ) -> Vec<(String, String)> {
        let key = format!("emoji${language_code}${text}");
        let mut result = Vec::new();
        let mut callback = |suffix: &[u8], value: &[u8]| {
            let keyword = format!("{text}{}", String::from_utf8_lossy(suffix));
            for emoji in full_split(&String::from_utf8_lossy(value), '$') {
                result.push((emoji.to_owned(), keyword.clone()));
            }
        };
        env.sqlite_pmc_get_by_prefix(key.as_bytes(), &mut callback);
        result
    }

    /// `get_keyword_language_emojis` (StickersManager.cpp:9815-9820).
    fn get_keyword_language_emojis(
        &mut self,
        env: &mut impl StickersManagerEnv,
        language_code: &str,
        text: &str,
    ) -> Vec<String> {
        let key = format!("emoji${language_code}${text}");
        let value = env.sqlite_pmc_get_raw(key.as_bytes());
        full_split(&String::from_utf8_lossy(&value), '$')
            .into_iter()
            .map(ToOwned::to_owned)
            .collect()
    }

    // -- the emoji-status planes (the EmojiStatus.cpp free functions the
    // -- five request arms delegate to)

    // -- the hashtag hints arms (the three HashtagHints actors)

    /// The `start_up` replay of the three hints actors
    /// (Td.cpp:1344-1347, HashtagHints.cpp:27-37).
    pub(crate) fn init_hashtag_hints(&mut self, env: &mut impl StickersManagerEnv) {
        // Td.cpp:1344-1347 — the modes name the pmc keys; constructing
        // here over the `Default` placeholders keeps the three
        // registries on distinct keys (the Default drift is inert until
        // this runs).
        self.hashtag_hints = HashtagHints::new("text", '#');
        self.hashtag_search_hints = HashtagHints::new("search", '#');
        self.cashtag_search_hints = HashtagHints::new("cashtag_search", '$');
        self.hashtag_hints.start_up(env);
        self.hashtag_search_hints.start_up(env);
        self.cashtag_search_hints.start_up(env);
    }

    /// `searchHashtags` (Requests.cpp:8981-8995) — the recent-hashtag
    /// registry query over `td_->hashtag_hints_`; the query promise
    /// re-wraps the `vector<string>` into the `hashtags` object.
    pub fn search_hashtags(
        &self,
        prefix: String,
        limit: i32,
        mut promise: Promise<td_api::Hashtags>,
    ) {
        let query_promise = PromiseCreator::lambda(move |hashtags: Vec<String>| {
            promise.set_value(td_api::Hashtags { hashtags });
        });
        self.hashtag_hints.query(&prefix, limit, query_promise);
    }

    /// `removeRecentHashtag` (Requests.cpp:8997-9002) — the
    /// recent-hashtag removal over `td_->hashtag_hints_`.
    pub fn remove_recent_hashtag(
        &mut self,
        env: &mut impl StickersManagerEnv,
        hashtag: String,
        promise: Promise<()>,
    ) {
        self.hashtag_hints.remove_hashtag(env, hashtag, promise);
    }

    /// `getSearchedForTags` (Requests.cpp:3797-3810) — the
    /// searched-for-tags query over `td_->hashtag_search_hints_` (or the
    /// cashtag twin for a '$'-prefixed prefix).
    pub fn get_searched_for_tags(
        &self,
        tag_prefix: String,
        limit: i32,
        mut promise: Promise<td_api::Hashtags>,
    ) {
        let query_promise = PromiseCreator::lambda(move |hashtags: Vec<String>| {
            promise.set_value(td_api::Hashtags { hashtags });
        });
        if tag_prefix.starts_with('$') {
            self.cashtag_search_hints
                .query(&tag_prefix, limit, query_promise);
        } else {
            self.hashtag_search_hints
                .query(&tag_prefix, limit, query_promise);
        }
    }

    /// `removeSearchedForTag` (Requests.cpp:3812-3818) — the
    /// searched-for-tag removal, routed by the '$' prefix.
    pub fn remove_searched_for_tag(
        &mut self,
        env: &mut impl StickersManagerEnv,
        tag: String,
        promise: Promise<()>,
    ) {
        if tag.starts_with('$') {
            self.cashtag_search_hints.remove_hashtag(env, tag, promise);
        } else {
            self.hashtag_search_hints.remove_hashtag(env, tag, promise);
        }
    }

    /// `clearSearchedForTags` (Requests.cpp:3820-3825) — the registry
    /// clear, routed by `clear_cashtags`.
    pub fn clear_searched_for_tags(
        &mut self,
        env: &mut impl StickersManagerEnv,
        clear_cashtags: bool,
        promise: Promise<()>,
    ) {
        if clear_cashtags {
            self.cashtag_search_hints.clear(env, promise);
        } else {
            self.hashtag_search_hints.clear(env, promise);
        }
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
        close: bool,
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
                close: false,
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
            self.close
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
        fn file_register_sticker(
            &mut self,
            _document_id: i64,
            _access_hash: i64,
            _dc_id: crate::chat_manager::DcId,
            _file_reference: String,
            _size: i64,
            _format: StickerFormat,
        ) -> crate::files::FileId {
            crate::files::FileId::new(1, 1)
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
        fn load_custom_emoji_sticker_from_database(&mut self, _custom_emoji_id: CustomEmojiId) {
            self.queries
                .borrow_mut()
                .push("LoadCustomEmojiFromDatabase".to_owned());
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
        let (promise, cell) = record::<td_api::Stickers>();
        manager.get_custom_emoji_stickers(&mut env, ids, false, promise);
        // the net-inert posture keeps the request parked in the family
        // lot (no dispatcher to ride); the deduped unknown id vector is
        // the lot's head (cpp:6570-6579)
        assert_eq!(manager.pending_get_custom_emoji_documents.len(), 1);
        assert_eq!(
            manager.pending_get_custom_emoji_documents[0]
                .0
                .iter()
                .map(|id| id.get())
                .collect::<Vec<_>>(),
            vec![1 << 21]
        );
        assert!(cell.lock().unwrap().is_none(), "the promise stays parked");
    }

    #[test]
    fn the_custom_emoji_documents_answer_bands_follow_the_cpp() {
        let mut manager = StickersManager::new();

        // the close band — the answer half fails the parked promise with
        // the close status (cpp:6590-6591)
        let mut env = TestEnv {
            close: true,
            ..TestEnv::default()
        };
        let (promise, cell) = record::<td_api::Stickers>();
        manager.on_get_custom_emoji_documents(
            &mut env,
            Result::Ok(Vec::new()),
            Vec::new(),
            promise,
        );
        assert_eq!(recorded_error(&cell), (500, "Request aborted".to_owned()));
        let mut env = TestEnv::default();

        // the error band — the codes forward verbatim (cpp:6593-6594)
        let (promise, cell) = record::<td_api::Stickers>();
        manager.on_get_custom_emoji_documents(
            &mut env,
            Result::Err(tdutils::status::Status::error(420, "EMOJI_INVALID")),
            Vec::new(),
            promise,
        );
        assert_eq!(recorded_error(&cell), (420, "EMOJI_INVALID".to_owned()));

        // the record intake — the id map + the set record land and the
        // per-id object vector answers over the stored record
        // (cpp:6596-6605)
        let custom_emoji_id = 1 << 21;
        // the id map keys on the file view's remote document id
        // (`get_sticker_id`, cpp:2101-2105)
        env.file_view.full_remote_document_id = Some(custom_emoji_id);
        let document = telegram_api::Document::document(telegram_api::document {
            id: custom_emoji_id,
            access_hash: 77,
            file_reference: b"ref".to_vec(),
            mime_type: "application/x-tgsticker".to_string(),
            size: 4096,
            thumbs: Vec::new(),
            video_thumbs: Vec::new(),
            dc_id: 2,
            attributes: vec![
                telegram_api::DocumentAttribute::documentAttributeImageSize(
                    telegram_api::documentAttributeImageSize { w: 100, h: 100 },
                ),
                telegram_api::DocumentAttribute::documentAttributeCustomEmoji(
                    telegram_api::documentAttributeCustomEmoji {
                        flags: 0,
                        free: true,
                        text_color: true,
                        alt: "\u{1f389}".to_owned(),
                        stickerset: Some(telegram_api::InputStickerSet::inputStickerSetID(
                            telegram_api::inputStickerSetID {
                                id: 4242,
                                access_hash: 99,
                            },
                        )),
                    },
                ),
            ],
            ..telegram_api::document::default()
        });
        let (promise, cell) = record::<td_api::Stickers>();
        manager.on_get_custom_emoji_documents(
            &mut env,
            Result::Ok(vec![document]),
            vec![CustomEmojiId::new(custom_emoji_id)],
            promise,
        );
        let stickers = match &*cell.lock().unwrap() {
            Some(Result::Ok(stickers)) => stickers.clone(),
            other => panic!("expected the sticker vector, got {other:?}"),
        };
        assert_eq!(stickers.stickers.len(), 1);
        let sticker = &stickers.stickers[0];
        assert_eq!(sticker.set_id, 4242);
        assert_eq!(sticker.emoji, "\u{1f389}");
        match &sticker.full_type {
            Some(td_api::StickerFullType::stickerFullTypeCustomEmoji(custom)) => {
                assert_eq!(custom.custom_emoji_id, custom_emoji_id);
                assert!(custom.needs_repainting, "the text_color attribute");
            }
            other => panic!("the custom-emoji full type: {other:?}"),
        }
        assert!(
            manager
                .custom_emoji_to_sticker_id
                .contains_key(&CustomEmojiId::new(custom_emoji_id)),
            "the id map lands"
        );

        // the known-id band — no wire query, the stored record answers
        // (cpp:6486-6493)
        manager.pending_get_custom_emoji_documents.clear();
        let (promise, cell) = record::<td_api::Stickers>();
        manager.get_custom_emoji_stickers(
            &mut env,
            vec![CustomEmojiId::new(custom_emoji_id)],
            false,
            promise,
        );
        assert!(manager.pending_get_custom_emoji_documents.is_empty());
        match &*cell.lock().unwrap() {
            Some(Result::Ok(stickers)) => assert_eq!(stickers.stickers.len(), 1),
            other => panic!("expected the cached answer, got {other:?}"),
        }
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

        // the special-set readers park their promises on the per-type lots
        // and the net-inert load keeps them parked (cpp:6171-6179/:6235-6243/
        // :6272-6280/:6331-6339/:6361-6369)
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
        assert_eq!(manager.pending_get_animated_emoji_queries.len(), 1);
        assert_eq!(manager.pending_get_generic_animations_queries.len(), 1);
        assert_eq!(manager.pending_get_default_topic_icons_queries.len(), 1);
        assert_eq!(manager.pending_get_default_statuses_queries.len(), 1);
        assert_eq!(
            manager.pending_get_default_channel_statuses_queries.len(),
            1
        );
        // the second reader of an in-flight load parks without re-sending
        let (promise, _) = record::<td_api::Stickers>();
        manager.get_custom_emoji_reaction_generic_animations(&mut env, false, promise);
        assert_eq!(manager.pending_get_generic_animations_queries.len(), 2);

        // the recursive re-entries answer the empty bands
        let (promise, cell) = record::<td_api::Stickers>();
        manager.get_custom_emoji_reaction_generic_animations(&mut env, true, promise);
        assert!(matches!(&*cell.lock().unwrap(), Some(Result::Ok(_))));

        // the premium plane parks on the per-type lot (cpp:2660-2666)
        let (promise, _) = record::<Option<td_api::Sticker>>();
        manager.load_premium_gift_sticker(&mut env, 3, 0, promise);
        assert_eq!(
            manager
                .pending_get_premium_gift_option_sticker_queries
                .len(),
            1
        );
        let (promise, _) = record::<td_api::Stickers>();
        manager.get_premium_stickers(&mut env, 10, promise);
        assert_eq!(env.recorded("LoadPremiumStickers"), 1);

        // the load-settle bands (cpp:1934-2010): the error band settles
        // only the premium-gift lot, over the OK arm — the re-entered
        // `return_premium_gift_sticker` runs over the still-unloaded set
        // and answers the null sticker — while every other lot stays
        // parked for the 300-600s retry (the parked timer plane); the ok
        // band clears the loaded flag and re-enters the readers, whose
        // recursive not-loaded arms answer the empty bands
        let (promise, cell) = record::<Option<td_api::Sticker>>();
        manager.load_premium_gift_sticker(&mut env, 6, 0, promise);
        assert_eq!(
            manager
                .pending_get_premium_gift_option_sticker_queries
                .len(),
            2
        );
        manager.on_load_special_sticker_set(
            &mut env,
            SpecialStickerSetType::PremiumGifts,
            Result::Err(tdutils::status::Status::error(420, "STICKERSET_INVALID")),
        );
        assert!(
            manager
                .pending_get_premium_gift_option_sticker_queries
                .is_empty(),
            "the premium-gift lot settles on the failed load"
        );
        assert!(matches!(&*cell.lock().unwrap(), Some(Result::Ok(None))));
        assert_eq!(
            manager.pending_get_generic_animations_queries.len(),
            2,
            "the non-premium lots stay parked for the retry"
        );
        manager.on_load_special_sticker_set(
            &mut env,
            SpecialStickerSetType::GenericAnimations,
            Result::Ok(()),
        );
        assert!(manager.pending_get_generic_animations_queries.is_empty());

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
