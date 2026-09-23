//! Port of the photo value classes the UserManager photos listing needs
//! (`td/telegram/Photo.h`, `PhotoSize.h`, `StickerPhotoSize.h`,
//! `Dimensions.h`) — the M.4.3 remainder that folds into M.4.13.
//!
//! The `FileManager` coupling is reduced until the Files step, exactly
//! like `ProfilePhoto` in `user_manager` (module docs there): every
//! `FileId` a size carries is a placeholder that never becomes valid, so
//! the file registration inside `get_photo_size`/`get_animation_size`/
//! `register_user_photo`'s type probe is a documented no-op and the
//! `td_api` file payloads are the codegen-forced empty objects. The
//! structural walk — size classification, the type-string validation,
//! the unallowed-type skip, the square-animation rule, the stripped
//! minithumbnail expansion — is 1:1 with the C++
//! (`PhotoSize.cpp:38-189`, `Photo.cpp:57-115`,
//! `StickerPhotoSize.cpp:95-128`).

use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::FileId;
use base64::Engine as _;
use tdmtproto::telegram_api;

/// The `storer.context()->td()` slice the FileId-carrying db records read —
/// the FileManager FileId codec (`FileManager::store_file`/`parse_file`,
/// the M.40 icon-plane encoding). Shared by the DialogPhoto and the
/// animation/pm records.
pub trait DialogPhotoStoreEnv {
    fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>);
    fn parse_file_id(&mut self, parser: &mut Parser<'_>) -> Result<FileId, &'static str>;
}

/// `td::Dimensions` (Dimensions.h:14) with `get_dimensions`
/// (Dimensions.cpp:23-31): a dimension outside `0..=u16::MAX` clamps to
/// zero (logged), and a zero on either axis zeroes both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dimensions {
    pub width: u16,
    pub height: u16,
}

fn get_dimension(size: i32, source: &str) -> u16 {
    if size < 0 || size > u16::MAX as i32 {
        tdutils::log_error!("Wrong image dimension = {size} from {source}");
        return 0;
    }
    size as u16
}

/// `get_dimensions` (Dimensions.cpp:23).
pub fn get_dimensions(width: i32, height: i32, source: &str) -> Dimensions {
    let mut result = Dimensions {
        width: get_dimension(width, source),
        height: get_dimension(height, source),
    };
    if result.width == 0 || result.height == 0 {
        result.width = 0;
        result.height = 0;
    }
    result
}

/// `get_dimensions_pixel_count` (Dimensions.cpp:33).
pub fn get_dimensions_pixel_count(dimensions: Dimensions) -> u32 {
    dimensions.width as u32 * dimensions.height as u32
}

/// `td::PhotoSizeType` (PhotoSizeType.h) is an `int32` newtype compared
/// against ASCII type characters; `0` is the empty type.
pub type PhotoSizeType = i32;

/// `td::PhotoSize` (PhotoSize.h:32). `file_id` is the placeholder until
/// the Files step (module docs).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PhotoSize {
    pub size_type: PhotoSizeType,
    pub size: i32,
    pub dimensions: Dimensions,
    pub file_id: FileId,
    pub progressive_sizes: Vec<i32>,
}

/// `td::AnimationSize` (PhotoSize.h:40) — the photo-size fields plus the
/// stream start.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AnimationSize {
    pub photo_size: PhotoSize,
    pub main_frame_timestamp: f64,
}

/// `td::PhotoFormat` (PhotoFormat.h).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhotoFormat {
    Jpeg,
    Png,
    Webp,
    Gif,
    Tgs,
    Mpeg4,
    Webm,
}

impl Dimensions {
    /// `store(Dimensions, StorerT)` (Dimensions.hpp:14-17) — the packed
    /// `(width << 16) | height` uint32.
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_u32(out, ((self.width as u32) << 16) | self.height as u32);
    }

    /// `parse(Dimensions &, ParserT)` (Dimensions.hpp:19-25).
    pub fn parse_from(parser: &mut Parser<'_>) -> Result<Dimensions, &'static str> {
        let width_height = parser.u32_()?;
        Ok(Dimensions {
            width: (width_height >> 16) as u16,
            height: (width_height & 0xFFFF) as u16,
        })
    }
}

impl PhotoSize {
    /// `store(const PhotoSize &, StorerT)` (PhotoSize.hpp:17-25). The
    /// `file_id` rides the FileManager codec (`store_file`), the
    /// `DialogPhotoStoreEnv` framing the FileId-consuming records use.
    pub fn store(&self, env: &mut impl DialogPhotoStoreEnv, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.size_type);
        self.dimensions.store(out);
        store_parse::store_i32(out, self.size);
        env.store_file_id(self.file_id, out);
        store_parse::store_i32_vec(out, &self.progressive_sizes);
    }

    /// `parse(PhotoSize &, ParserT)` (PhotoSize.hpp:27-40) — the
    /// progressive-sizes tail is version-gated
    /// (`Version::AddPhotoProgressiveSizes`).
    pub fn parse_from(
        env: &mut impl DialogPhotoStoreEnv,
        parser: &mut Parser<'_>,
    ) -> Result<PhotoSize, &'static str> {
        let mut result = PhotoSize {
            size_type: parser.i32_()?,
            ..PhotoSize::default()
        };
        if !(0..128).contains(&result.size_type) {
            return Err("Wrong photo size type");
        }
        result.dimensions = Dimensions::parse_from(parser)?;
        result.size = parser.i32_()?;
        result.file_id = env.parse_file_id(parser)?;
        if parser.version() >= crate::store_parse::version::ADD_PHOTO_PROGRESSIVE_SIZES {
            result.progressive_sizes = parser.i32_vec()?;
        } else {
            result.progressive_sizes.clear();
        }
        Ok(result)
    }
}

impl AnimationSize {
    /// `store(const AnimationSize &, StorerT)` (PhotoSize.hpp:42-46).
    pub fn store(&self, env: &mut impl DialogPhotoStoreEnv, out: &mut Vec<u8>) {
        self.photo_size.store(env, out);
        store_parse::store_f64(out, self.main_frame_timestamp);
    }

    /// `parse(AnimationSize &, ParserT)` (PhotoSize.hpp:48-58) — the
    /// frame timestamp is version-gated
    /// (`Version::AddDialogPhotoHasAnimation`).
    pub fn parse_from(
        env: &mut impl DialogPhotoStoreEnv,
        parser: &mut Parser<'_>,
    ) -> Result<AnimationSize, &'static str> {
        let mut result = AnimationSize {
            photo_size: PhotoSize::parse_from(env, parser)?,
            ..AnimationSize::default()
        };
        if parser.version() >= crate::store_parse::version::ADD_DIALOG_PHOTO_HAS_ANIMATION {
            result.main_frame_timestamp = parser.f64_()?;
        } else {
            result.main_frame_timestamp = 0.0;
        }
        Ok(result)
    }
}

/// `StickerPhotoSize::Type` (StickerPhotoSize.h).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StickerPhotoSizeType {
    Sticker,
    CustomEmoji,
}

/// `td::StickerPhotoSize` (StickerPhotoSize.h:20). The fields are
/// crate-visible for the member-style `parse` (the C++ parse is a
/// member template doing the same).
#[derive(Clone, Debug, PartialEq)]
pub struct StickerPhotoSize {
    pub(crate) type_: StickerPhotoSizeType,
    pub(crate) custom_emoji_id: i64,
    pub(crate) sticker_set_id: i64,
    pub(crate) sticker_id: i64,
    pub(crate) background_colors: Vec<i32>,
}

impl StickerPhotoSize {
    /// `StickerPhotoSize::get_sticker_photo_size(VideoSize &&)`
    /// (StickerPhotoSize.cpp:95-128). The `videoSizeStickerMarkup`
    /// branch's `stickers_manager_->add_sticker_set` is a documented
    /// stand-in until the StickersManager step: only `inputStickerSetID`
    /// yields an id, every other InputStickerSet form is "invalid" and
    /// drops the size like the C++ validity check does.
    /// `StickerPhotoSize::get_type` (StickerPhotoSize.h).
    pub fn get_type(&self) -> StickerPhotoSizeType {
        self.type_
    }

    pub fn get_custom_emoji_id(&self) -> i64 {
        self.custom_emoji_id
    }

    pub fn get_sticker_set_id(&self) -> i64 {
        self.sticker_set_id
    }

    pub fn get_sticker_id(&self) -> i64 {
        self.sticker_id
    }

    pub fn get_background_colors(&self) -> &[i32] {
        &self.background_colors
    }
}

impl Default for StickerPhotoSize {
    fn default() -> Self {
        StickerPhotoSize {
            type_: StickerPhotoSizeType::CustomEmoji,
            custom_emoji_id: 0,
            sticker_set_id: 0,
            sticker_id: 0,
            background_colors: Vec::new(),
        }
    }
}

impl StickerPhotoSize {
    pub fn get_sticker_photo_size(size_ptr: telegram_api::VideoSize) -> Option<StickerPhotoSize> {
        let mut result = StickerPhotoSize {
            type_: StickerPhotoSizeType::CustomEmoji,
            custom_emoji_id: 0,
            sticker_set_id: 0,
            sticker_id: 0,
            background_colors: Vec::new(),
        };
        let is_valid = match size_ptr {
            telegram_api::VideoSize::videoSizeEmojiMarkup(size) => {
                result.type_ = StickerPhotoSizeType::CustomEmoji;
                result.custom_emoji_id = size.emoji_id;
                result.background_colors = size.background_colors;
                result.custom_emoji_id != 0
            }
            telegram_api::VideoSize::videoSizeStickerMarkup(size) => {
                result.type_ = StickerPhotoSizeType::Sticker;
                result.sticker_set_id = match &size.stickerset {
                    Some(telegram_api::InputStickerSet::inputStickerSetID(input)) => input.id,
                    // add_sticker_set — with the StickersManager step
                    _ => 0,
                };
                result.sticker_id = size.sticker_id;
                result.background_colors = size.background_colors;
                result.sticker_set_id != 0 && result.sticker_id != 0
            }
            telegram_api::VideoSize::videoSize(_) => return None,
        };
        if !is_valid || result.background_colors.is_empty() || result.background_colors.len() > 4 {
            tdutils::log_error!("Receive invalid sticker photo size");
            return None;
        }
        for color in &mut result.background_colors {
            *color &= 0xFFFFFF;
        }
        Some(result)
    }

    /// `StickerPhotoSize::store` (StickerPhotoSize.hpp:16-30).
    pub fn store(&self, out: &mut Vec<u8>) {
        let is_custom_emoji = self.type_ == StickerPhotoSizeType::CustomEmoji;
        let is_sticker = self.type_ == StickerPhotoSizeType::Sticker;
        crate::store_parse::store_flags(out, [is_custom_emoji, is_sticker]);
        if is_custom_emoji {
            crate::store_parse::store_i64(out, self.custom_emoji_id);
        } else if is_sticker {
            crate::store_parse::store_i64(out, self.sticker_set_id);
            crate::store_parse::store_i64(out, self.sticker_id);
        }
        crate::store_parse::store_i32_vec(out, &self.background_colors);
    }

    /// `StickerPhotoSize::parse` (StickerPhotoSize.hpp:33-51). The C++
    /// `UNREACHABLE()` on an all-zero flag block becomes an error (the
    /// parser state is corrupted; the C++ would abort).
    pub fn parse_from(
        parser: &mut crate::store_parse::Parser,
    ) -> Result<StickerPhotoSize, &'static str> {
        let flags = parser.parse_flags::<2>()?;
        let (type_, custom_emoji_id, sticker_set_id, sticker_id) = if flags[0] {
            let id = parser.i64_()?;
            (StickerPhotoSizeType::CustomEmoji, id, 0, 0)
        } else if flags[1] {
            let set_id = parser.i64_()?;
            let sticker_id = parser.i64_()?;
            (StickerPhotoSizeType::Sticker, 0, set_id, sticker_id)
        } else {
            return Err("StickerPhotoSize: unreachable type");
        };
        let background_colors = parser.i32_vec()?;
        Ok(StickerPhotoSize {
            type_,
            custom_emoji_id,
            sticker_set_id,
            sticker_id,
            background_colors,
        })
    }

    /// `StickerPhotoSize::get_chat_photo_sticker_object`
    /// (StickerPhotoSize.cpp:131-160).
    pub fn get_chat_photo_sticker_object(&self) -> td_api::ChatPhotoSticker {
        let r#type = match self.type_ {
            StickerPhotoSizeType::Sticker => {
                td_api::ChatPhotoStickerType::chatPhotoStickerTypeRegularOrMask(
                    td_api::chatPhotoStickerTypeRegularOrMask {
                        sticker_set_id: self.sticker_set_id,
                        sticker_id: self.sticker_id,
                    },
                )
            }
            StickerPhotoSizeType::CustomEmoji => {
                td_api::ChatPhotoStickerType::chatPhotoStickerTypeCustomEmoji(
                    td_api::chatPhotoStickerTypeCustomEmoji {
                        custom_emoji_id: self.custom_emoji_id,
                    },
                )
            }
        };
        let background_fill = match self.background_colors.len() {
            1 => td_api::BackgroundFill::backgroundFillSolid(td_api::backgroundFillSolid {
                color: self.background_colors[0],
            }),
            2 => td_api::BackgroundFill::backgroundFillGradient(td_api::backgroundFillGradient {
                top_color: self.background_colors[0],
                bottom_color: self.background_colors[1],
                rotation_angle: 0,
            }),
            3 | 4 => td_api::BackgroundFill::backgroundFillFreeformGradient(
                td_api::backgroundFillFreeformGradient {
                    colors: self.background_colors.clone(),
                },
            ),
            _ => {
                tdutils::log_error!("Invalid number of background colors specified");
                td_api::BackgroundFill::backgroundFillSolid(td_api::backgroundFillSolid {
                    color: 0,
                })
            }
        };
        td_api::ChatPhotoSticker {
            r#type: Some(r#type),
            background_fill: Some(background_fill),
        }
    }
}

/// `td::Photo` (Photo.h:32). The empty id sentinel is the
/// `MovableValue<int64, -2>` default; `sticker_file_ids` (Photo.h:39)
/// joins the store/parse as `FileId` rows.
#[derive(Clone, Debug, PartialEq)]
pub struct Photo {
    pub id: i64,
    pub date: i32,
    pub minithumbnail: String,
    pub photos: Vec<PhotoSize>,
    pub animations: Vec<AnimationSize>,
    pub sticker_photo_size: Option<StickerPhotoSize>,
    pub has_stickers: bool,
    /// `Photo::sticker_file_ids` (Photo.h:39).
    pub sticker_file_ids: Vec<FileId>,
}

impl Default for Photo {
    fn default() -> Self {
        Photo {
            id: -2,
            date: 0,
            minithumbnail: String::new(),
            photos: Vec::new(),
            animations: Vec::new(),
            sticker_photo_size: None,
            has_stickers: false,
            sticker_file_ids: Vec::new(),
        }
    }
}

impl Photo {
    /// `Photo::is_empty` (Photo.h:43).
    pub fn is_empty(&self) -> bool {
        self.id == -2
    }
}

/// `photo_get_file_ids` (Photo.cpp:435-448) — the photo sizes first (the
/// order `register_suggested_profile_photo` relies on), then the
/// animations.
pub fn photo_get_file_ids(photo: &Photo) -> Vec<FileId> {
    let mut file_ids = Vec::new();
    photo_append_file_ids(photo, &mut file_ids);
    file_ids
}

/// `get_photo_any_file_id` (Photo.cpp:451-457) — the file id of the last
/// size, i.e. the best full representation.
pub fn get_photo_any_file_id(photo: &Photo) -> FileId {
    if let Some(last) = photo.photos.last() {
        return last.file_id;
    }
    FileId::default()
}

/// `photo_append_file_ids` (Photo.cpp:440).
pub fn photo_append_file_ids(photo: &Photo, file_ids: &mut Vec<FileId>) {
    for photo_size in &photo.photos {
        file_ids.push(photo_size.file_id);
    }
    for animation_size in &photo.animations {
        file_ids.push(animation_size.photo_size.file_id);
    }
}

/// The `Variant<PhotoSize, string>` result of `get_photo_size`
/// (PhotoSize.cpp:38): a classified size, or the packed minithumbnail
/// string of a `photoStrippedSize`.
enum PhotoSizeOrMinithumbnail {
    Size(PhotoSize),
    Minithumbnail(String),
}

/// `get_photo_size` (PhotoSize.cpp:38-189) for the profile-photo format
/// (`PhotoFormat::Jpeg`): the size classification and type-string
/// validation without the file registration. `photoPathSize` is an SVG
/// minithumbnail, unexpected for JPEG — the C++ logs and returns the
/// empty size, which the caller's unallowed-type skip then drops.
fn get_photo_size(size_ptr: telegram_api::PhotoSize) -> PhotoSizeOrMinithumbnail {
    let mut res = PhotoSize::default();
    let type_str: String;
    match size_ptr {
        telegram_api::PhotoSize::photoSizeEmpty(_) => return PhotoSizeOrMinithumbnail::Size(res),
        telegram_api::PhotoSize::photoSize(size) => {
            type_str = size.r#type;
            res.dimensions = get_dimensions(size.w, size.h, "photoSize");
            res.size = size.size;
        }
        telegram_api::PhotoSize::photoCachedSize(size) => {
            type_str = size.r#type;
            res.dimensions = get_dimensions(size.w, size.h, "photoCachedSize");
            res.size = size.bytes.len() as i32;
        }
        telegram_api::PhotoSize::photoStrippedSize(size) => {
            // The packed bytes are binary; the module-wide minithumbnail
            // representation is the lossy String (module docs)
            return PhotoSizeOrMinithumbnail::Minithumbnail(
                String::from_utf8_lossy(&size.bytes).into_owned(),
            );
        }
        telegram_api::PhotoSize::photoSizeProgressive(mut size) => {
            if size.sizes.is_empty() {
                tdutils::log_error!("Receive photo with empty progressive size");
                return PhotoSizeOrMinithumbnail::Size(res);
            }
            size.sizes.sort_unstable();
            type_str = size.r#type;
            res.dimensions = get_dimensions(size.w, size.h, "photoSizeProgressive");
            res.size = *size.sizes.last().unwrap();
            size.sizes.pop();
            res.progressive_sizes = size.sizes;
        }
        telegram_api::PhotoSize::photoPathSize(_) => {
            // format != Tgs/Webp/Webm — "Receive unexpected SVG
            // minithumbnail" (PhotoSize.cpp:88-93), then the empty size
            tdutils::log_error!("Receive unexpected SVG minithumbnail in photo");
            return PhotoSizeOrMinithumbnail::Size(res);
        }
    }

    if type_str.len() != 1 {
        tdutils::log_error!("Wrong photoSize \"{type_str}\"");
        res.size_type = 0;
    } else {
        let mut int_type = type_str.as_bytes()[0] as i32;
        if int_type >= 128 {
            tdutils::log_error!("Wrong photoSize \"{int_type}\"");
            int_type = 0;
        }
        res.size_type = int_type;
    }
    PhotoSizeOrMinithumbnail::Size(res)
}

/// `get_animation_size` (PhotoSize.cpp:165-196) for the profile-photo
/// source, without the file registration: the type-string validation
/// (only one byte, < 128) and the size clamp.
fn get_animation_size(size: telegram_api::videoSize) -> AnimationSize {
    let mut result = AnimationSize {
        photo_size: PhotoSize::default(),
        main_frame_timestamp: size.video_start_ts,
    };
    if size.r#type != "p" && size.r#type != "u" && size.r#type != "v" {
        tdutils::log_error!("Unsupported videoSize \"{}\"", size.r#type);
    }
    // An empty type string reads the terminating zero, exactly like the
    // C++ `size->type_[0]` on an empty std::string.
    let mut type_ = size.r#type.as_bytes().first().copied().unwrap_or(0) as i32;
    if type_ >= 128 {
        tdutils::log_error!("Wrong videoSize \"{type_}\"");
        type_ = 0;
    }
    result.photo_size.size_type = type_;
    result.photo_size.dimensions = get_dimensions(size.w, size.h, "get_animation_size");
    result.photo_size.size = size.size;
    if result.photo_size.size < 0 || result.photo_size.size > 1_000_000_000 {
        tdutils::log_error!("Receive animation of size {}", result.photo_size.size);
        result.photo_size.size = 0;
    }
    result
}

/// `process_video_size` (PhotoSize.cpp:198-219): a `videoSize` becomes an
/// animation (dropped when the type is empty), the two markup variants a
/// sticker photo size.
fn process_video_size(size_ptr: telegram_api::VideoSize) -> Option<AnimationOrSticker> {
    match size_ptr {
        telegram_api::VideoSize::videoSize(size) => {
            let animation_size = get_animation_size(size);
            if animation_size.photo_size.size_type == 0 {
                return None;
            }
            Some(AnimationOrSticker::Animation(animation_size))
        }
        size_ptr @ (telegram_api::VideoSize::videoSizeEmojiMarkup(_)
        | telegram_api::VideoSize::videoSizeStickerMarkup(_)) => {
            StickerPhotoSize::get_sticker_photo_size(size_ptr).map(AnimationOrSticker::Sticker)
        }
    }
}

enum AnimationOrSticker {
    Animation(AnimationSize),
    Sticker(StickerPhotoSize),
}

/// `get_photo` (Photo.cpp:57-115) for the `telegram_api::Photo` sum type
/// with the profile-photo `FileType::Photo` format. The empty id is
/// remapped to -3 so `is_empty` no longer matches, like the C++.
pub fn get_photo(photo: telegram_api::Photo) -> Photo {
    let telegram_api::Photo::photo(photo) = photo else {
        return Photo::default();
    };
    let mut res = Photo {
        id: photo.id,
        date: photo.date,
        has_stickers: photo.has_stickers,
        ..Photo::default()
    };
    if res.is_empty() {
        tdutils::log_error!("Receive photo with identifier {}", res.id);
        res.id = -3;
    }

    for size_ptr in photo.sizes {
        match get_photo_size(size_ptr) {
            PhotoSizeOrMinithumbnail::Size(size) => {
                // cpp:85-91 — types 0, 't', 'i', 'p', 'u', 'v' are not
                // allowed for photo sizes
                if matches!(
                    size.size_type as u8 as char,
                    '\0' | 't' | 'i' | 'p' | 'u' | 'v'
                ) {
                    tdutils::log_error!("Skip unallowed photo size of type {}", size.size_type);
                    continue;
                }
                res.photos.push(size);
            }
            PhotoSizeOrMinithumbnail::Minithumbnail(bytes) => {
                // The packed stripped JPEG is binary; the module-wide
                // minithumbnail representation is the lossy String of
                // `ProfilePhoto` (user_manager module docs) until the
                // Files step makes the whole byte path honest.
                res.minithumbnail = bytes;
            }
        }
    }

    for size_ptr in photo.video_sizes {
        let Some(animation) = process_video_size(size_ptr) else {
            continue;
        };
        match animation {
            AnimationOrSticker::Animation(animation_size) => {
                if animation_size.photo_size.size_type != 0
                    && animation_size.photo_size.dimensions.width
                        == animation_size.photo_size.dimensions.height
                {
                    res.animations.push(animation_size);
                }
            }
            AnimationOrSticker::Sticker(sticker_photo_size) => {
                res.sticker_photo_size = Some(sticker_photo_size);
            }
        }
    }

    res
}

/// `get_minithumbnail_object` (PhotoSize.cpp:49-79): the packed stripped
/// JPEG expands to the full header + payload + footer. A value the C++
/// answers `nullptr` for becomes the codegen-forced empty payload
/// (module docs).
pub fn get_minithumbnail_object(packed: &[u8]) -> td_api::Minithumbnail {
    if packed.len() < 3 {
        return td_api::Minithumbnail {
            width: 0,
            height: 0,
            data: Vec::new(),
        };
    }
    if packed[0] != 0x01 {
        return td_api::Minithumbnail {
            width: 0,
            height: 0,
            data: Vec::new(),
        };
    }
    const HEADER_B64: &str = concat!(
        "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDACgcHiMeGSgjISMtKygwPGRBPDc3PHtYXUlkkYCZlo+AjIqgtObDoKrarYqMyP/L2u71////",
        "m8H///",
        "/6/+b9//j/2wBDASstLTw1PHZBQXb4pYyl+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj/",
        "wAARCAAAAAADASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/",
        "8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0R",
        "FRkd",
        "ISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2",
        "uHi4",
        "+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/",
        "8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkN",
        "ERUZ",
        "HSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2",
        "Nna4",
        "uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwA="
    );
    const FOOTER_B64: &str = "/9k=";
    let header = base64::engine::general_purpose::STANDARD
        .decode(HEADER_B64)
        .unwrap();
    let footer = base64::engine::general_purpose::STANDARD
        .decode(FOOTER_B64)
        .unwrap();
    let mut data = Vec::new();
    data.extend_from_slice(&header[..164]);
    data.push(packed[1]);
    data.push(header[165]);
    data.push(packed[2]);
    data.extend_from_slice(&header[167..]);
    data.extend_from_slice(&packed[3..]);
    data.extend_from_slice(&footer);
    td_api::Minithumbnail {
        height: packed[1] as i32,
        width: packed[2] as i32,
        data,
    }
}

/// `get_photo_size_object` (PhotoSize.cpp:506-514): the type character
/// (empty for type 0) over the codegen-forced empty file payload.
fn get_photo_size_object(photo_size: &PhotoSize) -> td_api::PhotoSize {
    td_api::PhotoSize {
        r#type: if photo_size.size_type != 0 {
            (photo_size.size_type as u8 as char).to_string()
        } else {
            String::new()
        },
        photo: Some(td_api::File::default()),
        width: photo_size.dimensions.width as i32,
        height: photo_size.dimensions.height as i32,
        progressive_sizes: photo_size.progressive_sizes.clone(),
    }
}

/// `get_photo_sizes_object` (PhotoSize.cpp:516-532). Divergence until the
/// Files step: the C++ stable-sorts by the file objects' expected sizes
/// and drops sizes whose file cannot be downloaded — both read
/// FileManager state the placeholders cannot carry, so the sizes keep
/// their classified order.
pub fn get_photo_sizes_object(photo_sizes: &[PhotoSize]) -> Vec<td_api::PhotoSize> {
    photo_sizes.iter().map(get_photo_size_object).collect()
}

/// `get_animated_chat_photo_object` (DialogPhoto.cpp:224-232). The C++
/// `nullptr` (no animation, or one without a valid file id) becomes the
/// codegen-forced empty payload.
pub fn get_animated_chat_photo_object(
    animation_size: Option<&AnimationSize>,
) -> td_api::AnimatedChatPhoto {
    match animation_size {
        Some(animation) if animation.photo_size.file_id.is_valid() => td_api::AnimatedChatPhoto {
            length: animation.photo_size.dimensions.width as i32,
            file: Some(td_api::File::default()),
            main_frame_timestamp: animation.main_frame_timestamp,
        },
        _ => td_api::AnimatedChatPhoto {
            length: 0,
            file: Some(td_api::File::default()),
            main_frame_timestamp: 0.0,
        },
    }
}

/// `get_chat_photo_object` (DialogPhoto.cpp:235-259). An empty photo —
/// impossible from the listing cache, which drops empties at intake —
/// answers the codegen-forced empty payload instead of the C++ `nullptr`.
pub fn get_chat_photo_object(photo: &Photo) -> td_api::ChatPhoto {
    if photo.is_empty() {
        return td_api::ChatPhoto {
            id: 0,
            added_date: 0,
            minithumbnail: Some(get_minithumbnail_object(&[])),
            sizes: Vec::new(),
            animation: Some(get_animated_chat_photo_object(None)),
            small_animation: Some(get_animated_chat_photo_object(None)),
            sticker: Some(td_api::ChatPhotoSticker {
                r#type: Some(
                    td_api::ChatPhotoStickerType::chatPhotoStickerTypeCustomEmoji(
                        td_api::chatPhotoStickerTypeCustomEmoji { custom_emoji_id: 0 },
                    ),
                ),
                background_fill: Some(td_api::BackgroundFill::backgroundFillSolid(
                    td_api::backgroundFillSolid { color: 0 },
                )),
            }),
        };
    }

    let mut small_animation: Option<&AnimationSize> = None;
    let mut big_animation: Option<&AnimationSize> = None;
    for animation in &photo.animations {
        if animation.photo_size.size_type == b'p' as i32 {
            small_animation = Some(animation);
        } else if animation.photo_size.size_type == b'u' as i32 {
            big_animation = Some(animation);
        }
    }
    if big_animation.is_none() && small_animation.is_some() {
        tdutils::log_error!(
            "Have small animation without big animation in photo {}",
            photo.id
        );
        small_animation = None;
    }
    let sticker = photo
        .sticker_photo_size
        .as_ref()
        .map(|sticker| sticker.get_chat_photo_sticker_object())
        .unwrap_or(td_api::ChatPhotoSticker {
            r#type: Some(
                td_api::ChatPhotoStickerType::chatPhotoStickerTypeCustomEmoji(
                    td_api::chatPhotoStickerTypeCustomEmoji { custom_emoji_id: 0 },
                ),
            ),
            background_fill: Some(td_api::BackgroundFill::backgroundFillSolid(
                td_api::backgroundFillSolid { color: 0 },
            )),
        });
    td_api::ChatPhoto {
        id: photo.id,
        added_date: photo.date,
        minithumbnail: Some(get_minithumbnail_object(photo.minithumbnail.as_bytes())),
        sizes: get_photo_sizes_object(&photo.photos),
        animation: Some(get_animated_chat_photo_object(big_animation)),
        small_animation: Some(get_animated_chat_photo_object(small_animation)),
        sticker: Some(sticker),
    }
}

/// `td::DialogPhoto` (DialogPhoto.h:23) — the chat-photo handle the basic
/// group/channel records carry (`ProfilePhoto` is this plus the photo id).
/// The two `FileId`s stay placeholders until the Files step, with the same
/// consequences the module docs describe for the photo sizes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DialogPhoto {
    pub small_file_id: FileId,
    pub big_file_id: FileId,
    pub minithumbnail: String,
    pub has_animation: bool,
    pub is_personal: bool,
}

impl DialogPhoto {
    /// `get_dialog_photo` (DialogPhoto.cpp:90-130): a null/`chatPhotoEmpty`
    /// yields the empty photo; a `chatPhoto` contributes the stripped
    /// minithumbnail, the animation flag and the dc id (kept for the
    /// invalid-value parity of `DcId::create`). The two
    /// `register_photo_size` calls are the documented Files-step no-op —
    /// both file ids stay invalid placeholders.
    pub fn get_dialog_photo(chat_photo: Option<telegram_api::ChatPhoto>) -> DialogPhoto {
        let mut result = DialogPhoto::default();
        match chat_photo {
            None => {}
            Some(telegram_api::ChatPhoto::chatPhotoEmpty(_)) => {}
            Some(telegram_api::ChatPhoto::chatPhoto(chat_photo)) => {
                let _dc_id = crate::chat_manager::DcId::create(chat_photo.dc_id);
                result.has_animation = chat_photo.has_video;
                result.is_personal = false;
                result.minithumbnail =
                    String::from_utf8_lossy(&chat_photo.stripped_thumb).into_owned();
            }
        }
        result
    }

    /// `need_update_dialog_photo` (DialogPhoto.cpp:212) — the minithumbnail
    /// is deliberately not compared, like the C++.
    pub fn need_update(&self, to: &DialogPhoto) -> bool {
        self.small_file_id != to.small_file_id
            || self.big_file_id != to.big_file_id
            || self.has_animation != to.has_animation
            || self.is_personal != to.is_personal
    }

    /// `as_fake_dialog_photo` (DialogPhoto.cpp:144-160): the 'a'/'c' size
    /// selection, the minithumbnail and the animation flag. The C++
    /// "Failed to convert" error fires when either selected size carries no
    /// valid file id — a condition the placeholders cannot express until the
    /// Files step, so the selected fields are kept instead (module docs).
    pub fn as_fake_dialog_photo(
        photo: &Photo,
        _dialog_id: crate::chat_manager::DialogId,
        is_personal: bool,
    ) -> DialogPhoto {
        let mut result = DialogPhoto::default();
        if !photo.is_empty() {
            for size in &photo.photos {
                if size.size_type == b'a' as i32 {
                    result.small_file_id = size.file_id;
                } else if size.size_type == b'c' as i32 {
                    result.big_file_id = size.file_id;
                }
            }
            result.minithumbnail = photo.minithumbnail.clone();
            result.has_animation = !photo.animations.is_empty();
            result.is_personal = is_personal;
        }
        result
    }
}

/// `dialog_photo_get_file_ids` (DialogPhoto.cpp:133-139) — the valid file
/// ids, small then big; placeholders never qualify.
pub fn dialog_photo_get_file_ids(dialog_photo: &DialogPhoto) -> Vec<FileId> {
    let mut result = Vec::new();
    if dialog_photo.small_file_id.is_valid() {
        result.push(dialog_photo.small_file_id);
    }
    if dialog_photo.big_file_id.is_valid() {
        result.push(dialog_photo.big_file_id);
    }
    result
}

// ---------------------------------------------------------------------------
// The Photo byte plane (Photo.hpp, PhotoSize.hpp, StickerPhotoSize.hpp,
// Dimensions.hpp, PhotoSizeType.hpp) — the pmc/log-event store the WebApp
// and Game value classes persist through. The `FileId` rows ride
// [`PhotoStoreEnv`] (the C++ `storer.context()->td()->file_manager_`
// accessor); the AnimationsManager leg is the caller's seam.
// ---------------------------------------------------------------------------

/// The file-manager slice of the Photo byte plane (`store_file`/`parse_file`
/// in FileId.hpp).
pub trait PhotoStoreEnv {
    /// `td_->file_manager_->store_file(file_id, storer)` (FileId.hpp:16-18).
    fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>);

    /// `file_id = parser.context()->td()->file_manager_->parse_file(parser)`
    /// (FileId.hpp:21-23).
    fn parse_file_id(&mut self, parser: &mut Parser) -> Result<FileId, &'static str>;
}

/// `store(Dimensions)` (Dimensions.hpp:13-16) — the packed
/// `(width << 16) | height` uint32.
pub fn store_dimensions(out: &mut Vec<u8>, dimensions: Dimensions) {
    store_parse::store_u32(
        out,
        ((dimensions.width as u32) << 16) | dimensions.height as u32,
    );
}

/// `parse(Dimensions)` (Dimensions.hpp:18-23).
pub fn parse_dimensions(parser: &mut Parser) -> Result<Dimensions, &'static str> {
    let width_height = parser.u32_()?;
    Ok(Dimensions {
        width: (width_height >> 16) as u16,
        height: (width_height & 0xFFFF) as u16,
    })
}

/// `store(const PhotoSizeType &)` (PhotoSizeType.hpp:11-14) — the raw
/// `int32` type character.
pub fn store_photo_size_type(out: &mut Vec<u8>, size_type: PhotoSizeType) {
    store_parse::store_i32(out, size_type);
}

/// `parse(PhotoSizeType &)` (PhotoSizeType.hpp:16-22) — the C++
/// `parser.set_error` becomes the Err arm.
pub fn parse_photo_size_type(parser: &mut Parser) -> Result<PhotoSizeType, &'static str> {
    let size_type = parser.i32_()?;
    if !(0..128).contains(&size_type) {
        return Result::Err("Wrong photo size type");
    }
    Ok(size_type)
}

/// `store(const PhotoSize &)` (PhotoSize.hpp:15-22).
pub fn store_photo_size(env: &mut impl PhotoStoreEnv, out: &mut Vec<u8>, photo_size: &PhotoSize) {
    store_photo_size_type(out, photo_size.size_type);
    store_dimensions(out, photo_size.dimensions);
    store_parse::store_i32(out, photo_size.size);
    env.store_file_id(photo_size.file_id, out);
    store_parse::store_i32_vec(out, &photo_size.progressive_sizes);
}

/// `parse(PhotoSize &)` (PhotoSize.hpp:24-34) — the progressive-sizes
/// vector exists since `Version::AddPhotoProgressiveSizes`.
pub fn parse_photo_size(
    env: &mut impl PhotoStoreEnv,
    parser: &mut Parser,
) -> Result<PhotoSize, &'static str> {
    let size_type = parse_photo_size_type(parser)?;
    let dimensions = parse_dimensions(parser)?;
    let size = parser.i32_()?;
    let file_id = env.parse_file_id(parser)?;
    let progressive_sizes = if parser.version() >= store_parse::version::ADD_PHOTO_PROGRESSIVE_SIZES
    {
        parser.i32_vec()?
    } else {
        Vec::new()
    };
    Ok(PhotoSize {
        size_type,
        size,
        dimensions,
        file_id,
        progressive_sizes,
    })
}

/// `store(const AnimationSize &)` (PhotoSize.hpp:36-40) — the PhotoSize
/// base then the stream start.
pub fn store_animation_size(
    env: &mut impl PhotoStoreEnv,
    out: &mut Vec<u8>,
    animation_size: &AnimationSize,
) {
    store_photo_size(env, out, &animation_size.photo_size);
    store_parse::store_f64(out, animation_size.main_frame_timestamp);
}

/// `parse(AnimationSize &)` (PhotoSize.hpp:42-52) — the timestamp exists
/// since `Version::AddDialogPhotoHasAnimation`.
pub fn parse_animation_size(
    env: &mut impl PhotoStoreEnv,
    parser: &mut Parser,
) -> Result<AnimationSize, &'static str> {
    let photo_size = parse_photo_size(env, parser)?;
    let main_frame_timestamp =
        if parser.version() >= store_parse::version::ADD_DIALOG_PHOTO_HAS_ANIMATION {
            parser.f64_()?
        } else {
            0.0
        };
    Ok(AnimationSize {
        photo_size,
        main_frame_timestamp,
    })
}

/// `StickerPhotoSize::store` (StickerPhotoSize.hpp:13-32).
pub fn store_sticker_photo_size(out: &mut Vec<u8>, sticker_photo_size: &StickerPhotoSize) {
    let is_custom_emoji = sticker_photo_size.type_ == StickerPhotoSizeType::CustomEmoji;
    let is_sticker = sticker_photo_size.type_ == StickerPhotoSizeType::Sticker;
    store_parse::store_flags(out, [is_custom_emoji, is_sticker]);
    if is_custom_emoji {
        store_parse::store_i64(out, sticker_photo_size.custom_emoji_id);
    } else if is_sticker {
        store_parse::store_i64(out, sticker_photo_size.sticker_set_id);
        store_parse::store_i64(out, sticker_photo_size.sticker_id);
    }
    store_parse::store_i32_vec(out, &sticker_photo_size.background_colors);
}

/// `StickerPhotoSize::parse` (StickerPhotoSize.hpp:34-54) — the C++
/// `UNREACHABLE` on a flag-less row becomes the parse error.
pub fn parse_sticker_photo_size(parser: &mut Parser) -> Result<StickerPhotoSize, &'static str> {
    let [is_custom_emoji, is_sticker] = parser.parse_flags()?;
    let type_;
    let custom_emoji_id;
    let sticker_set_id;
    let sticker_id;
    if is_custom_emoji {
        type_ = StickerPhotoSizeType::CustomEmoji;
        custom_emoji_id = parser.i64_()?;
        sticker_set_id = 0;
        sticker_id = 0;
    } else if is_sticker {
        type_ = StickerPhotoSizeType::Sticker;
        custom_emoji_id = 0;
        sticker_set_id = parser.i64_()?;
        sticker_id = parser.i64_()?;
    } else {
        return Result::Err("Wrong StickerPhotoSize flags");
    }
    let background_colors = parser.i32_vec()?;
    Ok(StickerPhotoSize {
        type_,
        custom_emoji_id,
        sticker_set_id,
        sticker_id,
        background_colors,
    })
}

/// `store(const Photo &)` (Photo.hpp:19-44).
pub fn store_photo(env: &mut impl PhotoStoreEnv, out: &mut Vec<u8>, photo: &Photo) {
    let has_minithumbnail = !photo.minithumbnail.is_empty();
    let has_animations = !photo.animations.is_empty();
    let has_sticker_photo_size = photo.sticker_photo_size.is_some();
    store_parse::store_flags(
        out,
        [
            photo.has_stickers,
            has_minithumbnail,
            has_animations,
            has_sticker_photo_size,
        ],
    );
    store_parse::store_i64(out, photo.id);
    store_parse::store_i32(out, photo.date);
    // `store(photo.photos, storer)` — the vector length prefix
    store_parse::store_i32(out, photo.photos.len() as i32);
    for photo_size in &photo.photos {
        store_photo_size(env, out, photo_size);
    }
    if photo.has_stickers {
        // `store(photo.sticker_file_ids, storer)` (Photo.h:43) — the
        // FileId rows behind the length prefix.
        store_parse::store_i32(out, photo.sticker_file_ids.len() as i32);
        for file_id in &photo.sticker_file_ids {
            env.store_file_id(*file_id, out);
        }
    }
    if has_minithumbnail {
        store_parse::store_string(out, &photo.minithumbnail);
    }
    if has_animations {
        // the vector length prefix
        store_parse::store_i32(out, photo.animations.len() as i32);
        for animation in &photo.animations {
            store_animation_size(env, out, animation);
        }
    }
    if let Some(sticker_photo_size) = &photo.sticker_photo_size {
        store_sticker_photo_size(out, sticker_photo_size);
    }
}

/// `parse(Photo &)` (Photo.hpp:46-74).
pub fn parse_photo(
    env: &mut impl PhotoStoreEnv,
    parser: &mut Parser,
) -> Result<Photo, &'static str> {
    let [
        has_stickers,
        has_minithumbnail,
        has_animations,
        has_sticker_photo_size,
    ] = parser.parse_flags()?;
    let id = parser.i64_()?;
    let date = parser.i32_()?;
    let photo_count = parser.i32_()?;
    let mut photos = Vec::with_capacity(photo_count.unsigned_abs() as usize);
    for _ in 0..photo_count {
        photos.push(parse_photo_size(env, parser)?);
    }
    let mut sticker_file_ids = Vec::new();
    if has_stickers {
        let sticker_file_ids_len = parser.i32_()?;
        if sticker_file_ids_len < 0 || parser.left_len() < sticker_file_ids_len as usize {
            return Result::Err("Wrong sticker_file_ids length");
        }
        for _ in 0..sticker_file_ids_len {
            sticker_file_ids.push(env.parse_file_id(parser)?);
        }
    }
    let minithumbnail = if has_minithumbnail {
        parser.string()?
    } else {
        String::new()
    };
    let mut animations = Vec::new();
    if has_animations {
        let animation_count = parser.i32_()?;
        animations.reserve(animation_count.unsigned_abs() as usize);
        for _ in 0..animation_count {
            animations.push(parse_animation_size(env, parser)?);
        }
    }
    let sticker_photo_size = if has_sticker_photo_size {
        Some(parse_sticker_photo_size(parser)?)
    } else {
        None
    };
    Ok(Photo {
        id,
        date,
        minithumbnail,
        photos,
        animations,
        sticker_photo_size,
        has_stickers,
        sticker_file_ids,
    })
}

/// `get_photo_object` (Photo.cpp:201-208) — the empty photo answers
/// `None`; the file payloads are the codegen-forced empty objects (the
/// listing-path FileManager reduction, module docs).
pub fn get_photo_object(photo: &Photo) -> Option<td_api::Photo> {
    if photo.is_empty() {
        return None;
    }
    Some(td_api::Photo {
        has_stickers: photo.has_stickers,
        minithumbnail: Some(get_minithumbnail_object(photo.minithumbnail.as_bytes())),
        sizes: get_photo_sizes_object(&photo.photos),
    })
}

/// `store(DialogPhoto)` (DialogPhoto.hpp:16-34).
pub fn store_dialog_photo(
    env: &mut impl PhotoStoreEnv,
    out: &mut Vec<u8>,
    dialog_photo: &DialogPhoto,
) {
    let has_file_ids = dialog_photo.small_file_id.is_valid() || dialog_photo.big_file_id.is_valid();
    let has_minithumbnail = !dialog_photo.minithumbnail.is_empty();
    crate::store_parse::store_flags(
        out,
        [
            has_file_ids,
            dialog_photo.has_animation,
            has_minithumbnail,
            dialog_photo.is_personal,
        ],
    );
    if has_file_ids {
        env.store_file_id(dialog_photo.small_file_id, out);
        env.store_file_id(dialog_photo.big_file_id, out);
    }
    if has_minithumbnail {
        crate::store_parse::store_string(out, &dialog_photo.minithumbnail);
    }
}

/// `parse(DialogPhoto)` (DialogPhoto.hpp:36-58) — the flags are
/// `AddDialogPhotoHasAnimation`-gated.
pub fn parse_dialog_photo(
    env: &mut impl PhotoStoreEnv,
    parser: &mut crate::store_parse::Parser<'_>,
) -> Result<DialogPhoto, &'static str> {
    let mut dialog_photo = DialogPhoto::default();
    let [has_file_ids, has_animation, has_minithumbnail, is_personal] =
        if parser.version() >= crate::store_parse::version::ADD_DIALOG_PHOTO_HAS_ANIMATION {
            parser.parse_flags()?
        } else {
            [true, false, false, false]
        };
    if has_file_ids {
        dialog_photo.small_file_id = env.parse_file_id(parser)?;
        dialog_photo.big_file_id = env.parse_file_id(parser)?;
    }
    dialog_photo.has_animation = has_animation;
    if has_minithumbnail {
        dialog_photo.minithumbnail = parser.string()?;
    }
    dialog_photo.is_personal = is_personal;
    Ok(dialog_photo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo_size(r#type: &str, w: i32, h: i32, size: i32) -> telegram_api::PhotoSize {
        telegram_api::PhotoSize::photoSize(telegram_api::photoSize {
            r#type: r#type.to_owned(),
            w,
            h,
            size,
        })
    }

    #[test]
    fn get_dimensions_clamps_and_collapses() {
        assert_eq!(get_dimensions(100, 200, "test").width, 100);
        assert_eq!(get_dimensions(100, 200, "test").height, 200);
        // a zero on either axis zeroes both
        assert_eq!(get_dimensions(0, 200, "test"), Dimensions::default());
        // out-of-range clamps to zero
        assert_eq!(get_dimensions(-1, 200, "test"), Dimensions::default());
        assert_eq!(
            get_dimensions(u16::MAX as i32 + 1, 200, "test"),
            Dimensions::default()
        );
    }

    #[test]
    fn photo_empty_and_error_ids() {
        assert!(Photo::default().is_empty());
        let photo = get_photo(telegram_api::Photo::photoEmpty(telegram_api::photoEmpty {
            id: 1,
        }));
        assert!(photo.is_empty());
        // A zero id is NOT the empty sentinel — no remap
        let photo = get_photo(telegram_api::Photo::photo(telegram_api::photo {
            flags: 0,
            has_stickers: false,
            id: 0,
            access_hash: 0,
            file_reference: Vec::new(),
            date: 123,
            sizes: Vec::new(),
            video_sizes: Vec::new(),
            dc_id: 2,
        }));
        assert!(!photo.is_empty());
        assert_eq!(photo.id, 0);
        // The -2 sentinel remaps to -3 so is_empty no longer matches
        let photo = get_photo(telegram_api::Photo::photo(telegram_api::photo {
            flags: 0,
            has_stickers: false,
            id: -2,
            access_hash: 0,
            file_reference: Vec::new(),
            date: 123,
            sizes: Vec::new(),
            video_sizes: Vec::new(),
            dc_id: 2,
        }));
        assert!(!photo.is_empty());
        assert_eq!(photo.id, -3);
    }

    #[test]
    fn get_photo_classifies_sizes_and_minithumbnail() {
        let photo = get_photo(telegram_api::Photo::photo(telegram_api::photo {
            flags: 0b11,
            has_stickers: true,
            id: 77,
            access_hash: 5,
            file_reference: Vec::new(),
            date: 1000,
            sizes: vec![
                photo_size("m", 320, 320, 15_000),
                photo_size("t", 80, 80, 1_000),
                photo_size("bad", 10, 10, 1),
                photo_size("", 10, 10, 1),
                telegram_api::PhotoSize::photoStrippedSize(telegram_api::photoStrippedSize {
                    r#type: String::new(),
                    bytes: vec![0x01, 90, 60, 0x20, 0x41],
                }),
            ],
            video_sizes: vec![telegram_api::VideoSize::videoSize(
                telegram_api::videoSize {
                    flags: 0,
                    r#type: "u".to_owned(),
                    w: 512,
                    h: 512,
                    size: 60_000,
                    video_start_ts: 1.5,
                },
            )],
            dc_id: 2,
        }));
        assert_eq!(photo.id, 77);
        assert!(photo.has_stickers);
        // 't' skipped, invalid type strings ("bad", "") reset to 0 and
        // skipped; only 'm' remains
        assert_eq!(photo.photos.len(), 1);
        assert_eq!(photo.photos[0].size_type, b'm' as i32);
        assert_eq!(
            photo.photos[0].dimensions,
            Dimensions {
                width: 320,
                height: 320
            }
        );
        // the stripped size becomes the packed minithumbnail
        assert_eq!(photo.minithumbnail.as_bytes(), &[0x01, 90, 60, 0x20, 0x41]);
        // the square 'u' animation is kept with its stream start
        assert_eq!(photo.animations.len(), 1);
        assert_eq!(photo.animations[0].photo_size.size_type, b'u' as i32);
        assert_eq!(photo.animations[0].main_frame_timestamp, 1.5);
    }

    #[test]
    fn get_photo_drops_non_square_animations_and_progressive_tail() {
        let photo = get_photo(telegram_api::Photo::photo(telegram_api::photo {
            flags: 0,
            has_stickers: false,
            id: 9,
            access_hash: 0,
            file_reference: Vec::new(),
            date: 5,
            sizes: vec![telegram_api::PhotoSize::photoSizeProgressive(
                telegram_api::photoSizeProgressive {
                    r#type: "x".to_owned(),
                    w: 800,
                    h: 600,
                    sizes: vec![300, 100, 200],
                },
            )],
            video_sizes: vec![telegram_api::VideoSize::videoSize(
                telegram_api::videoSize {
                    flags: 0,
                    r#type: "v".to_owned(),
                    w: 100,
                    h: 101, // not square
                    size: 1,
                    video_start_ts: 0.0,
                },
            )],
            dc_id: 2,
        }));
        assert_eq!(photo.animations.len(), 0);
        assert_eq!(photo.photos.len(), 1);
        // progressive: sorted sizes' max is the size, the rest are the
        // progressive steps
        assert_eq!(photo.photos[0].size, 300);
        assert_eq!(photo.photos[0].progressive_sizes, vec![100, 200]);
    }

    #[test]
    fn minithumbnail_object_expands_the_stripped_jpeg() {
        let object = get_minithumbnail_object(&[0x01, 90, 60, 0xde, 0xad]);
        assert_eq!(object.height, 90);
        assert_eq!(object.width, 60);
        // header[0..164] + height byte + header[165] + width byte +
        // header[167..] + payload + footer
        let header = base64::engine::general_purpose::STANDARD
            .decode(concat!(
                "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDACgcHiMeGSgjISMtKygwPGRBPDc3PHtYXUlkkYCZlo+AjIqgtObDoKrarYqMyP/L2u71////",
                "m8H///",
                "/6/+b9//j/2wBDASstLTw1PHZBQXb4pYyl+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj/",
                "wAARCAAAAAADASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/",
                "8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0R",
                "FRkd",
                "ISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2",
                "uHi4",
                "+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/",
                "8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkN",
                "ERUZ",
                "HSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2",
                "Nna4",
                "uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwA="
            ))
            .unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(&header[..164]);
        expected.push(90);
        expected.push(header[165]);
        expected.push(60);
        expected.extend_from_slice(&header[167..]);
        expected.extend_from_slice(&[0xde, 0xad]);
        expected.extend_from_slice(b"\xff\xd9");
        assert_eq!(object.data, expected);

        // short / non-0x01 payloads answer the forced-empty payload
        let empty = get_minithumbnail_object(&[0x01]);
        assert_eq!(empty.data, Vec::<u8>::new());
        let not_stripped = get_minithumbnail_object(&[0x02, 1, 2, 3]);
        assert_eq!(not_stripped.data, Vec::<u8>::new());
    }

    #[test]
    fn chat_photo_object_builds_from_photo() {
        let photo = Photo::default();
        let object = get_chat_photo_object(&photo);
        assert_eq!(object.id, 0);

        let mut photo = Photo {
            sticker_file_ids: Vec::new(),
            id: 42,
            date: 4242,
            minithumbnail: String::from_utf8_lossy(&[0x01, 90, 60, 0x21]).into_owned(),
            photos: vec![PhotoSize {
                size_type: b'm' as i32,
                size: 10,
                dimensions: Dimensions {
                    width: 320,
                    height: 320,
                },
                file_id: FileId::default(),
                progressive_sizes: vec![1, 2],
            }],
            animations: Vec::new(),
            sticker_photo_size: None,
            has_stickers: false,
        };
        let object = get_chat_photo_object(&photo);
        assert_eq!(object.id, 42);
        assert_eq!(object.added_date, 4242);
        assert_eq!(object.sizes.len(), 1);
        assert_eq!(object.sizes[0].r#type, "m");
        assert_eq!(object.sizes[0].progressive_sizes, vec![1, 2]);
        assert_eq!(
            object.minithumbnail.as_ref().expect("minithumbnail").height,
            90
        );

        // a small 'p' animation without its 'u' counterpart is dropped
        photo.animations = vec![AnimationSize {
            photo_size: PhotoSize {
                size_type: b'p' as i32,
                size: 5,
                dimensions: Dimensions {
                    width: 512,
                    height: 512,
                },
                file_id: FileId::default(),
                progressive_sizes: Vec::new(),
            },
            main_frame_timestamp: 0.25,
        }];
        let object = get_chat_photo_object(&photo);
        assert_eq!(object.animation.as_ref().expect("animation").length, 0);
        assert_eq!(
            object
                .small_animation
                .as_ref()
                .expect("small animation")
                .length,
            0
        );
    }

    #[test]
    fn sticker_photo_size_round_trips_the_object() {
        let sticker = StickerPhotoSize::get_sticker_photo_size(
            telegram_api::VideoSize::videoSizeEmojiMarkup(telegram_api::videoSizeEmojiMarkup {
                emoji_id: 500,
                background_colors: vec![0xFF0000, 0x00FF00],
            }),
        )
        .unwrap();
        let object = sticker.get_chat_photo_sticker_object();
        assert!(matches!(
            object.r#type,
            Some(td_api::ChatPhotoStickerType::chatPhotoStickerTypeCustomEmoji(_))
        ));
        assert!(matches!(
            object.background_fill,
            Some(td_api::BackgroundFill::backgroundFillGradient(_))
        ));

        // invalid ids / color counts drop the size
        assert!(
            StickerPhotoSize::get_sticker_photo_size(
                telegram_api::VideoSize::videoSizeEmojiMarkup(telegram_api::videoSizeEmojiMarkup {
                    emoji_id: 0,
                    background_colors: vec![1],
                })
            )
            .is_none()
        );
        assert!(
            StickerPhotoSize::get_sticker_photo_size(
                telegram_api::VideoSize::videoSizeEmojiMarkup(telegram_api::videoSizeEmojiMarkup {
                    emoji_id: 1,
                    background_colors: vec![1, 2, 3, 4, 5],
                })
            )
            .is_none()
        );
        // non-ID InputStickerSet forms are invalid until StickersManager
        assert!(
            StickerPhotoSize::get_sticker_photo_size(
                telegram_api::VideoSize::videoSizeStickerMarkup(
                    telegram_api::videoSizeStickerMarkup {
                        stickerset: Some(telegram_api::InputStickerSet::inputStickerSetShortName(
                            telegram_api::inputStickerSetShortName {
                                short_name: "x".to_owned(),
                            },
                        ),),
                        sticker_id: 1,
                        background_colors: vec![1],
                    }
                )
            )
            .is_none()
        );
        // the colors clamp to 24 bits
        let sticker = StickerPhotoSize::get_sticker_photo_size(
            telegram_api::VideoSize::videoSizeEmojiMarkup(telegram_api::videoSizeEmojiMarkup {
                emoji_id: 7,
                background_colors: vec![0x1FF0000],
            }),
        )
        .unwrap();
        let object = sticker.get_chat_photo_sticker_object();
        assert!(matches!(
            object.background_fill,
            Some(td_api::BackgroundFill::backgroundFillSolid(
                td_api::backgroundFillSolid { color: 0xFF0000 }
            ))
        ));
    }

    #[test]
    fn photo_get_file_ids_orders_sizes_first() {
        let photo = Photo {
            photos: vec![
                PhotoSize {
                    file_id: FileId::new(1, 0),
                    ..Default::default()
                },
                PhotoSize {
                    file_id: FileId::new(2, 0),
                    ..Default::default()
                },
            ],
            animations: vec![AnimationSize {
                photo_size: PhotoSize {
                    file_id: FileId::new(3, 0),
                    ..Default::default()
                },
                main_frame_timestamp: 0.0,
            }],
            ..Default::default()
        };
        assert_eq!(
            photo_get_file_ids(&photo),
            vec![FileId::new(1, 0), FileId::new(2, 0), FileId::new(3, 0)]
        );
    }

    #[test]
    fn pixel_count_helper_matches() {
        assert_eq!(
            get_dimensions_pixel_count(Dimensions {
                width: 320,
                height: 240
            }),
            320 * 240
        );
    }

    #[test]
    fn get_dialog_photo_matches_the_cpp_mapping() {
        // null and chatPhotoEmpty yield the empty photo
        assert_eq!(DialogPhoto::default(), DialogPhoto::get_dialog_photo(None));
        assert_eq!(
            DialogPhoto::default(),
            DialogPhoto::get_dialog_photo(Some(telegram_api::ChatPhoto::chatPhotoEmpty(
                telegram_api::chatPhotoEmpty {}
            )))
        );

        // a chatPhoto keeps the stripped minithumbnail, the animation flag and
        // the not-personal default; the file ids stay invalid placeholders
        // and an out-of-range dc id degrades to invalid (DcId::create parity)
        let photo = DialogPhoto::get_dialog_photo(Some(telegram_api::ChatPhoto::chatPhoto(
            telegram_api::chatPhoto {
                flags: 1,
                has_video: true,
                photo_id: 1234567890,
                stripped_thumb: b"abc".to_vec(),
                dc_id: 99999,
            },
        )));
        assert!(photo.has_animation);
        assert!(!photo.is_personal);
        assert_eq!(photo.minithumbnail, "abc");
        assert!(!photo.small_file_id.is_valid());
        assert!(!photo.big_file_id.is_valid());
        assert!(dialog_photo_get_file_ids(&photo).is_empty());
    }

    #[test]
    fn need_update_dialog_photo_ignores_the_minithumbnail() {
        let from = DialogPhoto {
            small_file_id: FileId::default(),
            big_file_id: FileId::default(),
            minithumbnail: "old".to_owned(),
            has_animation: false,
            is_personal: false,
        };
        let mut to = from.clone();
        to.minithumbnail = "new".to_owned();
        assert!(!from.need_update(&to));

        to.has_animation = true;
        assert!(from.need_update(&to));
        to.has_animation = false;
        to.is_personal = true;
        assert!(from.need_update(&to));
        to.is_personal = false;
        to.small_file_id = FileId::default();
        assert!(!from.need_update(&to));
    }

    #[test]
    fn as_fake_dialog_photo_selects_the_a_and_c_sizes() {
        use crate::chat_manager::DialogId;

        // an empty photo stays empty
        let empty = DialogPhoto::as_fake_dialog_photo(&Photo::default(), DialogId::new(0), true);
        assert_eq!(DialogPhoto::default(), empty);

        let photo = Photo {
            id: 1,
            date: 100,
            minithumbnail: "mini".to_owned(),
            photos: vec![
                PhotoSize {
                    size_type: b'x' as i32,
                    file_id: FileId::default(),
                    ..Default::default()
                },
                PhotoSize {
                    size_type: b'a' as i32,
                    file_id: FileId::default(),
                    ..Default::default()
                },
                PhotoSize {
                    size_type: b'c' as i32,
                    file_id: FileId::default(),
                    ..Default::default()
                },
            ],
            animations: vec![AnimationSize::default()],
            ..Default::default()
        };
        let fake = DialogPhoto::as_fake_dialog_photo(&photo, DialogId::new(-1001000000002), true);
        assert_eq!(fake.minithumbnail, "mini");
        assert!(fake.has_animation);
        assert!(fake.is_personal);
    }
}
