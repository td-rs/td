//! Port of the `td/telegram/files/` model and persistence layer — the
//! M.12.1 slice of the Files step: the `FileId` handle, the `FileType`
//! classification family, the `PhotoSizeSource`/`PhotoSizeType` pair the
//! remote photo locations carry, all `FileLocation` variants with their
//! store/parse and key formats, `FileEncryptionKey`, the `FileData`
//! record and the `FileDb` key-value access layer.
//!
//! Byte-exact against tdlib 1.8.67; the formats are pinned by the
//! conformance fixture of M.12.1h (a C++-written `files` table cross-read
//! in both directions).
//!
//! Deferred to their owners:
//! - the `FileSource` 28-variant wire format and the FileReferenceManager
//!   registry itself are M.12.2a; [`FileSourceStorage`] is the seam
//!   `FileData` serialization calls through (the C++
//!   `FileReferenceManager::store_file_source`/`parse_file_source`);
//! - `FileId` serialization is FileManager-mediated in the C++
//!   (`FileManager::store_file`), so the photo structures' file-id stores
//!   stay gated until M.12.2 — this slice only lands the two-component
//!   `FileId` type itself;
//! - the `FullRemoteFileLocation::as_input_*` telegram_api builders and
//!   `PhotoSizeSource::DialogPhoto::get_input_peer` are M.12.4 (they are
//!   the download-RPC query builders);
//! - the secure-storage surface of `FileEncryptionKey` (`create_secure_key`,
//!   `secret`/`value_hash`) is the Wave 3 SecureStorage seam;
//! - the `FileSource` 28-variant wire format and the FileReferenceManager
//!   registry itself are M.12.2a; [`FileSourceStorage`] is the seam
//!   `FileData` serialization calls through (the C++
//!   `FileReferenceManager::store_file_source`/`parse_file_source`);
//! - `FileId` serialization is FileManager-mediated in the C++
//!   (`FileManager::store_file`), so the photo structures' file-id stores
//!   stay gated until M.12.2 — this slice only lands the two-component
//!   `FileId` type itself;
//! - the `FullRemoteFileLocation::as_input_*` telegram_api builders and
//!   `PhotoSizeSource::DialogPhoto::get_input_peer` are M.12.4 (they are
//!   the download-RPC query builders).

use std::cell::RefCell;
use std::cmp::Ordering;
use std::fmt;
use std::rc::Rc;

use crate::chat_manager::DcId;
use crate::message_id::{DialogId, DialogType};
use crate::store_parse::{self, Parser};
use crate::td_api;
use tddb::sqlite_key_value::SqliteKeyValue;
use tdutils::status::{Result as TdResult, Status};

/// `td::FileId` (files/FileId.h) — the file-manager handle. Comparisons
/// and the hash cover the id alone, exactly like the C++ operators.
#[derive(Clone, Copy, Debug)]
pub struct FileId {
    id: i32,
    remote_id: i32,
}

impl FileId {
    pub fn new(id: i32, remote_id: i32) -> FileId {
        FileId { id, remote_id }
    }

    pub fn empty(&self) -> bool {
        self.id <= 0
    }

    pub fn is_valid(&self) -> bool {
        self.id > 0
    }

    pub fn get(self) -> i32 {
        self.id
    }

    pub fn get_remote(self) -> i32 {
        self.remote_id
    }
}

impl Default for FileId {
    fn default() -> FileId {
        FileId::new(0, 0)
    }
}

impl PartialEq for FileId {
    fn eq(&self, other: &FileId) -> bool {
        self.id == other.id
    }
}

impl Eq for FileId {}

impl PartialOrd for FileId {
    fn partial_cmp(&self, other: &FileId) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FileId {
    fn cmp(&self, other: &FileId) -> Ordering {
        self.id.cmp(&other.id)
    }
}

impl std::hash::Hash for FileId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_i32(self.id);
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.id, self.remote_id)
    }
}

/// `td::FileSourceId` (files/FileSourceId.h) — an opaque id the
/// FileReferenceManager allocates. The manager itself is M.12.2; the
/// UserManager still hands out ids from its local stand-in counter until
/// then.
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

impl fmt::Display for FileSourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileSourceId({})", self.0)
    }
}

/// `td::FileDbId` (files/FileDbId.h) — the database row id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileDbId(u64);

impl FileDbId {
    pub fn new(id: u64) -> FileDbId {
        FileDbId(id)
    }

    pub fn empty(&self) -> bool {
        self.0 == 0
    }

    pub fn is_valid(&self) -> bool {
        self.0 > 0
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for FileDbId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileDbId{{{}}}", self.0)
    }
}

/// `td::FileType` (files/FileType.h:17-48) — the discriminants are the
/// stored values (an enum store is a raw `int32`).
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileType {
    Thumbnail = 0,
    ProfilePhoto = 1,
    Photo = 2,
    VoiceNote = 3,
    Video = 4,
    Document = 5,
    Encrypted = 6,
    Temp = 7,
    Sticker = 8,
    Audio = 9,
    Animation = 10,
    EncryptedThumbnail = 11,
    Wallpaper = 12,
    VideoNote = 13,
    SecureDecrypted = 14,
    SecureEncrypted = 15,
    Background = 16,
    DocumentAsFile = 17,
    Ringtone = 18,
    CallLog = 19,
    PhotoStory = 20,
    VideoStory = 21,
    SelfDestructingPhoto = 22,
    SelfDestructingVideo = 23,
    SelfDestructingVideoNote = 24,
    SelfDestructingVoiceNote = 25,
    LivePhoto = 26,
    SelfDestructingLivePhoto = 27,
    Size = 28,
    None = 29,
}

/// `td::FileDirType` (files/FileType.h:50).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileDirType {
    Secure,
    Common,
}

impl FileType {
    /// The stored int32 → `FileType` (`static_cast<FileType>` in the
    /// C++; the Rust `as` only casts enum→int, so the reverse direction
    /// is this constructor). The C++ keeps an out-of-range value as raw
    /// garbage that aborts at first use; the panic here is the same
    /// refusal (the parse sites range-gate first, so only the ungated
    /// `FullGenerateFileLocation` type can reach it).
    pub fn from_raw(raw: i32) -> FileType {
        match raw {
            0 => FileType::Thumbnail,
            1 => FileType::ProfilePhoto,
            2 => FileType::Photo,
            3 => FileType::VoiceNote,
            4 => FileType::Video,
            5 => FileType::Document,
            6 => FileType::Encrypted,
            7 => FileType::Temp,
            8 => FileType::Sticker,
            9 => FileType::Audio,
            10 => FileType::Animation,
            11 => FileType::EncryptedThumbnail,
            12 => FileType::Wallpaper,
            13 => FileType::VideoNote,
            14 => FileType::SecureDecrypted,
            15 => FileType::SecureEncrypted,
            16 => FileType::Background,
            17 => FileType::DocumentAsFile,
            18 => FileType::Ringtone,
            19 => FileType::CallLog,
            20 => FileType::PhotoStory,
            21 => FileType::VideoStory,
            22 => FileType::SelfDestructingPhoto,
            23 => FileType::SelfDestructingVideo,
            24 => FileType::SelfDestructingVideoNote,
            25 => FileType::SelfDestructingVoiceNote,
            26 => FileType::LivePhoto,
            27 => FileType::SelfDestructingLivePhoto,
            28 => FileType::Size,
            29 => FileType::None,
            _ => unreachable!("Invalid FileType raw value {raw}"),
        }
    }
}

/// The C++ default (`FileType::None` — e.g. `FullLocalFileLocation()`
/// in FileLocation.h:705).
impl Default for FileType {
    fn default() -> FileType {
        FileType::None
    }
}

/// `td::FileTypeClass` (files/FileType.h:64).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileTypeClass {
    Photo,
    Document,
    Secure,
    Encrypted,
    Temp,
}

/// `td::MAX_FILE_TYPE` (files/FileType.h:52).
pub const MAX_FILE_TYPE: i32 = FileType::Size as i32;

/// `get_file_type` (FileType.cpp:14-70).
pub fn get_file_type(file_type: &td_api::FileType) -> FileType {
    match file_type {
        td_api::FileType::fileTypeThumbnail(_) => FileType::Thumbnail,
        td_api::FileType::fileTypeProfilePhoto(_) => FileType::ProfilePhoto,
        td_api::FileType::fileTypePhoto(_) => FileType::Photo,
        td_api::FileType::fileTypePhotoStory(_) => FileType::PhotoStory,
        td_api::FileType::fileTypeVoiceNote(_) => FileType::VoiceNote,
        td_api::FileType::fileTypeVideo(_) => FileType::Video,
        td_api::FileType::fileTypeVideoStory(_) => FileType::VideoStory,
        td_api::FileType::fileTypeDocument(_) => FileType::Document,
        td_api::FileType::fileTypeSecret(_) => FileType::Encrypted,
        td_api::FileType::fileTypeUnknown(_) => FileType::Temp,
        td_api::FileType::fileTypeSticker(_) => FileType::Sticker,
        td_api::FileType::fileTypeAudio(_) => FileType::Audio,
        td_api::FileType::fileTypeAnimation(_) => FileType::Animation,
        td_api::FileType::fileTypeSecretThumbnail(_) => FileType::EncryptedThumbnail,
        td_api::FileType::fileTypeWallpaper(_) => FileType::Background,
        td_api::FileType::fileTypeVideoNote(_) => FileType::VideoNote,
        td_api::FileType::fileTypeSecure(_) => FileType::SecureEncrypted,
        td_api::FileType::fileTypeNotificationSound(_) => FileType::Ringtone,
        td_api::FileType::fileTypeSelfDestructingPhoto(_) => FileType::SelfDestructingPhoto,
        td_api::FileType::fileTypeSelfDestructingVideo(_) => FileType::SelfDestructingVideo,
        td_api::FileType::fileTypeSelfDestructingVideoNote(_) => FileType::SelfDestructingVideoNote,
        td_api::FileType::fileTypeSelfDestructingVoiceNote(_) => FileType::SelfDestructingVoiceNote,
        td_api::FileType::fileTypeLivePhotoVideo(_) => FileType::LivePhoto,
        td_api::FileType::fileTypeSelfDestructingLivePhotoVideo(_) => {
            FileType::SelfDestructingLivePhoto
        }
        td_api::FileType::fileTypeNone(_) => FileType::None,
    }
}

/// `get_file_type_object` (FileType.cpp:72-137); the C++ `UNREACHABLE`
/// arms abort there too.
pub fn get_file_type_object(file_type: FileType) -> td_api::FileType {
    match file_type {
        FileType::Thumbnail => td_api::FileType::fileTypeThumbnail(td_api::fileTypeThumbnail),
        FileType::ProfilePhoto => {
            td_api::FileType::fileTypeProfilePhoto(td_api::fileTypeProfilePhoto)
        }
        FileType::Photo => td_api::FileType::fileTypePhoto(td_api::fileTypePhoto),
        FileType::VoiceNote => td_api::FileType::fileTypeVoiceNote(td_api::fileTypeVoiceNote),
        FileType::Video => td_api::FileType::fileTypeVideo(td_api::fileTypeVideo),
        FileType::Document => td_api::FileType::fileTypeDocument(td_api::fileTypeDocument),
        FileType::Encrypted => td_api::FileType::fileTypeSecret(td_api::fileTypeSecret),
        FileType::Temp => td_api::FileType::fileTypeUnknown(td_api::fileTypeUnknown),
        FileType::Sticker => td_api::FileType::fileTypeSticker(td_api::fileTypeSticker),
        FileType::Audio => td_api::FileType::fileTypeAudio(td_api::fileTypeAudio),
        FileType::Animation => td_api::FileType::fileTypeAnimation(td_api::fileTypeAnimation),
        FileType::EncryptedThumbnail => {
            td_api::FileType::fileTypeSecretThumbnail(td_api::fileTypeSecretThumbnail)
        }
        FileType::Wallpaper => td_api::FileType::fileTypeWallpaper(td_api::fileTypeWallpaper),
        FileType::VideoNote => td_api::FileType::fileTypeVideoNote(td_api::fileTypeVideoNote),
        FileType::SecureEncrypted => td_api::FileType::fileTypeSecure(td_api::fileTypeSecure),
        FileType::SecureDecrypted => unreachable!("SecureDecrypted in get_file_type_object"),
        FileType::Background => td_api::FileType::fileTypeWallpaper(td_api::fileTypeWallpaper),
        FileType::DocumentAsFile => td_api::FileType::fileTypeDocument(td_api::fileTypeDocument),
        FileType::Ringtone => {
            td_api::FileType::fileTypeNotificationSound(td_api::fileTypeNotificationSound)
        }
        FileType::CallLog => td_api::FileType::fileTypeDocument(td_api::fileTypeDocument),
        FileType::PhotoStory => td_api::FileType::fileTypePhotoStory(td_api::fileTypePhotoStory),
        FileType::VideoStory => td_api::FileType::fileTypeVideoStory(td_api::fileTypeVideoStory),
        FileType::SelfDestructingPhoto => {
            td_api::FileType::fileTypeSelfDestructingPhoto(td_api::fileTypeSelfDestructingPhoto)
        }
        FileType::SelfDestructingVideo => {
            td_api::FileType::fileTypeSelfDestructingVideo(td_api::fileTypeSelfDestructingVideo)
        }
        FileType::SelfDestructingVideoNote => td_api::FileType::fileTypeSelfDestructingVideoNote(
            td_api::fileTypeSelfDestructingVideoNote,
        ),
        FileType::SelfDestructingVoiceNote => td_api::FileType::fileTypeSelfDestructingVoiceNote(
            td_api::fileTypeSelfDestructingVoiceNote,
        ),
        FileType::LivePhoto => {
            td_api::FileType::fileTypeLivePhotoVideo(td_api::fileTypeLivePhotoVideo)
        }
        FileType::SelfDestructingLivePhoto => {
            td_api::FileType::fileTypeSelfDestructingLivePhotoVideo(
                td_api::fileTypeSelfDestructingLivePhotoVideo,
            )
        }
        FileType::None => td_api::FileType::fileTypeNone(td_api::fileTypeNone),
        FileType::Size => unreachable!("FileType::Size in get_file_type_object"),
    }
}

/// `get_main_file_type` (FileType.cpp:139-152).
pub fn get_main_file_type(file_type: FileType) -> FileType {
    match file_type {
        FileType::Wallpaper => FileType::Background,
        FileType::SecureDecrypted => FileType::SecureEncrypted,
        FileType::DocumentAsFile => FileType::Document,
        FileType::CallLog => FileType::Document,
        _ => file_type,
    }
}

/// `get_file_type_name` (FileType.cpp:154-202).
pub fn get_file_type_name(file_type: FileType) -> &'static str {
    match get_main_file_type(file_type) {
        FileType::Thumbnail => "thumbnails",
        FileType::ProfilePhoto => "profile_photos",
        FileType::Photo
        | FileType::SelfDestructingPhoto
        | FileType::LivePhoto
        | FileType::SelfDestructingLivePhoto => "photos",
        FileType::VoiceNote | FileType::SelfDestructingVoiceNote => "voice",
        FileType::Video | FileType::SelfDestructingVideo => "videos",
        FileType::Document => "documents",
        FileType::Encrypted => "secret",
        FileType::Temp => "temp",
        FileType::Sticker => "stickers",
        FileType::Audio => "music",
        FileType::Animation => "animations",
        FileType::EncryptedThumbnail => "secret_thumbnails",
        FileType::VideoNote | FileType::SelfDestructingVideoNote => "video_notes",
        FileType::SecureEncrypted => "passport",
        FileType::Background => "wallpapers",
        FileType::Ringtone => "notification_sounds",
        FileType::PhotoStory => "stories",
        FileType::VideoStory => "stories",
        _ => unreachable!("get_file_type_name"),
    }
}

/// `get_file_type_unique_name` (FileType.cpp:204-209).
pub fn get_file_type_unique_name(file_type: FileType) -> &'static str {
    if file_type == FileType::VideoStory {
        return "video_stories";
    }
    get_file_type_name(file_type)
}

/// `get_file_type_class` (FileType.cpp:211-252).
pub fn get_file_type_class(file_type: FileType) -> FileTypeClass {
    match file_type {
        FileType::Photo
        | FileType::ProfilePhoto
        | FileType::Thumbnail
        | FileType::EncryptedThumbnail
        | FileType::Wallpaper
        | FileType::PhotoStory
        | FileType::SelfDestructingPhoto => FileTypeClass::Photo,
        FileType::Video
        | FileType::VoiceNote
        | FileType::Document
        | FileType::Sticker
        | FileType::Audio
        | FileType::Animation
        | FileType::VideoNote
        | FileType::Background
        | FileType::DocumentAsFile
        | FileType::Ringtone
        | FileType::CallLog
        | FileType::VideoStory
        | FileType::SelfDestructingVideo
        | FileType::SelfDestructingVideoNote
        | FileType::SelfDestructingVoiceNote
        | FileType::LivePhoto
        | FileType::SelfDestructingLivePhoto => FileTypeClass::Document,
        FileType::SecureDecrypted | FileType::SecureEncrypted => FileTypeClass::Secure,
        FileType::Encrypted => FileTypeClass::Encrypted,
        FileType::Temp => FileTypeClass::Temp,
        FileType::None | FileType::Size => unreachable!("get_file_type_class"),
    }
}

/// `is_document_file_type` (FileType.cpp:254-256).
pub fn is_document_file_type(file_type: FileType) -> bool {
    get_file_type_class(file_type) == FileTypeClass::Document
}

/// `get_file_dir_type` (FileType.cpp:323-347).
pub fn get_file_dir_type(file_type: FileType) -> FileDirType {
    match file_type {
        FileType::Thumbnail
        | FileType::ProfilePhoto
        | FileType::Encrypted
        | FileType::Sticker
        | FileType::Temp
        | FileType::Wallpaper
        | FileType::EncryptedThumbnail
        | FileType::SecureEncrypted
        | FileType::SecureDecrypted
        | FileType::Background
        | FileType::Ringtone
        | FileType::PhotoStory
        | FileType::VideoStory
        | FileType::SelfDestructingPhoto
        | FileType::SelfDestructingVideo
        | FileType::SelfDestructingVideoNote
        | FileType::SelfDestructingVoiceNote
        | FileType::SelfDestructingLivePhoto => FileDirType::Secure,
        _ => FileDirType::Common,
    }
}

/// `is_file_big` (FileType.cpp:349-367).
pub fn is_file_big(file_type: FileType, expected_size: i64) -> bool {
    if get_file_type_class(file_type) == FileTypeClass::Photo {
        return false;
    }
    match file_type {
        FileType::VideoNote
        | FileType::Ringtone
        | FileType::CallLog
        | FileType::VideoStory
        | FileType::LivePhoto
        | FileType::SelfDestructingLivePhoto => return false,
        _ => {}
    }
    const SMALL_FILE_MAX_SIZE: i64 = 10 << 20;
    expected_size > SMALL_FILE_MAX_SIZE
}

/// `can_reuse_remote_file` (FileType.cpp:369-386).
pub fn can_reuse_remote_file(file_type: FileType) -> bool {
    !matches!(
        file_type,
        FileType::Thumbnail
            | FileType::EncryptedThumbnail
            | FileType::Background
            | FileType::CallLog
            | FileType::PhotoStory
            | FileType::VideoStory
            | FileType::SelfDestructingPhoto
            | FileType::SelfDestructingVideo
            | FileType::SelfDestructingVideoNote
            | FileType::SelfDestructingVoiceNote
            | FileType::SelfDestructingLivePhoto
    )
}

/// `guess_file_type_by_path` (FileType.cpp:388-421).
pub fn guess_file_type_by_path(file_path: &str, default_file_type: FileType) -> FileType {
    if default_file_type != FileType::None {
        if default_file_type == FileType::PhotoStory && file_path.ends_with(".mp4") {
            return FileType::VideoStory;
        }
        return default_file_type;
    }

    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    let extension = match file_name.rfind('.') {
        Some(pos) => &file_name[pos + 1..],
        None => "",
    };
    if extension == "jpg" || extension == "jpeg" {
        return FileType::Photo;
    }
    if extension == "ogg" || extension == "oga" || extension == "opus" {
        return FileType::VoiceNote;
    }
    if extension == "3gp" || extension == "mov" {
        return FileType::Video;
    }
    if extension == "mp3" || extension == "mpeg3" || extension == "m4a" {
        return FileType::Audio;
    }
    if extension == "webp" || extension == "tgs" || extension == "webm" {
        return FileType::Sticker;
    }
    if extension == "gif" {
        return FileType::Animation;
    }
    if extension == "mp4" || extension == "mpeg4" {
        return if file_name.to_lowercase().contains("-gif-") {
            FileType::Animation
        } else {
            FileType::Video
        };
    }
    FileType::Document
}

impl fmt::Display for FileType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            FileType::Thumbnail => "Thumbnail",
            FileType::ProfilePhoto => "ChatPhoto",
            FileType::Photo => "Photo",
            FileType::VoiceNote => "VoiceNote",
            FileType::Video => "Video",
            FileType::Document => "Document",
            FileType::Encrypted => "Secret",
            FileType::Temp => "Temp",
            FileType::Sticker => "Sticker",
            FileType::Audio => "Audio",
            FileType::Animation => "Animation",
            FileType::EncryptedThumbnail => "SecretThumbnail",
            FileType::Wallpaper => "Wallpaper",
            FileType::VideoNote => "VideoNote",
            FileType::SecureDecrypted => "Passport",
            FileType::SecureEncrypted => "Passport",
            FileType::Background => "Background",
            FileType::DocumentAsFile => "DocumentAsFile",
            FileType::Ringtone => "NotificationSound",
            FileType::CallLog => "CallLog",
            FileType::PhotoStory => "PhotoStory",
            FileType::VideoStory => "VideoStory",
            FileType::SelfDestructingPhoto => "SelfDestructingPhoto",
            FileType::SelfDestructingVideo => "SelfDestructingVideo",
            FileType::SelfDestructingVideoNote => "SelfDestructingVideoNote",
            FileType::SelfDestructingVoiceNote => "SelfDestructingVoiceNote",
            FileType::LivePhoto => "LivePhotoVideo",
            FileType::SelfDestructingLivePhoto => "SelfDestructingLivePhotoVideo",
            FileType::Size | FileType::None => "<invalid>",
        };
        f.write_str(name)
    }
}

/// `td::PhotoSizeType` (PhotoSizeType.h) — a size letter or a number.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhotoSizeType {
    pub type_: i32,
}

impl PhotoSizeType {
    pub fn new(type_: i32) -> PhotoSizeType {
        PhotoSizeType { type_ }
    }
}

impl PhotoSizeType {
    fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.type_);
    }

    fn parse_from(parser: &mut Parser) -> Result<PhotoSizeType, &'static str> {
        let type_ = parser.i32_()?;
        if !(0..128).contains(&type_) {
            return Err("Wrong photo size type");
        }
        Ok(PhotoSizeType::new(type_))
    }
}

impl fmt::Display for PhotoSizeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if (b'a' as i32..=b'z' as i32).contains(&self.type_) {
            write!(f, "{}", self.type_ as u8 as char)
        } else {
            write!(f, "{}", self.type_)
        }
    }
}

/// `td::PhotoSizeSource` (PhotoSizeSource.h:21-272) — what a photo-sized
/// remote file references. The variant order IS the stored order (a
/// `Variant` store writes the offset as `int32`).
#[derive(Clone, Debug, PartialEq)]
pub enum PhotoSizeSource {
    /// `Legacy` — parseable but never stored (the C++ `store` is
    /// `UNREACHABLE`).
    Legacy { secret: i64 },
    Thumbnail {
        file_type: FileType,
        thumbnail_type: PhotoSizeType,
    },
    DialogPhotoSmall {
        dialog_id: DialogId,
        dialog_access_hash: i64,
    },
    DialogPhotoBig {
        dialog_id: DialogId,
        dialog_access_hash: i64,
    },
    StickerSetThumbnail {
        sticker_set_id: i64,
        sticker_set_access_hash: i64,
    },
    FullLegacy {
        volume_id: i64,
        local_id: i32,
        secret: i64,
    },
    DialogPhotoSmallLegacy {
        dialog_id: DialogId,
        dialog_access_hash: i64,
        volume_id: i64,
        local_id: i32,
    },
    DialogPhotoBigLegacy {
        dialog_id: DialogId,
        dialog_access_hash: i64,
        volume_id: i64,
        local_id: i32,
    },
    StickerSetThumbnailLegacy {
        sticker_set_id: i64,
        sticker_set_access_hash: i64,
        volume_id: i64,
        local_id: i32,
    },
    StickerSetThumbnailVersion {
        sticker_set_id: i64,
        sticker_set_access_hash: i64,
        version: i32,
    },
}

impl PhotoSizeSource {
    /// `PhotoSizeSource::get_type` — total in Rust (the variant IS the
    /// type); the C++ `LOG_CHECK` against a negative offset cannot fire.
    pub fn get_type(&self) -> PhotoSizeSourceType {
        match self {
            PhotoSizeSource::Legacy { .. } => PhotoSizeSourceType::Legacy,
            PhotoSizeSource::Thumbnail { .. } => PhotoSizeSourceType::Thumbnail,
            PhotoSizeSource::DialogPhotoSmall { .. } => PhotoSizeSourceType::DialogPhotoSmall,
            PhotoSizeSource::DialogPhotoBig { .. } => PhotoSizeSourceType::DialogPhotoBig,
            PhotoSizeSource::StickerSetThumbnail { .. } => PhotoSizeSourceType::StickerSetThumbnail,
            PhotoSizeSource::FullLegacy { .. } => PhotoSizeSourceType::FullLegacy,
            PhotoSizeSource::DialogPhotoSmallLegacy { .. } => {
                PhotoSizeSourceType::DialogPhotoSmallLegacy
            }
            PhotoSizeSource::DialogPhotoBigLegacy { .. } => {
                PhotoSizeSourceType::DialogPhotoBigLegacy
            }
            PhotoSizeSource::StickerSetThumbnailLegacy { .. } => {
                PhotoSizeSourceType::StickerSetThumbnailLegacy
            }
            PhotoSizeSource::StickerSetThumbnailVersion { .. } => {
                PhotoSizeSourceType::StickerSetThumbnailVersion
            }
        }
    }

    pub fn thumbnail(file_type: FileType, thumbnail_type: i32) -> PhotoSizeSource {
        PhotoSizeSource::Thumbnail {
            file_type,
            thumbnail_type: PhotoSizeType::new(thumbnail_type),
        }
    }

    pub fn dialog_photo(
        dialog_id: DialogId,
        dialog_access_hash: i64,
        is_big: bool,
    ) -> PhotoSizeSource {
        if is_big {
            PhotoSizeSource::DialogPhotoBig {
                dialog_id,
                dialog_access_hash,
            }
        } else {
            PhotoSizeSource::DialogPhotoSmall {
                dialog_id,
                dialog_access_hash,
            }
        }
    }

    pub fn full_legacy(volume_id: i64, local_id: i32, secret: i64) -> PhotoSizeSource {
        PhotoSizeSource::FullLegacy {
            volume_id,
            local_id,
            secret,
        }
    }

    pub fn dialog_photo_legacy(
        dialog_id: DialogId,
        dialog_access_hash: i64,
        is_big: bool,
        volume_id: i64,
        local_id: i32,
    ) -> PhotoSizeSource {
        if is_big {
            PhotoSizeSource::DialogPhotoBigLegacy {
                dialog_id,
                dialog_access_hash,
                volume_id,
                local_id,
            }
        } else {
            PhotoSizeSource::DialogPhotoSmallLegacy {
                dialog_id,
                dialog_access_hash,
                volume_id,
                local_id,
            }
        }
    }

    pub fn sticker_set_thumbnail_legacy(
        sticker_set_id: i64,
        sticker_set_access_hash: i64,
        volume_id: i64,
        local_id: i32,
    ) -> PhotoSizeSource {
        PhotoSizeSource::StickerSetThumbnailLegacy {
            sticker_set_id,
            sticker_set_access_hash,
            volume_id,
            local_id,
        }
    }

    pub fn sticker_set_thumbnail(
        sticker_set_id: i64,
        sticker_set_access_hash: i64,
        version: i32,
    ) -> PhotoSizeSource {
        PhotoSizeSource::StickerSetThumbnailVersion {
            sticker_set_id,
            sticker_set_access_hash,
            version,
        }
    }

    /// `PhotoSizeSource::unique_less` (PhotoSizeSource.cpp:158-173).
    pub fn unique_less(lhs: &PhotoSizeSource, rhs: &PhotoSizeSource) -> bool {
        let lhs_compare_type = lhs.get_compare_type();
        let rhs_compare_type = rhs.get_compare_type();
        if lhs_compare_type != rhs_compare_type {
            return lhs_compare_type < rhs_compare_type;
        }
        if lhs_compare_type != 2 && lhs_compare_type != 3 {
            return false;
        }
        let lhs_volume_id = lhs.get_compare_volume_id();
        let rhs_volume_id = rhs.get_compare_volume_id();
        if lhs_volume_id != rhs_volume_id {
            return lhs_volume_id < rhs_volume_id;
        }
        lhs.get_compare_local_id() < rhs.get_compare_local_id()
    }

    /// `PhotoSizeSource::unique_equal` (PhotoSizeSource.cpp:175-186).
    pub fn unique_equal(lhs: &PhotoSizeSource, rhs: &PhotoSizeSource) -> bool {
        let lhs_compare_type = lhs.get_compare_type();
        let rhs_compare_type = rhs.get_compare_type();
        if lhs_compare_type != rhs_compare_type {
            return false;
        }
        if lhs_compare_type != 2 && lhs_compare_type != 3 {
            return true;
        }
        lhs.get_compare_volume_id() == rhs.get_compare_volume_id()
            && lhs.get_compare_local_id() == rhs.get_compare_local_id()
    }

    /// `PhotoSizeSource::get_file_type` (PhotoSizeSource.cpp:46-65).
    pub fn get_file_type(&self) -> FileType {
        match self.get_type() {
            PhotoSizeSourceType::Thumbnail => {
                let PhotoSizeSource::Thumbnail { file_type, .. } = self else {
                    unreachable!()
                };
                *file_type
            }
            PhotoSizeSourceType::DialogPhotoSmall
            | PhotoSizeSourceType::DialogPhotoBig
            | PhotoSizeSourceType::DialogPhotoSmallLegacy
            | PhotoSizeSourceType::DialogPhotoBigLegacy => FileType::ProfilePhoto,
            PhotoSizeSourceType::StickerSetThumbnail
            | PhotoSizeSourceType::StickerSetThumbnailLegacy
            | PhotoSizeSourceType::StickerSetThumbnailVersion => FileType::Thumbnail,
            PhotoSizeSourceType::Legacy | PhotoSizeSourceType::FullLegacy => {
                unreachable!("get_file_type of a legacy photo size source")
            }
        }
    }

    pub fn legacy(&self) -> i64 {
        match self {
            PhotoSizeSource::Legacy { secret } => *secret,
            _ => unreachable!("legacy() on a non-Legacy source"),
        }
    }

    pub fn thumbnail_type(&self) -> PhotoSizeType {
        match self {
            PhotoSizeSource::Thumbnail { thumbnail_type, .. } => *thumbnail_type,
            _ => unreachable!("thumbnail_type() on a non-Thumbnail source"),
        }
    }

    /// The `DialogPhoto` payload (dialog id + access hash); the `get_`
    /// prefix because the C++ shares the `dialog_photo` name between the
    /// static constructor and this accessor, which Rust cannot.
    pub fn get_dialog_photo(&self) -> (DialogId, i64) {
        match self {
            PhotoSizeSource::DialogPhotoSmall {
                dialog_id,
                dialog_access_hash,
            }
            | PhotoSizeSource::DialogPhotoBig {
                dialog_id,
                dialog_access_hash,
            }
            | PhotoSizeSource::DialogPhotoSmallLegacy {
                dialog_id,
                dialog_access_hash,
                ..
            }
            | PhotoSizeSource::DialogPhotoBigLegacy {
                dialog_id,
                dialog_access_hash,
                ..
            } => (*dialog_id, *dialog_access_hash),
            _ => unreachable!("dialog_photo() on a non-dialog source"),
        }
    }

    pub fn get_sticker_set_thumbnail(&self) -> (i64, i64) {
        match self {
            PhotoSizeSource::StickerSetThumbnail {
                sticker_set_id,
                sticker_set_access_hash,
            }
            | PhotoSizeSource::StickerSetThumbnailLegacy {
                sticker_set_id,
                sticker_set_access_hash,
                ..
            }
            | PhotoSizeSource::StickerSetThumbnailVersion {
                sticker_set_id,
                sticker_set_access_hash,
                ..
            } => (*sticker_set_id, *sticker_set_access_hash),
            _ => unreachable!("sticker_set_thumbnail() on a non-sticker-set source"),
        }
    }

    pub fn get_full_legacy(&self) -> (i64, i32, i64) {
        match self {
            PhotoSizeSource::FullLegacy {
                volume_id,
                local_id,
                secret,
            } => (*volume_id, *local_id, *secret),
            _ => unreachable!("full_legacy() on a non-full-legacy source"),
        }
    }

    pub fn get_dialog_photo_legacy(&self) -> (DialogId, i64, i64, i32) {
        match self {
            PhotoSizeSource::DialogPhotoSmallLegacy {
                dialog_id,
                dialog_access_hash,
                volume_id,
                local_id,
            }
            | PhotoSizeSource::DialogPhotoBigLegacy {
                dialog_id,
                dialog_access_hash,
                volume_id,
                local_id,
            } => (*dialog_id, *dialog_access_hash, *volume_id, *local_id),
            _ => unreachable!("dialog_photo_legacy() on a non-dialog-legacy source"),
        }
    }

    pub fn get_sticker_set_thumbnail_legacy(&self) -> (i64, i64, i64, i32) {
        match self {
            PhotoSizeSource::StickerSetThumbnailLegacy {
                sticker_set_id,
                sticker_set_access_hash,
                volume_id,
                local_id,
            } => (
                *sticker_set_id,
                *sticker_set_access_hash,
                *volume_id,
                *local_id,
            ),
            _ => unreachable!("sticker_set_thumbnail_legacy() on a non-sticker-set-legacy source"),
        }
    }

    pub fn sticker_set_thumbnail_version(&self) -> (i64, i64, i32) {
        match self {
            PhotoSizeSource::StickerSetThumbnailVersion {
                sticker_set_id,
                sticker_set_access_hash,
                version,
            } => (*sticker_set_id, *sticker_set_access_hash, *version),
            _ => unreachable!("sticker_set_thumbnail_version() on a non-version source"),
        }
    }

    /// `PhotoSizeSource::get_unique` (PhotoSizeSource.cpp:67-84) — the
    /// unique byte representation used in the FileDb keys.
    pub fn get_unique(&self) -> Vec<u8> {
        let compare_type = self.get_compare_type();
        if compare_type != 2 && compare_type != 3 {
            return vec![compare_type as u8];
        }

        let mut data = Vec::with_capacity(13);
        if compare_type == 2 {
            data.push(0x02);
        }
        store_parse::store_i64(&mut data, self.get_compare_volume_id());
        store_parse::store_i32(&mut data, self.get_compare_local_id());
        assert!(data.len() <= 13);
        data
    }

    /// `PhotoSizeSource::get_unique_name` (PhotoSizeSource.cpp:188-213).
    pub fn get_unique_name(&self, photo_id: i64) -> String {
        match self.get_type() {
            PhotoSizeSourceType::Thumbnail => {
                let type_ = self.thumbnail_type().type_;
                assert!((0..=127).contains(&type_));
                format!("{photo_id}_{type_}")
            }
            PhotoSizeSourceType::DialogPhotoSmall => photo_id.to_string(),
            PhotoSizeSourceType::DialogPhotoBig => format!("{photo_id}_1"),
            PhotoSizeSourceType::StickerSetThumbnailVersion => {
                let (sticker_set_id, _, version) = self.sticker_set_thumbnail_version();
                format!("{sticker_set_id}_{}", version as u32)
            }
            _ => unreachable!("get_unique_name"),
        }
    }

    /// `PhotoSizeSource::get_compare_type` (PhotoSizeSource.cpp:86-122).
    fn get_compare_type(&self) -> i32 {
        match self.get_type() {
            PhotoSizeSourceType::Legacy => unreachable!("get_compare_type of Legacy"),
            PhotoSizeSourceType::Thumbnail => {
                let type_ = self.thumbnail_type().type_;
                assert!((0..=127).contains(&type_));
                if type_ == b'a' as i32 {
                    return 0;
                }
                if type_ == b'c' as i32 {
                    return 1;
                }
                type_ + 5
            }
            PhotoSizeSourceType::DialogPhotoSmall => 0,
            PhotoSizeSourceType::DialogPhotoBig => 1,
            PhotoSizeSourceType::StickerSetThumbnail => {
                unreachable!("get_compare_type of StickerSetThumbnail")
            }
            PhotoSizeSourceType::FullLegacy
            | PhotoSizeSourceType::DialogPhotoSmallLegacy
            | PhotoSizeSourceType::DialogPhotoBigLegacy
            | PhotoSizeSourceType::StickerSetThumbnailLegacy => 3,
            PhotoSizeSourceType::StickerSetThumbnailVersion => 2,
        }
    }

    /// `PhotoSizeSource::get_compare_volume_id` (PhotoSizeSource.cpp:124-139).
    fn get_compare_volume_id(&self) -> i64 {
        match self.get_type() {
            PhotoSizeSourceType::FullLegacy => self.get_full_legacy().0,
            PhotoSizeSourceType::DialogPhotoSmallLegacy
            | PhotoSizeSourceType::DialogPhotoBigLegacy => self.get_dialog_photo_legacy().2,
            PhotoSizeSourceType::StickerSetThumbnailLegacy => {
                self.get_sticker_set_thumbnail_legacy().2
            }
            PhotoSizeSourceType::StickerSetThumbnailVersion => {
                self.sticker_set_thumbnail_version().0
            }
            _ => unreachable!("get_compare_volume_id"),
        }
    }

    /// `PhotoSizeSource::get_compare_local_id` (PhotoSizeSource.cpp:141-156).
    fn get_compare_local_id(&self) -> i32 {
        match self.get_type() {
            PhotoSizeSourceType::FullLegacy => self.get_full_legacy().1,
            PhotoSizeSourceType::DialogPhotoSmallLegacy
            | PhotoSizeSourceType::DialogPhotoBigLegacy => self.get_dialog_photo_legacy().3,
            PhotoSizeSourceType::StickerSetThumbnailLegacy => {
                self.get_sticker_set_thumbnail_legacy().3
            }
            PhotoSizeSourceType::StickerSetThumbnailVersion => {
                self.sticker_set_thumbnail_version().2
            }
            _ => unreachable!("get_compare_local_id"),
        }
    }

    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.get_type() as i32);
        match self {
            PhotoSizeSource::Legacy { .. } => unreachable!("storing PhotoSizeSource::Legacy"),
            PhotoSizeSource::Thumbnail {
                file_type,
                thumbnail_type,
            } => {
                store_parse::store_i32(out, *file_type as i32);
                thumbnail_type.store(out);
            }
            PhotoSizeSource::DialogPhotoSmall {
                dialog_id,
                dialog_access_hash,
            }
            | PhotoSizeSource::DialogPhotoBig {
                dialog_id,
                dialog_access_hash,
            } => {
                dialog_id.store(out);
                store_parse::store_i64(out, *dialog_access_hash);
            }
            PhotoSizeSource::StickerSetThumbnail {
                sticker_set_id,
                sticker_set_access_hash,
            } => {
                store_parse::store_i64(out, *sticker_set_id);
                store_parse::store_i64(out, *sticker_set_access_hash);
            }
            PhotoSizeSource::FullLegacy {
                volume_id,
                local_id,
                secret,
            } => {
                store_parse::store_i64(out, *volume_id);
                store_parse::store_i64(out, *secret);
                store_parse::store_i32(out, *local_id);
            }
            PhotoSizeSource::DialogPhotoSmallLegacy {
                dialog_id,
                dialog_access_hash,
                volume_id,
                local_id,
            }
            | PhotoSizeSource::DialogPhotoBigLegacy {
                dialog_id,
                dialog_access_hash,
                volume_id,
                local_id,
            } => {
                dialog_id.store(out);
                store_parse::store_i64(out, *dialog_access_hash);
                store_parse::store_i64(out, *volume_id);
                store_parse::store_i32(out, *local_id);
            }
            PhotoSizeSource::StickerSetThumbnailLegacy {
                sticker_set_id,
                sticker_set_access_hash,
                volume_id,
                local_id,
            } => {
                store_parse::store_i64(out, *sticker_set_id);
                store_parse::store_i64(out, *sticker_set_access_hash);
                store_parse::store_i64(out, *volume_id);
                store_parse::store_i32(out, *local_id);
            }
            PhotoSizeSource::StickerSetThumbnailVersion {
                sticker_set_id,
                sticker_set_access_hash,
                version,
            } => {
                store_parse::store_i64(out, *sticker_set_id);
                store_parse::store_i64(out, *sticker_set_access_hash);
                store_parse::store_i32(out, *version);
            }
        }
    }

    pub fn parse_from(parser: &mut Parser) -> Result<PhotoSizeSource, &'static str> {
        let offset = parser.i32_()?;
        let source = match offset {
            0 => {
                let secret = parser.i64_()?;
                PhotoSizeSource::Legacy { secret }
            }
            1 => {
                let raw_type = parser.i32_()?;
                if !(0..MAX_FILE_TYPE).contains(&raw_type) {
                    return Err("Wrong file type in PhotoSizeSource::Thumbnail");
                }
                let file_type = FileType::from_raw(raw_type);
                let thumbnail_type = PhotoSizeType::parse_from(parser)?;
                PhotoSizeSource::Thumbnail {
                    file_type,
                    thumbnail_type,
                }
            }
            2 | 3 => {
                let dialog_id = DialogId::parse_from(parser)?;
                let dialog_access_hash = parser.i64_()?;
                match dialog_id.get_type() {
                    DialogType::SecretChat | DialogType::None => {
                        return Err("Invalid chat identifier in PhotoSizeSource");
                    }
                    _ => {}
                }
                if offset == 2 {
                    PhotoSizeSource::DialogPhotoSmall {
                        dialog_id,
                        dialog_access_hash,
                    }
                } else {
                    PhotoSizeSource::DialogPhotoBig {
                        dialog_id,
                        dialog_access_hash,
                    }
                }
            }
            4 => {
                let sticker_set_id = parser.i64_()?;
                let sticker_set_access_hash = parser.i64_()?;
                PhotoSizeSource::StickerSetThumbnail {
                    sticker_set_id,
                    sticker_set_access_hash,
                }
            }
            5 => {
                let volume_id = parser.i64_()?;
                let secret = parser.i64_()?;
                let local_id = parser.i32_()?;
                // local_id can be negative in secret chat thumbnails
                PhotoSizeSource::FullLegacy {
                    volume_id,
                    local_id,
                    secret,
                }
            }
            6 | 7 => {
                let dialog_id = DialogId::parse_from(parser)?;
                let dialog_access_hash = parser.i64_()?;
                match dialog_id.get_type() {
                    DialogType::SecretChat | DialogType::None => {
                        return Err("Invalid chat identifier in PhotoSizeSource");
                    }
                    _ => {}
                }
                let volume_id = parser.i64_()?;
                let local_id = parser.i32_()?;
                if local_id < 0 {
                    return Err("Wrong local_id");
                }
                if offset == 6 {
                    PhotoSizeSource::DialogPhotoSmallLegacy {
                        dialog_id,
                        dialog_access_hash,
                        volume_id,
                        local_id,
                    }
                } else {
                    PhotoSizeSource::DialogPhotoBigLegacy {
                        dialog_id,
                        dialog_access_hash,
                        volume_id,
                        local_id,
                    }
                }
            }
            8 => {
                let sticker_set_id = parser.i64_()?;
                let sticker_set_access_hash = parser.i64_()?;
                let volume_id = parser.i64_()?;
                let local_id = parser.i32_()?;
                if local_id < 0 {
                    return Err("Wrong local_id");
                }
                PhotoSizeSource::StickerSetThumbnailLegacy {
                    sticker_set_id,
                    sticker_set_access_hash,
                    volume_id,
                    local_id,
                }
            }
            9 => {
                let sticker_set_id = parser.i64_()?;
                let sticker_set_access_hash = parser.i64_()?;
                let version = parser.i32_()?;
                PhotoSizeSource::StickerSetThumbnailVersion {
                    sticker_set_id,
                    sticker_set_access_hash,
                    version,
                }
            }
            _ => return Err("Wrong index in variant"),
        };
        if parser.version() >= store_parse::version::REMOVE_PHOTO_VOLUME_AND_LOCAL_ID
            && matches!(
                source.get_type(),
                PhotoSizeSourceType::Legacy | PhotoSizeSourceType::StickerSetThumbnail
            )
        {
            return Err("Invalid photo size source stored");
        }
        Ok(source)
    }
}

/// `td::PhotoSizeSource::Type` (PhotoSizeSource.h:22-33) — the variant
/// offsets, stored order.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhotoSizeSourceType {
    Legacy = 0,
    Thumbnail = 1,
    DialogPhotoSmall = 2,
    DialogPhotoBig = 3,
    StickerSetThumbnail = 4,
    FullLegacy = 5,
    DialogPhotoSmallLegacy = 6,
    DialogPhotoBigLegacy = 7,
    StickerSetThumbnailLegacy = 8,
    StickerSetThumbnailVersion = 9,
}

impl fmt::Display for PhotoSizeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.get_type() {
            PhotoSizeSourceType::Legacy => f.write_str("PhotoSizeSourceLegacy[]"),
            PhotoSizeSourceType::Thumbnail => {
                let PhotoSizeSource::Thumbnail {
                    file_type,
                    thumbnail_type,
                } = self
                else {
                    unreachable!()
                };
                write!(
                    f,
                    "PhotoSizeSourceThumbnail[{file_type}, type = {thumbnail_type}]"
                )
            }
            PhotoSizeSourceType::DialogPhotoSmall => f.write_str("PhotoSizeSourceChatPhotoSmall[]"),
            PhotoSizeSourceType::DialogPhotoBig => f.write_str("PhotoSizeSourceChatPhotoBig[]"),
            PhotoSizeSourceType::StickerSetThumbnail => {
                write!(
                    f,
                    "PhotoSizeSourceStickerSetThumbnail[{}]",
                    self.get_sticker_set_thumbnail().0
                )
            }
            PhotoSizeSourceType::FullLegacy => f.write_str("PhotoSizeSourceFullLegacy[]"),
            PhotoSizeSourceType::DialogPhotoSmallLegacy => {
                f.write_str("PhotoSizeSourceChatPhotoSmallLegacy[]")
            }
            PhotoSizeSourceType::DialogPhotoBigLegacy => {
                f.write_str("PhotoSizeSourceChatPhotoBigLegacy[]")
            }
            PhotoSizeSourceType::StickerSetThumbnailLegacy => {
                write!(
                    f,
                    "PhotoSizeSourceStickerSetThumbnailLegacy[{}]",
                    self.get_sticker_set_thumbnail().0
                )
            }
            PhotoSizeSourceType::StickerSetThumbnailVersion => {
                write!(
                    f,
                    "PhotoSizeSourceStickerSetThumbnailVersion[{}_{}]",
                    self.get_sticker_set_thumbnail().0,
                    self.sticker_set_thumbnail_version().2
                )
            }
        }
    }
}

// ==== Bitmask ====

/// `zero_one_encode` (tdutils/td/utils/misc.cpp:271-287) — run-length
/// escape of `0x00`/`0xff` runs.
fn zero_one_encode(data: &[u8]) -> Vec<u8> {
    let mut res = Vec::with_capacity(data.len());
    let mut i = 0;
    let n = data.len();
    while i < n {
        res.push(data[i]);
        let c = data[i];
        if c == 0 || c == 0xff {
            let mut cnt = 1u8;
            while cnt < 250 && i + (cnt as usize) < n && data[i + cnt as usize] == data[i] {
                cnt += 1;
            }
            res.push(cnt);
            i += cnt as usize - 1;
        }
        i += 1;
    }
    res
}

/// `zero_one_decode` (tdutils/td/utils/misc.cpp:289-307).
fn zero_one_decode(data: &[u8]) -> Vec<u8> {
    let mut res = Vec::with_capacity(data.len());
    let mut i = 0;
    let n = data.len();
    while i < n {
        let c = data[i];
        if (c == 0 || c == 0xff) && i + 1 < n {
            for _ in 0..data[i + 1] {
                res.push(data[i]);
            }
            i += 1;
            continue;
        }
        res.push(data[i]);
        i += 1;
    }
    res
}

/// `td::Bitmask` (files/FileBitmask.h) — the ready-parts bitmap the
/// chunk engine tracks and `PartialLocalFileLocation` persists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bitmask {
    data: Vec<u8>,
}

impl Bitmask {
    /// `Bitmask(Bitmask::Decode{}, data)` (FileBitmask.cpp:15-17).
    pub fn from_decode(data: &[u8]) -> Bitmask {
        Bitmask {
            data: zero_one_decode(data),
        }
    }

    /// `Bitmask(Bitmask::Ones{}, count)` (FileBitmask.cpp:18-22).
    pub fn from_ones(count: i64) -> Bitmask {
        let mut result = Bitmask {
            data: vec![0; ((count + 7) / 8) as usize],
        };
        for i in 0..count {
            result.set(i);
        }
        result
    }

    /// `Bitmask::encode` (FileBitmask.cpp:38-63): optionally truncate to
    /// `prefix_count` bits (masking the tail of the last byte), strip the
    /// trailing zero bytes to make the encoding deterministic, then
    /// zero-one-encode. `prefix_count == -1` keeps the whole mask.
    pub fn encode(&self, prefix_count: i32) -> Vec<u8> {
        let mut data = self.data.clone();
        if prefix_count != -1 {
            let truncated_size = ((prefix_count + 7) / 8) as usize;
            data.truncate(truncated_size);
            if prefix_count % 8 != 0 {
                let mask: u8 = (0xffu32 >> (8 - prefix_count % 8)) as u8;
                if let Some(last) = data.last_mut() {
                    *last &= mask;
                }
            }
        }
        while data.last() == Some(&0) {
            data.pop();
        }
        zero_one_encode(&data)
    }

    /// `Bitmask::get_ready_prefix_size` (FileBitmask.cpp:65-88) — the byte
    /// size of the contiguous ready prefix starting at `offset`.
    pub fn get_ready_prefix_size(&self, offset: i64, part_size: i64, file_size: i64) -> i64 {
        if offset < 0 {
            return 0;
        }
        if part_size == 0 {
            return 0;
        }
        assert!(part_size > 0);
        let offset_part = offset / part_size;
        let ones = self.get_ready_parts(offset_part);
        if ones == 0 {
            return 0;
        }
        let mut ready_parts_end = (offset_part + ones) * part_size;
        let mut offset = offset;
        if file_size != 0 && ready_parts_end > file_size {
            ready_parts_end = file_size;
            if offset > file_size {
                offset = file_size;
            }
        }
        let res = ready_parts_end - offset;
        assert!(res >= 0);
        res
    }

    /// `Bitmask::get_total_size` (FileBitmask.cpp:90-105).
    pub fn get_total_size(&self, part_size: i64, file_size: i64) -> i64 {
        let mut res = 0;
        for i in 0..self.size() {
            if self.get(i) {
                let from = i * part_size;
                let mut to = from + part_size;
                if file_size != 0 && file_size < to {
                    to = file_size;
                }
                if from < to {
                    res += to - from;
                }
            }
        }
        res
    }

    /// `Bitmask::get` (FileBitmask.cpp:107-116).
    pub fn get(&self, offset_part: i64) -> bool {
        if offset_part < 0 {
            return false;
        }
        let index = (offset_part / 8) as usize;
        if index >= self.data.len() {
            return false;
        }
        (self.data[index] & (1 << (offset_part % 8))) != 0
    }

    /// `Bitmask::get_ready_parts` (FileBitmask.cpp:118-124) — the number of
    /// consecutive ready parts starting at `offset_part`.
    pub fn get_ready_parts(&self, offset_part: i64) -> i64 {
        let mut res = 0;
        while self.get(offset_part + res) {
            res += 1;
        }
        res
    }

    /// `Bitmask::as_vector` (FileBitmask.cpp:126-135).
    pub fn as_vector(&self) -> Vec<i32> {
        let size = (self.data.len() * 8) as i32;
        (0..size).filter(|&i| self.get(i as i64)).collect()
    }

    /// `Bitmask::set` (FileBitmask.cpp:137-144).
    pub fn set(&mut self, offset_part: i64) {
        assert!(offset_part >= 0);
        let need_size = (offset_part / 8 + 1) as usize;
        if need_size > self.data.len() {
            self.data.resize(need_size, 0);
        }
        self.data[need_size - 1] |= 1 << (offset_part % 8);
    }

    /// `Bitmask::size` (FileBitmask.cpp:146-148).
    pub fn size(&self) -> i64 {
        self.data.len() as i64 * 8
    }

    /// `Bitmask::compress` (FileBitmask.cpp:24-36) — result part `i` is set
    /// iff parts `i * k .. (i + 1) * k` are all set.
    pub fn compress(&self, k: i32) -> Bitmask {
        let k = i64::from(k);
        let mut res = Bitmask::default();
        let mut i = 0;
        while i * k < self.size() {
            let mut f = true;
            let mut j = 0;
            while j < k && f {
                f &= self.get(i * k + j);
                j += 1;
            }
            if f {
                res.set(i);
            }
            i += 1;
        }
        res
    }

    /// The number of set bits — used only for the log rendering of
    /// [`PartialLocalFileLocation`] (the C++ decodes the mask there).
    fn ready_parts(&self) -> u64 {
        self.data.iter().map(|byte| byte.count_ones() as u64).sum()
    }
}

impl fmt::Display for Bitmask {
    /// The `operator<<` (FileBitmask.cpp:150-170) — run-length '0'/'1'
    /// rendering; trailing zeros are intentionally skipped.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut prev = false;
        let mut cnt: i64 = 0;
        for i in 0..=self.size() {
            let cur = self.get(i);
            if cur != prev {
                if cnt < 5 {
                    while cnt > 0 {
                        f.write_str(if prev { "1" } else { "0" })?;
                        cnt -= 1;
                    }
                } else {
                    write!(f, "{}(x{})", if prev { '1' } else { '0' }, cnt)?;
                    cnt = 0;
                }
            }
            prev = cur;
            cnt += 1;
        }
        Ok(())
    }
}

// ==== FileLocation family ====

/// `td::EmptyRemoteFileLocation` (FileLocation.h:36-51).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EmptyRemoteFileLocation {}

/// `td::PartialRemoteFileLocation` (FileLocation.h:53-81). The
/// `ready_size` is derived and never stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartialRemoteFileLocation {
    pub file_id: i64,
    pub part_count: i32,
    pub part_size: i32,
    pub ready_part_count: i32,
    pub is_big: i32,
    pub ready_size: i64,
}

impl PartialRemoteFileLocation {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.file_id);
        store_parse::store_i32(out, self.part_count);
        store_parse::store_i32(out, self.part_size);
        store_parse::store_i32(out, self.ready_part_count);
        store_parse::store_i32(out, self.is_big);
    }

    pub fn parse_from(parser: &mut Parser) -> Result<PartialRemoteFileLocation, &'static str> {
        let file_id = parser.i64_()?;
        let part_count = parser.i32_()?;
        let part_size = parser.i32_()?;
        let ready_part_count = parser.i32_()?;
        let is_big = parser.i32_()?;
        Ok(PartialRemoteFileLocation {
            file_id,
            part_count,
            part_size,
            ready_part_count,
            is_big,
            ready_size: 0,
        })
    }
}

impl fmt::Display for PartialRemoteFileLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let big = if self.is_big != 0 { "Big" } else { "Small" };
        write!(
            f,
            "[{big} partial remote location with {} parts of size {} with {} ready parts of total size {}]",
            self.part_count, self.part_size, self.ready_part_count, self.ready_size
        )
    }
}

/// `td::PhotoRemoteFileLocation` (FileLocation.h:83-119).
#[derive(Clone, Debug, PartialEq)]
pub struct PhotoRemoteFileLocation {
    pub id: i64,
    pub access_hash: i64,
    pub source: PhotoSizeSource,
}

impl PhotoRemoteFileLocation {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.id);
        store_parse::store_i64(out, self.access_hash);
        self.source.store(out);
    }

    /// `PhotoRemoteFileLocation::parse` (FileLocation.hpp:50-103) with
    /// the pre-`RemovePhotoVolumeAndLocalId` legacy path.
    pub fn parse_from(parser: &mut Parser) -> Result<PhotoRemoteFileLocation, &'static str> {
        let id = parser.i64_()?;
        let access_hash = parser.i64_()?;
        let source;
        if parser.version() >= store_parse::version::REMOVE_PHOTO_VOLUME_AND_LOCAL_ID {
            source = PhotoSizeSource::parse_from(parser)?;
        } else {
            let volume_id = parser.i64_()?;
            if parser.version() >= store_parse::version::ADD_PHOTO_SIZE_SOURCE {
                let parsed_source = PhotoSizeSource::parse_from(parser)?;
                let local_id = parser.i32_()?;
                source = match parsed_source.get_type() {
                    PhotoSizeSourceType::Legacy => {
                        PhotoSizeSource::full_legacy(volume_id, local_id, parsed_source.legacy())
                    }
                    PhotoSizeSourceType::FullLegacy | PhotoSizeSourceType::Thumbnail => {
                        parsed_source
                    }
                    PhotoSizeSourceType::DialogPhotoSmall | PhotoSizeSourceType::DialogPhotoBig => {
                        let (dialog_id, dialog_access_hash) = parsed_source.get_dialog_photo();
                        let is_big =
                            parsed_source.get_type() == PhotoSizeSourceType::DialogPhotoBig;
                        PhotoSizeSource::dialog_photo_legacy(
                            dialog_id,
                            dialog_access_hash,
                            is_big,
                            volume_id,
                            local_id,
                        )
                    }
                    PhotoSizeSourceType::StickerSetThumbnail => {
                        let (sticker_set_id, sticker_set_access_hash) =
                            parsed_source.get_sticker_set_thumbnail();
                        PhotoSizeSource::sticker_set_thumbnail_legacy(
                            sticker_set_id,
                            sticker_set_access_hash,
                            volume_id,
                            local_id,
                        )
                    }
                    _ => {
                        return Err("Invalid PhotoSizeSource in legacy PhotoRemoteFileLocation");
                    }
                };
            } else {
                let secret = parser.i64_()?;
                let local_id = parser.i32_()?;
                source = PhotoSizeSource::full_legacy(volume_id, local_id, secret);
            }
        }
        Ok(PhotoRemoteFileLocation {
            id,
            access_hash,
            source,
        })
    }

    /// `PhotoRemoteFileLocation::AsKey::store` (FileLocation.hpp:106-137)
    /// — the key body written after the FileDb `KEY_MAGIC` prefix.
    pub fn store_key(&self, out: &mut Vec<u8>, is_unique: bool) {
        let unique = self.source.get_unique();
        match self.source.get_type() {
            PhotoSizeSourceType::Legacy | PhotoSizeSourceType::StickerSetThumbnail => {
                unreachable!("storing a key of a legacy photo size source")
            }
            PhotoSizeSourceType::FullLegacy
            | PhotoSizeSourceType::DialogPhotoSmallLegacy
            | PhotoSizeSourceType::DialogPhotoBigLegacy
            | PhotoSizeSourceType::StickerSetThumbnailLegacy => {
                if !is_unique {
                    store_parse::store_i64(out, self.id);
                }
                store_parse::store_string_bytes(out, &unique);
            }
            PhotoSizeSourceType::DialogPhotoSmall
            | PhotoSizeSourceType::DialogPhotoBig
            | PhotoSizeSourceType::Thumbnail => {
                store_parse::store_i64(out, self.id);
                store_parse::store_string_bytes(out, &unique);
            }
            PhotoSizeSourceType::StickerSetThumbnailVersion => {
                store_parse::store_string_bytes(out, &unique);
            }
        }
    }
}

impl fmt::Display for PhotoRemoteFileLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[ID = {}, access_hash = {}, {}]",
            self.id, self.access_hash, self.source
        )
    }
}

/// `td::WebRemoteFileLocation` (FileLocation.h:121-150).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebRemoteFileLocation {
    pub url: String,
    pub access_hash: i64,
}

impl WebRemoteFileLocation {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_string(out, &self.url);
        store_parse::store_i64(out, self.access_hash);
    }

    pub fn parse_from(parser: &mut Parser) -> Result<WebRemoteFileLocation, &'static str> {
        let url = parser.string()?;
        let access_hash = parser.i64_()?;
        Ok(WebRemoteFileLocation { url, access_hash })
    }

    /// `WebRemoteFileLocation::AsKey::store` (FileLocation.hpp:154-156).
    pub fn store_key(&self, out: &mut Vec<u8>) {
        store_parse::store_string(out, &self.url);
    }
}

impl fmt::Display for WebRemoteFileLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[URL = {}, access_hash = {}]",
            self.url, self.access_hash
        )
    }
}

/// `td::CommonRemoteFileLocation` (FileLocation.h:152-181).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommonRemoteFileLocation {
    pub id: i64,
    pub access_hash: i64,
}

impl CommonRemoteFileLocation {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.id);
        store_parse::store_i64(out, self.access_hash);
    }

    pub fn parse_from(parser: &mut Parser) -> Result<CommonRemoteFileLocation, &'static str> {
        let id = parser.i64_()?;
        let access_hash = parser.i64_()?;
        Ok(CommonRemoteFileLocation { id, access_hash })
    }

    /// `CommonRemoteFileLocation::AsKey::store` (FileLocation.hpp:173-175).
    pub fn store_key(&self, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.id);
    }
}

impl fmt::Display for CommonRemoteFileLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[ID = {}, access_hash = {}]", self.id, self.access_hash)
    }
}

/// `td::FullRemoteFileLocation` (FileLocation.h:183-543).
#[derive(Clone, Debug)]
pub struct FullRemoteFileLocation {
    pub file_type: FileType,
    dc_id: DcId,
    file_reference: String,
    variant: FullRemoteVariant,
}

#[derive(Clone, Debug)]
enum FullRemoteVariant {
    Web(WebRemoteFileLocation),
    Photo(PhotoRemoteFileLocation),
    Common(CommonRemoteFileLocation),
}

/// The C++ `FileReferenceView::invalid_file_reference` (FileLocation.h:31-33).
const INVALID_FILE_REFERENCE: &str = "#";

/// `FullRemoteFileLocation::WEB_LOCATION_FLAG` (FileLocation.h:188).
const WEB_LOCATION_FLAG: i32 = 1 << 24;
/// `FullRemoteFileLocation::FILE_REFERENCE_FLAG` (FileLocation.h:189).
const FILE_REFERENCE_FLAG: i32 = 1 << 25;
/// `FullRemoteFileLocation::KEY_MAGIC` (FileLocation.h:542).
const FULL_REMOTE_KEY_MAGIC: i32 = 0x64374632;

/// `FullRemoteFileLocation::LocationType` (FileLocation.h:192).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteLocationType {
    Web,
    Photo,
    Common,
    None,
}

impl FullRemoteFileLocation {
    fn location_type(&self) -> RemoteLocationType {
        if self.is_web() {
            return RemoteLocationType::Web;
        }
        match get_file_type_class(self.file_type) {
            FileTypeClass::Photo => RemoteLocationType::Photo,
            FileTypeClass::Document | FileTypeClass::Secure | FileTypeClass::Encrypted => {
                RemoteLocationType::Common
            }
            FileTypeClass::Temp => RemoteLocationType::None,
        }
    }

    fn web(&self) -> &WebRemoteFileLocation {
        match &self.variant {
            FullRemoteVariant::Web(web) => web,
            _ => unreachable!("web() on a non-web location"),
        }
    }

    fn photo(&self) -> &PhotoRemoteFileLocation {
        match &self.variant {
            FullRemoteVariant::Photo(photo) => photo,
            _ => unreachable!("photo() on a non-photo location"),
        }
    }

    fn common(&self) -> &CommonRemoteFileLocation {
        match &self.variant {
            FullRemoteVariant::Common(common) => common,
            _ => unreachable!("common() on a non-common location"),
        }
    }

    /// `FullRemoteFileLocation::key_type` (FileLocation.h:236-242).
    fn key_type(&self) -> i32 {
        let mut type_ = self.file_type as i32;
        if self.is_web() {
            type_ |= WEB_LOCATION_FLAG;
        }
        type_
    }

    /// `FullRemoteFileLocation::check_file_reference` (FileLocation.h:244-249).
    fn check_file_reference(&mut self) {
        if self.file_reference == INVALID_FILE_REFERENCE {
            tdutils::log_error!("Tried to register file with invalid file reference");
            self.file_reference.clear();
        }
    }

    /// The photo constructor (FileLocation.h:501-509).
    pub fn from_photo(
        source: PhotoSizeSource,
        id: i64,
        access_hash: i64,
        dc_id: DcId,
        file_reference: String,
    ) -> FullRemoteFileLocation {
        let mut result = FullRemoteFileLocation {
            file_type: source.get_file_type(),
            dc_id,
            file_reference,
            variant: FullRemoteVariant::Photo(PhotoRemoteFileLocation {
                id,
                access_hash,
                source,
            }),
        };
        assert!(result.is_photo());
        result.check_file_reference();
        result
    }

    /// The document constructor (FileLocation.h:512-519).
    pub fn from_document(
        file_type: FileType,
        id: i64,
        access_hash: i64,
        dc_id: DcId,
        file_reference: String,
    ) -> FullRemoteFileLocation {
        let mut result = FullRemoteFileLocation {
            file_type,
            dc_id,
            file_reference,
            variant: FullRemoteVariant::Common(CommonRemoteFileLocation { id, access_hash }),
        };
        assert!(result.is_common());
        result.check_file_reference();
        result
    }

    /// The web-document constructor (FileLocation.h:522-526).
    pub fn from_web(file_type: FileType, url: String, access_hash: i64) -> FullRemoteFileLocation {
        let result = FullRemoteFileLocation {
            file_type,
            dc_id: DcId::default(),
            file_reference: String::new(),
            variant: FullRemoteVariant::Web(WebRemoteFileLocation { url, access_hash }),
        };
        assert!(result.is_web());
        assert!(!result.web().url.is_empty());
        result
    }

    pub fn get_dc_id(&self) -> DcId {
        assert!(!self.is_web());
        self.dc_id
    }

    /// `FullRemoteFileLocation::get_access_hash` (FileLocation.h:282-295).
    pub fn get_access_hash(&self) -> i64 {
        match self.location_type() {
            RemoteLocationType::Photo => self.photo().access_hash,
            RemoteLocationType::Common => self.common().access_hash,
            RemoteLocationType::Web => self.web().access_hash,
            RemoteLocationType::None => unreachable!("get_access_hash of an empty location"),
        }
    }

    /// `FullRemoteFileLocation::get_id` (FileLocation.h:297-309).
    pub fn get_id(&self) -> i64 {
        match self.location_type() {
            RemoteLocationType::Photo => self.photo().id,
            RemoteLocationType::Common => self.common().id,
            RemoteLocationType::Web | RemoteLocationType::None => {
                unreachable!("get_id of a web location")
            }
        }
    }

    /// `FullRemoteFileLocation::get_source` (FileLocation.h:311-323) —
    /// the C++ returns the `full_legacy(0, 0, 0)` stand-in for
    /// non-photo locations.
    pub fn get_source(&self) -> PhotoSizeSource {
        match self.location_type() {
            RemoteLocationType::Photo => self.photo().source.clone(),
            RemoteLocationType::Common | RemoteLocationType::Web => {
                PhotoSizeSource::full_legacy(0, 0, 0)
            }
            RemoteLocationType::None => unreachable!("get_source of an empty location"),
        }
    }

    /// `FullRemoteFileLocation::set_source` (FileLocation.h:325-329).
    pub fn set_source(&mut self, source: PhotoSizeSource) {
        assert!(self.is_photo());
        self.file_type = source.get_file_type();
        match &mut self.variant {
            FullRemoteVariant::Photo(photo) => photo.source = source,
            _ => unreachable!(),
        }
    }

    /// `FullRemoteFileLocation::delete_file_reference` (FileLocation.h:331-337).
    pub fn delete_file_reference(&mut self, bad_file_reference: &[u8]) -> bool {
        if self.file_reference != INVALID_FILE_REFERENCE
            && self.file_reference.as_bytes() == bad_file_reference
        {
            self.file_reference = INVALID_FILE_REFERENCE.to_string();
            return true;
        }
        false
    }

    pub fn has_file_reference(&self) -> bool {
        self.file_reference != INVALID_FILE_REFERENCE
    }

    pub fn get_file_reference(&self) -> &str {
        &self.file_reference
    }

    /// `FullRemoteFileLocation::get_url` (FileLocation.h:347-353).
    pub fn get_url(&self) -> &str {
        if self.is_web() { &self.web().url } else { "" }
    }

    pub fn is_web(&self) -> bool {
        matches!(self.variant, FullRemoteVariant::Web(_))
    }

    pub fn is_photo(&self) -> bool {
        self.location_type() == RemoteLocationType::Photo
    }

    pub fn is_common(&self) -> bool {
        self.location_type() == RemoteLocationType::Common
    }

    pub fn is_encrypted_secret(&self) -> bool {
        self.file_type == FileType::Encrypted
    }

    pub fn is_encrypted_secure(&self) -> bool {
        self.file_type == FileType::SecureEncrypted
    }

    pub fn is_encrypted_any(&self) -> bool {
        self.is_encrypted_secret() || self.is_encrypted_secure()
    }

    pub fn is_secure(&self) -> bool {
        self.file_type == FileType::SecureDecrypted || self.file_type == FileType::SecureEncrypted
    }

    pub fn is_document(&self) -> bool {
        self.is_common() && !self.is_secure() && !self.is_encrypted_secret()
    }

    /// `FullRemoteFileLocation::store` (FileLocation.hpp:178-194).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_file_reference = !self.file_reference.is_empty();
        let mut type_ = self.key_type();
        if has_file_reference {
            type_ |= FILE_REFERENCE_FLAG;
        }
        store_parse::store_i32(out, type_);
        store_parse::store_i32(out, self.dc_id.get_value());
        if has_file_reference {
            store_parse::store_string(out, &self.file_reference);
        }
        match &self.variant {
            FullRemoteVariant::Web(web) => web.store(out),
            FullRemoteVariant::Photo(photo) => photo.store(out),
            FullRemoteVariant::Common(common) => common.store(out),
        }
    }

    /// `FullRemoteFileLocation::parse` (FileLocation.hpp:197-273) with
    /// the full validation cascade.
    pub fn parse_from(parser: &mut Parser) -> Result<FullRemoteFileLocation, &'static str> {
        let raw_type = parser.i32_()?;
        let is_web = (raw_type & WEB_LOCATION_FLAG) != 0;
        let mut raw_type = raw_type & !WEB_LOCATION_FLAG;
        let has_file_reference = (raw_type & FILE_REFERENCE_FLAG) != 0;
        raw_type &= !FILE_REFERENCE_FLAG;
        if raw_type < 0 || !(0..MAX_FILE_TYPE).contains(&raw_type) {
            return Err("Invalid FileType in FullRemoteFileLocation");
        }
        let file_type = FileType::from_raw(raw_type);
        let dc_id_value = parser.i32_()?;
        let dc_id = DcId::from_value(dc_id_value);

        let mut file_reference = String::new();
        if has_file_reference {
            file_reference = parser.string()?;
            if file_reference == INVALID_FILE_REFERENCE {
                file_reference.clear();
            }
        }
        if is_web {
            return Ok(FullRemoteFileLocation {
                file_type,
                dc_id,
                file_reference,
                variant: FullRemoteVariant::Web(WebRemoteFileLocation::parse_from(parser)?),
            });
        }

        // The class match IS `location_type()` for a non-web location
        // (the C++ switches `get_file_type_class(file_type_)`).
        let variant = match get_file_type_class(file_type) {
            FileTypeClass::Photo => {
                let photo = PhotoRemoteFileLocation::parse_from(parser)?;
                match photo.source.get_type() {
                    PhotoSizeSourceType::Legacy | PhotoSizeSourceType::FullLegacy => {}
                    PhotoSizeSourceType::Thumbnail => {
                        let source_file_type = photo.source.get_file_type();
                        if source_file_type != file_type
                            || !(file_type == FileType::Photo
                                || file_type == FileType::PhotoStory
                                || file_type == FileType::SelfDestructingPhoto
                                || file_type == FileType::Thumbnail
                                || file_type == FileType::EncryptedThumbnail)
                        {
                            return Err("Invalid FileType in PhotoRemoteFileLocation Thumbnail");
                        }
                    }
                    PhotoSizeSourceType::DialogPhotoSmall
                    | PhotoSizeSourceType::DialogPhotoBig
                    | PhotoSizeSourceType::DialogPhotoSmallLegacy
                    | PhotoSizeSourceType::DialogPhotoBigLegacy => {
                        if file_type != FileType::ProfilePhoto {
                            return Err("Invalid FileType in PhotoRemoteFileLocation DialogPhoto");
                        }
                    }
                    PhotoSizeSourceType::StickerSetThumbnail
                    | PhotoSizeSourceType::StickerSetThumbnailLegacy
                    | PhotoSizeSourceType::StickerSetThumbnailVersion => {
                        if file_type != FileType::Thumbnail {
                            return Err(
                                "Invalid FileType in PhotoRemoteFileLocation StickerSetThumbnail",
                            );
                        }
                    }
                }
                FullRemoteVariant::Photo(photo)
            }
            FileTypeClass::Document | FileTypeClass::Secure | FileTypeClass::Encrypted => {
                FullRemoteVariant::Common(CommonRemoteFileLocation::parse_from(parser)?)
            }
            FileTypeClass::Temp => return Err("Invalid FileType in FullRemoteFileLocation"),
        };
        Ok(FullRemoteFileLocation {
            file_type,
            dc_id,
            file_reference,
            variant,
        })
    }

    /// `FullRemoteFileLocation::AsKey::store` (FileLocation.hpp:276-283).
    pub fn store_key(&self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.key_type());
        match &self.variant {
            FullRemoteVariant::Web(web) => web.store_key(out),
            FullRemoteVariant::Photo(photo) => photo.store_key(out, false),
            FullRemoteVariant::Common(common) => common.store_key(out),
        }
    }

    /// `FullRemoteFileLocation::AsUnique::store` (FileLocation.hpp:286-300).
    pub fn store_unique(&self, out: &mut Vec<u8>) {
        let type_ = if self.is_web() {
            0
        } else {
            get_file_type_class(self.file_type) as i32 + 1
        };
        store_parse::store_i32(out, type_);
        match &self.variant {
            FullRemoteVariant::Web(web) => web.store_key(out),
            FullRemoteVariant::Photo(photo) => photo.store_key(out, true),
            FullRemoteVariant::Common(common) => common.store_key(out),
        }
    }
}

impl PartialEq for FullRemoteFileLocation {
    fn eq(&self, other: &FullRemoteFileLocation) -> bool {
        self.file_type == other.file_type
            && self.dc_id == other.dc_id
            && variant_eq(&self.variant, &other.variant)
    }
}

impl Eq for FullRemoteFileLocation {}

fn variant_eq(lhs: &FullRemoteVariant, rhs: &FullRemoteVariant) -> bool {
    match (lhs, rhs) {
        (FullRemoteVariant::Web(lhs), FullRemoteVariant::Web(rhs)) => lhs.url == rhs.url,
        (FullRemoteVariant::Photo(lhs), FullRemoteVariant::Photo(rhs)) => {
            lhs.id == rhs.id && PhotoSizeSource::unique_equal(&lhs.source, &rhs.source)
        }
        (FullRemoteVariant::Common(lhs), FullRemoteVariant::Common(rhs)) => lhs.id == rhs.id,
        _ => false,
    }
}

fn variant_cmp(lhs: &FullRemoteVariant, rhs: &FullRemoteVariant) -> Ordering {
    match (lhs, rhs) {
        (FullRemoteVariant::Web(lhs), FullRemoteVariant::Web(rhs)) => lhs.url.cmp(&rhs.url),
        (FullRemoteVariant::Photo(lhs), FullRemoteVariant::Photo(rhs)) => {
            lhs.id.cmp(&rhs.id).then_with(|| {
                if PhotoSizeSource::unique_less(&lhs.source, &rhs.source) {
                    Ordering::Less
                } else if PhotoSizeSource::unique_equal(&lhs.source, &rhs.source) {
                    Ordering::Equal
                } else {
                    Ordering::Greater
                }
            })
        }
        (FullRemoteVariant::Common(lhs), FullRemoteVariant::Common(rhs)) => lhs.id.cmp(&rhs.id),
        (lhs, rhs) => variant_index(lhs).cmp(&variant_index(rhs)),
    }
}

fn variant_index(variant: &FullRemoteVariant) -> i32 {
    match variant {
        FullRemoteVariant::Web(_) => 0,
        FullRemoteVariant::Photo(_) => 1,
        FullRemoteVariant::Common(_) => 2,
    }
}

impl PartialOrd for FullRemoteFileLocation {
    fn partial_cmp(&self, other: &FullRemoteFileLocation) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FullRemoteFileLocation {
    /// `FullRemoteFileLocation::operator<` (FileLocation.h:528-536):
    /// variant first, then file type, then dc id.
    fn cmp(&self, other: &FullRemoteFileLocation) -> Ordering {
        variant_cmp(&self.variant, &other.variant)
            .then_with(|| self.file_type.cmp(&other.file_type))
            .then_with(|| self.dc_id.cmp(&other.dc_id))
    }
}

impl fmt::Display for FullRemoteFileLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}", self.file_type)?;
        if !self.is_web() {
            write!(f, ", {}", self.dc_id)?;
        }
        if !self.file_reference.is_empty() {
            // the C++ renders the file reference base64-encoded; the
            // tdutils base64 helper lands with the recorder step
            write!(f, ", file_reference = {:?}", self.file_reference)?;
        }
        write!(f, ", location = ")?;
        match &self.variant {
            FullRemoteVariant::Web(web) => write!(f, "{web}")?,
            FullRemoteVariant::Photo(photo) => write!(f, "{photo}")?,
            FullRemoteVariant::Common(common) => write!(f, "{common}")?,
        }
        write!(f, "]")
    }
}

/// `td::RemoteFileLocation` (FileLocation.h:567-643) — `Empty` equals a
/// `Partial` with zero ready parts, exactly like the C++ `is_empty`
/// collapse in `operator==`.
#[derive(Clone, Debug, Default)]
pub enum RemoteFileLocation {
    #[default]
    Empty,
    Partial(PartialRemoteFileLocation),
    Full(FullRemoteFileLocation),
}

impl RemoteFileLocation {
    fn is_empty(&self) -> bool {
        match self {
            RemoteFileLocation::Empty => true,
            RemoteFileLocation::Partial(partial) => partial.ready_part_count == 0,
            RemoteFileLocation::Full(_) => false,
        }
    }

    pub fn store(&self, out: &mut Vec<u8>) {
        match self {
            RemoteFileLocation::Empty => store_parse::store_i32(out, 0),
            RemoteFileLocation::Partial(partial) => {
                store_parse::store_i32(out, 1);
                partial.store(out);
            }
            RemoteFileLocation::Full(full) => {
                store_parse::store_i32(out, 2);
                full.store(out);
            }
        }
    }

    pub fn parse_from(parser: &mut Parser) -> Result<RemoteFileLocation, &'static str> {
        match parser.i32_()? {
            0 => Ok(RemoteFileLocation::Empty),
            1 => Ok(RemoteFileLocation::Partial(
                PartialRemoteFileLocation::parse_from(parser)?,
            )),
            2 => Ok(RemoteFileLocation::Full(
                FullRemoteFileLocation::parse_from(parser)?,
            )),
            _ => Err("Wrong index in variant"),
        }
    }
}

impl From<PartialRemoteFileLocation> for RemoteFileLocation {
    fn from(partial: PartialRemoteFileLocation) -> RemoteFileLocation {
        RemoteFileLocation::Partial(partial)
    }
}

impl From<FullRemoteFileLocation> for RemoteFileLocation {
    fn from(full: FullRemoteFileLocation) -> RemoteFileLocation {
        RemoteFileLocation::Full(full)
    }
}

impl PartialEq for RemoteFileLocation {
    fn eq(&self, other: &RemoteFileLocation) -> bool {
        if self.is_empty() && other.is_empty() {
            return true;
        }
        match (self, other) {
            (RemoteFileLocation::Empty, RemoteFileLocation::Empty) => true,
            (RemoteFileLocation::Partial(lhs), RemoteFileLocation::Partial(rhs)) => lhs == rhs,
            (RemoteFileLocation::Full(lhs), RemoteFileLocation::Full(rhs)) => lhs == rhs,
            _ => false,
        }
    }
}

impl Eq for RemoteFileLocation {}

impl fmt::Display for RemoteFileLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteFileLocation::Empty => f.write_str("[empty remote location]"),
            RemoteFileLocation::Partial(partial) => write!(f, "{partial}"),
            RemoteFileLocation::Full(full) => write!(f, "{full}"),
        }
    }
}

/// `td::EmptyLocalFileLocation` (FileLocation.h:645-660).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EmptyLocalFileLocation {}

/// `td::PartialLocalFileLocation` (FileLocation.h:662-689).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartialLocalFileLocation {
    pub file_type: FileType,
    pub part_size: i64,
    pub path: String,
    pub iv: String,
    /// The C++ `ready_bitmask_` is a binary `std::string` (a
    /// zero-one-encoded [`Bitmask`]); not UTF-8 in general.
    pub ready_bitmask: Vec<u8>,
    pub ready_size: i64,
}

impl PartialLocalFileLocation {
    /// `PartialLocalFileLocation::store` (FileLocation.hpp:313-326) —
    /// the split 31-bit part size and the `-1`/`-2` deprecated marker.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.file_type as i32);
        store_parse::store_string(out, &self.path);
        store_parse::store_i32(out, (self.part_size & 0x7FFFFFFF) as i32);
        let deprecated_ready_part_count = if self.part_size > 0x7FFFFFFF { -2 } else { -1 };
        store_parse::store_i32(out, deprecated_ready_part_count);
        store_parse::store_string(out, &self.iv);
        store_parse::store_string_bytes(out, &self.ready_bitmask);
        if deprecated_ready_part_count == -2 {
            assert!(self.part_size < (1i64 << 62));
            store_parse::store_i32(out, (self.part_size >> 31) as i32);
        }
    }

    /// `PartialLocalFileLocation::parse` (FileLocation.hpp:329-354) with
    /// the legacy all-ones bitmask path.
    pub fn parse_from(parser: &mut Parser) -> Result<PartialLocalFileLocation, &'static str> {
        let raw_file_type = parser.i32_()?;
        let file_type = FileType::from_raw(raw_file_type);
        if file_type < FileType::Thumbnail || file_type >= FileType::Size {
            return Err("Invalid type in PartialLocalFileLocation");
        }
        let path = parser.string()?;
        let part_size_low = parser.i32_()?;
        let mut part_size = part_size_low as i64;
        let deprecated_ready_part_count = parser.i32_()?;
        let iv = parser.string()?;
        let ready_bitmask;
        if deprecated_ready_part_count == -1 || deprecated_ready_part_count == -2 {
            ready_bitmask = parser.string_bytes()?;
            if deprecated_ready_part_count == -2 {
                let part_size_high = parser.i32_()?;
                part_size += (part_size_high as i64) << 31;
            }
        } else {
            assert!(deprecated_ready_part_count >= 0);
            assert!(deprecated_ready_part_count <= (1 << 22));
            ready_bitmask = Bitmask::from_ones(deprecated_ready_part_count as i64).encode(-1);
        }
        Ok(PartialLocalFileLocation {
            file_type,
            part_size,
            path,
            iv,
            ready_bitmask,
            ready_size: 0,
        })
    }
}

impl fmt::Display for PartialLocalFileLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // the C++ renders the decoded bitmask; the count of ready parts
        // is the equivalent summary (log-only)
        let ready_parts = Bitmask {
            data: zero_one_decode(&self.ready_bitmask),
        }
        .ready_parts();
        write!(
            f,
            "[partial local location of {} with part size {} and {ready_parts} ready parts of size {}] at {:?}",
            self.file_type, self.part_size, self.ready_size, self.path
        )
    }
}

/// `td::FullLocalFileLocation` (FileLocation.h:691-728).
#[derive(Clone, Debug)]
pub struct FullLocalFileLocation {
    pub file_type: FileType,
    pub path: String,
    pub mtime_nsec: u64,
}

/// `FullLocalFileLocation::KEY_MAGIC` (FileLocation.h:711).
const FULL_LOCAL_KEY_MAGIC: i32 = 0x84373817u32 as i32;

impl FullLocalFileLocation {
    pub fn new(file_type: FileType, path: String, mtime_nsec: u64) -> FullLocalFileLocation {
        FullLocalFileLocation {
            file_type,
            path,
            mtime_nsec,
        }
    }

    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.file_type as i32);
        store_parse::store_u64(out, self.mtime_nsec);
        store_parse::store_string(out, &self.path);
    }

    pub fn parse_from(parser: &mut Parser) -> Result<FullLocalFileLocation, &'static str> {
        let raw_file_type = parser.i32_()?;
        let file_type = FileType::from_raw(raw_file_type);
        if file_type < FileType::Thumbnail || file_type >= FileType::Size {
            return Err("Invalid type in FullLocalFileLocation");
        }
        let mtime_nsec = parser.u64_()?;
        let path = parser.string()?;
        Ok(FullLocalFileLocation {
            file_type,
            path,
            mtime_nsec,
        })
    }
}

impl Default for FullLocalFileLocation {
    fn default() -> FullLocalFileLocation {
        FullLocalFileLocation {
            file_type: FileType::None,
            path: String::new(),
            mtime_nsec: 0,
        }
    }
}

impl PartialEq for FullLocalFileLocation {
    /// `operator==` (FileLocation.h:718-720) — the `(mtime, file_type,
    /// path)` tie order.
    fn eq(&self, other: &FullLocalFileLocation) -> bool {
        self.mtime_nsec == other.mtime_nsec
            && self.file_type == other.file_type
            && self.path == other.path
    }
}

impl Eq for FullLocalFileLocation {}

impl PartialOrd for FullLocalFileLocation {
    fn partial_cmp(&self, other: &FullLocalFileLocation) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FullLocalFileLocation {
    /// `operator<` (FileLocation.h:714-716).
    fn cmp(&self, other: &FullLocalFileLocation) -> Ordering {
        self.mtime_nsec
            .cmp(&other.mtime_nsec)
            .then_with(|| self.file_type.cmp(&other.file_type))
            .then_with(|| self.path.cmp(&other.path))
    }
}

impl fmt::Display for FullLocalFileLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[full local location of {}] at {:?}",
            self.file_type, self.path
        )
    }
}

/// `td::LocalFileLocation` (FileLocation.h:766-841).
#[derive(Clone, Debug, Default)]
pub enum LocalFileLocation {
    #[default]
    Empty,
    Partial(PartialLocalFileLocation),
    Full(FullLocalFileLocation),
}

impl LocalFileLocation {
    pub fn new(file_type: FileType, path: String, mtime_nsec: u64) -> LocalFileLocation {
        LocalFileLocation::Full(FullLocalFileLocation::new(file_type, path, mtime_nsec))
    }

    /// `LocalFileLocation::file_name` (FileLocation.h:787-797).
    pub fn file_name(&self) -> &str {
        match self {
            LocalFileLocation::Partial(partial) => &partial.path,
            LocalFileLocation::Full(full) => &full.path,
            LocalFileLocation::Empty => "",
        }
    }

    pub fn store(&self, out: &mut Vec<u8>) {
        match self {
            LocalFileLocation::Empty => store_parse::store_i32(out, 0),
            LocalFileLocation::Partial(partial) => {
                store_parse::store_i32(out, 1);
                partial.store(out);
            }
            LocalFileLocation::Full(full) => {
                store_parse::store_i32(out, 2);
                full.store(out);
            }
        }
    }

    pub fn parse_from(parser: &mut Parser) -> Result<LocalFileLocation, &'static str> {
        match parser.i32_()? {
            0 => Ok(LocalFileLocation::Empty),
            1 => Ok(LocalFileLocation::Partial(
                PartialLocalFileLocation::parse_from(parser)?,
            )),
            2 => Ok(LocalFileLocation::Full(FullLocalFileLocation::parse_from(
                parser,
            )?)),
            _ => Err("Wrong index in variant"),
        }
    }
}

impl From<PartialLocalFileLocation> for LocalFileLocation {
    fn from(partial: PartialLocalFileLocation) -> LocalFileLocation {
        LocalFileLocation::Partial(partial)
    }
}

impl From<FullLocalFileLocation> for LocalFileLocation {
    fn from(full: FullLocalFileLocation) -> LocalFileLocation {
        LocalFileLocation::Full(full)
    }
}

impl PartialEq for LocalFileLocation {
    fn eq(&self, other: &LocalFileLocation) -> bool {
        match (self, other) {
            (LocalFileLocation::Empty, LocalFileLocation::Empty) => true,
            (LocalFileLocation::Partial(lhs), LocalFileLocation::Partial(rhs)) => lhs == rhs,
            (LocalFileLocation::Full(lhs), LocalFileLocation::Full(rhs)) => lhs == rhs,
            _ => false,
        }
    }
}

impl Eq for LocalFileLocation {}

impl fmt::Display for LocalFileLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LocalFileLocation::Empty => f.write_str("[empty local location]"),
            LocalFileLocation::Partial(partial) => write!(f, "{partial}"),
            LocalFileLocation::Full(full) => write!(f, "{full}"),
        }
    }
}

/// `td::FullGenerateFileLocation` (FileLocation.h:843-882).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FullGenerateFileLocation {
    pub file_type: FileType,
    pub original_path: String,
    pub conversion: String,
}

/// `FullGenerateFileLocation::KEY_MAGIC` (FileLocation.h:847).
const FULL_GENERATE_KEY_MAGIC: i32 = 0x8b60a1c8u32 as i32;

impl FullGenerateFileLocation {
    pub fn new(
        file_type: FileType,
        original_path: String,
        conversion: String,
    ) -> FullGenerateFileLocation {
        FullGenerateFileLocation {
            file_type,
            original_path,
            conversion,
        }
    }

    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.file_type as i32);
        store_parse::store_string(out, &self.original_path);
        store_parse::store_string(out, &self.conversion);
    }

    pub fn parse_from(parser: &mut Parser) -> Result<FullGenerateFileLocation, &'static str> {
        let file_type = FileType::from_raw(parser.i32_()?);
        let original_path = parser.string()?;
        let conversion = parser.string()?;
        Ok(FullGenerateFileLocation {
            file_type,
            original_path,
            conversion,
        })
    }
}

impl Default for FullGenerateFileLocation {
    fn default() -> FullGenerateFileLocation {
        FullGenerateFileLocation {
            file_type: FileType::None,
            original_path: String::new(),
            conversion: String::new(),
        }
    }
}

impl fmt::Display for FullGenerateFileLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[file_type = {}, original_path = {:?}, conversion = {:?}]",
            self.file_type, self.original_path, self.conversion
        )
    }
}

/// `td::GenerateFileLocation` (FileLocation.h:884-937).
#[derive(Clone, Debug, Default)]
pub enum GenerateFileLocation {
    #[default]
    Empty,
    Full(FullGenerateFileLocation),
}

impl GenerateFileLocation {
    pub fn store(&self, out: &mut Vec<u8>) {
        match self {
            GenerateFileLocation::Empty => store_parse::store_i32(out, 0),
            GenerateFileLocation::Full(full) => {
                store_parse::store_i32(out, 1);
                full.store(out);
            }
        }
    }

    pub fn parse_from(parser: &mut Parser) -> Result<GenerateFileLocation, &'static str> {
        match parser.i32_()? {
            0 => Ok(GenerateFileLocation::Empty),
            1 => Ok(GenerateFileLocation::Full(
                FullGenerateFileLocation::parse_from(parser)?,
            )),
            _ => Err("Invalid type in GenerateFileLocation"),
        }
    }
}

impl PartialEq for GenerateFileLocation {
    fn eq(&self, other: &GenerateFileLocation) -> bool {
        match (self, other) {
            (GenerateFileLocation::Empty, GenerateFileLocation::Empty) => true,
            (GenerateFileLocation::Full(lhs), GenerateFileLocation::Full(rhs)) => lhs == rhs,
            _ => false,
        }
    }
}

impl Eq for GenerateFileLocation {}

// ==== FileEncryptionKey ====

/// `td::FileEncryptionKey::Type` (FileEncryptionKey.h:24).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FileEncryptionKeyType {
    #[default]
    None,
    Secret,
    Secure,
}

/// `td::FileEncryptionKey` (files/FileEncryptionKey.h:23-104) — the
/// 32-byte key + 32-byte IV (secret) or the 32-byte passport secret +
/// optional value hash (secure). The secure-storage constructors and
/// accessors are the Wave 3 seam (module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileEncryptionKey {
    key_iv: Vec<u8>,
    type_: FileEncryptionKeyType,
}

impl FileEncryptionKey {
    /// The (key, iv) constructor (FileEncryptionKey.cpp:17-27).
    pub fn new(key: &[u8], iv: &[u8]) -> FileEncryptionKey {
        let mut key_iv = Vec::with_capacity(key.len() + iv.len());
        key_iv.extend_from_slice(key);
        key_iv.extend_from_slice(iv);
        if key.len() != 32 || iv.len() != 32 {
            tdutils::log_info!("Wrong key/iv sizes: {} {}", key.len(), iv.len());
            return FileEncryptionKey::default();
        }
        assert_eq!(key_iv.len(), 64);
        FileEncryptionKey {
            key_iv,
            type_: FileEncryptionKeyType::Secret,
        }
    }

    /// `FileEncryptionKey::create` (FileEncryptionKey.cpp:33-39).
    pub fn create() -> FileEncryptionKey {
        let mut key_iv = vec![0u8; 64];
        tdutils::random::secure_bytes(&mut key_iv);
        FileEncryptionKey {
            key_iv,
            type_: FileEncryptionKeyType::Secret,
        }
    }

    pub fn is_secret(&self) -> bool {
        self.type_ == FileEncryptionKeyType::Secret
    }

    pub fn is_secure(&self) -> bool {
        self.type_ == FileEncryptionKeyType::Secure
    }

    pub fn empty(&self) -> bool {
        self.key_iv.is_empty()
    }

    pub fn size(&self) -> usize {
        self.key_iv.len()
    }

    /// `FileEncryptionKey::key` (FileEncryptionKey.cpp:44-48).
    pub fn key(&self) -> &[u8; 32] {
        assert!(self.is_secret());
        assert_eq!(self.key_iv.len(), 64);
        self.key_iv[..32].try_into().unwrap()
    }

    /// `FileEncryptionKey::key_slice` (FileEncryptionKey.cpp:49-53).
    pub fn key_slice(&self) -> &[u8] {
        assert!(self.is_secret());
        assert_eq!(self.key_iv.len(), 64);
        &self.key_iv[..32]
    }

    /// `FileEncryptionKey::iv_slice` (FileEncryptionKey.cpp:79-83).
    pub fn iv_slice(&self) -> &[u8] {
        assert!(self.is_secret());
        assert_eq!(self.key_iv.len(), 64);
        &self.key_iv[32..]
    }

    /// `FileEncryptionKey::calc_fingerprint` (FileEncryptionKey.cpp:85-90).
    pub fn calc_fingerprint(&self) -> i32 {
        assert!(self.is_secret());
        let digest = tdutils::crypto::md5(&self.key_iv);
        let first = i32::from_le_bytes(digest[..4].try_into().unwrap());
        let second = i32::from_le_bytes(digest[4..8].try_into().unwrap());
        first ^ second
    }

    /// `FileEncryptionKey::store` (FileEncryptionKey.h:70-73) — only the
    /// raw bytes; the type rides the parse argument.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_string_bytes(out, &self.key_iv);
    }

    /// `FileEncryptionKey::parse` (FileEncryptionKey.h:74-87). The C++
    /// checks the PRE-parse `type_` before overwriting it — preserved
    /// here: on a fresh key the wrong-size log can never fire.
    pub fn parse_from(
        &mut self,
        type_: FileEncryptionKeyType,
        parser: &mut Parser,
    ) -> Result<(), &'static str> {
        self.key_iv = parser.string_bytes()?;
        if self.key_iv.is_empty() {
            self.type_ = FileEncryptionKeyType::None;
        } else {
            if self.type_ == FileEncryptionKeyType::Secure && self.key_iv.len() != 64 {
                tdutils::log_error!("Have wrong key size {}", self.key_iv.len());
            }
            self.type_ = type_;
        }
        Ok(())
    }
}

impl fmt::Display for FileEncryptionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_secret() {
            write!(f, "SecretKey{{{}}}", self.size())
        } else if self.is_secure() {
            write!(f, "SecureKey{{{}}}", self.size())
        } else {
            f.write_str("NoKey{}")
        }
    }
}

// ==== FileData ====

/// The file-source registry seam (module docs): the C++
/// `FileReferenceManager::store_file_source`/`parse_file_source` pair
/// that `FileData` serialization calls through. M.12.2a replaces the
/// stand-in with the real registry behind this trait.
pub trait FileSourceStorage {
    fn store_file_source(&mut self, file_source_id: FileSourceId, out: &mut Vec<u8>);
    fn parse_file_source(&mut self, parser: &mut Parser) -> Result<FileSourceId, &'static str>;
}

/// `td::FileData` (files/FileData.h:20-39).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileData {
    pub owner_dialog_id: DialogId,
    pub pmc_id: u64,
    pub remote: RemoteFileLocation,
    pub local: LocalFileLocation,
    pub generate: Option<FullGenerateFileLocation>,
    pub size: i64,
    pub expected_size: i64,
    pub remote_name: String,
    pub url: String,
    pub encryption_key: FileEncryptionKey,
    pub file_source_ids: Vec<FileSourceId>,
}

impl FileData {
    /// `FileData::store` (FileData.hpp:27-69).
    pub fn store(&self, out: &mut Vec<u8>, sources: &mut dyn FileSourceStorage) {
        let has_owner_dialog_id = self.owner_dialog_id.is_valid();
        let has_expected_size = self.size == 0 && self.expected_size != 0;
        let encryption_key_is_secure = self.encryption_key.is_secure();
        let has_sources = !self.file_source_ids.is_empty();
        let has_version = true;
        store_parse::store_flags(
            out,
            [
                has_owner_dialog_id,
                has_expected_size,
                encryption_key_is_secure,
                has_sources,
                has_version,
            ],
        );
        store_parse::store_i32(out, store_parse::version::LOG_EVENT_STORE_VERSION);
        if has_owner_dialog_id {
            self.owner_dialog_id.store(out);
        }
        store_parse::store_u64(out, self.pmc_id);
        self.remote.store(out);
        self.local.store(out);
        match &self.generate {
            Some(generate) => GenerateFileLocation::Full(generate.clone()).store(out),
            None => GenerateFileLocation::Empty.store(out),
        }
        if has_expected_size {
            store_parse::store_i64(out, self.expected_size);
        } else {
            store_parse::store_i64(out, self.size);
        }
        store_parse::store_string(out, &self.remote_name);
        store_parse::store_string(out, &self.url);
        self.encryption_key.store(out);
        if has_sources {
            store_parse::store_i32(out, self.file_source_ids.len() as i32);
            for file_source_id in &self.file_source_ids {
                sources.store_file_source(*file_source_id, out);
            }
        }
    }

    /// `FileData::parse` (FileData.hpp:71-135) — the blob is
    /// self-versioned (`parser.set_version`).
    pub fn parse_from(
        parser: &mut Parser,
        register_file_sources: bool,
        sources: Option<&mut dyn FileSourceStorage>,
    ) -> Result<FileData, &'static str> {
        let [
            has_owner_dialog_id,
            has_expected_size,
            encryption_key_is_secure,
            has_sources,
            has_version,
        ] = parser.parse_flags::<5>()?;

        let version = if has_version { parser.i32_()? } else { 0 };
        parser.set_version(version);

        let owner_dialog_id = if has_owner_dialog_id {
            DialogId::parse_from(parser)?
        } else {
            DialogId::default()
        };
        let pmc_id = parser.u64_()?;
        let remote = RemoteFileLocation::parse_from(parser)?;
        let local = LocalFileLocation::parse_from(parser)?;
        let generate = match GenerateFileLocation::parse_from(parser)? {
            GenerateFileLocation::Empty => None,
            GenerateFileLocation::Full(full) => Some(full),
        };
        let (size, expected_size) = if has_expected_size {
            (0, parser.i64_()?)
        } else {
            (parser.i64_()?, 0)
        };
        let remote_name = parser.string()?;
        let url = parser.string()?;
        let mut encryption_key = FileEncryptionKey::default();
        encryption_key.parse_from(
            if encryption_key_is_secure {
                FileEncryptionKeyType::Secure
            } else {
                FileEncryptionKeyType::Secret
            },
            parser,
        )?;
        let mut file_source_ids = Vec::new();
        if has_sources && register_file_sources {
            let registry = match sources {
                Some(registry) => registry,
                None => {
                    return Err(
                        "File data contains file sources, but no FileReferenceManager is available",
                    );
                }
            };
            let file_source_count = parser.i32_()?;
            if 0 < file_source_count && file_source_count < 5 {
                for _ in 0..file_source_count {
                    let file_source_id = registry.parse_file_source(parser)?;
                    if file_source_id.is_valid() && !file_source_ids.contains(&file_source_id) {
                        file_source_ids.push(file_source_id);
                    }
                }
            } else {
                return Err("Wrong number of file source identifiers");
            }
        }
        Ok(FileData {
            owner_dialog_id,
            pmc_id,
            remote,
            local,
            generate,
            size,
            expected_size,
            remote_name,
            url,
            encryption_key,
            file_source_ids,
        })
    }
}

impl fmt::Display for FileData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[remote_name = {:?} size = {} expected_size = {} {}",
            self.remote_name, self.size, self.expected_size, self.encryption_key
        )?;
        if !self.url.is_empty() {
            write!(f, " url = {:?}", self.url)?;
        }
        if let LocalFileLocation::Full(full) = &self.local {
            write!(f, " local {full}")?;
        }
        if let Some(generate) = &self.generate {
            write!(f, " generate {generate}")?;
        }
        if let RemoteFileLocation::Full(full) = &self.remote {
            write!(f, " remote {full}")?;
        }
        write!(f, ", sources = {:?}]", self.file_source_ids)
    }
}

// ==== FileDb ====

/// The key an entry is indexed under: the location's `KEY_MAGIC` plus
/// its key body (`FileDbInterface::as_key`, FileDb.h:47-61).
trait AsFileKey {
    fn as_key(&self) -> Vec<u8>;
}

impl AsFileKey for FullRemoteFileLocation {
    fn as_key(&self) -> Vec<u8> {
        let mut key = Vec::new();
        store_parse::store_i32(&mut key, FULL_REMOTE_KEY_MAGIC);
        self.store_key(&mut key);
        key
    }
}

impl AsFileKey for FullLocalFileLocation {
    fn as_key(&self) -> Vec<u8> {
        let mut key = Vec::new();
        store_parse::store_i32(&mut key, FULL_LOCAL_KEY_MAGIC);
        self.store(&mut key);
        key
    }
}

impl AsFileKey for FullGenerateFileLocation {
    fn as_key(&self) -> Vec<u8> {
        let mut key = Vec::new();
        store_parse::store_i32(&mut key, FULL_GENERATE_KEY_MAGIC);
        self.store(&mut key);
        key
    }
}

/// `td::to_integer<uint64>` (tdutils/td/utils/misc.h:185-194) — the
/// lenient digit-prefix fold the FileDb ctor uses for the `file_id`
/// counter (returns 0 on a missing/garbage value).
fn to_integer_u64(value: &[u8]) -> u64 {
    let mut integer_value = 0u64;
    for byte in value {
        if !byte.is_ascii_digit() {
            break;
        }
        integer_value = integer_value
            .wrapping_mul(10)
            .wrapping_add((byte - b'0') as u64);
    }
    integer_value
}

/// `td::FileDb` (files/FileDb.cpp:59-295) — the id-keyed access layer
/// over the "files" key-value table. The C++ `FileDbActor` +
/// `SqliteKeyValueSafe` marshaling collapses into direct calls: every
/// td-rs manager runs on the single scheduler thread.
pub struct FileDb {
    /// The `FileDb`-level counter — bumped by `get_next_file_db_id`
    /// (FileDb.cpp:176-179) and passed to the cycle-guard log; the store
    /// path never sees it.
    max_file_db_id: FileDbId,
    /// The `FileDbActor`-level counter — the persistence gate of the
    /// store/clear/ref ops (FileDb.cpp:83-86, 111-114, 135-138). The C++
    /// holds the two copies in different objects (the wrapper vs the
    /// actor, both seeded from the kv at open); `get_next` advancing only
    /// the wrapper copy is what lets a stored id below the handed-out
    /// maximum still update the `file_id` row.
    actor_max_file_db_id: FileDbId,
}

fn file_key(file_db_id: FileDbId) -> Vec<u8> {
    format!("file{}", file_db_id.get()).into_bytes()
}

fn file_ref_value(file_db_id: FileDbId) -> Vec<u8> {
    format!("@@{}", file_db_id.get()).into_bytes()
}

impl FileDb {
    /// `FileDb::FileDb` (FileDb.cpp:168-174) — reads the persisted
    /// counter from the `file_id` key.
    pub fn new(files_kv: &mut SqliteKeyValue) -> FileDb {
        let max_file_db_id = FileDbId::new(to_integer_u64(&files_kv.get(b"file_id")));
        FileDb {
            max_file_db_id,
            actor_max_file_db_id: max_file_db_id,
        }
    }

    /// `FileDbInterface::get_next_file_db_id` (FileDb.cpp:176-179) — the
    /// actor-level gate copy is deliberately untouched.
    pub fn get_next_file_db_id(&mut self) -> FileDbId {
        self.max_file_db_id = FileDbId::new(self.max_file_db_id.get() + 1);
        self.max_file_db_id
    }

    /// `FileDbInterface::get_file_data_sync` (FileDb.h:68-76) — the
    /// `@@`-ref chain with the C++'s cycle guard and the inline
    /// `optimize_refs` of the async path (the actor collapse, module
    /// docs).
    pub fn get_file_data_sync(
        &mut self,
        files_kv: &mut SqliteKeyValue,
        key: &[u8],
        sources: Option<&mut dyn FileSourceStorage>,
    ) -> Result<FileData, &'static str> {
        let file_db_id = get_file_db_id(files_kv, key)?;

        let mut file_db_ids = Vec::new();
        let mut data;
        let mut attempt_count = 0;
        let mut current_id = file_db_id;
        loop {
            attempt_count += 1;
            if attempt_count > 100 {
                panic!(
                    "Cycle in file database? max_file_db_id={} key={:?} links={:?}",
                    self.max_file_db_id, key, file_db_ids
                );
            }
            data = files_kv.get(&file_key(current_id));
            if data.starts_with(b"@@") {
                file_db_ids.push(current_id);
                current_id = FileDbId::new(to_integer_u64(&data[2..]));
            } else {
                break;
            }
        }

        if file_db_ids.len() > 1 {
            // `FileDbActor::optimize_refs` (FileDb.cpp:145-153)
            let final_id = current_id;
            files_kv.begin_write_transaction().ensure();
            for file_db_id in &file_db_ids[..file_db_ids.len() - 1] {
                files_kv.set(&file_key(*file_db_id), &file_ref_value(final_id));
            }
            files_kv.commit_transaction().ensure();
        }

        let mut parser = Parser::new(&data);
        let file_data = FileData::parse_from(&mut parser, true, sources)?;
        parser.fetch_end()?;
        Ok(file_data)
    }

    /// `FileDbInterface::set_file_data` (FileDb.cpp:209-229).
    #[allow(clippy::too_many_arguments)] // the C++ interface arity
    pub fn set_file_data(
        &mut self,
        files_kv: &mut SqliteKeyValue,
        file_db_id: FileDbId,
        file_data: &FileData,
        new_remote: bool,
        new_local: bool,
        new_generate: bool,
        sources: &mut dyn FileSourceStorage,
    ) {
        let remote_key = match (&file_data.remote, new_remote) {
            (RemoteFileLocation::Full(full), true) => Some(full.as_key()),
            _ => None,
        };
        let local_key = match (&file_data.local, new_local) {
            (LocalFileLocation::Full(full), true) => Some(full.as_key()),
            _ => None,
        };
        let generate_key = if new_generate {
            file_data
                .generate
                .as_ref()
                .map(|generate| generate.as_key())
        } else {
            None
        };

        files_kv.begin_write_transaction().ensure();
        self.persist_counter_if_needed(files_kv, file_db_id);
        let mut data = Vec::new();
        file_data.store(&mut data, sources);
        files_kv.set(&file_key(file_db_id), &data);
        if let Some(remote_key) = remote_key {
            files_kv.set(&remote_key, file_db_id.get().to_string().as_bytes());
        }
        if let Some(local_key) = local_key {
            files_kv.set(&local_key, file_db_id.get().to_string().as_bytes());
        }
        if let Some(generate_key) = generate_key {
            files_kv.set(&generate_key, file_db_id.get().to_string().as_bytes());
        }
        files_kv.commit_transaction().ensure();
    }

    /// `FileDbInterface::clear_file_data` (FileDb.cpp:193-207).
    pub fn clear_file_data(
        &mut self,
        files_kv: &mut SqliteKeyValue,
        file_db_id: FileDbId,
        file_data: &FileData,
    ) {
        let remote_key = match &file_data.remote {
            RemoteFileLocation::Full(full) => Some(full.as_key()),
            _ => None,
        };
        let local_key = match &file_data.local {
            LocalFileLocation::Full(full) => Some(full.as_key()),
            _ => None,
        };
        let generate_key = file_data
            .generate
            .as_ref()
            .map(|generate| generate.as_key());

        files_kv.begin_write_transaction().ensure();
        self.persist_counter_if_needed(files_kv, file_db_id);
        files_kv.erase(&file_key(file_db_id));
        if let Some(remote_key) = remote_key {
            files_kv.erase(&remote_key);
        }
        if let Some(local_key) = local_key {
            files_kv.erase(&local_key);
        }
        if let Some(generate_key) = generate_key {
            files_kv.erase(&generate_key);
        }
        files_kv.commit_transaction().ensure();
    }

    /// `FileDbInterface::set_file_data_ref` (FileDb.cpp:231-233).
    pub fn set_file_data_ref(
        &mut self,
        files_kv: &mut SqliteKeyValue,
        file_db_id: FileDbId,
        new_file_db_id: FileDbId,
    ) {
        files_kv.begin_write_transaction().ensure();
        self.persist_counter_if_needed(files_kv, file_db_id);
        files_kv.set(&file_key(file_db_id), &file_ref_value(new_file_db_id));
        files_kv.commit_transaction().ensure();
    }

    /// The shared `file_db_id > max_file_db_id_` counter write of the
    /// store/clear/ref methods (FileDb.cpp:83-86, 111-114, 135-138) —
    /// against the actor-level copy.
    fn persist_counter_if_needed(&mut self, files_kv: &mut SqliteKeyValue, file_db_id: FileDbId) {
        if file_db_id > self.actor_max_file_db_id {
            files_kv.set(b"file_id", file_db_id.get().to_string().as_bytes());
            self.actor_max_file_db_id = file_db_id;
        }
    }

    /// `FileDbInterface::pmc` (FileDb.cpp:234-236).
    pub fn pmc<'a>(&self, files_kv: &'a mut SqliteKeyValue) -> &'a mut SqliteKeyValue {
        files_kv
    }
}

/// `FileDb::get_file_db_id` (FileDb.cpp:287-294).
fn get_file_db_id(files_kv: &mut SqliteKeyValue, key: &[u8]) -> Result<FileDbId, &'static str> {
    let file_db_id_str = files_kv.get(key);
    if file_db_id_str.is_empty() {
        return Err("There is no such key in the database");
    }
    Ok(FileDbId::new(to_integer_u64(&file_db_id_str)))
}

// ==== ResourceState ====

/// `td::ResourceState` (files/ResourceState.h) — the bandwidth accounting
/// unit of the chunk engine: the [`ResourceManager`] holds the master
/// copy, every loader node a slave copy synchronized through
/// [`ResourceState::update_master`]/[`ResourceState::update_slave`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceState {
    estimated_limit: i64, // me
    limit: i64,           // master
    used: i64,            // me
    using: i64,           // me
    unit_size: usize,     // me
}

impl Default for ResourceState {
    /// The C++ member initializers — the unit size defaults to one part.
    fn default() -> ResourceState {
        ResourceState {
            estimated_limit: 0,
            limit: 0,
            used: 0,
            using: 0,
            unit_size: 1,
        }
    }
}

impl ResourceState {
    /// `ResourceState::start_use` (ResourceState.cpp:13-16) — take `x`
    /// units of the granted limit into flight.
    pub fn start_use(&mut self, x: i64) {
        self.using += x;
        assert!(self.used + self.using <= self.limit);
    }

    /// `ResourceState::stop_use` (ResourceState.cpp:18-22) — a flight
    /// completes: it stops using and counts as used.
    pub fn stop_use(&mut self, x: i64) {
        assert!(x <= self.using);
        self.using -= x;
        self.used += x;
    }

    /// `ResourceState::update_limit` (ResourceState.cpp:24-26).
    pub fn update_limit(&mut self, extra: i64) {
        self.limit += extra;
    }

    /// `ResourceState::update_estimated_limit` (ResourceState.cpp:28-46);
    /// returns false when nothing changed.
    pub fn update_estimated_limit(&mut self, extra: i64) -> bool {
        // unused() must be positive, i.e. used + using must be less than
        // limit
        // TODO(cpp): use exact intersection between using and extra.
        let using_and_extra_intersection = self.using.min(extra); // between 0 and min(using, extra)
        let mut new_estimated_limit = self.used + self.using + extra - using_and_extra_intersection;

        // Use extra extra limit
        if new_estimated_limit < self.limit {
            let extra_limit = self.limit - new_estimated_limit;
            self.used += extra_limit;
            new_estimated_limit += extra_limit;
        }

        if new_estimated_limit == self.estimated_limit {
            return false;
        }
        self.estimated_limit = new_estimated_limit;
        true
    }

    /// `ResourceState::set_unit_size` (ResourceState.h:24-26).
    pub fn set_unit_size(&mut self, new_unit_size: usize) {
        self.unit_size = new_unit_size;
    }

    /// `ResourceState::active_limit` (ResourceState.cpp:48-50).
    pub fn active_limit(&self) -> i64 {
        self.limit - self.used
    }

    /// `ResourceState::get_using` (ResourceState.h:30-32).
    pub fn get_using(&self) -> i64 {
        self.using
    }

    /// `ResourceState::unused` (ResourceState.cpp:52-54).
    pub fn unused(&self) -> i64 {
        self.limit - self.using - self.used
    }

    /// `ResourceState::estimated_extra` (ResourceState.cpp:56-60) — the
    /// remaining work estimate rounded up to whole unit sizes.
    pub fn estimated_extra(&self) -> i64 {
        let mut new_unused = self.limit.max(self.estimated_limit) - self.using - self.used;
        let unit_size = self.unit_size as i64;
        new_unused = (new_unused + unit_size - 1) / unit_size * unit_size;
        new_unused + self.using + self.used - self.limit
    }

    /// `ResourceState::unit_size` (ResourceState.h:38-40).
    pub fn unit_size(&self) -> usize {
        self.unit_size
    }

    /// `ResourceState::update_master` (ResourceState.cpp:74-79) — the
    /// slave adopts everything but the master's `limit`.
    pub fn update_master(&mut self, other: &ResourceState) {
        self.estimated_limit = other.estimated_limit;
        self.used = other.used;
        self.using = other.using;
        self.unit_size = other.unit_size;
    }

    /// `ResourceState::update_slave` (ResourceState.cpp:81-83) — the
    /// master adopts only the slave's `limit`.
    pub fn update_slave(&mut self, other: &ResourceState) {
        self.limit = other.limit;
    }
}

/// `ResourceState::operator+=` (ResourceState.cpp:62-66).
impl std::ops::AddAssign<&ResourceState> for ResourceState {
    fn add_assign(&mut self, rhs: &ResourceState) {
        self.using += rhs.active_limit();
        self.used += rhs.used;
    }
}

/// `ResourceState::operator-=` (ResourceState.cpp:68-72).
impl std::ops::SubAssign<&ResourceState> for ResourceState {
    fn sub_assign(&mut self, rhs: &ResourceState) {
        self.using -= rhs.active_limit();
        self.used -= rhs.used;
    }
}

impl fmt::Display for ResourceState {
    /// The `operator<<` (ResourceState.cpp:85-88), in the `[tag:value]`
    /// form of `td::format::tag`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[estimated_limit:{}][used:{}][using:{}][limit:{}]",
            self.estimated_limit, self.used, self.using, self.limit
        )
    }
}

// ==== PartsManager ====

/// `PartsManager::MAX_PART_COUNT` (PartsManager.h:56).
const MAX_PART_COUNT: i32 = 4000;
/// `PartsManager::MAX_PART_COUNT_PREMIUM` (PartsManager.h:57).
const MAX_PART_COUNT_PREMIUM: i32 = 8000;
/// `PartsManager::MAX_PART_SIZE` (PartsManager.h:58).
const MAX_PART_SIZE: usize = 512 << 10;
/// `PartsManager::MAX_FILE_SIZE` (PartsManager.h:59).
const MAX_FILE_SIZE: i64 = MAX_PART_SIZE as i64 * (MAX_PART_COUNT_PREMIUM as i64);

/// `calc_part_count` (PartsManager.cpp:22-25).
fn calc_part_count(size: i64, part_size: usize) -> i64 {
    assert!(part_size != 0);
    (size + part_size as i64 - 1) / part_size as i64
}

/// `td::Part` (PartsManager.h:17-21). `id == -1` is the C++ "empty part"
/// sentinel of `get_empty_part` — [`PartsManager::start_part`] returns it
/// when there is nothing to request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    pub id: i32,
    pub offset: i64,
    pub size: usize,
}

/// `PartsManager::PartStatus` (PartsManager.h:61).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PartStatus {
    Empty,
    Pending,
    Ready,
}

/// `td::PartsManager` (files/PartsManager.h) — the chunk scheduler both
/// download and upload run through: part state, the ready bitmask, prefix
/// and streaming accounting, and the unknown-size bookkeeping.
#[derive(Debug, Default)]
pub struct PartsManager {
    is_upload: bool,
    need_check: bool,
    checked_prefix_size: i64,

    known_prefix_flag: bool,
    known_prefix_size: i64,

    size: i64,
    expected_size: i64,
    min_size: i64,
    max_size: i64,
    unknown_size_flag: bool,
    ready_size: i64,
    streaming_ready_size: i64,

    part_size: usize,
    part_count: i32,
    pending_count: i32,
    first_empty_part: i32,
    first_not_ready_part: i32,
    streaming_offset: i64,
    streaming_limit: i64,
    first_streaming_empty_part: i32,
    first_streaming_not_ready_part: i32,
    part_status: Vec<PartStatus>,
    bitmask: Bitmask,
    use_part_count_limit: bool,
}

impl PartsManager {
    /// `PartsManager::init` (PartsManager.cpp:105-143).
    #[allow(clippy::too_many_arguments)]
    pub fn init(
        &mut self,
        size: i64,
        expected_size: i64,
        is_size_final: bool,
        part_size: usize,
        ready_parts: &[i32],
        use_part_count_limit: bool,
        is_upload: bool,
    ) -> TdResult<()> {
        assert!(expected_size >= size);
        self.is_upload = is_upload;
        self.use_part_count_limit = use_part_count_limit;
        self.expected_size = expected_size;
        if self.expected_size > MAX_FILE_SIZE {
            return TdResult::from_status(Status::error_message("Too big file"));
        }
        if !is_size_final {
            return self.init_known_prefix(size, part_size, ready_parts);
        }
        if size == 0 {
            return self.init_no_size(part_size, ready_parts);
        }
        assert!(size > 0, "[size:{}] {}", size, self);
        self.unknown_size_flag = false;
        self.size = size;

        if part_size != 0 {
            self.part_size = part_size;
            if self.use_part_count_limit
                && self.part_size < MAX_PART_SIZE
                && calc_part_count(self.expected_size, self.part_size) > i64::from(MAX_PART_COUNT)
            {
                assert!(self.is_upload);
                return TdResult::from_status(Status::error_message("FILE_UPLOAD_RESTART"));
            }
        } else {
            self.part_size = 64 << 10;
            while self.part_size < MAX_PART_SIZE
                && calc_part_count(self.expected_size, self.part_size) > i64::from(MAX_PART_COUNT)
            {
                self.part_size *= 2;
            }
        }
        assert!(1 <= self.size, "{}", self);
        assert!(
            !use_part_count_limit
                || calc_part_count(self.expected_size, self.part_size)
                    <= i64::from(MAX_PART_COUNT_PREMIUM),
            "[is_size_final:{}][ready_parts:{}] {}",
            is_size_final,
            ready_parts.len(),
            self
        );
        self.part_count = calc_part_count(self.size, self.part_size) as i32;

        self.init_common(ready_parts)
    }

    /// `PartsManager::init_known_prefix` (PartsManager.cpp:28-32).
    fn init_known_prefix(
        &mut self,
        known_prefix: i64,
        part_size: usize,
        ready_parts: &[i32],
    ) -> TdResult<()> {
        self.known_prefix_flag = true;
        self.known_prefix_size = known_prefix;
        self.init_no_size(part_size, ready_parts)
    }

    /// `PartsManager::init_no_size` (PartsManager.cpp:81-103).
    fn init_no_size(&mut self, part_size: usize, ready_parts: &[i32]) -> TdResult<()> {
        self.unknown_size_flag = true;
        self.size = 0;
        self.min_size = 0;
        self.max_size = i64::MAX;

        if part_size != 0 {
            self.part_size = part_size;
        } else {
            self.part_size = 32 << 10;
            while self.part_size < MAX_PART_SIZE
                && calc_part_count(self.expected_size, self.part_size) > i64::from(MAX_PART_COUNT)
            {
                self.part_size *= 2;
            }
            // just in case if expected_size is wrong
            if self.part_size < MAX_PART_SIZE {
                self.part_size *= 2;
            }
        }
        self.part_count = ready_parts
            .iter()
            .fold(0, |a, &b| a.max(b.wrapping_add(1)));

        self.init_common(ready_parts)
    }

    /// `PartsManager::init_common` (PartsManager.cpp:487-517).
    fn init_common(&mut self, ready_parts: &[i32]) -> TdResult<()> {
        self.ready_size = 0;
        self.streaming_ready_size = 0;
        self.pending_count = 0;
        self.first_empty_part = 0;
        self.first_not_ready_part = 0;
        self.part_status = vec![PartStatus::Empty; self.part_count as usize];

        for &i in ready_parts {
            if self.known_prefix_flag && i >= (self.known_prefix_size / self.part_size as i64) as i32
            {
                assert!(self.is_upload);
                return TdResult::from_status(Status::error_message("FILE_UPLOAD_RESTART"));
            }
            if self.is_upload && i >= self.part_count {
                return TdResult::from_status(Status::error_message("FILE_UPLOAD_RESTART"));
            }
            assert!(
                0 <= i && i < self.part_count,
                "[i:{}][real part_count:{}] {}",
                i,
                ready_parts.iter().fold(0i32, |a, &b| a.max(b.wrapping_add(1))),
                self
            );
            self.part_status[i as usize] = PartStatus::Ready;
            self.bitmask.set(i64::from(i));
            let part = self.get_part(i);
            self.ready_size += part.size as i64;
        }

        self.checked_prefix_size = i64::from(self.get_ready_prefix_count()) * self.part_size as i64;

        TdResult::Ok(())
    }

    /// `PartsManager::unchecked_ready` (PartsManager.cpp:145-150).
    pub fn unchecked_ready(&self) -> bool {
        !self.unknown_size_flag && self.ready_size == self.size
    }

    /// `PartsManager::may_finish` (PartsManager.cpp:152-157).
    pub fn may_finish(&mut self) -> bool {
        if self.is_streaming_limit_reached() {
            return true;
        }
        self.ready()
    }

    /// `PartsManager::ready` (PartsManager.cpp:159-161).
    pub fn ready(&self) -> bool {
        self.unchecked_ready() && (!self.need_check || self.checked_prefix_size == self.size)
    }

    /// `PartsManager::finish` (PartsManager.cpp:163-171).
    pub fn finish(&mut self) -> TdResult<()> {
        if self.ready() {
            return TdResult::Ok(());
        }
        if self.is_streaming_limit_reached() {
            return TdResult::from_status(Status::error_message("FILE_DOWNLOAD_LIMIT"));
        }
        TdResult::from_status(Status::error_message("File transferring not finished"))
    }

    /// `PartsManager::update_first_empty_part` (PartsManager.cpp:173-185).
    fn update_first_empty_part(&mut self) {
        while self.first_empty_part < self.part_count
            && self.part_status[self.first_empty_part as usize] != PartStatus::Empty
        {
            self.first_empty_part += 1;
        }

        if self.streaming_offset == 0 {
            self.first_streaming_empty_part = self.first_empty_part;
            return;
        }
        while self.first_streaming_empty_part < self.part_count
            && self.part_status[self.first_streaming_empty_part as usize] != PartStatus::Empty
        {
            self.first_streaming_empty_part += 1;
        }
    }

    /// `PartsManager::update_first_not_ready_part` (PartsManager.cpp:187-199).
    fn update_first_not_ready_part(&mut self) {
        while self.first_not_ready_part < self.part_count
            && self.part_status[self.first_not_ready_part as usize] == PartStatus::Ready
        {
            self.first_not_ready_part += 1;
        }
        if self.streaming_offset == 0 {
            self.first_streaming_not_ready_part = self.first_not_ready_part;
            return;
        }
        while self.first_streaming_not_ready_part < self.part_count
            && self.part_status[self.first_streaming_not_ready_part as usize] == PartStatus::Ready
        {
            self.first_streaming_not_ready_part += 1;
        }
    }

    /// `PartsManager::get_unchecked_ready_prefix_count` (PartsManager.cpp:201-204).
    pub fn get_unchecked_ready_prefix_count(&mut self) -> i32 {
        self.update_first_not_ready_part();
        self.first_not_ready_part
    }

    /// `PartsManager::get_ready_prefix_count` (PartsManager.cpp:206-215).
    pub fn get_ready_prefix_count(&mut self) -> i32 {
        let res = self.get_unchecked_ready_prefix_count();
        if self.need_check {
            let checked_parts = (self.checked_prefix_size / self.part_size as i64) as i32;
            if checked_parts < res {
                return checked_parts;
            }
        }
        res
    }

    /// `PartsManager::get_streaming_offset` (PartsManager.cpp:217-219).
    pub fn get_streaming_offset(&self) -> i64 {
        self.streaming_offset
    }

    /// `PartsManager::get_bitmask` (PartsManager.cpp:221-227) — the encoded
    /// ready mask, truncated to the checked prefix when checking is on.
    pub fn get_bitmask(&mut self) -> Vec<u8> {
        let mut prefix_count = -1;
        if self.need_check {
            prefix_count = (self.checked_prefix_size / self.part_size as i64) as i32;
        }
        self.bitmask.encode(prefix_count)
    }

    /// `PartsManager::is_part_in_streaming_limit` (PartsManager.cpp:229-252).
    fn is_part_in_streaming_limit(&self, part_id: i32) -> bool {
        assert!(part_id < self.part_count);
        let offset_begin = i64::from(part_id) * self.part_size as i64;
        let offset_end = offset_begin + self.get_part(part_id).size as i64;

        if self.streaming_limit == 0 {
            return true;
        }

        let is_intersect_with = |begin: i64, end: i64| {
            begin.max(offset_begin) < end.min(offset_end)
        };

        let streaming_begin = self.streaming_offset;
        let streaming_end = self.streaming_offset + self.streaming_limit;
        if is_intersect_with(streaming_begin, streaming_end) {
            return true;
        }
        // wrap limit
        if !self.unknown_size_flag
            && streaming_end > self.get_size()
            && is_intersect_with(0, streaming_end - self.get_size())
        {
            return true;
        }
        false
    }

    /// `PartsManager::is_streaming_limit_reached` (PartsManager.cpp:254-272).
    fn is_streaming_limit_reached(&mut self) -> bool {
        if self.streaming_limit == 0 {
            return false;
        }
        self.update_first_not_ready_part();
        let mut part_id = self.first_streaming_not_ready_part;

        // wrap
        if !self.unknown_size_flag && part_id == self.part_count {
            part_id = self.first_not_ready_part;
        }
        if part_id == self.part_count || !self.is_part_in_streaming_limit(part_id) {
            return self.get_estimated_extra() == 0;
        }
        false
    }

    /// `PartsManager::start_part` (PartsManager.cpp:274-308). Returns the
    /// empty part (`id == -1`) when there is nothing to request.
    pub fn start_part(&mut self) -> TdResult<Part> {
        self.update_first_empty_part();
        let mut part_id = self.first_streaming_empty_part;
        if self.known_prefix_flag
            && part_id >= (self.known_prefix_size / self.part_size as i64) as i32
        {
            return TdResult::from_status(Status::error(-1, "Wait for prefix to be known"));
        }
        if part_id == self.part_count {
            if self.unknown_size_flag {
                self.part_count += 1;
                let max_part_count = MAX_PART_COUNT_PREMIUM + if self.use_part_count_limit { 0 } else { 64 };
                if self.part_count > max_part_count {
                    if !self.is_upload {
                        // Caller will try to increase part size if it is possible
                        return TdResult::from_status(Status::error_message(
                            "FILE_DOWNLOAD_RESTART_INCREASE_PART_SIZE",
                        ));
                    }
                    return TdResult::from_status(Status::error_message(
                        "Too big file with unknown size",
                    ));
                }
                self.part_status.push(PartStatus::Empty);
            } else if self.first_empty_part < self.part_count {
                part_id = self.first_empty_part;
            } else {
                return TdResult::Ok(Self::get_empty_part());
            }
        }

        if !self.is_part_in_streaming_limit(part_id)
            || i64::from(part_id) * self.part_size as i64
                >= if self.unknown_size_flag {
                    self.max_size
                } else {
                    self.size
                }
        {
            return TdResult::Ok(Self::get_empty_part());
        }

        assert!(self.part_status[part_id as usize] == PartStatus::Empty);
        self.on_part_start(part_id);
        TdResult::Ok(self.get_part(part_id))
    }

    /// `PartsManager::set_known_prefix` (PartsManager.cpp:310-341).
    pub fn set_known_prefix(&mut self, size: i64, is_ready: bool) -> TdResult<()> {
        if !self.known_prefix_flag
            || size < self.known_prefix_size
            || (!is_ready && (size / self.part_size as i64) < self.part_status.len() as i64)
        {
            assert!(self.is_upload);
            return TdResult::from_status(Status::error_message("FILE_UPLOAD_RESTART"));
        }
        self.known_prefix_size = size;
        self.expected_size = self.known_prefix_size.max(self.expected_size);

        assert!(self.part_count as usize == self.part_status.len());
        if is_ready {
            self.part_count = calc_part_count(size, self.part_size) as i32;

            self.size = size;
            self.unknown_size_flag = false;
            self.known_prefix_flag = false;
            if self.streaming_limit != 0 {
                self.set_streaming_limit(self.streaming_limit);
            }
        } else {
            self.part_count = (size / self.part_size as i64) as i32;
        }

        assert!(
            self.part_count as usize >= self.part_status.len(),
            "{} {} {}",
            size,
            is_ready,
            self
        );
        self.part_status
            .resize(self.part_count as usize, PartStatus::Empty);
        if self.use_part_count_limit
            && self.part_size < MAX_PART_SIZE
            && calc_part_count(self.expected_size, self.part_size) > i64::from(MAX_PART_COUNT)
        {
            assert!(self.is_upload);
            return TdResult::from_status(Status::error_message("FILE_UPLOAD_RESTART"));
        }
        TdResult::Ok(())
    }

    /// `PartsManager::on_part_ok` (PartsManager.cpp:343-397).
    pub fn on_part_ok(&mut self, part_id: i32, part_size: usize, actual_size: usize) -> TdResult<()> {
        assert!(
            (part_id as usize) < self.part_status.len(),
            "{} {} {} {}",
            part_id,
            part_size,
            actual_size,
            self
        );
        assert!(
            self.part_status[part_id as usize] == PartStatus::Pending,
            "{} {:?} {} {} {}",
            part_id,
            self.part_status[part_id as usize],
            part_size,
            actual_size,
            self
        );
        self.pending_count -= 1;

        self.part_status[part_id as usize] = PartStatus::Ready;
        if actual_size != 0 {
            self.bitmask.set(i64::from(part_id));
        }
        self.ready_size += actual_size as i64;
        if self.streaming_limit > 0 && self.is_part_in_streaming_limit(part_id) {
            self.streaming_ready_size += actual_size as i64;
        }

        let offset = self.part_size as i64 * i64::from(part_id);
        let end_offset = offset + actual_size as i64;
        if self.unknown_size_flag {
            assert!(part_size == self.part_size);
            let mut is_size_changed = false;
            if actual_size < self.part_size && end_offset < self.max_size {
                self.max_size = end_offset;
                is_size_changed = true;
            }
            if actual_size != 0 {
                self.min_size = self.min_size.max(end_offset);
            }
            if self.min_size > self.max_size {
                return TdResult::from_status(Status::error_message(&format!(
                    "Failed to transfer file: [min_size:{}][max_size:{}]",
                    self.min_size, self.max_size
                )));
            } else if self.min_size == self.max_size {
                self.unknown_size_flag = false;
                self.size = self.min_size;
                is_size_changed = true;
            }

            if is_size_changed && self.streaming_limit != 0 {
                self.set_streaming_limit(self.streaming_limit);
            }
        } else if (actual_size < part_size && offset < self.size)
            || (offset >= self.size && actual_size > 0)
        {
            return TdResult::from_status(Status::error_message(&format!(
                "Failed to transfer file: [size:{}][offset:{}][transferred size:{}][part size:{}]",
                self.size, offset, actual_size, part_size
            )));
        }
        TdResult::Ok(())
    }

    /// `PartsManager::on_part_failed` (PartsManager.cpp:399-414).
    pub fn on_part_failed(&mut self, part_id: i32) {
        assert!(self.part_status[part_id as usize] == PartStatus::Pending);
        self.pending_count -= 1;
        self.part_status[part_id as usize] = PartStatus::Empty;
        if part_id < self.first_empty_part {
            self.first_empty_part = part_id;
        }
        if self.streaming_offset == 0 {
            self.first_streaming_empty_part = part_id;
            return;
        }
        let offset_part_id = (self.streaming_offset / self.part_size as i64) as i32;
        if part_id >= offset_part_id && part_id < self.first_streaming_empty_part {
            self.first_streaming_empty_part = part_id;
        }
    }

    /// `PartsManager::get_size` (PartsManager.cpp:416-419) — panics while
    /// the size is still unknown, exactly like the C++ CHECK.
    pub fn get_size(&self) -> i64 {
        assert!(!self.unknown_size_flag);
        self.size
    }

    /// `PartsManager::get_size_or_zero` (PartsManager.cpp:421-423).
    pub fn get_size_or_zero(&self) -> i64 {
        self.size
    }

    /// `PartsManager::get_estimated_extra` (PartsManager.cpp:425-466).
    pub fn get_estimated_extra(&self) -> i64 {
        let total_estimated_extra = self.get_expected_size() - self.get_ready_size();
        if self.streaming_limit != 0 {
            let part_size = self.part_size as i64;
            let round_up = |size: i64| (size + part_size - 1) / part_size * part_size;
            let mut streaming_begin = self.streaming_offset / part_size * part_size;
            let mut streaming_size = 0;
            if self.unknown_size_flag {
                streaming_begin = streaming_begin.min(self.max_size);
                let streaming_end =
                    round_up(self.streaming_offset + self.streaming_limit).min(self.max_size);
                streaming_size = streaming_end - streaming_begin;
            } else if self.streaming_offset + self.streaming_limit <= self.size {
                let streaming_end =
                    round_up(self.streaming_offset + self.streaming_limit).min(self.size);
                streaming_size = streaming_end - streaming_begin;
            } else if self.streaming_offset < self.size {
                let prefix =
                    round_up(self.streaming_offset + self.streaming_limit - self.size).min(self.size);
                let suffix = self.size - streaming_begin;
                streaming_size = self.size.min(prefix + suffix);
            }
            let mut res = streaming_size;

            //TODO(cpp): delete this block if CHECK won't fail
            let mut sub = 0;
            for part_id in 0..self.part_count {
                if self.is_part_in_streaming_limit(part_id)
                    && self.part_status[part_id as usize] == PartStatus::Ready
                {
                    sub += self.get_part(part_id).size as i64;
                }
            }
            assert!(sub == self.streaming_ready_size);

            res -= self.streaming_ready_size;
            assert!(res >= 0);
            return res;
        }
        total_estimated_extra
    }

    /// `PartsManager::get_ready_size` (PartsManager.cpp:468-470).
    pub fn get_ready_size(&self) -> i64 {
        self.ready_size
    }

    /// `PartsManager::get_expected_size` (PartsManager.cpp:472-477).
    pub fn get_expected_size(&self) -> i64 {
        if self.unknown_size_flag {
            return (self.min_size + (512 << 10))
                .max(self.get_ready_size() * 2)
                .min(self.max_size);
        }
        self.get_size()
    }

    /// `PartsManager::get_part_size` (PartsManager.cpp:479-481).
    pub fn get_part_size(&self) -> usize {
        self.part_size
    }

    /// `PartsManager::get_part_count` (PartsManager.cpp:483-485).
    pub fn get_part_count(&self) -> i32 {
        self.part_count
    }

    /// `PartsManager::set_need_check` (PartsManager.cpp:519-522).
    pub fn set_need_check(&mut self) {
        self.need_check = true;
        self.set_streaming_offset(0, 0);
    }

    /// `PartsManager::set_checked_prefix_size` (PartsManager.cpp:524-526).
    pub fn set_checked_prefix_size(&mut self, size: i64) {
        self.checked_prefix_size = size;
    }

    /// `PartsManager::get_checked_prefix_size` (PartsManager.cpp:528-530).
    pub fn get_checked_prefix_size(&self) -> i64 {
        self.checked_prefix_size
    }

    /// `PartsManager::get_unchecked_ready_prefix_size` (PartsManager.cpp:532-545).
    pub fn get_unchecked_ready_prefix_size(&mut self) -> i64 {
        self.update_first_not_ready_part();
        let count = self.first_not_ready_part;
        if count == 0 {
            return 0;
        }
        let part = self.get_part(count - 1);
        let mut res = part.offset;
        if !self.unknown_size_flag {
            res += part.size as i64;
            res = res.min(self.get_size());
        }
        res
    }

    /// `PartsManager::get_part` (PartsManager.cpp:547-557).
    fn get_part(&self, part_id: i32) -> Part {
        let mut size = self.part_size as i64;
        let offset = size * i64::from(part_id);
        let total_size = if self.unknown_size_flag {
            self.max_size
        } else {
            self.get_size()
        };
        if total_size < offset {
            size = 0;
        } else {
            size = size.min(total_size - offset);
        }
        Part {
            id: part_id,
            offset,
            size: size as usize,
        }
    }

    /// `PartsManager::get_empty_part` (PartsManager.cpp:559-561).
    fn get_empty_part() -> Part {
        Part {
            id: -1,
            offset: 0,
            size: 0,
        }
    }

    /// `PartsManager::on_part_start` (PartsManager.cpp:563-567).
    fn on_part_start(&mut self, part_id: i32) {
        assert!(self.part_status[part_id as usize] == PartStatus::Empty);
        self.part_status[part_id as usize] = PartStatus::Pending;
        self.pending_count += 1;
    }

    /// `PartsManager::set_streaming_offset` (PartsManager.cpp:34-66) —
    /// returns the first not-ready part of the streaming window.
    pub fn set_streaming_offset(&mut self, offset: i64, limit: i64) -> i32 {
        let invalid =
            offset < 0 || self.need_check || (!self.unknown_size_flag && self.get_size() < offset);
        let part_id = if invalid {
            0
        } else {
            (offset / self.part_size as i64) as i32
        };
        let too_many_parts =
            !invalid && self.use_part_count_limit && part_id >= MAX_PART_COUNT_PREMIUM;
        if invalid || too_many_parts {
            self.streaming_offset = 0;
        } else {
            self.streaming_offset = offset;
            self.first_streaming_empty_part = part_id;
            self.first_streaming_not_ready_part = part_id;
            if self.part_count < self.first_streaming_empty_part {
                self.part_count = self.first_streaming_empty_part;
                self.part_status
                    .resize(self.part_count as usize, PartStatus::Empty);
            }
        }

        self.set_streaming_limit(limit);
        self.update_first_not_ready_part();
        self.first_streaming_not_ready_part
    }

    /// `PartsManager::set_streaming_limit` (PartsManager.cpp:68-79).
    pub fn set_streaming_limit(&mut self, limit: i64) {
        self.streaming_limit = limit;
        self.streaming_ready_size = 0;
        if self.streaming_limit == 0 {
            return;
        }
        for part_id in 0..self.part_count {
            if self.is_part_in_streaming_limit(part_id)
                && self.part_status[part_id as usize] == PartStatus::Ready
            {
                self.streaming_ready_size += self.get_part(part_id).size as i64;
            }
        }
    }
}

impl fmt::Display for PartsManager {
    /// The `operator<<` (PartsManager.cpp:569-592); bools render as 0/1
    /// like the C++ stream insertion.
    #[allow(clippy::write_literal)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "PartsManager[{}load, need_check = {}, checked_prefix_size = {}, \
             known_prefix = {}, known_prefix_size = {}, size = {}, expected_size = {}, \
             min_size = {}, max_size = {}, unknown_size = {}, ready_size = {}, \
             streaming_ready_size = {}, part_size = {}, part_count = {}, \
             pending_count = {}, first_empty_part = {}, first_not_ready_part = {}, \
             streaming_offset = {}, streaming_limit = {}, \
             first_streaming_empty_part = {}, first_streaming_not_ready_part = {}, \
             use_part_count_limit = {}, part_status_count = {}: {}]",
            if self.is_upload { "up" } else { "down" },
            i32::from(self.need_check),
            self.checked_prefix_size,
            i32::from(self.known_prefix_flag),
            self.known_prefix_size,
            self.size,
            self.expected_size,
            self.min_size,
            self.max_size,
            i32::from(self.unknown_size_flag),
            self.ready_size,
            self.streaming_ready_size,
            self.part_size,
            self.part_count,
            self.pending_count,
            self.first_empty_part,
            self.first_not_ready_part,
            self.streaming_offset,
            self.streaming_limit,
            self.first_streaming_empty_part,
            self.first_streaming_not_ready_part,
            i32::from(self.use_part_count_limit),
            self.part_status.len(),
            self.bitmask
        )
    }
}

// ==== ResourceManager ====

/// `td::KHeap<int64>` (tdutils/td/utils/Heap.h:27-160) — the 4-ary
/// min-heap the ResourceManager schedules by. The C++ heap is intrusive
/// (`HeapNode::pos_`); here `fix`/`erase` find the entry by node id, and
/// the sift order is copied verbatim so equal keys pop in the same
/// sequence.
#[derive(Default)]
struct KHeap4 {
    array: Vec<(i64, u64)>,
}

impl KHeap4 {
    fn is_empty(&self) -> bool {
        self.array.is_empty()
    }

    /// `KHeap::insert` (Heap.h:58-62).
    fn insert(&mut self, key: i64, node_id: u64) {
        self.array.push((key, node_id));
        self.fix_up(self.array.len() - 1);
    }

    /// `KHeap::fix` (Heap.h:64-74).
    fn fix(&mut self, key: i64, node_id: u64) {
        let pos = self
            .array
            .iter()
            .position(|&(_, id)| id == node_id)
            .expect("KHeap::fix of a node that is not in the heap");
        let old_key = self.array[pos].0;
        self.array[pos].0 = key;
        if key < old_key {
            self.fix_up(pos);
        } else {
            self.fix_down(pos);
        }
    }

    /// `KHeap::erase` (Heap.h:76-83).
    fn erase(&mut self, node_id: u64) {
        let pos = self
            .array
            .iter()
            .position(|&(_, id)| id == node_id)
            .expect("KHeap::erase of a node that is not in the heap");
        self.erase_pos(pos);
    }

    /// `KHeap::pop` (Heap.h:50-56).
    fn pop(&mut self) -> u64 {
        let node_id = self.array[0].1;
        self.erase_pos(0);
        node_id
    }

    /// `KHeap::erase(size_t pos)` (Heap.h:154-163) — swap with the back,
    /// then re-sift both ways.
    fn erase_pos(&mut self, pos: usize) {
        self.array[pos] = self.array[self.array.len() - 1];
        self.array.pop();
        if pos < self.array.len() {
            self.fix_down(pos);
            self.fix_up(pos);
        }
    }

    /// `KHeap::fix_up` (Heap.h:132-148) — k = 4.
    fn fix_up(&mut self, mut pos: usize) {
        let item = self.array[pos];
        while pos != 0 {
            let parent_pos = (pos - 1) / 4;
            let parent_item = self.array[parent_pos];

            if parent_item.0 < item.0 {
                break;
            }

            self.array[pos] = parent_item;
            pos = parent_pos;
        }

        self.array[pos] = item;
    }

    /// `KHeap::fix_down` (Heap.h:150-175) — k = 4.
    fn fix_down(&mut self, mut pos: usize) {
        let item = self.array[pos];
        loop {
            let left_pos = pos * 4 + 1;
            let right_pos = (left_pos + 4).min(self.array.len());
            let mut next_pos = pos;
            let mut next_key = item.0;
            for i in left_pos..right_pos {
                let i_key = self.array[i].0;
                if i_key < next_key {
                    next_key = i_key;
                    next_pos = i;
                }
            }
            if next_pos == pos {
                break;
            }
            self.array[pos] = self.array[next_pos];
            pos = next_pos;
        }

        self.array[pos] = item;
    }
}

/// `ResourceManager::Mode` (ResourceManager.h:24).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceManagerMode {
    Baseline,
    Greedy,
}

/// The `FileLoaderActor` side of the worker link (the C++
/// `ActorShared<FileLoaderActor>`): the two closures the
/// [`ResourceManager`] sends. M.12.4's real loader implements this; the
/// unit tests fake it.
pub trait ResourceManagerWorker {
    /// `send_closure(callback, &FileLoaderActor::set_resource_manager, ...)`
    /// fired at the end of [`ResourceManager::register_worker`].
    fn set_resource_manager(&mut self, handle: ResourceManagerHandle);
    /// `send_closure(callback, &FileLoaderActor::update_resources, state)`.
    fn update_resources(&mut self, resource_state: ResourceState);
}

/// The worker→manager direction (the C++ actor link with the node id as
/// its link token). Single-threaded like the whole scheduler collapse, so
/// it is an `Rc` handle; calling back into the manager from inside a
/// manager-driven callback would panic on the borrow — the re-entrancy
/// C++ resolves through its message queue cannot occur in the collapse.
#[derive(Clone)]
pub struct ResourceManagerHandle {
    manager: Rc<RefCell<ResourceManager>>,
    node_id: u64,
}

impl ResourceManagerHandle {
    /// `ResourceManager::update_priority` (ResourceManager.cpp:34-43).
    pub fn update_priority(&self, priority: i8) {
        self.manager.borrow_mut().update_priority(self.node_id, priority);
    }

    /// `ResourceManager::update_resources` (ResourceManager.cpp:45-66).
    pub fn update_resources(&self, resource_state: &ResourceState) {
        self.manager
            .borrow_mut()
            .update_resources(self.node_id, resource_state);
    }

    /// `ResourceManager::hangup_shared` (ResourceManager.cpp:73-88) — the
    /// worker dropped its end of the link.
    pub fn hangup_shared(&self) {
        self.manager.borrow_mut().disconnect_worker(self.node_id);
    }
}

struct ResourceManagerNode {
    node_id: u64,
    resource_state: ResourceState,
    in_heap: bool,
    callback: Box<dyn ResourceManagerWorker>,
}

/// `td::ResourceManager` (files/ResourceManager.h) — the bandwidth
/// arbiter between concurrent loaders. The C++ actor shell collapses:
/// `get_link_token` becomes the explicit node id, `hangup`/`hangup_shared`
/// become [`ResourceManager::hangup`]/[`ResourceManager::disconnect_worker`],
/// and the actor's `stop()` (it has no more links) is observable as
/// `stop_flag` with an empty node set.
pub struct ResourceManager {
    max_resource_limit: i64,
    mode: ResourceManagerMode,

    /// The C++ `Container<unique_ptr<Node>>`: slots never shrink, ids are
    /// non-zero, freed slots are reused. The generation counter of the C++
    /// ids defended against stale async messages — impossible here, so ids
    /// are plain slot numbers.
    nodes: Vec<Option<ResourceManagerNode>>,
    free_slots: Vec<usize>,

    /// `(priority, node_id)`, ascending — the C++ `to_xload_`.
    to_xload: Vec<(i8, u64)>,
    by_estimated_extra: KHeap4,
    resource_state: ResourceState,

    stop_flag: bool,
}

impl ResourceManager {
    /// The `ResourceManager` constructor (ResourceManager.h:25-26).
    pub fn new(max_resource_limit: i64, mode: ResourceManagerMode) -> ResourceManager {
        ResourceManager {
            max_resource_limit,
            mode,
            nodes: Vec::new(),
            free_slots: Vec::new(),
            to_xload: Vec::new(),
            by_estimated_extra: KHeap4::default(),
            resource_state: ResourceState::default(),
            stop_flag: false,
        }
    }

    /// `ResourceManager::register_worker` (ResourceManager.cpp:21-32).
    /// Takes the shared handle so the `set_resource_manager` closure can
    /// carry it back to the worker.
    pub fn register_worker(
        this: &Rc<RefCell<ResourceManager>>,
        callback: Box<dyn ResourceManagerWorker>,
        priority: i8,
    ) {
        let node_id = this.borrow_mut().create_node(callback);
        this.borrow_mut().add_node(node_id, priority);
        // send_closure(node->callback_, &FileLoaderActor::set_resource_manager,
        //               actor_shared(this, node_id)) — sent with the manager
        // borrow released, so the worker may call straight back
        let handle = ResourceManagerHandle {
            manager: Rc::clone(this),
            node_id,
        };
        let mut node = this.borrow_mut().take_node(node_id);
        node.callback.set_resource_manager(handle);
        this.borrow_mut().put_node(node);
    }

    fn create_node(&mut self, callback: Box<dyn ResourceManagerWorker>) -> u64 {
        let node_id = if let Some(slot) = self.free_slots.pop() {
            self.nodes[slot] = Some(ResourceManagerNode {
                node_id: 0,
                resource_state: ResourceState::default(),
                in_heap: false,
                callback,
            });
            (slot + 1) as u64
        } else {
            self.nodes.push(Some(ResourceManagerNode {
                node_id: 0,
                resource_state: ResourceState::default(),
                in_heap: false,
                callback,
            }));
            self.nodes.len() as u64
        };
        self.node_mut(node_id).node_id = node_id;
        node_id
    }

    fn node(&self, node_id: u64) -> &ResourceManagerNode {
        self.nodes[(node_id - 1) as usize]
            .as_ref()
            .expect("ResourceManager: access of an erased node")
    }

    fn node_mut(&mut self, node_id: u64) -> &mut ResourceManagerNode {
        self.nodes
            .get_mut((node_id - 1) as usize)
            .and_then(Option::as_mut)
            .expect("ResourceManager: access of an erased node")
    }

    fn take_node(&mut self, node_id: u64) -> ResourceManagerNode {
        self.nodes[(node_id - 1) as usize]
            .take()
            .expect("ResourceManager: access of an erased node")
    }

    fn put_node(&mut self, node: ResourceManagerNode) {
        let slot = (node.node_id - 1) as usize;
        self.nodes[slot] = Some(node);
    }

    /// `ResourceManager::update_priority` (ResourceManager.cpp:34-43).
    pub fn update_priority(&mut self, node_id: u64, priority: i8) {
        if self.stop_flag {
            return;
        }
        if !self.remove_node(node_id) {
            return;
        }
        self.add_node(node_id, priority);
    }

    /// `ResourceManager::update_resources` (ResourceManager.cpp:45-66).
    pub fn update_resources(&mut self, node_id: u64, resource_state: &ResourceState) {
        if self.stop_flag {
            return;
        }
        if self.nodes.get((node_id - 1) as usize).and_then(Option::as_ref).is_none() {
            return;
        }
        let before = self.node(node_id).resource_state.clone();
        self.resource_state -= &before;
        self.node_mut(node_id).resource_state.update_master(resource_state);
        let after = self.node(node_id).resource_state.clone();
        self.resource_state += &after;

        if self.mode == ResourceManagerMode::Greedy {
            self.add_to_heap(node_id);
        }
        self.loop_();
    }

    /// `ResourceManager::hangup` (ResourceManager.cpp:68-71).
    pub fn hangup(&mut self) {
        self.stop_flag = true;
        self.loop_();
    }

    /// `ResourceManager::hangup_shared` (ResourceManager.cpp:73-88).
    pub fn disconnect_worker(&mut self, node_id: u64) {
        if self.nodes.get((node_id - 1) as usize).and_then(Option::as_ref).is_none() {
            return;
        }
        if self.node(node_id).in_heap {
            self.by_estimated_extra.erase(node_id);
            self.node_mut(node_id).in_heap = false;
        }
        let node_state = self.node(node_id).resource_state.clone();
        self.resource_state -= &node_state;
        self.remove_node(node_id);
        self.erase_node(node_id);
        self.loop_();
    }

    /// `ResourceManager::add_to_heap` (ResourceManager.cpp:90-104).
    fn add_to_heap(&mut self, node_id: u64) {
        let key = self.node(node_id).resource_state.estimated_extra();
        if self.node(node_id).in_heap {
            if key != 0 {
                self.by_estimated_extra.fix(key, node_id);
            } else {
                self.by_estimated_extra.erase(node_id);
                self.node_mut(node_id).in_heap = false;
            }
        } else if key != 0 {
            self.by_estimated_extra.insert(key, node_id);
            self.node_mut(node_id).in_heap = true;
        }
    }

    /// `ResourceManager::satisfy_node` (ResourceManager.cpp:106-130).
    fn satisfy_node(&mut self, node_id: u64) -> bool {
        let part_size = self.node(node_id).resource_state.unit_size() as i64;
        let mut need = self.node(node_id).resource_state.estimated_extra();
        need = (need + part_size - 1) / part_size * part_size;
        if need == 0 {
            return true;
        }
        let mut give = self.resource_state.unused();
        give = give.min(need);
        give -= give % part_size;
        if give == 0 {
            return false;
        }
        self.resource_state.start_use(give);
        let node = self.node_mut(node_id);
        node.resource_state.update_limit(give);
        node.callback.update_resources(node.resource_state.clone());
        true
    }

    /// `ResourceManager::loop` (ResourceManager.cpp:132-166).
    fn loop_(&mut self) {
        if self.stop_flag {
            // the C++ actor calls stop() once every node hung up; the
            // collapsed object just stays inert
            return;
        }
        let active_limit = self.resource_state.active_limit();
        self.resource_state
            .update_limit(self.max_resource_limit - active_limit);

        if self.mode == ResourceManagerMode::Greedy {
            let mut to_add = Vec::new();
            while !self.by_estimated_extra.is_empty() {
                let node_id = self.by_estimated_extra.pop();
                self.node_mut(node_id).in_heap = false;
                to_add.push(node_id);
                if !self.satisfy_node(node_id) {
                    break;
                }
            }
            for node_id in to_add {
                self.add_to_heap(node_id);
            }
        } else if self.mode == ResourceManagerMode::Baseline {
            // plain
            for i in 0..self.to_xload.len() {
                let file_node_id = self.to_xload[i].1;
                if !self.satisfy_node(file_node_id) {
                    break;
                }
            }
        }
    }

    /// `ResourceManager::add_node` (ResourceManager.cpp:168-176) —
    /// positive priorities order by the priority; negative ones are
    /// inverted (`-priority`) and inserted before equal keys, exactly like
    /// the C++ `find_if` positions.
    fn add_node(&mut self, node_id: u64, priority: i8) {
        let stored = (-i16::from(priority)) as i8;
        let pos = if priority >= 0 {
            self.to_xload
                .iter()
                .position(|&x| x.0 <= priority)
                .unwrap_or(self.to_xload.len())
        } else {
            self.to_xload
                .iter()
                .position(|&x| i16::from(x.0) < -i16::from(priority))
                .unwrap_or(self.to_xload.len())
        };
        self.to_xload.insert(pos, (stored, node_id));
    }

    /// `ResourceManager::remove_node` (ResourceManager.cpp:178-185).
    fn remove_node(&mut self, node_id: u64) -> bool {
        if let Some(pos) = self.to_xload.iter().position(|&x| x.1 == node_id) {
            self.to_xload.remove(pos);
            true
        } else {
            false
        }
    }

    fn erase_node(&mut self, node_id: u64) {
        let slot = (node_id - 1) as usize;
        self.nodes[slot] = None;
        self.free_slots.push(slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tddb::td_db::{Parameters, TdDb};

    /// A stand-in `FileReferenceManager` for the serialization tests: the
    /// id itself is the wire payload (the real 28-variant `FileSource`
    /// format lands with M.12.2a).
    struct TestFileSources;

    impl FileSourceStorage for TestFileSources {
        fn store_file_source(&mut self, file_source_id: FileSourceId, out: &mut Vec<u8>) {
            store_parse::store_i32(out, file_source_id.get());
        }

        fn parse_file_source(&mut self, parser: &mut Parser) -> Result<FileSourceId, &'static str> {
            Ok(FileSourceId::new(parser.i32_()?))
        }
    }

    #[test]
    fn file_id_compares_by_id_alone() {
        // the C++ `operator==`/`operator<`/`FileIdHash` cover the id only
        assert_eq!(FileId::new(1, 10), FileId::new(1, 20));
        assert!(FileId::new(1, 10) < FileId::new(2, 0));
        assert!(FileId::new(1, 10).is_valid());
        assert!(!FileId::new(0, 10).is_valid());
        assert!(FileId::new(-1, 10).empty());
        assert_eq!(FileId::new(7, 3).get(), 7);
        assert_eq!(FileId::new(7, 3).get_remote(), 3);
        assert_eq!(FileId::new(7, 3).to_string(), "7(3)");

        let mut set = std::collections::BTreeSet::new();
        set.insert(FileId::new(2, 0));
        set.insert(FileId::new(1, 0));
        assert_eq!(
            set.iter().map(|file_id| file_id.get()).collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn file_type_classification_matches_the_cpp_tables() {
        assert_eq!(get_file_type_class(FileType::Photo), FileTypeClass::Photo);
        assert_eq!(
            get_file_type_class(FileType::ProfilePhoto),
            FileTypeClass::Photo
        );
        assert_eq!(
            get_file_type_class(FileType::Wallpaper),
            FileTypeClass::Photo
        );
        assert_eq!(
            get_file_type_class(FileType::Document),
            FileTypeClass::Document
        );
        assert_eq!(
            get_file_type_class(FileType::Background),
            FileTypeClass::Document
        );
        assert_eq!(
            get_file_type_class(FileType::SecureEncrypted),
            FileTypeClass::Secure
        );
        assert_eq!(
            get_file_type_class(FileType::Encrypted),
            FileTypeClass::Encrypted
        );
        assert_eq!(get_file_type_class(FileType::Temp), FileTypeClass::Temp);
        assert!(is_document_file_type(FileType::Video));
        assert!(!is_document_file_type(FileType::Photo));

        assert_eq!(
            get_main_file_type(FileType::Wallpaper),
            FileType::Background
        );
        assert_eq!(
            get_main_file_type(FileType::SecureDecrypted),
            FileType::SecureEncrypted
        );
        assert_eq!(
            get_main_file_type(FileType::DocumentAsFile),
            FileType::Document
        );
        assert_eq!(get_main_file_type(FileType::CallLog), FileType::Document);
        assert_eq!(get_main_file_type(FileType::Video), FileType::Video);

        assert_eq!(get_file_type_name(FileType::Thumbnail), "thumbnails");
        assert_eq!(get_file_type_name(FileType::SelfDestructingPhoto), "photos");
        assert_eq!(get_file_type_name(FileType::VideoStory), "stories");
        assert_eq!(
            get_file_type_unique_name(FileType::VideoStory),
            "video_stories"
        );
        assert_eq!(get_file_type_unique_name(FileType::Photo), "photos");

        assert_eq!(get_file_dir_type(FileType::Photo), FileDirType::Common);
        assert_eq!(get_file_dir_type(FileType::Thumbnail), FileDirType::Secure);
        assert_eq!(get_file_dir_type(FileType::VideoStory), FileDirType::Secure);
        assert_eq!(get_file_dir_type(FileType::Video), FileDirType::Common);

        // is_file_big: photo class is never big; the special-cased small
        // types never are; the rest threshold at 10 MiB.
        assert!(!is_file_big(FileType::Photo, 1 << 40));
        assert!(!is_file_big(FileType::VideoNote, 1 << 40));
        assert!(!is_file_big(FileType::Document, 10 << 20));
        assert!(is_file_big(FileType::Document, (10 << 20) + 1));

        assert!(can_reuse_remote_file(FileType::Photo));
        assert!(!can_reuse_remote_file(FileType::Thumbnail));
        assert!(!can_reuse_remote_file(FileType::SelfDestructingVideo));
    }

    #[test]
    fn file_type_td_api_round_trip_covers_all_24_constructors() {
        let constructors = [
            td_api::FileType::fileTypeThumbnail(td_api::fileTypeThumbnail),
            td_api::FileType::fileTypeProfilePhoto(td_api::fileTypeProfilePhoto),
            td_api::FileType::fileTypePhoto(td_api::fileTypePhoto),
            td_api::FileType::fileTypePhotoStory(td_api::fileTypePhotoStory),
            td_api::FileType::fileTypeVoiceNote(td_api::fileTypeVoiceNote),
            td_api::FileType::fileTypeVideo(td_api::fileTypeVideo),
            td_api::FileType::fileTypeVideoStory(td_api::fileTypeVideoStory),
            td_api::FileType::fileTypeDocument(td_api::fileTypeDocument),
            td_api::FileType::fileTypeSecret(td_api::fileTypeSecret),
            td_api::FileType::fileTypeUnknown(td_api::fileTypeUnknown),
            td_api::FileType::fileTypeSticker(td_api::fileTypeSticker),
            td_api::FileType::fileTypeAudio(td_api::fileTypeAudio),
            td_api::FileType::fileTypeAnimation(td_api::fileTypeAnimation),
            td_api::FileType::fileTypeSecretThumbnail(td_api::fileTypeSecretThumbnail),
            td_api::FileType::fileTypeWallpaper(td_api::fileTypeWallpaper),
            td_api::FileType::fileTypeVideoNote(td_api::fileTypeVideoNote),
            td_api::FileType::fileTypeSecure(td_api::fileTypeSecure),
            td_api::FileType::fileTypeNotificationSound(td_api::fileTypeNotificationSound),
            td_api::FileType::fileTypeSelfDestructingPhoto(td_api::fileTypeSelfDestructingPhoto),
            td_api::FileType::fileTypeSelfDestructingVideo(td_api::fileTypeSelfDestructingVideo),
            td_api::FileType::fileTypeSelfDestructingVideoNote(
                td_api::fileTypeSelfDestructingVideoNote,
            ),
            td_api::FileType::fileTypeSelfDestructingVoiceNote(
                td_api::fileTypeSelfDestructingVoiceNote,
            ),
            td_api::FileType::fileTypeLivePhotoVideo(td_api::fileTypeLivePhotoVideo),
            td_api::FileType::fileTypeSelfDestructingLivePhotoVideo(
                td_api::fileTypeSelfDestructingLivePhotoVideo,
            ),
            td_api::FileType::fileTypeNone(td_api::fileTypeNone),
        ];
        for constructor in constructors {
            assert_eq!(
                get_file_type_object(get_file_type(&constructor)),
                constructor
            );
        }
        // the C++ asymmetries: wallpaper maps to Background on input, and
        // Background/DocumentAsFile/CallLog all render as fileTypeWallpaper
        // or fileTypeDocument on output
        assert_eq!(
            get_file_type(&td_api::FileType::fileTypeWallpaper(
                td_api::fileTypeWallpaper
            )),
            FileType::Background
        );
        assert_eq!(
            get_file_type_object(FileType::Background),
            td_api::FileType::fileTypeWallpaper(td_api::fileTypeWallpaper)
        );
        assert_eq!(
            get_file_type_object(FileType::CallLog),
            td_api::FileType::fileTypeDocument(td_api::fileTypeDocument)
        );
    }

    #[test]
    fn guess_file_type_by_path_matches_the_cpp_table() {
        assert_eq!(
            guess_file_type_by_path("a.jpg", FileType::None),
            FileType::Photo
        );
        assert_eq!(
            guess_file_type_by_path("a.jpeg", FileType::None),
            FileType::Photo
        );
        assert_eq!(
            guess_file_type_by_path("a.ogg", FileType::None),
            FileType::VoiceNote
        );
        assert_eq!(
            guess_file_type_by_path("a.opus", FileType::None),
            FileType::VoiceNote
        );
        assert_eq!(
            guess_file_type_by_path("a.3gp", FileType::None),
            FileType::Video
        );
        assert_eq!(
            guess_file_type_by_path("a.m4a", FileType::None),
            FileType::Audio
        );
        assert_eq!(
            guess_file_type_by_path("a.webm", FileType::None),
            FileType::Sticker
        );
        assert_eq!(
            guess_file_type_by_path("a.gif", FileType::None),
            FileType::Animation
        );
        assert_eq!(
            guess_file_type_by_path("a.mp4", FileType::None),
            FileType::Video
        );
        assert_eq!(
            guess_file_type_by_path("A-GIF-B.mp4", FileType::None),
            FileType::Animation
        );
        assert_eq!(
            guess_file_type_by_path("a.bin", FileType::None),
            FileType::Document
        );
        assert_eq!(
            guess_file_type_by_path("dir/a.jpg", FileType::None),
            FileType::Photo
        );
        // the default type wins, with the PhotoStory .mp4 exception on
        // the FULL path
        assert_eq!(
            guess_file_type_by_path("a.jpg", FileType::Audio),
            FileType::Audio
        );
        assert_eq!(
            guess_file_type_by_path("a.mp4", FileType::PhotoStory),
            FileType::VideoStory
        );
        assert_eq!(
            guess_file_type_by_path("a.jpg", FileType::PhotoStory),
            FileType::PhotoStory
        );
    }

    #[test]
    fn photo_size_source_round_trips() {
        let sources = [
            PhotoSizeSource::thumbnail(FileType::Photo, b'a' as i32),
            PhotoSizeSource::thumbnail(FileType::Document, 128 - 1),
            PhotoSizeSource::dialog_photo(DialogId::new(777000), 11, false),
            PhotoSizeSource::dialog_photo(DialogId::new(-1000000000001), 12, true),
            PhotoSizeSource::sticker_set_thumbnail(3, 4, 5),
            PhotoSizeSource::full_legacy(1, 2, 3),
            PhotoSizeSource::dialog_photo_legacy(DialogId::new(5), 6, false, 7, 8),
            PhotoSizeSource::dialog_photo_legacy(DialogId::new(5), 6, true, 7, 8),
            PhotoSizeSource::sticker_set_thumbnail_legacy(9, 10, 11, 12),
            PhotoSizeSource::sticker_set_thumbnail(13, 14, 15),
        ];
        for source in sources {
            let mut out = Vec::new();
            source.store(&mut out);
            let mut parser = Parser::new(&out);
            let parsed = PhotoSizeSource::parse_from(&mut parser).unwrap();
            parser.fetch_end().unwrap();
            assert_eq!(parsed, source);
            assert_eq!(parsed.get_type() as i32, source.get_type() as i32);
        }
    }

    #[test]
    fn photo_size_source_version_gates_reject_obsolete_variants() {
        // StickerSetThumbnail (offset 4) parses at v31 but is rejected at
        // the current version
        let mut out = Vec::new();
        store_parse::store_i32(&mut out, 4);
        store_parse::store_i64(&mut out, 1);
        store_parse::store_i64(&mut out, 2);
        let mut parser = Parser::new(&out);
        assert_eq!(
            PhotoSizeSource::parse_from(&mut parser),
            Err("Invalid photo size source stored")
        );
        let mut parser = Parser::new(&out);
        parser.set_version(store_parse::version::REMOVE_PHOTO_VOLUME_AND_LOCAL_ID - 1);
        let parsed = PhotoSizeSource::parse_from(&mut parser).unwrap();
        assert_eq!(parsed.get_type(), PhotoSizeSourceType::StickerSetThumbnail);

        // Legacy (offset 0) likewise
        let mut out = Vec::new();
        store_parse::store_i32(&mut out, 0);
        store_parse::store_i64(&mut out, 7);
        let mut parser = Parser::new(&out);
        assert_eq!(
            PhotoSizeSource::parse_from(&mut parser),
            Err("Invalid photo size source stored")
        );
        let mut parser = Parser::new(&out);
        parser.set_version(store_parse::version::REMOVE_PHOTO_VOLUME_AND_LOCAL_ID - 1);
        assert_eq!(
            PhotoSizeSource::parse_from(&mut parser).unwrap(),
            PhotoSizeSource::Legacy { secret: 7 }
        );
    }

    #[test]
    fn photo_size_source_rejects_bad_thumbnail_types_and_local_ids() {
        let mut out = Vec::new();
        store_parse::store_i32(&mut out, 1); // Thumbnail
        store_parse::store_i32(&mut out, 100); // file type out of range
        let mut parser = Parser::new(&out);
        assert_eq!(
            PhotoSizeSource::parse_from(&mut parser),
            Err("Wrong file type in PhotoSizeSource::Thumbnail")
        );

        let mut out = Vec::new();
        store_parse::store_i32(&mut out, 6); // DialogPhotoSmallLegacy
        store_parse::store_i64(&mut out, 777000); // dialog id
        store_parse::store_i64(&mut out, 0); // access hash
        store_parse::store_i64(&mut out, 1); // volume id
        store_parse::store_i32(&mut out, -1); // negative local id
        let mut parser = Parser::new(&out);
        assert_eq!(
            PhotoSizeSource::parse_from(&mut parser),
            Err("Wrong local_id")
        );

        // secret-chat dialogs are rejected in the dialog parse
        let mut out = Vec::new();
        store_parse::store_i32(&mut out, 2); // DialogPhotoSmall
        store_parse::store_i64(&mut out, DialogId::ZERO_SECRET_CHAT_ID - 5);
        store_parse::store_i64(&mut out, 0);
        let mut parser = Parser::new(&out);
        assert_eq!(
            PhotoSizeSource::parse_from(&mut parser),
            Err("Invalid chat identifier in PhotoSizeSource")
        );
    }

    #[test]
    fn photo_size_source_unique_representations() {
        // compare types: DialogPhotoSmall = 0, DialogPhotoBig = 1,
        // StickerSetThumbnailVersion = 2, legacy family = 3
        let small = PhotoSizeSource::dialog_photo(DialogId::new(1), 0, false);
        let big = PhotoSizeSource::dialog_photo(DialogId::new(1), 0, true);
        assert!(PhotoSizeSource::unique_less(&small, &big));
        assert_eq!(small.get_unique(), vec![0]);
        assert_eq!(big.get_unique(), vec![1]);

        let full_legacy = PhotoSizeSource::full_legacy(0x1122334455667788, 0x0a0b0c0d, 9);
        // compare_type 3 emits no type byte — just volume then local id
        // (PhotoSizeSource.cpp:74-78)
        assert_eq!(
            full_legacy.get_unique(),
            [0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x0d, 0x0c, 0x0b, 0x0a]
        );
        assert!(PhotoSizeSource::unique_equal(
            &full_legacy,
            &PhotoSizeSource::full_legacy(0x1122334455667788, 0x0a0b0c0d, 42)
        ));

        let version = PhotoSizeSource::sticker_set_thumbnail(5, 6, 7);
        assert_eq!(
            version.get_unique(),
            [2, 5, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0]
        );

        assert_eq!(small.get_unique_name(42), "42");
        assert_eq!(big.get_unique_name(42), "42_1");
        assert_eq!(
            PhotoSizeSource::thumbnail(FileType::Photo, b'm' as i32).get_unique_name(42),
            "42_109"
        );
        assert_eq!(version.get_unique_name(0), "5_7");
    }

    #[test]
    fn photo_remote_location_parses_legacy_forms() {
        // pre-AddPhotoSizeSource: id, access_hash, volume, secret, local
        let mut out = Vec::new();
        store_parse::store_i64(&mut out, 5);
        store_parse::store_i64(&mut out, 6);
        store_parse::store_i64(&mut out, 7);
        store_parse::store_i64(&mut out, 8);
        store_parse::store_i32(&mut out, 9);
        let mut parser = Parser::new(&out);
        parser.set_version(store_parse::version::ADD_PHOTO_SIZE_SOURCE - 1);
        let location = PhotoRemoteFileLocation::parse_from(&mut parser).unwrap();
        assert_eq!(location.source, PhotoSizeSource::full_legacy(7, 9, 8));

        // AddPhotoSizeSource..RemovePhotoVolumeAndLocalId: a source is
        // present and a DialogPhoto is repaired into the legacy form
        let mut out = Vec::new();
        store_parse::store_i64(&mut out, 5);
        store_parse::store_i64(&mut out, 6);
        store_parse::store_i64(&mut out, 7);
        let source = PhotoSizeSource::dialog_photo(DialogId::new(10), 11, true);
        source.store(&mut out);
        store_parse::store_i32(&mut out, 12);
        let mut parser = Parser::new(&out);
        parser.set_version(store_parse::version::REMOVE_PHOTO_VOLUME_AND_LOCAL_ID - 1);
        let location = PhotoRemoteFileLocation::parse_from(&mut parser).unwrap();
        assert_eq!(
            location.source,
            PhotoSizeSource::dialog_photo_legacy(DialogId::new(10), 11, true, 7, 12)
        );

        // the modern path stores the source directly
        let location = PhotoRemoteFileLocation {
            id: 5,
            access_hash: 6,
            source: PhotoSizeSource::thumbnail(FileType::Photo, b'a' as i32),
        };
        let mut out = Vec::new();
        location.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = PhotoRemoteFileLocation::parse_from(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed, location);
    }

    #[test]
    fn full_remote_location_round_trips() {
        let web = FullRemoteFileLocation::from_web(
            FileType::Document,
            "https://example.com/a".to_owned(),
            7,
        );
        let photo = FullRemoteFileLocation::from_photo(
            PhotoSizeSource::thumbnail(FileType::Photo, b'a' as i32),
            8,
            9,
            DcId::internal(2),
            "ref".to_owned(),
        );
        let common = FullRemoteFileLocation::from_document(
            FileType::Document,
            10,
            11,
            DcId::internal(2),
            String::new(),
        );
        for location in [web, photo, common] {
            let mut out = Vec::new();
            location.store(&mut out);
            let mut parser = Parser::new(&out);
            let parsed = FullRemoteFileLocation::parse_from(&mut parser).unwrap();
            parser.fetch_end().unwrap();
            assert_eq!(parsed, location);
        }

        // is_web/is_photo/is_common classification and accessors
        let web = FullRemoteFileLocation::from_web(
            FileType::Document,
            "https://example.com/a".to_owned(),
            7,
        );
        assert!(web.is_web());
        assert!(!web.is_common());
        assert!(!web.is_document());
        assert_eq!(web.get_access_hash(), 7);
        assert_eq!(web.get_url(), "https://example.com/a");
        let photo = FullRemoteFileLocation::from_photo(
            PhotoSizeSource::thumbnail(FileType::Photo, b'a' as i32),
            8,
            9,
            DcId::internal(2),
            String::new(),
        );
        assert!(photo.is_photo());
        assert_eq!(photo.get_dc_id(), DcId::internal(2));
        assert_eq!(photo.get_id(), 8);
        assert_eq!(photo.get_access_hash(), 9);
        assert_eq!(
            photo.get_source(),
            PhotoSizeSource::thumbnail(FileType::Photo, b'a' as i32)
        );
        let common = FullRemoteFileLocation::from_document(
            FileType::Encrypted,
            10,
            11,
            DcId::internal(2),
            String::new(),
        );
        assert!(common.is_encrypted_secret());
        assert!(common.is_encrypted_any());
        let secure = FullRemoteFileLocation::from_document(
            FileType::SecureEncrypted,
            10,
            11,
            DcId::internal(2),
            String::new(),
        );
        assert!(secure.is_secure());
        assert!(secure.is_encrypted_any());
    }

    #[test]
    fn full_remote_location_parse_validates() {
        // out-of-range file type
        let mut out = Vec::new();
        store_parse::store_i32(&mut out, 1000);
        let mut parser = Parser::new(&out);
        assert_eq!(
            FullRemoteFileLocation::parse_from(&mut parser),
            Err("Invalid FileType in FullRemoteFileLocation")
        );

        // photo location whose thumbnail source disagrees with the stored
        // photo-class file type (the blob's type byte must be photo-class
        // for the Photo branch to be taken at all)
        let source = PhotoSizeSource::thumbnail(FileType::Document, b'a' as i32);
        let mut out = Vec::new();
        let type_ = FileType::Photo as i32;
        store_parse::store_i32(&mut out, type_);
        store_parse::store_i32(&mut out, 2);
        store_parse::store_i64(&mut out, 8);
        store_parse::store_i64(&mut out, 9);
        source.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(
            FullRemoteFileLocation::parse_from(&mut parser),
            Err("Invalid FileType in PhotoRemoteFileLocation Thumbnail")
        );

        // temp-class file type is invalid in a full location
        let mut out = Vec::new();
        store_parse::store_i32(&mut out, FileType::Temp as i32);
        store_parse::store_i32(&mut out, 2);
        let mut parser = Parser::new(&out);
        assert_eq!(
            FullRemoteFileLocation::parse_from(&mut parser),
            Err("Invalid FileType in FullRemoteFileLocation")
        );
    }

    #[test]
    fn full_remote_location_handles_file_references() {
        let mut location = FullRemoteFileLocation::from_document(
            FileType::Document,
            10,
            11,
            DcId::internal(2),
            "abc".to_owned(),
        );
        assert!(location.has_file_reference());
        assert!(location.delete_file_reference(b"abc"));
        assert!(!location.has_file_reference());
        // the delete leaves the invalid-file-reference placeholder
        assert_eq!(location.get_file_reference(), "#");
        assert!(!location.delete_file_reference(b"abc"));

        // the "#" placeholder never survives a store/parse round trip
        let mut location = FullRemoteFileLocation::from_document(
            FileType::Document,
            10,
            11,
            DcId::internal(2),
            "#".to_owned(),
        );
        location.check_file_reference();
        // the C++ `has_file_reference` only compares against the "#"
        // placeholder (FileLocation.h:339-341) — a cleared reference
        // still answers true
        assert!(location.has_file_reference());
        assert_eq!(location.get_file_reference(), "");
        let mut out = Vec::new();
        location.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = FullRemoteFileLocation::parse_from(&mut parser).unwrap();
        assert_eq!(parsed.get_file_reference(), "");
    }

    #[test]
    fn full_remote_location_ordering_is_variant_then_type_then_dc() {
        let web = FullRemoteFileLocation::from_web(FileType::Document, "a".to_owned(), 0);
        let photo = FullRemoteFileLocation::from_photo(
            PhotoSizeSource::thumbnail(FileType::Photo, b'a' as i32),
            8,
            9,
            DcId::internal(2),
            String::new(),
        );
        let common = FullRemoteFileLocation::from_document(
            FileType::Document,
            10,
            11,
            DcId::internal(2),
            String::new(),
        );
        let mut sorted = vec![common.clone(), web.clone(), photo.clone()];
        sorted.sort();
        assert_eq!(sorted, vec![web, photo, common]);

        // photo locations order by id, then by the unique source
        let photo1 = FullRemoteFileLocation::from_photo(
            PhotoSizeSource::thumbnail(FileType::Photo, b'a' as i32),
            8,
            0,
            DcId::internal(2),
            String::new(),
        );
        let photo2 = FullRemoteFileLocation::from_photo(
            PhotoSizeSource::thumbnail(FileType::Photo, b'a' as i32),
            9,
            0,
            DcId::internal(2),
            String::new(),
        );
        assert!(photo1 < photo2);
        // equality ignores the access hash (the C++ operator==)
        let photo_a = FullRemoteFileLocation::from_photo(
            PhotoSizeSource::thumbnail(FileType::Photo, b'a' as i32),
            8,
            0,
            DcId::internal(2),
            String::new(),
        );
        assert_eq!(photo1, photo_a);
    }

    #[test]
    fn partial_local_location_legacy_bitmask_and_big_part_sizes() {
        // a legacy blob with the all-ones deprecated count
        let mut out = Vec::new();
        store_parse::store_i32(&mut out, FileType::Document as i32);
        store_parse::store_string(&mut out, "/tmp/a");
        store_parse::store_i32(&mut out, 524288); // 512 KiB part size
        store_parse::store_i32(&mut out, 3); // 3 ready parts
        store_parse::store_string(&mut out, "iv");
        let mut parser = Parser::new(&out);
        let parsed = PartialLocalFileLocation::parse_from(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed.part_size, 524288);
        assert_eq!(parsed.ready_bitmask, [0x07]);

        // the modern -2 path carries the high 31 bits
        let location = PartialLocalFileLocation {
            file_type: FileType::Document,
            path: "/tmp/a".to_owned(),
            part_size: (1i64 << 40) + 5,
            iv: "iv".to_owned(),
            ready_bitmask: vec![0xff, 1],
            ready_size: 0,
        };
        let mut out = Vec::new();
        location.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = PartialLocalFileLocation::parse_from(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed, location);

        // invalid file types are rejected
        let mut out = Vec::new();
        store_parse::store_i32(&mut out, FileType::Size as i32);
        let mut parser = Parser::new(&out);
        assert_eq!(
            PartialLocalFileLocation::parse_from(&mut parser),
            Err("Invalid type in PartialLocalFileLocation")
        );
    }

    #[test]
    fn location_variants_round_trip() {
        let remotes = [
            RemoteFileLocation::Empty,
            RemoteFileLocation::Partial(PartialRemoteFileLocation {
                file_id: 1,
                part_count: 2,
                part_size: 3,
                ready_part_count: 4,
                is_big: 1,
                ready_size: 5,
            }),
            RemoteFileLocation::Full(FullRemoteFileLocation::from_document(
                FileType::Document,
                10,
                11,
                DcId::internal(2),
                String::new(),
            )),
        ];
        for remote in remotes {
            let mut out = Vec::new();
            remote.store(&mut out);
            let mut parser = Parser::new(&out);
            let parsed = RemoteFileLocation::parse_from(&mut parser).unwrap();
            parser.fetch_end().unwrap();
            let mut expected = remote.clone();
            if let RemoteFileLocation::Partial(partial) = &mut expected {
                // `ready_size` is not part of the C++ store/parse
                // (FileLocation.hpp:23-39); it parses back as zero
                partial.ready_size = 0;
            }
            assert_eq!(parsed, expected);
        }
        // the C++ `is_empty` collapse: Empty equals a Partial with zero
        // ready parts
        assert_eq!(
            RemoteFileLocation::Empty,
            RemoteFileLocation::Partial(PartialRemoteFileLocation::default())
        );
        assert_ne!(
            RemoteFileLocation::Empty,
            RemoteFileLocation::Partial(PartialRemoteFileLocation {
                ready_part_count: 1,
                ..Default::default()
            })
        );

        let locals = [
            LocalFileLocation::Empty,
            LocalFileLocation::Partial(PartialLocalFileLocation {
                file_type: FileType::Document,
                path: "/tmp/a".to_owned(),
                part_size: 524288,
                iv: "iv".to_owned(),
                ready_bitmask: vec![0x07],
                ready_size: 0,
            }),
            LocalFileLocation::Full(FullLocalFileLocation::new(
                FileType::Document,
                "/tmp/b".to_owned(),
                7,
            )),
        ];
        for local in locals {
            let mut out = Vec::new();
            local.store(&mut out);
            let mut parser = Parser::new(&out);
            let parsed = LocalFileLocation::parse_from(&mut parser).unwrap();
            parser.fetch_end().unwrap();
            assert_eq!(parsed, local);
        }
        assert_eq!(
            LocalFileLocation::new(FileType::Document, "/tmp/b".to_owned(), 7).file_name(),
            "/tmp/b"
        );
        assert_eq!(LocalFileLocation::Empty.file_name(), "");

        let generates = [
            GenerateFileLocation::Empty,
            GenerateFileLocation::Full(FullGenerateFileLocation::new(
                FileType::Photo,
                "/tmp/c".to_owned(),
                "crop".to_owned(),
            )),
        ];
        for generate in generates {
            let mut out = Vec::new();
            generate.store(&mut out);
            let mut parser = Parser::new(&out);
            let parsed = GenerateFileLocation::parse_from(&mut parser).unwrap();
            parser.fetch_end().unwrap();
            assert_eq!(parsed, generate);
        }
        let mut out = Vec::new();
        store_parse::store_i32(&mut out, 7);
        let mut parser = Parser::new(&out);
        assert_eq!(
            GenerateFileLocation::parse_from(&mut parser),
            Err("Invalid type in GenerateFileLocation")
        );
    }

    #[test]
    fn full_local_location_orders_by_mtime_then_type_then_path() {
        let a = FullLocalFileLocation::new(FileType::Document, "/a".to_owned(), 1);
        let b = FullLocalFileLocation::new(FileType::Document, "/b".to_owned(), 1);
        let c = FullLocalFileLocation::new(FileType::Document, "/a".to_owned(), 2);
        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn encryption_key_shapes_and_fingerprint() {
        let key = [1u8; 32];
        let iv = [2u8; 32];
        let encryption_key = FileEncryptionKey::new(&key, &iv);
        assert!(encryption_key.is_secret());
        assert_eq!(encryption_key.size(), 64);
        assert_eq!(encryption_key.key_slice(), key);
        assert_eq!(encryption_key.iv_slice(), iv);
        assert_eq!(encryption_key.key(), &key);

        // the fingerprint is the first-two-words xor of the md5
        let digest = tdutils::crypto::md5(&[key.as_slice(), iv.as_slice()].concat());
        let expected = i32::from_le_bytes(digest[..4].try_into().unwrap())
            ^ i32::from_le_bytes(digest[4..8].try_into().unwrap());
        assert_eq!(encryption_key.calc_fingerprint(), expected);

        // wrong sizes collapse to NoKey
        let encryption_key = FileEncryptionKey::new(&key, &[1u8; 31]);
        assert!(!encryption_key.is_secret());
        assert!(encryption_key.empty());

        // store/parse round trip preserves the raw bytes (including
        // non-UTF-8 material)
        let encryption_key = FileEncryptionKey::new(&[0xffu8; 32], &[0u8; 32]);
        let mut out = Vec::new();
        encryption_key.store(&mut out);
        let mut parser = Parser::new(&out);
        let mut parsed = FileEncryptionKey::default();
        parsed
            .parse_from(FileEncryptionKeyType::Secret, &mut parser)
            .unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed, encryption_key);

        let mut parser = Parser::new(&out);
        let mut parsed = FileEncryptionKey::default();
        parsed
            .parse_from(FileEncryptionKeyType::Secure, &mut parser)
            .unwrap();
        assert!(parsed.is_secure());
    }

    #[test]
    fn bitmask_ones_encode_is_the_zero_one_escaping() {
        assert_eq!(Bitmask::from_ones(0).encode(-1), Vec::<u8>::new());
        assert_eq!(Bitmask::from_ones(3).encode(-1), [0x07]);
        assert_eq!(Bitmask::from_ones(8).encode(-1), [0xff, 1]);
        assert_eq!(Bitmask::from_ones(9).encode(-1), [0xff, 1, 0x01]);
        assert_eq!(Bitmask::from_ones(24).encode(-1), [0xff, 3]);
        assert_eq!(Bitmask::from_ones(16).encode(-1), [0xff, 2]);
    }

    #[test]
    fn file_data_empty_blob_pins_the_cpp_layout() {
        let mut out = Vec::new();
        FileData::default().store(&mut out, &mut TestFileSources);
        assert_eq!(
            out,
            [
                0x10, 0, 0, 0, // flags: has_version only
                61, 0, 0, 0, // the stored Version
                0, 0, 0, 0, 0, 0, 0, 0, // pmc_id
                0, 0, 0, 0, // empty remote
                0, 0, 0, 0, // empty local
                0, 0, 0, 0, // empty generate
                0, 0, 0, 0, 0, 0, 0, 0, // size
                0, 0, 0, 0, // remote_name
                0, 0, 0, 0, // url
                0, 0, 0, 0, // empty encryption key
            ]
        );

        let mut parser = Parser::new(&out);
        let parsed = FileData::parse_from(&mut parser, true, Some(&mut TestFileSources)).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed.pmc_id, 0);
        assert_eq!(parsed.remote, RemoteFileLocation::Empty);
        assert_eq!(parsed.local, LocalFileLocation::Empty);
        assert_eq!(parsed.generate, None);
        assert_eq!(parsed.size, 0);
        assert_eq!(parsed.expected_size, 0);
    }

    #[test]
    fn file_data_round_trips_all_fields() {
        let owner = DialogId::new(777000);
        let data = FileData {
            owner_dialog_id: owner,
            pmc_id: 42,
            remote: RemoteFileLocation::Full(FullRemoteFileLocation::from_document(
                FileType::Document,
                123456,
                654321,
                DcId::internal(2),
                "abc".to_owned(),
            )),
            local: LocalFileLocation::Full(FullLocalFileLocation::new(
                FileType::Document,
                "/tmp/a.txt".to_owned(),
                456,
            )),
            generate: Some(FullGenerateFileLocation::new(
                FileType::Photo,
                "/tmp/x.jpg".to_owned(),
                "crop".to_owned(),
            )),
            size: 0,
            expected_size: 100,
            remote_name: "a.txt".to_owned(),
            url: "https://example.com".to_owned(),
            encryption_key: FileEncryptionKey::new(&[1u8; 32], &[2u8; 32]),
            file_source_ids: vec![FileSourceId::new(1), FileSourceId::new(2)],
        };
        let mut out = Vec::new();
        data.store(&mut out, &mut TestFileSources);
        let mut parser = Parser::new(&out);
        let parsed = FileData::parse_from(&mut parser, true, Some(&mut TestFileSources)).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed.owner_dialog_id, owner);
        assert_eq!(parsed.pmc_id, 42);
        assert_eq!(parsed.remote, data.remote);
        assert_eq!(parsed.local, data.local);
        assert_eq!(parsed.generate, data.generate);
        assert_eq!(parsed.size, 0);
        assert_eq!(parsed.expected_size, 100);
        assert_eq!(parsed.remote_name, "a.txt");
        assert_eq!(parsed.url, "https://example.com");
        assert_eq!(parsed.encryption_key, data.encryption_key);
        assert_eq!(parsed.file_source_ids, data.file_source_ids);

        // registering is optional: the C++ gate skips the source bytes
        // without consuming them (FileData.hpp:128-149), so the leftovers
        // fail `fetch_end`
        let mut parser = Parser::new(&out);
        let parsed = FileData::parse_from(&mut parser, false, None).unwrap();
        assert!(parsed.file_source_ids.is_empty());
        assert_eq!(
            parser.fetch_end(),
            Err("Extra data in end of parsed string")
        );

        // duplicate ids collapse (the C++ `td::contains` gate)
        let data = FileData {
            file_source_ids: vec![FileSourceId::new(1), FileSourceId::new(1)],
            ..Default::default()
        };
        let mut out = Vec::new();
        data.store(&mut out, &mut TestFileSources);
        let mut parser = Parser::new(&out);
        let parsed = FileData::parse_from(&mut parser, true, Some(&mut TestFileSources)).unwrap();
        assert_eq!(parsed.file_source_ids, vec![FileSourceId::new(1)]);
    }

    #[test]
    fn file_data_rejects_hostile_source_counts() {
        // a source count of 5 must be rejected before any allocation
        let mut out = Vec::new();
        store_parse::store_flags(&mut out, [false, false, false, true, true]);
        store_parse::store_i32(&mut out, store_parse::version::LOG_EVENT_STORE_VERSION);
        store_parse::store_u64(&mut out, 0);
        RemoteFileLocation::Empty.store(&mut out);
        LocalFileLocation::Empty.store(&mut out);
        GenerateFileLocation::Empty.store(&mut out);
        store_parse::store_i64(&mut out, 0);
        store_parse::store_string(&mut out, "");
        store_parse::store_string(&mut out, "");
        FileEncryptionKey::default().store(&mut out);
        store_parse::store_i32(&mut out, 5);
        let mut parser = Parser::new(&out);
        assert_eq!(
            FileData::parse_from(&mut parser, true, Some(&mut TestFileSources)),
            Err("Wrong number of file source identifiers")
        );
    }

    struct FileDbEnv {
        db: TdDb,
        dir: String,
    }

    impl FileDbEnv {
        fn new(name: &str) -> FileDbEnv {
            // Unique per test thread: parallel tests in one process would
            // otherwise flock the same database against each other.
            let dir = std::env::temp_dir()
                .join(format!(
                    "td-rs-m12-{name}-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ))
                .to_string_lossy()
                .into_owned();
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let opened = TdDb::open(Parameters {
                database_directory: dir.clone(),
                ..Default::default()
            })
            .unwrap();
            FileDbEnv {
                db: opened.database,
                dir,
            }
        }
    }

    impl Drop for FileDbEnv {
        fn drop(&mut self) {
            self.db.close();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn sample_file_data() -> FileData {
        FileData {
            remote: RemoteFileLocation::Full(FullRemoteFileLocation::from_document(
                FileType::Document,
                123456,
                654321,
                DcId::internal(2),
                "abc".to_owned(),
            )),
            local: LocalFileLocation::Full(FullLocalFileLocation::new(
                FileType::Document,
                "/tmp/a.txt".to_owned(),
                456,
            )),
            generate: Some(FullGenerateFileLocation::new(
                FileType::Photo,
                "/tmp/x.jpg".to_owned(),
                "crop".to_owned(),
            )),
            size: 10,
            remote_name: "a.txt".to_owned(),
            encryption_key: FileEncryptionKey::new(&[1u8; 32], &[2u8; 32]),
            ..Default::default()
        }
    }

    #[test]
    fn file_db_set_get_clear_by_location_keys() {
        let mut env = FileDbEnv::new("set-get");
        let mut file_db = FileDb::new(env.db.files_kv());
        assert_eq!(file_db.get_next_file_db_id(), FileDbId::new(1));

        let data = sample_file_data();
        let id = file_db.get_next_file_db_id();
        file_db.set_file_data(
            env.db.files_kv(),
            id,
            &data,
            true,
            true,
            true,
            &mut TestFileSources,
        );

        let remote_key = match &data.remote {
            RemoteFileLocation::Full(full) => full.as_key(),
            _ => unreachable!(),
        };
        let local_key = match &data.local {
            LocalFileLocation::Full(full) => full.as_key(),
            _ => unreachable!(),
        };
        let generate_key = data.generate.as_ref().unwrap().as_key();
        assert_eq!(
            env.db.files_kv().get(&remote_key),
            id.get().to_string().as_bytes()
        );
        assert_eq!(
            env.db.files_kv().get(&local_key),
            id.get().to_string().as_bytes()
        );
        assert_eq!(
            env.db.files_kv().get(&generate_key),
            id.get().to_string().as_bytes()
        );

        let loaded = file_db
            .get_file_data_sync(env.db.files_kv(), &remote_key, Some(&mut TestFileSources))
            .unwrap();
        assert_eq!(loaded.remote, data.remote);
        assert_eq!(loaded.local, data.local);
        assert_eq!(loaded.generate, data.generate);
        assert_eq!(loaded.size, 10);
        assert_eq!(loaded.remote_name, "a.txt");
        assert_eq!(loaded.encryption_key, data.encryption_key);

        // missing keys report the C++ error
        assert_eq!(
            file_db.get_file_data_sync(
                env.db.files_kv(),
                b"no-such-key",
                Some(&mut TestFileSources)
            ),
            Err("There is no such key in the database")
        );

        file_db.clear_file_data(env.db.files_kv(), id, &data);
        assert!(env.db.files_kv().get(&remote_key).is_empty());
        assert!(env.db.files_kv().get(&local_key).is_empty());
        assert!(env.db.files_kv().get(&generate_key).is_empty());
        assert_eq!(
            file_db.get_file_data_sync(env.db.files_kv(), &remote_key, Some(&mut TestFileSources)),
            Err("There is no such key in the database")
        );
    }

    #[test]
    fn file_db_follows_and_optimizes_ref_chains() {
        let mut env = FileDbEnv::new("ref-chain");
        let mut file_db = FileDb::new(env.db.files_kv());

        let data = sample_file_data();
        let id1 = file_db.get_next_file_db_id();
        let id2 = file_db.get_next_file_db_id();
        let id3 = file_db.get_next_file_db_id();
        file_db.set_file_data(
            env.db.files_kv(),
            id3,
            &data,
            true,
            true,
            true,
            &mut TestFileSources,
        );
        file_db.set_file_data_ref(env.db.files_kv(), id2, id3);
        file_db.set_file_data_ref(env.db.files_kv(), id1, id2);

        // the remote key normally maps to id3; point it at id1 so the
        // chain must be followed
        let remote_key = match &data.remote {
            RemoteFileLocation::Full(full) => full.as_key(),
            _ => unreachable!(),
        };
        env.db.files_kv().set(&remote_key, b"1");

        let loaded = file_db
            .get_file_data_sync(env.db.files_kv(), &remote_key, Some(&mut TestFileSources))
            .unwrap();
        assert_eq!(loaded.remote, data.remote);

        // the intermediate ref was rewritten straight to the final id
        assert_eq!(env.db.files_kv().get(b"file1"), b"@@3");
    }

    #[test]
    #[should_panic(expected = "Cycle in file database")]
    fn file_db_detects_ref_cycles() {
        let mut env = FileDbEnv::new("cycle");
        let mut file_db = FileDb::new(env.db.files_kv());
        file_db.set_file_data_ref(env.db.files_kv(), FileDbId::new(1), FileDbId::new(2));
        file_db.set_file_data_ref(env.db.files_kv(), FileDbId::new(2), FileDbId::new(1));
        // the lookup key must resolve into the cycle for the guard to walk it
        env.db.files_kv().set(b"key", b"1");
        let _ = file_db.get_file_data_sync(env.db.files_kv(), b"key", Some(&mut TestFileSources));
    }

    #[test]
    fn file_db_persists_the_id_counter() {
        let mut env = FileDbEnv::new("counter");
        let mut file_db = FileDb::new(env.db.files_kv());
        let data = sample_file_data();
        // `get_next_file_db_id` advances only the FileDb-level counter
        // (FileDb.cpp:176-179); the persisted watermark is the actor-level
        // copy, which only the store/clear/ref ops move
        let id1 = file_db.get_next_file_db_id();
        for _ in 0..4 {
            file_db.get_next_file_db_id();
        }
        file_db.set_file_data(
            env.db.files_kv(),
            id1,
            &data,
            false,
            false,
            false,
            &mut TestFileSources,
        );
        assert_eq!(env.db.files_kv().get(b"file_id"), b"1");

        // a fresh FileDb reads the persisted counter back
        let mut file_db = FileDb::new(env.db.files_kv());
        assert_eq!(file_db.get_next_file_db_id(), FileDbId::new(2));
    }

    #[test]
    fn as_key_layouts_are_pinned() {
        // [KEY_MAGIC][key_type][variant key body]
        let remote = FullRemoteFileLocation::from_document(
            FileType::Document,
            0x0102030405060708,
            0,
            DcId::internal(2),
            String::new(),
        );
        let remote_key = remote.as_key();
        assert_eq!(&remote_key[..8], &[0x32, 0x46, 0x37, 0x64, 5, 0, 0, 0]);
        assert_eq!(&remote_key[8..], &0x0102030405060708i64.to_le_bytes());

        // [KEY_MAGIC][file_type][mtime][path]
        let local = FullLocalFileLocation::new(FileType::Photo, "/tmp/x".to_owned(), 7);
        let local_key = local.as_key();
        assert_eq!(&local_key[..4], &0x84373817u32.to_le_bytes());
        assert_eq!(&local_key[4..8], &2i32.to_le_bytes());
        assert_eq!(&local_key[8..16], &7u64.to_le_bytes());
        assert_eq!(&local_key[16..20], &[6, b'/', b't', b'm']);
        assert_eq!(&local_key[20..], &[b'p', b'/', b'x', 0]);
        assert_eq!(local_key.len(), 24); // 4 + 4 + 8 + 8 (string "/tmp/x")

        // [KEY_MAGIC][file_type][original_path][conversion]
        let generate =
            FullGenerateFileLocation::new(FileType::Photo, "/tmp/x".to_owned(), "c".to_owned());
        let generate_key = generate.as_key();
        assert_eq!(&generate_key[..8], &[0xc8, 0xa1, 0x60, 0x8b, 2, 0, 0, 0]);
    }

    // ==== chunk engine (M.12.3) ====

    #[test]
    fn resource_state_accounts_usage() {
        let mut state = ResourceState::default();
        state.update_limit(100);
        assert_eq!(state.active_limit(), 100);
        assert_eq!(state.unused(), 100);

        state.start_use(60);
        assert_eq!(state.get_using(), 60);
        assert_eq!(state.unused(), 40);
        state.stop_use(60);
        assert_eq!(state.get_using(), 0);
        assert_eq!(state.active_limit(), 40);
        assert_eq!(state.unused(), 40);
    }

    #[test]
    fn resource_state_estimated_limit_and_extra() {
        let mut state = ResourceState::default();
        state.update_limit(100);
        state.start_use(60);
        state.stop_use(60); // used = 60, using = 0
        assert!(state.update_estimated_limit(50));
        assert!(!state.update_estimated_limit(50), "same estimate is a no-op");
        // the 110 estimate covers the used 60 plus the 50 need: 10 beyond
        // the 100 limit
        assert_eq!(state.estimated_extra(), 10);

        // estimated_extra rounds the unused head up to whole units
        let mut state = ResourceState::default();
        state.set_unit_size(32);
        state.update_limit(70);
        assert_eq!(state.estimated_extra(), 26); // ceil(70/32)*32 - 70
    }

    #[test]
    fn resource_state_master_slave_and_ops() {
        let mut master = ResourceState::default();
        master.update_limit(100);
        master.start_use(2);
        master.stop_use(1); // used 1, using 1

        let mut slave = ResourceState::default();
        slave.update_limit(55);
        slave.update_estimated_limit(9);
        slave.update_master(&master);
        // the slave adopts everything but the master's limit
        assert_eq!(slave.get_using(), 1);
        assert_eq!(slave.active_limit(), 54); // own limit 55 - adopted used 1

        master.update_slave(&slave);
        assert_eq!(master.active_limit(), 54); // adopted the slave's limit 55

        let mut lhs = ResourceState::default();
        lhs.update_limit(10);
        lhs.start_use(5);
        lhs.stop_use(5); // used 5, active 5
        let mut rhs = ResourceState::default();
        rhs.update_limit(30);
        rhs.start_use(7);
        rhs.stop_use(7); // used 7, active 23

        lhs += &rhs;
        assert_eq!(lhs.get_using(), 23);
        assert_eq!(lhs.active_limit(), -2);
        lhs -= &rhs;
        assert_eq!(lhs.get_using(), 0);
        assert_eq!(lhs.active_limit(), 5);
    }

    #[test]
    fn bitmask_basics() {
        let mut mask = Bitmask::from_ones(10);
        assert_eq!(mask.size(), 16);
        assert!(mask.get(0) && mask.get(9));
        assert!(!mask.get(10));
        assert!(!mask.get(-1));
        assert_eq!(mask.encode(-1), [0xff, 1, 0x03]);
        assert_eq!(mask.as_vector(), vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);

        mask.set(70);
        assert_eq!(mask.size(), 72);
        assert!(mask.get(70));

        // encode with a prefix masks the tail of the last byte and never
        // mutates the mask
        assert_eq!(mask.encode(5), [0x1f]);
        assert!(mask.get(9));

        // compress: result part i is set iff parts i*k..i*k+k are all set
        assert_eq!(Bitmask::from_ones(9).compress(3).encode(-1), [0x07]);
        // part 2 covers only bits 6..8 of an 8-bit mask
        assert_eq!(Bitmask::from_ones(8).compress(3).encode(-1), [0x03]);
    }

    #[test]
    fn bitmask_prefix_accounting() {
        let mask = Bitmask::from_ones(4);
        // ready prefix from offset 128 covers parts 2..4
        assert_eq!(mask.get_ready_prefix_size(128, 64, 0), 128);
        assert_eq!(mask.get_ready_prefix_size(128, 64, 200), 72);
        assert_eq!(mask.get_ready_prefix_size(-1, 64, 0), 0);
        assert_eq!(mask.get_ready_prefix_size(256, 64, 0), 0);
        assert_eq!(mask.get_total_size(64, 0), 256);
        assert_eq!(mask.get_total_size(64, 200), 200);

        // zero_one round trip
        let decoded = Bitmask::from_decode(&mask.encode(-1));
        assert_eq!(decoded, mask);
    }

    #[test]
    fn parts_manager_known_size_flow() {
        let mut pm = PartsManager::default();
        pm.init(1000, 1000, true, 128, &[], false, false).unwrap();
        assert_eq!(pm.get_part_count(), 8);
        assert_eq!(pm.get_part_size(), 128);
        assert_eq!(pm.get_size(), 1000);
        assert!(!pm.ready());

        let part = pm.start_part().unwrap();
        assert_eq!(part, Part { id: 0, offset: 0, size: 128 });
        let part2 = pm.start_part().unwrap();
        assert_eq!(part2.id, 1);

        pm.on_part_ok(0, 128, 128).unwrap();
        assert_eq!(pm.get_ready_size(), 128);
        pm.on_part_failed(part2.id);
        assert_eq!(pm.get_ready_size(), 128);

        // the failed part is re-issued
        let reissued = pm.start_part().unwrap();
        assert_eq!(reissued.id, 1);
        pm.on_part_ok(1, 128, 128).unwrap();

        // drain the rest
        loop {
            let part = pm.start_part().unwrap();
            if part.id == -1 {
                break;
            }
            let actual = ((part.id as i64 + 1) * 128).min(1000) - part.offset;
            pm.on_part_ok(part.id, part.size, actual as usize).unwrap();
        }
        assert_eq!(pm.get_ready_size(), 1000);
        assert!(pm.ready());
        assert!(pm.may_finish());
        pm.finish().unwrap();
        assert_eq!(pm.get_estimated_extra(), 0);

        // nothing left to request
        assert_eq!(pm.start_part().unwrap(), Part { id: -1, offset: 0, size: 0 });
    }

    #[test]
    fn parts_manager_autoscales_part_size() {
        let mut pm = PartsManager::default();
        pm.init(500_000_000, 500_000_000, true, 0, &[], false, false)
            .unwrap();
        // 64 KiB would give 7630 parts; the size doubles until <= 4000
        assert_eq!(pm.get_part_size(), 128 << 10);
        assert_eq!(pm.get_part_count(), 3815);

        // unknown size starts at 32 KiB and doubles "just in case"
        let mut pm = PartsManager::default();
        pm.init(0, 1_000_000, false, 0, &[], false, false).unwrap();
        assert_eq!(pm.get_part_size(), 64 << 10);
    }

    #[test]
    fn parts_manager_upload_restart_gates() {
        // a forced small part size with the part-count limit must restart
        let mut pm = PartsManager::default();
        let result = pm.init(1000, 1_000_000, true, 128, &[], true, true);
        assert!(result.is_error());
        assert_eq!(result.error().message(), "FILE_UPLOAD_RESTART");

        // a ready part beyond the part count must restart
        let mut pm = PartsManager::default();
        let result = pm.init(1000, 1000, true, 128, &[50], false, true);
        assert!(result.is_error());
        assert_eq!(result.error().message(), "FILE_UPLOAD_RESTART");

        // the expected size hard limit
        let mut pm = PartsManager::default();
        let result = pm.init(0, 5_000_000_000, true, 0, &[], false, false);
        assert!(result.is_error());
        assert_eq!(result.error().message(), "Too big file");
    }

    #[test]
    fn parts_manager_resume_from_ready_parts() {
        let mut pm = PartsManager::default();
        pm.init(1000, 1000, true, 128, &[0, 1, 2, 3], false, false)
            .unwrap();
        assert_eq!(pm.get_ready_size(), 512);
        assert_eq!(pm.get_ready_prefix_count(), 4);
        assert_eq!(pm.get_unchecked_ready_prefix_size(), 512);
        assert_eq!(pm.get_bitmask(), [0x0f]);

        // the next issued part is the first empty one
        assert_eq!(pm.start_part().unwrap().id, 4);
    }

    #[test]
    fn parts_manager_known_prefix_flow() {
        let mut pm = PartsManager::default();
        pm.init(500, 2000, false, 128, &[0, 1, 2], false, true)
            .unwrap();
        assert_eq!(pm.get_part_count(), 3);
        assert_eq!(pm.get_ready_size(), 384);

        // growing the not-ready prefix just extends the part count
        pm.set_known_prefix(1000, false).unwrap();
        assert_eq!(pm.get_part_count(), 7);

        // the final prefix pins the size
        pm.set_known_prefix(1000, true).unwrap();
        assert_eq!(pm.get_part_count(), 8);
        assert_eq!(pm.get_size(), 1000);
        assert_eq!(pm.start_part().unwrap().id, 3);

        // a shrinking prefix is a restart
        let mut pm = PartsManager::default();
        pm.init(500, 2000, false, 128, &[], false, true).unwrap();
        let result = pm.set_known_prefix(400, true);
        assert!(result.is_error());
        assert_eq!(result.error().message(), "FILE_UPLOAD_RESTART");
    }

    #[test]
    fn parts_manager_unknown_size_discovery() {
        let mut pm = PartsManager::default();
        pm.init(0, 1_000_000, true, 65536, &[], false, false)
            .unwrap();
        assert!(!pm.unchecked_ready());
        let part = pm.start_part().unwrap();
        assert_eq!(part.id, 0);
        // a short part pins both min and max size
        pm.on_part_ok(0, 65536, 100).unwrap();
        assert_eq!(pm.get_size(), 100);
        assert!(pm.ready());

        // the download part-count cap
        let mut pm = PartsManager::default();
        pm.init(0, 1_000_000, true, 65536, &[], true, false)
            .unwrap();
        for _ in 0..8000 {
            let part = pm.start_part().unwrap();
            assert_ne!(part.id, -1);
        }
        let result = pm.start_part();
        assert!(result.is_error());
        assert_eq!(
            result.error().message(),
            "FILE_DOWNLOAD_RESTART_INCREASE_PART_SIZE"
        );

        // the upload variant reports the size as untransferable
        let mut pm = PartsManager::default();
        pm.init(0, 1_000_000, true, 65536, &[], true, true)
            .unwrap();
        for _ in 0..8000 {
            pm.start_part().unwrap();
        }
        let result = pm.start_part();
        assert!(result.is_error());
        assert_eq!(result.error().message(), "Too big file with unknown size");
    }

    #[test]
    fn parts_manager_rejects_bad_part_results() {
        let mut pm = PartsManager::default();
        pm.init(1000, 1000, true, 128, &[], false, false).unwrap();
        pm.start_part().unwrap();

        // a short part inside the file is an error
        let result = pm.on_part_ok(0, 128, 64);
        assert!(result.is_error());
        assert_eq!(
            result.error().message(),
            "Failed to transfer file: [size:1000][offset:0][transferred size:64][part size:128]"
        );

        // a non-empty part past the discovered file end is an error too:
        // part 1 goes Pending while the size is unknown, then part 0's
        // short result pins the size (final=true with size 0 enters the
        // no-size mode without the known-prefix gate)
        let mut pm = PartsManager::default();
        pm.init(0, 1_000_000, true, 65536, &[], false, false)
            .unwrap();
        pm.start_part().unwrap();
        pm.start_part().unwrap();
        pm.on_part_ok(0, 65536, 100).unwrap(); // size discovered: 100
        let result = pm.on_part_ok(1, 65536, 50);
        assert!(result.is_error());
        assert_eq!(
            result.error().message(),
            "Failed to transfer file: [size:100][offset:65536][transferred size:50][part size:65536]"
        );
    }

    #[test]
    fn parts_manager_streaming() {
        let mut pm = PartsManager::default();
        pm.init(1000, 1000, true, 128, &[], false, false).unwrap();
        assert_eq!(pm.set_streaming_offset(256, 300), 2);
        assert_eq!(pm.get_streaming_offset(), 256);

        // only the streaming window is served
        assert_eq!(pm.start_part().unwrap().id, 2);
        pm.on_part_ok(2, 128, 128).unwrap();
        assert_eq!(pm.get_estimated_extra(), 256);

        // parts 3 and 4 complete the window [256, 556)
        assert_eq!(pm.start_part().unwrap().id, 3);
        pm.on_part_ok(3, 128, 128).unwrap();
        assert_eq!(pm.start_part().unwrap().id, 4);
        pm.on_part_ok(4, 128, 128).unwrap();

        // the window is done but the file is not: the limit is "reached"
        assert!(pm.may_finish());
        let result = pm.finish();
        assert!(result.is_error());
        assert_eq!(result.error().message(), "FILE_DOWNLOAD_LIMIT");
        assert!(!pm.ready());

        // a wrapping window over an already-complete file just finishes
        let mut pm = PartsManager::default();
        pm.init(1000, 1000, true, 128, &[0, 1, 2, 3, 4, 5, 6, 7], false, false)
            .unwrap();
        pm.set_streaming_offset(900, 200);
        assert!(pm.ready());
        pm.finish().unwrap();
    }

    #[test]
    fn parts_manager_need_check_gates_prefix() {
        let mut pm = PartsManager::default();
        pm.init(1000, 1000, true, 128, &[0, 1, 2, 3], false, false)
            .unwrap();
        pm.set_need_check();
        assert_eq!(pm.get_checked_prefix_size(), 512);

        // the ready prefix is held back by the checked size
        assert_eq!(pm.get_ready_prefix_count(), 4);
        pm.set_checked_prefix_size(256);
        assert_eq!(pm.get_ready_prefix_count(), 2);
        assert_eq!(pm.get_bitmask(), [0x03]);
    }

    /// A fake `FileLoaderActor`. The worker half is moved into the
    /// manager; the driver half stays with the test and shares the
    /// received grants and the link handle through `Rc`s.
    struct FakeWorker {
        grants: Rc<RefCell<Vec<ResourceState>>>,
        handle: Rc<RefCell<Option<ResourceManagerHandle>>>,
    }

    #[derive(Clone)]
    struct FakeWorkerDriver {
        grants: Rc<RefCell<Vec<ResourceState>>>,
        handle: Rc<RefCell<Option<ResourceManagerHandle>>>,
    }

    impl FakeWorker {
        fn new() -> (FakeWorker, FakeWorkerDriver) {
            let part = (
                Rc::new(RefCell::new(Vec::new())),
                Rc::new(RefCell::new(None)),
            );
            (
                FakeWorker {
                    grants: Rc::clone(&part.0),
                    handle: Rc::clone(&part.1),
                },
                FakeWorkerDriver {
                    grants: part.0,
                    handle: part.1,
                },
            )
        }
    }

    impl ResourceManagerWorker for FakeWorker {
        fn set_resource_manager(&mut self, handle: ResourceManagerHandle) {
            *self.handle.borrow_mut() = Some(handle);
        }

        fn update_resources(&mut self, resource_state: ResourceState) {
            self.grants.borrow_mut().push(resource_state);
        }
    }

    impl FakeWorkerDriver {
        fn grant_count(&self) -> usize {
            self.grants.borrow().len()
        }

        fn last_grant_active_limit(&self) -> i64 {
            self.grants.borrow().last().expect("no grant recorded").active_limit()
        }

        /// Report a need of `estimated_limit` bytes in `unit_size`-byte
        /// parts — a fresh loader state, like the C++ loader's first
        /// update.
        fn report_need(&self, estimated_limit: i64, unit_size: usize) {
            let mut state = ResourceState::default();
            state.set_unit_size(unit_size);
            state.update_estimated_limit(estimated_limit);
            self.update(&state);
        }

        /// Consume `x` bytes of the last grant (a part completed) and
        /// report the mutated state back.
        fn complete_part_and_report(&self, x: i64) {
            let mut state = self.grants.borrow().last().expect("no grant recorded").clone();
            state.start_use(x);
            state.stop_use(x);
            self.update(&state);
        }

        fn hangup(&self) {
            self.handle
                .borrow()
                .as_ref()
                .expect("no link handle")
                .hangup_shared();
        }

        fn update(&self, state: &ResourceState) {
            self.handle
                .borrow()
                .as_ref()
                .expect("no link handle")
                .update_resources(state);
        }
    }

    #[test]
    fn resource_manager_greedy_satisfies_smallest_first() {
        let manager =
            Rc::new(RefCell::new(ResourceManager::new(1000, ResourceManagerMode::Greedy)));
        let (worker_a, driver_a) = FakeWorker::new();
        let (worker_b, driver_b) = FakeWorker::new();
        ResourceManager::register_worker(&manager, Box::new(worker_a), 0);
        ResourceManager::register_worker(&manager, Box::new(worker_b), 0);

        // A needs 250 bytes in 100-byte parts: the grant rounds up to 300
        driver_a.report_need(250, 100);
        assert_eq!(driver_a.grant_count(), 1);
        assert_eq!(driver_a.last_grant_active_limit(), 300);

        // B needs 500 bytes in 200-byte parts: grant 600 out of the 700
        // that is still unused
        driver_b.report_need(500, 200);
        assert_eq!(driver_b.grant_count(), 1);
        assert_eq!(driver_b.last_grant_active_limit(), 600);

        // A completing one part of its grant asks for nothing new: the
        // unused head of its grant keeps its heap key at zero
        driver_a.complete_part_and_report(100);
        assert_eq!(driver_a.grant_count(), 1);

        // B hanging up returns its whole grant to the pool: C's 5000-byte
        // request gets the 700 freed plus the 100 topped-up limit
        driver_b.hangup();
        let (worker_c, driver_c) = FakeWorker::new();
        ResourceManager::register_worker(&manager, Box::new(worker_c), 0);
        driver_c.report_need(5000, 100);
        assert_eq!(driver_c.last_grant_active_limit(), 800);
    }

    #[test]
    fn resource_manager_baseline_serves_priority_order() {
        let manager =
            Rc::new(RefCell::new(ResourceManager::new(350, ResourceManagerMode::Baseline)));
        let (worker0, driver0) = FakeWorker::new();
        let (worker1, driver1) = FakeWorker::new();
        let (worker2, driver2) = FakeWorker::new();
        let (worker3, driver3) = FakeWorker::new();
        // the C++ `add_node` order: -5 -> stored 5, inserted last; the
        // service order is the container order, so the stored-priority
        // DESCENDING sequence 5, 2, 1, 0 serves first
        ResourceManager::register_worker(&manager, Box::new(worker0), -5);
        ResourceManager::register_worker(&manager, Box::new(worker1), 2);
        ResourceManager::register_worker(&manager, Box::new(worker2), -1);
        ResourceManager::register_worker(&manager, Box::new(worker3), 0);

        for driver in &[&driver0, &driver1, &driver2, &driver3] {
            driver.report_need(100, 100);
        }

        // 350 bytes serve the first three workers exactly; the last one
        // would only get a 50-byte fraction and is left starving
        assert_eq!(driver0.last_grant_active_limit(), 100);
        assert_eq!(driver1.last_grant_active_limit(), 100);
        assert_eq!(driver2.last_grant_active_limit(), 100);
        assert_eq!(driver3.grant_count(), 0);
    }
}
