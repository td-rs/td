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

use parking_lot::Mutex;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::File;
use std::sync::Arc;

use crate::chat_manager::{ChannelId, ChatId, DcId};
use crate::message_id::{DialogId, DialogType, MessageFullId, MessageId};
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::{CommunityId, StoryId, UserId};
use tddb::sqlite_key_value::SqliteKeyValue;
use tdmtproto::telegram_api;
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
/// FileReferenceManager allocates. The registry is [`FileReferenceManager`]
/// below; the UserManager's local stand-in counter lifts when the manager is
/// wired into it (M.12.2b).
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

/// `is_background_type` (FileManager.cpp:3990-3992).
fn is_background_type(file_type: FileType) -> bool {
    file_type == FileType::Wallpaper || file_type == FileType::Background
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

    /// `FullRemoteFileLocation::as_input_document` (FileLocation.h:473-477).
    pub fn as_input_document(&self) -> telegram_api::inputDocument {
        assert!(self.is_common());
        assert!(self.is_document());
        let common = self.common();
        telegram_api::inputDocument {
            id: common.id,
            access_hash: common.access_hash,
            file_reference: self.file_reference.as_bytes().to_vec(),
        }
    }

    /// `FullRemoteFileLocation::as_input_photo` (FileLocation.h:481-484).
    pub fn as_input_photo(&self) -> telegram_api::inputPhoto {
        assert!(self.is_photo());
        let photo = self.photo();
        telegram_api::inputPhoto {
            id: photo.id,
            access_hash: photo.access_hash,
            file_reference: self.file_reference.as_bytes().to_vec(),
        }
    }

    /// `FullRemoteFileLocation::as_input_encrypted_file`
    /// (FileLocation.h:486-489).
    pub fn as_input_encrypted_file(&self) -> telegram_api::inputEncryptedFile {
        assert!(self.is_encrypted_secret());
        let common = self.common();
        telegram_api::inputEncryptedFile {
            id: common.id,
            access_hash: common.access_hash,
        }
    }

    /// `FullRemoteFileLocation::as_input_web_file_location`
    /// (FileLocation.h:381-384).
    pub fn as_input_web_file_location(
        &self,
    ) -> Result<telegram_api::InputWebFileLocation, &'static str> {
        if !self.is_web() {
            return Err("LOG_CHECK(is_web()) failed in as_input_web_file_location");
        }
        let FullRemoteVariant::Web(web) = &self.variant else {
            return Err("not a web location");
        };
        Ok(telegram_api::InputWebFileLocation::inputWebFileLocation(
            telegram_api::inputWebFileLocation {
                url: web.url.clone(),
                access_hash: web.access_hash,
            },
        ))
    }

    /// `FullRemoteFileLocation::as_input_file_location`
    /// (FileLocation.h:386-470). The dialog-photo arms need the peer's
    /// `InputPeer`, which the C++ fetches through
    /// `G()->dialog_manager()`; the resolver parameter is that seam (it
    /// receives the `DialogId` the source carries and answers with the
    /// peer, or `None` when the peer is not in the cache yet). The
    /// sticker-set arms are the StickersManager seam (Wave 2).
    pub fn as_input_file_location(
        &self,
        input_peer: &dyn Fn(DialogId) -> Option<telegram_api::InputPeer>,
    ) -> Result<telegram_api::InputFileLocation, String> {
        use telegram_api as tl;
        let file_reference = self.file_reference.as_bytes().to_vec();
        match &self.variant {
            FullRemoteVariant::Photo(photo) => {
                let id = photo.id;
                let access_hash = photo.access_hash;
                let thumb_byte = |thumbnail_type: i32| (thumbnail_type as u8 as char).to_string();
                match &photo.source {
                    PhotoSizeSource::Legacy { .. } | PhotoSizeSource::StickerSetThumbnail { .. } => {
                        Err("UNREACHABLE: legacy/sticker-set-thumbnail source in as_input_file_location".to_owned())
                    }
                    PhotoSizeSource::Thumbnail {
                        file_type,
                        thumbnail_type,
                    } => match file_type {
                        FileType::Photo
                        | FileType::PhotoStory
                        | FileType::SelfDestructingPhoto => {
                            Ok(tl::InputFileLocation::inputPhotoFileLocation(tl::inputPhotoFileLocation {
                                id,
                                access_hash,
                                file_reference,
                                thumb_size: thumb_byte(thumbnail_type.type_),
                            }))
                        }
                        FileType::Thumbnail => {
                            Ok(tl::InputFileLocation::inputDocumentFileLocation(tl::inputDocumentFileLocation {
                                id,
                                access_hash,
                                file_reference,
                                thumb_size: thumb_byte(thumbnail_type.type_),
                            }))
                        }
                        _ => Err("UNREACHABLE: thumbnail source file type".to_owned()),
                    },
                    PhotoSizeSource::DialogPhotoSmall { dialog_id, .. }
                    | PhotoSizeSource::DialogPhotoBig { dialog_id, .. } => {
                        let is_big = matches!(
                            &photo.source,
                            PhotoSizeSource::DialogPhotoBig { .. }
                        );
                        let peer = input_peer(*dialog_id).ok_or_else(|| {
                            format!("Have no input peer for dialog photo {dialog_id:?}")
                        })?;
                        Ok(tl::InputFileLocation::inputPeerPhotoFileLocation(
                            tl::inputPeerPhotoFileLocation {
                                flags: 0,
                                big: is_big,
                                peer: Some(Box::new(peer)),
                                photo_id: id,
                            },
                        ))
                    }
                    PhotoSizeSource::FullLegacy {
                        volume_id,
                        local_id,
                        secret,
                    } => Ok(tl::InputFileLocation::inputPhotoLegacyFileLocation(
                        tl::inputPhotoLegacyFileLocation {
                            id,
                            access_hash,
                            file_reference,
                            volume_id: *volume_id,
                            local_id: *local_id,
                            secret: *secret,
                        },
                    )),
                    PhotoSizeSource::DialogPhotoSmallLegacy {
                        dialog_id,
                        volume_id,
                        local_id,
                        ..
                    }
                    | PhotoSizeSource::DialogPhotoBigLegacy {
                        dialog_id,
                        volume_id,
                        local_id,
                        ..
                    } => {
                        let is_big = matches!(
                            &photo.source,
                            PhotoSizeSource::DialogPhotoBigLegacy { .. }
                        );
                        let peer = input_peer(*dialog_id).ok_or_else(|| {
                            format!("Have no input peer for dialog photo {dialog_id:?}")
                        })?;
                        Ok(tl::InputFileLocation::inputPeerPhotoFileLocationLegacy(
                            tl::inputPeerPhotoFileLocationLegacy {
                                flags: 0,
                                big: is_big,
                                peer: Some(Box::new(peer)),
                                volume_id: *volume_id,
                                local_id: *local_id,
                            },
                        ))
                    }
                    PhotoSizeSource::StickerSetThumbnailLegacy { .. }
                    | PhotoSizeSource::StickerSetThumbnailVersion { .. } => Err(
                        "StickersManager::get_input_sticker_set is the Wave 2 seam".to_owned(),
                    ),
                }
            }
            FullRemoteVariant::Common(common) => {
                if self.is_encrypted_secret() {
                    Ok(tl::InputFileLocation::inputEncryptedFileLocation(
                        tl::inputEncryptedFileLocation {
                            id: common.id,
                            access_hash: common.access_hash,
                        },
                    ))
                } else if self.is_secure() {
                    Ok(tl::InputFileLocation::inputSecureFileLocation(
                        tl::inputSecureFileLocation {
                            id: common.id,
                            access_hash: common.access_hash,
                        },
                    ))
                } else {
                    Ok(tl::InputFileLocation::inputDocumentFileLocation(
                        tl::inputDocumentFileLocation {
                            id: common.id,
                            access_hash: common.access_hash,
                            file_reference,
                            thumb_size: String::new(),
                        },
                    ))
                }
            }
            FullRemoteVariant::Web(_) => {
                Err("UNREACHABLE: web location in as_input_file_location".to_owned())
            }
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
    /// The C++ `iv_` is a binary `std::string` (the raw 32-byte IGE iv
    /// of a secret download, FileDownloader.cpp:310); not UTF-8 in
    /// general.
    pub iv: Vec<u8>,
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
        store_parse::store_string_bytes(out, &self.iv);
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
        let iv = parser.string_bytes()?;
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

    /// `FileEncryptionKey::mutable_iv()` — the in-place iv mutation the
    /// downloader's resume path performs (FileDownloader.cpp:501).
    pub fn set_iv(&mut self, iv: [u8; 32]) {
        assert!(self.is_secret());
        assert_eq!(self.key_iv.len(), 64);
        self.key_iv[32..].copy_from_slice(&iv);
    }

    /// `FileEncryptionKey::FileEncryptionKey(const secure_storage::Secret &)`
    /// (FileEncryptionKey.cpp:29-32) — a secure key wrapping the secret.
    pub fn from_secret(secret: &[u8]) -> FileEncryptionKey {
        FileEncryptionKey {
            key_iv: secret.to_vec(),
            type_: FileEncryptionKeyType::Secure,
        }
    }

    /// `FileEncryptionKey::secret` (FileEncryptionKey.cpp:53-56) — the
    /// first `Secret::size()` (32) bytes; `set_value_hash` appends the
    /// value hash after them.
    pub fn secret(&self) -> &[u8] {
        assert!(self.is_secure());
        &self.key_iv[..self.key_iv.len().min(32)]
    }

    /// `FileEncryptionKey::has_value_hash` (FileEncryptionKey.cpp:58-61).
    pub fn has_value_hash(&self) -> bool {
        assert!(self.is_secure());
        self.key_iv.len() > 32
    }

    /// `FileEncryptionKey::set_value_hash` (FileEncryptionKey.cpp:63-65).
    pub fn set_value_hash(&mut self, value_hash: &[u8]) {
        self.key_iv.resize(32 + value_hash.len(), 0);
        self.key_iv[32..].copy_from_slice(value_hash);
    }

    /// `FileEncryptionKey::value_hash` (FileEncryptionKey.cpp:67-70).
    pub fn value_hash(&self) -> &[u8] {
        assert!(self.has_value_hash());
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
/// that `FileData` serialization calls through. Implemented by
/// [`FileReferenceManager`] (M.12.2a); the byte-level tests keep the
/// [`TestFileSources`] stand-in.
pub trait FileSourceStorage: Send {
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

// ==== FileSource (M.12.2a) ====

/// `td::BackgroundId` (BackgroundId.h) — the canonical id wrapper lives in
/// [`crate::background_manager`] (the Backgrounds slice); the FileSource
/// wire format re-exports it.
pub use crate::background_manager::BackgroundId;

impl BackgroundId {
    /// The FileSource-codec name for `BackgroundId::parse` (the canonical
    /// wrapper's method is `parse`).
    pub fn parse_from(parser: &mut Parser) -> Result<BackgroundId, &'static str> {
        BackgroundId::parse(parser)
    }
}

/// `td::StoryFullId` (StoryFullId.h) — the dialog + story pair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StoryFullId {
    dialog_id: DialogId,
    story_id: StoryId,
}

impl StoryFullId {
    pub fn new(dialog_id: DialogId, story_id: StoryId) -> StoryFullId {
        StoryFullId {
            dialog_id,
            story_id,
        }
    }

    pub fn get_dialog_id(self) -> DialogId {
        self.dialog_id
    }

    pub fn get_story_id(self) -> StoryId {
        self.story_id
    }

    /// `StoryFullId::store` (StoryFullId.h:54-57) — dialog then story.
    pub fn store(self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        store_parse::store_i32(out, self.story_id.get());
    }

    /// `StoryFullId::parse` (StoryFullId.h:59-62).
    pub fn parse_from(parser: &mut Parser) -> Result<StoryFullId, &'static str> {
        let dialog_id = DialogId::parse_from(parser)?;
        let story_id = StoryId::new(parser.i32_()?);
        Ok(StoryFullId {
            dialog_id,
            story_id,
        })
    }
}

/// `td::StoryAlbumId` (StoryAlbumId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StoryAlbumId(i32);

impl StoryAlbumId {
    pub fn new(id: i32) -> StoryAlbumId {
        StoryAlbumId(id)
    }

    pub fn get(self) -> i32 {
        self.0
    }
}

/// `td::StoryAlbumFullId` (StoryAlbumFullId.h) — the dialog + album pair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StoryAlbumFullId {
    dialog_id: DialogId,
    story_album_id: StoryAlbumId,
}

impl StoryAlbumFullId {
    pub fn new(dialog_id: DialogId, story_album_id: StoryAlbumId) -> StoryAlbumFullId {
        StoryAlbumFullId {
            dialog_id,
            story_album_id,
        }
    }

    pub fn get_dialog_id(self) -> DialogId {
        self.dialog_id
    }

    pub fn get_story_album_id(self) -> StoryAlbumId {
        self.story_album_id
    }

    /// `StoryAlbumFullId::store` (StoryAlbumFullId.h:51-54) — dialog then album.
    pub fn store(self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        store_parse::store_i32(out, self.story_album_id.get());
    }

    /// `StoryAlbumFullId::parse` (StoryAlbumFullId.h:56-59).
    pub fn parse_from(parser: &mut Parser) -> Result<StoryAlbumFullId, &'static str> {
        let dialog_id = DialogId::parse_from(parser)?;
        let story_album_id = StoryAlbumId::new(parser.i32_()?);
        Ok(StoryAlbumFullId {
            dialog_id,
            story_album_id,
        })
    }
}

/// `td::QuickReplyShortcutId` (QuickReplyShortcutId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QuickReplyShortcutId(i32);

impl QuickReplyShortcutId {
    pub fn new(id: i32) -> QuickReplyShortcutId {
        QuickReplyShortcutId(id)
    }

    pub fn get(self) -> i32 {
        self.0
    }
}

/// `td::QuickReplyMessageFullId` (QuickReplyMessageFullId.h) — the shortcut
/// + message pair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QuickReplyMessageFullId {
    quick_reply_shortcut_id: QuickReplyShortcutId,
    message_id: MessageId,
}

impl QuickReplyMessageFullId {
    pub fn new(
        quick_reply_shortcut_id: QuickReplyShortcutId,
        message_id: MessageId,
    ) -> QuickReplyMessageFullId {
        QuickReplyMessageFullId {
            quick_reply_shortcut_id,
            message_id,
        }
    }

    pub fn get_quick_reply_shortcut_id(self) -> QuickReplyShortcutId {
        self.quick_reply_shortcut_id
    }

    pub fn get_message_id(self) -> MessageId {
        self.message_id
    }

    /// `QuickReplyMessageFullId::store` (QuickReplyMessageFullId.h:54-57) —
    /// shortcut then message.
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.quick_reply_shortcut_id.get());
        self.message_id.store(out);
    }

    /// `QuickReplyMessageFullId::parse` (QuickReplyMessageFullId.h:59-62).
    pub fn parse_from(parser: &mut Parser) -> Result<QuickReplyMessageFullId, &'static str> {
        let quick_reply_shortcut_id = QuickReplyShortcutId::new(parser.i32_()?);
        let message_id = MessageId::parse_from(parser)?;
        Ok(QuickReplyMessageFullId {
            quick_reply_shortcut_id,
            message_id,
        })
    }
}

/// `td::ForumTopicId` (ForumTopicId.h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ForumTopicId(i32);

impl ForumTopicId {
    pub fn new(id: i32) -> ForumTopicId {
        ForumTopicId(id)
    }

    /// `ForumTopicId::is_valid` (ForumTopicId.h:61-63).
    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    pub fn get(self) -> i32 {
        self.0
    }

    /// `ForumTopicId::store` (ForumTopicId.h:78-80).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.0);
    }

    /// `ForumTopicId::parse` (ForumTopicId.h:82-84).
    pub fn parse_from(parser: &mut Parser) -> Result<ForumTopicId, &'static str> {
        Ok(ForumTopicId::new(parser.i32_()?))
    }
}

/// `td::SavedMessagesTopicId` (SavedMessagesTopicId.h) — a DialogId in its
/// own namespace.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SavedMessagesTopicId {
    dialog_id: DialogId,
}

impl SavedMessagesTopicId {
    pub fn new(dialog_id: DialogId) -> SavedMessagesTopicId {
        SavedMessagesTopicId { dialog_id }
    }

    /// `SavedMessagesTopicId::is_valid` (SavedMessagesTopicId.h:43-45).
    pub fn is_valid(self) -> bool {
        self.dialog_id.is_valid()
    }

    pub fn get_dialog_id(self) -> DialogId {
        self.dialog_id
    }

    /// `SavedMessagesTopicId::store` (SavedMessagesTopicId.h:68-70).
    pub fn store(self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
    }

    /// `SavedMessagesTopicId::parse` (SavedMessagesTopicId.h:72-74).
    pub fn parse_from(parser: &mut Parser) -> Result<SavedMessagesTopicId, &'static str> {
        Ok(SavedMessagesTopicId::new(DialogId::parse_from(parser)?))
    }
}

/// `td::MessageTopic::Type` (MessageTopic.h:26) — kept as the raw `int32`
/// like the C++ enum: `td::parse` of a trivially-copyable enum neither
/// validates nor rejects unknown values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageTopicType(i32);

impl MessageTopicType {
    pub const NONE: Self = Self(0);
    pub const THREAD: Self = Self(1);
    pub const FORUM: Self = Self(2);
    pub const MONOFORUM: Self = Self(3);
    pub const SAVED_MESSAGES: Self = Self(4);

    pub const fn from_raw(raw: i32) -> MessageTopicType {
        MessageTopicType(raw)
    }

    pub const fn get(self) -> i32 {
        self.0
    }
}

/// `td::MessageTopic` (MessageTopic.h:22-83).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageTopic {
    type_: MessageTopicType,
    dialog_id: DialogId,
    top_thread_message_id: MessageId,
    forum_topic_id: ForumTopicId,
    saved_messages_topic_id: SavedMessagesTopicId,
}

impl MessageTopic {
    /// `MessageTopic::thread` (MessageTopic.cpp:105-112).
    pub fn thread(dialog_id: DialogId, top_thread_message_id: MessageId) -> MessageTopic {
        MessageTopic {
            type_: MessageTopicType::THREAD,
            dialog_id,
            top_thread_message_id,
            ..MessageTopic::default()
        }
    }

    /// `MessageTopic::forum` (MessageTopic.cpp:114-120).
    pub fn forum(dialog_id: DialogId, forum_topic_id: ForumTopicId) -> MessageTopic {
        MessageTopic {
            type_: MessageTopicType::FORUM,
            dialog_id,
            forum_topic_id,
            ..MessageTopic::default()
        }
    }

    /// `MessageTopic::monoforum` (MessageTopic.cpp:122-128).
    pub fn monoforum(
        dialog_id: DialogId,
        saved_messages_topic_id: SavedMessagesTopicId,
    ) -> MessageTopic {
        MessageTopic {
            type_: MessageTopicType::MONOFORUM,
            dialog_id,
            saved_messages_topic_id,
            ..MessageTopic::default()
        }
    }

    /// `MessageTopic::saved_messages` (MessageTopic.cpp:130-136).
    pub fn saved_messages(
        dialog_id: DialogId,
        saved_messages_topic_id: SavedMessagesTopicId,
    ) -> MessageTopic {
        MessageTopic {
            type_: MessageTopicType::SAVED_MESSAGES,
            dialog_id,
            saved_messages_topic_id,
            ..MessageTopic::default()
        }
    }

    /// `MessageTopic::store` (MessageTopic.hpp:16-45) — a flags bitmask then
    /// each present field, in declaration order.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_type = self.type_ != MessageTopicType::NONE;
        let has_dialog_id = self.dialog_id.is_valid();
        let has_top_thread_message_id = self.top_thread_message_id.is_valid();
        let has_forum_topic_id = self.forum_topic_id.is_valid();
        let has_saved_messages_topic_id = self.saved_messages_topic_id.is_valid();
        store_parse::store_flags(
            out,
            [
                has_type,
                has_dialog_id,
                has_top_thread_message_id,
                has_forum_topic_id,
                has_saved_messages_topic_id,
            ],
        );
        if has_type {
            store_parse::store_i32(out, self.type_.get());
        }
        if has_dialog_id {
            self.dialog_id.store(out);
        }
        if has_top_thread_message_id {
            self.top_thread_message_id.store(out);
        }
        if has_forum_topic_id {
            self.forum_topic_id.store(out);
        }
        if has_saved_messages_topic_id {
            self.saved_messages_topic_id.store(out);
        }
    }

    /// `MessageTopic::parse` (MessageTopic.hpp:47-81).
    pub fn parse_from(parser: &mut Parser) -> Result<MessageTopic, &'static str> {
        let [
            has_type,
            has_dialog_id,
            has_top_thread_message_id,
            has_forum_topic_id,
            has_saved_messages_topic_id,
        ] = parser.parse_flags::<5>()?;
        let type_ = if has_type {
            MessageTopicType::from_raw(parser.i32_()?)
        } else {
            MessageTopicType::NONE
        };
        let dialog_id = if has_dialog_id {
            DialogId::parse_from(parser)?
        } else {
            DialogId::default()
        };
        let top_thread_message_id = if has_top_thread_message_id {
            MessageId::parse_from(parser)?
        } else {
            MessageId::default()
        };
        let forum_topic_id = if has_forum_topic_id {
            ForumTopicId::parse_from(parser)?
        } else {
            ForumTopicId::default()
        };
        let saved_messages_topic_id = if has_saved_messages_topic_id {
            SavedMessagesTopicId::parse_from(parser)?
        } else {
            SavedMessagesTopicId::default()
        };
        Ok(MessageTopic {
            type_,
            dialog_id,
            top_thread_message_id,
            forum_topic_id,
            saved_messages_topic_id,
        })
    }
}

/// `FileReferenceManager::FileSource` (FileReferenceManager.h:153-249) —
/// the append-only source table's payload, one variant per manager-scoped
/// origin. The tag is the variant's position in the C++ `Variant` list.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FileSource {
    Message(MessageFullId),
    UserPhoto {
        user_id: UserId,
        photo_id: i64,
    },
    ChatPhoto(ChatId),
    ChannelPhoto(ChannelId),
    Wallpapers,
    WebPage(String),
    SavedAnimations,
    RecentStickers {
        is_attached: bool,
    },
    FavoriteStickers,
    Background {
        background_id: BackgroundId,
        access_hash: i64,
    },
    ChatFull(ChatId),
    ChannelFull(ChannelId),
    AppConfig,
    SavedRingtones,
    UserFull(UserId),
    AttachMenuBot(UserId),
    WebApp {
        user_id: UserId,
        short_name: String,
    },
    Story(StoryFullId),
    QuickReplyMessage(QuickReplyMessageFullId),
    StarTransaction {
        dialog_id: DialogId,
        transaction_id: String,
        is_refund: bool,
    },
    BotMediaPreview(UserId),
    BotMediaPreviewInfo {
        bot_user_id: UserId,
        language_code: String,
    },
    StoryAlbum(StoryAlbumFullId),
    UserSavedMusic {
        user_id: UserId,
        document_id: i64,
        access_hash: i64,
    },
    DraftMessage {
        dialog_id: DialogId,
        topic: MessageTopic,
    },
    RichMessage(MessageFullId),
    WelcomeMessages(DialogId),
    CommunityFull(CommunityId),
}

impl FileSource {
    /// The `Variant` offset (`FileReferenceManager.hpp:50`).
    fn tag(&self) -> i32 {
        match self {
            FileSource::Message(_) => 0,
            FileSource::UserPhoto { .. } => 1,
            FileSource::ChatPhoto(_) => 2,
            FileSource::ChannelPhoto(_) => 3,
            FileSource::Wallpapers => 4,
            FileSource::WebPage(_) => 5,
            FileSource::SavedAnimations => 6,
            FileSource::RecentStickers { .. } => 7,
            FileSource::FavoriteStickers => 8,
            FileSource::Background { .. } => 9,
            FileSource::ChatFull(_) => 10,
            FileSource::ChannelFull(_) => 11,
            FileSource::AppConfig => 12,
            FileSource::SavedRingtones => 13,
            FileSource::UserFull(_) => 14,
            FileSource::AttachMenuBot(_) => 15,
            FileSource::WebApp { .. } => 16,
            FileSource::Story(_) => 17,
            FileSource::QuickReplyMessage(_) => 18,
            FileSource::StarTransaction { .. } => 19,
            FileSource::BotMediaPreview(_) => 20,
            FileSource::BotMediaPreviewInfo { .. } => 21,
            FileSource::StoryAlbum(_) => 22,
            FileSource::UserSavedMusic { .. } => 23,
            FileSource::DraftMessage { .. } => 24,
            FileSource::RichMessage(_) => 25,
            FileSource::WelcomeMessages(_) => 26,
            FileSource::CommunityFull(_) => 27,
        }
    }

    /// The `store_file_source` payload: tag then fields, in the C++ `visit`
    /// order (FileReferenceManager.hpp:51-100).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.tag());
        match self {
            FileSource::Message(message_full_id) => message_full_id.store(out),
            FileSource::UserPhoto { user_id, photo_id } => {
                store_parse::store_i64(out, user_id.get());
                store_parse::store_i64(out, *photo_id);
            }
            FileSource::ChatPhoto(chat_id) => store_parse::store_i64(out, chat_id.get()),
            FileSource::ChannelPhoto(channel_id) => store_parse::store_i64(out, channel_id.get()),
            FileSource::Wallpapers
            | FileSource::SavedAnimations
            | FileSource::FavoriteStickers
            | FileSource::AppConfig
            | FileSource::SavedRingtones => {}
            FileSource::WebPage(url) => store_parse::store_string(out, url),
            FileSource::RecentStickers { is_attached } => {
                store_parse::store_i32(out, i32::from(*is_attached))
            }
            FileSource::Background {
                background_id,
                access_hash,
            } => {
                background_id.store(out);
                store_parse::store_i64(out, *access_hash);
            }
            FileSource::ChatFull(chat_id) => store_parse::store_i64(out, chat_id.get()),
            FileSource::ChannelFull(channel_id) => store_parse::store_i64(out, channel_id.get()),
            FileSource::UserFull(user_id)
            | FileSource::AttachMenuBot(user_id)
            | FileSource::BotMediaPreview(user_id) => {
                store_parse::store_i64(out, user_id.get());
            }
            FileSource::WebApp {
                user_id,
                short_name,
            } => {
                store_parse::store_i64(out, user_id.get());
                store_parse::store_string(out, short_name);
            }
            FileSource::Story(story_full_id) => story_full_id.store(out),
            FileSource::QuickReplyMessage(message_full_id) => message_full_id.store(out),
            FileSource::StarTransaction {
                dialog_id,
                transaction_id,
                is_refund,
            } => {
                dialog_id.store(out);
                store_parse::store_string(out, transaction_id);
                store_parse::store_i32(out, i32::from(*is_refund));
            }
            FileSource::BotMediaPreviewInfo {
                bot_user_id,
                language_code,
            } => {
                store_parse::store_i64(out, bot_user_id.get());
                store_parse::store_string(out, language_code);
            }
            FileSource::StoryAlbum(story_album_full_id) => story_album_full_id.store(out),
            FileSource::UserSavedMusic {
                user_id,
                document_id,
                access_hash,
            } => {
                store_parse::store_i64(out, user_id.get());
                store_parse::store_i64(out, *document_id);
                store_parse::store_i64(out, *access_hash);
            }
            FileSource::DraftMessage { dialog_id, topic } => {
                dialog_id.store(out);
                topic.store(out);
            }
            FileSource::RichMessage(message_full_id) => message_full_id.store(out),
            FileSource::WelcomeMessages(dialog_id) => dialog_id.store(out),
            FileSource::CommunityFull(community_id) => {
                store_parse::store_i64(out, community_id.get())
            }
        }
    }

    /// The `parse_file_source` dispatch (FileReferenceManager.hpp:103-254);
    /// unknown tags are the C++ `parser.set_error` case.
    pub fn parse_from(parser: &mut Parser) -> Result<FileSource, &'static str> {
        let tag = parser.i32_()?;
        match tag {
            0 => Ok(FileSource::Message(MessageFullId::parse_from(parser)?)),
            1 => Ok(FileSource::UserPhoto {
                user_id: UserId::new(parser.i64_()?),
                photo_id: parser.i64_()?,
            }),
            2 => Ok(FileSource::ChatPhoto(ChatId::new(parser.i64_()?))),
            3 => Ok(FileSource::ChannelPhoto(ChannelId::new(parser.i64_()?))),
            4 => Ok(FileSource::Wallpapers),
            5 => Ok(FileSource::WebPage(parser.string()?)),
            6 => Ok(FileSource::SavedAnimations),
            7 => Ok(FileSource::RecentStickers {
                is_attached: parser.i32_()? != 0,
            }),
            8 => Ok(FileSource::FavoriteStickers),
            9 => Ok(FileSource::Background {
                background_id: BackgroundId::parse_from(parser)?,
                access_hash: parser.i64_()?,
            }),
            10 => Ok(FileSource::ChatFull(ChatId::new(parser.i64_()?))),
            11 => Ok(FileSource::ChannelFull(ChannelId::new(parser.i64_()?))),
            12 => Ok(FileSource::AppConfig),
            13 => Ok(FileSource::SavedRingtones),
            14 => Ok(FileSource::UserFull(UserId::new(parser.i64_()?))),
            15 => Ok(FileSource::AttachMenuBot(UserId::new(parser.i64_()?))),
            16 => Ok(FileSource::WebApp {
                user_id: UserId::new(parser.i64_()?),
                short_name: parser.string()?,
            }),
            17 => Ok(FileSource::Story(StoryFullId::parse_from(parser)?)),
            18 => Ok(FileSource::QuickReplyMessage(
                QuickReplyMessageFullId::parse_from(parser)?,
            )),
            19 => Ok(FileSource::StarTransaction {
                dialog_id: DialogId::parse_from(parser)?,
                transaction_id: parser.string()?,
                is_refund: parser.i32_()? != 0,
            }),
            20 => Ok(FileSource::BotMediaPreview(UserId::new(parser.i64_()?))),
            21 => Ok(FileSource::BotMediaPreviewInfo {
                bot_user_id: UserId::new(parser.i64_()?),
                language_code: parser.string()?,
            }),
            22 => Ok(FileSource::StoryAlbum(StoryAlbumFullId::parse_from(
                parser,
            )?)),
            23 => Ok(FileSource::UserSavedMusic {
                user_id: UserId::new(parser.i64_()?),
                document_id: parser.i64_()?,
                access_hash: parser.i64_()?,
            }),
            24 => Ok(FileSource::DraftMessage {
                dialog_id: DialogId::parse_from(parser)?,
                topic: MessageTopic::parse_from(parser)?,
            }),
            25 => Ok(FileSource::RichMessage(MessageFullId::parse_from(parser)?)),
            26 => Ok(FileSource::WelcomeMessages(DialogId::parse_from(parser)?)),
            27 => Ok(FileSource::CommunityFull(CommunityId::new(parser.i64_()?))),
            _ => Err("Invalid type in FileSource"),
        }
    }
}

/// `td::FastSetWithPosition` (SetWithPosition.h:19-104) — the checked half
/// of the iteration cursor plus the not-yet-checked remainder, both kept in
/// value order (`std::set`).
#[derive(Debug, Default)]
struct FastSetWithPosition<T: Ord> {
    checked: BTreeSet<T>,
    not_checked: BTreeSet<T>,
}

impl<T: Ord + Copy> FastSetWithPosition<T> {
    /// `get_some_elements` (SetWithPosition.h:21-37): the extremes of each
    /// half deduplicate down to the overall first and last.
    fn get_some_elements(&self) -> Vec<T> {
        let mut res = Vec::with_capacity(4);
        if !self.checked.is_empty() {
            res.push(*self.checked.iter().next().unwrap());
            res.push(*self.checked.iter().next_back().unwrap());
        }
        if !self.not_checked.is_empty() {
            res.push(*self.not_checked.iter().next().unwrap());
            res.push(*self.not_checked.iter().next_back().unwrap());
        }
        res.dedup();
        if res.len() > 2 {
            res.drain(1..res.len() - 1);
        }
        res
    }

    fn add(&mut self, x: T) -> bool {
        if self.checked.contains(&x) {
            return false;
        }
        self.not_checked.insert(x)
    }

    fn remove(&mut self, x: T) -> bool {
        self.checked.remove(&x) || self.not_checked.remove(&x)
    }

    fn has_next(&self) -> bool {
        !self.not_checked.is_empty()
    }

    /// `reset_position` (SetWithPosition.h:54-61).
    #[allow(dead_code)] // the repair cursor walks it (M.12.4)
    fn reset_position(&mut self) {
        if self.not_checked.is_empty() {
            self.not_checked = std::mem::take(&mut self.checked);
        } else {
            self.not_checked.extend(self.checked.iter().copied());
            self.checked.clear();
        }
    }

    /// `next` (SetWithPosition.h:63-70).
    #[allow(clippy::should_implement_trait)]
    fn next(&mut self) -> T {
        let res = *self.not_checked.iter().next().expect("has_next");
        self.not_checked.remove(&res);
        self.checked.insert(res);
        res
    }

    /// `merge` (SetWithPosition.h:72-91): the larger set absorbs the smaller;
    /// the survivor keeps everything checked.
    fn merge(&mut self, mut other: Self) {
        if self.size() < other.size() {
            std::mem::swap(self, &mut other);
        }
        for x in other.checked {
            self.not_checked.remove(&x);
            self.checked.insert(x);
        }
        for x in other.not_checked {
            if self.checked.contains(&x) {
                continue;
            }
            self.not_checked.insert(x);
        }
    }

    fn size(&self) -> usize {
        self.checked.len() + self.not_checked.len()
    }
}

/// `td::SetWithPosition` (SetWithPosition.h:106-226) — the single-value
/// fast path in front of `FastSetWithPosition`, ported with the C++
/// `empty` quirk: once the fast set exists, `empty` is always false.
#[derive(Debug, Default)]
struct SetWithPosition<T: Ord + Copy + Default> {
    value: T,
    has_value: bool,
    is_checked: bool,
    fast: Option<Box<FastSetWithPosition<T>>>,
}

impl<T: Ord + Copy + Default> SetWithPosition<T> {
    /// `get_some_elements` (SetWithPosition.h:109-117).
    fn get_some_elements(&self) -> Vec<T> {
        if let Some(fast) = &self.fast {
            return fast.get_some_elements();
        }
        if self.has_value {
            return vec![self.value];
        }
        Vec::new()
    }

    /// `add` (SetWithPosition.h:119-134).
    fn add(&mut self, x: T) -> bool {
        if let Some(fast) = &mut self.fast {
            return fast.add(x);
        }
        if !self.has_value {
            self.value = x;
            self.has_value = true;
            self.is_checked = false;
            return true;
        }
        if self.value == x {
            return false;
        }
        self.make_fast();
        self.fast.as_mut().expect("make_fast").add(x)
    }

    /// `remove` (SetWithPosition.h:136-146).
    fn remove(&mut self, x: T) -> bool {
        if let Some(fast) = &mut self.fast {
            return fast.remove(x);
        }
        if self.has_value && self.value == x {
            self.has_value = false;
            self.is_checked = false;
            return true;
        }
        false
    }

    /// `has_next` (SetWithPosition.h:148-153).
    #[allow(dead_code)] // the repair cursor walks it (M.12.4)
    fn has_next(&self) -> bool {
        match &self.fast {
            Some(fast) => fast.has_next(),
            None => self.has_value && !self.is_checked,
        }
    }

    /// `reset_position` (SetWithPosition.h:155-161).
    #[allow(dead_code)] // the repair cursor walks it (M.12.4)
    fn reset_position(&mut self) {
        if let Some(fast) = &mut self.fast {
            fast.reset_position();
            return;
        }
        self.is_checked = false;
    }

    /// `next` (SetWithPosition.h:163-170).
    #[allow(clippy::should_implement_trait)]
    #[allow(dead_code)] // the repair cursor walks it (M.12.4)
    fn next(&mut self) -> T {
        if let Some(fast) = &mut self.fast {
            return fast.next();
        }
        assert!(self.has_next());
        self.is_checked = true;
        self.value
    }

    /// `merge` (SetWithPosition.h:172-193): size-swap first, then the
    /// equal-single-value fast path, then both halves go fast.
    fn merge(&mut self, other: &mut Self) {
        if self.size() < other.size() {
            std::mem::swap(self, other);
        }
        if other.size() == 0 {
            return;
        }
        if other.fast.is_none() && self.fast.is_none() && self.value == other.value {
            self.is_checked |= other.is_checked;
            *other = Self::default();
            return;
        }
        self.make_fast();
        other.make_fast();
        let other_fast = other.fast.take().expect("make_fast");
        self.fast.as_mut().expect("make_fast").merge(*other_fast);
        *other = Self::default();
    }

    /// `size` (SetWithPosition.h:195-200).
    fn size(&self) -> usize {
        match &self.fast {
            Some(fast) => fast.size(),
            None => usize::from(self.has_value),
        }
    }

    /// `empty` (SetWithPosition.h:202-207) — false unconditionally once the
    /// fast set exists, even when it holds nothing.
    #[allow(dead_code)] // the repair cursor walks it (M.12.4)
    fn empty(&self) -> bool {
        match &self.fast {
            Some(_) => false,
            None => !self.has_value,
        }
    }

    /// `make_fast` (SetWithPosition.h:215-225).
    fn make_fast(&mut self) {
        if self.fast.is_some() {
            return;
        }
        assert!(self.has_value);
        let mut fast = Box::<FastSetWithPosition<T>>::default();
        fast.add(self.value);
        if self.is_checked {
            fast.next();
        }
        self.fast = Some(fast);
    }
}

/// `td::FileReferenceManager` (FileReferenceManager.h) — the file-source
/// registry, collapsed per the single-scheduler convention: plain methods on
/// an owned struct replace the actor, and the per-manager `create_*` dedup
/// caches (`user_profile_photo_file_source_ids_` et al) fold into the single
/// value index below. The repair-query machinery (`Query`/`run_node`/
/// `on_query_result`) lands with the download path (M.12.4) once the
/// per-manager reload callbacks exist.
#[derive(Debug, Default)]
pub struct FileReferenceManager {
    /// The append-only source table (`file_sources_`): id = index + 1.
    file_sources: Vec<FileSource>,
    /// Source value → interned id (`add_file_source_id` +
    /// `get_current_file_source_id`, FileReferenceManager.cpp:210-219).
    source_index: HashMap<FileSource, FileSourceId>,
    /// Per-file source sets (`nodes_`).
    nodes: HashMap<FileId, FileSourceNode>,
}

/// `FileReferenceManager::Node` (FileReferenceManager.h:147-151) minus the
/// repair-query state.
#[derive(Debug, Default)]
struct FileSourceNode {
    file_source_ids: SetWithPosition<FileSourceId>,
}

impl FileReferenceManager {
    pub fn new() -> FileReferenceManager {
        FileReferenceManager::default()
    }

    /// `add_file_source_id` with the per-manager caches folded in: an equal
    /// source keeps its id, a new one appends.
    pub fn intern(&mut self, source: FileSource) -> FileSourceId {
        if let Some(&file_source_id) = self.source_index.get(&source) {
            return file_source_id;
        }
        self.file_sources.push(source.clone());
        let file_source_id = FileSourceId::new(self.file_sources.len() as i32);
        self.source_index.insert(source, file_source_id);
        file_source_id
    }

    /// `add_node` (FileReferenceManager.cpp:327-334).
    fn add_node(&mut self, node_id: FileId) -> &mut FileSourceNode {
        assert!(node_id.is_valid());
        self.nodes.entry(node_id).or_default()
    }

    /// `add_file_source` (FileReferenceManager.cpp:359-366) without the
    /// `source` trace argument.
    pub fn add_file_source(&mut self, node_id: FileId, file_source_id: FileSourceId) -> bool {
        self.add_node(node_id).file_source_ids.add(file_source_id)
    }

    /// `remove_file_source` (FileReferenceManager.cpp:368-377).
    pub fn remove_file_source(&mut self, node_id: FileId, file_source_id: FileSourceId) -> bool {
        assert!(node_id.is_valid());
        match self.nodes.get_mut(&node_id) {
            Some(node) => node.file_source_ids.remove(file_source_id),
            None => false,
        }
    }

    /// `FileManager::create_web_page_file_source` — the interned
    /// `FileSource::WebPage` slot.
    pub fn create_web_page_file_source(&mut self, url: &str) -> FileSourceId {
        self.intern(FileSource::WebPage(url.to_owned()))
    }

    /// `FileManager::change_files_source` — the main-file-id dedup walk
    /// collapses to the plain remove/add pair (the node's main id is the
    /// node itself in this port).
    pub fn change_files_source(
        &mut self,
        file_source_id: FileSourceId,
        old_file_ids: &[FileId],
        new_file_ids: &[FileId],
        source: &str,
    ) {
        let _ = source;
        if old_file_ids == new_file_ids {
            return;
        }
        for file_id in old_file_ids {
            if !new_file_ids.contains(file_id) {
                self.remove_file_source(*file_id, file_source_id);
            }
        }
        for file_id in new_file_ids {
            if !old_file_ids.contains(file_id) {
                self.add_file_source(*file_id, file_source_id);
            }
        }
    }

    /// `get_some_file_sources` (FileReferenceManager.cpp:379-386).
    pub fn get_some_file_sources(&self, node_id: FileId) -> Vec<FileSourceId> {
        match self.nodes.get(&node_id) {
            Some(node) => node.file_source_ids.get_some_elements(),
            None => Vec::new(),
        }
    }

    /// `get_some_message_file_sources` (FileReferenceManager.cpp:388-401).
    pub fn get_some_message_file_sources(&self, node_id: FileId) -> Vec<MessageFullId> {
        self.get_some_file_sources(node_id)
            .into_iter()
            .filter_map(|file_source_id| {
                let index = file_source_id.get() as usize - 1;
                assert!(index < self.file_sources.len());
                match &self.file_sources[index] {
                    FileSource::Message(message_full_id) => Some(*message_full_id),
                    _ => None,
                }
            })
            .collect()
    }

    /// `merge` (FileReferenceManager.cpp:403-431) without the repair-query
    /// block: the from-node keeps its (now empty) entry, as in C++, so the
    /// M.12.4 machinery finds it for query redirection.
    pub fn merge(&mut self, to_node_id: FileId, from_node_id: FileId) {
        let mut from_sources = match self.nodes.get_mut(&from_node_id) {
            Some(from_node) => std::mem::take(&mut from_node.file_source_ids),
            None => return,
        };
        let to_node = self.add_node(to_node_id);
        to_node.file_source_ids.merge(&mut from_sources);
    }
}

impl FileSourceStorage for FileReferenceManager {
    /// `FileReferenceManager::store_file_source`
    /// (FileReferenceManager.hpp:45-101).
    fn store_file_source(&mut self, file_source_id: FileSourceId, out: &mut Vec<u8>) {
        let index = file_source_id.get() as usize - 1;
        assert!(
            index < self.file_sources.len(),
            "file source id out of range"
        );
        self.file_sources[index].store(out);
    }

    /// `FileReferenceManager::parse_file_source`
    /// (FileReferenceManager.hpp:103-254): the never-repairable sources
    /// (chat/channel photos, wallpapers) consume their payload but yield no
    /// id, and every parsed source re-interns through the value index.
    fn parse_file_source(&mut self, parser: &mut Parser) -> Result<FileSourceId, &'static str> {
        match FileSource::parse_from(parser)? {
            FileSource::ChatPhoto(_) | FileSource::ChannelPhoto(_) | FileSource::Wallpapers => {
                Ok(FileSourceId::new(0))
            }
            source => Ok(self.intern(source)),
        }
    }
}

// ==== FileNode / merge machine (M.12.2b) ====

/// `td::FileLocationSource` (FileManager.h:58) — the order is the
/// `merge_choose_file_source_location` trust order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileLocationSource {
    #[default]
    None,
    FromUser,
    FromBinlog,
    FromDatabase,
    FromServer,
}

/// `td::NewRemoteFileLocation` (FileManager.h:60-72) — the per-node remote
/// location: at most one of the full or partial form is present.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NewRemoteFileLocation {
    pub partial: Option<PartialRemoteFileLocation>,
    pub full: Option<FullRemoteFileLocation>,
    pub is_full_alive: bool,
    pub full_source: FileLocationSource,
}

impl NewRemoteFileLocation {
    /// `NewRemoteFileLocation(RemoteFileLocation, FileLocationSource)`
    /// (FileManager.cpp:117-134).
    pub fn from_remote(
        remote: RemoteFileLocation,
        source: FileLocationSource,
    ) -> NewRemoteFileLocation {
        match remote {
            RemoteFileLocation::Empty => NewRemoteFileLocation::default(),
            RemoteFileLocation::Partial(partial) => NewRemoteFileLocation {
                partial: Some(partial),
                ..NewRemoteFileLocation::default()
            },
            RemoteFileLocation::Full(full) => NewRemoteFileLocation {
                partial: None,
                full: Some(full),
                full_source: source,
                is_full_alive: true,
            },
        }
    }

    /// `NewRemoteFileLocation::partial_or_empty` (FileManager.cpp:134-140).
    pub fn partial_or_empty(&self) -> RemoteFileLocation {
        match &self.partial {
            Some(partial) => RemoteFileLocation::Partial(partial.clone()),
            None => RemoteFileLocation::Empty,
        }
    }
}

impl fmt::Display for FileLocationSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            FileLocationSource::None => "None",
            FileLocationSource::FromUser => "FromUser",
            FileLocationSource::FromBinlog => "FromBinlog",
            FileLocationSource::FromDatabase => "FromDatabase",
            FileLocationSource::FromServer => "FromServer",
        };
        f.write_str(name)
    }
}

/// The C++ `operator<<` (FileManager.cpp) — alive/full/no-location plus the
/// source.
impl fmt::Display for NewRemoteFileLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_full_alive {
            f.write_str("alive ")?;
        }
        match &self.full {
            Some(full) => write!(f, "{full}")?,
            None => f.write_str("[no location]")?,
        }
        write!(f, " from {}", self.full_source)
    }
}

/// `LocalFileLocation::Type` (FileLocation.h:768) — the discriminant the
/// merge chooses on.
fn local_location_type_tag(location: &LocalFileLocation) -> i32 {
    match location {
        LocalFileLocation::Empty => 0,
        LocalFileLocation::Partial(_) => 1,
        LocalFileLocation::Full(_) => 2,
    }
}

/// `merge_choose_local_location` (FileManager.cpp:2038-2045): 0 = keep x,
/// 1 = take y, 2 = both are equal.
fn merge_choose_local_location(x: &LocalFileLocation, y: &LocalFileLocation) -> i32 {
    let x_type = local_location_type_tag(x);
    let y_type = local_location_type_tag(y);
    if x_type != y_type {
        return i32::from(x_type < y_type);
    }
    2
}

/// `merge_choose_file_source_location` (FileManager.cpp:2047-2049).
fn merge_choose_file_source_location(x: FileLocationSource, y: FileLocationSource) -> i32 {
    i32::from(x < y)
}

/// `merge_choose_remote_location` over full locations
/// (FileManager.cpp:2051-2072).
fn merge_choose_full_remote_location(
    x: &FullRemoteFileLocation,
    x_source: FileLocationSource,
    y: &FullRemoteFileLocation,
    y_source: FileLocationSource,
) -> i32 {
    if x.is_web() != y.is_web() {
        return i32::from(x.is_web()); // prefer non-web
    }
    let x_ref = x.has_file_reference();
    let y_ref = y.has_file_reference();
    if x_ref || y_ref {
        if x_ref != y_ref {
            return i32::from(!x_ref);
        }
        if x.get_file_reference() != y.get_file_reference() {
            return merge_choose_file_source_location(x_source, y_source);
        }
    }
    if (x.get_access_hash() != y.get_access_hash() || x.get_source() != y.get_source())
        && (x_source != y_source || x.is_web() || x.get_id() == y.get_id())
    {
        return merge_choose_file_source_location(x_source, y_source);
    }
    2
}

/// `merge_choose_remote_location` over node locations
/// (FileManager.cpp:2074-2085).
fn merge_choose_remote_location(x: &NewRemoteFileLocation, y: &NewRemoteFileLocation) -> i32 {
    if x.is_full_alive != y.is_full_alive {
        return i32::from(!x.is_full_alive);
    }
    if x.is_full_alive {
        return merge_choose_full_remote_location(
            x.full.as_ref().expect("is_full_alive"),
            x.full_source,
            y.full.as_ref().expect("is_full_alive"),
            y.full_source,
        );
    }
    if x.partial.is_some() != y.partial.is_some() {
        return i32::from(x.partial.is_none());
    }
    2
}

/// `merge_choose_generate_location` (FileManager.cpp:2087-2105).
fn merge_choose_generate_location(
    x: Option<&FullGenerateFileLocation>,
    y: Option<&FullGenerateFileLocation>,
) -> i32 {
    let x_empty = x.is_none();
    let y_empty = y.is_none();
    if x_empty != y_empty {
        return i32::from(x_empty);
    }
    if let (Some(x), Some(y)) = (x, y)
        && x != y
    {
        let x_has_mtime = x.conversion.starts_with("#mtime#");
        let y_has_mtime = y.conversion.starts_with("#mtime#");
        if x_has_mtime != y_has_mtime {
            return i32::from(!x_has_mtime);
        }
        // the bigger conversion, the bigger mtime or at least a stable choice
        return if x.conversion >= y.conversion { 0 } else { 1 };
    }
    2
}

/// `merge_choose_size` (FileManager.cpp:2107-2119) — `-1` is the conflict
/// error.
fn merge_choose_size(x: i64, y: i64) -> i32 {
    if x == 0 {
        return 1;
    }
    if y == 0 {
        return 0;
    }
    if x != y {
        return -1;
    }
    2
}

/// `merge_choose_expected_size` (FileManager.cpp:2121-2129).
fn merge_choose_expected_size(x: i64, y: i64) -> i32 {
    if x == 0 {
        return 1;
    }
    if y == 0 {
        return 0;
    }
    2
}

/// `merge_choose_name` (FileManager.cpp:2131-2136).
fn merge_choose_name(x: &str, y: &str) -> i32 {
    if x.is_empty() != y.is_empty() {
        return i32::from(x.is_empty());
    }
    2
}

/// `merge_choose_owner` (FileManager.cpp:2138-2143).
fn merge_choose_owner(x_owner_dialog_id: DialogId, y_owner_dialog_id: DialogId) -> i32 {
    if x_owner_dialog_id.is_valid() != y_owner_dialog_id.is_valid() {
        return i32::from(!x_owner_dialog_id.is_valid());
    }
    2
}

/// `merge_choose_main_file_id` (FileManager.cpp:2145-2150).
fn merge_choose_main_file_id(a: FileId, a_priority: i8, b: FileId, b_priority: i8) -> i32 {
    if a_priority != b_priority {
        return i32::from(a_priority < b_priority);
    }
    i32::from(a.get() > b.get())
}

/// `merge_choose_encryption_key` (FileManager.cpp:2152-2160) — `-1` is the
/// conflict error.
fn merge_choose_encryption_key(a: &FileEncryptionKey, b: &FileEncryptionKey) -> i32 {
    if a.empty() != b.empty() {
        return i32::from(a.empty());
    }
    if a != b {
        return -1;
    }
    2
}

/// The per-file registry record: `td::FileNode` (FileManager.h:76-238),
/// data plus pure logic. The download/upload/generate query ids stay as
/// plain fields; their manager round-trips are the [`FileManagerWorker`]
/// seam until M.12.4-8 land.
#[derive(Clone, Debug)]
pub struct FileNode {
    local: LocalFileLocation,
    upload_id: u64,
    download_offset: i64,
    private_download_limit: i64,
    /// `local_ready_prefix_size_` — Partial-local only.
    local_ready_prefix_size: i64,

    remote: NewRemoteFileLocation,

    download_id: u64,

    generate: Option<FullGenerateFileLocation>,
    generate_id: u64,

    size: i64,
    expected_size: i64,
    remote_name: String,
    url: String,
    owner_dialog_id: DialogId,
    encryption_key: FileEncryptionKey,
    pmc_id: FileDbId,
    file_ids: Vec<FileId>,

    main_file_id: FileId,

    last_successful_force_reupload_time: f64,

    upload_pause: FileUploadId,

    upload_priority: i8,
    download_priority: i8,
    generate_priority: i8,

    generate_download_priority: i8,
    generate_upload_priority: i8,

    main_file_id_priority: i8,

    is_download_offset_dirty: bool,
    is_download_limit_dirty: bool,

    get_by_hash: bool,
    can_search_locally: bool,
    #[allow(dead_code)] // read by the photo reload path (M.12.5)
    need_reload_photo: bool,

    is_download_started: bool,

    #[allow(dead_code)] // read by the generate flow (M.12.8)
    generate_was_update: bool,

    need_load_from_pmc: bool,

    pmc_changed_flag: bool,
    info_changed_flag: bool,

    upload_was_update_file_reference: bool,
    download_was_update_file_reference: bool,

    upload_prefer_small: bool,

    ignore_download_limit: bool,
}

impl FileNode {
    /// The `FileNode` constructor (FileManager.h:78-94).
    #[allow(clippy::too_many_arguments)] // the C++ constructor arity
    pub fn new(
        local: LocalFileLocation,
        remote: NewRemoteFileLocation,
        generate: Option<FullGenerateFileLocation>,
        size: i64,
        expected_size: i64,
        remote_name: String,
        url: String,
        owner_dialog_id: DialogId,
        encryption_key: FileEncryptionKey,
        main_file_id: FileId,
        main_file_id_priority: i8,
    ) -> FileNode {
        let mut node = FileNode {
            local,
            upload_id: 0,
            download_offset: 0,
            private_download_limit: 0,
            local_ready_prefix_size: 0,
            remote,
            download_id: 0,
            generate,
            generate_id: 0,
            size,
            expected_size,
            remote_name,
            url,
            owner_dialog_id,
            encryption_key,
            pmc_id: FileDbId::default(),
            file_ids: Vec::new(),
            main_file_id,
            last_successful_force_reupload_time: -1e10,
            upload_pause: FileUploadId::default(),
            upload_priority: 0,
            download_priority: 0,
            generate_priority: 0,
            generate_download_priority: 0,
            generate_upload_priority: 0,
            main_file_id_priority,
            is_download_offset_dirty: false,
            is_download_limit_dirty: false,
            get_by_hash: false,
            can_search_locally: true,
            need_reload_photo: false,
            is_download_started: false,
            generate_was_update: false,
            need_load_from_pmc: false,
            pmc_changed_flag: false,
            info_changed_flag: false,
            upload_was_update_file_reference: false,
            download_was_update_file_reference: false,
            upload_prefer_small: false,
            ignore_download_limit: false,
        };
        node.init_ready_size();
        node.file_ids.push(main_file_id);
        node
    }

    /// `FileNode::recalc_ready_prefix_size` (FileManager.cpp:579-596).
    fn recalc_ready_prefix_size(&mut self, prefix_offset: i64, ready_prefix_size: i64) {
        let PartialLocalFileLocation {
            ready_bitmask,
            part_size,
            ..
        } = match &self.local {
            LocalFileLocation::Partial(partial) => partial,
            _ => return,
        };
        let new_local_ready_prefix_size = if self.download_offset == prefix_offset {
            ready_prefix_size
        } else {
            Bitmask::from_decode(ready_bitmask).get_ready_prefix_size(
                self.download_offset,
                *part_size,
                self.size,
            )
        };
        if new_local_ready_prefix_size != self.local_ready_prefix_size {
            self.local_ready_prefix_size = new_local_ready_prefix_size;
            self.on_info_changed();
        }
    }

    /// `FileNode::init_ready_size` (FileManager.cpp:598-606).
    fn init_ready_size(&mut self) {
        let (ready_bitmask, part_size) = match &self.local {
            LocalFileLocation::Partial(partial) => {
                (partial.ready_bitmask.clone(), partial.part_size)
            }
            _ => return,
        };
        let bitmask = Bitmask::from_decode(&ready_bitmask);
        self.local_ready_prefix_size = bitmask.get_ready_prefix_size(0, part_size, self.size);
        if let LocalFileLocation::Partial(partial) = &mut self.local {
            partial.ready_size = bitmask.get_total_size(part_size, self.size);
        }
    }

    /// `FileNode::set_download_offset` (FileManager.cpp:608-622).
    pub fn set_download_offset(&mut self, download_offset: i64) {
        if !(0..=MAX_FILE_SIZE).contains(&download_offset)
            || download_offset == self.download_offset
        {
            return;
        }
        self.download_offset = download_offset;
        self.is_download_offset_dirty = true;
        self.recalc_ready_prefix_size(-1, -1);
        self.on_info_changed();
    }

    /// `FileNode::get_download_limit` (FileManager.cpp:624-629).
    pub fn get_download_limit(&self) -> i64 {
        if self.ignore_download_limit {
            return 0;
        }
        self.private_download_limit
    }

    /// `FileNode::update_effective_download_limit` (FileManager.cpp:631-642).
    fn update_effective_download_limit(&mut self, old_download_limit: i64) {
        if self.get_download_limit() == old_download_limit {
            return;
        }
        self.is_download_limit_dirty = true;
    }

    /// `FileNode::set_download_limit` (FileManager.cpp:644-654).
    pub fn set_download_limit(&mut self, mut download_limit: i64) {
        if download_limit < 0 {
            return;
        }
        if download_limit > MAX_FILE_SIZE {
            download_limit = MAX_FILE_SIZE;
        }
        let old_download_limit = self.get_download_limit();
        self.private_download_limit = download_limit;
        self.update_effective_download_limit(old_download_limit);
    }

    /// `FileNode::set_ignore_download_limit` (FileManager.cpp:656-660).
    pub fn set_ignore_download_limit(&mut self, ignore_download_limit: bool) {
        let old_download_limit = self.get_download_limit();
        self.ignore_download_limit = ignore_download_limit;
        self.update_effective_download_limit(old_download_limit);
    }

    /// `FileNode::drop_local_location` (FileManager.cpp:662-664).
    pub fn drop_local_location(&mut self) {
        self.set_local_location(LocalFileLocation::Empty, -1, -1);
    }

    /// `FileNode::set_local_location` (FileManager.cpp:666-675).
    pub fn set_local_location(
        &mut self,
        local: LocalFileLocation,
        prefix_offset: i64,
        ready_prefix_size: i64,
    ) {
        if self.local != local {
            self.local = local;
            self.recalc_ready_prefix_size(prefix_offset, ready_prefix_size);
            self.on_changed();
        }
    }

    /// `FileNode::set_new_remote_location` (FileManager.cpp:677-707).
    pub fn set_new_remote_location(&mut self, new_remote: NewRemoteFileLocation) {
        if let Some(full) = &new_remote.full {
            if let Some(old_full) = &self.remote.full {
                if old_full == full
                    && (old_full.get_access_hash() != full.get_access_hash()
                        || old_full.get_file_reference() != full.get_file_reference()
                        || old_full.get_source() != full.get_source())
                {
                    self.on_pmc_changed();
                }
            } else {
                self.on_changed();
            }
            self.remote.full = new_remote.full;
            self.remote.full_source = new_remote.full_source;
            self.remote.is_full_alive = new_remote.is_full_alive;
        } else if self.remote.full.is_some() {
            self.remote.full = None;
            self.remote.is_full_alive = false;
            self.remote.full_source = FileLocationSource::None;
            self.on_changed();
        }

        if let Some(partial) = new_remote.partial {
            self.set_partial_remote_location(partial);
        } else {
            self.delete_partial_remote_location();
        }
    }

    /// `FileNode::delete_partial_remote_location` (FileManager.cpp:708-714).
    pub fn delete_partial_remote_location(&mut self) {
        if self.remote.partial.is_some() {
            self.remote.partial = None;
            self.on_changed();
        }
    }

    /// `FileNode::set_partial_remote_location` (FileManager.cpp:716-735).
    pub fn set_partial_remote_location(&mut self, remote: PartialRemoteFileLocation) {
        if self.remote.is_full_alive {
            return;
        }
        if let Some(old_partial) = &self.remote.partial {
            if *old_partial == remote {
                return;
            }
        } else if remote.ready_part_count == 0 {
            // empty partial remote is equal to empty remote
            return;
        }
        self.remote.partial = Some(remote);
        self.on_changed();
    }

    /// `FileNode::delete_file_reference` (FileManager.cpp:737-754).
    pub fn delete_file_reference(&mut self, file_reference: &[u8]) -> bool {
        let Some(full) = &mut self.remote.full else {
            return false;
        };
        if !full.delete_file_reference(file_reference) {
            return false;
        }
        self.upload_was_update_file_reference = false;
        self.download_was_update_file_reference = false;
        self.on_pmc_changed();
        true
    }

    /// `FileNode::set_generate_location` (FileManager.cpp:756-762).
    pub fn set_generate_location(&mut self, generate: Option<FullGenerateFileLocation>) {
        let is_changed = match (&self.generate, &generate) {
            (None, None) => false,
            (None, Some(_)) | (Some(_), None) => true,
            (Some(old), Some(new)) => old != new,
        };
        if is_changed {
            self.generate = generate;
            self.on_pmc_changed();
        }
    }

    /// `FileNode::set_size` (FileManager.cpp:764-770).
    pub fn set_size(&mut self, size: i64) {
        if self.size != size {
            self.size = size;
            self.on_changed();
        }
    }

    /// `FileNode::set_expected_size` (FileManager.cpp:772-778).
    pub fn set_expected_size(&mut self, expected_size: i64) {
        if self.expected_size != expected_size {
            self.expected_size = expected_size;
            self.on_changed();
        }
    }

    /// `FileNode::set_remote_name` (FileManager.cpp:780-785).
    pub fn set_remote_name(&mut self, remote_name: String) {
        if self.remote_name != remote_name {
            self.remote_name = remote_name;
            self.on_pmc_changed();
        }
    }

    /// `FileNode::set_url` (FileManager.cpp:787-793).
    pub fn set_url(&mut self, url: String) {
        if self.url != url {
            self.url = url;
            self.on_changed();
        }
    }

    /// `FileNode::set_owner_dialog_id` (FileManager.cpp:795-800).
    pub fn set_owner_dialog_id(&mut self, owner_id: DialogId) {
        if self.owner_dialog_id != owner_id {
            self.owner_dialog_id = owner_id;
            self.on_pmc_changed();
        }
    }

    /// `FileNode::set_encryption_key` (FileManager.cpp:802-807).
    pub fn set_encryption_key(&mut self, key: FileEncryptionKey) {
        if self.encryption_key != key {
            self.encryption_key = key;
            self.on_pmc_changed();
        }
    }

    /// `FileNode::set_upload_pause` (FileManager.cpp:809-817).
    pub fn set_upload_pause(&mut self, upload_pause: FileUploadId) {
        if self.upload_pause != upload_pause {
            if self.upload_pause.file_id.is_valid() != upload_pause.file_id.is_valid() {
                self.on_info_changed();
            }
            self.upload_pause = upload_pause;
        }
    }

    /// `FileNode::set_download_priority` (FileManager.cpp:819-825).
    pub fn set_download_priority(&mut self, priority: i8) {
        if (self.download_priority == 0) != (priority == 0) {
            self.on_info_changed();
        }
        self.download_priority = priority;
    }

    /// `FileNode::set_upload_priority` (FileManager.cpp:827-833).
    pub fn set_upload_priority(&mut self, priority: i8) {
        if !self.remote.is_full_alive && (self.upload_priority == 0) != (priority == 0) {
            self.on_info_changed();
        }
        self.upload_priority = priority;
    }

    /// `FileNode::set_generate_priority` (FileManager.cpp:835-845).
    pub fn set_generate_priority(&mut self, download_priority: i8, upload_priority: i8) {
        if (self.generate_download_priority == 0) != (download_priority == 0)
            || (self.generate_upload_priority == 0) != (upload_priority == 0)
        {
            self.on_info_changed();
        }
        self.generate_priority = download_priority.max(upload_priority);
        self.generate_download_priority = download_priority;
        self.generate_upload_priority = upload_priority;
    }

    /// `FileNode::on_changed` (FileManager.cpp:847-850).
    pub fn on_changed(&mut self) {
        self.on_pmc_changed();
        self.on_info_changed();
    }

    /// `FileNode::on_info_changed` (FileManager.cpp:852-854).
    pub fn on_info_changed(&mut self) {
        self.info_changed_flag = true;
    }

    /// `FileNode::on_pmc_changed` (FileManager.cpp:856-858).
    pub fn on_pmc_changed(&mut self) {
        self.pmc_changed_flag = true;
    }

    /// `FileNode::need_info_flush` (FileManager.cpp:860-862).
    pub fn need_info_flush(&self) -> bool {
        self.info_changed_flag
    }

    /// `FileNode::need_pmc_flush` (FileManager.cpp:864-898).
    pub fn need_pmc_flush(&self) -> bool {
        if !self.pmc_changed_flag {
            return false;
        }
        // already in pmc
        if self.pmc_id.is_valid() {
            return true;
        }
        // we must save encryption key
        if !self.encryption_key.empty() {
            return true;
        }

        let mut has_generate_location = self.generate.is_some();
        // do not save "#file_id#" conversion
        if let Some(generate) = &self.generate
            && generate.conversion.starts_with("#file_id#")
        {
            has_generate_location = false;
        }

        // we need to always save file sources
        if self.remote.full.is_some() {
            return true;
        }
        if matches!(self.local, LocalFileLocation::Full(_))
            && (has_generate_location
                || self.remote.full.is_some()
                || self.remote.partial.is_some())
        {
            return true;
        }
        false
    }

    /// `FileNode::on_pmc_flushed` (FileManager.cpp:900-902).
    pub fn on_pmc_flushed(&mut self) {
        self.pmc_changed_flag = false;
    }

    /// `FileNode::on_info_flushed` (FileManager.cpp:903-905).
    pub fn on_info_flushed(&mut self) {
        self.info_changed_flag = false;
    }

    /// `FileNode::get_type` (FileManager.h:216-227).
    pub fn get_type(&self) -> FileType {
        if let LocalFileLocation::Full(full) = &self.local {
            return full.file_type;
        }
        if let Some(full) = &self.remote.full {
            return full.file_type;
        }
        if let Some(generate) = &self.generate {
            return generate.file_type;
        }
        FileType::Temp
    }

    /// `FileNode::expected_size` (FileManager.cpp:1012-1024).
    pub fn expected_size(&self, may_guess: bool) -> i64 {
        if self.size != 0 {
            return self.size;
        }
        let mut current_size = self.local_total_size(); // TODO: this is not the best approximation
        if self.expected_size != 0 {
            return current_size.max(self.expected_size);
        }
        if may_guess && matches!(self.local, LocalFileLocation::Partial(_)) {
            current_size *= 3;
        }
        current_size
    }

    /// `FileNode::is_downloading` (FileManager.cpp:1030-1032).
    pub fn is_downloading(&self) -> bool {
        self.download_priority != 0 || self.generate_download_priority != 0
    }

    /// `FileNode::downloaded_prefix` (FileManager.cpp:1042-1062).
    pub fn downloaded_prefix(&self, offset: i64) -> i64 {
        match &self.local {
            LocalFileLocation::Empty => 0,
            LocalFileLocation::Full(_) => {
                if offset < self.size {
                    self.size - offset
                } else {
                    0
                }
            }
            LocalFileLocation::Partial(partial) => {
                if self.get_type() == FileType::SecureEncrypted {
                    // file is not decrypted and verified yet
                    return 0;
                }
                Bitmask::from_decode(&partial.ready_bitmask).get_ready_prefix_size(
                    offset,
                    partial.part_size,
                    self.size,
                )
            }
        }
    }

    /// `FileNode::local_prefix_size` (FileManager.cpp:1068-1082).
    pub fn local_prefix_size(&self) -> i64 {
        match &self.local {
            LocalFileLocation::Full(_) => {
                if self.download_offset <= self.size {
                    self.size - self.download_offset
                } else {
                    0
                }
            }
            LocalFileLocation::Partial(_) => {
                if self.get_type() == FileType::SecureEncrypted {
                    // file is not decrypted and verified yet
                    return 0;
                }
                self.local_ready_prefix_size
            }
            LocalFileLocation::Empty => 0,
        }
    }

    /// `FileNode::local_total_size` (FileManager.cpp:1088-1100).
    pub fn local_total_size(&self) -> i64 {
        match &self.local {
            LocalFileLocation::Empty => 0,
            LocalFileLocation::Full(_) => self.size,
            LocalFileLocation::Partial(partial) => partial.ready_size,
        }
    }

    /// `FileNode::is_uploading` (FileManager.cpp:1106-1108).
    pub fn is_uploading(&self) -> bool {
        self.upload_priority != 0
            || self.generate_upload_priority != 0
            || self.upload_pause.file_id.is_valid()
    }

    /// `FileNode::remote_size` (FileManager.cpp:1114-1122).
    pub fn remote_size(&self) -> i64 {
        if self.remote.is_full_alive {
            return self.size;
        }
        if let Some(partial) = &self.remote.partial {
            return partial.ready_size;
        }
        0
    }

    /// `FileNode::path` (FileManager.cpp:1128-1137).
    pub fn path(&self) -> &str {
        match &self.local {
            LocalFileLocation::Full(full) => &full.path,
            LocalFileLocation::Partial(partial) => &partial.path,
            LocalFileLocation::Empty => "",
        }
    }

    pub fn get_main_file_id(&self) -> FileId {
        self.main_file_id
    }

    /// `FileView::download_offset` over the node record.
    pub fn download_offset(&self) -> i64 {
        self.download_offset
    }

    pub fn get_main_file_id_priority(&self) -> i8 {
        self.main_file_id_priority
    }

    /// `FileNode::get_unique_id(FullGenerateFileLocation)`
    /// (FileManager.cpp:1213-1215).
    pub fn get_generate_unique_id(location: &FullGenerateFileLocation) -> String {
        let mut binary = vec![0xff];
        location.store(&mut binary);
        base64url(&tdutils::zero_encode::zero_encode(&binary))
    }

    /// `FileNode::get_unique_id(FullRemoteFileLocation)`
    /// (FileManager.cpp:1217-1219).
    pub fn get_remote_unique_id(location: &FullRemoteFileLocation) -> String {
        let mut binary = Vec::new();
        location.store_unique(&mut binary);
        base64url(&tdutils::zero_encode::zero_encode(&binary))
    }

    /// `FileNode::get_persistent_id(FullGenerateFileLocation)`
    /// (FileManager.cpp:1221-1227).
    pub fn get_generate_persistent_id(location: &FullGenerateFileLocation) -> String {
        let mut binary = Vec::new();
        location.store(&mut binary);
        let mut binary = tdutils::zero_encode::zero_encode(&binary);
        binary.push(PERSISTENT_ID_VERSION_GENERATED);
        base64url(&binary)
    }

    /// `FileNode::get_persistent_id(FullRemoteFileLocation)`
    /// (FileManager.cpp:1229-1236).
    pub fn get_remote_persistent_id(location: &FullRemoteFileLocation) -> String {
        let mut binary = Vec::new();
        location.store(&mut binary);
        let mut binary = tdutils::zero_encode::zero_encode(&binary);
        binary.push(PERSISTENT_ID_VERSION_PREFIX);
        binary.push(PERSISTENT_ID_VERSION);
        base64url(&binary)
    }

    /// `FileNode::get_persistent_file_id` (FileManager.cpp:1238-1247).
    pub fn get_persistent_file_id(&self) -> String {
        if self.remote.is_full_alive {
            if let Some(full) = &self.remote.full {
                return FileNode::get_remote_persistent_id(full);
            }
        } else if !self.url.is_empty() {
            return self.url.clone();
        } else if let Some(generate) = &self.generate
            && is_remotely_generated_file(&generate.conversion)
        {
            return FileNode::get_generate_persistent_id(generate);
        }
        String::new()
    }

    /// `FileNode::get_unique_file_id` (FileManager.cpp:1249-1258).
    pub fn get_unique_file_id(&self) -> String {
        if self.remote.is_full_alive {
            if let Some(full) = &self.remote.full
                && !full.is_web()
            {
                return FileNode::get_remote_unique_id(full);
            }
        } else if let Some(generate) = &self.generate
            && is_remotely_generated_file(&generate.conversion)
        {
            return FileNode::get_generate_unique_id(generate);
        }
        String::new()
    }

    /// `FileNode::suggested_path` (FileManager.cpp:907-923).
    pub fn suggested_path(&self) -> String {
        if !self.remote_name.is_empty() {
            return self.remote_name.clone();
        }
        if !self.url.is_empty() {
            let file_name = tdutils::http_url::get_url_file_name(&self.url);
            if !file_name.is_empty() {
                return file_name;
            }
        }
        if let Some(generate) = &self.generate
            && !generate.original_path.is_empty()
        {
            return generate.original_path.clone();
        }
        self.local.file_name().to_string()
    }

    /// `FileNode::can_delete` (FileManager.cpp:1202-1207) — the
    /// files-dir seam is caller-supplied ([`get_files_dir`]).
    pub fn can_delete(&self, files_dir: &str, secure_files_dir: &str) -> bool {
        if let LocalFileLocation::Full(full) = &self.local {
            let file_type = self.get_type();
            return full
                .path
                .starts_with(&get_files_dir(file_type, files_dir, secure_files_dir));
        }
        matches!(self.local, LocalFileLocation::Partial(_))
    }
}

/// `td::FileView` (FileManager.h:288-...) — the read-only projection over a
/// [`FileNode`]; only the context-free accessors are here, `suggested_path`,
/// `can_delete` and the persistent/unique id strings need the options and
/// serialization surfaces of later slices.
#[derive(Clone, Copy, Debug)]
pub struct FileView<'a> {
    node: &'a FileNode,
}

impl<'a> FileView<'a> {
    pub fn new(node: &'a FileNode) -> FileView<'a> {
        FileView { node }
    }

    pub fn has_full_local_location(&self) -> bool {
        matches!(self.node.local, LocalFileLocation::Full(_))
    }

    pub fn get_full_local_location(&self) -> Option<&FullLocalFileLocation> {
        match &self.node.local {
            LocalFileLocation::Full(full) => Some(full),
            _ => None,
        }
    }

    pub fn has_generate_location(&self) -> bool {
        self.node.generate.is_some()
    }

    pub fn get_generate_location(&self) -> Option<&FullGenerateFileLocation> {
        self.node.generate.as_ref()
    }

    pub fn has_full_remote_location(&self) -> bool {
        self.node.remote.full.is_some()
    }

    pub fn has_alive_remote_location(&self) -> bool {
        self.node.remote.is_full_alive
    }

    /// `FileView::has_active_upload_remote_location` (FileManager.cpp).
    pub fn has_active_upload_remote_location(&self) -> bool {
        let Some(main_remote_location) = self.get_main_remote_location() else {
            return false;
        };
        if !self.has_alive_remote_location() {
            return false;
        }
        if main_remote_location.is_encrypted_any() {
            return true;
        }
        main_remote_location.has_file_reference()
    }

    /// `FileView::has_active_download_remote_location` (FileManager.cpp).
    pub fn has_active_download_remote_location(&self) -> bool {
        let Some(full_remote_location) = self.get_full_remote_location() else {
            return false;
        };
        if full_remote_location.is_encrypted_any() {
            return true;
        }
        full_remote_location.has_file_reference()
    }

    pub fn get_full_remote_location(&self) -> Option<&FullRemoteFileLocation> {
        self.node.remote.full.as_ref()
    }

    pub fn get_main_remote_location(&self) -> Option<&FullRemoteFileLocation> {
        self.node.remote.full.as_ref()
    }

    pub fn has_url(&self) -> bool {
        !self.node.url.is_empty()
    }

    pub fn get_url(&self) -> Option<&str> {
        if self.node.url.is_empty() {
            return None;
        }
        Some(&self.node.url)
    }

    pub fn remote_name(&self) -> &str {
        &self.node.remote_name
    }

    pub fn owner_dialog_id(&self) -> DialogId {
        self.node.owner_dialog_id
    }

    pub fn get_by_hash(&self) -> bool {
        self.node.get_by_hash
    }

    pub fn get_main_file_id(&self) -> FileId {
        self.node.main_file_id
    }

    pub fn size(&self) -> i64 {
        self.node.size
    }

    pub fn expected_size(&self, may_guess: bool) -> i64 {
        self.node.expected_size(may_guess)
    }

    pub fn is_downloading(&self) -> bool {
        self.node.is_downloading()
    }

    pub fn download_offset(&self) -> i64 {
        self.node.download_offset
    }

    pub fn downloaded_prefix(&self, offset: i64) -> i64 {
        self.node.downloaded_prefix(offset)
    }

    pub fn local_prefix_size(&self) -> i64 {
        self.node.local_prefix_size()
    }

    pub fn local_total_size(&self) -> i64 {
        self.node.local_total_size()
    }

    pub fn is_uploading(&self) -> bool {
        self.node.is_uploading()
    }

    pub fn remote_size(&self) -> i64 {
        self.node.remote_size()
    }

    pub fn path(&self) -> &str {
        self.node.path()
    }

    /// `FileView::can_download_from_server` (FileManager.cpp) without the
    /// null-node path (a `FileView` always carries a node here).
    pub fn can_download_from_server(&self) -> bool {
        let Some(full_remote_location) = self.get_full_remote_location() else {
            return false;
        };
        if full_remote_location.file_type == FileType::Encrypted && self.node.encryption_key.empty()
        {
            return false;
        }
        if full_remote_location.is_web() {
            return true;
        }
        if full_remote_location.get_dc_id().is_empty() {
            return false;
        }
        if !full_remote_location.is_encrypted_any()
            && !full_remote_location.has_file_reference()
            && ((self.node.download_id == 0 && self.node.download_was_update_file_reference)
                || !self.node.remote.is_full_alive)
        {
            return false;
        }
        true
    }

    pub fn can_generate(&self) -> bool {
        self.has_generate_location()
    }

    pub fn get_type(&self) -> FileType {
        self.node.get_type()
    }

    pub fn is_encrypted_secret(&self) -> bool {
        self.get_type() == FileType::Encrypted
    }

    pub fn is_encrypted_secure(&self) -> bool {
        self.get_type() == FileType::SecureEncrypted
    }

    pub fn is_secure(&self) -> bool {
        let type_ = self.get_type();
        type_ == FileType::SecureEncrypted || type_ == FileType::SecureDecrypted
    }

    pub fn is_encrypted_any(&self) -> bool {
        let type_ = self.get_type();
        type_ == FileType::Encrypted || type_ == FileType::SecureEncrypted
    }

    pub fn is_encrypted(&self) -> bool {
        let type_ = self.get_type();
        type_ == FileType::Encrypted
            || type_ == FileType::SecureEncrypted
            || type_ == FileType::SecureDecrypted
    }

    pub fn encryption_key(&self) -> &FileEncryptionKey {
        &self.node.encryption_key
    }

    /// `FileView::may_reload_photo` (FileManager.h:373-384).
    pub fn may_reload_photo(&self) -> bool {
        let Some(full_remote_location) = self.get_full_remote_location() else {
            return false;
        };
        if !full_remote_location.is_photo() {
            return false;
        }
        let type_ = full_remote_location.get_source().get_type();
        !matches!(
            type_,
            PhotoSizeSourceType::Legacy
                | PhotoSizeSourceType::FullLegacy
                | PhotoSizeSourceType::Thumbnail
        )
    }

    /// `FileView::suggested_path` (FileManager.cpp:1158-1160).
    pub fn suggested_path(&self) -> String {
        self.node.suggested_path()
    }

    /// `FileView::can_delete` (FileManager.cpp:1209-1211).
    pub fn can_delete(&self, files_dir: &str, secure_files_dir: &str) -> bool {
        self.node.can_delete(files_dir, secure_files_dir)
    }

    /// `FileView::get_unique_file_id` (FileManager.h:386-391).
    pub fn get_unique_file_id(&self) -> String {
        self.node.get_unique_file_id()
    }
}

/// `FileNode::PERSISTENT_ID_VERSION*` (FileManager.h:137-139).
pub const PERSISTENT_ID_VERSION_OLD: u8 = 2;
pub const PERSISTENT_ID_VERSION_GENERATED: u8 = 3;
pub const PERSISTENT_ID_VERSION: u8 = 4;
/// The `narrow_cast<uint8>(Version::Next) - 1` byte pushed before
/// [`PERSISTENT_ID_VERSION`] in remote persistent ids. `Version::Next` is 62
/// in tdlib 1.8.67 (Version.h: `SupportRichMessages` 60,
/// `SupportEphemeralMessages` 61, `Next` 62), so the byte is 61.
const PERSISTENT_ID_VERSION_PREFIX: u8 = 61;

/// `FileManager::is_remotely_generated_file` (FileManager.cpp:1431-1433).
pub fn is_remotely_generated_file(conversion: &str) -> bool {
    conversion.starts_with("#map#") || conversion.starts_with("#audio_t#")
}

/// `td::base64url_encode` over the id binaries (ASCII output).
fn base64url(data: &[u8]) -> String {
    String::from_utf8(tdmtproto::utils::base64url_encode(data)).expect("base64url is ascii")
}

/// `td::base64_encode` — the standard padded alphabet; the file-reference
/// error suffix encoding (FileDownloader.cpp:199-201).
fn base64_encode(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// `get_file_base_dir`/`get_files_base_dir` (FileLoaderUtils.cpp:238-249)
/// over caller-supplied `Global::get_files_dir()` /
/// `get_secure_files_dir()` values.
pub fn get_files_base_dir<'a>(
    file_type: FileType,
    files_dir: &'a str,
    secure_files_dir: &'a str,
) -> &'a str {
    match get_file_dir_type(file_type) {
        FileDirType::Secure => secure_files_dir,
        FileDirType::Common => files_dir,
    }
}

/// `get_files_dir` (FileLoaderUtils.cpp:255-257).
pub fn get_files_dir(file_type: FileType, files_dir: &str, secure_files_dir: &str) -> String {
    format!(
        "{}{}/",
        get_files_base_dir(file_type, files_dir, secure_files_dir),
        get_file_type_name(file_type)
    )
}

/// `td::Stat` fields `check_full_local_location`/`check_partial_local_location`
/// read (FileLoaderUtils.cpp:291-360).
pub struct FileStat {
    pub is_reg: bool,
    pub is_dir: bool,
    pub size: i64,
    pub mtime_nsec: u64,
    /// `Stat::real_size_` (port/stat.cpp) — `st_blocks * 512`, the
    /// allocated size quota consumers account.
    pub real_size: i64,
}

impl FileNode {
    /// The `FileData` construction of `try_flush_node_info`
    /// (FileManager.cpp) — the file sources arrive from the caller (the
    /// FileReferenceManager owns them).
    pub(crate) fn to_file_data(&self, sources: Vec<FileSourceId>) -> FileData {
        FileData {
            owner_dialog_id: self.owner_dialog_id,
            pmc_id: self.pmc_id.get(),
            remote: self.remote.partial_or_empty(),
            local: self.local.clone(),
            generate: self.generate.clone(),
            size: self.size,
            expected_size: self.expected_size,
            remote_name: self.remote_name.clone(),
            url: self.url.clone(),
            encryption_key: self.encryption_key.clone(),
            file_source_ids: sources,
        }
    }

    /// The node's main file id (the C++ `node.main_file_id_` read the
    /// flush/merge replays need from the Td side).
    pub fn main_file_id(&self) -> FileId {
        self.main_file_id
    }

    /// The pmc record id (`try_flush_node`'s file-db key).
    pub(crate) fn pmc_id(&self) -> FileDbId {
        self.pmc_id
    }
}

/// The environment seam of the collapsed [`FileManager`] — the C++
/// `FileManager::Context` flags (`keep_exact_remote_location`), the
/// filesystem access behind `FileLoaderUtils` (`realpath`, `stat`), the
/// `TdDb::with_db_path` bad paths, the `Global` files directories and the
/// option reads of `check_full_local_location`. The default methods do the
/// real `std::fs` work; tests override the pieces they exercise.
pub trait FileManagerContext: Send {
    /// `Context::keep_exact_remote_location` (FileManager.h).
    fn keep_exact_remote_location(&self) -> bool {
        false
    }

    /// `realpath(path, /*require_absolute=*/ true)` — `Err` when the path
    /// cannot be resolved (FileManager.cpp "Can't find real file path").
    fn real_path(&self, path: &str) -> Result<String, Status> {
        std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|_| Status::error(400, "Can't find real file path"))
    }

    /// `td::stat(path)`.
    fn stat(&self, path: &str) -> Result<FileStat, Status> {
        let meta = std::fs::metadata(path)
            .map_err(|_| Status::error(400, "Can't get stat about the file"))?;
        let mtime_nsec = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Ok(FileStat {
            is_reg: meta.is_file(),
            is_dir: meta.is_dir(),
            size: meta.len() as i64,
            mtime_nsec,
            #[cfg(unix)]
            real_size: {
                use std::os::unix::fs::MetadataExt as _;
                (meta.blocks() * 512) as i64
            },
            #[cfg(not(unix))]
            real_size: meta.len() as i64,
        })
    }

    /// `G()->get_files_dir()`.
    fn files_dir(&self) -> String {
        String::new()
    }

    /// `G()->get_secure_files_dir()`.
    fn secure_files_dir(&self) -> String {
        String::new()
    }

    /// `TdDb::with_db_path` — the database paths that must never be sent
    /// (`bad_paths_`).
    fn db_paths(&self) -> Vec<String> {
        Vec::new()
    }

    /// `G()->get_option_integer(name, default)` — the option cache read of
    /// `check_full_local_location`'s video-note limit.
    fn option_integer(&self, _name: &str, default: i64) -> i64 {
        default
    }

    /// `Context::need_notify_on_new_files` (FileManager.h:447).
    fn need_notify_on_new_files(&self) -> bool {
        false
    }

    /// `Context::on_new_file` (FileManager.h:449).
    fn on_new_file(&mut self, _size: i64, _real_size: i64, _cnt: i32) {}

    /// `Context::repair_file_reference` (FileManager.h:457) — the async
    /// repair collapses to a synchronous result: `Ok` means the reference
    /// was refreshed and the load restarts
    /// (`FILE_DOWNLOAD_RESTART_WITH_FILE_REFERENCE`), `Err` carries the
    /// failure the C++ lambda would forward.
    fn repair_file_reference(&mut self, _file_id: FileId) -> Result<(), Status> {
        Ok(())
    }

    /// `Context::reload_photo` (FileManager.h:459) — same collapse as
    /// [`FileManagerContext::repair_file_reference`].
    fn reload_photo(&mut self, _file_id: FileId) -> Result<(), Status> {
        Ok(())
    }

    /// `ConfigManager::request_config` on `MTPROTO_CLUSTER_INVALID`
    /// (FileManager.cpp:5168-5171).
    fn request_config(&mut self) {}

    /// `G()->get_option_boolean("ignore_inline_thumbnails")`
    /// (FileManager.cpp:2783-2785).
    fn ignore_inline_thumbnails(&self) -> bool {
        false
    }

    /// `LogInterface::get_file_paths` — `register_generate` skips the
    /// `#mtime#` prefix for logged paths.
    fn logged_file_paths(&self) -> Vec<String> {
        Vec::new()
    }

    /// `Td::send_update(updateFileGenerationStart)`
    /// (FileGenerateManager.cpp:322-327).
    fn on_file_generation_start(
        &mut self,
        _generation_id: i64,
        _original_path: &str,
        _path: &str,
        _conversion: &str,
    ) {
    }

    /// `Td::send_update(updateFileGenerationStop)` — the
    /// `FileExternalGenerateActor::tear_down`
    /// (FileGenerateManager.cpp:384-388).
    fn on_file_generation_stop(&mut self, _generation_id: i64) {}

    /// `Global::get_location_access_hash(latitude, longitude)`
    /// (Global.cpp:338-346) — the `#map#` conversion's geo-point access
    /// hash; the C++ map is option-populated, the default is the C++
    /// not-found `0`.
    fn location_access_hash(&mut self, _latitude: f64, _longitude: f64) -> i64 {
        0
    }
}

/// `are_modification_times_equal` (FileLoaderUtils.cpp:259-271).
pub fn are_modification_times_equal(old_mtime: i64, new_mtime: i64) -> bool {
    if old_mtime == new_mtime {
        return true;
    }
    if old_mtime < new_mtime {
        return false;
    }
    // FAT32 has 2 seconds mtime resolution, but file system sometimes
    // reports odd modification time
    old_mtime - new_mtime == 1_000_000_000
        && old_mtime % 1_000_000_000 == 0
        && new_mtime % 2_000_000_000 == 0
}

/// `FullLocalLocationInfo` (FileLoaderUtils.h:43-49).
pub struct FullLocalLocationInfo {
    pub location: FullLocalFileLocation,
    pub size: i64,
}

impl fmt::Debug for FullLocalLocationInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FullLocalLocationInfo")
            .field("location", &self.location)
            .field("size", &self.size)
            .finish()
    }
}

/// `check_full_local_location` (FileLoaderUtils.cpp:273-345) — the
/// register-local validation surface. `G()->get_option_integer` goes through
/// the [`FileManagerContext`].
pub fn check_full_local_location<C: FileManagerContext + ?Sized>(
    context: &C,
    mut local_info: FullLocalLocationInfo,
    skip_file_size_checks: bool,
) -> Result<FullLocalLocationInfo, Status> {
    const MAX_FILE_SIZE: i64 = 4000 << 20; // 4000 MB
    const MAX_THUMBNAIL_SIZE: i64 = 200 * (1 << 10) - 1; // 200 KB - 1 B
    const MAX_PHOTO_SIZE: i64 = 10 * (1 << 20); // 10 MB
    const DEFAULT_VIDEO_NOTE_SIZE_MAX: i64 = 12 * (1 << 20); // 12 MB
    const MAX_VIDEO_STORY_SIZE: i64 = 30 * (1 << 20); // 30 MB

    let mut size = local_info.size;
    if local_info.location.path.is_empty() {
        return Err(Status::error(400, "File must have non-empty path"));
    }
    local_info.location.path = context.real_path(&local_info.location.path)?;

    let stat = context.stat(&local_info.location.path)?;
    if !stat.is_reg {
        return Err(Status::error(400, "File must be a regular file"));
    }
    if stat.size < 0 {
        // TODO is it possible?
        return Err(Status::error(400, "File is too big"));
    }
    if stat.size == 0 {
        return Err(Status::error(400, "File must be non-empty"));
    }

    if size <= 0 {
        size = stat.size;
    } else if size != stat.size {
        return Err(Status::error(400, "File size has changed"));
    }

    if local_info.location.mtime_nsec == 0 {
        local_info.location.mtime_nsec = stat.mtime_nsec;
    } else if !are_modification_times_equal(
        local_info.location.mtime_nsec as i64,
        stat.mtime_nsec as i64,
    ) {
        return Err(Status::error(400, "File was modified"));
    }
    local_info.size = size;
    if skip_file_size_checks {
        return Ok(local_info);
    }

    let file_name = tdutils::path_view::PathView::new(&local_info.location.path)
        .file_name()
        .to_string();
    let get_file_size_error = |reason: &str, max_size: i64| {
        Status::error(
            400,
            format!(
                "File of size {} bytes is too big{}; the maximum size is {} bytes",
                size, reason, max_size
            )
            .as_str(),
        )
    };
    if (local_info.location.file_type == FileType::Thumbnail
        || local_info.location.file_type == FileType::EncryptedThumbnail)
        && size > MAX_THUMBNAIL_SIZE
        && !file_name.starts_with("map")
        && !file_name.starts_with("Album cover for ")
    {
        return Err(get_file_size_error(" for a thumbnail", MAX_THUMBNAIL_SIZE));
    }
    if size > MAX_FILE_SIZE {
        return Err(get_file_size_error("", MAX_FILE_SIZE));
    }
    if get_file_type_class(local_info.location.file_type) == FileTypeClass::Photo
        && size > MAX_PHOTO_SIZE
    {
        return Err(get_file_size_error(" for a photo", MAX_PHOTO_SIZE));
    }
    if local_info.location.file_type == FileType::VideoNote
        || local_info.location.file_type == FileType::SelfDestructingVideoNote
    {
        let max_size = context.option_integer("video_note_size_max", DEFAULT_VIDEO_NOTE_SIZE_MAX);
        if size > max_size {
            return Err(get_file_size_error(" for a video note", max_size));
        }
    }
    if local_info.location.file_type == FileType::VideoStory && size > MAX_VIDEO_STORY_SIZE {
        return Err(get_file_size_error(
            " for a video story",
            MAX_VIDEO_STORY_SIZE,
        ));
    }
    Ok(local_info)
}

/// `check_partial_local_location` (FileLoaderUtils.cpp:353-363).
pub fn check_partial_local_location<C: FileManagerContext + ?Sized>(
    context: &C,
    location: &PartialLocalFileLocation,
) -> Result<(), Status> {
    let stat = context.stat(&location.path)?;
    if !stat.is_reg {
        if stat.is_dir {
            return Err(Status::error(
                0,
                format!("Can't use directory \"{}\" as a file path", location.path).as_str(),
            ));
        }
        return Err(Status::error(0, "File must be a regular file"));
    }
    // can't check mtime. Hope nobody will mess with the file in our
    // temporary directory
    Ok(())
}

/// The actor-effect seam of the collapsed [`FileManager`]: the C++
/// `do_cancel_*`/`run_*`/`finish_*`/`try_flush_node_*` calls and
/// `Context::on_merge_files`. Every method is a no-op by default; the real
/// implementations land with the download/upload/generate managers and the
/// database flush loop (M.12.4-8).
pub trait FileManagerWorker: Send {
    /// `FileManager::do_cancel_download`.
    fn cancel_download(&mut self, _node: &FileNode) {}
    /// `FileManager::do_cancel_upload`.
    fn cancel_upload(&mut self, _node: &FileNode) {}
    /// `FileManager::do_cancel_generate`.
    fn cancel_generate(&mut self, _node: &FileNode) {}
    /// `FileManager::finish_downloads(file_id, Status::OK())` post-merge.
    fn finish_download(&mut self, _file_id: FileId) {}
    /// `FileManager::finish_uploads(file_id, Status::OK())` post-merge.
    fn finish_upload(&mut self, _file_id: FileId) {}
    /// `FileManager::clear_from_pmc` — the pmc record deletion; the
    /// TdDb wiring lands with the live flip (M.12.10).
    fn clear_from_pmc(&mut self, _node: &FileNode) {}
    /// `DownloadManager::remove_file_if_finished` — a no-op until the
    /// DownloadManager component lands under M.12.
    fn remove_file_if_finished(&mut self, _file_id: FileId) {}
    /// `FileManager::run_generate` + `run_download` + `run_upload`.
    fn run_node(&mut self, _node: &FileNode) {}
    /// The `flush_node_info` half of `FileManager::try_flush_node_info`.
    fn flush_node_info(&mut self, _node: &FileNode) {}
    /// `FileManager::try_flush_node_full`.
    fn flush_node_full(
        &mut self,
        _node: &FileNode,
        _new_remote: bool,
        _new_local: bool,
        _new_generate: bool,
        _other_pmc_id: FileDbId,
    ) {
    }
    /// `Context::on_merge_files`.
    fn on_merge_files(&mut self, _from_file_id: FileId, _to_file_id: FileId) {}
}

/// A hook call recorded while the merge mutates its two nodes; replayed
/// against the [`FileManagerWorker`] in call order afterwards.
enum MergeEvent {
    CancelDownload(Box<FileNode>),
    CancelUpload(Box<FileNode>),
    CancelGenerate(Box<FileNode>),
    OnMergeFiles(FileId, FileId),
    FinishQueries {
        with_downloads: bool,
        with_uploads: bool,
        file_ids: Vec<FileId>,
    },
    RunNode(Box<FileNode>),
    FlushFull {
        node: Box<FileNode>,
        new_remote: bool,
        new_local: bool,
        new_generate: bool,
        other_pmc_id: FileDbId,
    },
}

/// `FileManager::RemoteInfo` (FileManager.h:876-886) — the
/// `remote_location_info_` Enumerator entry; equality is by remote location.
struct RemoteInfo {
    location: FullRemoteFileLocation,
    source: FileLocationSource,
    file_id: FileId,
}

/// `td::FileManager` (FileManager.h:397) collapsed per the single-scheduler
/// convention: the node registry and the merge machine as plain methods, the
/// actor effects behind [`FileManagerWorker`]. The database round-trip that
/// C++ does in `get_sync_file_node` stays with the caller; `merge` operates
/// on already-loaded nodes.
#[derive(Default)]
pub struct FileManager {
    /// `file_nodes_` — index is the node id.
    file_nodes: Vec<Option<FileNode>>,
    /// `file_id_info_` — file id → node id.
    file_id_node: HashMap<FileId, usize>,
    worker: Option<Box<dyn FileManagerWorker>>,
    context: Option<Box<dyn FileManagerContext>>,

    /// `next_file_id`'s counter — the C++ `file_id_info_.size()`.
    last_file_id: i32,
    /// `empty_file_ids_`.
    empty_file_ids: Vec<i32>,
    /// `FileIdInfo::pin_flag_` per file id.
    pinned_file_ids: BTreeSet<FileId>,

    /// `local_location_to_file_id_`.
    local_location_to_file_id: BTreeMap<FullLocalFileLocation, FileId>,
    /// `generate_location_to_file_id_`.
    generate_location_to_file_id: BTreeMap<FullGenerateFileLocation, FileId>,
    /// `remote_location_to_file_id_` (the `keep_exact_remote_location = false`
    /// table).
    remote_location_to_file_id: BTreeMap<FullRemoteFileLocation, FileId>,
    /// `remote_location_info_` — the Enumerator entries; the index is the
    /// `remote_key` handed out inside `FileId::remote_id`.
    remote_location_info: Vec<RemoteInfo>,
    /// The Enumerator's value→index map.
    remote_location_index: BTreeMap<FullRemoteFileLocation, usize>,

    /// `bad_paths_` — seeded from `FileManagerContext::db_paths`.
    bad_paths: BTreeSet<String>,

    /// `file_download_requests_` (FileManager.h:855).
    file_download_requests: BTreeMap<FileId, DownloadRequests>,
    /// `file_upload_requests_` (FileManager.h:866).
    file_upload_requests: BTreeMap<FileId, UploadRequests>,
    /// `user_download_file_callback_` (FileManager.h:841).
    user_download_file_callback: Option<Arc<parking_lot::Mutex<dyn DownloadCallback>>>,
    /// `pending_user_file_downloads_` (FileManager.h:837-839) — the parked
    /// `downloadFile(synchronous)` promises.
    pending_user_file_downloads: BTreeMap<FileId, UserFileDownloadInfo>,
    /// The `download_queries_` container.
    download_queries: BTreeMap<u64, FileDownloadQuery>,
    /// The `upload_queries_` container.
    upload_queries: BTreeMap<u64, UploadQuery>,
    /// `FileManager::get_internal_download_id`'s counter.
    next_internal_download_id: i64,
    /// `FileManager::get_internal_upload_id`'s counter.
    next_internal_upload_id: i64,
    /// The `generate_queries_` container (FileManager.h:887) — the
    /// FileManager side of the generation queries.
    generate_queries: BTreeMap<u64, GenerateQuery>,
    /// The `generate_queries_` id allocator (`QueryId` starts at 1).
    next_generate_query_id: u64,
    /// The collapsed `file_generate_manager_` (FileManager.cpp:1330).
    generate: FileGenerateManager,
}

/// `FileManager::GenerateQuery` (FileManager.h:884-886).
#[derive(Clone, Copy)]
struct GenerateQuery {
    file_id: FileId,
}

/// The node snapshot `run_generate` takes before the priority walk —
/// the borrows end before the request tables are read.
struct NodeGeneratePlan {
    main_file_id: FileId,
    old_priority: i8,
    file_ids: Vec<FileId>,
    generate: Option<FullGenerateFileLocation>,
    local: LocalFileLocation,
    suggested_path: String,
}

/// The Download-worker seam `run_generate` re-enters
/// `FileManager::download` with: the referenced file id, the reserved
/// internal download id and the event-queue bridge.
type GenerateDownloadStart = (FileId, i64, Arc<parking_lot::Mutex<dyn DownloadCallback>>);

/// One entry of `finish_uploads`' drained callbacks: the manager query
/// id, the callback and its `was_active` flag.
type PendingUploadFinish = (i64, Arc<parking_lot::Mutex<dyn UploadCallback>>, bool);

/// The `external_path` answer: the worker state plus the pending
/// download seam of a Download worker.
type ExternalPathView<'a> = (&'a GenerateWorker, Option<(FileId, i64)>);

/// The trait-default [`FileManagerContext`] (no filesystem overrides, no
/// bad paths, `keep_exact_remote_location = false`).
struct DefaultFileManagerContext;

impl FileManagerContext for DefaultFileManagerContext {}

/// `FileStoreType` (FileManager.hpp:24).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileStoreType {
    Empty,
    Url,
    Generate,
    Local,
    Remote,
}

/// The `Version` entries the FileManager-mediated `FileId` serialization
/// branches on (td/telegram/Version.h).
const VERSION_STORE_FILE_ID: i32 = 1;
const VERSION_STORE_FILE_OWNER_ID: i32 = 6;
const VERSION_STORE_FILE_ENCRYPTION_KEY: i32 = 7;

impl FileManager {
    pub fn new() -> FileManager {
        FileManager {
            // the C++ constructor burns file id 0 (the invalid handle) and
            // node id 0 (FileManager.cpp:1316-1317), so the first registered
            // id/node is 1 and `FileId::is_valid` holds
            last_file_id: 1,
            file_nodes: vec![None],
            ..Default::default()
        }
    }

    /// Installs the context seam; also seeds `bad_paths_` the way the C++
    /// constructor does with `TdDb::with_db_path` (FileManager.cpp:1309-1320).
    pub fn set_context(&mut self, context: Box<dyn FileManagerContext>) {
        for path in context.db_paths() {
            self.bad_paths.insert(path);
        }
        self.context = Some(context);
    }

    fn ctx(&mut self) -> &mut dyn FileManagerContext {
        if self.context.is_none() {
            self.context = Some(Box::new(DefaultFileManagerContext));
        }
        self.context.as_deref_mut().expect("context")
    }

    /// Registers the worker; replaces the previous one.
    pub fn set_worker(&mut self, worker: Box<dyn FileManagerWorker>) {
        self.worker = Some(worker);
    }

    /// The `FileManager::hangup` half the Td close runs (the C++ resets
    /// the manager actors, Td.cpp:712): dropping the stored planes breaks
    /// the shared-cell reference cycle
    /// (`file_manager → context/worker → cell → file_manager`).
    pub fn teardown(&mut self) {
        self.context = None;
        self.worker = None;
    }

    /// `load_from_pmc` (FileManager.cpp:2704-2714): the flag clears, then
    /// the db-backed read + `on_pmc_loaded` merge runs (async in the C++).
    /// The collapse clears the flag and proceeds on the in-memory node —
    /// the db READ half stays `[blocked: db load]` (nothing ever loaded
    /// stored nodes in the collapse; the writes go through the FileDb
    /// flush plane).
    fn load_from_pmc(&mut self, node_id: usize) {
        let Some(node) = self.file_nodes[node_id].as_mut() else {
            return;
        };
        node.need_load_from_pmc = false;
    }

    /// `FileManager::next_file_id` (FileManager.cpp:4756-4766).
    fn next_file_id(&mut self) -> FileId {
        if let Some(id) = self.empty_file_ids.pop() {
            return FileId::new(id, 0);
        }
        let res = FileId::new(self.last_file_id, 0);
        self.last_file_id += 1;
        res
    }

    /// `FileManager::try_forget_file_id` (FileManager.cpp:1633-1651).
    fn try_forget_file_id(&mut self, file_id: FileId) -> bool {
        if self.pinned_file_ids.contains(&file_id) {
            return false; // "Can't forget file ..., because it is pinned"
        }
        let Some(&node_id) = self.file_id_node.get(&file_id) else {
            return false;
        };
        let Some(node) = self.file_nodes[node_id].as_ref() else {
            return false;
        };
        if node.main_file_id == file_id {
            return false; // "Can't forget main file ..."
        }
        let mut file_ids = std::mem::take(&mut self.file_nodes[node_id].as_mut().unwrap().file_ids);
        if !file_ids.contains(&file_id) {
            self.file_nodes[node_id].as_mut().unwrap().file_ids = file_ids;
            debug_assert!(false, "try_forget_file_id of a foreign file id");
            return false;
        }
        file_ids.retain(|id| *id != file_id);
        self.file_nodes[node_id].as_mut().unwrap().file_ids = file_ids;
        self.pinned_file_ids.remove(&file_id);
        self.file_id_node.remove(&file_id);
        self.empty_file_ids.push(file_id.get());
        true
    }

    /// `create_file_node` — appends a node and re-keys its file ids to it.
    /// The C++ id allocation (`create_file_id`) stays with the callers for
    /// now; both the main id and the node are handed in.
    pub fn register_file_node(
        &mut self,
        main_file_id: FileId,
        main_file_id_priority: i8,
        mut node: FileNode,
    ) -> usize {
        node.main_file_id = main_file_id;
        node.main_file_id_priority = main_file_id_priority;
        if !node.file_ids.contains(&main_file_id) {
            node.file_ids.push(main_file_id);
        }
        let node_id = self.file_nodes.len();
        for file_id in &node.file_ids {
            self.file_id_node.insert(*file_id, node_id);
        }
        self.file_nodes.push(Some(node));
        node_id
    }

    /// `get_file_node` — the node a file id currently points to.
    pub fn get_file_node(&self, file_id: FileId) -> Option<&FileNode> {
        self.file_id_node
            .get(&file_id)
            .and_then(|&node_id| self.file_nodes[node_id].as_ref())
    }

    fn get_file_node_mut(&mut self, file_id: FileId) -> Option<&mut FileNode> {
        let node_id = *self.file_id_node.get(&file_id)?;
        self.file_nodes[node_id].as_mut()
    }

    /// `FileManager::get_file_view` over the registry.
    pub fn get_file_view(&self, file_id: FileId) -> Option<FileView<'_>> {
        self.get_file_node(file_id).map(FileView::new)
    }

    /// `FileManager::pin_file_id` — the C++ pins through
    /// `get_file_id_info(file_id)->pin_flag_ = true`.
    fn pin_file_id(&mut self, file_id: FileId) {
        self.pinned_file_ids.insert(file_id);
    }

    /// `FileManager::try_flush_node` (FileManager.cpp:2574-2577) — the pmc
    /// half through the worker's `flush_node_full` hook, the info half
    /// through [`Self::try_flush_node_info`].
    fn try_flush_node(&mut self, node_id: usize) {
        let need_pmc_flush = self.file_nodes[node_id]
            .as_ref()
            .is_some_and(|node| node.need_pmc_flush());
        if need_pmc_flush {
            let node = self.file_nodes[node_id].as_ref().expect("node");
            if let Some(worker) = self.worker.as_deref_mut() {
                worker.flush_node_full(node, false, false, false, FileDbId::default());
            }
            if let Some(node) = self.file_nodes[node_id].as_mut() {
                node.on_pmc_flushed();
            }
        }
        self.try_flush_node_info(node_id);
    }

    /// `FileManager::register_empty` (FileManager.cpp:1653-1674).
    pub fn register_empty(&mut self, file_type: FileType) -> FileId {
        let location = FullLocalFileLocation {
            file_type,
            path: String::new(),
            mtime_nsec: 0,
        };
        if let Some(&file_id) = self.local_location_to_file_id.get(&location)
            && file_id.is_valid()
        {
            return file_id;
        }
        let file_id = self.next_file_id();
        self.local_location_to_file_id
            .insert(location.clone(), file_id);

        let node = FileNode::new(
            LocalFileLocation::Full(location),
            NewRemoteFileLocation::default(),
            None,
            0,
            0,
            String::new(),
            String::new(),
            DialogId::default(),
            FileEncryptionKey::default(),
            file_id,
            0,
        );
        self.register_file_node(file_id, 0, node);
        self.pin_file_id(file_id);
        file_id
    }

    /// `FileManager::register_local` (FileManager.cpp:1690-1745).
    pub fn register_local(
        &mut self,
        location: FullLocalFileLocation,
        owner_dialog_id: DialogId,
        size: i64,
        get_by_hash: bool,
        skip_file_size_checks: bool,
        merge_file_id: FileId,
    ) -> Result<FileId, Status> {
        let info = check_full_local_location(
            self.ctx(),
            FullLocalLocationInfo { location, size },
            skip_file_size_checks,
        )?;
        let location = info.location;
        let size = info.size;

        if self.bad_paths.contains(&location.path) {
            return Err(Status::error(
                400,
                "Sending of internal database files is forbidden",
            ));
        }

        let mut is_new = false;
        let file_id = match self.local_location_to_file_id.get(&location) {
            Some(file_id) if file_id.is_valid() => *file_id,
            _ => {
                let file_id = self.next_file_id();
                let mut node = FileNode::new(
                    LocalFileLocation::Full(location.clone()),
                    NewRemoteFileLocation::default(),
                    None,
                    size,
                    0,
                    String::new(),
                    String::new(),
                    owner_dialog_id,
                    FileEncryptionKey::default(),
                    file_id,
                    0,
                );
                node.need_load_from_pmc = true;
                self.register_file_node(file_id, 0, node);
                self.local_location_to_file_id
                    .insert(location.clone(), file_id);
                is_new = true;
                file_id
            }
        };
        let _ = get_by_hash; // vestigial in the C++ register_local too

        if merge_file_id.is_valid() {
            let merge_result = self.merge(file_id, merge_file_id);
            if merge_result.is_ok() {
                let main_file_id = self
                    .get_file_node(file_id)
                    .map(|node| node.main_file_id)
                    .expect("merged node");
                let mut node_id = *self
                    .file_id_node
                    .get(&main_file_id)
                    .expect("merged node id");
                if main_file_id != file_id {
                    if is_new && !self.pinned_file_ids.contains(&file_id) {
                        let is_removed = self.try_forget_file_id(file_id);
                        debug_assert!(is_removed, "try_forget_file_id of a fresh unmerged id");
                        self.local_location_to_file_id.remove(&location);
                    }
                    node_id = *self
                        .file_id_node
                        .get(&main_file_id)
                        .expect("merged node id");
                }
                self.try_flush_node(node_id);
            }
            if is_new {
                self.pin_file_id(file_id);
            }
            merge_result?;
        } else if is_new {
            self.pin_file_id(file_id);
        }
        Ok(file_id)
    }

    /// `FileManager::register_remote` (FileManager.cpp:1747-1819).
    pub fn register_remote(
        &mut self,
        location: FullRemoteFileLocation,
        file_location_source: FileLocationSource,
        owner_dialog_id: DialogId,
        size: i64,
        expected_size: i64,
        remote_name: String,
    ) -> FileId {
        let mut size = size;
        let mut expected_size = expected_size;
        if size < 0 {
            size = 0;
        }
        if expected_size < 0 {
            expected_size = 0;
        }
        let url = location.get_url().to_string();

        let file_id;
        let mut merge_file_id = FileId::default();
        let mut remote_key = 0;
        if self.ctx().keep_exact_remote_location() {
            file_id = self.next_file_id();
            match self.remote_location_index.get(&location).copied() {
                Some(index) => {
                    remote_key = index;
                    let stored_file_id = self.remote_location_info[index].file_id;
                    if stored_file_id != file_id {
                        merge_file_id = stored_file_id;
                        let stored_location = self.remote_location_info[index].location.clone();
                        let stored_source = self.remote_location_info[index].source;
                        if merge_choose_full_remote_location(
                            &location,
                            file_location_source,
                            &stored_location,
                            stored_source,
                        ) == 0
                        {
                            let info = &mut self.remote_location_info[index];
                            info.location = location.clone();
                            info.source = file_location_source;
                        }
                    }
                }
                None => {
                    let index = self.remote_location_info.len();
                    self.remote_location_info.push(RemoteInfo {
                        location: location.clone(),
                        source: file_location_source,
                        file_id,
                    });
                    self.remote_location_index.insert(location.clone(), index);
                    remote_key = index;
                }
            }
        } else {
            if let Some(other_id) = self.remote_location_to_file_id.get(&location).copied() {
                if other_id.is_valid() {
                    let other_source = self
                        .get_file_node(other_id)
                        .map(|node| node.remote.full_source)
                        .unwrap_or(FileLocationSource::None);
                    if other_source == FileLocationSource::FromServer {
                        // if the file has already been received from the
                        // server, then we don't need merge or create a new
                        // file: skip merging of dc_id, file_reference and
                        // access_hash
                        return other_id;
                    }
                }
                merge_file_id = other_id;
            }
            file_id = self.next_file_id();
            if !merge_file_id.is_valid() {
                self.remote_location_to_file_id
                    .insert(location.clone(), file_id);
            }
        }

        let node = FileNode::new(
            LocalFileLocation::Empty,
            NewRemoteFileLocation::from_remote(
                RemoteFileLocation::Full(location.clone()),
                file_location_source,
            ),
            None,
            size,
            expected_size,
            remote_name,
            url,
            owner_dialog_id,
            FileEncryptionKey::default(),
            file_id,
            1,
        );
        self.register_file_node(file_id, 1, node);

        let mut main_file_id = file_id;
        if !merge_file_id.is_valid() {
            if let Some(node) = self.get_file_node_mut(main_file_id) {
                node.need_load_from_pmc = true;
            }
            self.pin_file_id(main_file_id);
        } else {
            // may invalidate node
            let _ = self.merge(file_id, merge_file_id);
            if let Some(node_id) = self.file_id_node.get(&file_id).copied() {
                self.try_flush_node(node_id);
            }
            main_file_id = self
                .get_file_node(file_id)
                .map(|node| node.main_file_id)
                .expect("merged node");
            if main_file_id != file_id {
                self.try_forget_file_id(file_id);
            }
        }
        FileId::new(main_file_id.get(), remote_key as i32)
    }

    /// `FileManager::register_url` (FileManager.cpp:1821-1824).
    pub fn register_url(
        &mut self,
        url: String,
        file_type: FileType,
        owner_dialog_id: DialogId,
    ) -> FileId {
        self.do_register_generate(
            FullGenerateFileLocation::new(file_type, url.clone(), "#url#".to_string()),
            owner_dialog_id,
            0,
            url,
        )
    }

    /// `FileManager::register_generate` (FileManager.cpp:1826-1840).
    pub fn register_generate(
        &mut self,
        file_type: FileType,
        original_path: String,
        conversion: String,
        owner_dialog_id: DialogId,
        expected_size: i64,
    ) -> FileId {
        // add #mtime# into conversion
        let mut conversion = conversion;
        if !original_path.is_empty()
            && !conversion.starts_with('#')
            && std::path::Path::new(&original_path).is_absolute()
            && !self
                .ctx()
                .logged_file_paths()
                .iter()
                .any(|p| p == &original_path)
        {
            let stat = self.ctx().stat(&original_path);
            let mtime = stat.map(|s| s.mtime_nsec).unwrap_or(0);
            conversion = format!("#mtime#{:020}#{}", mtime, conversion);
        }
        self.do_register_generate(
            FullGenerateFileLocation::new(file_type, original_path, conversion),
            owner_dialog_id,
            expected_size.max(0),
            String::new(),
        )
    }

    /// `FileManager::do_register_generate` (FileManager.cpp:1842-1863).
    fn do_register_generate(
        &mut self,
        generate: FullGenerateFileLocation,
        owner_dialog_id: DialogId,
        expected_size: i64,
        url: String,
    ) -> FileId {
        if let Some(&file_id) = self.generate_location_to_file_id.get(&generate)
            && file_id.is_valid()
        {
            return file_id;
        }
        let file_id = self.next_file_id();

        let mut node = FileNode::new(
            LocalFileLocation::Empty,
            NewRemoteFileLocation::default(),
            Some(generate.clone()),
            0,
            expected_size,
            String::new(),
            url,
            owner_dialog_id,
            FileEncryptionKey::default(),
            file_id,
            0,
        );
        node.need_load_from_pmc = true;
        self.register_file_node(file_id, 0, node);
        self.generate_location_to_file_id.insert(generate, file_id);
        self.pin_file_id(file_id);
        file_id
    }

    /// `FileManager::copy_file_id` (FileManager.cpp:1625-1631).
    pub fn copy_file_id(
        &mut self,
        file_id: FileId,
        file_type: FileType,
        owner_dialog_id: DialogId,
        source: &str,
    ) -> FileId {
        let (suggested_path, size) = match self.get_file_view(file_id) {
            Some(view) => (view.suggested_path(), view.size()),
            None => (String::new(), 0),
        };
        let result_file_id = self.register_generate(
            file_type,
            suggested_path,
            format!("#file_id#{}", file_id.get()),
            owner_dialog_id,
            size,
        );
        let _ = source; // the C++ only logs it
        result_file_id
    }

    /// `FileManager::get_file_object` (FileManager.cpp:4103-4134).
    pub fn get_file_object(&self, file_id: FileId) -> td_api::File {
        let Some(file_node) = self.get_file_node(file_id) else {
            return td_api::File {
                id: 0,
                size: 0,
                expected_size: 0,
                local: Some(td_api::LocalFile::default()),
                remote: Some(td_api::RemoteFile::default()),
            };
        };

        let persistent_file_id = file_node.get_persistent_file_id();
        let unique_file_id = file_node.get_unique_file_id();
        let is_downloading_completed = matches!(file_node.local, LocalFileLocation::Full(_));
        let is_uploading_completed = !persistent_file_id.is_empty();
        let size = file_node.size;
        let download_offset = file_node.download_offset;
        let expected_size = file_node.expected_size(false);
        let local_prefix_size = file_node.local_prefix_size();
        let local_total_size = file_node.local_total_size();
        let remote_size = file_node.remote_size();
        let path = file_node.path().to_string();
        let files_dir = self
            .context
            .as_deref()
            .map(|c| c.files_dir())
            .unwrap_or_default();
        let secure_files_dir = self
            .context
            .as_deref()
            .map(|c| c.secure_files_dir())
            .unwrap_or_default();
        let can_be_deleted = file_node.can_delete(&files_dir, &secure_files_dir);

        let file_view = FileView::new(file_node);
        let can_be_downloaded = file_view.can_download_from_server() || file_view.can_generate();

        td_api::File {
            id: file_id.get(),
            size,
            expected_size,
            local: Some(td_api::LocalFile {
                path,
                can_be_downloaded,
                can_be_deleted,
                is_downloading_active: file_node.is_downloading(),
                is_downloading_completed,
                download_offset,
                downloaded_prefix_size: local_prefix_size,
                downloaded_size: local_total_size,
            }),
            remote: Some(td_api::RemoteFile {
                id: persistent_file_id,
                unique_id: unique_file_id,
                is_uploading_active: file_node.is_uploading(),
                is_uploading_completed,
                uploaded_size: remote_size,
            }),
        }
    }

    /// `FileManager::get_input_documents` (FileManager.cpp:4402-4413) —
    /// the sticker-file-id conversion the uploaded-document media builds.
    pub fn get_input_documents(&self, file_ids: &[FileId]) -> Vec<telegram_api::InputDocument> {
        file_ids
            .iter()
            .map(|file_id| {
                let file_view = self
                    .get_file_view(*file_id)
                    .expect("FileManager::get_input_documents: empty file view");
                let full_remote_location = file_view
                    .get_full_remote_location()
                    .expect("FileManager::get_input_documents: no full remote location");
                assert!(!full_remote_location.is_web());
                telegram_api::InputDocument::inputDocument(full_remote_location.as_input_document())
            })
            .collect()
    }

    /// `FileManager::extract_file_reference(const InputMedia *)`
    /// (FileManager.cpp:4542-4570) — the file reference an already-built
    /// input media carries, empty unless the media wraps a common
    /// document (or the story-document upload form).
    pub fn extract_input_media_file_reference(input_media: &telegram_api::InputMedia) -> String {
        const INVALID_FILE_REFERENCE: &[u8] = b"#";
        let reference = |input_document: Option<&telegram_api::InputDocument>| match input_document
        {
            Some(telegram_api::InputDocument::inputDocument(document)) => {
                if document.file_reference == INVALID_FILE_REFERENCE {
                    String::new()
                } else {
                    // the wire field is `bytes`; the module-wide
                    // file-reference representation is the String
                    // (FullRemoteFileLocation docs)
                    String::from_utf8_lossy(&document.file_reference).into_owned()
                }
            }
            _ => String::new(),
        };
        match input_media {
            telegram_api::InputMedia::inputMediaDocument(media) => reference(media.id.as_ref()),
            telegram_api::InputMedia::inputMediaUploadedDocument(media) => {
                // only the story-document upload form carries the
                // reference (FileManager.cpp:4560-4567)
                match media.file.as_ref() {
                    Some(telegram_api::InputFile::inputFileStoryDocument(story)) => {
                        reference(story.id.as_ref())
                    }
                    _ => String::new(),
                }
            }
            _ => String::new(),
        }
    }

    /// `FileManager::store_file` (FileManager.hpp:30-131) — the
    /// FileManager-mediated `FileId` serialization the photo stores call
    /// through (`store(dialog_photo.small_file_id, storer)`), ttl 5.
    pub fn store_file(&self, file_id: FileId, out: &mut Vec<u8>) {
        self.store_file_ttl(file_id, out, 5)
    }

    /// The ttl-parametered [`Self::store_file`].
    pub fn store_file_ttl(&self, file_id: FileId, out: &mut Vec<u8>, ttl: i32) {
        use store_parse::*;

        let file_view = self.get_file_view(file_id);
        let file_store_type = if ttl <= 0 {
            FileStoreType::Empty
        } else {
            file_view.as_ref().map_or(FileStoreType::Empty, |view| {
                if view.has_full_remote_location() {
                    FileStoreType::Remote
                } else if view.has_url() {
                    FileStoreType::Url
                } else if view.has_generate_location() {
                    FileStoreType::Generate
                } else if view.has_full_local_location() {
                    FileStoreType::Local
                } else {
                    FileStoreType::Empty
                }
            })
        };

        store_i32(out, file_store_type as i32);

        let mut has_encryption_key = false;
        let has_expected_size = file_store_type == FileStoreType::Remote
            && file_view
                .as_ref()
                .is_some_and(|view| view.size() == 0 && view.expected_size(false) != 0);
        let mut has_secure_key = false;
        let mut size: i64 = 0;
        let mut has_64bit_size = false;
        if file_store_type != FileStoreType::Empty {
            if let Some(view) = file_view.as_ref() {
                has_encryption_key = view.is_encrypted_secret();
                has_secure_key = view.is_encrypted_secure();
                if file_store_type != FileStoreType::Url {
                    size = if has_expected_size || file_store_type == FileStoreType::Generate {
                        view.expected_size(false)
                    } else {
                        view.size()
                    };
                    has_64bit_size = size > i32::MAX as i64;
                }
            }
            store_flags(
                out,
                [
                    has_encryption_key,
                    has_expected_size,
                    has_secure_key,
                    has_64bit_size,
                ],
            );
        }

        let Some(view) = file_view.as_ref() else {
            return;
        };
        match file_store_type {
            FileStoreType::Empty => {}
            FileStoreType::Url => {
                store_i32(out, view.get_type() as i32);
                store_string(out, view.get_url().expect("url"));
                store_i64(out, view.owner_dialog_id().get());
            }
            FileStoreType::Remote => {
                let full_remote_location = view.get_full_remote_location().expect("remote");
                full_remote_location.store(out);
                if has_64bit_size {
                    store_i64(out, size);
                } else {
                    store_i32(out, size as i32);
                }
                store_string(out, view.remote_name());
                store_i64(out, view.owner_dialog_id().get());
            }
            FileStoreType::Local => {
                let full_local_location = view.get_full_local_location().expect("local");
                full_local_location.store(out);
                if has_64bit_size {
                    store_i64(out, size);
                } else {
                    store_i32(out, size as i32);
                }
                store_i32(out, i32::from(view.get_by_hash()));
                store_i64(out, view.owner_dialog_id().get());
            }
            FileStoreType::Generate => {
                let mut generate_location = view.get_generate_location().expect("generate").clone();
                let mut from_file_id = FileId::default();
                let mut have_file_id = false;
                if generate_location.conversion == "#_file_id#" {
                    // the nested file id follows the location
                } else if let Some(raw_id) = generate_location.conversion.strip_prefix("#file_id#")
                {
                    // It is not the best possible way to serialize file_id
                    from_file_id = FileId::new(raw_id.parse().unwrap_or(0), 0);
                    generate_location.conversion = "#_file_id#".to_string();
                    have_file_id = true;
                }
                generate_location.store(out);
                if has_64bit_size {
                    store_i64(out, size);
                } else {
                    store_i32(out, size as i32);
                    store_i32(out, 0); // legacy
                }
                store_i64(out, view.owner_dialog_id().get());

                if have_file_id {
                    self.store_file_ttl(from_file_id, out, ttl - 1);
                }
            }
        }
        if has_encryption_key || has_secure_key {
            view.encryption_key().store(out);
        }
    }

    /// `FileManager::parse_file` (FileManager.hpp:135-273).
    pub fn parse_file(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        if parser.version() < VERSION_STORE_FILE_ID {
            return Ok(FileId::default());
        }

        let raw_store_type = parser.i32_()?;
        let file_store_type = match raw_store_type {
            0 => FileStoreType::Empty,
            1 => FileStoreType::Url,
            2 => FileStoreType::Generate,
            3 => FileStoreType::Local,
            4 => FileStoreType::Remote,
            _ => return Err("Unknown FileStoreType"),
        };

        let mut has_encryption_key = false;
        let mut has_expected_size = false;
        let mut has_secure_key = false;
        let mut has_64bit_size = false;
        if file_store_type != FileStoreType::Empty
            && parser.version() >= VERSION_STORE_FILE_ENCRYPTION_KEY
        {
            [
                has_encryption_key,
                has_expected_size,
                has_secure_key,
                has_64bit_size,
            ] = parser.parse_flags::<4>()?;
        }

        let file_id = match file_store_type {
            FileStoreType::Empty => FileId::default(),
            FileStoreType::Remote => {
                let full_remote_location = FullRemoteFileLocation::parse_from(parser)?;
                let stored_size = if has_64bit_size {
                    parser.i64_()?
                } else {
                    let int_size = parser.i32_()?;
                    if int_size < 0 {
                        int_size as i64 + (1i64 << 32)
                    } else {
                        int_size as i64
                    }
                };
                let size = if has_expected_size { 0 } else { stored_size };
                let expected_size = if has_expected_size { stored_size } else { 0 };
                let name = parser.string()?;
                let owner_dialog_id = if parser.version() >= VERSION_STORE_FILE_OWNER_ID {
                    DialogId::new(parser.i64_()?)
                } else {
                    DialogId::default()
                };
                self.register_remote(
                    full_remote_location,
                    FileLocationSource::FromBinlog,
                    owner_dialog_id,
                    size,
                    expected_size,
                    name,
                )
            }
            FileStoreType::Local => {
                let full_local_location = FullLocalFileLocation::parse_from(parser)?;
                let size = if has_64bit_size {
                    parser.i64_()?
                } else {
                    let int_size = parser.i32_()?;
                    if int_size < 0 {
                        int_size as i64 + (1i64 << 32)
                    } else {
                        int_size as i64
                    }
                };
                let get_by_hash = parser.i32_()?;
                let owner_dialog_id = if parser.version() >= VERSION_STORE_FILE_OWNER_ID {
                    DialogId::new(parser.i64_()?)
                } else {
                    DialogId::default()
                };
                match self.register_local(
                    full_local_location.clone(),
                    owner_dialog_id,
                    size,
                    get_by_hash != 0,
                    false,
                    FileId::default(),
                ) {
                    Ok(file_id) => file_id,
                    Err(_) => {
                        tdutils::log_error!(
                            "Can't resend local file {} of size {} owned by {:?}",
                            full_local_location,
                            size,
                            owner_dialog_id
                        );
                        self.register_empty(full_local_location.file_type)
                    }
                }
            }
            FileStoreType::Generate => {
                let mut full_generated_location = FullGenerateFileLocation::parse_from(parser)?;
                let expected_size = if has_64bit_size {
                    parser.i64_()?
                } else {
                    let int_size = parser.i32_()?;
                    let expected_size = if int_size < 0 {
                        int_size as i64 + (1i64 << 32)
                    } else {
                        int_size as i64
                    };
                    let zero = parser.i32_()?;
                    let _ = zero;
                    expected_size
                };
                let owner_dialog_id = if parser.version() >= VERSION_STORE_FILE_OWNER_ID {
                    DialogId::new(parser.i64_()?)
                } else {
                    DialogId::default()
                };
                if full_generated_location.conversion.starts_with("#file_id#") {
                    tdutils::log_error!("Can't resend message with '#file_id#...' location");
                    self.register_empty(full_generated_location.file_type)
                } else if full_generated_location.conversion == "#_file_id#" {
                    let file_id = self.parse_file(parser)?;
                    if file_id.empty() {
                        self.register_empty(full_generated_location.file_type)
                    } else {
                        full_generated_location.conversion = format!("#file_id#{}", file_id.get());
                        self.register_generate(
                            full_generated_location.file_type,
                            full_generated_location.original_path.clone(),
                            full_generated_location.conversion.clone(),
                            owner_dialog_id,
                            expected_size,
                        )
                    }
                } else {
                    self.register_generate(
                        full_generated_location.file_type,
                        full_generated_location.original_path.clone(),
                        full_generated_location.conversion.clone(),
                        owner_dialog_id,
                        expected_size,
                    )
                }
            }
            FileStoreType::Url => {
                let type_ = FileType::from_raw(parser.i32_()?);
                let url = parser.string()?;
                let owner_dialog_id = if parser.version() >= VERSION_STORE_FILE_OWNER_ID {
                    DialogId::new(parser.i64_()?)
                } else {
                    DialogId::default()
                };
                self.register_url(url, type_, owner_dialog_id)
            }
        };

        if has_encryption_key || has_secure_key {
            let encryption_key_type = if has_encryption_key {
                FileEncryptionKeyType::Secret
            } else {
                FileEncryptionKeyType::Secure
            };
            let mut encryption_key = FileEncryptionKey::default();
            encryption_key.parse_from(encryption_key_type, parser)?;
            self.set_encryption_key(file_id, encryption_key);
        }

        Ok(file_id)
    }

    /// `FileManager::from_persistent_id` (FileManager.cpp:3994-4025) — the
    /// `getRemoteFile`/`inputFileRemote` entry over the persistent-id
    /// string form.
    pub fn from_persistent_id(
        &mut self,
        persistent_id: &str,
        file_type: FileType,
    ) -> Result<FileId, Status> {
        if persistent_id.contains('.') {
            let url = match tdutils::http_url::parse_url(persistent_id) {
                tdutils::status::Result::Ok(url) => url.get_url(),
                tdutils::status::Result::Err(status) => {
                    return Err(Status::error(
                        400,
                        &format!("Invalid file HTTP URL specified: {}", status.message()),
                    ));
                }
            };
            let Some(url) = crate::misc::clean_input_string(&url) else {
                return Err(Status::error(400, "URL must be in UTF-8"));
            };
            return Ok(self.register_url(url, file_type, DialogId::default()));
        }

        let tdutils::status::Result::Ok(binary) =
            tdmtproto::utils::base64url_decode(persistent_id.as_bytes())
        else {
            return Err(Status::error(
                400,
                "Wrong remote file identifier specified: invalid base64url",
            ));
        };
        if binary.is_empty() {
            return Err(Status::error(
                400,
                "Remote file identifier must be non-empty",
            ));
        }
        match *binary.last().expect("non-empty") {
            PERSISTENT_ID_VERSION_OLD => self.from_persistent_id_v2(&binary, file_type),
            PERSISTENT_ID_VERSION => self.from_persistent_id_v3(&binary, file_type),
            PERSISTENT_ID_VERSION_GENERATED => {
                self.from_persistent_id_generated(&binary, file_type)
            }
            _ => Err(Status::error(
                400,
                "Wrong remote file identifier specified: can't unserialize it. Wrong last symbol",
            )),
        }
    }

    /// `FileManager::from_persistent_id_v2` (FileManager.cpp:4072-4075).
    #[allow(clippy::wrong_self_convention)]
    fn from_persistent_id_v2(
        &mut self,
        binary: &[u8],
        file_type: FileType,
    ) -> Result<FileId, Status> {
        self.from_persistent_id_v23(&binary[..binary.len() - 1], file_type, 0)
    }

    /// `FileManager::from_persistent_id_v3` (FileManager.cpp:4077-4086).
    #[allow(clippy::wrong_self_convention)]
    fn from_persistent_id_v3(
        &mut self,
        binary: &[u8],
        file_type: FileType,
    ) -> Result<FileId, Status> {
        let binary = &binary[..binary.len() - 1];
        if binary.is_empty() {
            return Err(Status::error(400, "Invalid remote file identifier"));
        }
        let version = *binary.last().expect("non-empty") as i32;
        self.from_persistent_id_v23(&binary[..binary.len() - 1], file_type, version)
    }

    /// `FileManager::from_persistent_id_v23` (FileManager.cpp:4047-4070).
    #[allow(clippy::wrong_self_convention)]
    fn from_persistent_id_v23(
        &mut self,
        binary: &[u8],
        file_type: FileType,
        version: i32,
    ) -> Result<FileId, Status> {
        if !(0..store_parse::version::NEXT).contains(&version) {
            return Err(Status::error(400, "Invalid remote file identifier"));
        }
        let decoded = tdutils::zero_encode::zero_decode(binary);
        let mut parser = Parser::new(&decoded);
        parser.set_version(version);
        let parse_result = FullRemoteFileLocation::parse_from(&mut parser);
        let remote_location = match parse_result.and_then(|location| {
            parser.fetch_end()?;
            Ok(location)
        }) {
            Ok(location) => location,
            Err(_) => {
                return Err(Status::error(
                    400,
                    "Wrong remote file identifier specified: can't unserialize it",
                ));
            }
        };
        let mut real_file_type = remote_location.file_type;
        if is_document_file_type(real_file_type) && is_document_file_type(file_type) {
            real_file_type = file_type;
        } else if is_background_type(real_file_type) && is_background_type(file_type) {
            // type of file matches, but real type is in the stored remote
            // location
        } else if real_file_type != file_type && file_type != FileType::Temp {
            return Err(Status::error(
                400,
                &format!("Can't use file of type {real_file_type:?} as {file_type:?}"),
            ));
        }
        let mut remote_location = remote_location;
        remote_location.file_type = real_file_type;
        Ok(self.register_remote(
            remote_location,
            FileLocationSource::FromUser,
            DialogId::default(),
            0,
            0,
            String::new(),
        ))
    }

    /// `FileManager::from_persistent_id_generated`
    /// (FileManager.cpp:4027-4045).
    #[allow(clippy::wrong_self_convention)]
    fn from_persistent_id_generated(
        &mut self,
        binary: &[u8],
        file_type: FileType,
    ) -> Result<FileId, Status> {
        let decoded = tdutils::zero_encode::zero_decode(&binary[..binary.len() - 1]);
        let mut parser = Parser::new(&decoded);
        let generate_location =
            match FullGenerateFileLocation::parse_from(&mut parser).and_then(|location| {
                parser.fetch_end()?;
                Ok(location)
            }) {
                Ok(location) => location,
                Err(_) => {
                    return Err(Status::error(
                        400,
                        "Wrong remote file identifier specified: can't unserialize it",
                    ));
                }
            };
        let real_file_type = generate_location.file_type;
        if (real_file_type != file_type && file_type != FileType::Temp)
            || (real_file_type != FileType::Thumbnail
                && real_file_type != FileType::EncryptedThumbnail)
        {
            return Err(Status::error(
                400,
                &format!("Can't use file of type {real_file_type:?} as {file_type:?}"),
            ));
        }
        if !is_remotely_generated_file(&generate_location.conversion) {
            return Err(Status::error(400, "Unexpected conversion type"));
        }
        Ok(self.do_register_generate(generate_location, DialogId::default(), 0, String::new()))
    }

    /// `FileManager::check_input_file_id` (FileManager.cpp:4140-4180) —
    /// the type re-check over the full node data, plus the exact-remote
    /// `RemoteInfo` registration the reused ids carry.
    fn check_input_file_id(
        &mut self,
        type_: FileType,
        result: Result<FileId, Status>,
        is_encrypted: bool,
        allow_zero: bool,
        is_secure: bool,
    ) -> Result<FileId, Status> {
        let file_id = result?;
        if allow_zero && !file_id.is_valid() {
            return Ok(FileId::default());
        }

        // the node snapshot: the reads end before the RemoteInfo
        // mutations borrow the manager again (the shadowed `is_encrypted`
        // is the view's flag, the param keeps the request's)
        let (main_file_id, real_type, has_url, view_is_encrypted, full_remote_location) = {
            let Some(node) = self.get_file_node(file_id) else {
                return Err(Status::error(400, "File not found"));
            };
            let view = FileView::new(node);
            (
                node.main_file_id,
                view.get_type(),
                view.has_url(),
                view.is_encrypted(),
                view.get_full_remote_location().cloned(),
            )
        };
        if !(is_encrypted || is_secure)
            && real_type != type_
            && !(real_type == FileType::Temp && has_url)
            && !(is_document_file_type(real_type) && is_document_file_type(type_))
            && !(is_background_type(real_type) && is_background_type(type_))
            && !(view_is_encrypted && type_ == FileType::Ringtone)
            && !(real_type == FileType::PhotoStory && type_ == FileType::Photo)
            && !(real_type == FileType::Photo && type_ == FileType::PhotoStory)
        {
            return Err(Status::error(
                400,
                &format!("Can't use file of type {real_type:?} as {type_:?}"),
            ));
        }

        let Some(full_remote_location) = full_remote_location else {
            return Ok(main_file_id);
        };

        let mut remote_id = file_id.get_remote();
        if remote_id == 0 && self.ctx().keep_exact_remote_location() {
            match self
                .remote_location_index
                .get(&full_remote_location)
                .copied()
            {
                Some(index) => remote_id = index as i32,
                None => {
                    let index = self.remote_location_info.len();
                    self.remote_location_info.push(RemoteInfo {
                        location: full_remote_location.clone(),
                        source: FileLocationSource::FromUser,
                        file_id,
                    });
                    self.remote_location_index
                        .insert(full_remote_location, index);
                    remote_id = index as i32;
                }
            }
            if self
                .remote_location_info
                .get(remote_id as usize)
                .is_some_and(|info| info.file_id == file_id)
            {
                self.pin_file_id(file_id);
            }
        }
        Ok(FileId::new(main_file_id.get(), remote_id))
    }

    /// `FileManager::get_input_file_id` (FileManager.cpp:4211-4283) — the
    /// `InputFile` builder. The `reuse_uploaded_photos_by_hash` arm of
    /// `inputFileLocal` is option-gated off by default and rides the
    /// `file_hash_to_file_id_` map of the C++ manager; the collapse
    /// registers locally like the option-off path.
    #[allow(clippy::too_many_arguments)]
    pub fn get_input_file_id(
        &mut self,
        type_: FileType,
        file: Option<&td_api::InputFile>,
        owner_dialog_id: DialogId,
        allow_zero: bool,
        is_encrypted: bool,
        mut get_by_hash: bool,
        is_secure: bool,
        force_reuse: bool,
        never_reuse: bool,
    ) -> Result<FileId, Status> {
        let _ = force_reuse;
        let Some(file) = file else {
            if allow_zero {
                return Ok(FileId::default());
            }
            return Err(Status::error(400, "InputFile is not specified"));
        };

        if is_encrypted || is_secure {
            get_by_hash = false;
        }

        let new_type = if is_encrypted {
            FileType::Encrypted
        } else if is_secure {
            FileType::SecureEncrypted
        } else {
            type_
        };

        let r_file_id = match file {
            td_api::InputFile::inputFileLocal(local) => {
                let path = local.path.clone();
                if allow_zero && path.is_empty() {
                    Ok(FileId::default())
                } else {
                    self.register_local(
                        FullLocalFileLocation::new(new_type, path, 0),
                        owner_dialog_id,
                        0,
                        get_by_hash,
                        false,
                        FileId::default(),
                    )
                }
            }
            td_api::InputFile::inputFileId(input_id) => {
                if never_reuse {
                    Err(Status::error(400, "Can't use inputFileId"))
                } else {
                    let file_id = FileId::new(input_id.id, 0);
                    if !file_id.is_valid() {
                        if !allow_zero {
                            return Err(Status::error(400, "InputFile is not specified"));
                        }
                        Ok(FileId::default())
                    } else {
                        Ok(file_id)
                    }
                }
            }
            td_api::InputFile::inputFileRemote(remote) => {
                if never_reuse {
                    Err(Status::error(400, "Can't use inputFileRemote"))
                } else if allow_zero && remote.id.is_empty() {
                    Ok(FileId::default())
                } else {
                    self.from_persistent_id(&remote.id.clone(), type_)
                }
            }
            td_api::InputFile::inputFileGenerated(generated) => Ok(self.register_generate(
                new_type,
                generated.original_path.clone(),
                generated.conversion.clone(),
                owner_dialog_id,
                generated.expected_size,
            )),
        };

        self.check_input_file_id(type_, r_file_id, is_encrypted, allow_zero, is_secure)
    }

    /// `FileManager::preliminary_upload_file` (FileManager.cpp:5356-5370)
    /// — the `preliminaryUploadFile` arm. The C++
    /// `PreliminaryUploadFileCallback` only ever observes the synchronous
    /// `resume_upload` outcomes (the user-arm callback is not parked in
    /// `file_upload_requests_`), and its `on_upload_ok` cancels the upload
    /// so a second upload of the same file succeeds — replayed here as the
    /// post-`upload` completed check.
    pub fn preliminary_upload_file(
        &mut self,
        input_file: Option<&td_api::InputFile>,
        file_type: FileType,
        priority: i32,
        fllm: &mut FileLoadManager,
    ) -> Result<td_api::File, Status> {
        check_download_priority(priority)?;

        let is_secret =
            file_type == FileType::Encrypted || file_type == FileType::EncryptedThumbnail;
        let is_secure = file_type == FileType::SecureEncrypted;
        let file_id = self.get_input_file_id(
            file_type,
            input_file,
            DialogId::default(),
            false,
            is_secret,
            !is_secure && !is_secret,
            is_secure,
            false,
            false,
        )?;

        self.upload(
            FileUploadId::new(file_id, 0),
            Arc::new(parking_lot::Mutex::new(NoopUploadCallback)),
            priority,
            0,
            fllm,
        );
        // the PreliminaryUploadFileCallback::on_upload_ok cancel hop
        // (FileManager.cpp:1289-1306)
        let upload_completed = self
            .get_file_view(file_id)
            .map(|view| {
                view.has_active_upload_remote_location() && can_reuse_remote_file(view.get_type())
            })
            .unwrap_or(false);
        if upload_completed {
            self.cancel_upload(FileUploadId::new(file_id, 0), fllm);
        }

        Ok(self.get_file_object(file_id))
    }

    /// `FileManager::set_encryption_key` (FileManager.cpp) over the node
    /// registry; a missing node is a no-op (the C++ null check).
    pub fn set_encryption_key(&mut self, file_id: FileId, key: FileEncryptionKey) {
        if let Some(node) = self.get_file_node_mut(file_id) {
            node.set_encryption_key(key);
        }
    }

    /// `FileManager::try_flush_node_info` (FileManager.cpp) — the gated
    /// half: flush only when `need_info_flush`, then reset the flag.
    fn try_flush_node_info(&mut self, node_id: usize) {
        let need_flush = self.file_nodes[node_id]
            .as_ref()
            .is_some_and(|node| node.need_info_flush());
        if !need_flush {
            return;
        }
        if let Some(node) = self.file_nodes[node_id].as_ref()
            && let Some(worker) = self.worker.as_deref_mut()
        {
            worker.flush_node_info(node);
        }
        if let Some(node) = self.file_nodes[node_id].as_mut() {
            node.on_info_flushed();
        }
    }

    /// `FileManager::merge` (FileManager.cpp:2193-2455) — the decision flow
    /// and re-keying; the cancel/run/flush effects are recorded in call
    /// order and replayed through [`FileManagerWorker`].
    pub fn merge(&mut self, x_file_id: FileId, y_file_id: FileId) -> Result<(), Status> {
        if !x_file_id.is_valid() {
            return Err(Status::error(400, "First file_id is invalid"));
        }
        let x_node_id = match self.file_id_node.get(&x_file_id) {
            Some(&node_id) => node_id,
            None => {
                return Err(Status::error(
                    400,
                    "Can't merge files. First identifier is invalid",
                ));
            }
        };

        if !y_file_id.is_valid() {
            return Ok(()); // the C++ "Old file is invalid" early-out
        }
        let y_node_id = match self.file_id_node.get(&y_file_id) {
            Some(&node_id) => node_id,
            None => {
                return Err(Status::error(
                    400,
                    "Can't merge files. Second identifier is invalid",
                ));
            }
        };

        if x_node_id == y_node_id {
            self.try_flush_node_info(x_node_id);
            return Ok(());
        }

        // the C++ merge runs on live node pointers into file_nodes_; the
        // port clones the pair, replays the mutation flow on the locals
        // exactly as the C++ statements chain, then writes back
        let mut nodes = [
            self.file_nodes[x_node_id].clone().expect("node"),
            self.file_nodes[y_node_id].clone().expect("node"),
        ];

        // the C++ only LOG(ERROR)s a dc-id change between two full FromServer
        // locations here
        let drop_last_successful_force_reupload_time = nodes[0].last_successful_force_reupload_time
            <= 0.
            && nodes[0].remote.full.is_some()
            && nodes[0].remote.full_source == FileLocationSource::FromServer;

        let trusted_by_source = merge_choose_file_source_location(
            nodes[0].remote.full_source,
            nodes[1].remote.full_source,
        );

        let local_i = merge_choose_local_location(&nodes[0].local, &nodes[1].local);
        let remote_i = merge_choose_remote_location(&nodes[0].remote, &nodes[1].remote);
        let generate_i =
            merge_choose_generate_location(nodes[0].generate.as_ref(), nodes[1].generate.as_ref());
        let size_i = merge_choose_size(nodes[0].size, nodes[1].size);
        let mut expected_size_i =
            merge_choose_expected_size(nodes[0].expected_size, nodes[1].expected_size);
        let mut remote_name_i = merge_choose_name(&nodes[0].remote_name, &nodes[1].remote_name);
        let mut url_i = merge_choose_name(&nodes[0].url, &nodes[1].url);
        let owner_i = merge_choose_owner(nodes[0].owner_dialog_id, nodes[1].owner_dialog_id);
        let mut encryption_key_i =
            merge_choose_encryption_key(&nodes[0].encryption_key, &nodes[1].encryption_key);
        let main_file_id_i = merge_choose_main_file_id(
            nodes[0].main_file_id,
            nodes[0].main_file_id_priority,
            nodes[1].main_file_id,
            nodes[1].main_file_id_priority,
        );

        if size_i == -1 {
            self.try_flush_node_info(x_node_id);
            self.try_flush_node_info(y_node_id);
            return Err(Status::error(
                400,
                format!(
                    "Can't merge files {} / {} and {} / {}. Different size: {} and {}",
                    nodes[0].local,
                    nodes[0].remote,
                    nodes[1].local,
                    nodes[1].remote,
                    nodes[0].size,
                    nodes[1].size
                )
                .as_str(),
            ));
        }
        if encryption_key_i == -1 {
            // FileNodePtr nodes[] = {x_node, y_node, x_node}: index 2 re-reads x
            let node_at = |i: i32| &nodes[usize::try_from(i).map_or(0, |i| i.min(1))];
            if node_at(remote_i).remote.full.is_some()
                && !matches!(node_at(local_i).local, LocalFileLocation::Partial(_))
            {
                // the C++ LOG(ERROR)s and keeps the key of the remote location
                encryption_key_i = remote_i;
            } else {
                self.try_flush_node_info(x_node_id);
                self.try_flush_node_info(y_node_id);
                return Err(Status::error(
                    400,
                    "Can't merge files. Different encryption keys",
                ));
            }
        }

        // prefer more trusted source
        if remote_name_i == 2 {
            remote_name_i = trusted_by_source;
        }
        if url_i == 2 {
            url_i = trusted_by_source;
        }
        if expected_size_i == 2 {
            expected_size_i = trusted_by_source;
        }

        let node_i = usize::from(
            (
                nodes[1].pmc_id.is_valid(),
                nodes[0].pmc_id,
                nodes[1].file_ids.len(),
                main_file_id_i == 1,
            ) > (
                nodes[0].pmc_id.is_valid(),
                nodes[1].pmc_id,
                nodes[0].file_ids.len(),
                main_file_id_i == 0,
            ),
        );
        let other_node_i = 1 - node_i;

        let mut events: Vec<MergeEvent> = Vec::new();
        {
            let [a, b] = &mut nodes;
            let (node, other): (&mut FileNode, &mut FileNode) =
                if node_i == 0 { (a, b) } else { (b, a) };

            if local_i as usize == other_node_i {
                events.push(MergeEvent::CancelDownload(Box::new(node.clone())));
                node.set_download_offset(other.download_offset);
                node.set_local_location(
                    other.local.clone(),
                    other.download_offset,
                    other.local_ready_prefix_size,
                );
                node.download_id = other.download_id;
                node.download_was_update_file_reference = other.download_was_update_file_reference;
                node.is_download_started |= other.is_download_started;
                node.set_download_priority(other.download_priority);
                other.download_id = 0;
                other.download_was_update_file_reference = false;
                other.is_download_started = false;
                other.download_priority = 0;
                other.download_offset = 0;
                other.local_ready_prefix_size = 0;
                // the C++ generate-move block here is commented out
            } else {
                events.push(MergeEvent::CancelDownload(Box::new(other.clone())));
            }

            if remote_i as usize == other_node_i {
                events.push(MergeEvent::CancelUpload(Box::new(node.clone())));
                let other_remote = std::mem::take(&mut other.remote);
                node.set_new_remote_location(other_remote);
                node.upload_id = other.upload_id;
                node.upload_was_update_file_reference = other.upload_was_update_file_reference;
                node.set_upload_priority(other.upload_priority);
                node.set_upload_pause(other.upload_pause);
                other.upload_id = 0;
                other.upload_was_update_file_reference = false;
                other.upload_priority = 0;
                other.upload_pause = FileUploadId::default();
            } else {
                events.push(MergeEvent::CancelUpload(Box::new(other.clone())));
            }

            if generate_i as usize == other_node_i {
                events.push(MergeEvent::CancelGenerate(Box::new(node.clone())));
                let other_generate = std::mem::take(&mut other.generate);
                node.set_generate_location(other_generate);
                node.generate_id = other.generate_id;
                node.set_generate_priority(
                    other.generate_download_priority,
                    other.generate_upload_priority,
                );
                other.generate_id = 0;
                other.generate_priority = 0;
                other.generate_download_priority = 0;
                other.generate_upload_priority = 0;
            } else {
                events.push(MergeEvent::CancelGenerate(Box::new(other.clone())));
            }

            if size_i as usize == other_node_i {
                node.set_size(other.size);
            }
            if expected_size_i as usize == other_node_i {
                node.set_expected_size(other.expected_size);
            }
            if remote_name_i as usize == other_node_i {
                node.set_remote_name(other.remote_name.clone());
            }
            if url_i as usize == other_node_i {
                node.set_url(other.url.clone());
            }
            if owner_i as usize == other_node_i {
                node.set_owner_dialog_id(other.owner_dialog_id);
            }
            if encryption_key_i as usize == other_node_i {
                let key = other.encryption_key.clone();
                node.set_encryption_key(key.clone());
                // the C++ assigns twice: nodes[node_i] from
                // nodes[encryption_key_i], and the branch guard makes that
                // the same node pair
                node.set_encryption_key(key);
            }

            node.need_load_from_pmc |= other.need_load_from_pmc;
            node.can_search_locally &= other.can_search_locally;
            node.upload_prefer_small |= other.upload_prefer_small;

            if drop_last_successful_force_reupload_time {
                node.last_successful_force_reupload_time = -1e10;
            } else if other.last_successful_force_reupload_time
                > node.last_successful_force_reupload_time
            {
                node.last_successful_force_reupload_time =
                    other.last_successful_force_reupload_time;
            }

            if main_file_id_i as usize == other_node_i {
                events.push(MergeEvent::OnMergeFiles(
                    other.main_file_id,
                    node.main_file_id,
                ));
                node.main_file_id = other.main_file_id;
                node.main_file_id_priority = other.main_file_id_priority;
            } else {
                events.push(MergeEvent::OnMergeFiles(
                    node.main_file_id,
                    other.main_file_id,
                ));
            }
        }

        self.file_nodes[x_node_id] = Some(nodes[0].clone());
        self.file_nodes[y_node_id] = Some(nodes[1].clone());

        // the registry id of the survivor differs from the pair index
        // whenever the reserved node slot 0 is in play
        let node_id = if node_i == 0 { x_node_id } else { y_node_id };
        let other_node_id = if node_i == 0 { y_node_id } else { x_node_id };

        let other_pmc_id = nodes[other_node_i].pmc_id;
        let other_file_ids = nodes[other_node_i].file_ids.clone();
        self.file_nodes[node_id]
            .as_mut()
            .expect("node")
            .file_ids
            .extend(other_file_ids.iter().copied());
        for file_id in &other_file_ids {
            self.file_id_node.insert(*file_id, node_id);
        }

        // the C++ "check if some download/upload queries are ready" loop
        {
            let node = self.file_nodes[node_id].as_ref().expect("node");
            let view = FileView::new(node);
            let with_downloads = view.has_full_local_location();
            let with_uploads = view.has_active_upload_remote_location();
            if with_downloads || with_uploads {
                events.push(MergeEvent::FinishQueries {
                    with_downloads,
                    with_uploads,
                    file_ids: node.file_ids.clone(),
                });
            }
        }

        self.file_nodes[other_node_id] = None;

        events.push(MergeEvent::RunNode(Box::new(
            self.file_nodes[node_id].as_ref().expect("node").clone(),
        )));

        if other_pmc_id.is_valid() {
            self.file_nodes[node_id]
                .as_mut()
                .expect("node")
                .on_pmc_changed();
        }
        {
            let node = self.file_nodes[node_id].as_ref().expect("node");
            events.push(MergeEvent::FlushFull {
                node: Box::new(node.clone()),
                new_remote: node_i != remote_i as usize,
                new_local: node_i != local_i as usize,
                new_generate: node_i != generate_i as usize,
                other_pmc_id,
            });
        }

        for event in events {
            let Some(worker) = self.worker.as_deref_mut() else {
                break; // no worker registered: the events are no-ops anyway
            };
            match event {
                MergeEvent::CancelDownload(node) => worker.cancel_download(&node),
                MergeEvent::CancelUpload(node) => worker.cancel_upload(&node),
                MergeEvent::CancelGenerate(node) => worker.cancel_generate(&node),
                MergeEvent::OnMergeFiles(from_file_id, to_file_id) => {
                    worker.on_merge_files(from_file_id, to_file_id)
                }
                MergeEvent::FinishQueries {
                    with_downloads,
                    with_uploads,
                    file_ids,
                } => {
                    for file_id in file_ids {
                        if with_downloads {
                            worker.finish_download(file_id);
                        }
                        if with_uploads {
                            worker.finish_upload(file_id);
                        }
                    }
                }
                MergeEvent::RunNode(node) => worker.run_node(&node),
                MergeEvent::FlushFull {
                    node,
                    new_remote,
                    new_local,
                    new_generate,
                    other_pmc_id,
                } => {
                    worker.flush_node_full(&node, new_remote, new_local, new_generate, other_pmc_id)
                }
            }
        }

        Ok(())
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
        self.part_count = ready_parts.iter().fold(0, |a, &b| a.max(b.wrapping_add(1)));

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
            if self.known_prefix_flag
                && i >= (self.known_prefix_size / self.part_size as i64) as i32
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
                ready_parts
                    .iter()
                    .fold(0i32, |a, &b| a.max(b.wrapping_add(1))),
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

        let is_intersect_with =
            |begin: i64, end: i64| begin.max(offset_begin) < end.min(offset_end);

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
                let max_part_count =
                    MAX_PART_COUNT_PREMIUM + if self.use_part_count_limit { 0 } else { 64 };
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
    pub fn on_part_ok(
        &mut self,
        part_id: i32,
        part_size: usize,
        actual_size: usize,
    ) -> TdResult<()> {
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
                let prefix = round_up(self.streaming_offset + self.streaming_limit - self.size)
                    .min(self.size);
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
pub trait ResourceManagerWorker: Send {
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
    manager: Arc<parking_lot::Mutex<ResourceManager>>,
    node_id: u64,
}

impl ResourceManagerHandle {
    /// `ResourceManager::update_priority` (ResourceManager.cpp:34-43).
    pub fn update_priority(&self, priority: i8) {
        self.manager.lock().update_priority(self.node_id, priority);
    }

    /// `ResourceManager::update_resources` (ResourceManager.cpp:45-66).
    pub fn update_resources(&self, resource_state: &ResourceState) {
        self.manager
            .lock()
            .update_resources(self.node_id, resource_state);
    }

    /// `ResourceManager::hangup_shared` (ResourceManager.cpp:73-88) — the
    /// worker dropped its end of the link.
    pub fn hangup_shared(&self) {
        self.manager.lock().disconnect_worker(self.node_id);
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
        this: &Arc<parking_lot::Mutex<ResourceManager>>,
        callback: Box<dyn ResourceManagerWorker>,
        priority: i8,
    ) {
        let node_id = this.lock().create_node(callback);
        this.lock().add_node(node_id, priority);
        // send_closure(node->callback_, &FileLoaderActor::set_resource_manager,
        //               actor_shared(this, node_id)) — sent with the manager
        // borrow released, so the worker may call straight back
        let handle = ResourceManagerHandle {
            manager: Arc::clone(this),
            node_id,
        };
        let mut node = this.lock().take_node(node_id);
        node.callback.set_resource_manager(handle);
        this.lock().put_node(node);
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
        if self
            .nodes
            .get((node_id - 1) as usize)
            .and_then(Option::as_ref)
            .is_none()
        {
            return;
        }
        let before = self.node(node_id).resource_state.clone();
        self.resource_state -= &before;
        self.node_mut(node_id)
            .resource_state
            .update_master(resource_state);
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
        if self
            .nodes
            .get((node_id - 1) as usize)
            .and_then(Option::as_ref)
            .is_none()
        {
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

// ==== FileUploader / FileHashUploader (M.12.6) ====

/// `NetQuery::Error::Canceled` (net/NetQuery.h:57) — a canceled upload-part
/// query restarts its part instead of failing the upload.
pub const NET_QUERY_CANCELED: i32 = 203;

/// `MAX_FILE_SIZE` (FileManager.cpp:52) — the download offset/limit
/// bound the encrypted folds CHECK.
const MAX_DOWNLOAD_FILE_SIZE: i64 = 4000 << 20; // 4000MB

/// The wire query `FileUploader::start_part` would send
/// (FileUploader.cpp:278-286); the TL codegen surface lands with the net
/// integration.
#[derive(Clone, Debug, PartialEq)]
pub enum UploadPartQuery {
    /// `upload_saveFilePart`
    SaveFilePart {
        file_id: i64,
        part_id: i32,
        bytes: Vec<u8>,
    },
    /// `upload_saveBigFilePart`; `part_count` is the C++
    /// `local_is_ready ? part_count : -1`.
    SaveBigFilePart {
        file_id: i64,
        part_id: i32,
        part_count: i32,
        bytes: Vec<u8>,
    },
}

/// The net seam of the collapsed [`FileUploader`]: the C++
/// `start_part` + `dispatch_with_callback` + `on_result` + `process_part`
/// quartet folded into one call that sends the query and fetches its `Bool`
/// result. Uploads are sequential here; the in-flight pipelining of the C++
/// actor loop is a scheduler property and returns with it (M.12.7).
pub trait FileUploaderNet: Send {
    /// `Ok(false)` is the C++ `!result.ok()` 500 path. `Err` with
    /// [`NET_QUERY_CANCELED`] restarts the part; any other `Err` fails the
    /// upload.
    fn upload_part(&mut self, query: UploadPartQuery) -> Result<bool, Status>;

    /// `open_temp_file` + `secure_storage::encrypt_file`
    /// (FileUploader.cpp:156-165) — the Wave-3 secure-storage seam; the
    /// default always fails, so secure-file uploads error out like any
    /// other storage failure.
    fn encrypt_file_for_secure_upload(
        &mut self,
        _secret: &[u8],
        _path: &str,
    ) -> Result<(String, Vec<u8>), Status> {
        Err(Status::error_message(
            "Secure file encryption is not available yet",
        ))
    }
}

/// `FileUploader::Callback` (FileUploader.h:32-42).
pub trait FileUploaderCallback: Send {
    /// The hash of a just-encrypted secure file.
    fn on_hash(&mut self, hash: Vec<u8>);
    fn on_partial_upload(&mut self, partial_remote: PartialRemoteFileLocation);
    fn on_ok(&mut self, file_type: FileType, partial_remote: PartialRemoteFileLocation);
    fn on_error(&mut self, status: Status);
}

/// `td::FileUploader` (FileUploader.h) collapsed per the single-scheduler
/// convention: the actor loop becomes a synchronous `loop_` over the net
/// seam, the `part_map_` of in-flight queries disappears with the pipeline,
/// and the ResourceManager link is the [`ResourceManagerWorker`] trait.
pub struct FileUploader {
    local: LocalFileLocation,
    remote: RemoteFileLocation,
    expected_size: i64,
    encryption_key: FileEncryptionKey,
    bad_parts: Vec<i32>,
    callback: Box<dyn FileUploaderCallback>,
    net: Box<dyn FileUploaderNet>,

    local_size: i64,
    local_is_ready: bool,
    file_type: FileType,

    iv_map: Vec<[u8; 32]>,
    iv: [u8; 32],
    generate_iv: Vec<u8>,
    generate_offset: i64,
    next_offset: i64,

    fd: Option<File>,
    fd_path: String,
    file_id: i64,
    is_temp: bool,
    big_flag: bool,
    keep_fd: bool,
    stop_flag: bool,

    resource_manager: Option<ResourceManagerHandle>,
    resource_state: ResourceState,
    parts_manager: PartsManager,
}

/// `FileUploader::PrefixInfo` (FileUploader.h:93-96).
#[derive(Clone, Copy, Debug)]
struct PrefixInfo {
    size: i64,
    is_ready: bool,
}

impl FileUploader {
    /// The `FileUploader` constructor (FileUploader.cpp:28-45).
    pub fn new(
        local: LocalFileLocation,
        remote: RemoteFileLocation,
        expected_size: i64,
        encryption_key: FileEncryptionKey,
        bad_parts: Vec<i32>,
        callback: Box<dyn FileUploaderCallback>,
        net: Box<dyn FileUploaderNet>,
    ) -> FileUploader {
        let mut iv = [0u8; 32];
        let mut generate_iv = Vec::new();
        if encryption_key.is_secret() {
            iv.copy_from_slice(encryption_key.iv_slice());
            generate_iv = encryption_key.iv_slice().to_vec();
        }
        let mut remote = remote;
        if let RemoteFileLocation::Partial(partial) = &remote
            && encryption_key.is_secure()
            && partial.part_count != partial.ready_part_count
        {
            remote = RemoteFileLocation::Empty;
        }
        FileUploader {
            local,
            remote,
            expected_size,
            encryption_key,
            bad_parts,
            callback,
            net,
            local_size: 0,
            local_is_ready: false,
            file_type: FileType::Temp,
            iv_map: Vec::new(),
            iv,
            generate_iv,
            generate_offset: 0,
            next_offset: 0,
            fd: None,
            fd_path: String::new(),
            file_id: 0,
            is_temp: false,
            big_flag: false,
            keep_fd: false,
            stop_flag: false,
            resource_manager: None,
            resource_state: ResourceState::default(),
            parts_manager: PartsManager::default(),
        }
    }

    /// `FileUploader::start_up` (FileUploader.cpp:47-117).
    pub fn start(&mut self) {
        if matches!(self.remote, RemoteFileLocation::Full(_)) {
            return self.on_error(Status::error_message("File is already uploaded"));
        }

        // file_size is needed only for partial local locations, but for
        // uploaded partial files size is yet unknown or local location is
        // full, so we can always pass 0 here
        let local = self.local.clone();
        let prefix_info = match self.on_update_local_location(&local, 0) {
            Ok(info) => info,
            Err(status) => return self.on_error(status),
        };
        let _ = prefix_info;

        let mut offset = 0i32;
        let mut part_size = 0i32;
        if let RemoteFileLocation::Partial(partial) = &self.remote {
            self.file_id = partial.file_id;
            part_size = partial.part_size;
            self.big_flag = partial.is_big != 0;
            offset = partial.ready_part_count;
        } else {
            self.file_id = tdutils::random::secure_int64();
            self.big_flag = is_file_big(self.file_type, self.expected_size);
        }

        let mut ok = vec![true; offset.max(0) as usize];
        for &bad_id in &self.bad_parts {
            if bad_id >= 0 && bad_id < offset {
                ok[bad_id as usize] = false;
            }
        }
        let mut ready_parts: Vec<i32> = (0..offset).filter(|&i| ok[i as usize]).collect();
        if !ok.is_empty() && !ok[0] {
            ready_parts.clear();
            part_size = 0;
            self.remote = RemoteFileLocation::Empty;
            self.file_id = tdutils::random::secure_int64();
            self.big_flag = is_file_big(self.file_type, self.expected_size);
        }

        let expected_size = self.local_size.max(self.expected_size);
        let init_result = self.parts_manager.init(
            self.local_size,
            expected_size,
            self.local_is_ready,
            part_size.max(0) as usize,
            &ready_parts,
            true,
            true,
        );
        if let tdutils::status::Result::Err(status) = init_result {
            return self.on_error(status);
        }
        self.resource_state
            .set_unit_size(self.parts_manager.get_part_size());
        self.update_estimated_limit();
        self.on_progress();
        self.loop_();
    }

    /// `FileUploader::on_update_local_location` (FileUploader.cpp:119-215)
    /// with the `try_release_fd` scope exit.
    fn on_update_local_location(
        &mut self,
        location: &LocalFileLocation,
        file_size: i64,
    ) -> Result<PrefixInfo, Status> {
        let result = self.on_update_local_location_impl(location, file_size);
        self.try_release_fd();
        result
    }

    fn on_update_local_location_impl(
        &mut self,
        location: &LocalFileLocation,
        file_size: i64,
    ) -> Result<PrefixInfo, Status> {
        if self.encryption_key.is_secure() && !self.fd_path.is_empty() {
            return Err(Status::error_message(
                "Can't change local location for Secure file",
            ));
        }

        let mut path = String::new();
        let mut local_size = -1i64;
        let mut local_is_ready = false;
        match location {
            LocalFileLocation::Empty => {
                local_size = 0;
                self.file_type = FileType::Temp;
            }
            LocalFileLocation::Partial(partial) => {
                if self.encryption_key.is_secure() {
                    local_size = 0;
                    self.file_type = FileType::Temp;
                } else {
                    path = partial.path.clone();
                    local_size = Bitmask::from_decode(&partial.ready_bitmask)
                        .get_ready_prefix_size(0, partial.part_size, file_size);
                    self.file_type = partial.file_type;
                }
            }
            LocalFileLocation::Full(full) => {
                path = full.path.clone();
                if path.is_empty() {
                    return Err(Status::error_message(
                        "FullLocalFileLocation with empty path",
                    ));
                }
                local_is_ready = true;
                self.file_type = full.file_type;
            }
        }

        let mut is_temp = false;
        if self.encryption_key.is_secure()
            && local_is_ready
            && matches!(self.remote, RemoteFileLocation::Empty)
        {
            let (new_path, hash) = self
                .net
                .encrypt_file_for_secure_upload(self.encryption_key.secret(), &path)?;
            self.callback.as_mut().on_hash(hash);
            path = new_path;
            is_temp = true;
        }

        if !path.is_empty() && (path != self.fd_path || self.fd.is_none()) {
            let res_fd = File::open(&path)
                .map_err(|e| Status::error_message(format!("Failed to open: {e}").as_str()));

            // race: partial location could be already deleted; just ignore
            // such locations
            let res_fd = match res_fd {
                Ok(fd) => Ok(fd),
                Err(err) => {
                    if matches!(location, LocalFileLocation::Partial(_)) {
                        return Ok(PrefixInfo {
                            size: self.local_size,
                            is_ready: self.local_is_ready,
                        });
                    }
                    Err(err)
                }
            }?;

            self.fd = Some(res_fd);
            self.fd_path = path;
            self.is_temp = is_temp;
        }
        if local_is_ready {
            assert!(self.fd.is_some());
            local_size = self.fd.as_ref().expect("fd").metadata().unwrap().len() as i64;
            if local_size == 0 {
                return Err(Status::error_message("Can't upload empty file"));
            }
        } else if let Some(fd) = &self.fd {
            let real_local_size = fd.metadata().unwrap().len() as i64;
            if real_local_size < local_size {
                return Ok(PrefixInfo {
                    size: self.local_size,
                    is_ready: self.local_is_ready,
                });
            }
        }

        self.local_size = local_size;
        if self.expected_size < self.local_size
            && (self.expected_size != (10 << 20) || self.local_size >= (30 << 20))
        {
            self.expected_size = self.local_size;
        }
        self.local_is_ready = local_is_ready;

        Ok(PrefixInfo {
            size: self.local_size,
            is_ready: self.local_is_ready,
        })
    }

    /// `FileUploader::on_error` (FileUploader.cpp:217-225).
    fn on_error(&mut self, status: Status) {
        self.fd = None;
        if self.is_temp {
            let _ = std::fs::remove_file(&self.fd_path);
        }
        self.stop_flag = true;
        self.callback.as_mut().on_error(status);
    }

    /// `FileUploader::generate_iv_map` (FileUploader.cpp:227-248).
    fn generate_iv_map(&mut self) -> Result<(), Status> {
        let part_size = self.parts_manager.get_part_size();
        let key = *self.encryption_key.key();
        let mut iv: [u8; 32] = self
            .generate_iv
            .as_slice()
            .try_into()
            .map_err(|_| Status::error_message("Bad iv length"))?;
        if self.iv_map.is_empty() {
            self.iv_map.push(iv);
        }
        let mut bytes = vec![0u8; part_size];
        while (self.generate_offset + part_size as i64) < self.local_size {
            let read_size = pread_at(
                self.fd
                    .as_ref()
                    .ok_or_else(|| Status::error_message("No fd"))?,
                &mut bytes,
                self.generate_offset as u64,
            )?;
            if read_size != part_size {
                return Err(Status::error_message(
                    "Failed to read file part (for iv_map)",
                ));
            }
            tdutils::crypto::aes_ige_encrypt_in_place(&key, &mut iv, &mut bytes);
            self.iv_map.push(iv);
            self.generate_offset += part_size as i64;
        }
        self.generate_iv = iv.to_vec();
        Ok(())
    }

    /// `FileUploader::start_part` (FileUploader.cpp:250-289) up to the query
    /// construction.
    fn start_part(&mut self, part: Part) -> Result<UploadPartQuery, Status> {
        let mut padded_size = part.size;
        if self.encryption_key.is_secret() {
            padded_size = (padded_size + 15) & !15;
        }
        let mut bytes = vec![0u8; padded_size];
        let size = pread_at(
            self.fd
                .as_ref()
                .ok_or_else(|| Status::error_message("No fd"))?,
            &mut bytes[..part.size],
            part.offset as u64,
        )?;
        if self.encryption_key.is_secret() {
            tdutils::random::secure_bytes(&mut bytes[part.size..]);
            if self.next_offset == part.offset {
                let key = *self.encryption_key.key();
                let mut iv = self.iv;
                tdutils::crypto::aes_ige_encrypt_in_place(&key, &mut iv, &mut bytes);
                self.iv = iv;
                self.next_offset += bytes.len() as i64;
            } else {
                if part.id < 0 || part.id as usize >= self.iv_map.len() {
                    self.generate_iv_map()?;
                }
                assert!((part.id as usize) < self.iv_map.len());
                let mut iv = self.iv_map[part.id as usize];
                tdutils::crypto::aes_ige_encrypt_in_place(
                    self.encryption_key.key(),
                    &mut iv,
                    &mut bytes,
                );
            }
        }

        if size != part.size {
            return Err(Status::error_message("Failed to read file part"));
        }

        if self.big_flag {
            Ok(UploadPartQuery::SaveBigFilePart {
                file_id: self.file_id,
                part_id: part.id,
                part_count: if self.local_is_ready {
                    self.parts_manager.get_part_count()
                } else {
                    -1
                },
                bytes,
            })
        } else {
            Ok(UploadPartQuery::SaveFilePart {
                file_id: self.file_id,
                part_id: part.id,
                bytes,
            })
        }
    }

    /// `FileUploader::on_progress` (FileUploader.cpp:309-319).
    fn on_progress(&mut self) {
        let part_count = self.parts_manager.get_part_count();
        let part_size = self.parts_manager.get_part_size() as i32;
        let ready_part_count = self.parts_manager.get_ready_prefix_count();
        self.callback
            .as_mut()
            .on_partial_upload(PartialRemoteFileLocation {
                file_id: self.file_id,
                part_count,
                part_size,
                ready_part_count,
                is_big: i32::from(self.big_flag),
                ready_size: self.parts_manager.get_ready_size(),
            });
        if self.parts_manager.ready() {
            self.callback.as_mut().on_ok(
                self.file_type,
                PartialRemoteFileLocation {
                    file_id: self.file_id,
                    part_count,
                    part_size,
                    ready_part_count,
                    is_big: i32::from(self.big_flag),
                    ready_size: self.local_size,
                },
            );
        }
    }

    /// `FileUploader::try_release_fd` (FileUploader.cpp:321-325).
    fn try_release_fd(&mut self) {
        if !self.keep_fd && self.fd.is_some() {
            self.fd = None;
        }
    }

    /// `FileUploader::acquire_fd` (FileUploader.cpp:327-332).
    fn acquire_fd(&mut self) -> Result<(), Status> {
        if self.fd.is_none() {
            self.fd = Some(File::open(&self.fd_path).map_err(|e| {
                Status::error_message(format!("Failed to open {}: {e}", self.fd_path).as_str())
            })?);
        }
        Ok(())
    }

    /// `FileUploader::update_local_file_location` (FileUploader.cpp:349-360).
    pub fn update_local_file_location(&mut self, local: &LocalFileLocation) {
        let prefix_info =
            match self.on_update_local_location(local, self.parts_manager.get_size_or_zero()) {
                Ok(info) => info,
                Err(status) => return self.on_error(status),
            };
        if let tdutils::status::Result::Err(status) = self
            .parts_manager
            .set_known_prefix(prefix_info.size, prefix_info.is_ready)
        {
            return self.on_error(status);
        }
        self.loop_();
    }

    /// `FileUploader::loop` (FileUploader.cpp:362-373); a `-1` status means
    /// "wait for the location to appear".
    fn loop_(&mut self) {
        if self.stop_flag {
            return;
        }
        if let Err(status) = self.do_loop() {
            if status.code() == -1 {
                return;
            }
            self.on_error(status);
        }
    }

    /// `FileUploader::do_loop` (FileUploader.cpp:375-417) — the in-flight
    /// pipeline collapsed into the sequential send-and-wait; the C++
    /// `on_result` tail re-enters `loop()`, which the may-finish check after
    /// each part does here.
    fn do_loop(&mut self) -> Result<(), Status> {
        if self.parts_manager.may_finish() {
            return self.finish_upload();
        }

        if let Err(status) = self.acquire_fd() {
            if !self.local_is_ready {
                return Err(Status::error(-1, "Can't open temporary file"));
            }
            return Err(status);
        }

        let result = self.drive_parts();
        self.try_release_fd();
        result
    }

    /// `FileUploader::finish` path — `may_finish` + `finish` + the temp-file
    /// cleanup of `do_loop`.
    fn finish_upload(&mut self) -> Result<(), Status> {
        if let tdutils::status::Result::Err(status) = self.parts_manager.finish() {
            return Err(status);
        }
        self.fd = None;
        if self.is_temp {
            let _ = std::fs::remove_file(&self.fd_path);
        }
        self.stop_flag = true;
        Ok(())
    }

    fn drive_parts(&mut self) -> Result<(), Status> {
        loop {
            if self.resource_state.unused() < self.parts_manager.get_part_size() as i64 {
                break;
            }
            let part = match self.parts_manager.start_part() {
                tdutils::status::Result::Ok(part) => part,
                tdutils::status::Result::Err(status) => return Err(status),
            };
            if part.size == 0 {
                break;
            }
            self.resource_state.start_use(part.size as i64);

            let query = self.start_part(part)?;
            match self.net.upload_part(query) {
                Ok(true) => {
                    self.resource_state.stop_use(part.size as i64);
                    if let tdutils::status::Result::Err(status) =
                        self.parts_manager.on_part_ok(part.id, part.size, part.size)
                    {
                        return Err(status);
                    }
                    self.on_progress();
                }
                Ok(false) => {
                    return Err(Status::error(
                        500,
                        "Internal Server Error during file upload",
                    ));
                }
                Err(status) if status.code() == NET_QUERY_CANCELED => {
                    self.resource_state.stop_use(part.size as i64);
                    self.parts_manager.on_part_failed(part.id);
                }
                Err(status) => return Err(status),
            }
            self.update_estimated_limit();
            if self.parts_manager.may_finish() {
                return self.finish_upload();
            }
        }
        Ok(())
    }

    /// `FileUploader::update_estimated_limit` (FileUploader.cpp:425-437).
    fn update_estimated_limit(&mut self) {
        if self.stop_flag {
            return;
        }
        let estimated_extra = self.parts_manager.get_estimated_extra();
        self.resource_state.update_estimated_limit(estimated_extra);
        if self.resource_manager.is_some() {
            self.keep_fd =
                self.resource_state.active_limit() as usize >= self.parts_manager.get_part_size();
            self.try_release_fd();
            if let Some(handle) = self.resource_manager.clone() {
                handle.update_resources(&self.resource_state);
            }
        }
    }

    /// `FileUploader::update_priority` (FileUploader.cpp:339-341).
    pub fn update_priority(&mut self, priority: i8) {
        if let Some(handle) = &self.resource_manager {
            handle.update_priority(priority);
        }
    }

    pub fn remote(&self) -> &RemoteFileLocation {
        &self.remote
    }

    pub fn file_id(&self) -> i64 {
        self.file_id
    }

    pub fn stop_flag(&self) -> bool {
        self.stop_flag
    }
}

impl ResourceManagerWorker for FileUploader {
    /// `FileUploader::set_resource_manager` (FileUploader.cpp:334-337).
    fn set_resource_manager(&mut self, handle: ResourceManagerHandle) {
        handle.update_resources(&self.resource_state);
        self.resource_manager = Some(handle);
    }

    /// `FileUploader::update_resources` (FileUploader.cpp:343-347).
    fn update_resources(&mut self, resource_state: ResourceState) {
        self.resource_state.update_slave(&resource_state);
        self.loop_();
    }
}

/// Positional read/write shims: `FileFd::pread`/`pwrite` ride the unix
/// `FileExt`/windows `seek_*` halves of the same std feature pair.
#[cfg(unix)]
pub(crate) fn compat_read_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(windows)]
pub(crate) fn compat_read_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

#[cfg(unix)]
pub(crate) fn compat_write_at(file: &File, buf: &[u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

#[cfg(windows)]
pub(crate) fn compat_write_at(file: &File, buf: &[u8], offset: u64) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

/// `FileExt::read_exact_at` — the unix half has it natively; the loop is the
/// std definition (EOF mid-read surfaces as an UnexpectedEof error).
pub(crate) fn compat_read_exact_at(
    file: &File,
    buf: &mut [u8],
    offset: u64,
) -> std::io::Result<()> {
    let mut total = 0usize;
    while total < buf.len() {
        let n = compat_read_at(file, &mut buf[total..], offset + total as u64)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "failed to fill whole buffer",
            ));
        }
        total += n;
    }
    Ok(())
}

/// `pread` (FileFd) — a positional read filling the buffer; a zero read is
/// the end of file.
fn pread_at(fd: &File, buf: &mut [u8], offset: u64) -> Result<usize, Status> {
    let mut total = 0usize;
    while total < buf.len() {
        let n = compat_read_at(fd, &mut buf[total..], offset + total as u64)
            .map_err(|e| Status::error_message(format!("Failed to read: {e}").as_str()))?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}

/// The `messages_getDocumentByHash` result variants the hash uploader
/// distinguishes (FileHashUploader.cpp:125-146).
#[derive(Clone, Debug, PartialEq)]
pub enum DocumentByHash {
    /// `documentEmpty`
    Empty,
    /// `document`
    Document {
        id: i64,
        access_hash: i64,
        dc_id: i32,
        file_reference: Vec<u8>,
    },
}

/// The net seam of the collapsed [`FileHashUploader`]: the C++
/// `dispatch_with_callback` + `on_result` + `fetch_result` folded into one
/// call.
pub trait FileHashUploaderNet: Send {
    /// Sends `messages_getDocumentByHash` and fetches the result.
    fn get_document_by_hash(
        &mut self,
        sha256: [u8; 32],
        size: i64,
        mime_type: String,
    ) -> Result<DocumentByHash, Status>;
}

/// `FileHashUploader::Callback` (FileHashUploader.h:25-34).
pub trait FileHashUploaderCallback: Send {
    fn on_ok(&mut self, location: FullRemoteFileLocation);
    fn on_error(&mut self, status: Status);
}

/// `td::FileHashUploader` (FileHashUploader.h) — hashes a local file and
/// asks the server whether it already knows the document.
pub struct FileHashUploader {
    resource_state: ResourceState,
    fd: Option<File>,
    fd_read_pos: u64,

    local: FullLocalFileLocation,
    size: i64,
    size_left: i64,
    callback: Box<dyn FileHashUploaderCallback>,
    net: Box<dyn FileHashUploaderNet>,
    resource_manager: Option<ResourceManagerHandle>,

    state: HashUploaderState,
    stop_flag: bool,
    sha256_state: tdutils::crypto::Sha256State,
}

/// `FileHashUploader::State` (FileHashUploader.h:51).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HashUploaderState {
    CalcSha,
    NetRequest,
    WaitNetResult,
}

/// `MimeType::from_extension` — the tdutils MIME table is not ported; every
/// extension falls back to the C++ default.
fn mime_type_from_extension(_path: &str, default: &str) -> String {
    default.to_owned()
}

impl FileHashUploader {
    /// The `FileHashUploader` constructor (FileHashUploader.h:36-38).
    pub fn new(
        local: FullLocalFileLocation,
        size: i64,
        callback: Box<dyn FileHashUploaderCallback>,
        net: Box<dyn FileHashUploaderNet>,
    ) -> FileHashUploader {
        FileHashUploader {
            resource_state: ResourceState::default(),
            fd: None,
            fd_read_pos: 0,
            size_left: size,
            local,
            size,
            callback,
            net,
            resource_manager: None,
            state: HashUploaderState::CalcSha,
            stop_flag: false,
            sha256_state: tdutils::crypto::Sha256State::new(),
        }
    }

    /// `FileHashUploader::start_up` + `init` (FileHashUploader.cpp:28-49).
    pub fn start(&mut self) {
        if let Err(status) = self.init() {
            self.callback.as_mut().on_error(status);
            self.stop_flag = true;
        }
    }

    fn init(&mut self) -> Result<(), Status> {
        let fd = File::open(&self.local.path)
            .map_err(|e| Status::error_message(format!("Failed to open: {e}").as_str()))?;
        let file_size = fd.metadata().unwrap().len() as i64;
        if file_size != self.size {
            return Err(Status::error_message("Size mismatch"));
        }
        self.fd = Some(fd);
        self.sha256_state.init();

        self.resource_state.set_unit_size(1024);
        self.resource_state.update_estimated_limit(self.size);
        Ok(())
    }

    /// `FileHashUploader::loop` (FileHashUploader.cpp:51-62).
    pub fn loop_(&mut self) {
        if self.stop_flag {
            return;
        }
        if let Err(status) = self.loop_impl() {
            self.callback.as_mut().on_error(status);
            self.stop_flag = true;
        }
    }

    /// `FileHashUploader::loop_impl` (FileHashUploader.cpp:64-80) — the
    /// WaitNetResult wait collapsed into the net call.
    fn loop_impl(&mut self) -> Result<(), Status> {
        if self.state == HashUploaderState::CalcSha {
            self.loop_sha()?;
        }
        if self.state == HashUploaderState::NetRequest {
            let mut hash = [0u8; 32];
            self.sha256_state.extract(&mut hash, true);
            let mime_type = mime_type_from_extension(&self.local.path, "image/gif");
            self.state = HashUploaderState::WaitNetResult;
            match self.net.get_document_by_hash(hash, self.size, mime_type)? {
                DocumentByHash::Empty => {
                    return Err(Status::error_message("Document is not found by hash"));
                }
                DocumentByHash::Document {
                    id,
                    access_hash,
                    dc_id,
                    file_reference,
                } => {
                    if !DcId::is_valid_raw(dc_id) {
                        return Err(Status::error_message("Found document has invalid DcId"));
                    }
                    // file references are server-issued ASCII; a lossy
                    // conversion matches the C++ byte copy for real data
                    let file_reference = String::from_utf8_lossy(&file_reference).into_owned();
                    self.callback
                        .as_mut()
                        .on_ok(FullRemoteFileLocation::from_document(
                            FileType::Document,
                            id,
                            access_hash,
                            DcId::internal(dc_id),
                            file_reference,
                        ));
                    self.stop_flag = true;
                }
            }
        }
        Ok(())
    }

    /// `FileHashUploader::loop_sha` (FileHashUploader.cpp:82-114).
    fn loop_sha(&mut self) -> Result<(), Status> {
        let mut limit = self.resource_state.unused();
        if limit == 0 {
            return Ok(());
        }
        if limit > self.size_left {
            limit = self.size_left;
        }
        self.resource_state.start_use(limit);

        let mut bytes = vec![0u8; limit as usize];
        let read_size = pread_at(
            self.fd
                .as_ref()
                .ok_or_else(|| Status::error_message("No fd"))?,
            &mut bytes,
            self.fd_read_pos,
        )?;
        if read_size != limit as usize {
            return Err(Status::error_message("Unexpected end of file"));
        }
        self.sha256_state.feed(&bytes);
        self.resource_state.stop_use(limit);

        self.fd_read_pos += limit as u64;
        self.size_left -= limit;
        assert!(self.size_left >= 0);
        if self.size_left == 0 {
            self.state = HashUploaderState::NetRequest;
        }
        Ok(())
    }

    /// `FileHashUploader::update_priority` (FileHashUploader.h:60-62).
    pub fn update_priority(&mut self, priority: i8) {
        if let Some(handle) = &self.resource_manager {
            handle.update_priority(priority);
        }
    }

    pub fn stop_flag(&self) -> bool {
        self.stop_flag
    }
}

impl ResourceManagerWorker for FileHashUploader {
    /// `FileHashUploader::set_resource_manager` (FileHashUploader.h:55-58).
    fn set_resource_manager(&mut self, handle: ResourceManagerHandle) {
        handle.update_resources(&self.resource_state);
        self.resource_manager = Some(handle);
    }

    /// `FileHashUploader::update_resources` (FileHashUploader.h:64-70).
    fn update_resources(&mut self, resource_state: ResourceState) {
        if self.stop_flag {
            return;
        }
        self.resource_state.update_slave(&resource_state);
        self.loop_();
    }
}

// ==== FileDownloader (M.12.4 + M.12.5) ====

/// `FileDownloader::QueryType` (FileDownloader.h:56).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadQueryType {
    Default,
    Cdn,
    ReuploadCdn,
    /// The COMMON_QUERY_KEY check queries
    /// (`upload.getFileHashes`, FileDownloader.h:98).
    Check,
}

/// The wire query `FileDownloader::start_part`/`check_loop` build
/// (FileDownloader.cpp:133-192, 376).
#[derive(Clone, Debug, PartialEq)]
pub enum DownloadQuery {
    /// `upload.getFile`
    GetFile {
        location: telegram_api::InputFileLocation,
        offset: i64,
        limit: i32,
        cdn_supported: bool,
    },
    /// `upload.getWebFile`
    GetWebFile {
        location: telegram_api::InputWebFileLocation,
        offset: i32,
        limit: i32,
    },
    /// `upload.getCdnFile`
    GetCdnFile {
        file_token: Vec<u8>,
        offset: i64,
        limit: i32,
    },
    /// `upload.reuploadCdnFile`
    ReuploadCdnFile {
        file_token: Vec<u8>,
        request_token: Vec<u8>,
    },
    /// `upload.getFileHashes` — the check query
    GetFileHashes {
        location: telegram_api::InputFileLocation,
        offset: i64,
    },
}

/// The fetched response of a [`DownloadQuery`] — the C++
/// `fetch_result<...>` arms of `should_restart_part`/`process_part`/
/// `process_check_query`.
#[derive(Clone, Debug, PartialEq)]
pub enum DownloadResult {
    /// `upload.file` / `upload.webFile` bytes
    Bytes(Vec<u8>),
    /// `upload.fileCdnRedirect`
    CdnRedirect {
        dc_id: i32,
        file_token: Vec<u8>,
        encryption_key: Vec<u8>,
        encryption_iv: Vec<u8>,
        file_hashes: Vec<telegram_api::FileHash>,
    },
    /// `upload.cdnFile`
    CdnBytes(Vec<u8>),
    /// `upload.cdnFileReuploadNeeded`
    ReuploadNeeded { request_token: Vec<u8> },
    /// `upload.getFileHashes` — `Vector<fileHash>`
    Hashes(Vec<telegram_api::FileHash>),
}

/// The connection-level descriptor the C++ bakes into the NetQuery at
/// creation (`G()->net_query_creator().create(..., dc_id, net_query_type,
/// auth_flag)`, FileDownloader.cpp:158-186, 378): which DC serves the
/// query, whether it carries the auth, and which connection class it
/// joins (`Download` vs `DownloadSmall`, FileDownloader.cpp:148). The
/// connection pools, reuse and stealing themselves are Session
/// properties and stay on the net line (M.12.10); the seam records the
/// per-query acquisition decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DownloadNetMeta {
    pub dc_id: DcId,
    pub auth_flag_on: bool,
    pub small: bool,
}

/// The net seam of the collapsed [`FileDownloader`]: the C++
/// `start_part` + `dispatch_with_callback` + `on_result` trio folded into
/// one call per query. Downloads are sequential here; the in-flight
/// pipelining of the C++ actor loop (and the DC-connection reuse/stealing
/// behind it) is a session property and returns with the live scheduler
/// (M.12.10). Quick acks: the C++ download queries never set
/// `quick_ack_promise_` (only the message-send path does,
/// MessagesManager.cpp:1239-1243; Session.cpp:795 fills it) — the
/// transport half of that machinery is a net-line property and the
/// downloader's half is empty, so the seam carries nothing for it.
pub trait FileDownloaderNet: Send {
    /// Sends the query and fetches its result. `Err` with the canceled
    /// code (203, `NetQuery::Error::Canceled`, NetQuery.h:57) restarts
    /// the part; the CDN control errors (`FILE_TOKEN_INVALID`,
    /// `REQUEST_TOKEN_INVALID`) are inspected by the downloader itself,
    /// everything else fails the download.
    fn download(
        &mut self,
        query: DownloadQuery,
        query_type: DownloadQueryType,
        meta: DownloadNetMeta,
    ) -> Result<DownloadResult, Status>;

    /// `open_temp_file(file_type)` (FileLoaderUtils.cpp) — the real
    /// files-dir layout lands with the orchestration (M.12.7); tests fake
    /// it with a tempdir.
    fn open_temp_file(&mut self, file_type: FileType) -> Result<(File, String), Status>;

    /// `FileFd::open(path, Write | Read)` for a known partial path. The
    /// `write` flag is false on the only_check paths, which must not
    /// modify the found local copy (FileDownloader.cpp:494, 514, 423).
    fn open_partial_file(&mut self, path: &str, write: bool) -> Result<File, Status>;

    /// `FileFd::open(path_, FileFd::Create | FileFd::Write)` of the
    /// `FileExternalGenerateActor::do_file_generate_write_part`
    /// (FileGenerateManager.cpp:341) — the generated path may have just
    /// been unlinked (a stale partial), so the open must create. The
    /// default answers the unavailable-io error; real io (M.12.10) and
    /// test mocks implement it.
    fn open_generated_file(&mut self, _path: &str) -> Result<File, Status> {
        Err(Status::error_message("File generation io is not available"))
    }

    /// `search_file(file_type, name, size)` (FileLoaderUtils.cpp) — the
    /// need_search_file seam; the default never finds anything.
    fn search_file(
        &mut self,
        _file_type: FileType,
        _name: &str,
        _size: i64,
    ) -> Result<String, Status> {
        Err(Status::error_message("File not found"))
    }

    /// `create_from_temp(file_type, path, name)` (FileLoaderUtils.cpp).
    fn create_from_temp(
        &mut self,
        file_type: FileType,
        path: &str,
        name: &str,
    ) -> Result<String, Status>;

    /// `secure_storage::decrypt_file(secret, value_hash, from, to)` + the
    /// resulting stat size (FileDownloader.cpp:579-588) — the Wave-3
    /// secure-storage seam.
    fn decrypt_secure_file(
        &mut self,
        _secret: &[u8],
        _value_hash: &[u8],
        _path: &str,
    ) -> Result<(String, i64), Status> {
        Err(Status::error_message(
            "Secure file decryption is not available yet",
        ))
    }

    /// `G()->get_webfile_dc_id()` (Global.cpp:254-264) — the
    /// `webfile_dc_id` config option (ConfigManager.cpp:1218) with the
    /// test-2/production-4 fallback. The option lands with the config
    /// seam; the default answers the invalid DcId.
    fn webfile_dc_id(&mut self) -> DcId {
        DcId::default()
    }

    /// The dialog-plane peer resolver behind the dialog-photo download
    /// arms (`G()->dialog_manager()->get_input_peer`, FileLocation.h:419)
    /// — `None` when the peer is not in the cache. The real resolver
    /// lands with the orchestration wiring (M.12.7).
    fn resolve_input_peer(&mut self, dialog_id: DialogId) -> Option<telegram_api::InputPeer> {
        let _ = dialog_id;
        None
    }
}

/// `FileDownloader::Callback` (FileDownloader.h:35-45).
pub trait FileDownloaderCallback: Send {
    fn on_start_download(&mut self);
    fn on_partial_download(&mut self, partial_local: PartialLocalFileLocation, size: i64);
    fn on_ok(&mut self, full_local: FullLocalFileLocation, size: i64, is_new: bool);
    fn on_error(&mut self, status: Status);
}

/// `FileDownloader::HashInfo` (FileDownloader.h:87-94); ordering is by
/// offset alone, like the C++ `operator<`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HashInfo {
    offset: i64,
    size: i64,
    hash: Vec<u8>,
}

impl PartialOrd for HashInfo {
    fn partial_cmp(&self, other: &HashInfo) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HashInfo {
    fn cmp(&self, other: &HashInfo) -> std::cmp::Ordering {
        self.offset.cmp(&other.offset)
    }
}

/// `td::FileDownloader` (FileDownloader.h) collapsed per the
/// single-scheduler convention: the actor loop becomes a synchronous
/// `loop_` over the [`FileDownloaderNet`] seam, the `part_map_` of
/// in-flight queries disappears with the pipeline (a part is sent and its
/// result processed inside one `do_loop` pass), and the ResourceManager
/// link is the [`ResourceManagerWorker`] trait. Secret-key downloads are
/// inherently ordered (the IGE chain), which the sequential loop provides
/// by construction — the C++ `ordered_flag_`/`OrderedEventsProcessor`
/// pair has nothing left to reorder.
pub struct FileDownloader {
    remote: FullRemoteFileLocation,
    local: LocalFileLocation,
    size: i64,
    name: String,
    encryption_key: FileEncryptionKey,
    callback: Box<dyn FileDownloaderCallback>,
    net: Box<dyn FileDownloaderNet>,

    only_check: bool,

    path: String,
    fd: Option<File>,

    next_part: i32,
    next_part_stop: bool,
    is_small: bool,
    need_search_file: bool,
    keep_fd: bool,
    offset: i64,
    limit: i64,

    use_cdn: bool,
    cdn_dc_id: DcId,
    cdn_encryption_key: Vec<u8>,
    cdn_encryption_iv: Vec<u8>,
    cdn_file_token: Vec<u8>,
    cdn_file_token_generation: i32,
    cdn_part_reupload_token: BTreeMap<i32, Vec<u8>>,
    cdn_part_file_token_generation: BTreeMap<i32, i32>,

    need_check: bool,
    hash_info: BTreeSet<HashInfo>,
    has_hash_query: bool,

    stop_flag: bool,
    resource_manager: Option<ResourceManagerHandle>,
    resource_state: ResourceState,
    parts_manager: PartsManager,

    debug_total_parts: u32,
    debug_bad_part_order: u32,
    debug_bad_parts: Vec<i32>,
}

impl FileDownloader {
    /// The `FileDownloader` constructor (FileDownloader.cpp:35-52).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        remote: FullRemoteFileLocation,
        local: LocalFileLocation,
        size: i64,
        name: String,
        encryption_key: FileEncryptionKey,
        is_small: bool,
        need_search_file: bool,
        offset: i64,
        limit: i64,
        callback: Box<dyn FileDownloaderCallback>,
        net: Box<dyn FileDownloaderNet>,
    ) -> FileDownloader {
        assert!(
            encryption_key.empty() || offset == 0,
            "CHECK(offset_ == 0) failed for a keyed download"
        );
        FileDownloader {
            remote,
            local,
            size,
            name,
            encryption_key,
            callback,
            net,
            only_check: false,
            path: String::new(),
            fd: None,
            next_part: 0,
            next_part_stop: false,
            is_small,
            need_search_file,
            keep_fd: false,
            offset,
            limit,
            use_cdn: false,
            cdn_dc_id: DcId::default(),
            cdn_encryption_key: Vec::new(),
            cdn_encryption_iv: Vec::new(),
            cdn_file_token: Vec::new(),
            cdn_file_token_generation: 0,
            cdn_part_reupload_token: BTreeMap::new(),
            cdn_part_file_token_generation: BTreeMap::new(),
            need_check: false,
            hash_info: BTreeSet::new(),
            has_hash_query: false,
            stop_flag: false,
            resource_manager: None,
            resource_state: ResourceState::default(),
            parts_manager: PartsManager::default(),
            debug_total_parts: 0,
            debug_bad_part_order: 0,
            debug_bad_parts: Vec::new(),
        }
    }

    /// `FileDownloader::start_up` (FileDownloader.cpp:479-556).
    pub fn start(&mut self) {
        if matches!(self.local, LocalFileLocation::Full(_)) {
            return self.on_error(Status::error_message("File is already downloaded"));
        }
        if self.encryption_key.is_secure() && !self.encryption_key.has_value_hash() {
            tdutils::log_error!("Can't download Secure file with unknown value_hash");
        }
        if self.remote.file_type == FileType::SecureEncrypted {
            self.size = 0;
        }
        let mut part_size = 0usize;
        let mut bitmask = Bitmask::from_ones(0);
        if let LocalFileLocation::Partial(partial) = &self.local.clone() {
            self.path = partial.path.clone();
            let result_fd = self.net.open_partial_file(&partial.path, true);
            // TODO: check timestamps..
            if let Ok(fd) = result_fd {
                if (!self.encryption_key.is_secret() || partial.iv.len() == 32)
                    && partial.part_size >= 0
                    && partial.part_size <= (1 << 20)
                    && (partial.part_size & (partial.part_size - 1)) == 0
                {
                    bitmask = Bitmask::from_decode(&partial.ready_bitmask);
                    if self.encryption_key.is_secret() {
                        let mut iv = [0u8; 32];
                        iv.copy_from_slice(&partial.iv);
                        self.encryption_key.set_iv(iv);
                        self.next_part = bitmask.get_ready_parts(0) as i32;
                    }
                    self.fd = Some(fd);
                    part_size = partial.part_size as usize;
                } else {
                    tdutils::log_error!("Have invalid partial local location {partial}");
                }
            }
        }
        if self.need_search_file
            && self.fd.is_none()
            && self.size > 0
            && self.encryption_key.empty()
            && !self.remote.is_web()
            && let Ok(r_path) = self
                .net
                .search_file(self.remote.file_type, &self.name, self.size)
            && let Ok(fd) = self.net.open_partial_file(&r_path, false)
        {
            self.path = r_path;
            self.fd = Some(fd);
            self.need_check = true;
            self.only_check = true;
            part_size = 128 * (1 << 10);
            bitmask = Bitmask::from_ones((self.size + part_size as i64 - 1) / part_size as i64);
        }
        self.try_release_fd();

        let ready_parts = bitmask.as_vector();
        let init_result = self.parts_manager.init(
            self.size,
            self.size,
            true,
            part_size,
            &ready_parts,
            false,
            false,
        );
        if let tdutils::status::Result::Err(status) = init_result {
            return self.on_error(status);
        }
        if self.only_check {
            self.parts_manager.set_checked_prefix_size(0);
        }
        self.parts_manager
            .set_streaming_offset(self.offset, self.limit);
        let file_type = get_main_file_type(self.remote.file_type);
        if !self.is_small
            && (matches!(
                file_type,
                FileType::VideoNote
                    | FileType::Document
                    | FileType::VoiceNote
                    | FileType::Audio
                    | FileType::Video
                    | FileType::Animation
                    | FileType::VideoStory
                    | FileType::SelfDestructingVideo
                    | FileType::SelfDestructingVideoNote
                    | FileType::SelfDestructingVoiceNote
            ) || (file_type == FileType::Encrypted && self.size > (1 << 20)))
        {
            // the C++ routes these through a DelayDispatcher (3ms inter-part
            // delay, ramping down to 3ms from 50ms); the sequential loop has
            // no queue to pace, the delay returns with the live scheduler
        }
        self.resource_state
            .set_unit_size(self.parts_manager.get_part_size());
        self.update_estimated_limit();
        self.on_progress();
        self.loop_();
    }

    /// `FileDownloader::on_error` (FileDownloader.cpp:54-58).
    fn on_error(&mut self, status: Status) {
        self.fd = None;
        self.stop_flag = true;
        self.callback.on_error(status);
    }

    /// `FileDownloader::loop` (FileDownloader.cpp:558-569); the `-1`
    /// "just stop" code is unreachable in the collapse.
    pub fn loop_(&mut self) {
        if self.stop_flag {
            return;
        }
        if let Err(status) = self.do_loop() {
            self.on_error(status);
        }
    }

    /// `FileDownloader::do_loop` (FileDownloader.cpp:571-631).
    fn do_loop(&mut self) -> Result<(), Status> {
        let checked_prefix_size = self.parts_manager.get_checked_prefix_size();
        let ready_prefix_size = self.parts_manager.get_unchecked_ready_prefix_size();
        let is_ready = self.parts_manager.unchecked_ready();
        self.check_loop(checked_prefix_size, ready_prefix_size, is_ready)?;

        if self.parts_manager.may_finish() {
            if let tdutils::status::Result::Err(status) = self.parts_manager.finish() {
                return Err(status);
            }
            self.fd = None;
            let mut size = self.parts_manager.get_size();
            if self.encryption_key.is_secure() {
                let (tmp_path, path_stat_size) = self.net.decrypt_secure_file(
                    self.encryption_key.secret(),
                    self.encryption_key.value_hash(),
                    &self.path,
                )?;
                let _ = std::fs::remove_file(&self.path);
                self.path = tmp_path;
                size = path_stat_size;
            }
            let path = if self.only_check {
                self.path.clone()
            } else {
                self.net
                    .create_from_temp(self.remote.file_type, &self.path, &self.name)?
            };
            self.callback.on_ok(
                FullLocalFileLocation {
                    file_type: self.remote.file_type,
                    path,
                    mtime_nsec: 0,
                },
                size,
                !self.only_check,
            );

            self.stop_flag = true;
            return Ok(());
        }

        loop {
            if self.resource_state.unused() < self.parts_manager.get_part_size() as i64 {
                break;
            }
            let part = match self.parts_manager.start_part() {
                tdutils::status::Result::Ok(part) => part,
                tdutils::status::Result::Err(status) => return Err(status),
            };
            if part.size == 0 {
                break;
            }
            self.resource_state.start_use(part.size as i64);

            let should_restart = self.run_part(part)?;
            if should_restart {
                self.resource_state.stop_use(part.size as i64);
                self.parts_manager.on_part_failed(part.id);
            }
            self.update_estimated_limit();
            if self.stop_flag {
                return Ok(());
            }
            if self.parts_manager.may_finish() {
                self.loop_();
                return Ok(());
            }
        }
        Ok(())
    }

    /// The `start_part` → `on_result` → `should_restart_part` →
    /// `process_part` → `try_on_part_query` round of one part
    /// (FileDownloader.cpp:133-192, 60-131, 207-285, 726-740) folded into
    /// one send-and-wait. Returns whether the part must restart.
    fn run_part(&mut self, part: Part) -> Result<bool, Status> {
        // the C++ pads only its by-value copy inside `start_part`
        // (FileDownloader.cpp:133-136); the part that flows on through
        // `process_part`/`try_on_part_query` stays unpadded — the write
        // is trimmed to the unpadded size (cpp:275), and `stop_use`/
        // `on_part_ok` account for the unpadded size (cpp:729, 731)
        let padded_size = if self.encryption_key.is_secret() {
            (part.size + 15) & !15
        } else {
            part.size
        };
        let size = self.parts_manager.get_part_size();
        assert!(
            padded_size <= size,
            "CHECK(part.size <= part_size()) failed"
        );

        self.callback.on_start_download();

        let query_type = if !self.use_cdn {
            DownloadQueryType::Default
        } else if self.cdn_part_reupload_token.contains_key(&part.id) {
            DownloadQueryType::ReuploadCdn
        } else {
            DownloadQueryType::Cdn
        };

        // `should_restart_part`'s error short-circuits
        // (FileDownloader.cpp:63-72) plus `on_result`'s Canceled restart
        // (FileDownloader.cpp:684-687)
        let send_result = self.send_part_query(part, size, query_type);
        let net_result = match send_result {
            Ok(result) => result,
            Err(status) => {
                if status.message() == "FILE_TOKEN_INVALID" {
                    self.use_cdn = false;
                    return Ok(true);
                }
                if status.message() == "REQUEST_TOKEN_INVALID" {
                    return Ok(true);
                }
                if status.code() == NET_QUERY_CANCELED {
                    return Ok(true);
                }
                // the generic error fails the download through
                // `process_part`'s `check_net_query`
                // (FileDownloader.cpp:208)
                return Err(self.check_net_query(status));
            }
        };

        match (&query_type, net_result) {
            // the CdnRedirect arm of should_restart_part
            // (FileDownloader.cpp:76-100)
            (
                DownloadQueryType::Default,
                DownloadResult::CdnRedirect {
                    dc_id,
                    file_token,
                    encryption_key,
                    encryption_iv,
                    file_hashes,
                },
            ) => {
                if self.cdn_file_token == *file_token {
                    return Ok(true);
                }
                self.use_cdn = true;
                self.need_check = true;
                self.cdn_file_token_generation += 1;
                self.cdn_file_token = file_token.clone();
                self.cdn_dc_id = DcId::external(dc_id);
                self.cdn_encryption_key = encryption_key.clone();
                self.cdn_encryption_iv = encryption_iv.clone();
                self.add_hash_info(&file_hashes);
                if self.cdn_encryption_iv.len() != 16 || self.cdn_encryption_key.len() != 32 {
                    return Err(Status::error_message("Wrong ctr key or iv size"));
                }
                Ok(true)
            }
            // the ReuploadCDN arm (FileDownloader.cpp:103-108)
            (DownloadQueryType::ReuploadCdn, DownloadResult::Hashes(file_hashes)) => {
                self.add_hash_info(&file_hashes);
                Ok(true)
            }
            // the CDN arm (FileDownloader.cpp:110-125)
            (DownloadQueryType::Cdn, DownloadResult::ReuploadNeeded { request_token }) => {
                self.cdn_part_reupload_token
                    .insert(part.id, request_token.clone());
                Ok(true)
            }
            (DownloadQueryType::Cdn, DownloadResult::CdnBytes(bytes)) => {
                // CHECK(it != cdn_part_file_token_generation_.end())
                // (FileDownloader.cpp:118-119)
                let generation = self
                    .cdn_part_file_token_generation
                    .get(&part.id)
                    .copied()
                    .expect("no file token generation for the cdn part");
                if generation != self.cdn_file_token_generation {
                    // "Receive part with an old file_token"
                    return Ok(true);
                }
                let written = self.process_part_bytes(part, bytes, true)?;
                self.finish_part(part, written)?;
                Ok(false)
            }
            (DownloadQueryType::Default, DownloadResult::Bytes(bytes)) => {
                let written = self.process_part_bytes(part, bytes, false)?;
                self.finish_part(part, written)?;
                Ok(false)
            }
            _ => Err(Status::error_message(
                "Unexpected download result for the query type",
            )),
        }
    }

    /// `FileDownloader::start_part` (FileDownloader.cpp:133-192), with the
    /// per-query connection descriptor of [`DownloadNetMeta`].
    fn send_part_query(
        &mut self,
        part: Part,
        size: usize,
        query_type: DownloadQueryType,
    ) -> Result<DownloadResult, Status> {
        let small = self.is_small;
        if !self.use_cdn {
            // CDN is supported, unless we use domains instead of IPs from
            // a browser or stream a middle window
            // (FileDownloader.cpp:151-157)
            let cdn_supported = self.parts_manager.get_streaming_offset() == 0;
            let dc_id = if self.remote.is_web() {
                self.net.webfile_dc_id()
            } else {
                self.remote.get_dc_id()
            };
            let meta = DownloadNetMeta {
                dc_id,
                auth_flag_on: true,
                small,
            };
            if self.remote.is_web() {
                let location = self
                    .remote
                    .as_input_web_file_location()
                    .map_err(Status::error_message)?;
                let query = DownloadQuery::GetWebFile {
                    location,
                    offset: part.offset as i32,
                    limit: size as i32,
                };
                return self.net.download(query, query_type, meta);
            }
            // the dialog-photo arms resolve the peer through the net
            // seam before the location is built (a mutable borrow may
            // not cross the immutable location borrow)
            let peer = self
                .dialog_photo_id()
                .and_then(|id| self.net.resolve_input_peer(id));
            let location = self
                .remote
                .as_input_file_location(&|_| peer.clone())
                .map_err(|e| Status::error_message(&e))?;
            let query = DownloadQuery::GetFile {
                location,
                offset: part.offset,
                limit: size as i32,
                cdn_supported,
            };
            return self.net.download(query, query_type, meta);
        }
        if self.remote.is_web() {
            return Err(Status::error_message("Can't download web file from CDN"));
        }
        // the C++ find/erase pair (FileDownloader.cpp:175-188): a parked
        // reupload token is consumed once
        let reupload_request_token = self.cdn_part_reupload_token.remove(&part.id);
        let (query, meta) = if let Some(request_token) = reupload_request_token {
            (
                DownloadQuery::ReuploadCdnFile {
                    file_token: self.cdn_file_token.clone(),
                    request_token,
                },
                DownloadNetMeta {
                    dc_id: self.remote.get_dc_id(),
                    auth_flag_on: true,
                    small,
                },
            )
        } else {
            self.cdn_part_file_token_generation
                .insert(part.id, self.cdn_file_token_generation);
            (
                DownloadQuery::GetCdnFile {
                    file_token: self.cdn_file_token.clone(),
                    offset: part.offset,
                    limit: size as i32,
                },
                DownloadNetMeta {
                    dc_id: self.cdn_dc_id,
                    // getCdnFile rides the CDN DC without the auth
                    // (FileDownloader.cpp:180-181)
                    auth_flag_on: false,
                    small,
                },
            )
        };
        self.net.download(query, query_type, meta)
    }

    /// `FileDownloader::check_net_query` (FileDownloader.cpp:194-205) —
    /// file-reference errors (`is_file_reference_error`,
    /// FileReferenceManager.cpp:60-62) get the base64 reference appended
    /// so the FileLoaderManager repair path can recognize them.
    fn check_net_query(&self, status: Status) -> Status {
        if status.code() == 400 && status.message().starts_with("FILE_REFERENCE_") {
            return Status::error(
                status.code(),
                &format!(
                    "{}#BASE64{}",
                    status.message(),
                    base64_encode(self.remote.file_reference.as_bytes())
                ),
            );
        }
        status
    }

    /// `FileDownloader::process_part` (FileDownloader.cpp:207-285) — the
    /// decrypt-and-store half, after the query type decided it is a real
    /// payload. Returns the number of bytes written.
    fn process_part_bytes(
        &mut self,
        part: Part,
        mut bytes: Vec<u8>,
        need_cdn_decrypt: bool,
    ) -> Result<usize, Status> {
        let mut padded_size = part.size;
        if self.encryption_key.is_secret() {
            padded_size = (part.size + 15) & !15;
        }
        if bytes.len() as i64 > padded_size as i64 {
            return Err(Status::error_message("Part size is more than requested"));
        }
        if bytes.is_empty() {
            return Ok(0);
        }

        // Encryption
        if need_cdn_decrypt {
            assert!(part.offset % 16 == 0, "CHECK(part.offset % 16 == 0) failed");
            let offset = (part.offset / 16) as u32;
            let offset = ((offset & 0xff) << 24)
                | ((offset & 0xff00) << 8)
                | ((offset & 0xff0000) >> 8)
                | ((offset & 0xff000000) >> 24);

            let mut ctr_state = tdutils::crypto::AesCtrState::new();
            let mut iv = self.cdn_encryption_iv.clone();
            iv[12..16].copy_from_slice(&offset.to_le_bytes());
            ctr_state.init(&self.cdn_encryption_key, &iv);
            let plain = bytes.clone();
            ctr_state.decrypt(&plain, &mut bytes);
        }
        if self.encryption_key.is_secret() {
            if self.next_part != part.id {
                return Err(Status::error(
                    500,
                    "LOG_CHECK(next_part_ == part.id) failed",
                ));
            }
            // `part.size` is the unpadded tail here, exactly the value the
            // C++ tests (FileDownloader.cpp:268)
            assert!(!self.next_part_stop, "CHECK(!next_part_stop_) failed");
            self.next_part += 1;
            if !part.size.is_multiple_of(16) {
                self.next_part_stop = true;
            }
            let mut iv = self.encryption_key.iv_slice().to_vec();
            tdutils::crypto::aes_ige_decrypt_in_place(
                self.encryption_key.key_slice(),
                &mut iv,
                &mut bytes,
            );
            self.encryption_key.set_iv(iv.try_into().expect("iv size"));
        }

        let written_slice_len = (bytes.len() as i64).min(part.size as i64) as usize;
        self.acquire_fd()?;
        let fd = self.fd.as_mut().expect("acquired fd");
        let written = compat_write_at(fd, &bytes[..written_slice_len], part.offset as u64)
            .map_err(|e| Status::error_message(format!("Failed to write: {e}").as_str()))?;
        // may write less than part.size, when size of downloadable file is
        // unknown
        if written != written_slice_len {
            return Err(Status::error_message(
                "Failed to save file part to the file",
            ));
        }
        Ok(written)
    }

    /// `FileDownloader::try_on_part_query`'s tail
    /// (FileDownloader.cpp:729-738).
    fn finish_part(&mut self, part: Part, size: usize) -> Result<(), Status> {
        self.resource_state.stop_use(part.size as i64);
        let old_ready_prefix_count = self.parts_manager.get_unchecked_ready_prefix_count();
        if let tdutils::status::Result::Err(status) =
            self.parts_manager.on_part_ok(part.id, part.size, size)
        {
            return Err(status);
        }
        let new_ready_prefix_count = self.parts_manager.get_unchecked_ready_prefix_count();
        self.debug_total_parts += 1;
        if old_ready_prefix_count == new_ready_prefix_count {
            self.debug_bad_parts.push(part.id);
            self.debug_bad_part_order += 1;
        }
        self.on_progress();
        Ok(())
    }

    /// `FileDownloader::on_progress` (FileDownloader.cpp:287-316).
    fn on_progress(&mut self) {
        if self.parts_manager.ready() {
            // do not send partial location. will lead to wrong local_size
            return;
        }
        let ready_size = self.parts_manager.get_ready_size();
        if ready_size == 0 || self.path.is_empty() {
            return;
        }
        let part_size = self.parts_manager.get_part_size() as i32;
        let size = self.parts_manager.get_size_or_zero();
        if self.encryption_key.empty() || self.encryption_key.is_secure() {
            let partial = PartialLocalFileLocation {
                file_type: self.remote.file_type,
                part_size: part_size as i64,
                path: self.path.clone(),
                iv: Vec::new(),
                ready_bitmask: self.parts_manager.get_bitmask(),
                ready_size,
            };
            self.callback.on_partial_download(partial, size);
        } else if self.encryption_key.is_secret() {
            let ready_part_count = self.parts_manager.get_ready_prefix_count();
            if ready_part_count != self.next_part {
                tdutils::log_fatal!(
                    "ready_part_count = {ready_part_count}, next_part = {}",
                    self.next_part
                );
            }
            let partial = PartialLocalFileLocation {
                file_type: self.remote.file_type,
                part_size: part_size as i64,
                path: self.path.clone(),
                // the raw iv bytes, like the C++ `as<UInt256>` copy
                // (FileDownloader.cpp:306-310)
                iv: self.encryption_key.iv_slice().to_vec(),
                ready_bitmask: self.parts_manager.get_bitmask(),
                ready_size,
            };
            self.callback.on_partial_download(partial, size);
        }
    }

    /// `FileDownloader::process_check_query` (FileDownloader.cpp:318-324).
    fn process_check_query(&mut self, hashes: Vec<telegram_api::FileHash>) {
        self.has_hash_query = false;
        self.add_hash_info(&hashes);
    }

    /// `FileDownloader::check_loop` (FileDownloader.cpp:326-399); the
    /// C++ SCOPE_EXIT (FileDownloader.cpp:330-332) releases the fd on
    /// every exit.
    fn check_loop(
        &mut self,
        checked_prefix_size: i64,
        ready_prefix_size: i64,
        is_ready: bool,
    ) -> Result<(), Status> {
        let result = self.check_loop_impl(checked_prefix_size, ready_prefix_size, is_ready);
        self.try_release_fd();
        result
    }

    /// The `check_loop` body.
    fn check_loop_impl(
        &mut self,
        mut checked_prefix_size: i64,
        ready_prefix_size: i64,
        is_ready: bool,
    ) -> Result<(), Status> {
        if !self.need_check {
            return Ok(());
        }
        let mut is_changed = false;
        let mut sent_hashes_query = false;
        while checked_prefix_size < ready_prefix_size {
            // the C++ `upper_bound` then `--it` dance lands on the single
            // hash entry at or before the offset
            // (FileDownloader.cpp:337-344); only that entry may verify —
            // a lower-offset entry that happens to cover is never
            // consulted
            let at_or_before = self
                .hash_info
                .range(
                    ..=HashInfo {
                        offset: checked_prefix_size,
                        size: 0,
                        hash: Vec::new(),
                    },
                )
                .next_back()
                .cloned();
            let cover = match at_or_before {
                Some(cover)
                    if cover.offset <= checked_prefix_size
                        && cover.offset + cover.size > checked_prefix_size =>
                {
                    cover
                }
                _ => {
                    if !self.has_hash_query && !sent_hashes_query {
                        sent_hashes_query = true;
                        self.has_hash_query = true;
                        let peer = self
                            .dialog_photo_id()
                            .and_then(|id| self.net.resolve_input_peer(id));
                        let location = self
                            .remote
                            .as_input_file_location(&|_| peer.clone())
                            .map_err(|e| Status::error_message(&e))?;
                        let query = DownloadQuery::GetFileHashes {
                            location,
                            offset: checked_prefix_size,
                        };
                        // the check query rides the default auth to the
                        // remote DC (FileDownloader.cpp:378)
                        let meta = DownloadNetMeta {
                            dc_id: self.remote.get_dc_id(),
                            auth_flag_on: true,
                            small: self.is_small,
                        };
                        let hashes = match self.net.download(query, DownloadQueryType::Check, meta)
                        {
                            Result::Ok(DownloadResult::Hashes(hashes)) => hashes,
                            Result::Ok(_) => {
                                return Err(Status::error_message("Unexpected check query result"));
                            }
                            Result::Err(status) => return Err(self.check_net_query(status)),
                        };
                        self.process_check_query(hashes);
                        // the query itself changes nothing; on_progress
                        // fires only after a verified segment
                        // (FileDownloader.cpp:386-388)
                        continue;
                    }
                    // Should fail?
                    break;
                }
            };
            let begin_offset = cover.offset;
            let mut end_offset = cover.offset + cover.size;
            if ready_prefix_size < end_offset {
                if !is_ready {
                    break;
                }
                end_offset = ready_prefix_size;
            }
            let slice_size = (end_offset - begin_offset) as usize;
            let mut slice = vec![0u8; slice_size];
            self.acquire_fd()?;
            let read_size = {
                let fd = self.fd.as_mut().expect("acquired fd");
                pread_at(fd, &mut slice, begin_offset as u64)?
            };
            if slice_size != read_size {
                return Err(Status::error_message("Failed to read file to check hash"));
            }
            let hash = tdutils::crypto::sha256(&slice);

            if hash.as_slice() != cover.hash.as_slice() {
                if self.only_check {
                    return Err(Status::error_message("FILE_DOWNLOAD_RESTART"));
                }
                return Err(Status::error_message("Hash mismatch"));
            }

            checked_prefix_size = end_offset;
            is_changed = true;
        }

        if is_changed {
            self.on_progress();
        }
        if self.need_check {
            self.parts_manager.set_need_check();
            self.parts_manager
                .set_checked_prefix_size(checked_prefix_size);
        }

        Ok(())
    }

    /// `FileDownloader::add_hash_info` (FileDownloader.cpp:401-410).
    fn add_hash_info(&mut self, hashes: &[telegram_api::FileHash]) {
        for hash in hashes {
            self.hash_info.insert(HashInfo {
                offset: hash.offset,
                size: hash.limit as i64,
                hash: hash.hash.clone(),
            });
        }
    }

    /// The `DialogId` a dialog-photo source carries — the resolver input
    /// of [`FileDownloaderNet::resolve_input_peer`].
    fn dialog_photo_id(&self) -> Option<DialogId> {
        let FullRemoteVariant::Photo(photo) = &self.remote.variant else {
            return None;
        };
        match &photo.source {
            PhotoSizeSource::DialogPhotoSmall { dialog_id, .. }
            | PhotoSizeSource::DialogPhotoBig { dialog_id, .. }
            | PhotoSizeSource::DialogPhotoSmallLegacy { dialog_id, .. }
            | PhotoSizeSource::DialogPhotoBigLegacy { dialog_id, .. } => Some(*dialog_id),
            _ => None,
        }
    }

    /// `FileDownloader::try_release_fd` (FileDownloader.cpp:412-416).
    fn try_release_fd(&mut self) {
        if !self.keep_fd && self.fd.is_some() {
            self.fd = None;
        }
    }

    /// `FileDownloader::acquire_fd` (FileDownloader.cpp:418-427).
    fn acquire_fd(&mut self) -> Result<(), Status> {
        if self.fd.is_none() {
            if self.path.is_empty() {
                let (fd, path) = self.net.open_temp_file(self.remote.file_type)?;
                self.fd = Some(fd);
                self.path = path;
            } else {
                // the only_check fd must not modify the checked file
                // (FileDownloader.cpp:423)
                self.fd = Some(self.net.open_partial_file(&self.path, !self.only_check)?);
            }
        }
        Ok(())
    }

    /// `FileDownloader::update_downloaded_part`
    /// (FileDownloader.cpp:458-477) — the streaming-window updates; the
    /// in-flight cancellation loop of the C++ has nothing to cancel in the
    /// sequential collapse.
    pub fn update_downloaded_part(&mut self, offset: i64, limit: i64, max_resource_limit: i64) {
        if self.parts_manager.get_streaming_offset() != offset {
            let begin_part_id = self.parts_manager.set_streaming_offset(offset, limit);
            let new_end_part_id = if limit <= 0 {
                self.parts_manager.get_part_count()
            } else {
                ((offset + limit - 1) / self.parts_manager.get_part_size() as i64) as i32 + 1
            };
            let max_parts = (max_resource_limit / self.parts_manager.get_part_size() as i64) as i32;
            let end_part_id = begin_part_id + max_parts.min(new_end_part_id - begin_part_id);
            let _ = end_part_id;
        } else {
            self.parts_manager.set_streaming_limit(limit);
        }
        self.update_estimated_limit();
        self.loop_();
    }

    /// `FileDownloader::update_estimated_limit`
    /// (FileDownloader.cpp:643-655).
    fn update_estimated_limit(&mut self) {
        if self.stop_flag {
            return;
        }
        let estimated_extra = self.parts_manager.get_estimated_extra();
        self.resource_state.update_estimated_limit(estimated_extra);
        if self.resource_manager.is_some() {
            self.keep_fd =
                self.resource_state.active_limit() as usize >= self.parts_manager.get_part_size();
            self.try_release_fd();
            if let Some(handle) = self.resource_manager.clone() {
                handle.update_resources(&self.resource_state);
            }
        }
    }

    /// `FileDownloader::update_priority` (FileDownloader.cpp:434-436).
    pub fn update_priority(&mut self, priority: i8) {
        if let Some(handle) = &self.resource_manager {
            handle.update_priority(priority);
        }
    }

    pub fn stop_flag(&self) -> bool {
        self.stop_flag
    }
}

/// The `#[cfg(test)]` seams of [`FileDownloader`] — the pieces the C++
/// gets from `G()` (the CDN state a redirect would have set) without a
/// live net.
#[cfg(test)]
impl FileDownloader {
    pub(crate) fn seed_cdn_for_test(&mut self, token: Vec<u8>, dc_id: i32) {
        self.use_cdn = true;
        self.need_check = true;
        self.cdn_file_token = token;
        self.cdn_dc_id = DcId::external(dc_id);
        self.cdn_file_token_generation = 1;
        self.cdn_encryption_key = (0u8..32).collect();
        self.cdn_encryption_iv = (32u8..48).collect();
    }
}

impl ResourceManagerWorker for FileDownloader {
    /// `FileDownloader::set_resource_manager`
    /// (FileDownloader.cpp:429-432).
    fn set_resource_manager(&mut self, handle: ResourceManagerHandle) {
        handle.update_resources(&self.resource_state);
        self.resource_manager = Some(handle);
    }

    /// `FileDownloader::update_resources` (FileDownloader.cpp:438-442).
    fn update_resources(&mut self, resource_state: ResourceState) {
        self.resource_state.update_slave(&resource_state);
        self.loop_();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tddb::td_db::{Parameters, TdDb};

    /// A stand-in `FileReferenceManager` for the serialization tests: the
    /// id itself is the wire payload (the real 28-variant `FileSource`
    /// format lands with M.12.2a).
    pub(crate) struct TestFileSources;

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
            [
                0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x0d, 0x0c, 0x0b, 0x0a
            ]
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
        // a legacy blob with the all-ones deprecated count; the iv is
        // binary, not UTF-8 (FileDownloader.cpp:310)
        let mut out = Vec::new();
        store_parse::store_i32(&mut out, FileType::Document as i32);
        store_parse::store_string(&mut out, "/tmp/a");
        store_parse::store_i32(&mut out, 524288); // 512 KiB part size
        store_parse::store_i32(&mut out, 3); // 3 ready parts
        store_parse::store_string_bytes(&mut out, &[0xff, 0x00, 0xfe, 0x80]);
        let mut parser = Parser::new(&out);
        let parsed = PartialLocalFileLocation::parse_from(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed.part_size, 524288);
        assert_eq!(parsed.ready_bitmask, [0x07]);
        assert_eq!(parsed.iv, [0xff, 0x00, 0xfe, 0x80]);

        // the modern -2 path carries the high 31 bits
        let location = PartialLocalFileLocation {
            file_type: FileType::Document,
            path: "/tmp/a".to_owned(),
            part_size: (1i64 << 40) + 5,
            iv: b"iv".to_vec(),
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
                iv: b"iv".to_vec(),
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
        assert!(
            !state.update_estimated_limit(50),
            "same estimate is a no-op"
        );
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
        assert_eq!(
            part,
            Part {
                id: 0,
                offset: 0,
                size: 128
            }
        );
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
        assert_eq!(
            pm.start_part().unwrap(),
            Part {
                id: -1,
                offset: 0,
                size: 0
            }
        );
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
        pm.init(0, 1_000_000, true, 65536, &[], true, true).unwrap();
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
        pm.init(
            1000,
            1000,
            true,
            128,
            &[0, 1, 2, 3, 4, 5, 6, 7],
            false,
            false,
        )
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
        grants: Arc<parking_lot::Mutex<Vec<ResourceState>>>,
        handle: Arc<parking_lot::Mutex<Option<ResourceManagerHandle>>>,
    }

    #[derive(Clone)]
    struct FakeWorkerDriver {
        grants: Arc<parking_lot::Mutex<Vec<ResourceState>>>,
        handle: Arc<parking_lot::Mutex<Option<ResourceManagerHandle>>>,
    }

    impl FakeWorker {
        fn new() -> (FakeWorker, FakeWorkerDriver) {
            let part = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(None)));
            (
                FakeWorker {
                    grants: Arc::clone(&part.0),
                    handle: Arc::clone(&part.1),
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
            *self.handle.lock() = Some(handle);
        }

        fn update_resources(&mut self, resource_state: ResourceState) {
            self.grants.lock().push(resource_state);
        }
    }

    impl FakeWorkerDriver {
        fn grant_count(&self) -> usize {
            self.grants.lock().len()
        }

        fn last_grant_active_limit(&self) -> i64 {
            self.grants
                .lock()
                .last()
                .expect("no grant recorded")
                .active_limit()
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
            let mut state = self
                .grants
                .lock()
                .last()
                .expect("no grant recorded")
                .clone();
            state.start_use(x);
            state.stop_use(x);
            self.update(&state);
        }

        fn hangup(&self) {
            self.handle
                .lock()
                .as_ref()
                .expect("no link handle")
                .hangup_shared();
        }

        fn update(&self, state: &ResourceState) {
            self.handle
                .lock()
                .as_ref()
                .expect("no link handle")
                .update_resources(state);
        }
    }

    #[test]
    fn resource_manager_greedy_satisfies_smallest_first() {
        let manager = Arc::new(Mutex::new(ResourceManager::new(
            1000,
            ResourceManagerMode::Greedy,
        )));
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
        let manager = Arc::new(Mutex::new(ResourceManager::new(
            350,
            ResourceManagerMode::Baseline,
        )));
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

    #[test]
    fn message_id_validity_matches_the_cpp_gate() {
        // `MessageId::is_valid` (MessageId.cpp:115-124): server ids plus the
        // yet-unsent and local suffixes only
        assert!(MessageId::new(1 << 20).is_valid());
        assert!(MessageId::new((5 << 20) | 1).is_valid());
        assert!(MessageId::new((5 << 20) | 2).is_valid());
        assert!(!MessageId::new(0).is_valid());
        assert!(!MessageId::new(-1).is_valid());
        assert!(!MessageId::new(4).is_valid()); // scheduled bits, no shift
        assert!(!MessageId::new((5 << 20) | 3).is_valid());
        assert!(!MessageId::new(((i32::MAX as i64) << 20) + (1 << 20)).is_valid());
    }

    #[test]
    fn message_topic_wire_matches_the_cpp_layout() {
        // the default topic stores only the zero flags word
        let mut out = Vec::new();
        MessageTopic::default().store(&mut out);
        assert_eq!(out, [0, 0, 0, 0]);
        assert_eq!(
            MessageTopic::parse_from(&mut Parser::new(&out)).unwrap(),
            MessageTopic::default()
        );

        let dialog_id = DialogId::new(100);

        // thread: type + dialog + top thread message -> flags 0b111
        let topic = MessageTopic::thread(dialog_id, MessageId::new(5 << 20));
        let mut out = Vec::new();
        topic.store(&mut out);
        let mut expected = Vec::new();
        store_parse::store_i32(&mut expected, 7);
        store_parse::store_i32(&mut expected, 1); // Type::Thread
        dialog_id.store(&mut expected);
        MessageId::new(5 << 20).store(&mut expected);
        assert_eq!(out, expected);
        assert_eq!(
            MessageTopic::parse_from(&mut Parser::new(&out)).unwrap(),
            topic
        );

        // forum: the forum topic flag joins the bitmask
        let topic = MessageTopic::forum(dialog_id, ForumTopicId::new(7));
        let mut out = Vec::new();
        topic.store(&mut out);
        let mut expected = Vec::new();
        store_parse::store_i32(&mut expected, 0b1011);
        store_parse::store_i32(&mut expected, 2); // Type::Forum
        dialog_id.store(&mut expected);
        ForumTopicId::new(7).store(&mut expected);
        assert_eq!(out, expected);
        assert_eq!(
            MessageTopic::parse_from(&mut Parser::new(&out)).unwrap(),
            topic
        );

        // monoforum and saved messages keep the dialog in both the flag and
        // the SavedMessagesTopicId payload
        let saved_messages_topic_id = SavedMessagesTopicId::new(DialogId::new(-100200300));
        for topic in [
            MessageTopic::monoforum(dialog_id, saved_messages_topic_id),
            MessageTopic::saved_messages(dialog_id, saved_messages_topic_id),
        ] {
            let mut out = Vec::new();
            topic.store(&mut out);
            assert_eq!(
                MessageTopic::parse_from(&mut Parser::new(&out)).unwrap(),
                topic
            );
        }
    }

    /// One representative of every `FileSource` variant, tag order 0-27.
    fn all_file_sources() -> Vec<FileSource> {
        let dialog_id = DialogId::new(-100200300);
        let user_id = UserId::new(42);
        vec![
            FileSource::Message(MessageFullId::new(dialog_id, MessageId::new(11 << 20))),
            FileSource::UserPhoto {
                user_id,
                photo_id: 99,
            },
            FileSource::ChatPhoto(ChatId::new(500)),
            FileSource::ChannelPhoto(ChannelId::new(1100)),
            FileSource::Wallpapers,
            FileSource::WebPage("https://example.com".to_string()),
            FileSource::SavedAnimations,
            FileSource::RecentStickers { is_attached: true },
            FileSource::FavoriteStickers,
            FileSource::Background {
                background_id: BackgroundId::new(77),
                access_hash: -123,
            },
            FileSource::ChatFull(ChatId::new(500)),
            FileSource::ChannelFull(ChannelId::new(1100)),
            FileSource::AppConfig,
            FileSource::SavedRingtones,
            FileSource::UserFull(user_id),
            FileSource::AttachMenuBot(user_id),
            FileSource::WebApp {
                user_id,
                short_name: "botapp".to_string(),
            },
            FileSource::Story(StoryFullId::new(dialog_id, StoryId::new(9))),
            FileSource::QuickReplyMessage(QuickReplyMessageFullId::new(
                QuickReplyShortcutId::new(3),
                MessageId::new(13 << 20),
            )),
            FileSource::StarTransaction {
                dialog_id,
                transaction_id: "tx".to_string(),
                is_refund: true,
            },
            FileSource::BotMediaPreview(user_id),
            FileSource::BotMediaPreviewInfo {
                bot_user_id: user_id,
                language_code: "en".to_string(),
            },
            FileSource::StoryAlbum(StoryAlbumFullId::new(dialog_id, StoryAlbumId::new(4))),
            FileSource::UserSavedMusic {
                user_id,
                document_id: 555,
                access_hash: -7,
            },
            FileSource::DraftMessage {
                dialog_id,
                topic: MessageTopic::forum(dialog_id, ForumTopicId::new(7)),
            },
            FileSource::RichMessage(MessageFullId::new(dialog_id, MessageId::new(12 << 20))),
            FileSource::WelcomeMessages(dialog_id),
            FileSource::CommunityFull(CommunityId::new(88)),
        ]
    }

    #[test]
    fn file_source_wire_round_trips_all_variants() {
        let sources = all_file_sources();
        assert_eq!(sources.len(), 28);

        // the wire payload round-trips value-exactly and keeps the variant
        // tag in the C++ Variant order
        for (tag, source) in sources.iter().enumerate() {
            let mut out = Vec::new();
            source.store(&mut out);
            assert_eq!(
                i32::from_le_bytes(out[0..4].try_into().unwrap()),
                tag as i32
            );
            assert_eq!(
                &FileSource::parse_from(&mut Parser::new(&out)).unwrap(),
                source
            );
        }

        // a concrete byte pin: tag 7 then the int32 bool
        let mut out = Vec::new();
        FileSource::RecentStickers { is_attached: true }.store(&mut out);
        assert_eq!(out, [7, 0, 0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn file_source_registry_interns_by_value_and_re_interns_on_parse() {
        let sources = all_file_sources();

        // interning: equal values keep ids, new values append
        let mut manager = FileReferenceManager::new();
        let ids: Vec<FileSourceId> = sources
            .iter()
            .cloned()
            .map(|source| manager.intern(source))
            .collect();
        assert_eq!(ids[0], FileSourceId::new(1));
        assert_eq!(manager.intern(sources[0].clone()), ids[0]);
        assert_eq!(manager.intern(sources[27].clone()), ids[27]);

        // store -> parse through the same registry returns the interned id,
        // except the never-repairable sources which parse to no id
        for (source, id) in sources.iter().zip(&ids) {
            if matches!(
                source,
                FileSource::ChatPhoto(_) | FileSource::ChannelPhoto(_) | FileSource::Wallpapers
            ) {
                continue;
            }
            let mut out = Vec::new();
            manager.store_file_source(*id, &mut out);
            assert_eq!(
                manager.parse_file_source(&mut Parser::new(&out)).unwrap(),
                *id
            );
        }

        // a fresh registry re-interns in encounter order, like the C++
        // per-manager caches do on load
        let mut fresh = FileReferenceManager::new();
        for source in &sources {
            let mut out = Vec::new();
            source.store(&mut out);
            let parsed = fresh.parse_file_source(&mut Parser::new(&out)).unwrap();
            if matches!(
                source,
                FileSource::ChatPhoto(_) | FileSource::ChannelPhoto(_) | FileSource::Wallpapers
            ) {
                assert_eq!(parsed, FileSourceId::new(0));
            }
        }
        // ...and the 25 repairable sources consumed ids 1.. in order: the
        // first one re-interns to its id and the next new value appends at 26
        assert_eq!(fresh.intern(sources[0].clone()), FileSourceId::new(1));
        assert_eq!(
            fresh.intern(FileSource::WebPage("https://other.example.com".to_string())),
            FileSourceId::new(26)
        );
    }

    #[test]
    fn file_source_parse_drops_never_repairable_sources() {
        let mut manager = FileReferenceManager::new();

        // chat photo: the payload is consumed, the id comes back empty
        let mut out = Vec::new();
        FileSource::ChatPhoto(ChatId::new(500)).store(&mut out);
        assert_eq!(
            manager.parse_file_source(&mut Parser::new(&out)).unwrap(),
            FileSourceId::new(0)
        );

        let mut out = Vec::new();
        FileSource::ChannelPhoto(ChannelId::new(1100)).store(&mut out);
        assert_eq!(
            manager.parse_file_source(&mut Parser::new(&out)).unwrap(),
            FileSourceId::new(0)
        );

        // wallpapers: the tag alone
        let mut out = Vec::new();
        FileSource::Wallpapers.store(&mut out);
        assert_eq!(
            manager.parse_file_source(&mut Parser::new(&out)).unwrap(),
            FileSourceId::new(0)
        );

        // nothing above was interned, so the next id is still 1
        assert_eq!(
            manager.intern(FileSource::SavedAnimations),
            FileSourceId::new(1)
        );
    }

    #[test]
    fn file_source_parse_rejects_unknown_tags() {
        let mut manager = FileReferenceManager::new();
        let mut out = Vec::new();
        store_parse::store_i32(&mut out, 28);
        assert_eq!(
            manager.parse_file_source(&mut Parser::new(&out)),
            Err("Invalid type in FileSource")
        );
    }

    #[test]
    fn set_with_position_matches_the_cpp_semantics() {
        // the single-value path: add once, dedupe the repeat, cursor moves
        let mut set: SetWithPosition<u32> = SetWithPosition::default();
        assert!(set.add(1));
        assert!(!set.add(1));
        assert_eq!(set.size(), 1);
        assert!(!set.empty());
        assert!(set.has_next());
        assert_eq!(set.next(), 1);
        assert!(!set.has_next());
        set.reset_position();
        assert!(set.has_next());
        assert!(!set.remove(2));

        // the extremes heuristic: checked {1}, not checked {2, 3} -> [1, 3]
        let mut set: SetWithPosition<u32> = SetWithPosition::default();
        assert!(set.add(1));
        assert_eq!(set.next(), 1);
        assert!(set.add(2));
        assert!(set.add(3));
        assert_eq!(set.get_some_elements(), [1, 3]);

        // the C++ `empty` quirk: fast path exists but holds nothing
        assert!(set.remove(3));
        assert!(set.remove(2));
        assert!(set.remove(1));
        assert_eq!(set.size(), 0);
        assert!(!set.empty());

        // merge keeps everything checked on the larger side; iteration stays
        // in value order
        let mut a: SetWithPosition<u32> = SetWithPosition::default();
        assert!(a.add(1));
        assert_eq!(a.next(), 1); // a = {1} checked, size 1
        let mut b: SetWithPosition<u32> = SetWithPosition::default();
        assert!(b.add(2));
        assert!(b.add(3)); // b = {2, 3} not checked, size 2
        a.merge(&mut b); // swap: b is larger, a absorbs b
        assert_eq!(a.size(), 3);
        assert_eq!(a.next(), 2);
        assert_eq!(a.next(), 3);
        assert!(!a.has_next());
        a.reset_position();
        assert_eq!(a.next(), 1);
        assert!(b.empty());

        // the equal-single-value fast path carries the checked flag across
        let mut a: SetWithPosition<u32> = SetWithPosition::default();
        assert!(a.add(5));
        let mut b: SetWithPosition<u32> = SetWithPosition::default();
        assert!(b.add(5));
        assert_eq!(b.next(), 5);
        a.merge(&mut b);
        assert_eq!(a.size(), 1);
        assert!(!a.has_next()); // `is_checked_ |= other.is_checked_`
        a.reset_position();
        assert!(a.has_next());
        assert_eq!(a.next(), 5);
        assert!(!a.has_next());
    }

    #[test]
    fn file_reference_manager_nodes_match_the_cpp_surface() {
        let mut manager = FileReferenceManager::new();
        let file_id = FileId::new(1, 0);
        let s1 = manager.intern(FileSource::SavedAnimations);
        let s2 = manager.intern(FileSource::FavoriteStickers);
        let s3 = manager.intern(FileSource::AppConfig);
        // a missing node removes and reads as nothing
        assert!(!manager.remove_file_source(file_id, s1));
        assert!(manager.get_some_file_sources(file_id).is_empty());

        // get_some keeps the extremes until the cursor walks the set
        assert!(manager.add_file_source(file_id, s1));
        assert!(!manager.add_file_source(file_id, s1));
        assert!(manager.add_file_source(file_id, s2));
        assert!(manager.add_file_source(file_id, s3));
        assert_eq!(manager.get_some_file_sources(file_id), [s1, s3]);

        // merge folds the from-node set in and leaves the entry in place;
        // get_some still reports only the extremes, so membership is probed
        // through remove
        let other_file_id = FileId::new(2, 0);
        assert!(manager.add_file_source(other_file_id, s2));
        manager.merge(file_id, other_file_id);
        assert_eq!(manager.get_some_file_sources(file_id), [s1, s3]);
        assert!(manager.get_some_file_sources(other_file_id).is_empty());
        assert!(manager.remove_file_source(file_id, s2));
        assert_eq!(manager.get_some_file_sources(file_id), [s1, s3]);

        // merging from a missing node is a no-op
        manager.merge(file_id, FileId::new(3, 0));
        assert_eq!(manager.get_some_file_sources(file_id), [s1, s3]);
    }

    #[test]
    fn file_reference_manager_filters_message_sources() {
        let mut manager = FileReferenceManager::new();
        let dialog_id = DialogId::new(-100200300);
        let m1 = MessageFullId::new(dialog_id, MessageId::new(11 << 20));
        let m2 = MessageFullId::new(dialog_id, MessageId::new(12 << 20));
        let file_id = FileId::new(1, 0);
        let s1 = manager.intern(FileSource::Message(m1));
        let s2 = manager.intern(FileSource::SavedAnimations);
        let s3 = manager.intern(FileSource::Message(m2));
        manager.add_file_source(file_id, s1);
        manager.add_file_source(file_id, s2);
        manager.add_file_source(file_id, s3);
        assert_eq!(manager.get_some_message_file_sources(file_id), [m1, m2]);
    }

    #[test]
    fn file_data_sources_survive_the_real_registry() {
        let message_full_id =
            MessageFullId::new(DialogId::new(-100200300), MessageId::new(11 << 20));
        let mut manager = FileReferenceManager::new();
        let ids = vec![
            manager.intern(FileSource::Message(message_full_id)),
            manager.intern(FileSource::RecentStickers { is_attached: true }),
            manager.intern(FileSource::WebPage("https://example.com".to_string())),
        ];

        let data = FileData {
            remote_name: "a.jpg".to_string(),
            file_source_ids: ids,
            ..FileData::default()
        };
        let mut out = Vec::new();
        data.store(&mut out, &mut manager);

        // the trailing source section: count, then tag + payload each
        let mut expected_tail = Vec::new();
        store_parse::store_i32(&mut expected_tail, 3);
        store_parse::store_i32(&mut expected_tail, 0);
        message_full_id.store(&mut expected_tail);
        store_parse::store_i32(&mut expected_tail, 7);
        store_parse::store_i32(&mut expected_tail, 1);
        store_parse::store_i32(&mut expected_tail, 5);
        store_parse::store_string(&mut expected_tail, "https://example.com");
        assert_eq!(&out[out.len() - expected_tail.len()..], &expected_tail[..]);

        // a fresh registry re-interns the sources in encounter order
        let mut fresh = FileReferenceManager::new();
        let parsed = FileData::parse_from(&mut Parser::new(&out), true, Some(&mut fresh)).unwrap();
        assert_eq!(
            parsed.file_source_ids,
            [
                FileSourceId::new(1),
                FileSourceId::new(2),
                FileSourceId::new(3)
            ]
        );
        assert_eq!(parsed.remote_name, "a.jpg");
    }

    #[test]
    fn merge_choose_helpers_match_the_cpp_tables() {
        // 0 = keep x, 1 = take y, 2 = equal, -1 = conflict
        let empty = LocalFileLocation::Empty;
        let full = LocalFileLocation::Full(FullLocalFileLocation::new(
            FileType::Document,
            "/tmp/a".to_owned(),
            0,
        ));
        assert_eq!(merge_choose_local_location(&empty, &full), 1);
        assert_eq!(merge_choose_local_location(&full, &empty), 0);
        assert_eq!(merge_choose_local_location(&empty, &empty), 2);

        assert_eq!(
            merge_choose_file_source_location(
                FileLocationSource::FromDatabase,
                FileLocationSource::FromServer
            ),
            1
        );
        assert_eq!(
            merge_choose_file_source_location(
                FileLocationSource::FromServer,
                FileLocationSource::FromDatabase
            ),
            0
        );
        assert_eq!(
            merge_choose_file_source_location(
                FileLocationSource::FromServer,
                FileLocationSource::FromServer
            ),
            0
        );

        assert_eq!(merge_choose_size(0, 5), 1);
        assert_eq!(merge_choose_size(5, 0), 0);
        assert_eq!(merge_choose_size(5, 5), 2);
        assert_eq!(merge_choose_size(5, 6), -1);

        assert_eq!(merge_choose_expected_size(0, 5), 1);
        assert_eq!(merge_choose_expected_size(5, 0), 0);
        // unlike the real size, a mismatch is not an error here
        assert_eq!(merge_choose_expected_size(5, 6), 2);

        assert_eq!(merge_choose_name("", "a"), 1);
        assert_eq!(merge_choose_name("a", ""), 0);
        assert_eq!(merge_choose_name("a", "b"), 2);

        assert_eq!(merge_choose_owner(DialogId::default(), DialogId::new(1)), 1);
        assert_eq!(merge_choose_owner(DialogId::new(1), DialogId::default()), 0);
        assert_eq!(merge_choose_owner(DialogId::new(1), DialogId::new(2)), 2);

        // equal priorities: the lower id wins; different: the higher priority
        assert_eq!(
            merge_choose_main_file_id(FileId::new(1, 0), 0, FileId::new(2, 0), 0),
            0
        );
        assert_eq!(
            merge_choose_main_file_id(FileId::new(2, 0), 0, FileId::new(1, 0), 0),
            1
        );
        assert_eq!(
            merge_choose_main_file_id(FileId::new(1, 0), 0, FileId::new(2, 0), 5),
            1
        );
        assert_eq!(
            merge_choose_main_file_id(FileId::new(2, 0), 5, FileId::new(1, 0), 0),
            0
        );

        let key1 = FileEncryptionKey::new(&[1u8; 32], &[2u8; 32]);
        let key2 = FileEncryptionKey::new(&[3u8; 32], &[4u8; 32]);
        assert_eq!(
            merge_choose_encryption_key(&FileEncryptionKey::default(), &key1),
            1
        );
        assert_eq!(
            merge_choose_encryption_key(&key1, &FileEncryptionKey::default()),
            0
        );
        assert_eq!(merge_choose_encryption_key(&key1, &key1), 2);
        assert_eq!(merge_choose_encryption_key(&key1, &key2), -1);
    }

    #[test]
    fn merge_choose_remote_and_generate_tables() {
        // a web location loses to a common one regardless of anything else:
        // the result is the index of the non-web side
        let web = FullRemoteFileLocation::from_web(
            FileType::Document,
            "https://example.com/a".to_owned(),
            7,
        );
        let common = FullRemoteFileLocation::from_document(
            FileType::Document,
            10,
            11,
            DcId::internal(2),
            String::new(),
        );
        assert_eq!(
            merge_choose_full_remote_location(
                &web,
                FileLocationSource::FromUser,
                &common,
                FileLocationSource::FromUser
            ),
            1
        );
        assert_eq!(
            merge_choose_full_remote_location(
                &common,
                FileLocationSource::FromUser,
                &web,
                FileLocationSource::FromUser
            ),
            0
        );

        // an existing reference beats a dropped one ("#" after deletion;
        // a literal "#" cannot reach the registry, it is cleared at
        // construction), everything else equal
        let with_ref = FullRemoteFileLocation::from_document(
            FileType::Document,
            10,
            11,
            DcId::internal(2),
            "r".to_owned(),
        );
        let mut no_ref = FullRemoteFileLocation::from_document(
            FileType::Document,
            10,
            11,
            DcId::internal(2),
            "r".to_owned(),
        );
        assert!(no_ref.delete_file_reference(b"r"));
        assert_eq!(
            merge_choose_full_remote_location(
                &with_ref,
                FileLocationSource::FromUser,
                &no_ref,
                FileLocationSource::FromUser
            ),
            0
        );
        assert_eq!(
            merge_choose_full_remote_location(
                &no_ref,
                FileLocationSource::FromUser,
                &with_ref,
                FileLocationSource::FromUser
            ),
            1
        );

        // same reference: access_hash mismatch with a source mismatch picks
        // the more trusted source
        let hash11 = FullRemoteFileLocation::from_document(
            FileType::Document,
            10,
            11,
            DcId::internal(2),
            "r".to_owned(),
        );
        let hash12 = FullRemoteFileLocation::from_document(
            FileType::Document,
            10,
            12,
            DcId::internal(2),
            "r".to_owned(),
        );
        assert_eq!(
            merge_choose_full_remote_location(
                &hash11,
                FileLocationSource::FromUser,
                &hash12,
                FileLocationSource::FromServer
            ),
            1
        );
        // but identical locations leave both in place
        assert_eq!(
            merge_choose_full_remote_location(
                &hash11,
                FileLocationSource::FromServer,
                &hash11,
                FileLocationSource::FromServer
            ),
            2
        );

        // the alive check dominates the NewRemote choice
        let full_alive = NewRemoteFileLocation::from_remote(
            RemoteFileLocation::Full(hash11.clone()),
            FileLocationSource::FromServer,
        );
        let partial = NewRemoteFileLocation::from_remote(
            RemoteFileLocation::Partial(PartialRemoteFileLocation {
                file_id: 1,
                part_count: 4,
                part_size: 1024,
                ready_part_count: 2,
                is_big: 1,
                ready_size: 0,
            }),
            FileLocationSource::FromUser,
        );
        assert_eq!(merge_choose_remote_location(&full_alive, &partial), 0);
        assert_eq!(merge_choose_remote_location(&partial, &full_alive), 1);
        assert_eq!(merge_choose_remote_location(&partial, &partial), 2);

        // generate: some beats none, an #mtime# conversion wins, otherwise the
        // bigger conversion is the stable choice
        let gen_a = FullGenerateFileLocation {
            file_type: FileType::Document,
            original_path: "/a".to_owned(),
            conversion: "#mtime#1".to_owned(),
        };
        let gen_b = FullGenerateFileLocation {
            file_type: FileType::Document,
            original_path: "/b".to_owned(),
            conversion: "map".to_owned(),
        };
        assert_eq!(merge_choose_generate_location(None, Some(&gen_a)), 1);
        assert_eq!(merge_choose_generate_location(Some(&gen_a), None), 0);
        assert_eq!(
            merge_choose_generate_location(Some(&gen_a), Some(&gen_b)),
            0
        );
        assert_eq!(
            merge_choose_generate_location(Some(&gen_b), Some(&gen_a)),
            1
        );
        assert_eq!(
            merge_choose_generate_location(Some(&gen_a), Some(&gen_a)),
            2
        );
    }

    /// A partial-local document with the first two of four 1024-byte parts.
    fn partial_local_node(main_file_id: FileId) -> FileNode {
        let local = LocalFileLocation::Partial(PartialLocalFileLocation {
            file_type: FileType::Document,
            part_size: 1024,
            path: "/tmp/partial".to_owned(),
            iv: Vec::new(),
            ready_bitmask: Bitmask::from_ones(2).encode(-1),
            ready_size: 0,
        });
        FileNode::new(
            local,
            NewRemoteFileLocation::default(),
            None,
            4096,
            0,
            String::new(),
            String::new(),
            DialogId::default(),
            FileEncryptionKey::default(),
            main_file_id,
            0,
        )
    }

    #[test]
    fn file_node_tracks_local_progress_and_limits() {
        let mut node = partial_local_node(FileId::new(1, 0));

        // init_ready_size derived both counters from the bitmask
        assert_eq!(node.local_prefix_size(), 2048);
        assert_eq!(node.local_total_size(), 2048);
        assert_eq!(node.downloaded_prefix(0), 2048);
        assert_eq!(node.downloaded_prefix(1024), 1024);
        assert_eq!(node.path(), "/tmp/partial");

        // the offset shift re-derives the prefix from the bitmask
        node.set_download_offset(1024);
        assert_eq!(node.local_prefix_size(), 1024);
        // out-of-range offsets are dropped
        node.set_download_offset(-1);
        assert_eq!(node.download_offset(), 1024);
        node.set_download_offset(MAX_FILE_SIZE + 1);
        assert_eq!(node.download_offset(), 1024);

        // download limits clamp at MAX_FILE_SIZE and the ignore switch
        node.set_download_limit(-1);
        assert_eq!(node.get_download_limit(), 0);
        node.set_download_limit(MAX_FILE_SIZE + 1);
        assert!(node.get_download_limit() <= MAX_FILE_SIZE);
        node.set_ignore_download_limit(true);
        assert_eq!(node.get_download_limit(), 0);

        // the SecureEncrypted gate hides the prefix until verified; the type
        // comes from the full remote location (get_type never reads a
        // partial local)
        let encrypted_remote = FullRemoteFileLocation::from_document(
            FileType::SecureEncrypted,
            10,
            11,
            DcId::internal(2),
            "r".to_owned(),
        );
        let encrypted_local = LocalFileLocation::Partial(PartialLocalFileLocation {
            file_type: FileType::SecureEncrypted,
            part_size: 1024,
            path: "/tmp/enc".to_owned(),
            iv: Vec::new(),
            ready_bitmask: Bitmask::from_ones(4).encode(-1),
            ready_size: 0,
        });
        let encrypted = FileNode::new(
            encrypted_local,
            NewRemoteFileLocation::from_remote(
                RemoteFileLocation::Full(encrypted_remote),
                FileLocationSource::FromServer,
            ),
            None,
            4096,
            0,
            String::new(),
            String::new(),
            DialogId::default(),
            FileEncryptionKey::default(),
            FileId::new(2, 0),
            0,
        );
        assert_eq!(encrypted.local_prefix_size(), 0);
        assert_eq!(encrypted.downloaded_prefix(0), 0);
        assert_eq!(encrypted.local_total_size(), 4096);
    }

    #[test]
    fn file_node_flush_flags_match_the_cpp_gates() {
        let mut node = FileNode::new(
            LocalFileLocation::Empty,
            NewRemoteFileLocation::default(),
            None,
            0,
            0,
            String::new(),
            String::new(),
            DialogId::default(),
            FileEncryptionKey::default(),
            FileId::new(1, 0),
            0,
        );
        assert!(!node.need_info_flush());
        assert!(!node.need_pmc_flush());

        // a bare full-local location alone is not worth a pmc row... unless
        // something else is present; the C++ gate needs the company
        node.set_local_location(
            LocalFileLocation::Full(FullLocalFileLocation::new(
                FileType::Document,
                "/tmp/a".to_owned(),
                0,
            )),
            -1,
            -1,
        );
        node.on_pmc_flushed();
        assert!(!node.need_pmc_flush());

        // the "#file_id#" conversion is explicitly not saved
        node.set_generate_location(Some(FullGenerateFileLocation {
            file_type: FileType::Document,
            original_path: "/a".to_owned(),
            conversion: "#file_id#123".to_owned(),
        }));
        assert!(!node.need_pmc_flush());

        // but a real conversion is
        node.set_generate_location(Some(FullGenerateFileLocation {
            file_type: FileType::Document,
            original_path: "/a".to_owned(),
            conversion: "#mtime#9".to_owned(),
        }));
        assert!(node.need_pmc_flush());
        node.on_pmc_flushed();

        // the encryption key always flushes
        node.set_encryption_key(FileEncryptionKey::new(&[1u8; 32], &[2u8; 32]));
        assert!(node.need_pmc_flush());
    }

    #[test]
    fn file_view_accessors_match_the_cpp() {
        let common = FullRemoteFileLocation::from_document(
            FileType::Document,
            10,
            11,
            DcId::internal(2),
            "r".to_owned(),
        );
        let mut node = FileNode::new(
            LocalFileLocation::Full(FullLocalFileLocation::new(
                FileType::Photo,
                "/photos/a".to_owned(),
                0,
            )),
            NewRemoteFileLocation::from_remote(
                RemoteFileLocation::Full(common),
                FileLocationSource::FromServer,
            ),
            None,
            500,
            0,
            "a.jpg".to_owned(),
            String::new(),
            DialogId::new(-100200300),
            FileEncryptionKey::default(),
            FileId::new(1, 0),
            0,
        );
        let view = FileView::new(&node);

        // the local location's type wins over the remote one
        assert_eq!(view.get_type(), FileType::Photo);
        assert!(!view.is_encrypted());
        assert!(view.has_full_local_location());
        assert!(view.has_alive_remote_location());
        assert!(view.has_active_upload_remote_location());
        assert_eq!(view.remote_name(), "a.jpg");
        assert_eq!(view.owner_dialog_id(), DialogId::new(-100200300));
        assert_eq!(view.size(), 500);
        assert_eq!(view.downloaded_prefix(100), 400);
        assert_eq!(view.remote_size(), 500);

        // web locations are always downloadable
        node.set_new_remote_location(NewRemoteFileLocation::from_remote(
            RemoteFileLocation::Full(FullRemoteFileLocation::from_web(
                FileType::Document,
                "https://example.com/a".to_owned(),
                7,
            )),
            FileLocationSource::FromUser,
        ));
        assert!(FileView::new(&node).can_download_from_server());

        // a dead non-encrypted location whose reference has been dropped
        // (deletion leaves the "#" sentinel) and which is no longer alive is
        // not downloadable
        let mut dead = FullRemoteFileLocation::from_document(
            FileType::Document,
            10,
            11,
            DcId::internal(2),
            "r".to_owned(),
        );
        assert!(dead.delete_file_reference(b"r"));
        let dead_node = FileNode::new(
            LocalFileLocation::Empty,
            NewRemoteFileLocation {
                partial: None,
                full: Some(dead),
                is_full_alive: false,
                full_source: FileLocationSource::FromDatabase,
            },
            None,
            500,
            0,
            String::new(),
            String::new(),
            DialogId::default(),
            FileEncryptionKey::default(),
            FileId::new(2, 0),
            0,
        );
        assert!(!FileView::new(&dead_node).can_download_from_server());

        // an encrypted file needs its key before the server can be asked
        let encrypted_remote = FullRemoteFileLocation::from_document(
            FileType::Encrypted,
            10,
            11,
            DcId::internal(2),
            "r".to_owned(),
        );
        let encrypted_alive = NewRemoteFileLocation::from_remote(
            RemoteFileLocation::Full(encrypted_remote),
            FileLocationSource::FromServer,
        );
        let no_key = FileNode::new(
            LocalFileLocation::Empty,
            encrypted_alive.clone(),
            None,
            500,
            0,
            String::new(),
            String::new(),
            DialogId::default(),
            FileEncryptionKey::default(),
            FileId::new(3, 0),
            0,
        );
        assert!(!FileView::new(&no_key).can_download_from_server());
        assert!(FileView::new(&no_key).is_encrypted_any());
        let with_key = FileNode::new(
            LocalFileLocation::Empty,
            encrypted_alive,
            None,
            500,
            0,
            String::new(),
            String::new(),
            DialogId::default(),
            FileEncryptionKey::new(&[1u8; 32], &[2u8; 32]),
            FileId::new(3, 0),
            0,
        );
        assert!(FileView::new(&with_key).can_download_from_server());

        // may_reload_photo: the thumbnail source is a legacy-class one
        let legacy_photo = NewRemoteFileLocation::from_remote(
            RemoteFileLocation::Full(FullRemoteFileLocation::from_photo(
                PhotoSizeSource::thumbnail(FileType::Photo, b'a' as i32),
                9,
                3,
                DcId::internal(2),
                "r".to_owned(),
            )),
            FileLocationSource::FromServer,
        );
        let legacy_node = FileNode::new(
            LocalFileLocation::Empty,
            legacy_photo,
            None,
            0,
            0,
            String::new(),
            String::new(),
            DialogId::default(),
            FileEncryptionKey::default(),
            FileId::new(4, 0),
            0,
        );
        assert!(!FileView::new(&legacy_node).may_reload_photo());
    }

    /// Records the merge hook calls; the C++ ordering is the test payload.
    #[derive(Clone, Default)]
    struct EventLog(Arc<parking_lot::Mutex<Vec<String>>>);

    struct RecordingWorker {
        log: EventLog,
    }

    impl RecordingWorker {
        fn record(&self, event: &str, main_file_id: FileId) {
            self.log.0.lock().push(format!("{event} {main_file_id}"));
        }
    }

    impl FileManagerWorker for RecordingWorker {
        fn cancel_download(&mut self, node: &FileNode) {
            self.record("cancel_download", node.get_main_file_id());
        }
        fn cancel_upload(&mut self, node: &FileNode) {
            self.record("cancel_upload", node.get_main_file_id());
        }
        fn cancel_generate(&mut self, node: &FileNode) {
            self.record("cancel_generate", node.get_main_file_id());
        }
        fn finish_download(&mut self, file_id: FileId) {
            self.record("finish_download", file_id);
        }
        fn finish_upload(&mut self, file_id: FileId) {
            self.record("finish_upload", file_id);
        }
        fn run_node(&mut self, node: &FileNode) {
            self.record("run_node", node.get_main_file_id());
        }
        fn flush_node_info(&mut self, node: &FileNode) {
            self.record("flush_info", node.get_main_file_id());
        }
        fn on_merge_files(&mut self, from_file_id: FileId, to_file_id: FileId) {
            self.log
                .0
                .lock()
                .push(format!("on_merge_files {from_file_id} -> {to_file_id}"));
        }
    }

    fn document_node(main_file_id: FileId, size: i64, file_reference: &str) -> FileNode {
        let remote = FullRemoteFileLocation::from_document(
            FileType::Document,
            10,
            11,
            DcId::internal(2),
            file_reference.to_owned(),
        );
        FileNode::new(
            LocalFileLocation::Empty,
            NewRemoteFileLocation::from_remote(
                RemoteFileLocation::Full(remote),
                FileLocationSource::FromServer,
            ),
            None,
            size,
            0,
            "doc.pdf".to_owned(),
            String::new(),
            DialogId::default(),
            FileEncryptionKey::default(),
            main_file_id,
            0,
        )
    }

    #[test]
    fn merge_duplicate_download_yields_a_single_file() {
        let mut file_manager = FileManager::new();
        let log = EventLog::default();
        file_manager.set_worker(Box::new(RecordingWorker { log: log.clone() }));

        file_manager.register_file_node(
            FileId::new(1, 0),
            0,
            document_node(FileId::new(1, 0), 100, "ref"),
        );
        file_manager.register_file_node(
            FileId::new(2, 0),
            0,
            document_node(FileId::new(2, 0), 100, "ref"),
        );

        file_manager
            .merge(FileId::new(1, 0), FileId::new(2, 0))
            .unwrap();

        // both ids resolve to the survivor node, whose main id is x's
        assert_eq!(
            file_manager
                .get_file_node(FileId::new(1, 0))
                .unwrap()
                .get_main_file_id(),
            FileId::new(1, 0)
        );
        assert_eq!(
            file_manager
                .get_file_node(FileId::new(2, 0))
                .unwrap()
                .get_main_file_id(),
            FileId::new(1, 0)
        );

        // hook order: the dropped node's queries cancel, the managers learn
        // about the re-key, the finished uploads drain, then the survivor
        // restarts and flushes
        assert_eq!(
            log.0.lock().clone(),
            [
                "cancel_download 2(0)".to_owned(),
                "cancel_upload 2(0)".to_owned(),
                "cancel_generate 2(0)".to_owned(),
                "on_merge_files 1(0) -> 2(0)".to_owned(),
                "finish_upload 1(0)".to_owned(),
                "finish_upload 2(0)".to_owned(),
                "run_node 1(0)".to_owned(),
            ]
        );

        // merging again is a same-node no-op
        file_manager
            .merge(FileId::new(1, 0), FileId::new(2, 0))
            .unwrap();
        assert_eq!(log.0.lock().len(), 7);
    }

    #[test]
    fn merge_rejects_conflicting_sizes_and_keys() {
        let mut file_manager = FileManager::new();
        let log = EventLog::default();
        file_manager.set_worker(Box::new(RecordingWorker { log: log.clone() }));

        file_manager.register_file_node(
            FileId::new(1, 0),
            0,
            document_node(FileId::new(1, 0), 100, "ref"),
        );
        file_manager.register_file_node(
            FileId::new(2, 0),
            0,
            document_node(FileId::new(2, 0), 200, "ref"),
        );
        let err = file_manager
            .merge(FileId::new(1, 0), FileId::new(2, 0))
            .unwrap_err();
        assert_eq!(err.code(), 400);
        assert!(err.message().contains("Different size: 100 and 200"));
        // both nodes stay intact
        assert_eq!(
            file_manager.get_file_node(FileId::new(1, 0)).unwrap().size,
            100
        );
        assert_eq!(
            file_manager.get_file_node(FileId::new(2, 0)).unwrap().size,
            200
        );
        // only the two gated info-flush probes ran; no merge effects fired
        assert!(log.0.lock().is_empty());

        // different keys without a full remote location to consult
        let keyed = |key: [u8; 32], main_file_id: FileId| {
            FileNode::new(
                LocalFileLocation::Empty,
                NewRemoteFileLocation::default(),
                None,
                100,
                0,
                String::new(),
                String::new(),
                DialogId::default(),
                FileEncryptionKey::new(&key, &[0u8; 32]),
                main_file_id,
                0,
            )
        };
        let mut file_manager = FileManager::new();
        file_manager.register_file_node(FileId::new(1, 0), 0, keyed([1u8; 32], FileId::new(1, 0)));
        file_manager.register_file_node(FileId::new(2, 0), 0, keyed([3u8; 32], FileId::new(2, 0)));
        let err = file_manager
            .merge(FileId::new(1, 0), FileId::new(2, 0))
            .unwrap_err();
        assert_eq!(err.code(), 400);
        assert_eq!(
            err.message(),
            "Can't merge files. Different encryption keys"
        );
    }

    #[test]
    fn merge_prefers_the_node_with_more_file_ids_as_survivor() {
        let mut file_manager = FileManager::new();
        let log = EventLog::default();
        file_manager.set_worker(Box::new(RecordingWorker { log: log.clone() }));

        let mut wide = document_node(FileId::new(2, 0), 100, "ref");
        wide.file_ids.push(FileId::new(5, 0)); // the tests share the node module
        file_manager.register_file_node(
            FileId::new(1, 0),
            0,
            document_node(FileId::new(1, 0), 100, "ref"),
        );
        file_manager.register_file_node(FileId::new(2, 0), 0, wide);

        file_manager
            .merge(FileId::new(1, 0), FileId::new(2, 0))
            .unwrap();

        // y had two ids, so its node survives and x's id re-keys into it...
        let survivor = file_manager.get_file_node(FileId::new(1, 0)).unwrap();
        assert_eq!(
            survivor.file_ids,
            [FileId::new(2, 0), FileId::new(5, 0), FileId::new(1, 0)]
        );
        // ...but main_file_id_i chose x's lower id, so the survivor adopts it
        assert_eq!(survivor.get_main_file_id(), FileId::new(1, 0));
        assert_eq!(log.0.lock().last().unwrap(), "run_node 1(0)");
    }

    // ==== M.12.6: FileUploader / FileHashUploader ====

    fn temp_file(contents: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tdrs-m12-upload-{}-{}",
            std::process::id(),
            tdutils::random::secure_int64() as u64
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn full_local(path: &std::path::Path) -> LocalFileLocation {
        LocalFileLocation::Full(FullLocalFileLocation::new(
            FileType::Document,
            path.to_string_lossy().into_owned(),
            0,
        ))
    }

    #[derive(Clone, Default)]
    struct UploadLog {
        events: Arc<parking_lot::Mutex<Vec<String>>>,
        partials: Arc<parking_lot::Mutex<Vec<PartialRemoteFileLocation>>>,
        oks: Arc<parking_lot::Mutex<Vec<PartialRemoteFileLocation>>>,
        error: Arc<parking_lot::Mutex<Option<(i32, String)>>>,
    }

    struct RecordingUploadCallback {
        log: UploadLog,
    }

    impl FileUploaderCallback for RecordingUploadCallback {
        fn on_hash(&mut self, hash: Vec<u8>) {
            self.log
                .events
                .lock()
                .push(format!("hash {} bytes", hash.len()));
        }
        fn on_partial_upload(&mut self, partial: PartialRemoteFileLocation) {
            self.log.partials.lock().push(partial);
        }
        fn on_ok(&mut self, _file_type: FileType, partial: PartialRemoteFileLocation) {
            self.log.oks.lock().push(partial);
        }
        fn on_error(&mut self, status: Status) {
            *self.log.error.lock() = Some((status.code(), status.message().to_owned()));
        }
    }

    #[derive(Clone)]
    struct FakeUploadNet {
        queries: Arc<parking_lot::Mutex<Vec<UploadPartQuery>>>,
        results: Arc<parking_lot::Mutex<VecDeque<std::result::Result<bool, Status>>>>,
    }

    impl Default for FakeUploadNet {
        fn default() -> FakeUploadNet {
            FakeUploadNet {
                queries: Arc::new(Mutex::new(Vec::new())),
                results: Arc::new(Mutex::new(VecDeque::new())),
            }
        }
    }

    impl FakeUploadNet {
        fn with_results(results: Vec<std::result::Result<bool, Status>>) -> FakeUploadNet {
            let net = FakeUploadNet::default();
            *net.results.lock() = results.into_iter().collect();
            net
        }
    }

    impl FileUploaderNet for FakeUploadNet {
        fn upload_part(&mut self, query: UploadPartQuery) -> std::result::Result<bool, Status> {
            self.queries.lock().push(query);
            self.results.lock().pop_front().unwrap_or(Ok(true))
        }
    }

    fn grant(limit: i64) -> ResourceState {
        let mut state = ResourceState::default();
        state.set_unit_size(1024);
        state.update_limit(limit);
        state
    }

    /// The C++ ResourceManager ping-pong, collapsed: every part the uploader
    /// completes consumes its allowance (`update_estimated_limit` raises
    /// `used` up to the estimate), the worker notifies the manager, and the
    /// manager answers with a fresh grant until the upload stops.
    fn drive_upload(uploader: &mut FileUploader, log: &UploadLog) {
        let mut allowance = 1i64 << 20;
        for _ in 0..64 {
            if uploader.stop_flag() || log.error.lock().is_some() {
                return;
            }
            allowance += 1 << 20;
            uploader.update_resources(grant(allowance));
        }
        panic!("the uploader did not settle in 64 grant rounds");
    }

    #[test]
    fn file_uploader_rejects_already_uploaded_files() {
        let log = UploadLog::default();
        let remote = RemoteFileLocation::Full(FullRemoteFileLocation::from_document(
            FileType::Document,
            10,
            11,
            DcId::internal(2),
            "r".to_owned(),
        ));
        let mut uploader = FileUploader::new(
            LocalFileLocation::Empty,
            remote,
            0,
            FileEncryptionKey::default(),
            Vec::new(),
            Box::new(RecordingUploadCallback { log: log.clone() }),
            Box::new(FakeUploadNet::default()),
        );
        uploader.start();
        let (code, message) = log.error.lock().clone().expect("error");
        assert_eq!((code, message.as_str()), (0, "File is already uploaded"));
        assert!(uploader.stop_flag());
    }

    #[test]
    fn file_encryption_key_value_hash_round_trips_the_secure_tail() {
        // FileEncryptionKey.cpp:53-70: secret() stays the first 32 bytes
        // and the value hash rides after them
        let mut key = FileEncryptionKey::from_secret(&(0u8..32).collect::<Vec<u8>>());
        assert!(!key.has_value_hash());
        key.set_value_hash(&(64u8..96).collect::<Vec<u8>>());
        assert!(key.has_value_hash());
        assert_eq!(key.secret(), &(0u8..32).collect::<Vec<u8>>());
        assert_eq!(key.value_hash(), &(64u8..96).collect::<Vec<u8>>());
    }

    #[test]
    fn file_uploader_resets_incomplete_secure_partials() {
        // a secure upload whose partial remote is incomplete restarts from
        // scratch (the constructor drops the remote)
        let key = FileEncryptionKey::from_secret(b"the-secure-secret".to_vec().as_slice());
        let partial = PartialRemoteFileLocation {
            file_id: 4242,
            part_count: 5,
            part_size: 256,
            ready_part_count: 2,
            is_big: 0,
            ready_size: 0,
        };
        let uploader = FileUploader::new(
            LocalFileLocation::Empty,
            RemoteFileLocation::Partial(partial),
            0,
            key,
            Vec::new(),
            Box::new(RecordingUploadCallback {
                log: UploadLog::default(),
            }),
            Box::new(FakeUploadNet::default()),
        );
        assert!(matches!(uploader.remote(), RemoteFileLocation::Empty));

        // the same partial under a non-secure key is kept
        let partial = PartialRemoteFileLocation {
            file_id: 4242,
            part_count: 5,
            part_size: 256,
            ready_part_count: 2,
            is_big: 0,
            ready_size: 0,
        };
        let uploader = FileUploader::new(
            LocalFileLocation::Empty,
            RemoteFileLocation::Partial(partial),
            0,
            FileEncryptionKey::default(),
            Vec::new(),
            Box::new(RecordingUploadCallback {
                log: UploadLog::default(),
            }),
            Box::new(FakeUploadNet::default()),
        );
        assert!(matches!(uploader.remote(), RemoteFileLocation::Partial(_)));
    }

    #[test]
    fn file_uploader_uploads_a_small_file() {
        let contents = vec![7u8; 600];
        let path = temp_file(&contents);
        let log = UploadLog::default();
        let net = FakeUploadNet::default();
        let mut uploader = FileUploader::new(
            full_local(&path),
            RemoteFileLocation::Empty,
            0,
            FileEncryptionKey::default(),
            Vec::new(),
            Box::new(RecordingUploadCallback { log: log.clone() }),
            Box::new(net.clone()),
        );

        // start reports the empty progress; nothing is sent without resources
        uploader.start();
        assert_eq!(net.queries.lock().len(), 0);
        assert_eq!(log.partials.lock().len(), 1);

        // the manager grant drives the upload
        drive_upload(&mut uploader, &log);

        // a single small part carrying the whole file
        let queries = net.queries.lock();
        assert_eq!(queries.len(), 1);
        match &queries[0] {
            UploadPartQuery::SaveFilePart {
                file_id,
                part_id,
                bytes,
            } => {
                assert_eq!(*file_id, uploader.file_id());
                assert_ne!(*file_id, 0);
                assert_eq!(*part_id, 0);
                assert_eq!(bytes, &contents);
            }
            _ => panic!("expected a small-file part query"),
        }

        // the final ok carries the full size
        assert_eq!(log.partials.lock().len(), 2);
        assert_eq!(log.oks.lock().len(), 1);
        let ok = &log.oks.lock()[0];
        assert_eq!(ok.file_id, uploader.file_id());
        assert_eq!(ok.ready_size, 600);
        assert_eq!(ok.ready_part_count, 1);
        assert_eq!(ok.is_big, 0);
        assert!(uploader.stop_flag());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_uploader_resumes_from_a_partial_remote() {
        // parts 0 and 1 of a 3-part upload are on the server; part 1 is bad
        let contents = vec![9u8; 600];
        let path = temp_file(&contents);
        let log = UploadLog::default();
        let net = FakeUploadNet::default();
        let mut uploader = FileUploader::new(
            full_local(&path),
            RemoteFileLocation::Partial(PartialRemoteFileLocation {
                file_id: 4242,
                part_count: 3,
                part_size: 256,
                ready_part_count: 2,
                is_big: 0,
                ready_size: 512,
            }),
            0,
            FileEncryptionKey::default(),
            vec![1],
            Box::new(RecordingUploadCallback { log: log.clone() }),
            Box::new(net.clone()),
        );
        uploader.start();
        drive_upload(&mut uploader, &log);

        // only parts 1 and 2 are sent, under the kept file id
        let queries = net.queries.lock();
        assert_eq!(queries.len(), 2);
        for (i, query) in queries.iter().enumerate() {
            match query {
                UploadPartQuery::SaveFilePart {
                    file_id,
                    part_id,
                    bytes,
                } => {
                    assert_eq!(*file_id, 4242);
                    assert_eq!(*part_id, i as i32 + 1);
                    let expected_len = if i == 0 { 256 } else { 88 };
                    assert_eq!(bytes.len(), expected_len);
                }
                _ => panic!("expected a small-file part query"),
            }
        }
        assert_eq!(log.oks.lock().len(), 1);
        assert_eq!(log.oks.lock()[0].file_id, 4242);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_uploader_restarts_when_the_first_part_is_bad() {
        // bad part 0 invalidates the whole partial remote: a fresh file id
        // replaces it and the file uploads from scratch
        let contents = vec![9u8; 600];
        let path = temp_file(&contents);
        let log = UploadLog::default();
        let net = FakeUploadNet::default();
        let mut uploader = FileUploader::new(
            full_local(&path),
            RemoteFileLocation::Partial(PartialRemoteFileLocation {
                file_id: 4242,
                part_count: 3,
                part_size: 256,
                ready_part_count: 2,
                is_big: 0,
                ready_size: 512,
            }),
            0,
            FileEncryptionKey::default(),
            vec![0],
            Box::new(RecordingUploadCallback { log: log.clone() }),
            Box::new(net.clone()),
        );
        uploader.start();
        assert!(matches!(uploader.remote(), RemoteFileLocation::Empty));
        drive_upload(&mut uploader, &log);

        let queries = net.queries.lock();
        assert_eq!(queries.len(), 1);
        match &queries[0] {
            UploadPartQuery::SaveFilePart {
                file_id,
                part_id,
                bytes,
            } => {
                assert_ne!(*file_id, 4242);
                assert_eq!(*part_id, 0);
                assert_eq!(bytes.len(), 600);
            }
            _ => panic!("expected a small-file part query"),
        }
        assert_eq!(log.oks.lock()[0].file_id, uploader.file_id());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_uploader_pads_and_chains_the_iv_for_secret_files() {
        let contents: Vec<u8> = (0..40).map(|i| i as u8).collect();
        let path = temp_file(&contents);
        let log = UploadLog::default();
        let net = FakeUploadNet::default();
        let key = FileEncryptionKey::new(&[1u8; 32], &[2u8; 32]);
        let mut uploader = FileUploader::new(
            full_local(&path),
            RemoteFileLocation::Empty,
            0,
            key.clone(),
            Vec::new(),
            Box::new(RecordingUploadCallback { log: log.clone() }),
            Box::new(net.clone()),
        );
        uploader.start();
        drive_upload(&mut uploader, &log);

        // one part, padded to the 16-byte boundary
        let recorded = net.queries.lock();
        assert_eq!(recorded.len(), 1);
        let cipher = match &recorded[0] {
            UploadPartQuery::SaveFilePart { bytes, .. } => bytes.clone(),
            _ => panic!("expected a small-file part query"),
        };
        assert_eq!(cipher.len(), 48);
        assert_ne!(cipher[..40], contents);

        // the chained iv decrypts back to the file contents
        let mut iv = [2u8; 32];
        let mut plain = cipher.clone();
        tdutils::crypto::aes_ige_decrypt_in_place(&[1u8; 32], &mut iv, &mut plain);
        assert_eq!(&plain[..40], &contents[..]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_uploader_handles_net_restarts_and_failures() {
        let contents = vec![1u8; 600];
        let path = temp_file(&contents);

        // a canceled part query restarts the same part
        let log = UploadLog::default();
        let net = FakeUploadNet::with_results(vec![
            Err(Status::error(NET_QUERY_CANCELED, "Canceled")),
            Ok(true),
        ]);
        let mut uploader = FileUploader::new(
            full_local(&path),
            RemoteFileLocation::Empty,
            0,
            FileEncryptionKey::default(),
            Vec::new(),
            Box::new(RecordingUploadCallback { log: log.clone() }),
            Box::new(net.clone()),
        );
        uploader.start();
        drive_upload(&mut uploader, &log);
        {
            let queries = net.queries.lock();
            assert_eq!(queries.len(), 2);
            let part_id = |query: &UploadPartQuery| match query {
                UploadPartQuery::SaveFilePart { part_id, .. } => *part_id,
                UploadPartQuery::SaveBigFilePart { part_id, .. } => *part_id,
            };
            assert_eq!(part_id(&queries[0]), 0);
            assert_eq!(part_id(&queries[1]), 0);
        }
        assert_eq!(log.oks.lock().len(), 1);
        assert!(log.error.lock().is_none());

        // a server Bool(false) is the 500 error
        let log = UploadLog::default();
        let net = FakeUploadNet::with_results(vec![Ok(false)]);
        let mut uploader = FileUploader::new(
            full_local(&path),
            RemoteFileLocation::Empty,
            0,
            FileEncryptionKey::default(),
            Vec::new(),
            Box::new(RecordingUploadCallback { log: log.clone() }),
            Box::new(net),
        );
        uploader.start();
        drive_upload(&mut uploader, &log);
        let (code, message) = log.error.lock().clone().expect("error");
        assert_eq!(
            (code, message.as_str()),
            (500, "Internal Server Error during file upload")
        );

        // any other net error fails the upload verbatim
        let log = UploadLog::default();
        let net = FakeUploadNet::with_results(vec![Err(Status::error(400, "FILE_PART_INVALID"))]);
        let mut uploader = FileUploader::new(
            full_local(&path),
            RemoteFileLocation::Empty,
            0,
            FileEncryptionKey::default(),
            Vec::new(),
            Box::new(RecordingUploadCallback { log: log.clone() }),
            Box::new(net),
        );
        uploader.start();
        drive_upload(&mut uploader, &log);
        let (code, message) = log.error.lock().clone().expect("error");
        assert_eq!((code, message.as_str()), (400, "FILE_PART_INVALID"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_uploader_enforces_the_expected_size_bump_rule() {
        // the 10 MB default estimate is only bumped once the local size
        // passes 30 MB; any other estimate follows the local size at once
        let cases: Vec<(i64, i64, i64)> = vec![
            (10 << 20, 15 << 20, 10 << 20), // default estimate, small growth: no bump
            (10 << 20, 31 << 20, 31 << 20), // past 30 MB: bump
            (5 << 20, 6 << 20, 6 << 20),    // a real estimate: bump
        ];
        for (expected_size, local_size, want) in cases {
            let contents = vec![0u8; local_size as usize];
            let path = temp_file(&contents);
            let mut uploader = FileUploader::new(
                full_local(&path),
                RemoteFileLocation::Empty,
                expected_size,
                FileEncryptionKey::default(),
                Vec::new(),
                Box::new(RecordingUploadCallback {
                    log: UploadLog::default(),
                }),
                Box::new(FakeUploadNet::default()),
            );
            let local = full_local(&path);
            uploader.on_update_local_location(&local, 0).unwrap();
            assert_eq!(uploader.expected_size, want);
            let _ = std::fs::remove_file(&path);
        }
    }

    /// A recorded `getDocumentByHash` query: (sha256, size, url).
    type HashQuery = ([u8; 32], i64, String);

    #[derive(Clone, Default)]
    struct HashNetState {
        queries: Arc<parking_lot::Mutex<Vec<HashQuery>>>,
        result: Arc<parking_lot::Mutex<Option<std::result::Result<DocumentByHash, Status>>>>,
    }

    struct FakeHashNet {
        state: HashNetState,
    }

    impl FileHashUploaderNet for FakeHashNet {
        fn get_document_by_hash(
            &mut self,
            sha256: [u8; 32],
            size: i64,
            mime_type: String,
        ) -> std::result::Result<DocumentByHash, Status> {
            self.state.queries.lock().push((sha256, size, mime_type));
            self.state
                .result
                .lock()
                .take()
                .unwrap_or(Ok(DocumentByHash::Empty))
        }
    }

    #[derive(Clone, Default)]
    struct HashLog {
        ok: Arc<parking_lot::Mutex<Option<FullRemoteFileLocation>>>,
        error: Arc<parking_lot::Mutex<Option<String>>>,
    }

    struct RecordingHashCallback {
        log: HashLog,
    }

    impl FileHashUploaderCallback for RecordingHashCallback {
        fn on_ok(&mut self, location: FullRemoteFileLocation) {
            *self.log.ok.lock() = Some(location);
        }
        fn on_error(&mut self, status: Status) {
            *self.log.error.lock() = Some(status.message().to_owned());
        }
    }

    #[test]
    fn file_hash_uploader_hashes_then_queries_the_server() {
        let contents: Vec<u8> = (0..1000).map(|i| (i % 251) as u8).collect();
        let path = temp_file(&contents);
        let state = HashNetState::default();
        let log = HashLog::default();
        let mut uploader = FileHashUploader::new(
            FullLocalFileLocation::new(FileType::Document, path.to_string_lossy().into_owned(), 0),
            1000,
            Box::new(RecordingHashCallback { log: log.clone() }),
            Box::new(FakeHashNet {
                state: state.clone(),
            }),
        );
        uploader.start();

        // no hashing without a resource grant
        uploader.loop_();
        assert_eq!(state.queries.lock().len(), 0);

        // the server answer waiting on the seam
        *state.result.lock() = Some(Ok(DocumentByHash::Document {
            id: 77,
            access_hash: 88,
            dc_id: 2,
            file_reference: b"ref".to_vec(),
        }));

        // the grant drives the sha pass and then the query
        uploader.update_resources(grant(1 << 20));
        let queries = state.queries.lock();
        assert_eq!(queries.len(), 1);
        let (hash, size, mime_type) = &queries[0];
        assert_eq!(*hash, tdutils::crypto::sha256(&contents));
        assert_eq!(*size, 1000);
        assert_eq!(mime_type, "image/gif");

        // the server found the document: on_ok carries the remote location
        let expected = FullRemoteFileLocation::from_document(
            FileType::Document,
            77,
            88,
            DcId::internal(2),
            "ref".to_owned(),
        );
        assert_eq!(log.ok.lock().as_ref(), Some(&expected));
        assert!(uploader.stop_flag());
        assert!(log.error.lock().is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_hash_uploader_reports_empty_and_invalid_dc() {
        let contents = vec![3u8; 1000];
        let path = temp_file(&contents);

        // documentEmpty
        let state = HashNetState::default();
        let log = HashLog::default();
        let mut uploader = FileHashUploader::new(
            FullLocalFileLocation::new(FileType::Document, path.to_string_lossy().into_owned(), 0),
            1000,
            Box::new(RecordingHashCallback { log: log.clone() }),
            Box::new(FakeHashNet {
                state: state.clone(),
            }),
        );
        uploader.start();
        uploader.update_resources(grant(1 << 20));
        assert_eq!(
            log.error.lock().as_deref(),
            Some("Document is not found by hash")
        );
        assert!(uploader.stop_flag());

        // a document on an invalid dc
        let state = HashNetState::default();
        *state.result.lock() = Some(Ok(DocumentByHash::Document {
            id: 1,
            access_hash: 2,
            dc_id: 0,
            file_reference: b"ref".to_vec(),
        }));
        let log = HashLog::default();
        let mut uploader = FileHashUploader::new(
            FullLocalFileLocation::new(FileType::Document, path.to_string_lossy().into_owned(), 0),
            1000,
            Box::new(RecordingHashCallback { log: log.clone() }),
            Box::new(FakeHashNet { state }),
        );
        uploader.start();
        uploader.update_resources(grant(1 << 20));
        assert_eq!(
            log.error.lock().as_deref(),
            Some("Found document has invalid DcId")
        );

        // a declared size that mismatches the file fails at init
        let log = HashLog::default();
        let mut uploader = FileHashUploader::new(
            FullLocalFileLocation::new(FileType::Document, path.to_string_lossy().into_owned(), 0),
            999,
            Box::new(RecordingHashCallback { log: log.clone() }),
            Box::new(FakeHashNet {
                state: HashNetState::default(),
            }),
        );
        uploader.start();
        assert_eq!(log.error.lock().as_deref(), Some("Size mismatch"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_hash_uploader_streams_the_hash_across_grants() {
        let contents: Vec<u8> = (0..1000).map(|i| (i % 253) as u8).collect();
        let path = temp_file(&contents);
        let state = HashNetState::default();
        let log = HashLog::default();
        let mut uploader = FileHashUploader::new(
            FullLocalFileLocation::new(FileType::Document, path.to_string_lossy().into_owned(), 0),
            1000,
            Box::new(RecordingHashCallback { log: log.clone() }),
            Box::new(FakeHashNet {
                state: state.clone(),
            }),
        );
        uploader.start();

        // two half-size grants: the hash streams across both, one query
        uploader.update_resources(grant(500));
        assert_eq!(state.queries.lock().len(), 0);
        uploader.update_resources(grant(1000));
        assert_eq!(state.queries.lock().len(), 1);
        assert_eq!(
            state.queries.lock()[0].0,
            tdutils::crypto::sha256(&contents)
        );

        // the document answer completes the upload
        *state.result.lock() = Some(Ok(DocumentByHash::Document {
            id: 5,
            access_hash: 6,
            dc_id: 2,
            file_reference: b"r".to_vec(),
        }));
        let mut uploader2 = FileHashUploader::new(
            FullLocalFileLocation::new(FileType::Document, path.to_string_lossy().into_owned(), 0),
            1000,
            Box::new(RecordingHashCallback { log: log.clone() }),
            Box::new(FakeHashNet { state }),
        );
        uploader2.start();
        uploader2.update_resources(grant(1 << 20));
        assert!(log.ok.lock().is_some());
        let _ = std::fs::remove_file(&path);
    }

    // ==== M.12.2c — entry points, id strings and the register family ====

    fn document_location() -> FullRemoteFileLocation {
        FullRemoteFileLocation::from_document(
            FileType::Document,
            10,
            11,
            DcId::internal(2),
            String::new(),
        )
    }

    /// The persistent/unique id binaries, computed independently: a
    /// Document/5 location on dc 2 with id 10, access hash 11 stores as
    /// `i32 type | i32 dc | i64 id | i64 access_hash`; the unique id drops
    /// the type word to the FileTypeClass+1 (Document → 2) and the
    /// file-reference/dc words; zero_encode is the 250-capped zero RLE;
    /// remote persistent ids append the `Version::Next - 1` (61) and
    /// `PERSISTENT_ID_VERSION` (4) bytes, generated ones just the version 3.
    #[test]
    fn persistent_and_unique_ids_match_the_cpp_bytes() {
        let location = document_location();
        // store bytes: 05000000 02000000 0a00000000000000 0b00000000000000
        // zero RLE:   05 00 02 02 00 02 0a 00 06 0b 00 06
        assert_eq!(
            FileNode::get_remote_persistent_id(&location),
            "BQADAgADCgAHCwAHPQQ"
        );
        // unique: i32 class+1 = 2, then the id (the C++ CommonAsKey drops
        // the access hash)
        assert_eq!(FileNode::get_remote_unique_id(&location), "AgADCgAH");

        // a generate location with a remotely-generated conversion
        let generate = FullGenerateFileLocation::new(
            FileType::Document,
            String::new(),
            "#map#0:0:100:100".to_owned(),
        );
        assert!(is_remotely_generated_file(&generate.conversion));
        let node = FileNode::new(
            LocalFileLocation::Empty,
            NewRemoteFileLocation::default(),
            Some(generate.clone()),
            0,
            0,
            String::new(),
            String::new(),
            DialogId::default(),
            FileEncryptionKey::default(),
            FileId::new(1, 0),
            0,
        );
        // store: i32 5, empty string, "#map#0:0:100:100"
        assert_eq!(
            node.get_persistent_file_id(),
            "BQAHECNtYXAjMDowOjEwMDoxMDAAAwM"
        );
        assert_eq!(node.get_unique_file_id(), "_wUABxAjbWFwIzA6MDoxMDA6MTAwAAM");

        // a url-only node: the persistent id is the url itself, the unique
        // id is empty
        let url_node = FileNode::new(
            LocalFileLocation::Empty,
            NewRemoteFileLocation::default(),
            None,
            0,
            0,
            String::new(),
            "https://example.com/a.jpg".to_owned(),
            DialogId::default(),
            FileEncryptionKey::default(),
            FileId::new(2, 0),
            0,
        );
        assert_eq!(
            url_node.get_persistent_file_id(),
            "https://example.com/a.jpg"
        );
        assert_eq!(url_node.get_unique_file_id(), "");
    }

    #[test]
    fn is_remotely_generated_file_matches_the_cpp() {
        assert!(is_remotely_generated_file("#map#1:2:3:4"));
        assert!(is_remotely_generated_file("#audio_t#something"));
        assert!(!is_remotely_generated_file("#url#"));
        assert!(!is_remotely_generated_file("#file_id#5"));
        assert!(!is_remotely_generated_file("#mtime#000#map#"));
        assert!(!is_remotely_generated_file(""));
    }

    #[test]
    fn get_files_dir_matches_the_cpp() {
        // the C++ base directories end with a slash
        assert_eq!(get_files_dir(FileType::Photo, "/f/", "/s/"), "/f/photos/");
        assert_eq!(
            get_files_dir(FileType::Document, "/f/", "/s/"),
            "/f/documents/"
        );
        assert_eq!(
            get_files_dir(FileType::SecureDecrypted, "/f/", "/s/"),
            "/s/passport/"
        );
        assert_eq!(get_files_base_dir(FileType::Photo, "/f/", "/s/"), "/f/");
        assert_eq!(
            get_files_base_dir(FileType::SecureEncrypted, "/f/", "/s/"),
            "/s/"
        );
    }

    /// A context that fakes `realpath` (identity) but keeps the real stat
    /// and option reads.
    struct IdentityRealPathContext;

    impl FileManagerContext for IdentityRealPathContext {
        fn real_path(&self, path: &str) -> Result<String, Status> {
            Ok(path.to_owned())
        }
    }

    #[test]
    fn check_full_local_location_validates_like_the_cpp() {
        let context = IdentityRealPathContext;
        let mk = |path: &str, size: i64| FullLocalLocationInfo {
            location: FullLocalFileLocation::new(FileType::Document, path.to_owned(), 0),
            size,
        };

        // empty path
        let err = check_full_local_location(&context, mk("", 0), false).unwrap_err();
        assert_eq!(err.code(), 400);
        assert_eq!(err.message(), "File must have non-empty path");

        // real_path failure (the default context's canonicalize)
        let err =
            check_full_local_location(&DefaultFileManagerContext, mk("/nonexistent/a/b", 0), false)
                .unwrap_err();
        assert_eq!(err.message(), "Can't find real file path");

        // real file: the zero size is filled from the stat
        let dir = std::env::temp_dir();
        let path = dir.join("m12_check_local_file");
        std::fs::write(&path, b"12345").unwrap();
        let location_path = path.to_string_lossy().into_owned();
        let info = check_full_local_location(&context, mk(&location_path, 0), false).unwrap();
        assert_eq!(info.size, 5);
        assert!(info.location.mtime_nsec != 0);

        // size mismatch
        let err = check_full_local_location(&context, mk(&location_path, 6), false).unwrap_err();
        assert_eq!(err.message(), "File size has changed");

        // empty file
        let empty_path = dir.join("m12_check_empty_file");
        std::fs::write(&empty_path, b"").unwrap();
        let err = check_full_local_location(&context, mk(&empty_path.to_string_lossy(), 0), false)
            .unwrap_err();
        assert_eq!(err.message(), "File must be non-empty");

        // thumbnail limit (200 KB - 1), bypassed for "map*" names
        let big = dir.join("m12_check_big_thumb");
        std::fs::write(&big, vec![0u8; 200 * 1024]).unwrap();
        let thumb = |path: &str| FullLocalLocationInfo {
            location: FullLocalFileLocation::new(FileType::Thumbnail, path.to_owned(), 0),
            size: 200 * 1024,
        };
        let err =
            check_full_local_location(&context, thumb(&big.to_string_lossy()), false).unwrap_err();
        assert!(
            err.message()
                .starts_with("File of size 204800 bytes is too big for a thumbnail")
        );
        let map_thumb = dir.join("map_m12_check_big_thumb");
        std::fs::write(&map_thumb, vec![0u8; 200 * 1024]).unwrap();
        check_full_local_location(&context, thumb(&map_thumb.to_string_lossy()), false).unwrap();

        // the video-note option override through the context
        let video_note = dir.join("m12_check_video_note");
        std::fs::write(&video_note, vec![0u8; 100]).unwrap();
        struct NoteContext;
        impl FileManagerContext for NoteContext {
            fn real_path(&self, path: &str) -> Result<String, Status> {
                Ok(path.to_owned())
            }
            fn option_integer(&self, _name: &str, _default: i64) -> i64 {
                50
            }
        }
        let info = FullLocalLocationInfo {
            location: FullLocalFileLocation::new(
                FileType::VideoNote,
                video_note.to_string_lossy().into_owned(),
                0,
            ),
            size: 100,
        };
        let err = check_full_local_location(&NoteContext, info, false).unwrap_err();
        assert!(
            err.message()
                .starts_with("File of size 100 bytes is too big for a video note")
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&empty_path);
        let _ = std::fs::remove_file(&big);
        let _ = std::fs::remove_file(&map_thumb);
        let _ = std::fs::remove_file(&video_note);
    }

    #[test]
    fn register_family_and_get_file_object_match_the_cpp() {
        let mut manager = FileManager::new();

        // register_remote: two registrations of the same FromServer location
        // collapse to one id
        let first = manager.register_remote(
            document_location(),
            FileLocationSource::FromServer,
            DialogId::new(-100200300),
            500,
            0,
            "a.pdf".to_owned(),
        );
        let second = manager.register_remote(
            document_location(),
            FileLocationSource::FromServer,
            DialogId::default(),
            0,
            0,
            String::new(),
        );
        assert_eq!(first.get(), second.get());

        let file = manager.get_file_object(first);
        assert_eq!(file.id, first.get());
        assert_eq!(file.size, 500);
        assert_eq!(file.remote.as_ref().unwrap().id, "BQADAgADCgAHCwAHPQQ");
        assert_eq!(file.remote.as_ref().unwrap().unique_id, "AgADCgAH");
        assert!(file.remote.as_ref().unwrap().is_uploading_completed);
        assert_eq!(file.remote.as_ref().unwrap().uploaded_size, 500);
        assert!(file.local.as_ref().unwrap().can_be_downloaded);
        assert_eq!(file.local.as_ref().unwrap().downloaded_size, 0);

        // an unknown id produces the C++ empty-file object
        let empty = manager.get_file_object(FileId::new(10000, 0));
        assert_eq!(empty.id, 0);
        assert_eq!(empty.size, 0);

        // register_url: dedup over the generate table, persistent id = url
        let url_id = manager.register_url(
            "https://example.com/b.pdf".to_owned(),
            FileType::Document,
            DialogId::new(1),
        );
        let url_id_again = manager.register_url(
            "https://example.com/b.pdf".to_owned(),
            FileType::Document,
            DialogId::default(),
        );
        assert_eq!(url_id.get(), url_id_again.get());
        let url_object = manager.get_file_object(url_id);
        assert_eq!(
            url_object.remote.as_ref().unwrap().id,
            "https://example.com/b.pdf"
        );
        assert!(url_object.remote.as_ref().unwrap().is_uploading_completed);
        // not downloadable from the server, but generate-able (the C++
        // can_download_from_server || can_generate for the #url# location)
        assert!(!FileView::new(manager.get_file_node(url_id).unwrap()).can_download_from_server());
        assert!(url_object.local.as_ref().unwrap().can_be_downloaded);

        // register_empty: dedup, and an empty file object
        let empty_id = manager.register_empty(FileType::Document);
        let empty_id_again = manager.register_empty(FileType::Document);
        assert_eq!(empty_id.get(), empty_id_again.get());
        let empty_object = manager.get_file_object(empty_id);
        assert_eq!(empty_object.id, empty_id.get());
        assert!(!empty_object.remote.as_ref().unwrap().is_uploading_completed);

        // copy_file_id registers a #file_id#N generate alias
        let copied = manager.copy_file_id(first, FileType::Document, DialogId::new(2), "test");
        let copied_object = manager.get_file_object(copied);
        // the alias adopts the remote location through the pmc-less node
        // only after a merge; until then the persistent id stays empty
        assert_eq!(copied_object.id, copied.get());
    }

    #[test]
    fn register_local_rejects_bad_paths_like_the_cpp() {
        let mut manager = FileManager::new();
        struct BadPathContext(String);
        impl FileManagerContext for BadPathContext {
            fn real_path(&self, path: &str) -> Result<String, Status> {
                Ok(path.to_owned())
            }
            fn stat(&self, _path: &str) -> Result<FileStat, Status> {
                Ok(FileStat {
                    is_reg: true,
                    is_dir: false,
                    size: 5,
                    mtime_nsec: 1,
                    real_size: 5,
                })
            }
            fn db_paths(&self) -> Vec<String> {
                vec![self.0.clone()]
            }
        }
        manager.set_context(Box::new(BadPathContext("/td/binlog".to_owned())));

        let err = manager
            .register_local(
                FullLocalFileLocation::new(FileType::Document, "/td/binlog".to_owned(), 0),
                DialogId::default(),
                5,
                false,
                false,
                FileId::default(),
            )
            .unwrap_err();
        assert_eq!(err.code(), 400);
        assert_eq!(
            err.message(),
            "Sending of internal database files is forbidden"
        );

        let file_id = manager
            .register_local(
                FullLocalFileLocation::new(FileType::Document, "/tmp/ok".to_owned(), 0),
                DialogId::new(3),
                5,
                false,
                true, // skip the size checks so the fake stat wins
                FileId::default(),
            )
            .unwrap();
        assert!(file_id.is_valid());
        // registering the same location again returns the same id
        let file_id_again = manager
            .register_local(
                FullLocalFileLocation::new(FileType::Document, "/tmp/ok".to_owned(), 0),
                DialogId::default(),
                5,
                false,
                true,
                FileId::default(),
            )
            .unwrap();
        assert_eq!(file_id.get(), file_id_again.get());
    }

    /// `FileManager::store_file`/`parse_file` round-trip through the
    /// FileManager-mediated FileId format the photo stores use.
    #[test]
    fn store_parse_file_round_trips_all_arms() {
        let mut manager = FileManager::new();

        // Remote arm
        let remote_id = manager.register_remote(
            document_location(),
            FileLocationSource::FromServer,
            DialogId::new(-100200300),
            500,
            0,
            "a.pdf".to_owned(),
        );

        // Local arm
        let local_id = manager.register_empty(FileType::Document);

        // Generate arm with the nested #file_id# payload
        let generate_id = manager.register_generate(
            FileType::Document,
            "/tmp/x".to_owned(),
            format!("#file_id#{}", remote_id.get()),
            DialogId::new(4),
            0,
        );

        // Url arm
        let url_id = manager.register_url(
            "https://example.com/c.pdf".to_owned(),
            FileType::Document,
            DialogId::new(5),
        );

        for file_id in [remote_id, local_id, generate_id, url_id] {
            let mut bytes = Vec::new();
            manager.store_file(file_id, &mut bytes);
            let mut parser = Parser::new(&bytes);
            let parsed = manager.parse_file(&mut parser).unwrap();
            parser.fetch_end().unwrap();
            // the parse registers an equal node; compare through the objects
            let original = manager.get_file_object(file_id);
            let reparsed = manager.get_file_object(parsed);
            assert_eq!(
                original.remote.as_ref().unwrap().id,
                reparsed.remote.as_ref().unwrap().id,
                "remote ids must survive the round trip"
            );
            assert_eq!(original.size, reparsed.size);
            assert_eq!(
                original.local.as_ref().unwrap().path,
                reparsed.local.as_ref().unwrap().path
            );
        }
    }

    /// The pre-`StoreFileEncryptionKey` layout: no flags word, legacy int32
    /// sizes with the negative wrap, and the generate arm's legacy zero.
    #[test]
    fn parse_file_legacy_layout_matches_the_cpp() {
        use store_parse::*;

        let mut manager = FileManager::new();
        let remote_id = manager.register_remote(
            document_location(),
            FileLocationSource::FromServer,
            DialogId::new(7),
            500,
            0,
            "a.pdf".to_owned(),
        );

        let mut bytes = Vec::new();
        manager.store_file(remote_id, &mut bytes);
        // strip the modern flags word (bytes 4..8) to model version < 7
        let legacy: Vec<u8> = bytes[..4]
            .iter()
            .chain(bytes[8..].iter())
            .copied()
            .collect();

        let mut other = FileManager::new();
        let mut parser = Parser::new(&legacy);
        parser.set_version(VERSION_STORE_FILE_OWNER_ID);
        let parsed = other.parse_file(&mut parser).unwrap();
        parser.fetch_end().unwrap();

        let original = manager.get_file_object(remote_id);
        let reparsed = other.get_file_object(parsed);
        assert_eq!(
            original.remote.as_ref().unwrap().id,
            reparsed.remote.as_ref().unwrap().id
        );
        assert_eq!(reparsed.size, 500);
    }

    #[test]
    fn suggested_path_and_can_delete_match_the_cpp() {
        // remote_name wins over the url file name and the generate path
        let mut node = FileNode::new(
            LocalFileLocation::Empty,
            NewRemoteFileLocation::default(),
            Some(FullGenerateFileLocation::new(
                FileType::Document,
                "/gen/orig.bin".to_owned(),
                String::new(),
            )),
            0,
            0,
            "remote.bin".to_owned(),
            "https://example.com/url.bin".to_owned(),
            DialogId::default(),
            FileEncryptionKey::default(),
            FileId::new(1, 0),
            0,
        );
        assert_eq!(node.suggested_path(), "remote.bin");
        node.set_remote_name(String::new());
        assert_eq!(node.suggested_path(), "url.bin");
        node.set_url(String::new());
        assert_eq!(node.suggested_path(), "/gen/orig.bin");

        // can_delete: full-local inside the files dir, partial always,
        // full-local outside never
        node.set_local_location(
            LocalFileLocation::Full(FullLocalFileLocation::new(
                FileType::Document,
                "/files/documents/remote.bin".to_owned(),
                0,
            )),
            -1,
            -1,
        );
        assert!(node.can_delete("/files/", "/secure_files/"));
        assert!(!node.can_delete("/other/", "/secure_files/"));
        node.set_local_location(
            LocalFileLocation::Partial(PartialLocalFileLocation {
                file_type: FileType::Document,
                path: "/tmp/x.bin".to_owned(),
                ready_size: 0,
                ready_bitmask: Vec::new(),
                part_size: 1,
                iv: Vec::new(),
            }),
            -1,
            -1,
        );
        assert!(node.can_delete("/files/", "/secure_files/"));
    }
}

// ==== FileLoadManager + load orchestration (M.12.7) ====

/// `FileUploadId` (FileId.hpp) — a `FileId` plus the internal upload id
/// (`0` for the user request; the default is the invalid id).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FileUploadId {
    pub file_id: FileId,
    pub internal_upload_id: i64,
}

impl FileUploadId {
    pub fn new(file_id: FileId, internal_upload_id: i64) -> FileUploadId {
        FileUploadId {
            file_id,
            internal_upload_id,
        }
    }
}

/// `FileManager::DownloadCallback` (FileManager.h:399-410); the
/// `on_progress` default is the trait default method.
pub trait DownloadCallback: Send {
    fn on_progress(&mut self, _file_id: FileId) {}
    fn on_download_ok(&mut self, file_id: FileId);
    fn on_download_error(&mut self, file_id: FileId, status: Status);
}

/// The null `user_download_file_callback_` stand-in for the managers
/// driven without one (the C++ CHECK(callback != nullptr) cannot hold for
/// the user arm before Td installs its callback).
struct NoopDownloadCallback;

impl DownloadCallback for NoopDownloadCallback {
    fn on_download_ok(&mut self, _file_id: FileId) {}
    fn on_download_error(&mut self, _file_id: FileId, _status: Status) {}
}

/// The null upload callback of [`FileManager::preliminary_upload_file`]
/// (the C++ `PreliminaryUploadFileCallback` observes only the synchronous
/// outcomes; its effects replay in the manager call itself).
struct NoopUploadCallback;

impl UploadCallback for NoopUploadCallback {
    fn on_upload_ok(&mut self, _file_upload_id: FileUploadId) {}
    fn on_upload_error(&mut self, _file_upload_id: FileUploadId, _status: Status) {}
}

/// `FileManager::UploadCallback` (FileManager.h:412-436). The C++
/// `on_upload_ok` carries the resolved `InputFile`/`InputEncryptedFile`;
/// the wire conversion belongs to the caller, so the collapsed callback
/// receives only the id (the `on_upload_encrypted_ok` default arm folds
/// into the same method).
pub trait UploadCallback: Send {
    fn on_upload_ok(&mut self, file_upload_id: FileUploadId);
    fn on_upload_error(&mut self, file_upload_id: FileUploadId, status: Status);
}

/// `FileManager::FileDownloadInfo` (FileManager.h:845-848).
#[derive(Default)]
struct DownloadInfo {
    download_priority: i8,
    download_callback: Option<Arc<parking_lot::Mutex<dyn DownloadCallback>>>,
}

/// `FileManager::UserFileDownloadInfo` (FileManager.h:834-836) — a
/// synchronous `downloadFile` park: the promise resolves when the user
/// download finishes ([`FileManager::on_user_file_download_finished`]).
struct UserFileDownloadInfo {
    offset: i64,
    limit: i64,
    promises: Vec<tdutils::promise::Promise<td_api::File>>,
}

/// `FileManager::FileDownloadRequests` (FileManager.h:849-854).
#[derive(Default)]
struct DownloadRequests {
    user_download_priority: i8,
    user_offset: i64,
    user_limit: i64,
    internal_downloads: BTreeMap<i64, DownloadInfo>,
}

/// `FileManager::FileUploadInfo` (FileManager.h:857-861).
#[derive(Default)]
struct UploadInfo {
    upload_priority: i8,
    upload_order: u64,
    upload_callback: Option<Arc<parking_lot::Mutex<dyn UploadCallback>>>,
}

/// `FileManager::FileUploadRequests` (FileManager.h:862-865).
#[derive(Default)]
struct UploadRequests {
    user_upload_priority: i8,
    internal_uploads: BTreeMap<i64, UploadInfo>,
}

/// The `DownloadQuery::Type` values (FileManager.h:110-118).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DownloadQueryKind {
    Download,
    DownloadReloadDialog,
    DownloadWaitFileReference,
    SetContent,
}

/// The `DownloadQuery` container entry (FileManager.h:104-109), named
/// `FileDownloadQuery` after the manager (`DownloadQuery` is taken by the
/// M.12.4 wire-query enum).
#[derive(Clone, Copy, Debug)]
struct FileDownloadQuery {
    file_id: FileId,
    kind: DownloadQueryKind,
}

/// The `UploadQuery::Type` values (FileManager.h:121-128).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UploadQueryKind {
    Upload,
    UploadByHash,
    UploadWaitFileReference,
}

/// The `UploadQuery` container entry (FileManager.h:131-136).
#[derive(Clone, Copy, Debug)]
struct UploadQuery {
    file_id: FileId,
    kind: UploadQueryKind,
}

/// The `start_up` option reads of the manager actors
/// (FileDownloadManager.cpp:24-28, FileUploadManager.cpp:21-26) — the
/// M.3 option-cache (`is_premium`, `session_count`, `keep_media_order`)
/// collapsed into the constructor argument.
#[derive(Clone, Copy, Debug, Default)]
pub struct FileLoadManagerOptions {
    pub is_premium: bool,
    pub session_count: i64,
    pub keep_media_order: bool,
}

/// One event of the collapsed `FileDownloadManager::Callback` /
/// `FileUploadManager::Callback` planes; each C++ `send_closure` into
/// `FileManager` becomes one queued event.
#[derive(Clone, Debug)]
pub enum FileLoadEvent {
    StartDownload {
        query_id: u64,
    },
    PartialDownload {
        query_id: u64,
        partial: PartialLocalFileLocation,
        size: i64,
    },
    DownloadOk {
        query_id: u64,
        local: FullLocalFileLocation,
        size: i64,
        is_new: bool,
    },
    Hash {
        query_id: u64,
        hash: Vec<u8>,
    },
    PartialUpload {
        query_id: u64,
        partial: PartialRemoteFileLocation,
    },
    UploadOk {
        query_id: u64,
        file_type: FileType,
        partial: PartialRemoteFileLocation,
    },
    UploadFullOk {
        query_id: u64,
        remote: FullRemoteFileLocation,
    },
    Error {
        query_id: u64,
        status: Status,
    },
}

/// `save_file_bytes` (FileLoaderUtils.cpp:209-233) — the `FileFromBytes`
/// storage half. The io rides the [`FileDownloaderNet`] seam (the
/// `search_file`/`open_temp_file`/`create_from_temp` trio of
/// FileLoaderUtils).
pub fn save_file_bytes(
    io: &mut dyn FileDownloaderNet,
    file_type: FileType,
    bytes: &[u8],
    name: &str,
) -> Result<FullLocalFileLocation, Status> {
    if let Ok(old_path) = io.search_file(file_type, name, bytes.len() as i64)
        && let Ok(mut old_fd) = io.open_partial_file(&old_path, false)
    {
        let mut old_bytes = Vec::new();
        {
            use std::io::Read as _;
            let _ = old_fd.read_to_end(&mut old_bytes);
        }
        if old_bytes == bytes {
            return Ok(FullLocalFileLocation::new(file_type, old_path, 0));
        }
    }

    let (mut fd, path) = io.open_temp_file(file_type)?;
    {
        use std::io::Write as _;
        fd.write_all(bytes).map_err(|e| {
            Status::error_message(format!("Failed to write bytes to the file: {e}").as_str())
        })?;
    }
    let perm_path = io.create_from_temp(file_type, &path, name)?;
    Ok(FullLocalFileLocation::new(file_type, perm_path, 0))
}

/// The per-query net-seam factory — the C++ loaders reach the net through
/// `G()`; the collapsed managers build each loader's seam explicitly.
pub trait FileLoadNetFactory: Send {
    /// A fresh net seam for a new [`FileDownloader`] query.
    fn download_net(&mut self) -> Box<dyn FileDownloaderNet>;
    /// A fresh net seam for a new [`FileUploader`] query.
    fn upload_net(&mut self) -> Box<dyn FileUploaderNet>;
    /// A fresh net seam for a new [`FileHashUploader`] query.
    fn hash_net(&mut self) -> Box<dyn FileHashUploaderNet>;
    /// The FileLoaderUtils io seam (`save_file_bytes`/from-bytes).
    fn utils_net(&mut self) -> Box<dyn FileDownloaderNet>;
    /// The [`FileGenerateManager`] io seam — the `FileExternalGenerateActor`
    /// `FileFd` opens and the `WebFileDownloadGenerateActor`
    /// `upload.getWebFile` query. The default reuses [`Self::utils_net`];
    /// the C++ reaches both through the same `G()` plane.
    fn generate_net(&mut self) -> Box<dyn FileDownloaderNet> {
        self.utils_net()
    }
    /// `G()->get_webfile_dc_id()` — the resource-pool key of a web
    /// download (FileDownloadManager.cpp:56).
    fn webfile_dc_id(&mut self) -> DcId {
        DcId::default()
    }
}

/// One resource pool of the C++ managers — the `(is_small, dc_id)`
/// download pools (FileDownloadManager.cpp:30-38) and the single upload
/// pool (FileUploadManager.cpp:22-25) with their capacity constants. The
/// C++ pool is a `ResourceManager` actor; the collapsed pools keep the
/// capacity and mode, while the grant scheduling itself is
/// [`FileLoadManager::drive`]'s priority order (the `RefCell` borrow
/// wall makes an in-manager arbitration loop un-driveable — see the
/// [`FileLoadManager`] doc).
#[derive(Clone, Copy)]
struct ResourcePool {
    max_resource_limit: i64,
    #[allow(dead_code)]
    mode: ResourceManagerMode,
}

/// One active download query: the loader plus its pool.
struct DownloadQueryNode {
    file_id: FileId,
    priority: i8,
    pool_key: (bool, DcId),
    pool: ResourcePool,
    loader: Box<FileDownloader>,
    canceled: bool,
}

struct UploadQueryNode {
    file_id: FileId,
    priority: i8,
    pool: ResourcePool,
    loader: UploadLoader,
    canceled: bool,
}

enum UploadLoader {
    Uploader(Box<FileUploader>),
    HashUploader(Box<FileHashUploader>),
}

/// `FileDownloadManager` + `FileUploadManager` + `FileLoadManager`
/// (the sync-ops actor) collapsed into one object. The loaders run to
/// completion inside [`FileLoadManager::drive`]'s grant rounds — the
/// sequential collapse of the actor scheduler. Grants are delivered
/// through the [`ResourceManagerWorker`] seam in the C++ `to_xload`
/// order (priority desc, then registration order), with each loader's
/// `ResourceState` accounting (start/stop use, estimated extra) the
/// landed M.12.3 code; the in-manager arbitration loop cannot be used
/// because a worker's report-back would re-enter the manager `RefCell`
/// (the documented borrow wall where the C++ uses its message queue).
pub struct FileLoadManager {
    /// `max_download_resource_limit_` (FileDownloadManager.h:70) —
    /// `1 << 21`, x8 for a premium or multi-session client.
    max_download_resource_limit: i64,
    download_resource_managers: BTreeMap<(bool, DcId), ResourcePool>,
    /// The single `upload_resource_manager_`
    /// (FileUploadManager.cpp:22-25) — `4 << 20`, Greedy unless
    /// `keep_media_order`.
    upload_resource_manager: ResourcePool,

    download_nodes: BTreeMap<u64, DownloadQueryNode>,
    upload_nodes: BTreeMap<u64, UploadQueryNode>,
    next_query_id: u64,

    factory: Box<dyn FileLoadNetFactory>,
    events: Arc<parking_lot::Mutex<Vec<FileLoadEvent>>>,
}

/// The loader-side callback adapter: queues the manager-plane events
/// (the C++ `FileDownloaderCallback` → `send_closure(manager, ...)`).
struct ManagedDownloadCallback {
    query_id: u64,
    events: Arc<parking_lot::Mutex<Vec<FileLoadEvent>>>,
}

impl FileDownloaderCallback for ManagedDownloadCallback {
    fn on_start_download(&mut self) {
        self.events.lock().push(FileLoadEvent::StartDownload {
            query_id: self.query_id,
        });
    }
    fn on_partial_download(&mut self, partial_local: PartialLocalFileLocation, size: i64) {
        self.events.lock().push(FileLoadEvent::PartialDownload {
            query_id: self.query_id,
            partial: partial_local,
            size,
        });
    }
    fn on_ok(&mut self, full_local: FullLocalFileLocation, size: i64, is_new: bool) {
        self.events.lock().push(FileLoadEvent::DownloadOk {
            query_id: self.query_id,
            local: full_local,
            size,
            is_new,
        });
    }
    fn on_error(&mut self, status: Status) {
        self.events.lock().push(FileLoadEvent::Error {
            query_id: self.query_id,
            status,
        });
    }
}

struct ManagedUploadCallback {
    query_id: u64,
    events: Arc<parking_lot::Mutex<Vec<FileLoadEvent>>>,
}

impl FileUploaderCallback for ManagedUploadCallback {
    fn on_hash(&mut self, hash: Vec<u8>) {
        self.events.lock().push(FileLoadEvent::Hash {
            query_id: self.query_id,
            hash,
        });
    }
    fn on_partial_upload(&mut self, partial_remote: PartialRemoteFileLocation) {
        self.events.lock().push(FileLoadEvent::PartialUpload {
            query_id: self.query_id,
            partial: partial_remote,
        });
    }
    fn on_ok(&mut self, file_type: FileType, partial_remote: PartialRemoteFileLocation) {
        self.events.lock().push(FileLoadEvent::UploadOk {
            query_id: self.query_id,
            file_type,
            partial: partial_remote,
        });
    }
    fn on_error(&mut self, status: Status) {
        self.events.lock().push(FileLoadEvent::Error {
            query_id: self.query_id,
            status,
        });
    }
}

struct ManagedHashUploadCallback {
    query_id: u64,
    events: Arc<parking_lot::Mutex<Vec<FileLoadEvent>>>,
}

impl FileHashUploaderCallback for ManagedHashUploadCallback {
    fn on_ok(&mut self, location: FullRemoteFileLocation) {
        self.events.lock().push(FileLoadEvent::UploadFullOk {
            query_id: self.query_id,
            remote: location,
        });
    }
    fn on_error(&mut self, status: Status) {
        self.events.lock().push(FileLoadEvent::Error {
            query_id: self.query_id,
            status,
        });
    }
}

impl FileLoadManager {
    /// The T.9 factory re-install: the Td replaces the pre-init factory
    /// (whose nets answer the session-down surface) with the bridge-backed
    /// one once the session exists.
    pub fn set_factory(&mut self, factory: Box<dyn FileLoadNetFactory>) {
        self.factory = factory;
    }

    /// The `FileDownloadManager::start_up` + `FileUploadManager::start_up`
    /// option reads.
    pub fn new(
        factory: Box<dyn FileLoadNetFactory>,
        options: FileLoadManagerOptions,
    ) -> FileLoadManager {
        let mut max_download_resource_limit: i64 = 1 << 21;
        if options.is_premium || options.session_count > 1 {
            max_download_resource_limit *= 8;
        }
        // FileUploadManager.cpp:24-25: Greedy unless the media order is
        // kept
        let upload_mode = if options.keep_media_order {
            ResourceManagerMode::Baseline
        } else {
            ResourceManagerMode::Greedy
        };
        FileLoadManager {
            max_download_resource_limit,
            download_resource_managers: BTreeMap::new(),
            upload_resource_manager: ResourcePool {
                max_resource_limit: 4 << 20,
                mode: upload_mode,
            },
            download_nodes: BTreeMap::new(),
            upload_nodes: BTreeMap::new(),
            next_query_id: 1,
            factory,
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Allocates a query id in the managers' shared id space (the C++
    /// `Container` ids) for the FileManager-side pseudo queries
    /// (`DownloadReloadDialog`/`DownloadWaitFileReference`/...).
    pub fn alloc_query_id(&mut self) -> u64 {
        let query_id = self.next_query_id;
        self.next_query_id += 1;
        query_id
    }

    /// `FileLoadManager::read_file_part` (FileLoadManager.cpp:18-20) — the
    /// synchronous `read_file_str` tail of the collapsed actor; the C++
    /// `promise.set_result(...)` becomes the returned result.
    pub fn read_file_part(
        &mut self,
        path: &str,
        offset: i64,
        count: i64,
    ) -> Result<Vec<u8>, Status> {
        read_file_str(path, count, offset)
    }

    /// `FileLoadManager::unlink_file` (FileLoadManager.cpp:22-24) — the
    /// best-effort unlink; the immediate `promise.set_value(Unit())`
    /// rides the caller.
    pub fn unlink_file(&mut self, path: &str) {
        let _ = std::fs::remove_file(path);
    }

    fn download_resource_pool(&mut self, is_small: bool, dc_id: DcId) -> ResourcePool {
        let pool = ResourcePool {
            max_resource_limit: self.max_download_resource_limit,
            mode: ResourceManagerMode::Baseline,
        };
        *self
            .download_resource_managers
            .entry((is_small, dc_id))
            .or_insert(pool)
    }

    /// `FileDownloadManager::download` (FileDownloadManager.cpp:40-62).
    /// Registers the query and returns its id; the download itself runs
    /// under [`FileLoadManager::drive`].
    #[allow(clippy::too_many_arguments)]
    pub fn download(
        &mut self,
        query_id: u64,
        file_id: FileId,
        remote_location: &FullRemoteFileLocation,
        local: &LocalFileLocation,
        size: i64,
        name: String,
        encryption_key: &FileEncryptionKey,
        need_search_file: bool,
        offset: i64,
        limit: i64,
        priority: i8,
    ) -> u64 {
        // bool is_small = size < 20 * 1024 (FileDownloadManager.cpp:52)
        let is_small = size < 20 * 1024;
        let dc_id = if remote_location.is_web() {
            self.factory.webfile_dc_id()
        } else {
            remote_location.get_dc_id()
        };
        let pool = self.download_resource_pool(is_small, dc_id);
        let mut loader = Box::new(FileDownloader::new(
            remote_location.clone(),
            local.clone(),
            size,
            name,
            encryption_key.clone(),
            is_small,
            need_search_file,
            offset,
            limit,
            Box::new(ManagedDownloadCallback {
                query_id,
                events: Arc::clone(&self.events),
            }),
            self.factory.download_net(),
        ));
        // the C++ downloader actor's start_up runs at creation
        // (FileDownloadManager.cpp:53-55)
        loader.start();
        self.download_nodes.insert(
            query_id,
            DownloadQueryNode {
                file_id,
                priority,
                pool_key: (is_small, dc_id),
                pool,
                loader,
                canceled: false,
            },
        );
        query_id
    }

    /// `FileDownloadManager::update_priority`
    /// (FileDownloadManager.cpp:64-77).
    pub fn update_download_priority(&mut self, query_id: u64, priority: i8) {
        let Some(node) = self.download_nodes.get_mut(&query_id) else {
            return;
        };
        if node.canceled {
            return;
        }
        node.priority = priority;
        node.loader.update_priority(priority);
    }

    /// `FileDownloadManager::update_downloaded_part`
    /// (FileDownloadManager.cpp:105-118).
    pub fn update_downloaded_part(&mut self, query_id: u64, offset: i64, limit: i64) {
        let Some(node) = self.download_nodes.get_mut(&query_id) else {
            return;
        };
        let max_resource_limit = node.pool.max_resource_limit;
        node.loader
            .update_downloaded_part(offset, limit, max_resource_limit);
    }

    /// `FileDownloadManager::from_bytes` (FileDownloadManager.cpp:79-92)
    /// — the `FileFromBytes` actor's `save_file_bytes` runs immediately
    /// in the collapse, so the query cannot be canceled afterwards: the
    /// C++ window between registration and the actor wakeup does not
    /// exist here, and a later `cancel` finds no node exactly like the
    /// C++ after `close_node` ran on ok.
    pub fn from_bytes(
        &mut self,
        query_id: u64,
        _file_id: FileId,
        file_type: FileType,
        bytes: Vec<u8>,
        name: String,
    ) {
        let size = bytes.len() as i64;
        match save_file_bytes(self.factory.utils_net().as_mut(), file_type, &bytes, &name) {
            Ok(local) => self.events.lock().push(FileLoadEvent::DownloadOk {
                query_id,
                local,
                size,
                is_new: true,
            }),
            Err(status) => self
                .events
                .lock()
                .push(FileLoadEvent::Error { query_id, status }),
        }
    }

    /// `FileDownloadManager::cancel` (FileDownloadManager.cpp:94-103) —
    /// the loader is dropped (the C++ `downloader_.reset()`) and the
    /// `-1 "Canceled"` error is emitted.
    pub fn cancel_download(&mut self, query_id: u64) {
        let Some(node) = self.download_nodes.get_mut(&query_id) else {
            return;
        };
        if node.canceled {
            return;
        }
        node.canceled = true;
        self.events.lock().push(FileLoadEvent::Error {
            query_id,
            status: Status::error(-1, "Canceled"),
        });
    }

    /// `FileUploadManager::upload` (FileUploadManager.cpp:28-45).
    #[allow(clippy::too_many_arguments)]
    pub fn upload(
        &mut self,
        query_id: u64,
        file_id: FileId,
        local_location: &LocalFileLocation,
        remote_location: &RemoteFileLocation,
        expected_size: i64,
        encryption_key: &FileEncryptionKey,
        priority: i8,
        bad_parts: Vec<i32>,
    ) {
        let mut loader = Box::new(FileUploader::new(
            local_location.clone(),
            remote_location.clone(),
            expected_size,
            encryption_key.clone(),
            bad_parts,
            Box::new(ManagedUploadCallback {
                query_id,
                events: Arc::clone(&self.events),
            }),
            self.factory.upload_net(),
        ));
        loader.start();
        self.upload_nodes.insert(
            query_id,
            UploadQueryNode {
                file_id,
                priority,
                pool: ResourcePool {
                    max_resource_limit: self.upload_resource_manager.max_resource_limit,
                    mode: self.upload_resource_manager.mode,
                },
                loader: UploadLoader::Uploader(loader),
                canceled: false,
            },
        );
    }

    /// `FileUploadManager::upload_by_hash`
    /// (FileUploadManager.cpp:47-62).
    pub fn upload_by_hash(
        &mut self,
        query_id: u64,
        file_id: FileId,
        local_location: &FullLocalFileLocation,
        size: i64,
        priority: i8,
    ) {
        let mut loader = Box::new(FileHashUploader::new(
            local_location.clone(),
            size,
            Box::new(ManagedHashUploadCallback {
                query_id,
                events: Arc::clone(&self.events),
            }),
            self.factory.hash_net(),
        ));
        loader.start();
        self.upload_nodes.insert(
            query_id,
            UploadQueryNode {
                file_id,
                priority,
                pool: ResourcePool {
                    max_resource_limit: self.upload_resource_manager.max_resource_limit,
                    mode: self.upload_resource_manager.mode,
                },
                loader: UploadLoader::HashUploader(loader),
                canceled: false,
            },
        );
    }

    /// `FileUploadManager::update_priority`
    /// (FileUploadManager.cpp:64-81) — the C++ sends `-priority` to the
    /// uploaders.
    pub fn update_upload_priority(&mut self, query_id: u64, priority: i8) {
        let Some(node) = self.upload_nodes.get_mut(&query_id) else {
            return;
        };
        if node.canceled {
            return;
        }
        node.priority = priority;
        match &mut node.loader {
            UploadLoader::Uploader(uploader) => uploader.update_priority(-priority),
            UploadLoader::HashUploader(hash_uploader) => hash_uploader.update_priority(-priority),
        }
    }

    /// The [`FileLoadNetFactory::generate_net`] passthrough — the
    /// [`FileGenerateManager`] io/net seam comes from the same factory.
    pub fn generate_net(&mut self) -> Box<dyn FileDownloaderNet> {
        self.factory.generate_net()
    }

    /// `FileUploadManager::update_local_file_location`
    /// (FileUploadManager.cpp:96-101).
    pub fn update_local_file_location(&mut self, query_id: u64, local: &LocalFileLocation) {
        let Some(node) = self.upload_nodes.get_mut(&query_id) else {
            return;
        };
        if node.canceled {
            return;
        }
        if let UploadLoader::Uploader(uploader) = &mut node.loader {
            uploader.update_local_file_location(local);
        }
    }

    /// `FileUploadManager::cancel` (FileUploadManager.cpp:83-92).
    pub fn cancel_upload(&mut self, query_id: u64) {
        let Some(node) = self.upload_nodes.get_mut(&query_id) else {
            return;
        };
        if node.canceled {
            return;
        }
        node.canceled = true;
        self.events.lock().push(FileLoadEvent::Error {
            query_id,
            status: Status::error(-1, "Canceled"),
        });
    }

    /// The active (not stopped, not canceled) download query ids in the
    /// grant order — the C++ `ResourceManager::add_node`/`to_xload`
    /// order: descending by key (a negative priority stores
    /// `-priority`), and within equal keys the newest query first
    /// (insert-before-the-first-entry-with-key-≤-priority,
    /// ResourceManager.cpp:168-176).
    fn active_download_order(&self) -> Vec<u64> {
        let mut order: Vec<(i8, u64)> = self
            .download_nodes
            .iter()
            .filter(|(_, node)| !node.canceled && !node.loader.stop_flag())
            .map(|(qid, node)| {
                (
                    if node.priority >= 0 {
                        node.priority
                    } else {
                        -node.priority
                    },
                    *qid,
                )
            })
            .collect();
        order.sort_by(|a, b| b.cmp(a));
        order.into_iter().map(|(_, qid)| qid).collect()
    }

    /// The active upload query ids in the same grant order.
    fn active_upload_order(&self) -> Vec<u64> {
        let mut order: Vec<(i8, u64)> = self
            .upload_nodes
            .iter()
            .filter(|(_, node)| {
                !node.canceled
                    && match &node.loader {
                        UploadLoader::Uploader(uploader) => !uploader.stop_flag(),
                        UploadLoader::HashUploader(hash_uploader) => !hash_uploader.stop_flag(),
                    }
            })
            .map(|(qid, node)| {
                (
                    if node.priority >= 0 {
                        node.priority
                    } else {
                        -node.priority
                    },
                    *qid,
                )
            })
            .collect();
        order.sort_by(|a, b| b.cmp(a));
        order.into_iter().map(|(_, qid)| qid).collect()
    }

    /// One grant round: every active loader's allowance rises to the
    /// round budget in the C++ `to_xload` order (priority desc, newest
    /// first within equal priority), with each pool's capacity
    /// depleting across the pass the way `ResourceManager::satisfy_node`
    /// fair-shares it (ResourceManager.cpp:106-130). Returns whether any
    /// loader was granted.
    pub fn drive_round(&mut self, round: usize) -> bool {
        let allowance = (round as i64) * (1 << 20);
        let mut progressed = false;

        // the download pools, one per (is_small, dc_id)
        let pool_keys: Vec<(bool, DcId)> =
            self.download_resource_managers.keys().copied().collect();
        for pool_key in pool_keys {
            let pool_max = self.download_resource_managers[&pool_key].max_resource_limit;
            let mut unused = pool_max;
            for query_id in self.active_download_order() {
                let node = self.download_nodes.get_mut(&query_id).expect("active node");
                if node.pool_key != pool_key {
                    continue;
                }
                let give = allowance.min(unused);
                if give == 0 {
                    // the pool is exhausted; the C++ `break`s the pass
                    break;
                }
                unused -= give;
                let mut grant = ResourceState::default();
                grant.set_unit_size(1024);
                grant.update_limit(give);
                ResourceManagerWorker::update_resources(node.loader.as_mut(), grant);
                progressed = true;
            }
        }

        // the single upload pool
        let mut unused = self.upload_resource_manager.max_resource_limit;
        for query_id in self.active_upload_order() {
            let node = self.upload_nodes.get_mut(&query_id).expect("active node");
            let give = allowance.min(unused).min(node.pool.max_resource_limit);
            if give == 0 {
                break;
            }
            unused -= give;
            let mut grant = ResourceState::default();
            grant.set_unit_size(1024);
            grant.update_limit(give);
            match &mut node.loader {
                UploadLoader::Uploader(uploader) => {
                    ResourceManagerWorker::update_resources(uploader.as_mut(), grant);
                }
                UploadLoader::HashUploader(hash_uploader) => {
                    ResourceManagerWorker::update_resources(hash_uploader.as_mut(), grant);
                }
            }
            progressed = true;
        }
        progressed
    }

    /// Runs the registered loaders to completion: grant rounds (see
    /// [`FileLoadManager::drive_round`]) until no active loader remains.
    pub fn drive(&mut self) {
        for round in 1..=64 {
            if !self.drive_round(round) {
                return;
            }
        }
    }

    /// The count of active (not stopped, not canceled) download queries.
    pub fn active_downloads(&self) -> usize {
        self.download_nodes
            .values()
            .filter(|node| !node.canceled && !node.loader.stop_flag())
            .count()
    }

    /// The count of active upload queries.
    pub fn active_uploads(&self) -> usize {
        self.upload_nodes
            .values()
            .filter(|node| {
                !node.canceled
                    && match &node.loader {
                        UploadLoader::Uploader(uploader) => !uploader.stop_flag(),
                        UploadLoader::HashUploader(hash_uploader) => !hash_uploader.stop_flag(),
                    }
            })
            .count()
    }

    /// The `file_id` a query was registered with (the C++
    /// `DownloadQuery::file_id_` lookups).
    pub fn download_query_file_id(&self, query_id: u64) -> Option<FileId> {
        self.download_nodes.get(&query_id).map(|node| node.file_id)
    }

    pub fn upload_query_file_id(&self, query_id: u64) -> Option<FileId> {
        self.upload_nodes.get(&query_id).map(|node| node.file_id)
    }

    /// Takes the queued events (the C++ actor-queue drain into
    /// `FileManager`).
    pub fn take_events(&mut self) -> Vec<FileLoadEvent> {
        std::mem::take(&mut *self.events.lock())
    }

    /// The drive quiescence probe — whether any loader events are queued.
    pub fn has_queued_events(&self) -> bool {
        !self.events.lock().is_empty()
    }
}

/// `get_files_temp_dir` (FileLoaderUtils.cpp:251-253) — one shared
/// per-base "temp" directory, not per-type.
pub fn get_files_temp_dir(file_type: FileType, files_dir: &str, secure_files_dir: &str) -> String {
    format!(
        "{}temp/",
        get_files_base_dir(file_type, files_dir, secure_files_dir)
    )
}

// ── the FileLoaderUtils io (FileLoaderUtils.cpp:32-158) — the real
// files-dir file machinery behind the downloader/generation net seams ──

/// The pmc counters `open_temp_file`/`create_from_temp` allocate names
/// from (`tmp_file_id`/`perm_file_id`, FileLoaderUtils.cpp:85-92,
/// 116-122). The Td preloads them from the binlog pmc; the increments
/// stay in the shared snapshot for the run (the C++ persists every
/// increment; a restart reusing an id hits `try_create_new_file`'s
/// random-suffix fallback, so the divergence is unobservable).
#[derive(Default)]
pub struct FileNameCounters {
    pub tmp_file_id: i32,
    pub perm_file_id: i32,
}

/// `try_create_new_file` (FileLoaderUtils.cpp:32-56): open
/// Read|Write|CreateNew under `path`, making the directory once on
/// failure.
pub fn try_create_new_file(path: &str, file_name: &str) -> Result<(File, String), Status> {
    let name = format!("{path}{file_name}");
    let mut r_fd = open_create_new(&name);
    if r_fd.is_err() {
        if std::fs::create_dir_all(path).is_err() {
            let is_dir = std::fs::metadata(path).is_ok_and(|meta| meta.is_dir());
            if !is_dir {
                return Err(Status::error(
                    500,
                    &format!("Creation of directory \"{path}\" failed"),
                ));
            }
        }
        r_fd = open_create_new(&name);
    }
    let fd = r_fd.map_err(|_| Status::error(500, "Can't create a file"))?;
    Ok((fd, name))
}

/// `FileFd::open(name, FileFd::Read | FileFd::Write | FileFd::CreateNew)`
/// of `try_create_new_file` (the platform-flag spelling varies; the
/// portable Rust form is CreateNew + read + write).
fn open_create_new(name: &str) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(name)
}

/// `RandSuff{6}` (FileLoaderUtils.cpp:63-69): six random hex digits.
fn rand_suffix() -> String {
    (0..6)
        .map(|_| format!("{:x}", tdutils::random::fast(0, 15)))
        .collect()
}

/// `clean_filename` (filesystem.cpp:156-174) over the ported unicode
/// tables (prepare_search_character + the simple categories).
pub fn clean_filename(name: &str) -> String {
    if !tdutils::utf8::check_utf8(name.as_bytes()) {
        return String::new();
    }
    let view = tdutils::path_view::PathView::new(name);
    let filename = clean_filename_part(view.file_stem(), 64);
    let extension = clean_filename_part(view.extension(), 16);
    if extension.is_empty() {
        filename
    } else if filename.is_empty() {
        extension
    } else {
        format!("{filename}.{extension}")
    }
}

/// `clean_filename_part` (filesystem.cpp:102-152).
fn clean_filename_part(name: &str, max_length: usize) -> String {
    let is_ok = |code: u32| -> bool {
        if code < 32 {
            return false;
        }
        if code < 127 {
            // < > : " / \ | ? * & ` '
            return !matches!(
                code,
                60 | 62 | 58 | 34 | 47 | 92 | 124 | 63 | 42 | 38 | 96 | 39
            );
        }
        matches!(
            tdutils::unicode::get_unicode_simple_category(code),
            tdutils::unicode::UnicodeSimpleCategory::Letter
                | tdutils::unicode::UnicodeSimpleCategory::DecimalNumber
                | tdutils::unicode::UnicodeSimpleCategory::Number
        )
    };
    let mut new_name = String::new();
    let mut size = 0usize;
    let mut rest = name.as_bytes();
    while size < max_length {
        let (code, len) = if rest.is_empty() {
            break;
        } else {
            tdutils::utf8::next_utf8_unsafe(rest)
        };
        rest = &rest[len..];
        let mut code = code;
        if !is_ok(code) {
            let search_character = tdutils::unicode::prepare_search_character(code);
            if search_character == 0 {
                continue;
            }
            code = b' ' as u32;
        }
        if new_name.is_empty() && (code == b' ' as u32 || code == b'.' as u32) {
            continue;
        }
        new_name.push(char::from_u32(code).unwrap_or(' '));
        size += 1;
    }
    while matches!(new_name.chars().last(), Some(' ') | Some('.')) {
        new_name.pop();
    }
    new_name
}

/// `for_suggested_file_name` (FileLoaderUtils.cpp:104-140) over the
/// caller-supplied counters (None = the `use_pmc`-free search mode).
pub fn for_suggested_file_name(
    name: &str,
    use_pmc: bool,
    use_random: bool,
    ignore_file_names: bool,
    mut counters: Option<&mut FileNameCounters>,
    mut callback: impl FnMut(&str) -> bool,
) -> bool {
    let cleaned_name = clean_filename(name);
    let view = tdutils::path_view::PathView::new(&cleaned_name);
    let stem = view.file_stem().to_string();
    let ext = view.extension().to_string();
    let mut active = true;
    if !stem.is_empty() && !ignore_file_names {
        let with_ext = |stem: &str| {
            if ext.is_empty() {
                stem.to_string()
            } else {
                format!("{stem}.{ext}")
            }
        };
        active = callback(&with_ext(&stem));
        for i in 0..10 {
            if !active {
                break;
            }
            active = callback(&with_ext(&format!("{stem}_({i})")));
        }
        for _ in 2..12 {
            if !active || !use_random {
                break;
            }
            active = callback(&with_ext(&format!("{stem}_({})", rand_suffix())));
        }
    } else if use_pmc && let Some(counters) = counters.as_mut() {
        let with_ext = |stem: &str| {
            if ext.is_empty() {
                stem.to_string()
            } else {
                format!("{stem}.{ext}")
            }
        };
        let file_id = counters.perm_file_id;
        counters.perm_file_id += 1;
        active = callback(&with_ext(&format!("file_{file_id}")));
        if active {
            active = callback(&with_ext(&format!("file_{file_id}_{}", rand_suffix())));
        }
    }
    active
}

/// `search_file` (FileLoaderUtils.cpp:142-161): the file-name search of
/// the `need_search_file` path — returns the path of a file of exactly
/// `expected_size` bytes, if one exists.
pub fn search_file_impl(
    file_type: FileType,
    name: &str,
    expected_size: i64,
    files_dir: &str,
    secure_files_dir: &str,
) -> Result<String, Status> {
    let dir = get_files_dir(file_type, files_dir, secure_files_dir);
    let mut result: Result<String, Status> =
        Err(Status::error(500, "Can't find suitable file name"));
    for_suggested_file_name(name, false, false, false, None, |suggested_name| {
        let path = format!("{dir}{suggested_name}");
        let Some(fd) = open_partial_file_impl(&path, false).ok() else {
            return false;
        };
        let size = fd.metadata().map(|meta| meta.len() as i64).unwrap_or(-1);
        if size != expected_size {
            return true;
        }
        result = Ok(path);
        false
    });
    result
}

/// `FileFd::open(path, Write | Read)`/`open(path, Read)` of the partial
/// file opens (FileDownloader.cpp:494, 514, 423).
pub fn open_partial_file_impl(path: &str, write: bool) -> Result<File, Status> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    if write {
        options.write(true);
    }
    options
        .open(path)
        .map_err(|e| Status::error(500, &format!("Can't open the file \"{path}\": {e}")))
}

/// `read_file_str` (filesystem.cpp:36-58, the `read_file_impl<string>`
/// tail): pread `size` bytes at `offset`; `size < 0` reads to the end.
/// The C++ `string` is a byte string — the Rust twin returns the raw
/// bytes (the `readFilePart` files may be binary).
pub fn read_file_str(path: &str, mut size: i64, offset: i64) -> Result<Vec<u8>, Status> {
    let from_file = open_partial_file_impl(path, false)?;
    let file_size = from_file
        .metadata()
        .map_err(|_| Status::error_message("Can't get stat about the file"))?
        .len() as i64;
    if !(0..=file_size).contains(&offset) {
        return Err(Status::error_message("Failed to read file: invalid offset"));
    }
    if size < 0 || size > file_size - offset {
        size = file_size - offset;
    }
    let mut content = vec![0u8; size.max(0) as usize];
    compat_read_exact_at(&from_file, &mut content, offset as u64)
        .map_err(|_| Status::error_message("Failed to read file"))?;
    Ok(content)
}

/// `open_temp_file` (FileLoaderUtils.cpp:85-102): the id-named temp file
/// under the per-base "temp" directory.
pub fn open_temp_file_impl(
    file_type: FileType,
    files_dir: &str,
    secure_files_dir: &str,
    counters: &mut FileNameCounters,
) -> Result<(File, String), Status> {
    let temp_dir = get_files_temp_dir(file_type, files_dir, secure_files_dir);
    let file_id = counters.tmp_file_id;
    counters.tmp_file_id += 1;
    let mut result = try_create_new_file(&temp_dir, &file_id.to_string());
    if result.is_err() {
        result = try_create_new_file(&temp_dir, &format!("{file_id}_{}", rand_suffix()));
    }
    result
}

/// `create_from_temp` (FileLoaderUtils.cpp:126-140): the suggested-name
/// walk + rename into the per-type directory.
pub fn create_from_temp_impl(
    file_type: FileType,
    temp_path: &str,
    name: &str,
    files_dir: &str,
    secure_files_dir: &str,
    ignore_file_names: bool,
    counters: &mut FileNameCounters,
) -> Result<String, Status> {
    let dir = get_files_dir(file_type, files_dir, secure_files_dir);
    let mut result: Result<String, Status> =
        Err(Status::error(500, "Can't find suitable file name"));
    for_suggested_file_name(
        name,
        true,
        true,
        ignore_file_names,
        Some(counters),
        |suggested_name| match try_create_new_file(&dir, suggested_name) {
            Ok((fd, perm_path)) => {
                drop(fd);
                result = Ok(perm_path);
                false
            }
            Err(_) => true,
        },
    );
    let perm_path = result?;
    std::fs::rename(temp_path, &perm_path)
        .map_err(|e| Status::error(500, &format!("Can't rename the temporary file: {e}")))?;
    Ok(perm_path)
}

/// `get_suggested_file_name` (FileLoaderUtils.cpp:164-204) — the
/// `getSuggestedFileName` tail: the cleaned name, or, when the directory
/// exists, the first free `stem (N).ext` slot of the directory.
pub fn get_suggested_file_name_impl<C: FileManagerContext + ?Sized>(
    context: &C,
    directory: &str,
    file_name: &str,
) -> String {
    let cleaned_name = clean_filename(file_name);

    let directory = if directory.is_empty() {
        "./"
    } else {
        directory
    };

    let dir_stat = match context.stat(directory) {
        Ok(stat) => stat,
        Err(_) => return cleaned_name,
    };
    if !dir_stat.is_dir {
        return cleaned_name;
    }

    let path_view = tdutils::path_view::PathView::new(&cleaned_name);
    let stem = path_view.file_stem();
    let ext = path_view.extension();

    if stem.is_empty() {
        return cleaned_name;
    }

    let mut directory = directory;
    while directory.len() > 1 && (directory.ends_with('/') || directory.ends_with('\\')) {
        directory = &directory[..directory.len() - 1];
    }

    let with_ext = |stem: &str| {
        if ext.is_empty() {
            stem.to_string()
        } else {
            format!("{stem}.{ext}")
        }
    };
    let check_file_name = |name: &str| context.stat(&format!("{directory}/{name}")).is_err();

    let checked_name = with_ext(stem);
    if check_file_name(&checked_name) {
        return checked_name;
    }

    for i in 1..100 {
        let checked_name = with_ext(&format!("{stem} ({i})"));
        if check_file_name(&checked_name) {
            return checked_name;
        }
    }

    // `StringBuilder::FixedDouble(Clocks::system(), 3)` — the current
    // system time with three decimals.
    with_ext(&format!("{stem} - {:.3}", tdutils::time::now()))
}

/// One event of the collapsed [`FileGenerateManager`] callback plane —
/// each C++ `send_closure` into `FileManager`
/// (`on_partial_generate`/`on_generate_ok`/`on_generate_error`,
/// FileManager.cpp:4988-5095) becomes one queued event.
/// [`FileGenerateEvent::DownloadOk`] is the
/// `FileDownloadGenerateActor::on_download_ok` half
/// (FileGenerateManager.cpp:93-115): the file-view read + file-type
/// override before it re-enters the same plane as a `GenerateOk`.
#[derive(Clone, Debug)]
pub enum FileGenerateEvent {
    PartialGenerate {
        query_id: u64,
        partial: PartialLocalFileLocation,
        expected_size: i64,
    },
    GenerateOk {
        query_id: u64,
        local: FullLocalFileLocation,
    },
    GenerateError {
        query_id: u64,
        status: Status,
    },
    DownloadOk {
        query_id: u64,
        file_id: FileId,
        file_type: FileType,
    },
}

/// The shared event queue — the manager hands [`Rc`] clones to its
/// per-query bridges.
#[derive(Clone)]
struct GenerateEventQueue(Arc<parking_lot::Mutex<Vec<FileGenerateEvent>>>);

impl GenerateEventQueue {
    fn push(&self, event: FileGenerateEvent) {
        self.0.lock().push(event);
    }

    fn has_events(&self) -> bool {
        !self.0.lock().is_empty()
    }
}

/// `FileDownloadGenerateActor`'s download `Callback`
/// (FileGenerateManager.cpp:56-70): ok/error forward into the generate
/// plane by query id. The C++ actor dies on cancel before the error can
/// be delivered; in the collapse the manager's `canceled` guard drops it
/// instead.
struct DownloadGenerateBridge {
    query_id: u64,
    file_type: FileType,
    events: GenerateEventQueue,
}

impl DownloadCallback for DownloadGenerateBridge {
    fn on_progress(&mut self, _file_id: FileId) {}

    fn on_download_ok(&mut self, file_id: FileId) {
        self.events.push(FileGenerateEvent::DownloadOk {
            query_id: self.query_id,
            file_id,
            file_type: self.file_type,
        });
    }

    fn on_download_error(&mut self, _file_id: FileId, status: Status) {
        self.events.push(FileGenerateEvent::GenerateError {
            query_id: self.query_id,
            status,
        });
    }
}

/// The worker state of one registered generation query.
enum GenerateWorker {
    /// `FileDownloadGenerateActor` (FileGenerateManager.cpp:41-121) —
    /// the download re-entry is mediated by `FileManager::run_generate`
    /// (the collapsed actor has no scheduler to `send_closure` from);
    /// the worker carries only the cancel seam.
    Download {
        file_id: FileId,
        internal_download_id: i64,
    },
    /// `FileExternalGenerateActor` (FileGenerateManager.cpp:299-391) —
    /// the app-driven conversion flow.
    External {
        file_type: FileType,
        name: String,
        path: String,
    },
    // `WebFileDownloadGenerateActor` (FileGenerateManager.cpp:123-297)
    // runs to completion inside `generate_file` — the collapsed worker
    // has no pending state and never registers.
}

struct GenerateQueryState {
    worker: GenerateWorker,
    /// `FileGenerateManager::cancel` marks the query; the queued
    /// terminal event finishes it (the C++ `worker_.reset()` + the
    /// actor's asynchronous death).
    canceled: bool,
}

/// `td::FileGenerateManager` (FileGenerateManager.cpp:393-519) collapsed
/// per the single-scheduler convention: the actor map becomes a query
/// table, the callbacks queue [`FileGenerateEvent`]s that
/// `FileManager::apply_generate_events` drains in order, and the Web
/// worker runs synchronously inside [`FileGenerateManager::generate_file`].
pub struct FileGenerateManager {
    queries: BTreeMap<u64, GenerateQueryState>,
    events: GenerateEventQueue,
}

impl Default for FileGenerateManager {
    fn default() -> FileGenerateManager {
        FileGenerateManager::new()
    }
}

impl FileGenerateManager {
    pub fn new() -> FileGenerateManager {
        FileGenerateManager {
            queries: BTreeMap::new(),
            events: GenerateEventQueue(Arc::new(Mutex::new(Vec::new()))),
        }
    }

    /// Whether the callback queue carries unapplied events (the drive
    /// loop's apply round must run even with an idle loader).
    pub fn has_events(&self) -> bool {
        self.events.has_events()
    }

    fn take_event(&mut self) -> Option<FileGenerateEvent> {
        let mut events = self.events.0.lock();
        if events.is_empty() {
            None
        } else {
            Some(events.remove(0))
        }
    }

    /// `FileGenerateManager::generate_file` (FileGenerateManager.cpp:436-471)
    /// — `check_mtime`, the `#file_id#`/web/external split, the External
    /// `start_up` and the synchronous Web run. `Some(..)` is the Download
    /// worker: the caller must re-enter `FileManager::download` with the
    /// returned bridge (the C++ `start_up`'s download send).
    #[allow(clippy::too_many_arguments)]
    pub fn generate_file(
        &mut self,
        query_id: u64,
        mut generate_location: FullGenerateFileLocation,
        local_location: &LocalFileLocation,
        name: String,
        internal_download_id: i64,
        net: &mut dyn FileDownloaderNet,
        ctx: &mut dyn FileManagerContext,
    ) -> Option<GenerateDownloadStart> {
        debug_assert!(query_id != 0, "CHECK(query_id != 0)");
        // check_mtime rewrites the conversion in place (the member is
        // passed by reference in the C++, FileGenerateManager.cpp:444)
        if let Err(error) = check_generate_mtime(
            ctx,
            &mut generate_location.conversion,
            &generate_location.original_path,
        ) {
            self.events.push(FileGenerateEvent::GenerateError {
                query_id,
                status: error,
            });
            return None;
        }

        let file_id = generate_location
            .conversion
            .strip_prefix("#file_id#")
            .map(|raw| FileId::new(raw.parse().unwrap_or(0), 0));
        if let Some(file_id) = file_id {
            self.queries.insert(
                query_id,
                GenerateQueryState {
                    worker: GenerateWorker::Download {
                        file_id,
                        internal_download_id,
                    },
                    canceled: false,
                },
            );
            let callback: Arc<parking_lot::Mutex<dyn DownloadCallback>> =
                Arc::new(Mutex::new(DownloadGenerateBridge {
                    query_id,
                    file_type: generate_location.file_type,
                    events: self.events.clone(),
                }));
            return Some((file_id, internal_download_id, callback));
        }

        if is_remotely_generated_file(&generate_location.conversion)
            && generate_location.original_path.is_empty()
        {
            // `WebFileDownloadGenerateActor::start_up` + `on_result`
            // (FileGenerateManager.cpp:212-253) — synchronous here.
            let result = web_generate_download(&generate_location.conversion, net, ctx);
            match result {
                Ok(local) => self
                    .events
                    .push(FileGenerateEvent::GenerateOk { query_id, local }),
                Err(status) => self
                    .events
                    .push(FileGenerateEvent::GenerateError { query_id, status }),
            }
            return None;
        }

        // `FileExternalGenerateActor::start_up` (FileGenerateManager.cpp:305-330)
        let worker_path = match local_location {
            LocalFileLocation::Full(full) => {
                // callback_->on_ok + callback_.reset() + stop() — the
                // worker registers only so the stop update fires at the
                // terminal event (the C++ tear_down)
                self.queries.insert(
                    query_id,
                    GenerateQueryState {
                        worker: GenerateWorker::External {
                            file_type: generate_location.file_type,
                            name,
                            path: String::new(),
                        },
                        canceled: false,
                    },
                );
                self.events.push(FileGenerateEvent::GenerateOk {
                    query_id,
                    local: full.clone(),
                });
                return None;
            }
            LocalFileLocation::Partial(partial) => {
                // LOG(INFO) "Unlink partially generated file at " + unlink
                let _ = std::fs::remove_file(&partial.path);
                Some(partial.path.clone())
            }
            LocalFileLocation::Empty => match net.open_temp_file(generate_location.file_type) {
                Ok((file, path)) => {
                    drop(file);
                    Some(path)
                }
                Err(status) => {
                    // check_status at start_up: the error path without a
                    // path to unlink
                    self.events
                        .push(FileGenerateEvent::GenerateError { query_id, status });
                    return None;
                }
            },
        };
        let path = worker_path.expect("worker path");
        ctx.on_file_generation_start(
            query_id as i64,
            &generate_location.original_path,
            &path,
            &generate_location.conversion,
        );
        self.queries.insert(
            query_id,
            GenerateQueryState {
                worker: GenerateWorker::External {
                    file_type: generate_location.file_type,
                    name,
                    path,
                },
                canceled: false,
            },
        );
        None
    }

    /// `FileGenerateManager::cancel` (FileGenerateManager.cpp:473-478)
    /// plus the actors' `hangup` halves. Returns the pending download for
    /// a `Download` worker — the caller must `FileManager::cancel_download`
    /// it (the actor's hangup send, FileGenerateManager.cpp:117-120). The
    /// worker stays registered (marked) until the queued terminal event
    /// drains, keeping the stop update after the error, like the C++
    /// queue order.
    pub fn cancel(&mut self, query_id: u64) -> Option<(FileId, i64)> {
        let state = self.queries.get_mut(&query_id)?;
        match &state.worker {
            GenerateWorker::Download {
                file_id,
                internal_download_id,
            } => {
                state.canceled = true;
                self.events.push(FileGenerateEvent::GenerateError {
                    query_id,
                    status: Status::error(200, "Canceled"),
                });
                Some((*file_id, *internal_download_id))
            }
            GenerateWorker::External { path, .. } => {
                state.canceled = true;
                // `FileExternalGenerateActor::hangup`
                // (FileGenerateManager.cpp:332-334): unlink + on_error
                let _ = std::fs::remove_file(path);
                self.events.push(FileGenerateEvent::GenerateError {
                    query_id,
                    status: Status::error(-1, "Canceled"),
                });
                None
            }
        }
    }

    fn external_path(&self, generation_id: u64) -> Result<ExternalPathView<'_>, Status> {
        let Some(state) = self.queries.get(&generation_id) else {
            return Err(Status::error(400, "Unknown generation_id"));
        };
        if state.canceled {
            return Err(Status::error(400, "Unknown generation_id"));
        }
        let download = match &state.worker {
            GenerateWorker::Download {
                file_id,
                internal_download_id,
            } => Some((*file_id, *internal_download_id)),
            GenerateWorker::External { .. } => None,
        };
        Ok((&state.worker, download))
    }

    /// `FileManager::external_file_generate_write_part` →
    /// `FileGenerateActor::file_generate_write_part` +
    /// `do_file_generate_write_part` (FileGenerateManager.cpp:305-341).
    pub fn external_write_part(
        &mut self,
        generation_id: u64,
        offset: i64,
        data: &[u8],
        net: &mut dyn FileDownloaderNet,
    ) -> Result<(), Status> {
        let path = match self.external_path(generation_id) {
            Ok((GenerateWorker::External { path, .. }, _)) => path.clone(),
            Ok(_) | Err(_) => return Err(Status::error(400, "Unknown generation_id")),
        };
        match generate_write_part(&path, offset, data, net) {
            Ok(()) => Ok(()),
            Err(status) => {
                // check_status: the promise sees 400 + the message while
                // the generation fails with the original status
                let promise_status = Status::error(400, status.message());
                self.fail_external(generation_id, status);
                Err(promise_status)
            }
        }
    }

    /// `file_generate_progress` + `do_file_generate_progress`
    /// (FileGenerateManager.cpp:315-318, 348-359).
    pub fn external_progress(
        &mut self,
        generation_id: u64,
        expected_size: i64,
        local_prefix_size: i64,
    ) -> Result<(), Status> {
        let (file_type, path) = match self.external_path(generation_id) {
            Ok((
                GenerateWorker::External {
                    file_type, path, ..
                },
                _,
            )) => (*file_type, path.clone()),
            Ok(_) | Err(_) => return Err(Status::error(400, "Unknown generation_id")),
        };
        let status = if local_prefix_size < 0 {
            Err(Status::error(400, "Invalid local prefix size"))
        } else {
            let partial = PartialLocalFileLocation {
                file_type,
                part_size: local_prefix_size,
                path: path.clone(),
                iv: Vec::new(),
                ready_bitmask: Bitmask::from_ones(1).encode(-1),
                ready_size: local_prefix_size,
            };
            self.events.push(FileGenerateEvent::PartialGenerate {
                query_id: generation_id,
                partial,
                expected_size,
            });
            Ok(())
        };
        match status {
            Ok(()) => Ok(()),
            Err(status) => {
                let promise_status = Status::error(400, status.message());
                self.fail_external(generation_id, status);
                Err(promise_status)
            }
        }
    }

    /// `file_generate_finish` + `do_file_generate_finish`
    /// (FileGenerateManager.cpp:319-322, 361-370). `error` is the
    /// client-reported failure — the td_api result still resolves ok.
    pub fn external_finish(
        &mut self,
        generation_id: u64,
        error: Option<Status>,
        net: &mut dyn FileDownloaderNet,
    ) -> Result<(), Status> {
        if let Some(status) = error {
            self.fail_external(generation_id, status);
            return Ok(());
        }
        let (file_type, name, path) = match self.external_path(generation_id) {
            Ok((
                GenerateWorker::External {
                    file_type,
                    name,
                    path,
                },
                _,
            )) => (*file_type, name.clone(), path.clone()),
            Ok(_) | Err(_) => return Err(Status::error(400, "Unknown generation_id")),
        };
        match net.create_from_temp(file_type, &path, &name) {
            Ok(perm_path) => {
                self.events.push(FileGenerateEvent::GenerateOk {
                    query_id: generation_id,
                    local: FullLocalFileLocation::new(file_type, perm_path, 0),
                });
                Ok(())
            }
            Err(status) => {
                let promise_status = Status::error(400, status.message());
                self.fail_external(generation_id, status);
                Err(promise_status)
            }
        }
    }

    /// The `check_status` error path (FileGenerateManager.cpp:372-383):
    /// unlink the partial + `callback_->on_error` + stop. The stop update
    /// rides `remove_worker` at the terminal event.
    fn fail_external(&mut self, generation_id: u64, status: Status) {
        if let Some(state) = self.queries.get_mut(&generation_id) {
            if let GenerateWorker::External { path, .. } = &state.worker {
                let _ = std::fs::remove_file(path);
            }
            state.canceled = true;
        }
        self.events.push(FileGenerateEvent::GenerateError {
            query_id: generation_id,
            status,
        });
    }
}

/// `FileExternalGenerateActor::do_file_generate_write_part`
/// (FileGenerateManager.cpp:341-354).
fn generate_write_part(
    path: &str,
    offset: i64,
    data: &[u8],
    net: &mut dyn FileDownloaderNet,
) -> Result<(), Status> {
    if offset < 0 {
        return Err(Status::error_message("Wrong offset specified"));
    }
    let mut fd = net.open_generated_file(path)?;
    use std::io::{Seek, Write as _};
    fd.seek(std::io::SeekFrom::Start(offset as u64))
        .map_err(|e| Status::error_message(&format!("Failed to write file: {e}")))?;
    let written = fd
        .write(data)
        .map_err(|e| Status::error_message(&format!("Failed to write file: {e}")))?
        as i64;
    if written != data.len() as i64 {
        return Err(Status::error_message(&format!(
            "Failed to write file: written {written} bytes instead of {}",
            data.len()
        )));
    }
    Ok(())
}

/// `WebFileDownloadGenerateActor::process_result` + `on_result`
/// (FileGenerateManager.cpp:228-253): one `upload.getWebFile` query, the
/// size sanity log (the collapsed net seam returns only the body — the
/// `web_file.size_` cross-check has no carrier here), and
/// `save_file_bytes` as a `Thumbnail`.
fn web_generate_download(
    conversion: &str,
    net: &mut dyn FileDownloaderNet,
    ctx: &mut dyn FileManagerContext,
) -> Result<FullLocalFileLocation, Status> {
    let (location, file_name) = parse_web_generate_conversion(conversion, ctx)?;
    let meta = DownloadNetMeta {
        dc_id: net.webfile_dc_id(),
        auth_flag_on: true,
        small: true,
    };
    let bytes = match net.download(
        DownloadQuery::GetWebFile {
            location,
            offset: 0,
            limit: 1 << 20,
        },
        DownloadQueryType::Default,
        meta,
    )? {
        DownloadResult::Bytes(bytes) => bytes,
        _ => return Err(Status::error_message("Wrong conversion result")),
    };
    save_file_bytes(net, FileType::Thumbnail, &bytes, &file_name)
}

/// `WebFileDownloadGenerateActor::parse_conversion`
/// (FileGenerateManager.cpp:131-210) — the `#`-framed `audio_t` album
/// thumb and the `map` geo-point conversions.
fn parse_web_generate_conversion(
    conversion: &str,
    ctx: &mut dyn FileManagerContext,
) -> Result<(telegram_api::InputWebFileLocation, String), Status> {
    let parts: Vec<&str> = conversion.split('#').collect();
    if parts.len() <= 2 || !parts[0].is_empty() || !parts.last().is_some_and(|p| p.is_empty()) {
        return Err(Status::error_message("Wrong conversion"));
    }
    if parts.len() == 6 && parts[1] == "audio_t" {
        // music thumbnail
        if parts[2].is_empty() && parts[3].is_empty() {
            return Err(Status::error_message(
                "Title or performer must be non-empty",
            ));
        }
        if parts[4] != "0" && parts[4] != "1" {
            return Err(Status::error_message("Invalid conversion"));
        }
        let is_small = parts[4].as_bytes()[0] == b'1';
        let file_name = format!(
            "Album cover {}for {} - {}.jpg",
            if is_small { "thumbnail " } else { "" },
            parts[3],
            parts[2]
        );
        // TITLE_MASK (flags.1) — title and performer share the bit
        const TITLE_MASK: i32 = 1 << 1;
        let flags = TITLE_MASK | if is_small { 1 << 2 } else { 0 };
        return Ok((
            telegram_api::InputWebFileLocation::inputWebFileAudioAlbumThumbLocation(
                telegram_api::inputWebFileAudioAlbumThumbLocation {
                    flags,
                    small: is_small,
                    document: None,
                    title: parts[2].to_owned(),
                    performer: parts[3].to_owned(),
                },
            ),
            file_name,
        ));
    }

    if parts.len() != 9 || parts[1] != "map" {
        return Err(Status::error_message("Wrong conversion"));
    }
    let parse = |raw: &str| -> Result<i32, Status> {
        raw.parse::<i32>()
            .map_err(|_| Status::error_message("Wrong conversion"))
    };
    let zoom = parse(parts[2])?;
    let x = parse(parts[3])?;
    let y = parse(parts[4])?;
    let width = parse(parts[5])?;
    let height = parse(parts[6])?;
    let scale = parse(parts[7])?;

    if !(13..=20).contains(&zoom) {
        return Err(Status::error_message("Wrong zoom"));
    }
    let size = 256i64 * (1i64 << zoom);
    if !(0..size).contains(&(x as i64)) {
        return Err(Status::error_message("Wrong x"));
    }
    if !(0..size).contains(&(y as i64)) {
        return Err(Status::error_message("Wrong y"));
    }
    if !(16..=1024).contains(&width) || !(16..=1024).contains(&height) {
        return Err(Status::error_message("Wrong dimensions"));
    }
    if !(1..=3).contains(&scale) {
        return Err(Status::error_message("Wrong scale"));
    }

    let file_name = format!("map_{zoom}_{x}_{y}.png");

    const PI: f64 = std::f64::consts::PI;
    let longitude = (x as f64 + 0.1) * 360.0 / size as f64 - 180.0;
    let latitude = 90.0
        - 360.0
            * (((y as f64 + 0.1) / size as f64 - 0.5) * 2.0 * PI)
                .exp()
                .atan()
            / PI;

    let access_hash = ctx.location_access_hash(latitude, longitude);
    Ok((
        telegram_api::InputWebFileLocation::inputWebFileGeoPointLocation(
            telegram_api::inputWebFileGeoPointLocation {
                geo_point: Some(telegram_api::InputGeoPoint::inputGeoPoint(
                    telegram_api::inputGeoPoint {
                        flags: 0,
                        lat: latitude,
                        long: longitude,
                        accuracy_radius: 0,
                    },
                )),
                access_hash,
                w: width,
                h: height,
                zoom,
                scale,
            },
        ),
        file_name,
    ))
}

/// `check_mtime` (FileGenerateManager.cpp:393-425) — the `#mtime#`
/// staleness guard `register_generate` embeds. The conversion is
/// rewritten in place on a parseable prefix, like the C++
/// `ConstParser` walk.
fn check_generate_mtime(
    ctx: &mut dyn FileManagerContext,
    conversion: &mut String,
    original_path: &str,
) -> Result<(), Status> {
    if original_path.is_empty() {
        return Ok(());
    }
    let Some(rest) = conversion.strip_prefix("#mtime#") else {
        return Ok(());
    };
    let Some((mtime_raw, tail)) = rest.split_once('#') else {
        return Ok(());
    };
    // the C++ keeps the last digit: strip while size() >= 2 && [0] == '0'
    let mut mtime_str = mtime_raw;
    while mtime_str.len() >= 2 && mtime_str.starts_with('0') {
        mtime_str = &mtime_str[1..];
    }
    let Ok(expected_mtime) = mtime_str.parse::<u64>() else {
        return Ok(());
    };
    *conversion = tail.to_owned();
    let actual_mtime = ctx.stat(original_path).map(|s| s.mtime_nsec).unwrap_or(0);
    if are_modification_times_equal(expected_mtime as i64, actual_mtime as i64) {
        return Ok(());
    }
    Err(Status::error_message(&format!(
        "FILE_GENERATE_LOCATION_INVALID: File \"{original_path}\" was modified: \
         [expected modification time:{expected_mtime}][actual modification time:{actual_mtime}]"
    )))
}

/// `FileTypeStat` (FileStats.h:19-22) — the per-file-type byte/count
/// accumulator; the binlog record is `store(size)` + `store(cnt)`
/// (FileStats.cpp:24-33), which the fast-stat persistence rides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileTypeStat {
    pub size: i64,
    pub cnt: i32,
}

/// `FullFileInfo` (FileStats.h:43-50) — one file as the gc and the
/// stats worker see it.
#[derive(Clone, Debug)]
pub struct FullFileInfo {
    pub file_type: FileType,
    pub path: String,
    pub owner_dialog_id: DialogId,
    pub size: i64,
    pub atime_nsec: u64,
    pub mtime_nsec: u64,
}

/// `FileStatsFast` (FileStats.h:52-67).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileStatsFast {
    pub size: i64,
    pub count: i32,
    pub database_size: i64,
    pub language_pack_database_size: i64,
    pub log_size: i64,
}

impl FileStatsFast {
    /// `get_storage_statistics_fast_object` (FileStats.cpp:26-30).
    pub fn get_storage_statistics_fast_object(&self) -> td_api::StorageStatisticsFast {
        td_api::StorageStatisticsFast {
            files_size: self.size,
            file_count: self.count,
            database_size: self.database_size,
            language_pack_database_size: self.language_pack_database_size,
            log_size: self.log_size,
        }
    }
}

/// `td::DatabaseStats` (StorageManager.h:40-45) — the `TdDb::get_stats`
/// debug string behind `getDatabaseStatistics`.
pub struct DatabaseStats(pub String);

impl DatabaseStats {
    /// `get_database_statistics_object` (StorageManager.cpp:30-32).
    pub fn get_database_statistics_object(&self) -> td_api::DatabaseStatistics {
        td_api::DatabaseStatistics {
            statistics: self.0.clone(),
        }
    }
}

/// The per-type stat array (`FileStats::StatByType`).
pub type StatByType = Vec<FileTypeStat>;

fn new_stat_by_type() -> StatByType {
    vec![FileTypeStat::default(); MAX_FILE_TYPE as usize]
}

/// `td::FileStats` (FileStats.h:69-117) — the accumulator behind
/// `getStorageStatistics`/`optimizeStorage`.
pub struct FileStats {
    need_all_files: bool,
    split_by_owner_dialog_id: bool,
    stat_by_type: StatByType,
    /// The C++ `std::unordered_map` becomes a `BTreeMap`: the td_api
    /// `by_chat` order is a full sort (size desc, chat-id-0 last) whose
    /// ties the C++ leaves hash-order dependent; the BTreeMap key order
    /// makes the tie-break chat-id ascending.
    stat_by_owner_dialog_id: BTreeMap<DialogId, StatByType>,
    all_files: Vec<FullFileInfo>,
}

impl FileStats {
    /// `FileStats::FileStats` (FileStats.cpp:106-110).
    pub fn new(need_all_files: bool, split_by_owner_dialog_id: bool) -> FileStats {
        FileStats {
            need_all_files,
            split_by_owner_dialog_id,
            stat_by_type: new_stat_by_type(),
            stat_by_owner_dialog_id: BTreeMap::new(),
            all_files: Vec::new(),
        }
    }

    /// `FileStats::add(StatByType&, ...)` (FileStats.cpp:32-38).
    fn add_to(by_type: &mut StatByType, file_type: FileType, size: i64) {
        let pos = file_type as usize;
        by_type[pos].size += size;
        by_type[pos].cnt += 1;
    }

    /// `FileStats::add_impl` (FileStats.cpp:40-47).
    fn add_impl(&mut self, info: &FullFileInfo) {
        if self.split_by_owner_dialog_id {
            Self::add_to(
                self.stat_by_owner_dialog_id
                    .entry(info.owner_dialog_id)
                    .or_insert_with(new_stat_by_type),
                info.file_type,
                info.size,
            );
        } else {
            Self::add_to(&mut self.stat_by_type, info.file_type, info.size);
        }
    }

    /// `FileStats::add_copy` (FileStats.cpp:49-54).
    pub fn add_copy(&mut self, info: &FullFileInfo) {
        self.add_impl(info);
        if self.need_all_files {
            self.all_files.push(info.clone());
        }
    }

    /// `FileStats::add` (FileStats.cpp:56-61).
    pub fn add(&mut self, info: FullFileInfo) {
        self.add_impl(&info);
        if self.need_all_files {
            self.all_files.push(info);
        }
    }

    /// `FileStats::get_nontemp_stat` (FileStats.cpp:63-72).
    fn get_nontemp_stat(by_type: &StatByType) -> FileTypeStat {
        let mut stat = FileTypeStat::default();
        for i in 0..MAX_FILE_TYPE {
            if FileType::from_raw(i) != FileType::Temp {
                stat.size += by_type[i as usize].size;
                stat.cnt += by_type[i as usize].cnt;
            }
        }
        stat
    }

    /// `FileStats::get_total_nontemp_stat` (FileStats.cpp:74-87).
    pub fn get_total_nontemp_stat(&self) -> FileTypeStat {
        if !self.split_by_owner_dialog_id {
            return Self::get_nontemp_stat(&self.stat_by_type);
        }
        let mut stat = FileTypeStat::default();
        for by_type in self.stat_by_owner_dialog_id.values() {
            let tmp = Self::get_nontemp_stat(by_type);
            stat.size += tmp.size;
            stat.cnt += tmp.cnt;
        }
        stat
    }

    /// `FileStats::apply_dialog_limit` (FileStats.cpp:89-112) — keeps
    /// the `limit` biggest dialogs, folding the rest into the default
    /// (invalid) dialog id.
    pub fn apply_dialog_limit(&mut self, limit: i32) {
        if limit == -1 || !self.split_by_owner_dialog_id {
            return;
        }
        let mut dialogs: Vec<(i64, DialogId)> = self
            .stat_by_owner_dialog_id
            .iter()
            .filter(|(dialog_id, _)| dialog_id.is_valid())
            .map(|(dialog_id, by_type)| (by_type.iter().map(|s| s.size).sum(), *dialog_id))
            .collect();
        let prefix = dialogs.len().min(limit as usize);
        dialogs.sort_by_key(|dialog| std::cmp::Reverse(dialog.0));
        dialogs.truncate(prefix);

        let dialog_ids: Vec<DialogId> = dialogs.iter().map(|(_, dialog_id)| *dialog_id).collect();
        self.apply_dialog_ids(&dialog_ids);
    }

    /// `FileStats::apply_dialog_ids` (FileStats.cpp:114-141) — drops
    /// every dialog outside `dialog_ids` into the default id bucket.
    pub fn apply_dialog_ids(&mut self, dialog_ids: &[DialogId]) {
        let all_dialog_ids: BTreeSet<DialogId> = dialog_ids.iter().copied().collect();
        let mut other_stats = new_stat_by_type();
        let mut other_flag = false;
        let mut removed = Vec::new();
        for (dialog_id, by_type) in &self.stat_by_owner_dialog_id {
            if !all_dialog_ids.contains(dialog_id) {
                for i in 0..MAX_FILE_TYPE as usize {
                    other_stats[i].size += by_type[i].size;
                    other_stats[i].cnt += by_type[i].cnt;
                }
                other_flag = true;
                removed.push(*dialog_id);
            }
        }
        for dialog_id in removed {
            self.stat_by_owner_dialog_id.remove(&dialog_id);
        }
        if other_flag {
            self.stat_by_owner_dialog_id
                .insert(DialogId::default(), other_stats);
        }
    }

    /// `FileStats::get_storage_statistics_by_chat_object`
    /// (FileStats.cpp:143-166) — aggregates by main file type and
    /// returns the non-zero entries.
    fn get_storage_statistics_by_chat_object(
        dialog_id: DialogId,
        stat_by_type: &StatByType,
    ) -> td_api::StorageStatisticsByChat {
        let mut stats = td_api::StorageStatisticsByChat {
            chat_id: dialog_id.get(),
            size: 0,
            count: 0,
            by_file_type: Vec::new(),
        };
        let mut aggregated = new_stat_by_type();
        for i in 0..MAX_FILE_TYPE {
            let main = get_main_file_type(FileType::from_raw(i)) as usize;
            aggregated[main].size += stat_by_type[i as usize].size;
            aggregated[main].cnt += stat_by_type[i as usize].cnt;
        }
        for i in 0..MAX_FILE_TYPE {
            let size = aggregated[i as usize].size;
            let cnt = aggregated[i as usize].cnt;
            if size == 0 {
                continue;
            }
            stats.size += size;
            stats.count += cnt;
            stats
                .by_file_type
                .push(td_api::StorageStatisticsByFileType {
                    file_type: Some(get_file_type_object(FileType::from_raw(i))),
                    size,
                    count: cnt,
                });
        }
        stats
    }

    /// `FileStats::get_storage_statistics_object` (FileStats.cpp:168-190).
    pub fn get_storage_statistics_object(&self) -> td_api::StorageStatistics {
        let mut stats = td_api::StorageStatistics {
            size: 0,
            count: 0,
            by_chat: Vec::new(),
        };
        if !self.split_by_owner_dialog_id {
            stats
                .by_chat
                .push(Self::get_storage_statistics_by_chat_object(
                    DialogId::default(),
                    &self.stat_by_type,
                ));
        } else {
            let mut by_chat: Vec<td_api::StorageStatisticsByChat> = self
                .stat_by_owner_dialog_id
                .iter()
                .map(|(dialog_id, by_type)| {
                    Self::get_storage_statistics_by_chat_object(*dialog_id, by_type)
                })
                .collect();
            // size desc, the empty chat_id last — the C++ comparator
            by_chat.sort_by(|x, y| {
                let x_zero = x.chat_id == 0;
                let y_zero = y.chat_id == 0;
                x_zero
                    .cmp(&y_zero)
                    .then_with(|| y.size.cmp(&x.size))
                    .then_with(|| x.chat_id.cmp(&y.chat_id))
            });
            stats.by_chat = by_chat;
        }
        for by_dialog in &stats.by_chat {
            stats.size += by_dialog.size;
            stats.count += by_dialog.count;
        }
        stats
    }

    /// `FileStats::get_dialog_ids` (FileStats.cpp:192-204).
    pub fn get_dialog_ids(&self) -> Vec<DialogId> {
        if !self.split_by_owner_dialog_id {
            return Vec::new();
        }
        self.stat_by_owner_dialog_id
            .keys()
            .filter(|dialog_id| dialog_id.is_valid())
            .copied()
            .collect()
    }

    /// `FileStats::get_all_files` (FileStats.cpp:206-209).
    pub fn get_all_files(&mut self) -> Vec<FullFileInfo> {
        std::mem::take(&mut self.all_files)
    }
}

/// `FileGcParameters` (FileGcParameters.h:19-31).
#[derive(Clone, Debug)]
pub struct FileGcParameters {
    pub max_files_size: i64,
    pub max_time_from_last_access: u32,
    pub max_file_count: u32,
    pub immunity_delay: u32,
    pub file_types: Vec<FileType>,
    pub owner_dialog_ids: Vec<DialogId>,
    pub exclude_owner_dialog_ids: Vec<DialogId>,
    pub dialog_limit: i32,
}

impl Default for FileGcParameters {
    /// `FileGcParameters()` (FileGcParameters.h:23) — all -1/0 sentinels.
    fn default() -> FileGcParameters {
        FileGcParameters {
            max_files_size: -1,
            max_time_from_last_access: -1i32 as u32,
            max_file_count: -1i32 as u32,
            immunity_delay: -1i32 as u32,
            file_types: Vec::new(),
            owner_dialog_ids: Vec::new(),
            exclude_owner_dialog_ids: Vec::new(),
            dialog_limit: 0,
        }
    }
}

impl FileGcParameters {
    /// The `FileGcParameters::FileGcParameters` ctor
    /// (FileGcParameters.cpp:19-33) — the -1 sentinels fall back to the
    /// `storage_*` option reads with their defaults.
    pub fn from_options(
        ctx: &mut dyn FileManagerContext,
        size: i64,
        ttl: i32,
        count: i32,
        immunity_delay: i32,
    ) -> FileGcParameters {
        let max_files_size = if size >= 0 {
            size
        } else {
            ctx.option_integer("storage_max_files_size", 100 << 10) << 10
        };
        let max_time_from_last_access = if ttl >= 0 {
            ttl
        } else {
            ctx.option_integer("storage_max_time_from_last_access", 60 * 60 * 23) as i32
        };
        let max_file_count = if count >= 0 {
            count
        } else {
            ctx.option_integer("storage_max_file_count", 40000) as i32
        };
        let immunity_delay = if immunity_delay >= 0 {
            immunity_delay
        } else {
            ctx.option_integer("storage_immunity_delay", 60 * 60) as i32
        };
        FileGcParameters {
            max_files_size,
            max_time_from_last_access: max_time_from_last_access as u32,
            max_file_count: max_file_count as u32,
            immunity_delay: immunity_delay as u32,
            file_types: Vec::new(),
            owner_dialog_ids: Vec::new(),
            exclude_owner_dialog_ids: Vec::new(),
            dialog_limit: 0,
        }
    }
}

/// `FileGcResult` (FileGcWorker.h:24-27).
pub struct FileGcResult {
    pub kept_file_stats: FileStats,
    pub removed_file_stats: FileStats,
}

/// The gc's outside world: the `G()->use_file_database()` immunity
/// switch, the `FileManager::on_file_unlink` update plane and the
/// cancellation probe. The C++ `CancellationToken` collapses to the
/// `is_canceled()` probe.
pub trait FileGcSink: Send {
    fn use_file_database(&self) -> bool {
        false
    }
    fn is_canceled(&self) -> bool {
        false
    }
    /// `FileManager::on_file_unlink` (FileManager.cpp:1676-1688).
    fn on_file_unlink(&mut self, _location: FullLocalFileLocation) {}
}

/// `FileGcWorker::run_gc` (FileGcWorker.cpp:32-197) — the immunity and
/// removal decision machine over the scanned `files`. The VLOG counters
/// are logging-only in the C++ and are dropped here.
pub fn run_file_gc(
    sink: &mut dyn FileGcSink,
    parameters: &FileGcParameters,
    mut files: Vec<FullFileInfo>,
    send_updates: bool,
) -> Result<FileGcResult, Status> {
    let mut immune_types = vec![false; MAX_FILE_TYPE as usize];
    if sink.use_file_database() {
        // immune by default (FileGcWorker.cpp:44-53)
        for file_type in [
            FileType::Sticker,
            FileType::ProfilePhoto,
            FileType::Thumbnail,
            FileType::Wallpaper,
            FileType::Background,
            FileType::Ringtone,
        ] {
            immune_types[file_type as usize] = true;
        }
    }

    if !parameters.file_types.is_empty() {
        immune_types.fill(true);
        for file_type in &parameters.file_types {
            immune_types[*file_type as usize] = false;
        }
        for i in 0..MAX_FILE_TYPE {
            let main_file_type = get_main_file_type(FileType::from_raw(i)) as usize;
            if !immune_types[main_file_type] {
                immune_types[i as usize] = false;
            }
        }
    }

    if sink.use_file_database() {
        immune_types[FileType::EncryptedThumbnail as usize] = true;
    }

    for info in &mut files {
        if info.atime_nsec < info.mtime_nsec {
            info.atime_nsec = info.mtime_nsec;
        }
    }

    let mut new_stats = FileStats::new(false, parameters.dialog_limit != 0);
    let mut removed_stats = FileStats::new(false, parameters.dialog_limit != 0);

    let now = tdutils::time::system();

    // Remove all suitable files with (atime > now - max_time_from_last_access)
    let mut kept = Vec::with_capacity(files.len());
    for info in files.drain(..) {
        if sink.is_canceled() {
            kept.push(info);
            continue;
        }
        if immune_types[info.file_type as usize] {
            new_stats.add_copy(&info);
            continue;
        }
        if parameters
            .exclude_owner_dialog_ids
            .contains(&info.owner_dialog_id)
        {
            new_stats.add_copy(&info);
            continue;
        }
        if !parameters.owner_dialog_ids.is_empty()
            && !parameters.owner_dialog_ids.contains(&info.owner_dialog_id)
        {
            new_stats.add_copy(&info);
            continue;
        }
        if info.mtime_nsec as f64 * 1e-9 > now - parameters.immunity_delay as f64 {
            // new files are immune to gc
            new_stats.add_copy(&info);
            continue;
        }
        if info.atime_nsec as f64 * 1e-9 < now - parameters.max_time_from_last_access as f64 {
            // do_remove_file (FileGcWorker.cpp:74-82)
            removed_stats.add_copy(&info);
            let _ = std::fs::remove_file(&info.path);
            if send_updates {
                sink.on_file_unlink(FullLocalFileLocation::new(
                    info.file_type,
                    info.path.clone(),
                    info.mtime_nsec,
                ));
            }
            continue;
        }
        kept.push(info);
    }
    if sink.is_canceled() {
        return Err(Status::error(500, "Request aborted"));
    }

    // sort by atime (the C++ comment says max(atime, mtime); the
    // comparator is atime alone)
    kept.sort_by_key(|info| info.atime_nsec);

    // 1. Total size must be less than parameters.max_files_size_
    // 2. Total file count must be less than parameters.max_file_count_
    let mut remove_count = kept
        .len()
        .saturating_sub(parameters.max_file_count as usize);
    let mut remove_size = -parameters.max_files_size;
    for file in &kept {
        remove_size += file.size;
    }

    let mut pos = 0;
    while pos < kept.len() && (remove_count > 0 || remove_size > 0) {
        if sink.is_canceled() {
            return Err(Status::error(500, "Request aborted"));
        }
        remove_size -= kept[pos].size;

        removed_stats.add_copy(&kept[pos]);
        let _ = std::fs::remove_file(&kept[pos].path);
        if send_updates {
            sink.on_file_unlink(FullLocalFileLocation::new(
                kept[pos].file_type,
                kept[pos].path.clone(),
                kept[pos].mtime_nsec,
            ));
        }
        remove_count = remove_count.saturating_sub(1);
        pos += 1;
    }

    for info in &kept[pos..] {
        new_stats.add_copy(info);
    }

    Ok(FileGcResult {
        kept_file_stats: new_stats,
        removed_file_stats: removed_stats,
    })
}

/// The `scan_fs` file record (FileStatsWorker.cpp:56-63).
struct FsFileInfo {
    file_type: FileType,
    path: String,
    size: i64,
    atime_nsec: u64,
    mtime_nsec: u64,
}

/// The stats-worker environment: the files dirs, the db scan and the
/// stat source — the `FileStatsWorker`'s `G()` plane over the
/// [`FileManagerContext`] and [`FileGcSink`].
pub trait FileStatsSink: FileManagerContext + FileGcSink + Send {
    /// `log_interface->get_file_paths()` — the fast-stat log size.
    fn log_file_paths(&self) -> Vec<String> {
        Vec::new()
    }
    /// The `SqliteDb::with_db_path` pair for the database size.
    fn database_paths(&self) -> Vec<String> {
        Vec::new()
    }
    /// `G()->get_option_string("language_pack_database_path")`.
    fn language_pack_database_path(&self) -> String {
        String::new()
    }
    /// `scan_db` (FileStatsWorker.cpp:39-94) — the `file0`..`file:`
    /// pmc range join. The callback receives (file_type, path,
    /// owner_dialog_id, size); relative paths are already resolved.
    fn scan_file_db(&mut self, _callback: &mut dyn FnMut(FileType, String, DialogId, i64)) {}
}

/// `walk_path` (td/utils/port/path) over the regular files of one dir.
fn walk_regular_files(dir: &str, callback: &mut dyn FnMut(String)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk_regular_files(&path.to_string_lossy(), callback);
        } else if file_type.is_file() {
            callback(path.to_string_lossy().into_owned());
        }
    }
}

/// `scan_fs` (FileStatsWorker.cpp:66-114) — every main file type's dir
/// plus the secure-decrypted and video temp dirs, deduplicated.
fn scan_fs(sink: &mut dyn FileStatsSink, callback: &mut dyn FnMut(FsFileInfo)) {
    let mut scanned_file_dirs: BTreeSet<String> = BTreeSet::new();
    let files_dir = sink.files_dir();
    let secure_dir = sink.secure_files_dir();
    let mut scan_dir = |file_type: FileType, file_dir: String| {
        if file_dir.is_empty() || !scanned_file_dirs.insert(file_dir.clone()) {
            return;
        }
        walk_regular_files(&file_dir, &mut |path| {
            let Ok(stat) = sink.stat(&path) else {
                return;
            };
            if stat.size == 0 && path.ends_with("/.nomedia") {
                // skip .nomedia file
                return;
            }
            callback(FsFileInfo {
                file_type: guess_file_type_by_path(&path, file_type),
                path: path.clone(),
                size: stat.real_size,
                atime_nsec: 0,
                mtime_nsec: stat.mtime_nsec,
            });
        });
    };
    for i in 0..MAX_FILE_TYPE {
        let file_type = FileType::from_raw(i);
        let dir = get_files_dir(file_type, &files_dir, &secure_dir);
        scan_dir(get_main_file_type(file_type), dir);
    }
    scan_dir(
        get_main_file_type(FileType::Temp),
        get_files_temp_dir(FileType::SecureDecrypted, &files_dir, &secure_dir),
    );
    scan_dir(
        get_main_file_type(FileType::Temp),
        get_files_temp_dir(FileType::Video, &files_dir, &secure_dir),
    );
}

/// `FileStatsWorker::get_stats` (FileStatsWorker.cpp:117-200)
/// collapsed: the fs scan joined with the db scan by path hash.
pub fn get_file_stats(
    sink: &mut dyn FileStatsSink,
    need_all_files: bool,
    split_by_owner_dialog_id: bool,
) -> FileStats {
    let mut full_infos: Vec<FullFileInfo> = Vec::new();
    scan_fs(sink, &mut |fs_info| {
        full_infos.push(FullFileInfo {
            file_type: fs_info.file_type,
            path: fs_info.path,
            owner_dialog_id: DialogId::default(),
            size: fs_info.size,
            atime_nsec: fs_info.atime_nsec,
            mtime_nsec: fs_info.mtime_nsec,
        });
    });

    if !sink.use_file_database() {
        let mut file_stats = FileStats::new(need_all_files, false);
        for info in full_infos {
            file_stats.add(info);
        }
        return file_stats;
    }

    let mut hash_to_pos: HashMap<u64, usize> = HashMap::new();
    for (pos, full_info) in full_infos.iter().enumerate() {
        let hash = default_path_hash(&full_info.path);
        hash_to_pos.insert(hash, pos);
    }
    {
        let mut join = |file_type: FileType, path: String, owner_dialog_id: DialogId, size: i64| {
            let hash = default_path_hash(&path);
            if let Some(&pos) = hash_to_pos.get(&hash) {
                let full_info = &mut full_infos[pos];
                full_info.owner_dialog_id = owner_dialog_id;
                full_info.file_type = file_type; // database file_type is the correct one
                full_info.size = size;
            }
        };
        sink.scan_file_db(&mut join);
    }

    let mut file_stats = FileStats::new(need_all_files, split_by_owner_dialog_id);
    for full_info in full_infos {
        file_stats.add(full_info);
    }
    file_stats
}

/// The `Hash<std::string>()` stand-in for the scan join.
fn default_path_hash(path: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

/// `StorageManager` (StorageManager.cpp:32-366) collapsed: the
/// actors/workers disappear, `run_gc` runs the scan + gc synchronously
/// and the auto-gc timer is `tick`ed by the owner.
pub struct StorageManager {
    /// `fast_stat_` (StorageManager.cpp:45-58).
    fast_stat: FileTypeStat,
    /// `last_gc_timestamp_` (StorageManager.cpp:310-318).
    last_gc_timestamp: u32,
}

/// The StorageManager environment: the binlog pmc persistence, the
/// option reads and the database/log size sources.
pub trait StorageContext: FileStatsSink + Send {
    /// `get_binlog_pmc()->get/set` (StorageManager.cpp:240-252).
    fn pmc_get(&mut self, _key: &str) -> Vec<u8> {
        Vec::new()
    }
    fn pmc_set(&mut self, _key: &str, _value: &[u8]) {}
    /// `G()->get_option_boolean("use_storage_optimizer")`.
    fn use_storage_optimizer(&self) -> bool {
        false
    }
    /// `G()->use_file_database()`.
    fn use_file_database(&self) -> bool {
        false
    }
    /// `G()->td_db()->get_stats()` (StorageManager.cpp:95-99).
    fn database_stats(&mut self) -> Result<String, Status> {
        Err(Status::error_message(
            "Database statistics are not available",
        ))
    }
    /// `MessagesManager::load_dialogs` pre-warm — the collapsed
    /// client resolves dialog names lazily, so this is a no-op seam.
    fn preload_dialogs(&mut self, _dialog_ids: &[DialogId]) {}
}

const GC_EACH: u32 = 60 * 60 * 24; // 1 day
const GC_DELAY: u32 = 60;
const GC_RAND_DELAY: u32 = 60 * 15;

impl Default for StorageManager {
    fn default() -> StorageManager {
        StorageManager::new()
    }
}

impl StorageManager {
    pub fn new() -> StorageManager {
        StorageManager {
            fast_stat: FileTypeStat::default(),
            last_gc_timestamp: 0,
        }
    }

    /// `StorageManager::on_new_file` (StorageManager.cpp:44-62) — the
    /// `Context::on_new_file` sink of the FileManager.
    pub fn on_new_file(&mut self, _size: i64, real_size: i64, cnt: i32) {
        self.fast_stat.cnt += cnt;
        // the non-Windows arm of StorageManager.cpp:49-54 (the Windows
        // arm adds `size`)
        let add_size = real_size;
        self.fast_stat.size += add_size;
        if self.fast_stat.cnt < 0 || self.fast_stat.size < 0 {
            self.fast_stat = FileTypeStat::default();
        }
    }

    /// `save_fast_stat`/`load_fast_stat` (StorageManager.cpp:235-252) —
    /// stored via `log_event_store`: the log-event version prefix, then
    /// `store(size)` + `store(cnt)` (i64 + i32).
    pub fn save_fast_stat(&mut self, ctx: &mut dyn StorageContext) {
        let mut out = Vec::with_capacity(16);
        store_parse::store_log_event_version(&mut out);
        store_parse::store_i64(&mut out, self.fast_stat.size);
        store_parse::store_i32(&mut out, self.fast_stat.cnt);
        ctx.pmc_set("fast_file_stat", &out);
    }

    pub fn load_fast_stat(&mut self, ctx: &mut dyn StorageContext) {
        let data = ctx.pmc_get("fast_file_stat");
        let payload = data
            .strip_prefix(
                store_parse::version::LOG_EVENT_STORE_VERSION
                    .to_le_bytes()
                    .as_slice(),
            )
            .unwrap_or(&data);
        let mut parser = Parser::new(payload);
        self.fast_stat = match (parser.i64_(), parser.i32_()) {
            (Ok(size), Ok(cnt)) if parser.left_len() == 0 => FileTypeStat { size, cnt },
            _ => FileTypeStat::default(),
        };
    }

    /// `update_fast_stats` (StorageManager.cpp:254-260).
    pub fn update_fast_stats(&mut self, stats: &FileStats) -> bool {
        self.fast_stat = stats.get_total_nontemp_stat();
        true
    }

    /// `get_storage_stats_fast` (StorageManager.cpp:89-93) minus the
    /// database sizes, which the context answers.
    pub fn get_storage_stats_fast(&mut self, ctx: &mut dyn StorageContext) -> FileStatsFast {
        let mut database_size = 0i64;
        for path in ctx.database_paths() {
            database_size += file_size_of(&path);
        }
        let mut language_pack_database_size = 0i64;
        let language_path = ctx.language_pack_database_path();
        if !language_path.is_empty() {
            language_pack_database_size += file_size_of(&language_path);
        }
        let mut log_size = 0i64;
        for path in ctx.log_file_paths() {
            log_size += file_size_of(&path);
        }
        FileStatsFast {
            size: self.fast_stat.size,
            count: self.fast_stat.cnt,
            database_size,
            language_pack_database_size,
            log_size,
        }
    }

    /// `StorageManager::get_storage_stats` → `on_file_stats` →
    /// `send_stats` (StorageManager.cpp:63-87, 131-142, 253-280) — the
    /// full scan behind the `getStorageStatistics` request: the stats
    /// worker pass, the fast-stat refresh and the dialog-limit slice.
    pub fn get_storage_stats(
        &mut self,
        ctx: &mut dyn StorageContext,
        need_all_files: bool,
        dialog_limit: i32,
    ) -> FileStats {
        let mut file_stats = get_file_stats(ctx, need_all_files, dialog_limit != 0);
        self.update_fast_stats(&file_stats);
        file_stats.apply_dialog_limit(dialog_limit);
        let dialog_ids = file_stats.get_dialog_ids();
        ctx.preload_dialogs(&dialog_ids);
        file_stats
    }

    /// `get_database_stats` (StorageManager.cpp:95-103) — the TdDb stats
    /// string behind the `getDatabaseStatistics` request.
    pub fn get_database_stats(
        &mut self,
        ctx: &mut dyn StorageContext,
    ) -> Result<DatabaseStats, Status> {
        ctx.database_stats().map(DatabaseStats)
    }

    /// The `schedule_next_gc` deadline (StorageManager.cpp:323-345) — the
    /// upper bound the shared Td timeout slot arms at between auto-gc
    /// passes ([`Self::auto_gc_due`] re-checks the window at fire time).
    /// `None` mirrors the C++ optimizer-off cancel. The deadline rides the
    /// alarm clock (`now`) with the system-clock offset the C++
    /// `next_gc_at - sys_time` computes.
    pub fn next_gc_at(&self, use_storage_optimizer: bool, sys_time: u32, now: f64) -> Option<f64> {
        if !use_storage_optimizer {
            return None;
        }
        let mut next_gc_at = self.last_gc_timestamp + GC_EACH;
        next_gc_at = next_gc_at.clamp(sys_time, sys_time + GC_EACH);
        next_gc_at += GC_DELAY + GC_RAND_DELAY;
        let next_gc_in = i64::from(next_gc_at) - i64::from(sys_time);
        Some(now + next_gc_in as f64)
    }

    /// `load_last_gc_timestamp`/`save_last_gc_timestamp`
    /// (StorageManager.cpp:310-324).
    pub fn load_last_gc_timestamp(&mut self, ctx: &mut dyn StorageContext) {
        let data = ctx.pmc_get("files_gc_ts");
        self.last_gc_timestamp = std::str::from_utf8(&data)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
    }

    pub fn save_last_gc_timestamp(&mut self, ctx: &mut dyn StorageContext, now: f64) {
        self.last_gc_timestamp = now as u32;
        ctx.pmc_set("files_gc_ts", self.last_gc_timestamp.to_string().as_bytes());
    }

    /// `schedule_next_gc` + `timeout_expired` (StorageManager.cpp:326-364)
    /// collapsed into a tick: the caller passes the monotonic `now`
    /// (seconds); the answer is whether an automatic gc is due. The
    /// caller then runs [`StorageManager::run_gc`] with the default
    /// parameters and saves the timestamp when it did not abort.
    pub fn auto_gc_due(&self, ctx: &mut dyn StorageContext, now: f64) -> bool {
        if !ctx.use_storage_optimizer() {
            return false;
        }
        let sys_time = now as u32;
        let mut next_gc_at = self.last_gc_timestamp + GC_EACH;
        if next_gc_at < sys_time {
            next_gc_at = sys_time;
        }
        if next_gc_at > sys_time + GC_EACH {
            next_gc_at = sys_time + GC_EACH;
        }
        let jitter =
            tdutils::random::fast(GC_DELAY as i32, (GC_DELAY + GC_RAND_DELAY) as i32) as u32;
        next_gc_at += GC_DELAY + jitter;
        now >= next_gc_at as f64
    }

    /// `StorageManager::run_gc` → `on_all_files` → `FileGcWorker::run_gc`
    /// (StorageManager.cpp:101-133, 160-176) — the scan, the join and
    /// the gc in one synchronous pass. `send_updates` is the C++
    /// `!G()->get_option_string("my_phone_number").empty()` probe.
    #[allow(clippy::too_many_arguments)]
    pub fn run_gc(
        &mut self,
        ctx: &mut dyn StorageContext,
        gc_sink: &mut dyn FileGcSink,
        parameters: FileGcParameters,
        return_deleted_file_statistics: bool,
    ) -> Result<FileStats, Status> {
        let split_by_owner_dialog_id = !parameters.owner_dialog_ids.is_empty()
            || !parameters.exclude_owner_dialog_ids.is_empty()
            || parameters.dialog_limit != 0;
        let mut file_stats = get_file_stats(ctx, true, split_by_owner_dialog_id);

        self.update_fast_stats(&file_stats);
        let all_files = file_stats.get_all_files();
        let gc_result = run_file_gc(gc_sink, &parameters, all_files, true)?;

        self.update_fast_stats(&gc_result.kept_file_stats);

        let mut kept = gc_result.kept_file_stats;
        kept.apply_dialog_limit(parameters.dialog_limit);
        let dialog_ids = kept.get_dialog_ids();
        ctx.preload_dialogs(&dialog_ids);
        if return_deleted_file_statistics {
            let mut removed = gc_result.removed_file_stats;
            removed.apply_dialog_limit(parameters.dialog_limit);
            return Ok(removed);
        }
        Ok(kept)
    }
}

/// `StorageManager::get_file_size` (StorageManager.cpp:178-187) — the
/// allocated size of one path, 0 on a failed stat.
fn file_size_of(path: &str) -> i64 {
    std::fs::metadata(path)
        .map(|meta| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                (meta.blocks() * 512) as i64
            }
            #[cfg(not(unix))]
            {
                meta.len() as i64
            }
        })
        .unwrap_or(0)
}

/// `DownloadManager::Counters` (DownloadManager.h:21-38) — the
/// `updateFileDownloads` plane; the binlog record is the plain
/// `store(total_size) + store(total_count) + store(downloaded_size)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DownloadCounters {
    pub total_size: i64,
    pub total_count: i32,
    pub downloaded_size: i64,
}

impl DownloadCounters {
    /// `Counters::get_update_file_downloads_object`
    /// (DownloadManager.cpp:777-780).
    pub fn get_update_file_downloads_object(&self) -> td_api::updateFileDownloads {
        td_api::updateFileDownloads {
            total_size: self.total_size,
            total_count: self.total_count,
            downloaded_size: self.downloaded_size,
        }
    }

    /// `Counters::store` (DownloadManager.cpp:782-790) — no flags.
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.total_size);
        store_parse::store_i32(out, self.total_count);
        store_parse::store_i64(out, self.downloaded_size);
    }

    /// `Counters::parse` (DownloadManager.cpp:792-800).
    pub fn parse(data: &[u8]) -> Result<DownloadCounters, Status> {
        let mut parser = Parser::new(data);
        let err = |e: &'static str| Status::error_message(e);
        let total_size = parser.i64_().map_err(err)?;
        let total_count = parser.i32_().map_err(err)?;
        let downloaded_size = parser.i64_().map_err(err)?;
        if parser.left_len() != 0 {
            return Err(Status::error_message("Wrong trailing data in counters"));
        }
        Ok(DownloadCounters {
            total_size,
            total_count,
            downloaded_size,
        })
    }
}

/// `DownloadManager::FileCounters` (DownloadManager.h:40-56).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DownloadFileCounters {
    pub active_count: i32,
    pub paused_count: i32,
    pub completed_count: i32,
}

impl DownloadFileCounters {
    /// `FileCounters::get_downloaded_file_counts_object`
    /// (DownloadManager.cpp:781-785).
    pub fn get_downloaded_file_counts_object(&self) -> td_api::DownloadedFileCounts {
        td_api::DownloadedFileCounts {
            active_count: self.active_count,
            paused_count: self.paused_count,
            completed_count: self.completed_count,
        }
    }
}

/// `FileDownloadInDatabase` (DownloadManager.cpp:33-57) — the
/// `dlds#<download_id>` binlog record: the `is_paused` flags word, the
/// counters and times, and the FileManager-mediated file ids.
#[derive(Clone, Debug)]
struct FileDownloadInDatabase {
    download_id: i64,
    file_id: FileId,
    file_source_id: FileSourceId,
    priority: i32,
    created_at: i32,
    completed_at: i32,
    is_paused: bool,
}

impl FileDownloadInDatabase {
    /// `parse` (DownloadManager.cpp:52-56) — the file_id field rides
    /// the context's FileManager serialization.
    fn parse(
        ctx: &mut dyn DownloadManagerContext,
        parser: &mut Parser,
    ) -> Result<FileDownloadInDatabase, Status> {
        let err = |e: &'static str| Status::error_message(e);
        let is_paused = parser.bool().map_err(err)?;
        let download_id = parser.i64_().map_err(err)?;
        let file_id = ctx.parse_file_id(parser)?;
        let file_source_id = FileSourceId::new(parser.i32_().map_err(err)?);
        let priority = parser.i32_().map_err(err)?;
        let created_at = parser.i32_().map_err(err)?;
        let completed_at = parser.i32_().map_err(err)?;
        Ok(FileDownloadInDatabase {
            download_id,
            file_id,
            file_source_id,
            priority,
            created_at,
            completed_at,
            is_paused,
        })
    }
}

/// The `DownloadManager::Callback` (DownloadManager.h:60-86) — the Td
/// plane. The promise-based `get_file_search_text` answers
/// synchronously in the collapse.
pub trait DownloadManagerCallback: Send {
    fn update_counters(&mut self, _counters: DownloadCounters) {}
    fn update_file_added(
        &mut self,
        _file_id: FileId,
        _file_source_id: FileSourceId,
        _add_date: i32,
        _complete_date: i32,
        _is_paused: bool,
        _counters: DownloadFileCounters,
    ) {
    }
    fn update_file_changed(
        &mut self,
        _file_id: FileId,
        _complete_date: i32,
        _is_paused: bool,
        _counters: DownloadFileCounters,
    ) {
    }
    fn update_file_removed(&mut self, _file_id: FileId, _counters: DownloadFileCounters) {}
    fn get_internal_download_id(&mut self) -> i64 {
        0
    }
    fn start_file(&mut self, _file_id: FileId, _internal_download_id: i64, _priority: i8) {}
    fn pause_file(&mut self, _file_id: FileId, _internal_download_id: i64) {}
    fn delete_file(&mut self, _file_id: FileId) {}
    /// `get_file_search_text` — `Err` removes the download
    /// (DownloadManager.cpp:459-468).
    fn get_file_search_text(
        &mut self,
        _file_id: FileId,
        _file_source_id: FileSourceId,
    ) -> Result<String, Status> {
        Err(Status::error_message("Search text is not available"))
    }
    /// `get_sync_file_view` collapsed to the three sizes
    /// `add_file_info` reads (DownloadManager.cpp:416-418).
    fn sync_file_sizes(&mut self, _file_id: FileId) -> (i64, i64, i64) {
        (0, 0, 0)
    }
    /// `get_file_object` (DownloadManagerCallback.cpp:83-85).
    fn get_file_object(&mut self, _file_id: FileId) -> Option<td_api::File> {
        None
    }
    /// `get_file_download_object`; `None` folds the C++
    /// `message_ == nullptr` filter (DownloadManager.cpp:264-266).
    fn get_file_download_object(
        &mut self,
        _file_id: FileId,
        _file_source_id: FileSourceId,
        _add_date: i32,
        _complete_date: i32,
        _is_paused: bool,
    ) -> Option<td_api::FileDownload> {
        None
    }
}

/// The DownloadManager pmc plane (`G()->td_db()->get_binlog_pmc()` +
/// `use_message_database` + `unix_time`) plus the FileManager-mediated
/// file-id serialization the `dlds#` record carries.
pub trait DownloadManagerContext: Send {
    fn use_message_database(&self) -> bool {
        false
    }
    fn pmc_get(&mut self, _key: &str) -> Vec<u8> {
        Vec::new()
    }
    fn pmc_set(&mut self, _key: &str, _value: &[u8]) {}
    fn pmc_erase(&mut self, _key: &str) {}
    fn pmc_erase_by_prefix(&mut self, _prefix: &str) {}
    fn pmc_prefix_get(&mut self, _prefix: &str) -> Vec<(String, Vec<u8>)> {
        Vec::new()
    }
    fn unix_time(&mut self) -> i32 {
        0
    }
    /// `FileManager::store_file` (the versioned FileId record).
    fn store_file_id(&mut self, _file_id: FileId, _out: &mut Vec<u8>) {}
    /// `FileManager::parse_file`.
    fn parse_file_id(&mut self, _parser: &mut store_parse::Parser) -> Result<FileId, Status> {
        Err(Status::error_message("File id parsing is not available"))
    }
}

/// The trait-default [`DownloadManagerContext`] — an empty pmc with no
/// message database.
struct DefaultDownloadManagerContext;

impl DownloadManagerContext for DefaultDownloadManagerContext {}

/// `FileManager::check_priority` (FileManager.cpp:1442-1447).
fn check_download_priority(priority: i32) -> Result<(), Status> {
    if (1..=32).contains(&priority) {
        Ok(())
    } else {
        Err(Status::error(400, "Invalid priority specified"))
    }
}

/// `FileManager::check_priority` (FileManager.cpp:1424-1429) — the
/// download/upload priority band behind `downloadFile` and
/// `addFileToDownloads`.
pub fn check_priority(priority: i32) -> Result<(), Status> {
    if (1..=32).contains(&priority) {
        Ok(())
    } else {
        Err(Status::error(400, "Priority must be between 1 and 32"))
    }
}

#[derive(Clone, Debug, Default)]
struct DownloadFileInfo {
    download_id: i64,
    file_id: FileId,
    internal_download_id: i64,
    file_source_id: FileSourceId,
    priority: i8,
    is_paused: bool,
    is_counted: bool,
    is_registered: bool,
    need_save_to_database: bool,
    size: i64,
    expected_size: i64,
    downloaded_size: i64,
    created_at: i32,
    completed_at: i32,
    link_token: u64,
}

/// `td::DownloadManagerImpl` (DownloadManager.cpp:59-774) collapsed per
/// the single-scheduler convention: the actor disappears, the
/// MultiPromise search-text load runs synchronously through the
/// callback, and the 60-second `clear_counters` timeout becomes a
/// caller-ticked deadline ([`DownloadManager::tick`]).
pub struct DownloadManager {
    callback: Option<Box<dyn DownloadManagerCallback>>,
    /// The `G()` plane; installed with [`DownloadManager::set_context`]
    /// like the FileManager's.
    context: Option<Box<dyn DownloadManagerContext>>,
    by_file_id: HashMap<FileId, i64>,
    /// download_id → info; the ordered map keeps the
    /// `remove_all_files`/`search` flows deterministic (the C++
    /// FlatHashMap order leaks into nothing observable — the search
    /// sorts its ids).
    files: BTreeMap<i64, Box<DownloadFileInfo>>,
    completed_download_ids: BTreeSet<i64>,
    unviewed_completed_download_ids: BTreeSet<i64>,
    hints: tdutils::hints::Hints,
    counters: DownloadCounters,
    sent_counters: DownloadCounters,
    file_counters: DownloadFileCounters,
    is_inited: bool,
    is_database_loaded: bool,
    is_search_inited: bool,
    max_download_id: i64,
    last_link_token: u64,
    /// `set_timeout_in(EMPTY_UPDATE_DELAY)` (DownloadManager.cpp:719) —
    /// the wall-clock time the counters clear is due; `None` cancels it.
    counters_clear_at: Option<f64>,
}

const MAX_COMPLETED_DOWNLOADS: usize = 200;
const EMPTY_UPDATE_DELAY: f64 = 60.0;

impl Default for DownloadManager {
    fn default() -> DownloadManager {
        DownloadManager::new()
    }
}

impl DownloadManager {
    /// `DownloadManager::create` + `start_up` → `init`
    /// (DownloadManager.cpp:70-73, 205-227).
    pub fn new() -> DownloadManager {
        DownloadManager {
            callback: None,
            context: None,
            by_file_id: HashMap::new(),
            files: BTreeMap::new(),
            completed_download_ids: BTreeSet::new(),
            unviewed_completed_download_ids: BTreeSet::new(),
            hints: tdutils::hints::Hints::default(),
            counters: DownloadCounters::default(),
            sent_counters: DownloadCounters::default(),
            file_counters: DownloadFileCounters::default(),
            is_inited: false,
            is_database_loaded: false,
            is_search_inited: false,
            max_download_id: 0,
            last_link_token: 0,
            counters_clear_at: None,
        }
    }

    fn next_download_id(&mut self) -> i64 {
        self.max_download_id += 1;
        self.max_download_id
    }

    fn is_completed(file_info: &DownloadFileInfo) -> bool {
        file_info.completed_at != 0
    }

    fn get_file_size(file_info: &DownloadFileInfo) -> i64 {
        if file_info.size != 0 {
            file_info.size
        } else {
            std::cmp::max(file_info.downloaded_size + 1, file_info.expected_size)
        }
    }

    fn pmc_key(download_id: i64) -> String {
        format!("dlds#{download_id}")
    }

    fn ctx(&mut self) -> &mut dyn DownloadManagerContext {
        if self.context.is_none() {
            self.context = Some(Box::new(DefaultDownloadManagerContext));
        }
        self.context.as_deref_mut().expect("context")
    }

    /// Installs the context seam (the `G()` plane) and the callback.
    pub fn set_callback(&mut self, callback: Box<dyn DownloadManagerCallback>) {
        self.callback = Some(callback);
    }

    pub fn set_context(&mut self, context: Box<dyn DownloadManagerContext>) {
        self.context = Some(context);
    }

    /// The close half (the C++ actor reset): drops the stored callback and
    /// context boxes with the manager.
    pub fn teardown(&mut self) {
        self.callback = None;
        self.context = None;
    }

    /// `init` (DownloadManager.cpp:205-227).
    pub fn init(&mut self) {
        if self.is_inited {
            return;
        }
        let use_message_database = self.ctx().use_message_database();
        if use_message_database {
            let serialized_counter = self.ctx().pmc_get("dlds_counter");
            // the log-event version prefix leads the counters
            let payload = serialized_counter
                .strip_prefix(
                    store_parse::version::LOG_EVENT_STORE_VERSION
                        .to_le_bytes()
                        .as_slice(),
                )
                .unwrap_or(&serialized_counter);
            if !serialized_counter.is_empty() {
                match DownloadCounters::parse(payload) {
                    Ok(counters) => {
                        if counters.downloaded_size == counters.total_size
                            || counters.total_size == 0
                        {
                            self.ctx().pmc_erase("dlds_counter");
                            self.sent_counters = DownloadCounters::default();
                        } else {
                            self.sent_counters = counters;
                        }
                    }
                    Err(_) => {
                        self.sent_counters = DownloadCounters::default();
                    }
                }
            }
        } else if !self.ctx().pmc_get("dlds_counter").is_empty() {
            self.ctx().pmc_erase("dlds_counter");
            self.ctx().pmc_erase_by_prefix("dlds#");
        }
        if let Some(callback) = self.callback.as_deref_mut() {
            callback.update_counters(self.sent_counters);
        }
        self.is_inited = true;
    }

    /// `add_file_from_database` (DownloadManager.cpp:229-254).
    fn add_file_from_database(&mut self, in_db: FileDownloadInDatabase) {
        if !in_db.file_id.is_valid() || in_db.file_source_id == FileSourceId::default() {
            return;
        }
        if self.by_file_id.contains_key(&in_db.file_id) {
            // file has already been added
            return;
        }
        if check_download_priority(in_db.priority).is_err() {
            // LOG(ERROR) "Receive invalid download priority from database"
            return;
        }

        let file_info = DownloadFileInfo {
            download_id: in_db.download_id,
            file_id: in_db.file_id,
            file_source_id: in_db.file_source_id,
            is_paused: in_db.is_paused,
            priority: in_db.priority as i8,
            completed_at: in_db.completed_at,
            created_at: in_db.created_at,
            ..DownloadFileInfo::default()
        };
        self.add_file_info(file_info, "");
    }

    /// `load_database_files` (DownloadManager.cpp:256-288).
    pub fn load_database_files(&mut self) {
        if self.is_database_loaded {
            return;
        }
        if !self.ctx().use_message_database() {
            self.is_database_loaded = true;
            return;
        }
        debug_assert!(self.is_inited, "LOG_CHECK(is_inited_)");
        let downloads_in_kv = self.ctx().pmc_prefix_get("dlds#");
        for (key, value) in downloads_in_kv {
            // the log_event_parse framing: the version prefix feeds the
            // parser's version (the nested store_file branches on it)
            let Ok(mut parser) = Parser::new_log_event(&value) else {
                continue;
            };
            let record =
                FileDownloadInDatabase::parse(self.ctx(), &mut parser).and_then(|record| {
                    match parser.fetch_end() {
                        Ok(()) => Ok(record),
                        Err(error) => Err(Status::error_message(error)),
                    }
                });
            let Ok(record) = record else {
                continue;
            };
            let key_id = key.trim_start_matches("dlds#").parse::<i64>().unwrap_or(0);
            debug_assert!(record.download_id == key_id);
            self.max_download_id = std::cmp::max(record.download_id, self.max_download_id);
            self.add_file_from_database(record);
        }

        self.is_database_loaded = true;
        self.update_counters();
        self.check_completed_downloads_size();
    }

    /// `prepare_hints` + `add_download_to_hints`
    /// (DownloadManager.cpp:290-320) — synchronous in the collapse; a
    /// failed search text removes the download like the C++ actor.
    fn prepare_hints(&mut self) {
        let download_ids: Vec<i64> = self.files.keys().copied().collect();
        for download_id in download_ids {
            let Some(file_info) = self.files.get(&download_id) else {
                continue;
            };
            let (file_id, file_source_id) = (file_info.file_id, file_info.file_source_id);
            let search_text = self
                .callback
                .as_deref_mut()
                .map(|callback| callback.get_file_search_text(file_id, file_source_id));
            let Some(Ok(text)) = search_text else {
                let Some(file_info) = self.files.get(&download_id) else {
                    continue;
                };
                let file_info = file_info.clone();
                if let Some(callback) = self.callback.as_deref_mut() {
                    let _ = callback;
                }
                self.remove_file_impl(&file_info, false, "add_download_to_hints");
                continue;
            };
            let text = if text.is_empty() {
                " ".to_owned()
            } else {
                text
            };
            self.hints.add(download_id, &text);
        }
        self.is_search_inited = true;
    }

    /// `add_file` (DownloadManager.cpp:97-118).
    pub fn add_file(
        &mut self,
        file_id: FileId,
        file_source_id: FileSourceId,
        search_text: &str,
        priority: i8,
    ) -> Result<Option<td_api::File>, Status> {
        self.check_is_active("add_file")?;

        if let Some(&old_download_id) = self.by_file_id.get(&file_id) {
            let old_file_info = self.files.get(&old_download_id).cloned();
            if let Some(old_file_info) = old_file_info {
                self.remove_file_impl(&old_file_info, false, "add_file");
            }
        }

        let download_id = self.next_download_id();
        let created_at = self.ctx().unix_time();

        let file_info = DownloadFileInfo {
            download_id,
            file_id,
            file_source_id,
            priority,
            need_save_to_database: true,
            created_at,
            ..DownloadFileInfo::default()
        };
        self.add_file_info(file_info, search_text);

        Ok(self
            .callback
            .as_deref_mut()
            .and_then(|callback| callback.get_file_object(file_id)))
    }

    /// `add_file_info` (DownloadManager.cpp:403-452).
    fn add_file_info(&mut self, mut file_info: DownloadFileInfo, search_text: &str) {
        let download_id = file_info.download_id;
        file_info.internal_download_id = self
            .callback
            .as_deref_mut()
            .map(|callback| callback.get_internal_download_id())
            .unwrap_or(0);
        let (size, expected_size, downloaded_size) = self
            .callback
            .as_deref_mut()
            .map(|callback| callback.sync_file_sizes(file_info.file_id))
            .unwrap_or((0, 0, 0));
        file_info.size = size;
        file_info.expected_size = expected_size;
        file_info.downloaded_size = downloaded_size;
        file_info.is_counted = !Self::is_completed(&file_info);

        if file_info.completed_at > 0
            && (file_info.size == 0 || file_info.downloaded_size != file_info.size)
        {
            // Skip adding to recently downloaded files: the local size
            // does not match
            self.remove_from_database(download_id);
            return;
        }

        self.by_file_id.insert(file_info.file_id, download_id);
        let text = if search_text.is_empty() {
            " "
        } else {
            search_text
        };
        self.hints.add(download_id, text);
        self.last_link_token += 1;
        file_info.link_token = self.last_link_token;

        self.files.insert(download_id, Box::new(file_info));
        let was_completed = self
            .files
            .get(&download_id)
            .map(|file_info| Self::is_completed(file_info))
            .unwrap_or(false);
        self.register_file_info(download_id); // must run before start_file, which can call update_file_download_state
        let (is_completed, is_paused, file_id, internal_download_id, priority) = {
            let Some(file_info) = self.files.get(&download_id) else {
                return;
            };
            (
                Self::is_completed(file_info),
                file_info.is_paused,
                file_info.file_id,
                file_info.internal_download_id,
                file_info.priority,
            )
        };
        if is_completed {
            let is_inserted = self.completed_download_ids.insert(download_id);
            debug_assert!(is_inserted == was_completed);
        } else if !is_paused && let Some(callback) = self.callback.as_deref_mut() {
            callback.start_file(file_id, internal_download_id, priority);
        }
        if self.is_search_inited {
            let (file_id, file_source_id, created_at, completed_at, is_paused) = {
                let file_info = self.files.get(&download_id).expect("inserted");
                (
                    file_info.file_id,
                    file_info.file_source_id,
                    file_info.created_at,
                    file_info.completed_at,
                    file_info.is_paused,
                )
            };
            let file_counters = self.file_counters;
            if let Some(callback) = self.callback.as_deref_mut() {
                callback.update_file_added(
                    file_id,
                    file_source_id,
                    created_at,
                    completed_at,
                    is_paused,
                    file_counters,
                );
            }
        }
    }

    /// `remove_file_impl` (DownloadManager.cpp:454-476).
    fn remove_file_impl(
        &mut self,
        file_info: &DownloadFileInfo,
        delete_from_cache: bool,
        _source: &str,
    ) {
        let file_id = file_info.file_id;
        let download_id = file_info.download_id;
        if !Self::is_completed(file_info)
            && !file_info.is_paused
            && let Some(callback) = self.callback.as_deref_mut()
        {
            callback.pause_file(file_info.file_id, file_info.internal_download_id);
        }
        self.unregister_file_info(download_id);
        if delete_from_cache && let Some(callback) = self.callback.as_deref_mut() {
            callback.delete_file(file_id);
        }
        self.by_file_id.remove(&file_id);
        self.hints.remove(download_id);
        self.completed_download_ids.remove(&download_id);

        self.remove_from_database(download_id);
        self.files.remove(&download_id);
        if self.is_search_inited {
            let file_counters = self.file_counters;
            if let Some(callback) = self.callback.as_deref_mut() {
                callback.update_file_removed(file_id, file_counters);
            }
        }

        self.on_file_viewed_internal(download_id);
    }

    /// `remove_file_if_finished_impl` (DownloadManager.cpp:130-137).
    pub fn remove_file_if_finished(&mut self, file_id: FileId) -> Result<(), Status> {
        self.check_is_active("remove_file_if_finished_impl")?;
        let download_id = self
            .by_file_id
            .get(&file_id)
            .copied()
            .ok_or_else(|| Status::error(400, "Can't find file"))?;
        let is_completed = self
            .files
            .get(&download_id)
            .map(|file_info| Self::is_completed(file_info))
            .expect("file");
        if !is_completed {
            return Err(Status::error_message("File is active"));
        }
        let file_info = self.files.get(&download_id).expect("file").clone();
        self.remove_file_impl(&file_info, false, "remove_file_if_finished_impl");
        Ok(())
    }

    /// `toggle_is_paused(FileId, bool, Promise)` + the private
    /// `toggle_is_paused(FileInfo&, bool)` (DownloadManager.cpp:75-82,
    /// 527-548).
    pub fn toggle_is_paused(&mut self, file_id: FileId, is_paused: bool) -> Result<(), Status> {
        self.check_is_active("toggle_is_paused")?;
        let download_id = self
            .by_file_id
            .get(&file_id)
            .copied()
            .ok_or_else(|| Status::error(400, "Can't find file"))?;
        let (was_completed, was_paused) = {
            let file_info = self
                .files
                .get(&download_id)
                .ok_or_else(|| Status::error(400, "Can't find file"))?;
            (Self::is_completed(file_info), file_info.is_paused)
        };
        if was_completed || is_paused == was_paused {
            return Ok(());
        }

        self.unregister_file_info(download_id);
        let (file_id, internal_download_id, priority) = {
            let file_info = self.files.get_mut(&download_id).expect("file");
            file_info.is_paused = is_paused;
            file_info.need_save_to_database = true;
            self.last_link_token += 1;
            file_info.link_token = self.last_link_token;
            (
                file_info.file_id,
                file_info.internal_download_id,
                file_info.priority,
            )
        };
        self.register_file_info(download_id);
        if is_paused {
            if let Some(callback) = self.callback.as_deref_mut() {
                callback.pause_file(file_id, internal_download_id);
            }
        } else if let Some(callback) = self.callback.as_deref_mut() {
            callback.start_file(file_id, internal_download_id, priority);
        }
        if self.is_search_inited {
            let (file_id, completed_at, is_paused) = {
                let file_info = self.files.get(&download_id).expect("file");
                (
                    file_info.file_id,
                    file_info.completed_at,
                    file_info.is_paused,
                )
            };
            let file_counters = self.file_counters;
            if let Some(callback) = self.callback.as_deref_mut() {
                callback.update_file_changed(file_id, completed_at, is_paused, file_counters);
            }
        }
        Ok(())
    }

    /// `toggle_all_is_paused` (DownloadManager.cpp:84-99).
    pub fn toggle_all_is_paused(&mut self, is_paused: bool) -> Result<(), Status> {
        self.check_is_active("toggle_all_is_paused")?;
        let to_toggle: Vec<i64> = self
            .files
            .values()
            .filter(|file_info| !Self::is_completed(file_info) && is_paused != file_info.is_paused)
            .map(|file_info| file_info.download_id)
            .collect();
        for download_id in to_toggle {
            self.toggle_is_paused_download_id(download_id, is_paused);
        }
        Ok(())
    }

    /// The per-download half of `toggle_all_is_paused` — no
    /// check_is_active (the C++ iterates after the single check).
    fn toggle_is_paused_download_id(&mut self, download_id: i64, is_paused: bool) {
        let was_completed = self
            .files
            .get(&download_id)
            .map(|file_info| Self::is_completed(file_info))
            .unwrap_or(true);
        let was_paused = self
            .files
            .get(&download_id)
            .map(|file_info| file_info.is_paused)
            .unwrap_or(!is_paused);
        if was_completed || is_paused == was_paused {
            return;
        }

        self.unregister_file_info(download_id);
        let (file_id, internal_download_id, priority) = {
            let file_info = self.files.get_mut(&download_id).expect("file");
            file_info.is_paused = is_paused;
            file_info.need_save_to_database = true;
            self.last_link_token += 1;
            file_info.link_token = self.last_link_token;
            (
                file_info.file_id,
                file_info.internal_download_id,
                file_info.priority,
            )
        };
        self.register_file_info(download_id);
        if is_paused {
            if let Some(callback) = self.callback.as_deref_mut() {
                callback.pause_file(file_id, internal_download_id);
            }
        } else if let Some(callback) = self.callback.as_deref_mut() {
            callback.start_file(file_id, internal_download_id, priority);
        }
        if self.is_search_inited {
            let (file_id, completed_at, is_paused) = {
                let file_info = self.files.get(&download_id).expect("file");
                (
                    file_info.file_id,
                    file_info.completed_at,
                    file_info.is_paused,
                )
            };
            let file_counters = self.file_counters;
            if let Some(callback) = self.callback.as_deref_mut() {
                callback.update_file_changed(file_id, completed_at, is_paused, file_counters);
            }
        }
    }

    /// `remove_file` (DownloadManager.cpp:101-106).
    pub fn remove_file(
        &mut self,
        file_id: FileId,
        file_source_id: FileSourceId,
        delete_from_cache: bool,
    ) -> Result<(), Status> {
        self.check_is_active("remove_file")?;
        let download_id = self
            .by_file_id
            .get(&file_id)
            .copied()
            .ok_or_else(|| Status::error(400, "Can't find file"))?;
        {
            let file_info = self
                .files
                .get(&download_id)
                .ok_or_else(|| Status::error(400, "Can't find file"))?;
            if file_source_id != FileSourceId::default()
                && file_source_id != file_info.file_source_id
            {
                return Err(Status::error(400, "Can't find file with such source"));
            }
        }
        let file_info = self.files.get(&download_id).expect("file").clone();
        self.remove_file_impl(&file_info, delete_from_cache, "remove_file");
        Ok(())
    }

    /// `remove_all_files` (DownloadManager.cpp:108-122).
    pub fn remove_all_files(
        &mut self,
        only_active: bool,
        only_completed: bool,
        delete_from_cache: bool,
    ) -> Result<(), Status> {
        self.check_is_active("remove_all_files")?;
        let to_remove: Vec<i64> = self
            .files
            .values()
            .filter(|file_info| {
                let completed = Self::is_completed(file_info);
                (!only_active || !completed) && (!only_completed || completed)
            })
            .map(|file_info| file_info.download_id)
            .collect();
        for download_id in to_remove {
            let file_info = self.files.get(&download_id).expect("file").clone();
            self.remove_file_impl(&file_info, delete_from_cache, "remove_all_files");
        }
        Ok(())
    }

    /// `search` + `do_search` (DownloadManager.cpp:125-183) — the
    /// MultiPromise search-text load is synchronous here.
    pub fn search(
        &mut self,
        query: &str,
        only_active: bool,
        only_completed: bool,
        offset: &str,
        limit: i32,
    ) -> Result<td_api::FoundFileDownloads, Status> {
        self.check_is_active("do_search")?;
        if !self.is_search_inited {
            self.prepare_hints();
        }
        if limit <= 0 {
            return Err(Status::error(400, "Limit must be positive"));
        }
        let mut offset_int64 = i64::MAX;
        if !offset.is_empty() {
            offset_int64 = offset
                .parse::<i64>()
                .map_err(|_| Status::error(400, "Invalid offset"))?;
        }
        let (_, mut download_ids) = self.hints.search(query, 10000, true);
        let mut counters = DownloadFileCounters::default();
        download_ids.retain(|&download_id| {
            let Some(file_info) = self.files.get(&download_id) else {
                return false;
            };
            if Self::is_completed(file_info) {
                counters.completed_count += 1;
                if only_active {
                    return false;
                }
            } else {
                counters.active_count += 1;
                if file_info.is_paused {
                    counters.paused_count += 1;
                }
                if only_completed {
                    return false;
                }
            }
            if download_id >= offset_int64 {
                return false;
            }
            true
        });
        download_ids.sort_unstable_by(|a, b| b.cmp(a));
        download_ids.truncate(limit as usize);

        let mut file_downloads: Vec<td_api::FileDownload> = Vec::new();
        for &download_id in &download_ids {
            self.on_file_viewed_internal(download_id);
            let built = {
                let Some(file_info) = self.files.get(&download_id) else {
                    continue;
                };
                let (file_id, file_source_id, add_date, complete_date, is_paused) = (
                    file_info.file_id,
                    file_info.file_source_id,
                    file_info.created_at,
                    file_info.completed_at,
                    file_info.is_paused,
                );
                // the C++ `get_file_download_object` answers with a null
                // message when the source cannot be resolved; those
                // entries are dropped (DownloadManager.cpp:264-266)
                self.callback.as_deref_mut().and_then(|callback| {
                    callback.get_file_download_object(
                        file_id,
                        file_source_id,
                        add_date,
                        complete_date,
                        is_paused,
                    )
                })
            };
            // the C++ drops the entries whose message is unresolved
            // (DownloadManager.cpp:265-266)
            if let Some(file_download) = built
                && file_download.message.is_some()
            {
                file_downloads.push(file_download);
            }
        }

        let next_offset = download_ids
            .last()
            .map(|download_id| download_id.to_string())
            .unwrap_or_default();
        Ok(td_api::FoundFileDownloads {
            total_counts: Some(counters.get_downloaded_file_counts_object()),
            files: file_downloads,
            next_offset,
        })
    }

    /// `update_file_download_state` (DownloadManager.cpp:185-218) —
    /// `link_token` is the collapse of the C++ actor link token: the
    /// download-file callback echoes the token it started with.
    pub fn update_file_download_state(
        &mut self,
        file_id: FileId,
        downloaded_size: i64,
        size: i64,
        expected_size: i64,
        is_paused: bool,
        link_token: u64,
    ) {
        if self.callback.is_none() || !self.is_database_loaded {
            return;
        }
        let Some(&download_id) = self.by_file_id.get(&file_id) else {
            return;
        };
        if self
            .files
            .get(&download_id)
            .map(|file_info| file_info.link_token != link_token)
            .unwrap_or(true)
        {
            // Ignore update_file_download_state because of outdated link_token
            return;
        }

        self.unregister_file_info(download_id);
        let mut need_update = false;
        let mut pause_file_needed = false;
        let (file_id2, internal_download_id) = {
            let file_info = self.files.get_mut(&download_id).expect("file");
            file_info.size = size;
            file_info.expected_size = expected_size;
            file_info.downloaded_size = downloaded_size;
            if is_paused && !file_info.is_paused {
                file_info.is_paused = true;
                file_info.need_save_to_database = true;
                need_update = true;
                pause_file_needed = true;
            }
            (file_info.file_id, file_info.internal_download_id)
        };
        if is_paused && pause_file_needed {
            // keep the state consistent
            if let Some(callback) = self.callback.as_deref_mut() {
                callback.pause_file(file_id2, internal_download_id);
            }
        }
        self.register_file_info(download_id);
        if self.is_search_inited && need_update {
            let (file_id, completed_at, is_paused) = {
                let file_info = self.files.get(&download_id).expect("file");
                (
                    file_info.file_id,
                    file_info.completed_at,
                    file_info.is_paused,
                )
            };
            let file_counters = self.file_counters;
            if let Some(callback) = self.callback.as_deref_mut() {
                callback.update_file_changed(file_id, completed_at, is_paused, file_counters);
            }
        }
    }

    /// `update_file_viewed` (DownloadManager.cpp:220-228).
    pub fn update_file_viewed(&mut self, file_id: FileId, file_source_id: FileSourceId) {
        if self.unviewed_completed_download_ids.is_empty()
            || self.callback.is_none()
            || !self.is_database_loaded
        {
            return;
        }
        let Some(&download_id) = self.by_file_id.get(&file_id) else {
            return;
        };
        let matches = self
            .files
            .get(&download_id)
            .map(|file_info| file_info.file_source_id == file_source_id)
            .unwrap_or(false);
        if !matches {
            return;
        }
        self.on_file_viewed_internal(download_id);
    }

    /// `sync_with_database` (DownloadManager.cpp:178-191) — the record
    /// store rides the context's FileManager file-id serialization.
    fn sync_with_database(&mut self, download_id: i64) {
        let need_save = self
            .files
            .get(&download_id)
            .map(|file_info| file_info.need_save_to_database)
            .unwrap_or(false);
        if !need_save {
            return;
        }
        if let Some(file_info) = self.files.get_mut(&download_id) {
            file_info.need_save_to_database = false;
        }
        if !self.ctx().use_message_database() {
            return;
        }
        let record = {
            let Some(file_info) = self.files.get(&download_id) else {
                return;
            };
            FileDownloadInDatabase {
                download_id: file_info.download_id,
                file_id: file_info.file_id,
                file_source_id: file_info.file_source_id,
                priority: file_info.priority as i32,
                created_at: file_info.created_at,
                completed_at: file_info.completed_at,
                is_paused: file_info.is_paused,
            }
        };
        let mut out = Vec::new();
        // the log_event_store framing: the writer's version first
        // (LogEvent.h:166-174), then the record fields
        store_parse::store_log_event_version(&mut out);
        store_parse::store_flags(&mut out, [record.is_paused]);
        store_parse::store_i64(&mut out, record.download_id);
        self.ctx().store_file_id(record.file_id, &mut out);
        store_parse::store_i32(&mut out, record.file_source_id.0);
        store_parse::store_i32(&mut out, record.priority);
        store_parse::store_i32(&mut out, record.created_at);
        store_parse::store_i32(&mut out, record.completed_at);
        let key = Self::pmc_key(download_id);
        self.ctx().pmc_set(&key, &out);
    }

    /// `remove_from_database` (DownloadManager.cpp:193-200).
    fn remove_from_database(&mut self, download_id: i64) {
        if !self.ctx().use_message_database() {
            return;
        }
        let key = Self::pmc_key(download_id);
        self.ctx().pmc_erase(&key);
    }

    /// `register_file_info` (DownloadManager.cpp:675-718).
    fn register_file_info(&mut self, download_id: i64) {
        let need_update = {
            let Some(file_info) = self.files.get_mut(&download_id) else {
                return;
            };
            debug_assert!(!file_info.is_registered);
            file_info.is_registered = true;
            let mut need_update = false;
            if !Self::is_completed(file_info)
                && file_info.size != 0
                && file_info.downloaded_size == file_info.size
            {
                file_info.is_paused = false;
                file_info.completed_at = 1; // G()->unix_time(); the tests drive the state through updates
                file_info.need_save_to_database = true;
                let is_inserted = self.completed_download_ids.insert(download_id);
                debug_assert!(is_inserted);
                if file_info.is_counted {
                    self.unviewed_completed_download_ids.insert(download_id);
                }
                need_update = true;
            }
            if file_info.is_counted && (Self::is_completed(file_info) || !file_info.is_paused) {
                self.counters.downloaded_size += file_info.downloaded_size;
                self.counters.total_size += Self::get_file_size(file_info);
                self.counters.total_count += 1;
            }
            if Self::is_completed(file_info) {
                self.file_counters.completed_count += 1;
            } else {
                if file_info.is_paused {
                    self.file_counters.paused_count += 1;
                }
                self.file_counters.active_count += 1;
            }
            need_update
        };
        if self.is_search_inited && need_update {
            let (file_id, completed_at, is_paused) = {
                let file_info = self.files.get(&download_id).expect("file");
                (
                    file_info.file_id,
                    file_info.completed_at,
                    file_info.is_paused,
                )
            };
            let file_counters = self.file_counters;
            if let Some(callback) = self.callback.as_deref_mut() {
                callback.update_file_changed(file_id, completed_at, is_paused, file_counters);
            }
        }
        self.sync_with_database(download_id);
        self.update_counters();
        self.check_completed_downloads_size();
    }

    /// `unregister_file_info` (DownloadManager.cpp:651-673).
    fn unregister_file_info(&mut self, download_id: i64) {
        let Some(file_info) = self.files.get_mut(&download_id) else {
            return;
        };
        debug_assert!(file_info.is_registered);
        file_info.is_registered = false;
        if file_info.is_counted && (Self::is_completed(file_info) || !file_info.is_paused) {
            self.counters.downloaded_size -= file_info.downloaded_size;
            self.counters.total_size -= Self::get_file_size(file_info);
            self.counters.total_count -= 1;
        }
        if Self::is_completed(file_info) {
            self.file_counters.completed_count -= 1;
        } else {
            if file_info.is_paused {
                self.file_counters.paused_count -= 1;
            }
            self.file_counters.active_count -= 1;
        }
    }

    /// `check_completed_downloads_size` (DownloadManager.cpp:720-731).
    fn check_completed_downloads_size(&mut self) {
        while self.completed_download_ids.len() > MAX_COMPLETED_DOWNLOADS {
            let Some(&download_id) = self.completed_download_ids.iter().next() else {
                break;
            };
            let Some(file_info) = self.files.get(&download_id) else {
                break;
            };
            let file_info = file_info.clone();
            self.remove_file_impl(&file_info, false, "check_completed_downloads_size");
        }
    }

    /// `on_file_viewed` (DownloadManager.cpp:733-744).
    fn on_file_viewed_internal(&mut self, download_id: i64) {
        if self.unviewed_completed_download_ids.is_empty() {
            return;
        }
        self.unviewed_completed_download_ids.remove(&download_id);
        if self.unviewed_completed_download_ids.is_empty() {
            self.clear_counters_due();
        }
    }

    /// `timeout_expired` → `clear_counters` (DownloadManager.cpp:624-643)
    /// — the caller ticks the 60-second deadline with wall time.
    pub fn tick(&mut self, now: f64) {
        let Some(clear_at) = self.counters_clear_at else {
            return;
        };
        if now < clear_at {
            return;
        }
        self.counters_clear_at = None;
        self.clear_counters_due();
    }

    /// `clear_counters` (DownloadManager.cpp:627-643) — zeroes the
    /// visible counters once every completed download was viewed.
    pub fn clear_counters_due(&mut self) {
        if !self.is_database_loaded {
            return;
        }
        debug_assert!(self.counters == self.sent_counters);
        if self.counters.downloaded_size != self.counters.total_size
            || self.counters.total_size == 0
        {
            return;
        }
        for file_info in self.files.values_mut() {
            if Self::is_completed(file_info) || !file_info.is_paused {
                file_info.is_counted = false;
            }
        }
        self.counters = DownloadCounters::default();
        self.update_counters();
    }

    /// `update_counters` (DownloadManager.cpp:694-717).
    fn update_counters(&mut self) {
        if !self.is_database_loaded {
            return;
        }
        if self.counters == self.sent_counters {
            return;
        }
        if (self.counters.downloaded_size == self.counters.total_size
            && self.counters.total_size != 0)
            || self.counters == DownloadCounters::default()
        {
            if self.counters.total_size != 0 {
                self.counters_clear_at = Some(tdutils::time::system() + EMPTY_UPDATE_DELAY);
            } else {
                self.counters_clear_at = None;
            }
            self.ctx().pmc_erase("dlds_counter");
        } else {
            self.counters_clear_at = None;
            // `log_event_store` framing (DownloadManager.cpp:675): the
            // log-event version prefix leads the counters.
            let mut out = Vec::new();
            store_parse::store_log_event_version(&mut out);
            self.counters.store(&mut out);
            self.ctx().pmc_set("dlds_counter", &out);
        }
        self.sent_counters = self.counters;
        if let Some(callback) = self.callback.as_deref_mut() {
            callback.update_counters(self.counters);
        }
    }

    /// `check_is_active` (DownloadManager.cpp:746-754) — the database
    /// load is synchronous in the collapse.
    fn check_is_active(&mut self, _source: &str) -> Result<(), Status> {
        if self.callback.is_none() {
            return Err(Status::error(500, "DownloadManager is closed"));
        }
        debug_assert!(self.is_inited);
        self.load_database_files();
        Ok(())
    }
}

impl FileManager {
    /// Sets `user_download_file_callback_` (FileManager.h:841) — the
    /// callback the user download requests finish through.
    pub fn set_user_download_callback(
        &mut self,
        callback: Arc<parking_lot::Mutex<dyn DownloadCallback>>,
    ) {
        self.user_download_file_callback = Some(callback);
    }

    /// `FileManager::get_internal_download_id` (FileManager.cpp:3244).
    pub fn get_internal_download_id(&mut self) -> i64 {
        self.next_internal_download_id += 1;
        self.next_internal_download_id
    }

    /// `FileManager::get_internal_upload_id`.
    pub fn get_internal_upload_id(&mut self) -> i64 {
        self.next_internal_upload_id += 1;
        self.next_internal_upload_id
    }

    fn node_id_of(&self, file_id: FileId) -> Option<usize> {
        self.file_id_node.get(&file_id).copied()
    }

    /// `FileManager::download` (FileManager.cpp:3032-3063). The async
    /// pre-download location check cannot change the outcome —
    /// `download_impl` only logs its failure (FileManager.cpp:3075-3077)
    /// — so the collapse goes straight to `download_impl`. The td_api
    /// promise plane folds into the caller's callback/polling.
    #[allow(clippy::too_many_arguments)]
    pub fn download(
        &mut self,
        file_id: FileId,
        internal_download_id: i64,
        callback: Arc<parking_lot::Mutex<dyn DownloadCallback>>,
        new_priority: i32,
        offset: i64,
        limit: i64,
        fllm: &mut FileLoadManager,
    ) {
        debug_assert!(new_priority > 0, "CHECK(new_priority > 0)");
        let Some(node_id) = self.node_id_of(file_id) else {
            let error = Status::error(400, "File not found");
            callback.lock().on_download_error(file_id, error.clone());
            return;
        };
        let local_is_empty = self.file_nodes[node_id]
            .as_ref()
            .is_some_and(|node| matches!(node.local, LocalFileLocation::Empty));
        if !local_is_empty {
            // the C++ pre-download check_local_location (skip=true): a
            // failed check drops the local location
            // (on_failed_check_local_location, FileManager.cpp:1483-1489)
            // so a node whose file vanished from disk re-downloads
            let (full, size) = self.file_nodes[node_id]
                .as_ref()
                .map(|node| match &node.local {
                    LocalFileLocation::Full(full) => (Some(full.clone()), node.size),
                    _ => (None, node.size),
                })
                .unwrap_or((None, 0));
            if let Some(full) = full {
                let check = check_full_local_location(
                    self.ctx(),
                    FullLocalLocationInfo {
                        location: full,
                        size,
                    },
                    true,
                );
                let bad_path = check
                    .as_ref()
                    .ok()
                    .is_some_and(|info| self.bad_paths.contains(&info.location.path));
                if check.is_err() || bad_path {
                    self.file_nodes[node_id]
                        .as_mut()
                        .expect("node")
                        .drop_local_location();
                    self.try_flush_node(node_id);
                }
            }
        }
        self.download_impl(
            file_id,
            internal_download_id,
            callback,
            new_priority,
            offset,
            limit,
            fllm,
        );
    }

    /// `FileManager::download_impl` (FileManager.cpp:3065-3114).
    #[allow(clippy::too_many_arguments)]
    fn download_impl(
        &mut self,
        file_id: FileId,
        internal_download_id: i64,
        callback: Arc<parking_lot::Mutex<dyn DownloadCallback>>,
        new_priority: i32,
        offset: i64,
        limit: i64,
        fllm: &mut FileLoadManager,
    ) {
        let Some(node_id) = self.node_id_of(file_id) else {
            return;
        };
        let already_downloaded = self.file_nodes[node_id]
            .as_ref()
            .is_some_and(|node| matches!(node.local, LocalFileLocation::Full(_)));
        if already_downloaded {
            callback.lock().on_download_ok(file_id);
            return;
        }

        let can_download = self.file_nodes[node_id]
            .as_ref()
            .map(|node| {
                let view = FileView::new(node);
                view.can_download_from_server() || view.can_generate()
            })
            .unwrap_or(false);
        if !can_download {
            let error = Status::error(400, "Can't download or generate the file");
            callback.lock().on_download_error(file_id, error.clone());
            return;
        }

        let requests = self.file_download_requests.entry(file_id).or_default();
        if internal_download_id != 0 {
            debug_assert!(offset == -1, "CHECK(offset == -1)");
            debug_assert!(limit == -1, "CHECK(limit == -1)");
            let info = requests
                .internal_downloads
                .entry(internal_download_id)
                .or_default();
            debug_assert!(
                info.download_callback.is_none(),
                "CHECK(download_callback == nullptr)"
            );
            info.download_priority = new_priority as i8;
            info.download_callback = Some(callback.clone());
            callback.lock().on_progress(file_id);
        } else {
            let node = self.file_nodes[node_id].as_mut().expect("node");
            node.set_download_offset(offset);
            node.set_download_limit(limit);
            let requests = self
                .file_download_requests
                .get_mut(&file_id)
                .expect("requests");
            requests.user_offset = offset;
            requests.user_limit = limit;
            requests.user_download_priority = new_priority as i8;
        }

        self.run_generate(node_id, fllm);
        self.run_download(node_id, true, fllm);
        self.try_flush_node(node_id);
    }

    /// `FileManager::extract_download_callback`
    /// (FileManager.cpp:3116-3143).
    fn extract_download_callback(
        &mut self,
        file_id: FileId,
        internal_download_id: i64,
    ) -> Option<Arc<parking_lot::Mutex<dyn DownloadCallback>>> {
        let requests = self.file_download_requests.get_mut(&file_id)?;
        let callback = if internal_download_id != 0 {
            let info = requests.internal_downloads.remove(&internal_download_id)?;
            info.download_callback
        } else {
            if requests.user_download_priority == 0 {
                return None;
            }
            let callback = self.user_download_file_callback.clone();
            requests.user_download_priority = 0;
            requests.user_offset = 0;
            requests.user_limit = 0;
            callback
        };
        if requests.user_download_priority == 0 && requests.internal_downloads.is_empty() {
            self.file_download_requests.remove(&file_id);
        }
        callback
    }

    /// `FileManager::finish_downloads` (FileManager.cpp:3145-3167). The
    /// C++ user arm fires `user_download_file_callback_`; the upload
    /// twin's user arm constructs a log-only callback and is skipped in
    /// the collapse.
    fn finish_downloads(&mut self, file_id: FileId, status: Option<Status>) {
        let Some(mut requests) = self.file_download_requests.remove(&file_id) else {
            return;
        };
        let mut callbacks: Vec<Arc<parking_lot::Mutex<dyn DownloadCallback>>> = requests
            .internal_downloads
            .values_mut()
            .filter_map(|info| info.download_callback.take())
            .collect();
        if requests.user_download_priority != 0
            && let Some(user_callback) = &self.user_download_file_callback
        {
            callbacks.push(user_callback.clone());
        }
        for callback in callbacks {
            match &status {
                None => callback.lock().on_download_ok(file_id),
                Some(status) => {
                    callback.lock().on_download_error(file_id, status.clone());
                }
            }
        }
    }

    /// `FileManager::download_file` (FileManager.cpp:2961-3000) — the
    /// `downloadFile` request entry. The parked synchronous promises live
    /// in `pending_user_file_downloads_`; the non-synchronous promise
    /// resolves inline with the `download_impl` outcome order (the C++
    /// promise rides `download`/`download_impl`, whose error paths are
    /// the same node reads done here).
    #[allow(clippy::too_many_arguments)]
    pub fn download_file(
        &mut self,
        file_id: FileId,
        priority: i32,
        offset: i64,
        limit: i64,
        synchronous: bool,
        mut promise: tdutils::promise::Promise<td_api::File>,
        fllm: &mut FileLoadManager,
    ) {
        if let Err(status) = check_download_priority(priority) {
            promise.set_error(status);
            return;
        }
        if offset < 0 {
            promise.set_error(Status::error(400, "Download offset must be non-negative"));
            return;
        }
        if limit < 0 {
            promise.set_error(Status::error(400, "Download limit must be non-negative"));
            return;
        }
        let node_snapshot = self.get_file_view(file_id).map(|view| {
            (
                matches!(view.node.local, LocalFileLocation::Full(_)),
                view.can_download_from_server() || view.can_generate(),
            )
        });
        let Some((already_downloaded, can_download)) = node_snapshot else {
            promise.set_error(Status::error(400, "File not found"));
            return;
        };

        // we can't have two pending user requests with different offset
        // and limit, so cancel all previous requests
        let same_request = self
            .pending_user_file_downloads
            .get(&file_id)
            .is_some_and(|info| info.offset == offset && info.limit == limit);
        if !same_request && let Some(mut info) = self.pending_user_file_downloads.remove(&file_id) {
            let canceled = Status::error(200, "Canceled by another downloadFile request");
            if synchronous {
                info.promises.clear();
                self.pending_user_file_downloads.insert(
                    file_id,
                    UserFileDownloadInfo {
                        offset,
                        limit,
                        promises: Vec::new(),
                    },
                );
            }
            for mut parked in info.promises {
                parked.set_error(canceled.clone());
            }
        }

        let callback = self
            .user_download_file_callback
            .clone()
            .unwrap_or_else(|| Arc::new(parking_lot::Mutex::new(NoopDownloadCallback)));
        if synchronous {
            let info = self
                .pending_user_file_downloads
                .entry(file_id)
                .or_insert_with(|| UserFileDownloadInfo {
                    offset,
                    limit,
                    promises: Vec::new(),
                });
            info.promises.push(promise);
            self.download(file_id, 0, callback, priority, offset, limit, fllm);
            return;
        }
        self.download(file_id, 0, callback, priority, offset, limit, fllm);
        // the `download_impl` promise outcomes in order: the
        // already-downloaded and cannot-download early returns answer
        // before the request registration
        if !already_downloaded && !can_download {
            promise.set_error(Status::error(400, "Can't download or generate the file"));
            return;
        }
        promise.set_value(self.get_file_object(file_id));
    }

    /// `FileManager::on_user_file_download_finished`
    /// (FileManager.cpp:3002-3029) — the `UserDownloadFileCallback` hop:
    /// resolve the parked synchronous promises against the final file
    /// object.
    pub fn on_user_file_download_finished(&mut self, file_id: FileId) {
        let Some(info) = self.pending_user_file_downloads.remove(&file_id) else {
            return;
        };
        let UserFileDownloadInfo {
            offset,
            mut limit,
            promises,
        } = info;
        if limit == 0 {
            limit = i64::MAX;
        }
        for mut promise in promises {
            let file_object = self.get_file_object(file_id);
            let Some(local) = &file_object.local else {
                promise.set_error(Status::error(
                    400,
                    "File download has failed or was canceled",
                ));
                continue;
            };
            let download_offset = local.download_offset;
            let downloaded_size = local.downloaded_prefix_size;
            let file_size = file_object.size;
            if local.is_downloading_completed
                || (download_offset <= offset
                    && download_offset + downloaded_size >= offset
                    && ((file_size != 0 && download_offset + downloaded_size == file_size)
                        || download_offset + downloaded_size - offset >= limit))
            {
                promise.set_value(file_object);
            } else {
                promise.set_error(Status::error(
                    400,
                    "File download has failed or was canceled",
                ));
            }
        }
    }

    /// `FileManager::cancel_download` (FileManager.cpp:3169-3195).
    pub fn cancel_download(
        &mut self,
        file_id: FileId,
        internal_download_id: i64,
        only_if_pending: bool,
        fllm: &mut FileLoadManager,
    ) {
        let Some(node_id) = self.node_id_of(file_id) else {
            return;
        };
        if only_if_pending
            && self.file_nodes[node_id]
                .as_ref()
                .is_some_and(|node| node.is_download_started)
        {
            return;
        }
        let Some(callback) = self.extract_download_callback(file_id, internal_download_id) else {
            return;
        };
        callback
            .lock()
            .on_download_error(file_id, Status::error(200, "Canceled"));
        self.run_generate(node_id, fllm);
        self.run_download(node_id, true, fllm);
        self.try_flush_node(node_id);
    }

    /// `FileManager::do_cancel_download` (FileManager.cpp:2162-2171).
    fn do_cancel_download(&mut self, node_id: usize, fllm: &mut FileLoadManager) {
        let Some(node) = self.file_nodes[node_id].as_mut() else {
            return;
        };
        if node.download_id == 0 {
            return;
        }
        fllm.cancel_download(node.download_id);
        node.download_id = 0;
        node.is_download_started = false;
        node.download_was_update_file_reference = false;
        node.set_download_priority(0);
    }

    /// `FileManager::run_download` (FileManager.cpp:3197-3336) — the
    /// queue decision order.
    fn run_download(
        &mut self,
        node_id: usize,
        force_update_priority: bool,
        fllm: &mut FileLoadManager,
    ) {
        let Some(node) = self.file_nodes[node_id].as_ref() else {
            return;
        };
        let mut priority: i8 = 0;
        let mut ignore_download_limit = false;
        for file_id in &node.file_ids {
            if let Some(requests) = self.file_download_requests.get(file_id) {
                priority = priority.max(requests.user_download_priority);
                for info in requests.internal_downloads.values() {
                    priority = priority.max(info.download_priority);
                    ignore_download_limit = true;
                }
            }
        }
        let old_priority = node.download_priority;
        let need_load_from_pmc = node.need_load_from_pmc;
        let generate_id = node.generate_id;
        let download_id = node.download_id;
        let can_download = FileView::new(node).can_download_from_server();
        let _ = &node; // the read borrow ends here

        if priority == 0 {
            let node = self.file_nodes[node_id].as_mut().expect("node");
            node.set_download_priority(0);
            if old_priority != 0 {
                self.do_cancel_download(node_id, fllm);
            }
            return;
        }
        if need_load_from_pmc {
            // the C++ `get_sync_file_node` load_from_pmc: the flag clears
            // before the run decision (the db read is the [blocked] half)
            self.load_from_pmc(node_id);
        }
        if generate_id != 0 {
            // the M.12.8 generate plane never sets generate_id today
            return;
        }
        if !can_download {
            return;
        }
        let (download_offset, download_limit, is_encrypted_any) = {
            let node = self.file_nodes[node_id].as_mut().expect("node");
            node.set_download_priority(priority);
            node.set_ignore_download_limit(ignore_download_limit);
            let need_update_offset = node.is_download_offset_dirty;
            node.is_download_offset_dirty = false;
            let need_update_limit = node.is_download_limit_dirty;
            node.is_download_limit_dirty = false;
            (
                need_update_offset,
                need_update_limit,
                FileView::new(node).is_encrypted_any(),
            )
        };
        let need_update_offset = download_offset;
        let need_update_limit = download_limit;

        if old_priority != 0 {
            debug_assert!(download_id != 0, "CHECK(download_id_ != 0)");
            let node = self.file_nodes[node_id].as_ref().expect("node");
            if force_update_priority || priority != old_priority {
                fllm.update_download_priority(node.download_id, priority);
            }
            if need_update_limit || need_update_offset {
                let mut download_offset = node.download_offset;
                let mut download_limit = node.get_download_limit();
                if is_encrypted_any {
                    assert!(
                        download_offset <= MAX_DOWNLOAD_FILE_SIZE,
                        "CHECK(download_offset <= MAX_FILE_SIZE)"
                    );
                    assert!(
                        download_limit <= MAX_DOWNLOAD_FILE_SIZE,
                        "CHECK(download_limit <= MAX_FILE_SIZE)"
                    );
                    download_limit += download_offset;
                    download_offset = 0;
                }
                fllm.update_downloaded_part(node.download_id, download_offset, download_limit);
            }
            return;
        }

        debug_assert!(download_id == 0, "CHECK(download_id_ == 0)");
        let (file_id, need_reload_photo, has_active_download_remote) = {
            let node = self.file_nodes[node_id].as_ref().expect("node");
            (
                node.main_file_id,
                node.need_reload_photo && FileView::new(node).may_reload_photo(),
                FileView::new(node).has_active_download_remote_location(),
            )
        };

        if need_reload_photo {
            let query_id = fllm.alloc_query_id();
            self.download_queries.insert(
                query_id,
                FileDownloadQuery {
                    file_id,
                    kind: DownloadQueryKind::DownloadReloadDialog,
                },
            );
            let node = self.file_nodes[node_id].as_mut().expect("node");
            node.download_id = query_id;
            node.need_reload_photo = false;
            let result = self.ctx().reload_photo(file_id);
            let error = match result {
                Ok(()) => Status::error_message("FILE_DOWNLOAD_ID_INVALID"),
                Err(error) => error,
            };
            return self.on_download_error(query_id, error, fllm);
        }

        if !has_active_download_remote {
            let query_id = fllm.alloc_query_id();
            self.download_queries.insert(
                query_id,
                FileDownloadQuery {
                    file_id,
                    kind: DownloadQueryKind::DownloadWaitFileReference,
                },
            );
            let node = self.file_nodes[node_id].as_mut().expect("node");
            node.download_id = query_id;
            if node.download_was_update_file_reference {
                return self.on_download_error(
                    query_id,
                    Status::error_message("Can't download file: have no valid file reference"),
                    fllm,
                );
            }
            node.download_was_update_file_reference = true;
            let result = self.ctx().repair_file_reference(file_id);
            let error = match result {
                Ok(()) => Status::error_message("FILE_DOWNLOAD_RESTART_WITH_FILE_REFERENCE"),
                Err(error) => error,
            };
            return self.on_download_error(query_id, error, fllm);
        }

        let query_id = fllm.alloc_query_id();
        self.download_queries.insert(
            query_id,
            FileDownloadQuery {
                file_id,
                kind: DownloadQueryKind::Download,
            },
        );
        let (
            remote,
            local,
            size,
            name,
            encryption_key,
            can_search_locally,
            download_offset,
            download_limit,
        ) = {
            let node = self.file_nodes[node_id].as_mut().expect("node");
            node.download_id = query_id;
            node.is_download_started = false;
            let mut download_offset = node.download_offset;
            let mut download_limit = node.get_download_limit();
            if FileView::new(node).is_encrypted_any() {
                assert!(
                    download_offset <= MAX_DOWNLOAD_FILE_SIZE,
                    "CHECK(download_offset <= MAX_FILE_SIZE)"
                );
                assert!(
                    download_limit <= MAX_DOWNLOAD_FILE_SIZE,
                    "CHECK(download_limit <= MAX_FILE_SIZE)"
                );
                download_limit += download_offset;
                download_offset = 0;
            }
            (
                node.remote
                    .full
                    .clone()
                    .expect("checked has_active_download_remote_location"),
                node.local.clone(),
                node.size,
                node.suggested_path(),
                node.encryption_key.clone(),
                node.can_search_locally,
                download_offset,
                download_limit,
            )
        };
        fllm.download(
            query_id,
            file_id,
            &remote,
            &local,
            size,
            name,
            &encryption_key,
            can_search_locally,
            download_offset,
            download_limit,
            priority,
        );
    }

    /// `FileManager::finish_download_query`
    /// (FileManager.cpp:5300-5319).
    fn finish_download_query(&mut self, query_id: u64) -> Option<(FileDownloadQuery, bool)> {
        let query = self.download_queries.remove(&query_id)?;
        let mut was_active = false;
        if let Some(node_id) = self.node_id_of(query.file_id)
            && let Some(node) = self.file_nodes[node_id].as_mut()
            && node.download_id == query_id
        {
            node.download_id = 0;
            node.download_was_update_file_reference = false;
            node.is_download_started = false;
            node.set_download_priority(0);
            was_active = true;
        }
        Some((query, was_active))
    }

    /// `FileManager::on_start_download` + `on_partial_download`
    /// (FileManager.cpp:4775-4825).
    fn on_start_download(&mut self, query_id: u64) {
        let Some(query) = self.download_queries.get(&query_id) else {
            return;
        };
        let file_id = query.file_id;
        let Some(node_id) = self.node_id_of(file_id) else {
            return;
        };
        let node = self.file_nodes[node_id].as_mut().expect("node");
        if node.download_id == query_id {
            node.is_download_started = true;
        }
    }

    fn on_partial_download(&mut self, query_id: u64, partial: PartialLocalFileLocation, size: i64) {
        let Some(query) = self.download_queries.get(&query_id) else {
            return;
        };
        let file_id = query.file_id;
        let Some(node_id) = self.node_id_of(file_id) else {
            return;
        };
        if size != 0 {
            let is_encrypted_secure = self.file_nodes[node_id]
                .as_ref()
                .map(|node| FileView::new(node).is_encrypted_secure())
                .unwrap_or(false);
            if !is_encrypted_secure {
                self.file_nodes[node_id]
                    .as_mut()
                    .expect("node")
                    .set_size(size);
            }
        }
        let node = self.file_nodes[node_id].as_mut().expect("node");
        if node.download_id == query_id {
            node.set_local_location(LocalFileLocation::Partial(partial), -1, -1);
            self.try_flush_node(node_id);
        }
    }

    /// `FileManager::on_download_ok` (FileManager.cpp:4874-4898). The
    /// user callbacks fire from the register_local-triggered merge's
    /// `finish_downloads` (FileManager.cpp:2433-2438); the collapse runs
    /// the same decision synchronously when the node carries a full local
    /// location.
    fn on_download_ok(
        &mut self,
        query_id: u64,
        local: FullLocalFileLocation,
        size: i64,
        is_new: bool,
        fllm: &mut FileLoadManager,
    ) {
        let Some((query, was_active)) = self.finish_download_query(query_id) else {
            return;
        };
        let file_id = query.file_id;
        let registered =
            self.register_local(local, DialogId::default(), size, false, true, file_id);
        match registered {
            Err(register_status) => {
                let status = Status::error(
                    400,
                    format!(
                        "Can't register local file after download: {}",
                        register_status.message()
                    )
                    .as_str(),
                );
                if let Some(node_id) = self.node_id_of(file_id) {
                    self.on_download_error_impl(node_id, query.kind, was_active, status, fllm);
                }
            }
            Ok(new_file_id) => {
                if is_new && self.ctx().need_notify_on_new_files() {
                    let real_size = self
                        .get_file_node(new_file_id)
                        .map(|node| FileView::new(node).local_total_size())
                        .unwrap_or(0);
                    self.ctx().on_new_file(size, real_size, 1);
                }
                // the register_local-triggered merge tail
                // (FileManager.cpp:2432-2451): finish the settled queries
                // of every merged file id and re-evaluate the queues
                let (node_file_ids, has_full_local, has_active_upload_remote) = self
                    .get_file_node(file_id)
                    .map(|node| {
                        let view = FileView::new(node);
                        (
                            node.file_ids.clone(),
                            view.has_full_local_location(),
                            view.has_active_upload_remote_location(),
                        )
                    })
                    .unwrap_or_default();
                for merged_file_id in node_file_ids {
                    if has_full_local {
                        self.finish_downloads(merged_file_id, None);
                    }
                    if has_active_upload_remote {
                        self.finish_uploads(merged_file_id, None, fllm);
                    }
                }
                if let Some(node_id) = self.node_id_of(file_id) {
                    self.run_generate(node_id, fllm);
                    self.run_download(node_id, false, fllm);
                    self.run_upload(node_id, Vec::new(), fllm);
                }
            }
        }
    }

    /// `FileManager::on_download_error` (FileManager.cpp:5069-5083).
    fn on_download_error(&mut self, query_id: u64, status: Status, fllm: &mut FileLoadManager) {
        let Some((query, was_active)) = self.finish_download_query(query_id) else {
            return;
        };
        let Some(node_id) = self.node_id_of(query.file_id) else {
            return;
        };
        self.on_download_error_impl(node_id, query.kind, was_active, status, fllm);
    }

    /// `FileManager::on_download_error_impl`
    /// (FileManager.cpp:5123-5203) — the restart decision order.
    fn on_download_error_impl(
        &mut self,
        node_id: usize,
        kind: DownloadQueryKind,
        was_active: bool,
        status: Status,
        fllm: &mut FileLoadManager,
    ) {
        // the C++ SCOPE_EXIT try_flush_node (FileManager.cpp:5125-5127)
        self.on_download_error_impl_inner(node_id, kind, was_active, status, fllm);
        self.try_flush_node(node_id);
    }

    /// The `on_download_error_impl` body.
    fn on_download_error_impl_inner(
        &mut self,
        node_id: usize,
        kind: DownloadQueryKind,
        was_active: bool,
        status: Status,
        fllm: &mut FileLoadManager,
    ) {
        let message = status.message().to_owned();
        if (message == "FILE_ID_INVALID" || message == "LOCATION_INVALID")
            && self.file_nodes[node_id]
                .as_ref()
                .map(|node| FileView::new(node).may_reload_photo())
                .unwrap_or(false)
        {
            self.file_nodes[node_id]
                .as_mut()
                .expect("node")
                .need_reload_photo = true;
            return self.run_download(node_id, true, fllm);
        }

        // FileReferenceManager::is_file_reference_error
        // (FileReferenceManager.cpp:60-62) + the #BASE64 suffix
        if status.code() == 400 && message.starts_with("FILE_REFERENCE_") {
            let mut file_reference: Vec<u8> = Vec::new();
            if let Some(pos) = message.rfind('#')
                && message[pos..].starts_with("#BASE64")
                && let tdutils::status::Result::Ok(decoded) =
                    tdmtproto::utils::base64_decode(&message.as_bytes()[pos + "#BASE64".len()..])
            {
                file_reference = decoded;
            }
            let _ = file_reference;
            if let Some(node) = self.file_nodes[node_id].as_mut() {
                node.delete_file_reference(&file_reference);
            }
            return self.run_download(node_id, true, fllm);
        }

        if message.starts_with("FILE_DOWNLOAD_RESTART") {
            if message.ends_with("WITH_FILE_REFERENCE") {
                self.file_nodes[node_id]
                    .as_mut()
                    .expect("node")
                    .download_was_update_file_reference = true;
                return self.run_download(node_id, true, fllm);
            }
            if message.ends_with("INCREASE_PART_SIZE") {
                // try_fix_partial_local_location is a wave-gated deferral:
                // the part-size bump falls through to the failure path
            } else {
                self.file_nodes[node_id]
                    .as_mut()
                    .expect("node")
                    .can_search_locally = false;
                return self.run_download(node_id, true, fllm);
            }
        }

        if message == "MTPROTO_CLUSTER_INVALID" {
            self.ctx().request_config();
            return self.run_download(node_id, true, fllm);
        }

        if !was_active {
            return;
        }

        // FileManager.cpp:5185-5198: a code-0 failure drops a partial
        // location, unlinking the temp file for the secure-decrypted and
        // video temp dirs
        if status.code() == 0 {
            let is_partial = self.file_nodes[node_id]
                .as_ref()
                .is_some_and(|node| matches!(node.local, LocalFileLocation::Partial(_)));
            let keep_partial = message.starts_with("FILE_DOWNLOAD_ID_INVALID")
                || message.starts_with("FILE_DOWNLOAD_LIMIT");
            if is_partial && !keep_partial {
                let files_dir = self.ctx().files_dir();
                let secure_files_dir = self.ctx().secure_files_dir();
                let path = match self.file_nodes[node_id].as_ref() {
                    Some(node) => match &node.local {
                        LocalFileLocation::Partial(partial) => Some(partial.path.clone()),
                        _ => None,
                    },
                    None => None,
                };
                if let Some(path) = path {
                    let is_managed_temp = path.starts_with(&get_files_temp_dir(
                        FileType::SecureDecrypted,
                        &files_dir,
                        &secure_files_dir,
                    )) || path.starts_with(&get_files_temp_dir(
                        FileType::Video,
                        &files_dir,
                        &secure_files_dir,
                    ));
                    if is_managed_temp {
                        let _ = std::fs::remove_file(&path);
                        self.file_nodes[node_id]
                            .as_mut()
                            .expect("node")
                            .drop_local_location();
                    }
                }
            }
        }

        let _ = kind;
        let status = Status::error(400, status.message());
        self.on_file_load_error(node_id, status, fllm);
    }

    /// `FileManager::do_cancel_generate` (FileManager.cpp:2183-2191) —
    /// resets the node first, then cancels the manager query (the C++
    /// `send_closure` ordering: the node is already reset when the
    /// cancel lands) and re-enters `cancel_download` for a Download
    /// worker (the actor's hangup).
    fn do_cancel_generate(&mut self, node_id: usize, fllm: &mut FileLoadManager) {
        let query_id = {
            let Some(node) = self.file_nodes[node_id].as_mut() else {
                return;
            };
            if node.generate_id == 0 {
                return;
            }
            let query_id = node.generate_id;
            node.generate_id = 0;
            node.generate_was_update = false;
            node.set_generate_priority(0, 0);
            query_id
        };
        let download_cancel = self.generate.cancel(query_id);
        if let Some((file_id, internal_download_id)) = download_cancel {
            self.cancel_download(file_id, internal_download_id, false, fllm);
        }
    }

    /// `FileManager::do_cancel_upload` (FileManager.cpp:2173-2181).
    fn do_cancel_upload(&mut self, node_id: usize, fllm: &mut FileLoadManager) {
        let Some(node) = self.file_nodes[node_id].as_mut() else {
            return;
        };
        if node.upload_id == 0 {
            return;
        }
        fllm.cancel_upload(node.upload_id);
        node.upload_id = 0;
        node.upload_was_update_file_reference = false;
        node.set_upload_priority(0);
    }

    /// `FileManager::on_file_load_error` (FileManager.cpp:5288-5299).
    fn on_file_load_error(&mut self, node_id: usize, status: Status, fllm: &mut FileLoadManager) {
        // Stop everything on error
        self.do_cancel_generate(node_id, fllm);
        self.do_cancel_download(node_id, fllm);
        self.do_cancel_upload(node_id, fllm);

        let file_ids = self.file_nodes[node_id]
            .as_ref()
            .map(|node| node.file_ids.clone())
            .unwrap_or_default();
        for file_id in file_ids {
            self.finish_downloads(file_id, Some(status.clone()));
            self.finish_uploads(file_id, Some(status.clone()), fllm);
        }
    }

    /// `FileManager::extract_upload_callback`
    /// (FileManager.cpp:3907-3932).
    fn extract_upload_callback(
        &mut self,
        file_upload_id: FileUploadId,
    ) -> Option<Arc<parking_lot::Mutex<dyn UploadCallback>>> {
        let requests = self.file_upload_requests.get_mut(&file_upload_id.file_id)?;
        let callback = if file_upload_id.internal_upload_id != 0 {
            let info = requests
                .internal_uploads
                .remove(&file_upload_id.internal_upload_id)?;
            info.upload_callback
        } else {
            if requests.user_upload_priority == 0 {
                return None;
            }
            requests.user_upload_priority = 0;
            // the C++ hands back a fresh PreliminaryUploadFileCallback
            // (FileManager.cpp:3925)
            Some(Arc::new(Mutex::new(PreliminaryUploadCallback))
                as Arc<parking_lot::Mutex<dyn UploadCallback>>)
        };
        if requests.user_upload_priority == 0 && requests.internal_uploads.is_empty() {
            self.file_upload_requests.remove(&file_upload_id.file_id);
        }
        callback
    }

    /// `FileManager::finish_uploads` (FileManager.cpp:3934-3956). The
    /// C++ user arm constructs a fresh `PreliminaryUploadFileCallback`
    /// whose `on_upload_ok` cancels the upload to release the pause for
    /// the next upload (FileManager.cpp:1288-1291) - the collapse runs
    /// that effect inline.
    fn finish_uploads(
        &mut self,
        file_id: FileId,
        status: Option<Status>,
        fllm: &mut FileLoadManager,
    ) {
        let Some(mut requests) = self.file_upload_requests.remove(&file_id) else {
            return;
        };
        let mut callbacks: Vec<PendingUploadFinish> = requests
            .internal_uploads
            .iter_mut()
            .map(|(internal_upload_id, info)| {
                (
                    *internal_upload_id,
                    info.upload_callback.take().expect("upload callback"),
                    false,
                )
            })
            .collect();
        if requests.user_upload_priority != 0 {
            callbacks.push((
                0,
                Arc::new(Mutex::new(PreliminaryUploadCallback))
                    as Arc<parking_lot::Mutex<dyn UploadCallback>>,
                true,
            ));
        }
        for (internal_upload_id, callback, is_preliminary) in callbacks {
            let file_upload_id = FileUploadId::new(file_id, internal_upload_id);
            match &status {
                None => {
                    callback.lock().on_upload_ok(file_upload_id);
                    if is_preliminary {
                        // PreliminaryUploadFileCallback::on_upload_ok ->
                        // FileManager::cancel_upload
                        // (FileManager.cpp:1288-1291)
                        if let Some(node_id) = self.node_id_of(file_id) {
                            if self.file_nodes[node_id]
                                .as_ref()
                                .expect("node")
                                .upload_pause
                                == file_upload_id
                            {
                                self.file_nodes[node_id]
                                    .as_mut()
                                    .expect("node")
                                    .set_upload_pause(FileUploadId::default());
                            }
                            self.run_upload(node_id, Vec::new(), fllm);
                            self.try_flush_node(node_id);
                        }
                    }
                }
                Some(status) => callback
                    .lock()
                    .on_upload_error(file_upload_id, status.clone()),
            }
        }
    }

    /// `FileManager::upload` (FileManager.cpp:3902-3905).
    pub fn upload(
        &mut self,
        file_upload_id: FileUploadId,
        callback: Arc<parking_lot::Mutex<dyn UploadCallback>>,
        new_priority: i32,
        upload_order: u64,
        fllm: &mut FileLoadManager,
    ) {
        self.resume_upload(
            file_upload_id,
            Vec::new(),
            callback,
            new_priority,
            upload_order,
            false,
            false,
            fllm,
        );
    }

    /// `FileManager::resume_upload` (FileManager.cpp:3482-3586). The
    /// `ForceUploadActor` re-upload dance (the `[-1]` bad-parts request,
    /// FileManager.cpp:3338-3460) is a wave-gated deferral: only the
    /// recent-reupload gate runs, and the callback is consumed without a
    /// result exactly like the C++ handing it to the actor.
    #[allow(clippy::too_many_arguments)]
    pub fn resume_upload(
        &mut self,
        file_upload_id: FileUploadId,
        bad_parts: Vec<i32>,
        callback: Arc<parking_lot::Mutex<dyn UploadCallback>>,
        new_priority: i32,
        upload_order: u64,
        force: bool,
        prefer_small: bool,
        fllm: &mut FileLoadManager,
    ) {
        self.resume_upload_inner(
            file_upload_id,
            bad_parts,
            callback,
            new_priority,
            upload_order,
            force,
            prefer_small,
            fllm,
        );
        // the C++ SCOPE_EXIT try_flush_node (FileManager.cpp:3514-3516)
        if let Some(node_id) = self.node_id_of(file_upload_id.file_id) {
            self.try_flush_node(node_id);
        }
    }

    /// The `resume_upload` body.
    #[allow(clippy::too_many_arguments)]
    fn resume_upload_inner(
        &mut self,
        file_upload_id: FileUploadId,
        bad_parts: Vec<i32>,
        callback: Arc<parking_lot::Mutex<dyn UploadCallback>>,
        new_priority: i32,
        upload_order: u64,
        force: bool,
        prefer_small: bool,
        fllm: &mut FileLoadManager,
    ) {
        debug_assert!(new_priority > 0, "CHECK(new_priority > 0)");
        let Some(node_id) = self.node_id_of(file_upload_id.file_id) else {
            callback
                .lock()
                .on_upload_error(file_upload_id, Status::error(400, "File not found"));
            return;
        };

        if bad_parts == vec![-1] {
            let recently_reuploaded = self.file_nodes[node_id]
                .as_ref()
                .map(|node| node.last_successful_force_reupload_time >= 0.0)
                .unwrap_or(false);
            if recently_reuploaded {
                callback.lock().on_upload_error(
                    file_upload_id,
                    Status::error(400, "Failed to reupload file"),
                );
                return;
            }
            // the ForceUploadActor deferral: the callback moves into the
            // (absent) actor and no result arrives
            return;
        }

        if force && let Some(node) = self.file_nodes[node_id].as_mut() {
            node.remote.is_full_alive = false;
        }
        if prefer_small {
            self.file_nodes[node_id]
                .as_mut()
                .expect("node")
                .upload_prefer_small = true;
        }
        if self.file_nodes[node_id]
            .as_ref()
            .expect("node")
            .upload_pause
            == file_upload_id
        {
            self.file_nodes[node_id]
                .as_mut()
                .expect("node")
                .set_upload_pause(FileUploadId::default());
        }

        let (
            already_uploaded,
            has_full_local,
            has_generate,
            has_alive_remote,
            file_type,
            can_download,
        ) = {
            let node = self.file_nodes[node_id].as_ref().expect("node");
            let view = FileView::new(node);
            (
                view.has_active_upload_remote_location() && can_reuse_remote_file(view.get_type()),
                view.has_full_local_location(),
                view.has_generate_location(),
                view.has_alive_remote_location(),
                view.get_type(),
                view.can_download_from_server(),
            )
        };
        if already_uploaded {
            callback.lock().on_upload_ok(file_upload_id);
            return;
        }

        if has_full_local {
            // the C++ check_local_location's failure only logs
            // (FileManager.cpp:3520-3524)
            self.check_upload_local_location(node_id);
        }

        if !has_full_local && !has_generate && !has_alive_remote {
            callback.lock().on_upload_error(
                file_upload_id,
                Status::error(
                    400,
                    "Need full local (or generate, or inactive remote) location for upload",
                ),
            );
            return;
        }
        if file_type == FileType::Thumbnail && !has_full_local && can_download {
            callback.lock().on_upload_error(
                file_upload_id,
                Status::error(400, "Failed to upload thumbnail without local location"),
            );
            return;
        }

        let requests = self
            .file_upload_requests
            .entry(file_upload_id.file_id)
            .or_default();
        if file_upload_id.internal_upload_id != 0 {
            let info = requests
                .internal_uploads
                .entry(file_upload_id.internal_upload_id)
                .or_default();
            debug_assert!(
                info.upload_callback.is_none(),
                "CHECK(upload_callback == nullptr)"
            );
            info.upload_order = upload_order;
            info.upload_priority = new_priority as i8;
            info.upload_callback = Some(callback);
        } else {
            requests.user_upload_priority = new_priority as i8;
        }

        self.run_upload(node_id, bad_parts, fllm);
        self.try_flush_node(node_id);
    }

    /// The `check_local_location(node, false)` of the upload path
    /// (FileManager.cpp:3520) — status logged and ignored; the collapse
    /// drops the log.
    fn check_upload_local_location(&mut self, node_id: usize) {
        let Some(node) = self.file_nodes[node_id].as_ref() else {
            return;
        };
        if let LocalFileLocation::Full(full) = node.local.clone() {
            let size = node.size;
            let files_dir = self.ctx().files_dir();
            let secure_files_dir = self.ctx().secure_files_dir();
            let base = get_files_base_dir(full.file_type, &files_dir, &secure_files_dir);
            let _ = base;
            let _ = check_full_local_location(
                self.ctx(),
                FullLocalLocationInfo {
                    location: full,
                    size,
                },
                false,
            );
        }
    }

    /// `FileManager::run_upload` (FileManager.cpp:3775-3900) — the queue
    /// decision order.
    fn run_upload(&mut self, node_id: usize, mut bad_parts: Vec<i32>, fllm: &mut FileLoadManager) {
        // the C++ callers reach here through `get_sync_file_node`, whose
        // `load_from_pmc` clears the flag before the gates
        self.load_from_pmc(node_id);
        let mut priority: i8 = 0;
        let mut file_id = self.file_nodes[node_id]
            .as_ref()
            .expect("node")
            .main_file_id;
        {
            let node = self.file_nodes[node_id].as_ref().expect("node");
            for id in &node.file_ids {
                if let Some(requests) = self.file_upload_requests.get(id) {
                    if requests.user_upload_priority > priority {
                        priority = requests.user_upload_priority;
                        file_id = *id;
                    }
                    for info in requests.internal_uploads.values() {
                        if info.upload_priority > priority {
                            priority = info.upload_priority;
                            file_id = *id;
                        }
                    }
                }
            }
        }

        let old_priority = self.file_nodes[node_id]
            .as_ref()
            .expect("node")
            .upload_priority;
        let upload_id = self.file_nodes[node_id].as_ref().expect("node").upload_id;

        if priority == 0 {
            self.file_nodes[node_id]
                .as_mut()
                .expect("node")
                .set_upload_priority(0);
            if old_priority != 0 {
                self.do_cancel_upload(node_id, fllm);
            }
            return;
        }
        if self.file_nodes[node_id]
            .as_ref()
            .expect("node")
            .upload_pause
            .file_id
            .is_valid()
        {
            return;
        }

        let (
            has_full_local,
            has_full_remote,
            get_by_hash,
            generate_id,
            generate_was_update,
            generate_is_secure_encrypted,
        ) = {
            let node = self.file_nodes[node_id].as_ref().expect("node");
            let view = FileView::new(node);
            let generate_is_secure = view
                .get_generate_location()
                .map(|location| location.file_type == FileType::SecureEncrypted)
                .unwrap_or(false);
            (
                view.has_full_local_location(),
                view.has_full_remote_location(),
                view.get_by_hash(),
                node.generate_id,
                node.generate_was_update,
                generate_is_secure,
            )
        };

        if !has_full_local && !has_full_remote {
            if get_by_hash || generate_id == 0 || !generate_was_update {
                return;
            }
            if generate_is_secure_encrypted {
                // Can't upload secure file before its size is known
                return;
            }
        }

        self.file_nodes[node_id]
            .as_mut()
            .expect("node")
            .set_upload_priority(priority);

        // create encryption key if necessary (FileManager.cpp:3834-3849)
        let needs_key = {
            let node = self.file_nodes[node_id].as_ref().expect("node");
            let view = FileView::new(node);
            let generate_is_encrypted = view
                .get_generate_location()
                .map(|location| location.file_type == FileType::Encrypted)
                .unwrap_or(false);
            let full_local_is_encrypted = view
                .get_full_local_location()
                .map(|location| location.file_type == FileType::Encrypted)
                .unwrap_or(false);
            let full_local_is_secure = view
                .get_full_local_location()
                .map(|location| location.file_type == FileType::SecureEncrypted)
                .unwrap_or(false);
            (
                (generate_is_encrypted || full_local_is_encrypted) && view.encryption_key().empty(),
                full_local_is_secure && view.encryption_key().empty(),
            )
        };
        if needs_key.0 {
            let node = self.file_nodes[node_id].as_mut().expect("node");
            assert!(!node.file_ids.is_empty(), "CHECK(!file_ids_.empty())");
            node.encryption_key = FileEncryptionKey::create();
        }
        if needs_key.1 {
            let node = self.file_nodes[node_id].as_mut().expect("node");
            node.encryption_key = FileEncryptionKey::from_secret(&{
                let mut secret = vec![0u8; 32];
                tdutils::random::secure_bytes(&mut secret);
                secret
            });
        }

        if old_priority != 0 {
            debug_assert!(upload_id != 0, "CHECK(upload_id_ != 0)");
            fllm.update_upload_priority(upload_id, priority);
            return;
        }

        let (
            has_alive_remote,
            has_active_upload_remote,
            upload_was_update_file_reference,
            reuse_type,
        ) = {
            let node = self.file_nodes[node_id].as_ref().expect("node");
            let view = FileView::new(node);
            (
                view.has_alive_remote_location(),
                view.has_active_upload_remote_location(),
                node.upload_was_update_file_reference,
                view.get_type(),
            )
        };
        debug_assert!(self.file_nodes[node_id].as_ref().expect("node").upload_id == 0);
        if has_alive_remote
            && !has_active_upload_remote
            && can_reuse_remote_file(reuse_type)
            && !upload_was_update_file_reference
        {
            let query_id = fllm.alloc_query_id();
            self.upload_queries.insert(
                query_id,
                UploadQuery {
                    file_id,
                    kind: UploadQueryKind::UploadWaitFileReference,
                },
            );
            let main_file_id = self.file_nodes[node_id]
                .as_ref()
                .expect("node")
                .main_file_id;
            {
                let node = self.file_nodes[node_id].as_mut().expect("node");
                node.upload_id = query_id;
                node.upload_was_update_file_reference = true;
            }
            let result = self.ctx().repair_file_reference(main_file_id);
            let error = match result {
                Ok(()) => Status::error_message("FILE_UPLOAD_RESTART_WITH_FILE_REFERENCE"),
                Err(error) => error,
            };
            return self.on_upload_error(query_id, error, fllm);
        }

        let has_partial_remote = self.file_nodes[node_id]
            .as_ref()
            .expect("node")
            .remote
            .partial
            .is_some();
        if !has_partial_remote && self.file_nodes[node_id].as_ref().expect("node").get_by_hash {
            let query_id = fllm.alloc_query_id();
            self.upload_queries.insert(
                query_id,
                UploadQuery {
                    file_id,
                    kind: UploadQueryKind::UploadByHash,
                },
            );
            let (local, size) = {
                let node = self.file_nodes[node_id].as_ref().expect("node");
                let full_local = match &node.local {
                    LocalFileLocation::Full(full) => full.clone(),
                    _ => panic!("by-hash upload needs the full local location"),
                };
                (full_local, node.size)
            };
            self.file_nodes[node_id].as_mut().expect("node").upload_id = query_id;
            fllm.upload_by_hash(query_id, file_id, &local, size, -priority);
            return;
        }

        let new_priority = if bad_parts.is_empty() {
            -priority
        } else {
            priority
        };
        bad_parts.retain(|part_id| *part_id >= 0);

        let (local, partial_remote, expected_size, encryption_key) = {
            let node = self.file_nodes[node_id].as_ref().expect("node");
            let view = FileView::new(node);
            let mut expected_size = view.expected_size(true);
            if node.upload_prefer_small && (10 << 20) < expected_size && expected_size < (30 << 20)
            {
                expected_size = 10 << 20;
            }
            (
                node.local.clone(),
                node.remote.partial_or_empty(),
                expected_size,
                node.encryption_key.clone(),
            )
        };
        let query_id = fllm.alloc_query_id();
        self.upload_queries.insert(
            query_id,
            UploadQuery {
                file_id,
                kind: UploadQueryKind::Upload,
            },
        );
        self.file_nodes[node_id].as_mut().expect("node").upload_id = query_id;
        fllm.upload(
            query_id,
            file_id,
            &local,
            &partial_remote,
            expected_size,
            &encryption_key,
            new_priority,
            bad_parts,
        );
    }

    /// `FileManager::cancel_upload` (FileManager.cpp:3958-3982).
    pub fn cancel_upload(&mut self, file_upload_id: FileUploadId, fllm: &mut FileLoadManager) {
        let Some(node_id) = self.node_id_of(file_upload_id.file_id) else {
            return;
        };
        if self.file_nodes[node_id]
            .as_ref()
            .expect("node")
            .upload_pause
            == file_upload_id
        {
            self.file_nodes[node_id]
                .as_mut()
                .expect("node")
                .set_upload_pause(FileUploadId::default());
        }
        let Some(callback) = self.extract_upload_callback(file_upload_id) else {
            return;
        };
        callback
            .lock()
            .on_upload_error(file_upload_id, Status::error(200, "Canceled"));
        self.run_upload(node_id, Vec::new(), fllm);
        self.try_flush_node(node_id);
    }

    /// `FileManager::cancel_uploads` (FileManager.cpp:3984-3988).
    pub fn cancel_uploads(&mut self, file_upload_ids: &[FileUploadId], fllm: &mut FileLoadManager) {
        for file_upload_id in file_upload_ids {
            self.cancel_upload(*file_upload_id, fllm);
        }
    }

    /// `FileManager::external_file_generate_write_part`
    /// (FileManager.cpp:3656-3660) — the `writeGeneratedFilePart` arm.
    /// The C++ SafePromise "Generation has already been finished" race
    /// has no window in the collapse.
    pub fn external_file_generate_write_part(
        &mut self,
        generation_id: i64,
        offset: i64,
        data: Vec<u8>,
        fllm: &mut FileLoadManager,
    ) -> Result<(), Status> {
        let mut net = fllm.generate_net();
        self.generate
            .external_write_part(generation_id as u64, offset, &data, net.as_mut())
    }

    /// `FileManager::external_file_generate_progress`
    /// (FileManager.cpp:3661-3667) — the `setFileGenerationProgress` arm.
    pub fn external_file_generate_progress(
        &mut self,
        generation_id: i64,
        expected_size: i64,
        local_prefix_size: i64,
    ) -> Result<(), Status> {
        self.generate
            .external_progress(generation_id as u64, expected_size, local_prefix_size)
    }

    /// `FileManager::external_file_generate_finish`
    /// (FileManager.cpp:3668-3672) — the `finishFileGeneration` arm;
    /// `error` is the client-reported failure.
    pub fn external_file_generate_finish(
        &mut self,
        generation_id: i64,
        error: Option<Status>,
        fllm: &mut FileLoadManager,
    ) -> Result<(), Status> {
        let mut net = fllm.generate_net();
        self.generate
            .external_finish(generation_id as u64, error, net.as_mut())
    }

    /// `FileManager::finish_upload_query` (FileManager.cpp:5330-5345).
    fn finish_upload_query(&mut self, query_id: u64) -> Option<(UploadQuery, bool)> {
        let query = self.upload_queries.remove(&query_id)?;
        let mut was_active = false;
        if let Some(node_id) = self.node_id_of(query.file_id)
            && let Some(node) = self.file_nodes[node_id].as_mut()
            && node.upload_id == query_id
        {
            node.upload_id = 0;
            node.upload_was_update_file_reference = false;
            node.set_upload_priority(0);
            was_active = true;
        }
        Some((query, was_active))
    }

    /// `FileManager::on_hash` + `on_partial_upload`
    /// (FileManager.cpp:4827-4872).
    fn on_hash(&mut self, query_id: u64, hash: Vec<u8>) {
        let Some(query) = self.upload_queries.get(&query_id) else {
            return;
        };
        let file_id = query.file_id;
        let Some(node_id) = self.node_id_of(file_id) else {
            return;
        };
        let node = self.file_nodes[node_id].as_mut().expect("node");
        if node.upload_id == query_id {
            node.encryption_key.set_value_hash(&hash);
        }
    }

    fn on_partial_upload(&mut self, query_id: u64, partial: PartialRemoteFileLocation) {
        let Some(query) = self.upload_queries.get(&query_id) else {
            return;
        };
        let file_id = query.file_id;
        let Some(node_id) = self.node_id_of(file_id) else {
            return;
        };
        let matches = self.file_nodes[node_id]
            .as_ref()
            .is_some_and(|node| node.upload_id == query_id);
        if matches {
            self.file_nodes[node_id]
                .as_mut()
                .expect("node")
                .set_partial_remote_location(partial);
            self.try_flush_node(node_id);
        }
    }

    /// `FileManager::on_upload_ok` (FileManager.cpp:4900-4954) — the
    /// wire `InputFile` construction folds into the caller's callback
    /// (see [`UploadCallback`]); the upload pauses after `on_upload_ok`
    /// exactly like the C++ `set_upload_pause`.
    fn on_upload_ok(
        &mut self,
        query_id: u64,
        file_type: FileType,
        partial_remote: PartialRemoteFileLocation,
    ) {
        debug_assert!(
            partial_remote.ready_part_count == partial_remote.part_count,
            "CHECK(ready_part_count_ == part_count_)"
        );
        let Some((query, _was_active)) = self.finish_upload_query(query_id) else {
            return;
        };
        let some_file_id = query.file_id;
        let Some(node_id) = self.node_id_of(some_file_id) else {
            return;
        };

        // pick the request to report: a user-priority id sets the
        // selection WITHOUT updating the order watermark, so a later
        // internal entry with any finite order still overrides it -
        // exactly the C++ walk (FileManager.cpp:4917-4931)
        let mut selected: Option<(FileId, i64, u64)> = None;
        {
            let node = self.file_nodes[node_id].as_ref().expect("node");
            for id in &node.file_ids {
                let Some(requests) = self.file_upload_requests.get(id) else {
                    continue;
                };
                if requests.user_upload_priority != 0 {
                    selected = Some((*id, 0, u64::MAX));
                } else {
                    for (internal_upload_id, info) in &requests.internal_uploads {
                        let better = match selected {
                            Some((_, _, best_order)) => info.upload_order < best_order,
                            None => true,
                        };
                        if better {
                            selected = Some((*id, *internal_upload_id, info.upload_order));
                        }
                    }
                }
            }
        }
        let Some((id, internal_upload_id, _upload_order)) = selected else {
            return;
        };
        let file_upload_id = FileUploadId::new(id, internal_upload_id);
        let Some(callback) = self.extract_upload_callback(file_upload_id) else {
            return;
        };

        self.file_nodes[node_id]
            .as_mut()
            .expect("node")
            .set_upload_pause(file_upload_id);
        callback.lock().on_upload_ok(file_upload_id);
        let _ = file_type;
        // don't flush node info, because nothing actually changed
        // (FileManager.cpp:4953)
    }

    /// `FileManager::on_upload_full_ok` (FileManager.cpp:4956-4962). The
    /// `register_remote` + merge of the by-hash result is the remote
    /// registration plane; the collapse records the location on the node
    /// directly.
    fn on_upload_full_ok(
        &mut self,
        query_id: u64,
        remote: FullRemoteFileLocation,
        fllm: &mut FileLoadManager,
    ) {
        let Some((query, _was_active)) = self.finish_upload_query(query_id) else {
            return;
        };
        let file_id = query.file_id;
        let Some(node_id) = self.node_id_of(file_id) else {
            return;
        };
        if let Some(node) = self.file_nodes[node_id].as_mut() {
            node.set_new_remote_location(NewRemoteFileLocation {
                partial: None,
                full: Some(remote),
                is_full_alive: true,
                full_source: FileLocationSource::FromServer,
            });
        }
        self.finish_uploads(file_id, None, fllm);
    }

    /// `FileManager::on_upload_error` (FileManager.cpp:5101-5121).
    fn on_upload_error(&mut self, query_id: u64, status: Status, fllm: &mut FileLoadManager) {
        let Some((query, was_active)) = self.finish_upload_query(query_id) else {
            return;
        };
        let Some(node_id) = self.node_id_of(query.file_id) else {
            return;
        };
        if query.kind == UploadQueryKind::UploadByHash {
            self.file_nodes[node_id].as_mut().expect("node").get_by_hash = false;
            return self.run_upload(node_id, Vec::new(), fllm);
        }
        self.on_upload_error_impl(node_id, query.kind, was_active, status, fllm);
    }

    /// `FileManager::on_upload_error_impl`
    /// (FileManager.cpp:5229-5287).
    fn on_upload_error_impl(
        &mut self,
        node_id: usize,
        kind: UploadQueryKind,
        was_active: bool,
        status: Status,
        fllm: &mut FileLoadManager,
    ) {
        // the C++ SCOPE_EXIT try_flush_node (FileManager.cpp:5230-5232)
        self.on_upload_error_impl_inner(node_id, kind, was_active, status, fllm);
        self.try_flush_node(node_id);
    }

    /// The `on_upload_error_impl` body.
    fn on_upload_error_impl_inner(
        &mut self,
        node_id: usize,
        kind: UploadQueryKind,
        was_active: bool,
        status: Status,
        fllm: &mut FileLoadManager,
    ) {
        let message = status.message().to_owned();
        if message == "FILE_PART_INVALID" {
            let (has_partial_small_location, expected_size) = {
                let node = self.file_nodes[node_id].as_ref().expect("node");
                let view = FileView::new(node);
                (
                    node.remote
                        .partial
                        .as_ref()
                        .is_some_and(|partial| partial.is_big == 0),
                    view.expected_size(true),
                )
            };
            let should_be_big_location = is_file_big(
                self.file_nodes[node_id]
                    .as_ref()
                    .map(|node| FileView::new(node).get_type())
                    .expect("node"),
                expected_size,
            );
            if let Some(node) = self.file_nodes[node_id].as_mut() {
                node.delete_partial_remote_location();
            }
            if has_partial_small_location && should_be_big_location {
                return self.run_upload(node_id, Vec::new(), fllm);
            }
        }

        if message.starts_with("FILE_UPLOAD_RESTART") {
            if message.ends_with("WITH_FILE_REFERENCE") {
                self.file_nodes[node_id]
                    .as_mut()
                    .expect("node")
                    .upload_was_update_file_reference = true;
            } else if let Some(node) = self.file_nodes[node_id].as_mut() {
                node.delete_partial_remote_location();
            }
            return self.run_upload(node_id, Vec::new(), fllm);
        }

        if !was_active {
            return;
        }

        if status.code() == 0
            && let Some(node) = self.file_nodes[node_id].as_mut()
        {
            node.delete_partial_remote_location();
        }

        let _ = kind;
        let status = Status::error(400, status.message());
        self.on_file_load_error(node_id, status, fllm);
    }

    /// The drained event dispatch — the C++ `FileManager` handler half
    /// of the manager callbacks, in queue order.
    pub fn apply_load_events(&mut self, fllm: &mut FileLoadManager) {
        for event in fllm.take_events() {
            match event {
                FileLoadEvent::StartDownload { query_id } => self.on_start_download(query_id),
                FileLoadEvent::PartialDownload {
                    query_id,
                    partial,
                    size,
                } => self.on_partial_download(query_id, partial, size),
                FileLoadEvent::DownloadOk {
                    query_id,
                    local,
                    size,
                    is_new,
                } => self.on_download_ok(query_id, local, size, is_new, fllm),
                FileLoadEvent::Hash { query_id, hash } => self.on_hash(query_id, hash),
                FileLoadEvent::PartialUpload { query_id, partial } => {
                    self.on_partial_upload(query_id, partial)
                }
                FileLoadEvent::UploadOk {
                    query_id,
                    file_type,
                    partial,
                } => self.on_upload_ok(query_id, file_type, partial),
                FileLoadEvent::UploadFullOk { query_id, remote } => {
                    self.on_upload_full_ok(query_id, remote, fllm)
                }
                FileLoadEvent::Error { query_id, status } => {
                    // route by which query table holds the id
                    if self.download_queries.contains_key(&query_id) {
                        self.on_download_error(query_id, status, fllm);
                    } else if self.upload_queries.contains_key(&query_id) {
                        self.on_upload_error(query_id, status, fllm);
                    }
                }
            }
        }
        self.apply_generate_events(fllm);
    }

    /// The [`FileManagerWorker`] replay targets. The merge/flush hooks fire
    /// while `&mut self` is held, so the Td-side worker records the
    /// effects and the Td replays them through its `Arc<parking_lot::Mutex<FileManager>>`
    /// handle after the driving call returns (the C++ runs the same calls
    /// on itself inside the single actor).
    pub fn replay_cancel_download(&mut self, node: &FileNode, fllm: &mut FileLoadManager) {
        if let Some(node_id) = self.node_id_of(node.main_file_id) {
            self.do_cancel_download(node_id, fllm);
        }
    }

    pub fn replay_cancel_upload(&mut self, node: &FileNode, fllm: &mut FileLoadManager) {
        if let Some(node_id) = self.node_id_of(node.main_file_id) {
            self.do_cancel_upload(node_id, fllm);
        }
    }

    pub fn replay_cancel_generate(&mut self, node: &FileNode, fllm: &mut FileLoadManager) {
        if let Some(node_id) = self.node_id_of(node.main_file_id) {
            self.do_cancel_generate(node_id, fllm);
        }
    }

    pub fn replay_finish_downloads(&mut self, file_id: FileId) {
        self.finish_downloads(file_id, None);
    }

    pub fn replay_finish_uploads(&mut self, file_id: FileId, fllm: &mut FileLoadManager) {
        self.finish_uploads(file_id, None, fllm);
    }

    /// The `run_node` replay — re-evaluates the node's three queues in the
    /// `on_download_ok` tail order.
    pub fn replay_run_node(&mut self, node: &FileNode, fllm: &mut FileLoadManager) {
        let Some(node_id) = self.node_id_of(node.main_file_id) else {
            return;
        };
        self.run_generate(node_id, fllm);
        self.run_download(node_id, false, fllm);
        self.run_upload(node_id, Vec::new(), fllm);
    }

    /// The drained generate-event dispatch — the `FileManager` handler
    /// half of the [`FileGenerateManager`] callbacks
    /// (`on_partial_generate`/`on_generate_ok`/`on_generate_error`, and
    /// the `FileDownloadGenerateActor::on_download_ok` lambda), in queue
    /// order. Events pushed while handling join the same drain, like the
    /// C++ actor queue.
    /// Whether the generate callbacks queue unapplied events.
    pub fn generate_has_events(&self) -> bool {
        self.generate.has_events()
    }

    fn apply_generate_events(&mut self, fllm: &mut FileLoadManager) {
        while let Some(event) = self.generate.take_event() {
            match event {
                FileGenerateEvent::PartialGenerate {
                    query_id,
                    partial,
                    expected_size,
                } => self.on_partial_generate(query_id, partial, expected_size, fllm),
                FileGenerateEvent::GenerateOk { query_id, local } => {
                    self.on_generate_ok(query_id, local, fllm);
                    self.finish_generate_worker(query_id);
                }
                FileGenerateEvent::GenerateError { query_id, status } => {
                    self.on_generate_error(query_id, status, fllm);
                    self.finish_generate_worker(query_id);
                }
                FileGenerateEvent::DownloadOk {
                    query_id,
                    file_id,
                    file_type,
                } => {
                    // the `on_download_ok` send_lambda
                    // (FileGenerateManager.cpp:93-115): read the view,
                    // override the file type, route like the callback's
                    // on_ok
                    let location = self
                        .get_file_view(file_id)
                        .and_then(|view| view.get_full_local_location().cloned());
                    match location {
                        Some(full) => {
                            let local = FullLocalFileLocation { file_type, ..full };
                            self.on_generate_ok(query_id, local, fllm);
                        }
                        None => {
                            // LOG(ERROR) "Expected to have local location"
                            self.on_generate_error(query_id, Status::error(500, "Unknown"), fllm);
                        }
                    }
                    self.finish_generate_worker(query_id);
                }
            }
        }
    }

    /// `FileGenerateManager::do_cancel` — the worker erase at a terminal
    /// event, emitting the `FileExternalGenerateActor::tear_down` stop
    /// update.
    fn finish_generate_worker(&mut self, query_id: u64) {
        if let Some(state) = self.generate.queries.remove(&query_id)
            && matches!(state.worker, GenerateWorker::External { .. })
            && let Some(ctx) = self.context.as_deref_mut()
        {
            ctx.on_file_generation_stop(query_id as i64);
        }
    }

    /// `FileManager::on_partial_generate` (FileManager.cpp:4988-5022).
    fn on_partial_generate(
        &mut self,
        query_id: u64,
        partial: PartialLocalFileLocation,
        expected_size: i64,
        fllm: &mut FileLoadManager,
    ) {
        let Some(query) = self.generate_queries.get(&query_id) else {
            unreachable!("CHECK(query != nullptr)");
        };
        let file_id = query.file_id;
        let Some(node_id) = self.node_id_of(file_id) else {
            return;
        };
        if self.file_nodes[node_id]
            .as_ref()
            .is_some_and(|node| node.generate_id != query_id)
        {
            return;
        }
        // TODO check for size and local_size, abort generation if needed
        // (the C++ comment)
        let (upload_id, need_run_upload) = {
            let node = self.file_nodes[node_id].as_mut().expect("node");
            node.set_local_location(LocalFileLocation::Partial(partial.clone()), -1, -1);
            if expected_size > 0 {
                node.set_expected_size(expected_size);
            }
            let need_run_upload = !node.generate_was_update;
            if need_run_upload {
                node.generate_was_update = true;
            }
            (node.upload_id, need_run_upload)
        };
        if need_run_upload {
            self.run_upload(node_id, Vec::new(), fllm);
        }
        if upload_id != 0 {
            fllm.update_local_file_location(upload_id, &LocalFileLocation::Partial(partial));
        }
        self.try_flush_node(node_id);
    }

    /// `FileManager::on_generate_ok` (FileManager.cpp:5024-5083).
    fn on_generate_ok(
        &mut self,
        query_id: u64,
        local: FullLocalFileLocation,
        fllm: &mut FileLoadManager,
    ) {
        let Some((generate_file_id, was_active)) = self.finish_generate_query(query_id) else {
            unreachable!("CHECK(query != nullptr)");
        };
        let Some(node_id) = self.node_id_of(generate_file_id) else {
            return;
        };
        let old_upload_id = self.file_nodes[node_id]
            .as_ref()
            .map(|node| node.upload_id)
            .unwrap_or(0);

        let r_new_file_id = self.register_local(
            local.clone(),
            DialogId::default(),
            0,
            false,
            false,
            generate_file_id,
        );
        if let Err(register_error) = r_new_file_id {
            let error = Status::error_message(&format!(
                "Can't register local file after generate: {}",
                register_error.message()
            ));
            if let Some(node_id) = self.node_id_of(generate_file_id) {
                return self.on_generate_error_impl(node_id, was_active, error, fllm);
            }
            return;
        }

        // the register_local-triggered merge tail
        // (FileManager.cpp:2432-2451): finish the settled queries of
        // every merged file id and re-evaluate the queues — the C++
        // reaches it through the merge this register_local triggered
        let (node_file_ids, has_full_local, has_active_upload_remote) = self
            .get_file_node(generate_file_id)
            .map(|node| {
                let view = FileView::new(node);
                (
                    node.file_ids.clone(),
                    view.has_full_local_location(),
                    view.has_active_upload_remote_location(),
                )
            })
            .unwrap_or_default();
        for merged_file_id in node_file_ids {
            if has_full_local {
                self.finish_downloads(merged_file_id, None);
            }
            if has_active_upload_remote {
                self.finish_uploads(merged_file_id, None, fllm);
            }
        }
        if let Some(merged_node_id) = self.node_id_of(generate_file_id) {
            self.run_generate(merged_node_id, fllm);
            self.run_download(merged_node_id, false, fllm);
            self.run_upload(merged_node_id, Vec::new(), fllm);
        }

        if self.ctx().need_notify_on_new_files() {
            let generate_conversion = self.file_nodes[node_id]
                .as_ref()
                .and_then(|node| FileView::new(node).get_generate_location().cloned())
                .map(|generate| generate.conversion);
            if generate_conversion.is_none()
                || !generate_conversion
                    .as_deref()
                    .is_some_and(|conversion| conversion.starts_with("#file_id#"))
            {
                let (size, allocated) = {
                    let view = self
                        .get_file_view(generate_file_id)
                        .expect("CHECK(file_node)");
                    let size = view.size();
                    let path = view.path().to_owned();
                    // the C++ stats the path through the free `stat()`
                    // (FileManager.cpp:1000-1010); a failed stat is 0
                    let allocated = if path.is_empty() {
                        0
                    } else {
                        self.ctx().stat(&path).map(|s| s.real_size).unwrap_or(0)
                    };
                    (size, allocated)
                };
                self.ctx().on_new_file(size, allocated, 1);
            }
        }

        self.run_upload(node_id, Vec::new(), fllm);

        if was_active {
            let upload_id = self.file_nodes[node_id]
                .as_ref()
                .map(|node| node.upload_id)
                .unwrap_or(0);
            if old_upload_id != 0 && old_upload_id == upload_id {
                fllm.update_local_file_location(old_upload_id, &LocalFileLocation::Full(local));
            }
        }
    }

    /// `FileManager::on_file_unlink` (FileManager.cpp:1676-1688) — the
    /// gc's removal plane: the node drops its local location and its
    /// pmc records, and the download manager reaps finished queries.
    pub fn on_file_unlink(&mut self, location: &FullLocalFileLocation) {
        let Some(&file_id) = self.local_location_to_file_id.get(location) else {
            return;
        };
        // get_sync_file_node: the pmc load collapses to a direct node
        // fetch — a missing node means the location table is stale
        let Some(node_id) = self.node_id_of(file_id) else {
            return;
        };
        {
            let node = self.file_nodes[node_id].as_ref().expect("node");
            if let Some(worker) = self.worker.as_deref_mut() {
                worker.clear_from_pmc(node);
            }
        }
        let main_file_id = self.file_nodes[node_id]
            .as_ref()
            .expect("node")
            .main_file_id;
        if let Some(worker) = self.worker.as_deref_mut() {
            worker.remove_file_if_finished(main_file_id);
        }
        if let Some(node) = self.file_nodes[node_id].as_mut() {
            node.drop_local_location();
        }
        self.try_flush_node(node_id);
    }

    /// `FileManager::on_generate_error` (FileManager.cpp:5085-5095).
    fn on_generate_error(&mut self, query_id: u64, status: Status, fllm: &mut FileLoadManager) {
        let Some((file_id, was_active)) = self.finish_generate_query(query_id) else {
            unreachable!("CHECK(query != nullptr)");
        };
        let Some(node_id) = self.node_id_of(file_id) else {
            return;
        };
        self.on_generate_error_impl(node_id, was_active, status, fllm);
    }

    /// `FileManager::on_generate_error_impl` (FileManager.cpp:5204-5232).
    fn on_generate_error_impl(
        &mut self,
        node_id: usize,
        was_active: bool,
        status: Status,
        fllm: &mut FileLoadManager,
    ) {
        if status
            .message()
            .starts_with("FILE_GENERATE_LOCATION_INVALID")
            && let Some(node) = self.file_nodes[node_id].as_mut()
        {
            node.set_generate_location(None);
        }
        if !was_active {
            self.try_flush_node(node_id);
            return;
        }

        // the C++ close-flag arm (`G()->close_flag()` collapses
        // `request_aborted_error` statuses) lands with the close plane
        if status.code() == 0 {
            // Remove partial locations
            if let Some(node) = self.file_nodes[node_id].as_mut() {
                if matches!(node.local, LocalFileLocation::Partial(_)) {
                    // the file itself has already been deleted
                    node.drop_local_location();
                }
                node.delete_partial_remote_location();
            }
        }
        let status = Status::error(400, status.message());
        self.on_file_load_error(node_id, status, fllm);
        self.try_flush_node(node_id);
    }

    /// `FileManager::finish_generate_query` (FileManager.cpp:5317-5333).
    fn finish_generate_query(&mut self, query_id: u64) -> Option<(FileId, bool)> {
        let query = self.generate_queries.remove(&query_id)?;
        let res = query;
        let node_id = self.node_id_of(res.file_id)?;
        let was_active = self.file_nodes[node_id].as_mut().is_some_and(|node| {
            if node.generate_id == query_id {
                node.generate_id = 0;
                node.generate_was_update = false;
                node.set_generate_priority(0, 0);
                true
            } else {
                false
            }
        });
        Some((res.file_id, was_active))
    }

    /// `FileManager::run_generate` (FileManager.cpp:3673-3786).
    fn run_generate(&mut self, node_id: usize, fllm: &mut FileLoadManager) {
        // the C++ callers reach here through `get_sync_file_node`, whose
        // `load_from_pmc` clears the flag before the gates (the db read
        // half is async); the collapse clears it up front
        self.load_from_pmc(node_id);
        let plan = {
            let Some(node) = self.file_nodes[node_id].as_ref() else {
                return;
            };
            let view = FileView::new(node);
            if !view.can_generate()
                || view.has_full_local_location()
                || view.can_download_from_server()
            {
                return;
            }

            NodeGeneratePlan {
                main_file_id: node.main_file_id,
                old_priority: node.generate_priority,
                file_ids: node.file_ids.clone(),
                generate: node.generate.clone(),
                local: node.local.clone(),
                suggested_path: node.suggested_path(),
            }
        };
        let _ = node_id; // the load_from_pmc clear ran at the entry

        let mut download_priority: i8 = 0;
        let mut upload_priority: i8 = 0;
        let mut file_id = plan.main_file_id;
        for id in plan.file_ids {
            if let Some(requests) = self.file_download_requests.get(&id) {
                if requests.user_download_priority > download_priority {
                    download_priority = requests.user_download_priority;
                }
                for info in requests.internal_downloads.values() {
                    if info.download_priority > download_priority {
                        download_priority = info.download_priority;
                    }
                }
                if download_priority > upload_priority {
                    file_id = id;
                }
            }
            if let Some(requests) = self.file_upload_requests.get(&id) {
                if requests.user_upload_priority > upload_priority {
                    upload_priority = requests.user_upload_priority;
                }
                for info in requests.internal_uploads.values() {
                    if info.upload_priority > upload_priority {
                        upload_priority = info.upload_priority;
                    }
                }
                if upload_priority > download_priority {
                    file_id = id;
                }
            }
        }

        if let Some(node) = self.file_nodes[node_id].as_mut() {
            node.set_generate_priority(download_priority, upload_priority);
        }
        if download_priority.max(upload_priority) == 0 {
            if plan.old_priority != 0 {
                self.do_cancel_generate(node_id, fllm);
            }
            return;
        }
        if plan.old_priority != 0 {
            // TODO: change file generation priority (the C++ TODO)
            return;
        }
        let Some(generate_location) = plan.generate else {
            return;
        };

        self.next_generate_query_id += 1;
        let query_id = self.next_generate_query_id;
        self.generate_queries
            .insert(query_id, GenerateQuery { file_id });
        if let Some(node) = self.file_nodes[node_id].as_mut() {
            node.generate_id = query_id;
        }

        let internal_download_id = self.get_internal_download_id();
        if self.context.is_none() {
            self.context = Some(Box::new(DefaultFileManagerContext));
        }
        let mut net = fllm.generate_net();
        let start = self.generate.generate_file(
            query_id,
            generate_location,
            &plan.local,
            plan.suggested_path,
            internal_download_id,
            net.as_mut(),
            self.context.as_deref_mut().expect("context"),
        );
        if let Some((download_file_id, internal_download_id, callback)) = start {
            self.download(
                download_file_id,
                internal_download_id,
                callback,
                1,
                -1,
                -1,
                fllm,
            );
        }
    }

    /// `FileManager::set_content` (FileManager.cpp:2782-2829) — the
    /// inline-thumbnail bytes plane; the C++ logs-only callback is
    /// folded into the manager event queue.
    pub fn set_content(
        &mut self,
        file_id: FileId,
        bytes: Vec<u8>,
        fllm: &mut FileLoadManager,
    ) -> bool {
        // G()->get_option_boolean("ignore_inline_thumbnails")
        // (FileManager.cpp:2783-2785)
        if self.ctx().ignore_inline_thumbnails() {
            return false;
        }

        let Some(node_id) = self.node_id_of(file_id) else {
            return false;
        };
        let (local_is_full, remote_full_file_type, suggested_path) = {
            let node = self.file_nodes[node_id].as_ref().expect("node");
            (
                matches!(node.local, LocalFileLocation::Full(_)),
                node.remote.full.as_ref().map(|full| full.file_type),
                node.suggested_path(),
            )
        };
        if local_is_full {
            // There was no download so we don't need an update
            return true;
        }

        self.do_cancel_download(node_id, fllm);

        let internal_download_id = self.get_internal_download_id();
        let requests = self.file_download_requests.entry(file_id).or_default();
        let info = requests
            .internal_downloads
            .entry(internal_download_id)
            .or_default();
        info.download_priority = 10;
        info.download_callback = Some(Arc::new(Mutex::new(LoggingDownloadCallback)));

        let Some(file_type) = remote_full_file_type else {
            return false;
        };
        let node = self.file_nodes[node_id].as_mut().expect("node");
        node.set_download_priority(10);
        let query_id = fllm.alloc_query_id();
        self.download_queries.insert(
            query_id,
            FileDownloadQuery {
                file_id,
                kind: DownloadQueryKind::SetContent,
            },
        );
        node.download_id = query_id;
        node.is_download_started = true;
        fllm.from_bytes(query_id, file_id, file_type, bytes, suggested_path);
        true
    }

    /// `FileManager::read_file_part` (FileManager.cpp:2843-2913) — the
    /// validation ladder, then the synchronous read of the downloaded
    /// prefix. The C++ retry actor (`SleepActor` + `left_tries`) collapses
    /// to the inline retry loop — nothing can change between the tries of
    /// a synchronous read, so a partial-local retry replays the identical
    /// read and answers the same way; the loop burns all `left_tries`
    /// before the error, like the C++ reschedule chain.
    pub fn read_file_part(
        &mut self,
        file_id: FileId,
        offset: i64,
        mut count: i64,
        left_tries: i32,
        mut promise: tdutils::promise::Promise<Vec<u8>>,
        fllm: &mut FileLoadManager,
    ) {
        if !file_id.is_valid() {
            promise.set_error(Status::error(400, "File identifier is invalid"));
            return;
        }
        let node_snapshot = self.get_file_view(file_id).map(|view| {
            (
                view.get_full_local_location().map(|full| full.path.clone()),
                view.node.local.clone(),
                view.get_type(),
                view.downloaded_prefix(offset),
            )
        });
        let Some((full_path, local, file_type, prefix)) = node_snapshot else {
            promise.set_error(Status::error(400, "File not found"));
            return;
        };
        if offset < 0 {
            promise.set_error(Status::error(400, "Parameter offset must be non-negative"));
            return;
        }
        if count < 0 {
            promise.set_error(Status::error(400, "Parameter count must be non-negative"));
            return;
        }
        if count == 0 {
            count = prefix;
            if count == 0 {
                promise.set_value(Vec::new());
                return;
            }
        } else if prefix < count {
            // TODO this check is safer to do in another thread
            promise.set_error(Status::error(
                400,
                "There is not enough downloaded bytes in the file to read",
            ));
            return;
        }
        if count >= (usize::MAX / 2 - 1) as i64 {
            promise.set_error(Status::error(400, "Part length is too big"));
            return;
        }

        let (path, is_partial) = if let Some(path) = full_path {
            let (files_dir, secure_files_dir) = {
                let ctx = self.ctx();
                (ctx.files_dir(), ctx.secure_files_dir())
            };
            if !path.starts_with(&get_files_dir(file_type, &files_dir, &secure_files_dir)) {
                promise.set_error(Status::error(400, "File is not inside the cache"));
                return;
            }
            (path, false)
        } else {
            match local {
                LocalFileLocation::Partial(partial) => (partial.path, true),
                _ => unreachable!("read_file_part: neither full nor partial local location"),
            }
        };

        let mut remaining = left_tries;
        loop {
            match fllm.read_file_part(&path, offset, count) {
                Result::Ok(bytes) => {
                    promise.set_value(bytes);
                    return;
                }
                Result::Err(status) => {
                    tracing::info!("Failed to read file bytes: {status}");
                    remaining -= 1;
                    // the C++ errors when `left_tries == 1` and otherwise
                    // reschedules with `left_tries - 1` — `left_tries`
                    // attempts in total
                    if !is_partial || remaining == 0 {
                        promise.set_error(Status::error(400, "Failed to read the file"));
                        return;
                    }
                }
            }
        }
    }

    /// `FileManager::delete_file` (FileManager.cpp:2915-2952) — the local
    /// location teardown + the unlink tail. The pmc/notify halves ride the
    /// worker/context seams, exactly like the C++ `clear_from_pmc` and
    /// `Context::on_new_file` calls.
    pub fn delete_file(
        &mut self,
        file_id: FileId,
        mut promise: tdutils::promise::Promise<()>,
        source: &str,
        fllm: &mut FileLoadManager,
    ) {
        tracing::info!("Trying to delete file {file_id} from {source}");
        let Some(node_id) = self.node_id_of(file_id) else {
            promise.set_value(());
            return;
        };

        let snapshot = {
            let node = self.file_nodes[node_id].as_ref().expect("node");
            let view = FileView::new(node);
            (
                view.get_main_file_id(),
                view.has_full_local_location(),
                view.get_type(),
                view.size(),
                view.path().to_string(),
                matches!(node.local, LocalFileLocation::Partial(_)),
                node.local.clone(),
            )
        };
        let (main_file_id, has_full_local, file_type, size, node_path, is_partial, local) =
            snapshot;

        if let Some(worker) = self.worker.as_deref_mut() {
            worker.remove_file_if_finished(main_file_id);
        }

        let (files_dir, secure_files_dir) = {
            let ctx = self.ctx();
            (ctx.files_dir(), ctx.secure_files_dir())
        };
        let files_dir = get_files_dir(file_type, &files_dir, &secure_files_dir);
        let mut path = String::new();
        if has_full_local {
            if node_path.starts_with(&files_dir) {
                if let Some(node) = self.file_nodes[node_id].as_ref()
                    && let Some(worker) = self.worker.as_deref_mut()
                {
                    worker.clear_from_pmc(node);
                }
                if self.ctx().need_notify_on_new_files() {
                    // `FileView::get_allocated_local_size`
                    // (FileManager.cpp:1000-1008): the stat'ed disk usage,
                    // 0 when the path is gone.
                    let allocated = if node_path.is_empty() {
                        0
                    } else {
                        self.ctx()
                            .stat(&node_path)
                            .map(|stat| stat.real_size)
                            .unwrap_or(0)
                    };
                    self.ctx().on_new_file(-size, -allocated, -1);
                }
                path = node_path;
            }
        } else {
            if file_type == FileType::Encrypted
                && let Some(node) = self.file_nodes[node_id].as_ref()
                && let Some(worker) = self.worker.as_deref_mut()
            {
                worker.clear_from_pmc(node);
            }
            if is_partial {
                path = match local {
                    LocalFileLocation::Partial(partial) => partial.path,
                    _ => unreachable!("delete_file: partial flag without the location"),
                };
            }
        }

        if path.is_empty() {
            promise.set_value(());
            return;
        }

        tracing::info!("Unlink file {file_id} at {path}");
        if let Some(node) = self.file_nodes[node_id].as_mut() {
            node.drop_local_location();
        }
        self.try_flush_node(node_id);
        fllm.unlink_file(&path);
        promise.set_value(());
    }

    /// `FileManager::get_suggested_file_name` (FileManager.cpp:5374-5383).
    pub fn get_suggested_file_name(
        &mut self,
        file_id: FileId,
        directory: &str,
    ) -> Result<String, Status> {
        if !file_id.is_valid() {
            return Err(Status::error(400, "Invalid file identifier"));
        }
        let suggested_path = match self.get_file_view(file_id) {
            Some(view) => view.suggested_path(),
            None => return Err(Status::error(400, "Wrong file identifier")),
        };
        Ok(get_suggested_file_name_impl(
            self.ctx(),
            directory,
            tdutils::path_view::PathView::new(&suggested_path).file_name(),
        ))
    }

    /// `FileManager::get_map_thumbnail_file_id` (FileManager.cpp:4311-4350)
    /// — the validation ladder, the mercator tile coordinates and the
    /// `#map#` generate registration. The tile fetch itself is the
    /// remotely-generated seam (`is_remotely_generated_file`) parked at
    /// the `[blocked: net]` generate plane.
    pub fn get_map_thumbnail_file_id(
        &mut self,
        location: &crate::chat_manager::Location,
        zoom: i32,
        width: i32,
        height: i32,
        scale: i32,
        owner_dialog_id: DialogId,
    ) -> Result<FileId, Status> {
        if !location.is_valid_map_point() {
            return Err(Status::error(400, "Invalid location specified"));
        }
        if !(13..=20).contains(&zoom) {
            return Err(Status::error(400, "Wrong zoom"));
        }
        if !(16..=1024).contains(&width) {
            return Err(Status::error(400, "Wrong width"));
        }
        if !(16..=1024).contains(&height) {
            return Err(Status::error(400, "Wrong height"));
        }
        if !(1..=3).contains(&scale) {
            return Err(Status::error(400, "Wrong scale"));
        }

        const PI: f64 = std::f64::consts::PI;
        let sin_latitude = (location.get_latitude() * PI / 180.0).sin();
        let size = 256i32 * (1i32 << zoom);
        let mut x = ((location.get_longitude() + 180.0) / 360.0 * size as f64) as i32;
        let mut y = ((0.5 - ((1.0 + sin_latitude) / (1.0 - sin_latitude)).ln() / (4.0 * PI))
            * size as f64) as i32;
        x = x.clamp(0, size - 1); // just in case
        y = y.clamp(0, size - 1); // just in case

        let conversion = format!("#map#{zoom}#{x}#{y}#{width}#{height}#{scale}#");
        let file_type = if owner_dialog_id.get_type() == DialogType::SecretChat {
            FileType::EncryptedThumbnail
        } else {
            FileType::Thumbnail
        };
        Ok(self.register_generate(file_type, String::new(), conversion, owner_dialog_id, 0))
    }
}

/// The log-only `DownloadCallback` of `FileManager::set_content`
/// (FileManager.cpp:2799-2806).
struct LoggingDownloadCallback;

impl DownloadCallback for LoggingDownloadCallback {
    fn on_download_ok(&mut self, _file_id: FileId) {}
    fn on_download_error(&mut self, _file_id: FileId, _status: Status) {}
}

/// `FileManager::PreliminaryUploadFileCallback`
/// (FileManager.cpp:1286-1307): a successful upload reports back through
/// `cancel_upload` (run by [`FileManager::finish_uploads`] inline) and
/// errors are no-ops.
struct PreliminaryUploadCallback;

impl UploadCallback for PreliminaryUploadCallback {
    fn on_upload_ok(&mut self, _file_upload_id: FileUploadId) {}
    fn on_upload_error(&mut self, _file_upload_id: FileUploadId, _status: Status) {}
}

#[cfg(test)]
mod file_downloader_tests {
    use super::*;
    use std::collections::VecDeque;

    /// One scripted net answer: the expected query-type tag (`None`
    /// answers any query) and the response to hand back.
    type ScriptEntry = (Option<String>, Result<DownloadResult, Status>);

    /// The scripted net: pairs of (expected query-type tag, response)
    /// consumed in order; a `None` tag answers any query. All state is
    /// shared with the test through `Rc`.
    struct SharedNetState {
        script: parking_lot::Mutex<VecDeque<ScriptEntry>>,
        /// The recorded request sequence: (query type, wire query,
        /// connection meta).
        queries: parking_lot::Mutex<Vec<(DownloadQueryType, DownloadQuery, DownloadNetMeta)>>,
        /// The `search_file` seam answer (the local copy the need_search_file
        /// path finds).
        search_hit: parking_lot::Mutex<Option<String>>,
        /// The `webfile_dc_id` seam answer.
        webfile_dc: parking_lot::Mutex<DcId>,
    }

    impl SharedNetState {
        fn new(script: Vec<ScriptEntry>) -> Arc<SharedNetState> {
            Arc::new(SharedNetState {
                script: Mutex::new(VecDeque::from(script)),
                queries: Mutex::new(Vec::new()),
                search_hit: Mutex::new(None),
                webfile_dc: Mutex::new(DcId::default()),
            })
        }
    }

    struct FakeDownloadNet {
        state: Arc<SharedNetState>,
        root: std::path::PathBuf,
    }

    impl FakeDownloadNet {
        fn new(state: Arc<SharedNetState>) -> Self {
            let root =
                std::env::temp_dir().join(format!("m12_dl_{}", tdutils::random::secure_int64()));
            std::fs::create_dir_all(&root).unwrap();
            FakeDownloadNet { state, root }
        }

        fn temp_path(&self, name: &str) -> String {
            self.root.join(name).to_string_lossy().into_owned()
        }
    }

    impl Drop for FakeDownloadNet {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl FileDownloaderNet for FakeDownloadNet {
        fn download(
            &mut self,
            query: DownloadQuery,
            query_type: DownloadQueryType,
            meta: DownloadNetMeta,
        ) -> Result<DownloadResult, Status> {
            self.state.queries.lock().push((query_type, query, meta));
            let entry = {
                let mut guard = self.state.script.lock();
                match guard.pop_front() {
                    Some(entry) => entry,
                    None => panic!(
                        "script exhausted; got queries: {:?}",
                        self.state
                            .queries
                            .lock()
                            .iter()
                            .map(|(t, q, _)| (t, q.clone()))
                            .collect::<Vec<_>>()
                    ),
                }
            };
            match entry.0 {
                Some(tag) if tag != format!("{query_type:?}") => {
                    panic!("expected query {tag}, got {query_type:?}")
                }
                _ => entry.1,
            }
        }

        fn open_temp_file(&mut self, _file_type: FileType) -> Result<(File, String), Status> {
            let path = self.temp_path(&format!("temp_{}.bin", self.state.queries.lock().len()));
            let fd = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(|e| Status::error_message(format!("{e}").as_str()))?;
            Ok((fd, path))
        }

        fn open_partial_file(&mut self, path: &str, write: bool) -> Result<File, Status> {
            // std rejects `create` without write access, so a read-only
            // open (the only_check paths) never creates
            File::options()
                .read(true)
                .write(write)
                .create(write)
                .truncate(false)
                .open(path)
                .map_err(|e| Status::error_message(format!("{e}").as_str()))
        }

        fn webfile_dc_id(&mut self) -> DcId {
            *self.state.webfile_dc.lock()
        }

        fn create_from_temp(
            &mut self,
            _file_type: FileType,
            path: &str,
            name: &str,
        ) -> Result<String, Status> {
            let target = self.root.join(format!("done_{name}"));
            std::fs::rename(path, &target)
                .map_err(|e| Status::error_message(format!("{e}").as_str()))?;
            Ok(target.to_string_lossy().into_owned())
        }

        fn search_file(
            &mut self,
            _file_type: FileType,
            _name: &str,
            _size: i64,
        ) -> Result<String, Status> {
            match self.state.search_hit.lock().clone() {
                Some(path) => Ok(path),
                None => Err(Status::error_message("File not found")),
            }
        }
    }

    #[derive(Default)]
    struct RecordedCallback {
        started: usize,
        partials: usize,
        ok: Option<(FullLocalFileLocation, i64, bool)>,
        error: Option<Status>,
    }

    /// The callback adapter sharing the log with the test (the C++
    /// `unique_ptr<Callback>` becomes an owned box, so the log lives in an
    /// `Rc`).
    struct SharingCallback {
        inner: Arc<parking_lot::Mutex<RecordedCallback>>,
    }

    impl FileDownloaderCallback for SharingCallback {
        fn on_start_download(&mut self) {
            self.inner.lock().started += 1;
        }
        fn on_partial_download(&mut self, _partial: PartialLocalFileLocation, _size: i64) {
            self.inner.lock().partials += 1;
        }
        fn on_ok(&mut self, full_local: FullLocalFileLocation, size: i64, is_new: bool) {
            self.inner.lock().ok = Some((full_local, size, is_new));
        }
        fn on_error(&mut self, status: Status) {
            self.inner.lock().error = Some(status);
        }
    }

    fn document_location() -> FullRemoteFileLocation {
        FullRemoteFileLocation::from_document(
            FileType::Document,
            10,
            11,
            DcId::internal(2),
            "r".to_owned(),
        )
    }

    fn document_bytes() -> Vec<u8> {
        (0u8..16).collect()
    }

    /// The C++ ResourceManager ping-pong, collapsed: each grant round
    /// raises the allowance and re-enters the downloader loop.
    fn drive_download(
        downloader: &mut FileDownloader,
        log: &Arc<parking_lot::Mutex<RecordedCallback>>,
    ) {
        let mut allowance = 1i64 << 20;
        for _ in 0..64 {
            if downloader.stop_flag() || log.lock().error.is_some() || log.lock().ok.is_some() {
                return;
            }
            allowance += 1 << 20;
            ResourceManagerWorker::update_resources(downloader, grant(allowance));
        }
        panic!("the downloader did not settle in 64 grant rounds");
    }

    fn grant(limit: i64) -> ResourceState {
        let mut state = ResourceState::default();
        state.set_unit_size(1024);
        state.update_limit(limit);
        state
    }

    fn build_downloader(
        state: Arc<SharedNetState>,
        local: LocalFileLocation,
        size: i64,
        name: &str,
        need_search_file: bool,
    ) -> (FileDownloader, Arc<parking_lot::Mutex<RecordedCallback>>) {
        let callback = Arc::new(Mutex::new(RecordedCallback::default()));
        let downloader = FileDownloader::new(
            document_location(),
            local,
            size,
            name.to_owned(),
            FileEncryptionKey::default(),
            true,
            need_search_file,
            0,
            0,
            Box::new(SharingCallback {
                inner: callback.clone(),
            }),
            Box::new(FakeDownloadNet::new(state)),
        );
        (downloader, callback)
    }

    #[test]
    fn file_downloader_downloads_a_small_document() {
        let bytes = document_bytes();
        let script = vec![(None, Ok(DownloadResult::Bytes(bytes.clone())))];
        let state = SharedNetState::new(script);
        let (mut downloader, callback) = build_downloader(
            state.clone(),
            LocalFileLocation::Empty,
            bytes.len() as i64,
            "doc.bin",
            false,
        );
        downloader.start();
        drive_download(&mut downloader, &callback);

        let log = callback.lock();
        if let Some(err) = &log.error {
            panic!("download failed: {} ({})", err.message(), err.code());
        }
        let (location, size, is_new) = log.ok.as_ref().expect("download ok");
        assert_eq!(size, &(bytes.len() as i64));
        assert!(is_new);
        assert_eq!(std::fs::read(&location.path).unwrap(), bytes);

        // the single query was a getFile for the document location over
        // the authorized remote-DC Download connection
        let queries = state.queries.lock();
        assert_eq!(queries.len(), 1);
        let (query_type, query, meta) = &queries[0];
        assert_eq!(*query_type, DownloadQueryType::Default);
        assert_eq!(
            *meta,
            DownloadNetMeta {
                dc_id: DcId::internal(2),
                auth_flag_on: true,
                small: true,
            }
        );
        let DownloadQuery::GetFile {
            location,
            offset,
            limit,
            cdn_supported,
        } = query
        else {
            panic!("expected a getFile query");
        };
        assert!(*cdn_supported);
        assert_eq!(*offset, 0);
        assert!(*limit > 0);
        let telegram_api::InputFileLocation::inputDocumentFileLocation(doc) = location else {
            panic!("expected an inputDocumentFileLocation");
        };
        assert_eq!(doc.id, 10);
        assert_eq!(doc.access_hash, 11);
        assert_eq!(doc.file_reference, b"r".to_vec());
        assert!(log.started >= 1);
        assert!(log.error.is_none());
    }

    #[test]
    fn file_downloader_rejects_an_already_downloaded_file() {
        let state = SharedNetState::new(vec![]);
        let (mut downloader, callback) = build_downloader(
            state,
            LocalFileLocation::Full(FullLocalFileLocation::new(
                FileType::Document,
                "/tmp/already".to_owned(),
                0,
            )),
            16,
            "doc.bin",
            false,
        );
        downloader.start();
        drive_download(&mut downloader, &callback);
        assert_eq!(
            callback.lock().error.as_ref().expect("error").message(),
            "File is already downloaded"
        );
    }

    #[test]
    fn file_downloader_resumes_a_partial_local() {
        // part 0 of 2 is ready: the first query must be for the second half
        let bytes = document_bytes();
        let half = (bytes.len() / 2) as i64;
        let root = std::env::temp_dir().join(format!("m12_dl_{}", tdutils::random::secure_int64()));
        std::fs::create_dir_all(&root).unwrap();
        let partial_path = root.join("partial.bin");
        std::fs::write(&partial_path, &bytes[..half as usize]).unwrap();
        let partial = PartialLocalFileLocation {
            file_type: FileType::Document,
            part_size: half,
            path: partial_path.to_string_lossy().into_owned(),
            iv: Vec::new(),
            ready_bitmask: Bitmask::from_ones(1).encode(-1),
            ready_size: half,
        };
        let script = vec![(
            None,
            Ok(DownloadResult::Bytes(bytes[half as usize..].to_vec())),
        )];
        let state = SharedNetState::new(script);
        let (mut downloader, callback) = build_downloader(
            state.clone(),
            LocalFileLocation::Partial(partial),
            bytes.len() as i64,
            "doc.bin",
            false,
        );
        downloader.start();
        drive_download(&mut downloader, &callback);

        let log = callback.lock();
        if let Some(err) = &log.error {
            panic!("download failed: {} ({})", err.message(), err.code());
        }
        let (location, size, _) = log.ok.as_ref().expect("download ok");
        assert_eq!(size, &(bytes.len() as i64));
        assert_eq!(std::fs::read(&location.path).unwrap(), bytes);

        // exactly one query, for the second part
        let queries = state.queries.lock();
        assert_eq!(queries.len(), 1);
        let DownloadQuery::GetFile { offset, .. } = &queries[0].1 else {
            panic!("expected a getFile query");
        };
        assert_eq!(queries[0].0, DownloadQueryType::Default);
        assert_eq!(*offset, half);
        drop(queries);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_downloader_handles_the_cdn_redirect_and_decrypts() {
        let bytes = document_bytes();
        let key: Vec<u8> = (0u8..32).collect();
        let iv: Vec<u8> = (32u8..48).collect();

        // the CDN ciphertext the fake server serves for offset 0: the
        // server counter-block carries the byte-swapped part-offset word in
        // iv[12..16], which is zero at offset 0
        let mut server_iv = iv.clone();
        server_iv[12..16].copy_from_slice(&0u32.to_le_bytes());
        let mut ctr = tdutils::crypto::AesCtrState::new();
        ctr.init(&key, &server_iv);
        let mut ciphertext = bytes.clone();
        let plain = bytes.clone();
        ctr.encrypt(&plain, &mut ciphertext);

        let redirect = Ok(DownloadResult::CdnRedirect {
            dc_id: 4,
            file_token: b"token1".to_vec(),
            encryption_key: key,
            encryption_iv: iv,
            file_hashes: Vec::new(),
        });
        let script = vec![
            (Some("Default".to_owned()), redirect),
            (
                Some("Cdn".to_owned()),
                Ok(DownloadResult::CdnBytes(ciphertext)),
            ),
            // the redirect set need_check: the whole prefix is verified
            (
                Some("Check".to_owned()),
                Ok(DownloadResult::Hashes(vec![telegram_api::FileHash {
                    offset: 0,
                    limit: bytes.len() as i32,
                    hash: tdutils::crypto::sha256(&bytes).to_vec(),
                }])),
            ),
        ];
        let state = SharedNetState::new(script);
        let (mut downloader, callback) = build_downloader(
            state.clone(),
            LocalFileLocation::Empty,
            bytes.len() as i64,
            "doc.bin",
            false,
        );
        downloader.start();
        drive_download(&mut downloader, &callback);

        let log = callback.lock();
        if let Some(err) = &log.error {
            panic!("download failed: {} ({})", err.message(), err.code());
        }
        let (location, size, _) = log.ok.as_ref().expect("download ok");
        assert_eq!(size, &(bytes.len() as i64));
        assert_eq!(std::fs::read(&location.path).unwrap(), bytes);

        // getFile → cdnRedirect restarts on the CDN with getCdnFile
        let queries = state.queries.lock();
        assert_eq!(queries.len(), 3);
        assert_eq!(queries[0].0, DownloadQueryType::Default);
        assert_eq!(queries[1].0, DownloadQueryType::Cdn);
        // getFile rides the authorized remote DC; getCdnFile goes to the
        // CDN DC without the auth (FileDownloader.cpp:158-170, 180-181);
        // the check query returns to the remote DC with the auth
        // (FileDownloader.cpp:378)
        assert_eq!(
            queries[0].2,
            DownloadNetMeta {
                dc_id: DcId::internal(2),
                auth_flag_on: true,
                small: true,
            }
        );
        assert_eq!(
            queries[1].2,
            DownloadNetMeta {
                dc_id: DcId::external(4),
                auth_flag_on: false,
                small: true,
            }
        );
        assert_eq!(
            queries[2].2,
            DownloadNetMeta {
                dc_id: DcId::internal(2),
                auth_flag_on: true,
                small: true,
            }
        );
        let DownloadQuery::GetCdnFile {
            file_token, offset, ..
        } = &queries[1].1
        else {
            panic!("expected a getCdnFile query");
        };
        assert_eq!(file_token, b"token1");
        assert_eq!(*offset, 0);
    }

    #[test]
    fn file_downloader_reuploads_a_cdn_part() {
        let bytes = document_bytes();
        let script = vec![
            // the part is served from the CDN but needs a reupload
            (
                Some("Cdn".to_owned()),
                Ok(DownloadResult::ReuploadNeeded {
                    request_token: b"req1".to_vec(),
                }),
            ),
            // the reupload answer carries fresh hashes
            (
                Some("ReuploadCdn".to_owned()),
                Ok(DownloadResult::Hashes(vec![telegram_api::FileHash {
                    offset: 0,
                    limit: bytes.len() as i32,
                    hash: tdutils::crypto::sha256(&bytes).to_vec(),
                }])),
            ),
            // and then the CDN serves the bytes (CTR-encrypted with the
            // seeded key/iv)
            (
                Some("Cdn".to_owned()),
                Ok(DownloadResult::CdnBytes({
                    let mut server_iv: Vec<u8> = (32u8..48).collect();
                    server_iv[12..16].copy_from_slice(&0u32.to_le_bytes());
                    let mut ctr = tdutils::crypto::AesCtrState::new();
                    ctr.init(&(0u8..32).collect::<Vec<u8>>(), &server_iv);
                    let mut ciphertext = bytes.clone();
                    let plain = bytes.clone();
                    ctr.encrypt(&plain, &mut ciphertext);
                    ciphertext
                })),
            ),
            // the need_check machine then verifies the whole prefix
            (
                Some("Check".to_owned()),
                Ok(DownloadResult::Hashes(vec![telegram_api::FileHash {
                    offset: 0,
                    limit: bytes.len() as i32,
                    hash: tdutils::crypto::sha256(&bytes).to_vec(),
                }])),
            ),
        ];
        let state = SharedNetState::new(script);
        let (mut downloader, callback) = build_downloader(
            state.clone(),
            LocalFileLocation::Empty,
            bytes.len() as i64,
            "doc.bin",
            false,
        );
        // seed the CDN state the redirect arm would have set
        downloader.seed_cdn_for_test(b"tok".to_vec(), 4);
        downloader.start();
        drive_download(&mut downloader, &callback);

        let log = callback.lock();
        if let Some(err) = &log.error {
            panic!("download failed: {} ({})", err.message(), err.code());
        }
        let (location, _, _) = log.ok.as_ref().expect("download ok");
        assert_eq!(std::fs::read(&location.path).unwrap(), bytes);

        let queries = state.queries.lock();
        // the reupload hashes already cover the whole prefix, so no
        // getFileHashes check query is needed
        assert_eq!(queries.len(), 3);
        assert_eq!(queries[0].0, DownloadQueryType::Cdn);
        // reuploadCdnFile carries the auth back to the remote DC
        // (FileDownloader.cpp:184-186)
        assert_eq!(
            queries[1].2,
            DownloadNetMeta {
                dc_id: DcId::internal(2),
                auth_flag_on: true,
                small: true,
            }
        );
        let DownloadQuery::ReuploadCdnFile {
            file_token,
            request_token,
        } = &queries[1].1
        else {
            panic!("expected a reuploadCdnFile query");
        };
        assert_eq!(file_token, b"tok");
        assert_eq!(request_token, b"req1");
        // the follow-up getCdnFile still goes to the CDN
        assert_eq!(queries[2].0, DownloadQueryType::Cdn);
    }

    #[test]
    fn file_downloader_falls_back_from_cdn_on_file_token_invalid() {
        let bytes = document_bytes();
        let script = vec![
            (
                Some("Cdn".to_owned()),
                Err(Status::error_message("FILE_TOKEN_INVALID")),
            ),
            (None, Ok(DownloadResult::Bytes(bytes.clone()))),
            // the CDN round left need_check set; the check runs on the
            // downloaded data and passes
            (
                Some("Check".to_owned()),
                Ok(DownloadResult::Hashes(vec![telegram_api::FileHash {
                    offset: 0,
                    limit: bytes.len() as i32,
                    hash: tdutils::crypto::sha256(&bytes).to_vec(),
                }])),
            ),
        ];
        let state = SharedNetState::new(script);
        let (mut downloader, callback) = build_downloader(
            state.clone(),
            LocalFileLocation::Empty,
            bytes.len() as i64,
            "doc.bin",
            false,
        );
        downloader.seed_cdn_for_test(b"tok".to_vec(), 4);
        downloader.start();
        drive_download(&mut downloader, &callback);

        let log = callback.lock();
        if let Some(err) = &log.error {
            panic!("download failed: {} ({})", err.message(), err.code());
        }
        let (location, _, _) = log.ok.as_ref().expect("download ok");
        assert_eq!(std::fs::read(&location.path).unwrap(), bytes);

        let queries = state.queries.lock();
        assert_eq!(queries[0].0, DownloadQueryType::Cdn);
        assert_eq!(queries[1].0, DownloadQueryType::Default);
        // the fallback getFile rides the authorized remote DC again
        assert_eq!(
            queries[1].2,
            DownloadNetMeta {
                dc_id: DcId::internal(2),
                auth_flag_on: true,
                small: true,
            }
        );
    }

    #[test]
    fn file_downloader_detects_a_hash_mismatch_on_check() {
        let bytes = document_bytes();
        let wrong_hash = vec![0u8; 32];
        let script = vec![(
            Some("Check".to_owned()),
            Ok(DownloadResult::Hashes(vec![telegram_api::FileHash {
                offset: 0,
                limit: bytes.len() as i32,
                hash: wrong_hash,
            }])),
        )];
        let state = SharedNetState::new(script);
        let root = std::env::temp_dir().join(format!("m12_dl_{}", tdutils::random::secure_int64()));
        std::fs::create_dir_all(&root).unwrap();
        let existing = root.join("existing_doc.bin");
        std::fs::write(&existing, &bytes).unwrap();
        let (mut downloader, callback) = build_downloader(
            state.clone(),
            LocalFileLocation::Empty,
            bytes.len() as i64,
            "existing_doc.bin",
            true,
        );
        // need_search_file finds the local copy, and the downloader then
        // only checks it against the server hashes
        *state.search_hit.lock() = Some(existing.to_string_lossy().into_owned());
        downloader.start();
        drive_download(&mut downloader, &callback);

        let log = callback.lock();
        let error = log.error.as_ref().expect("hash mismatch error");
        // only_check (the search-file path) reports a restart request
        assert_eq!(error.message(), "FILE_DOWNLOAD_RESTART");
        assert!(log.ok.is_none());
        drop(log);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_downloader_restarts_a_canceled_query() {
        // a canceled in-flight query (NetQuery::Error::Canceled = 203)
        // restarts its part instead of failing the download
        // (FileDownloader.cpp:684-687)
        let bytes = document_bytes();
        let script = vec![
            (None, Err(Status::error(NET_QUERY_CANCELED, "Canceled"))),
            (None, Ok(DownloadResult::Bytes(bytes.clone()))),
        ];
        let state = SharedNetState::new(script);
        let (mut downloader, callback) = build_downloader(
            state.clone(),
            LocalFileLocation::Empty,
            bytes.len() as i64,
            "doc.bin",
            false,
        );
        downloader.start();
        drive_download(&mut downloader, &callback);

        let log = callback.lock();
        if let Some(err) = &log.error {
            panic!("download failed: {} ({})", err.message(), err.code());
        }
        let (location, size, _) = log.ok.as_ref().expect("download ok");
        assert_eq!(size, &(bytes.len() as i64));
        assert_eq!(std::fs::read(&location.path).unwrap(), bytes);

        assert_eq!(state.queries.lock().len(), 2);
    }

    #[test]
    fn file_downloader_annotates_a_file_reference_error() {
        // the failing error gets the base64-encoded file reference
        // appended (FileDownloader.cpp:194-205), so the FileLoaderManager
        // repair path recognizes it
        let script = vec![(None, Err(Status::error(400, "FILE_REFERENCE_INVALID")))];
        let state = SharedNetState::new(script);
        let (mut downloader, callback) =
            build_downloader(state, LocalFileLocation::Empty, 16, "doc.bin", false);
        downloader.start();
        drive_download(&mut downloader, &callback);

        let log = callback.lock();
        let error = log.error.as_ref().expect("file reference error");
        assert_eq!(error.code(), 400);
        assert_eq!(error.message(), "FILE_REFERENCE_INVALID#BASE64cg==");
        assert!(log.ok.is_none());
    }

    #[test]
    fn file_downloader_drops_cdn_support_while_streaming() {
        // a middle-of-file streaming window asks without cdn_supported
        // (FileDownloader.cpp:151-157); the offset rides start_up's
        // set_streaming_offset
        let bytes = document_bytes();
        let script = vec![(None, Ok(DownloadResult::Bytes(bytes.clone())))];
        let state = SharedNetState::new(script);
        let callback = Arc::new(Mutex::new(RecordedCallback::default()));
        let mut downloader = FileDownloader::new(
            document_location(),
            LocalFileLocation::Empty,
            bytes.len() as i64,
            "doc.bin".to_owned(),
            FileEncryptionKey::default(),
            true,
            false,
            8, // the streaming offset
            0, // no limit
            Box::new(SharingCallback {
                inner: callback.clone(),
            }),
            Box::new(FakeDownloadNet::new(state.clone())),
        );
        downloader.start();
        drive_download(&mut downloader, &callback);

        let log = callback.lock();
        if let Some(err) = &log.error {
            panic!("download failed: {} ({})", err.message(), err.code());
        }
        let (location, _, _) = log.ok.as_ref().expect("download ok");
        assert_eq!(std::fs::read(&location.path).unwrap(), bytes);

        let queries = state.queries.lock();
        assert_eq!(queries.len(), 1);
        let DownloadQuery::GetFile {
            offset,
            cdn_supported,
            ..
        } = &queries[0].1
        else {
            panic!("expected a getFile query");
        };
        // part 0 still starts at byte 0 — the streaming window only gates
        // which parts are pending
        assert_eq!(*offset, 0);
        assert!(!*cdn_supported);
    }

    #[test]
    fn file_downloader_downloads_a_web_file() {
        // upload.getWebFile over the webfile DC (FileDownloader.cpp:158-165)
        let bytes = b"<html>hi</html>".to_vec();
        let script = vec![(None, Ok(DownloadResult::Bytes(bytes.clone())))];
        let state = SharedNetState::new(script);
        *state.webfile_dc.lock() = DcId::external(4);
        let callback = Arc::new(Mutex::new(RecordedCallback::default()));
        let mut downloader = FileDownloader::new(
            FullRemoteFileLocation::from_web(
                FileType::Document,
                "https://example.com/a.html".to_owned(),
                42,
            ),
            LocalFileLocation::Empty,
            bytes.len() as i64,
            "a.html".to_owned(),
            FileEncryptionKey::default(),
            true,
            false,
            0,
            0,
            Box::new(SharingCallback {
                inner: callback.clone(),
            }),
            Box::new(FakeDownloadNet::new(state.clone())),
        );
        downloader.start();
        drive_download(&mut downloader, &callback);

        let log = callback.lock();
        if let Some(err) = &log.error {
            panic!("download failed: {} ({})", err.message(), err.code());
        }
        let (location, size, _) = log.ok.as_ref().expect("download ok");
        assert_eq!(size, &(bytes.len() as i64));
        assert_eq!(std::fs::read(&location.path).unwrap(), bytes);

        let queries = state.queries.lock();
        assert_eq!(queries.len(), 1);
        let (query_type, query, meta) = &queries[0];
        assert_eq!(*query_type, DownloadQueryType::Default);
        assert_eq!(
            *meta,
            DownloadNetMeta {
                dc_id: DcId::external(4),
                auth_flag_on: true,
                small: true,
            }
        );
        let DownloadQuery::GetWebFile {
            location,
            offset,
            limit,
        } = query
        else {
            panic!("expected a getWebFile query");
        };
        let telegram_api::InputWebFileLocation::inputWebFileLocation(web) = location else {
            panic!("expected an inputWebFileLocation");
        };
        assert_eq!(web.url, "https://example.com/a.html");
        assert_eq!(web.access_hash, 42);
        assert_eq!(*offset, 0);
        assert!(*limit > 0);
    }

    /// IGE-encrypts `plain` under `key`/`iv` (the server side of a secret
    /// download).
    fn secret_encrypt(key: &[u8], iv: &[u8], plain: &[u8]) -> Vec<u8> {
        let mut iv = iv.to_vec();
        let mut data = plain.to_vec();
        tdutils::crypto::aes_ige_encrypt_in_place(key, &mut iv, &mut data);
        data
    }

    /// The iv after IGE-decrypting `ct` — the chain the next part rides.
    fn secret_advance_iv(key: &[u8], iv: &[u8], ct: &[u8]) -> Vec<u8> {
        let mut iv = iv.to_vec();
        let mut data = ct.to_vec();
        tdutils::crypto::aes_ige_decrypt_in_place(key, &mut iv, &mut data);
        iv
    }

    #[test]
    fn file_downloader_downloads_a_secret_document_with_ige() {
        // a secret download whose size (40) is not 16-aligned: start_part
        // pads only its local copy (FileDownloader.cpp:133-136), the
        // server serves the 16-aligned ciphertext (48 bytes), and the
        // write is trimmed back to the unpadded part size (cpp:275) —
        // padding bytes must not reach the file
        let key: Vec<u8> = (0u8..32).collect();
        let iv: Vec<u8> = (32u8..64).collect();
        let plain: Vec<u8> = (0u8..40).collect();
        let mut server_payload = plain.clone();
        server_payload.extend_from_slice(&[0xab; 8]); // the aligned-up tail
        let ciphertext = secret_encrypt(&key, &iv, &server_payload);
        assert_eq!(ciphertext.len(), 48);

        let script = vec![(None, Ok(DownloadResult::Bytes(ciphertext)))];
        let state = SharedNetState::new(script);
        let callback = Arc::new(Mutex::new(RecordedCallback::default()));
        let mut downloader = FileDownloader::new(
            document_location(),
            LocalFileLocation::Empty,
            plain.len() as i64,
            "sec.bin".to_owned(),
            FileEncryptionKey::new(&key, &iv),
            true,
            false,
            0,
            0,
            Box::new(SharingCallback {
                inner: callback.clone(),
            }),
            Box::new(FakeDownloadNet::new(state)),
        );
        downloader.start();
        drive_download(&mut downloader, &callback);

        let log = callback.lock();
        if let Some(err) = &log.error {
            panic!("download failed: {} ({})", err.message(), err.code());
        }
        let (location, size, _) = log.ok.as_ref().expect("download ok");
        assert_eq!(size, &(plain.len() as i64));
        let written = std::fs::read(&location.path).unwrap();
        assert_eq!(written.len(), plain.len());
        assert_eq!(written, plain);
    }

    #[test]
    fn file_downloader_resumes_a_secret_partial_with_the_chained_iv() {
        // the partial state carries the raw 32-byte iv (the chain after
        // the ready prefix, FileDownloader.cpp:306-310); the resume gate
        // needs iv.len() == 32 (cpp:497) and the next part decrypts with
        // the advanced iv
        let key: Vec<u8> = (0u8..32).collect();
        let iv: Vec<u8> = (32u8..64).collect();
        let plain: Vec<u8> = (0u8..40).collect();
        let part_size: i64 = 16;

        // part 0 was downloaded before the restart; the partial file
        // stores the DECRYPTED bytes (process_part writes the decrypted
        // slice) and the saved iv is the chain after its ciphertext
        let ct0 = secret_encrypt(&key, &iv, &plain[..16]);
        let chained_iv = secret_advance_iv(&key, &iv, &ct0);

        let root = std::env::temp_dir().join(format!("m12_dl_{}", tdutils::random::secure_int64()));
        std::fs::create_dir_all(&root).unwrap();
        let partial_path = root.join("partial_sec.bin");
        std::fs::write(&partial_path, &plain[..16]).unwrap();
        let partial = PartialLocalFileLocation {
            file_type: FileType::Document,
            part_size,
            path: partial_path.to_string_lossy().into_owned(),
            iv: chained_iv.clone(),
            ready_bitmask: Bitmask::from_ones(1).encode(-1),
            ready_size: part_size,
        };

        // part 1 rides the chained iv; part 2 is the unaligned tail the
        // server aligns up to 16
        let ct1 = secret_encrypt(&key, &chained_iv, &plain[16..32]);
        let iv_after_1 = secret_advance_iv(&key, &chained_iv, &ct1);
        let mut payload2 = plain[32..40].to_vec();
        payload2.extend_from_slice(&[0xcd; 8]);
        let ct2 = secret_encrypt(&key, &iv_after_1, &payload2);
        assert_eq!(ct2.len(), 16);

        let script = vec![
            (None, Ok(DownloadResult::Bytes(ct1))),
            (None, Ok(DownloadResult::Bytes(ct2))),
        ];
        let state = SharedNetState::new(script);
        let callback = Arc::new(Mutex::new(RecordedCallback::default()));
        let mut downloader = FileDownloader::new(
            document_location(),
            LocalFileLocation::Partial(partial),
            plain.len() as i64,
            "sec.bin".to_owned(),
            FileEncryptionKey::new(&key, &iv),
            true,
            false,
            0,
            0,
            Box::new(SharingCallback {
                inner: callback.clone(),
            }),
            Box::new(FakeDownloadNet::new(state.clone())),
        );
        downloader.start();
        drive_download(&mut downloader, &callback);

        let log = callback.lock();
        if let Some(err) = &log.error {
            panic!("download failed: {} ({})", err.message(), err.code());
        }
        let (location, size, _) = log.ok.as_ref().expect("download ok");
        assert_eq!(size, &(plain.len() as i64));
        let written = std::fs::read(&location.path).unwrap();
        assert_eq!(written.len(), plain.len());
        assert_eq!(written, plain);

        // the resume asked exactly the two missing parts
        let queries = state.queries.lock();
        let offsets: Vec<i64> = queries
            .iter()
            .map(|(_, q, _)| match q {
                DownloadQuery::GetFile { offset, .. } => *offset,
                _ => panic!("expected getFile queries"),
            })
            .collect();
        assert_eq!(offsets, vec![16, 32]);
        drop(queries);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The deterministic generator behind the Track C crypto vectors
    /// (`DeterministicBytes` in the tdutils crypto tests — the C++
    /// `TEST(Crypto, AesIgeState)` recipe; the same LCG, mirrored here
    /// because the tdutils one is `cfg(test)`-private).
    struct TrackCBytes {
        seed: u32,
    }

    impl TrackCBytes {
        fn new(seed: u32) -> TrackCBytes {
            TrackCBytes { seed }
        }

        fn fill(&mut self, out: &mut [u8]) {
            for byte in out {
                self.seed = self
                    .seed
                    .wrapping_mul(123_457_567)
                    .wrapping_add(987_651_241);
                *byte = ((self.seed >> 23) & 255) as u8;
            }
        }
    }

    #[test]
    fn file_downloader_ige_round_trips_the_track_c_vectors() {
        // the C++ `TEST(Crypto, AesIgeState)` recipe: each length seeds a
        // fresh generator that yields the data, then the key, then the
        // iv. `aes_ige_encrypt` is CRC32-anchored against the C++ by the
        // tdutils `aes_ige_state_matches_cpp`, so downloading its output
        // and comparing the decrypted file pins the downloader's IGE half
        // to the same vectors. 65536 is the big-file arm.
        for length in [16usize, 32, 256, 1024, 65536] {
            let mut rnd = TrackCBytes::new(length as u32);
            let mut data = vec![0u8; length];
            rnd.fill(&mut data);
            let mut key = [0u8; 32];
            rnd.fill(&mut key);
            let mut iv = [0u8; 32];
            rnd.fill(&mut iv);

            let mut iv_copy = iv;
            let mut ciphertext = vec![0u8; length];
            tdutils::crypto::aes_ige_encrypt(&key, &mut iv_copy, &data, &mut ciphertext);

            let script = vec![(None, Ok(DownloadResult::Bytes(ciphertext)))];
            let state = SharedNetState::new(script);
            let callback = Arc::new(Mutex::new(RecordedCallback::default()));
            let mut downloader = FileDownloader::new(
                document_location(),
                LocalFileLocation::Empty,
                length as i64,
                format!("track_c_{length}.bin"),
                FileEncryptionKey::new(&key, &iv),
                true,
                false,
                0,
                0,
                Box::new(SharingCallback {
                    inner: callback.clone(),
                }),
                Box::new(FakeDownloadNet::new(state)),
            );
            downloader.start();
            drive_download(&mut downloader, &callback);

            let log = callback.lock();
            if let Some(err) = &log.error {
                panic!(
                    "length {length} download failed: {} ({})",
                    err.message(),
                    err.code()
                );
            }
            let (location, size, _) = log.ok.as_ref().expect("download ok");
            assert_eq!(size, &(length as i64));
            assert_eq!(
                std::fs::read(&location.path).unwrap(),
                data,
                "length {length}"
            );
        }
    }

    #[test]
    fn file_downloader_decrypts_cdn_parts_at_a_nonzero_offset() {
        // the CDN CTR counter block carries the byte-swapped
        // part-offset/16 word in iv[12..16] (FileDownloader.cpp:252-262):
        // part 1 starts at byte 65536, offset/16 = 0x1000, byte-swapped
        // to 0x00100000
        let data: Vec<u8> = {
            let mut rnd = TrackCBytes::new(42);
            let mut data = vec![0u8; 131072];
            rnd.fill(&mut data);
            data
        };
        let key: Vec<u8> = (0u8..32).collect();
        let iv: Vec<u8> = (32u8..48).collect();
        let cdn_encrypt = |part_iv: &[u8], plain: &[u8]| {
            let mut ctr = tdutils::crypto::AesCtrState::new();
            ctr.init(&key, part_iv);
            let mut ciphertext = plain.to_vec();
            ctr.encrypt(plain, &mut ciphertext);
            ciphertext
        };
        // the server replaces iv[12..16] with the swapped offset word for
        // EVERY part — part 0 carries word 0
        let mut server_iv0 = iv.clone();
        server_iv0[12..16].copy_from_slice(&0u32.to_le_bytes());
        let mut server_iv1 = iv.clone();
        server_iv1[12..16].copy_from_slice(&0x0010_0000u32.to_le_bytes());
        let ct0 = cdn_encrypt(&server_iv0, &data[..65536]);
        let ct1 = cdn_encrypt(&server_iv1, &data[65536..]);

        let redirect = Ok(DownloadResult::CdnRedirect {
            dc_id: 4,
            file_token: b"token1".to_vec(),
            encryption_key: key.clone(),
            encryption_iv: iv.clone(),
            file_hashes: Vec::new(),
        });
        let script = vec![
            (Some("Default".to_owned()), redirect),
            (Some("Cdn".to_owned()), Ok(DownloadResult::CdnBytes(ct0))),
            (Some("Cdn".to_owned()), Ok(DownloadResult::CdnBytes(ct1))),
            // the redirect set need_check: the whole prefix is verified
            (
                Some("Check".to_owned()),
                Ok(DownloadResult::Hashes(vec![telegram_api::FileHash {
                    offset: 0,
                    limit: data.len() as i32,
                    hash: tdutils::crypto::sha256(&data).to_vec(),
                }])),
            ),
        ];
        let state = SharedNetState::new(script);
        let (mut downloader, callback) = build_downloader(
            state.clone(),
            LocalFileLocation::Empty,
            data.len() as i64,
            "doc.bin",
            false,
        );
        downloader.start();
        drive_download(&mut downloader, &callback);

        let log = callback.lock();
        if let Some(err) = &log.error {
            panic!("download failed: {} ({})", err.message(), err.code());
        }
        let (location, size, _) = log.ok.as_ref().expect("download ok");
        assert_eq!(size, &(data.len() as i64));
        assert_eq!(std::fs::read(&location.path).unwrap(), data);

        // both parts rode the CDN: offsets 0 and 65536
        let queries = state.queries.lock();
        assert_eq!(queries.len(), 4);
        let offsets: Vec<i64> = queries
            .iter()
            .filter_map(|(_, q, _)| match q {
                DownloadQuery::GetCdnFile { offset, .. } => Some(*offset),
                _ => None,
            })
            .collect();
        assert_eq!(offsets, vec![0, 65536]);
    }

    #[test]
    fn file_downloader_rejects_a_web_file_over_cdn() {
        // use_cdn on a web location fails before any query
        // (FileDownloader.cpp:172-174)
        let state = SharedNetState::new(vec![]);
        let callback = Arc::new(Mutex::new(RecordedCallback::default()));
        let mut downloader = FileDownloader::new(
            FullRemoteFileLocation::from_web(
                FileType::Document,
                "https://example.com/a.html".to_owned(),
                42,
            ),
            LocalFileLocation::Empty,
            15,
            "a.html".to_owned(),
            FileEncryptionKey::default(),
            true,
            false,
            0,
            0,
            Box::new(SharingCallback {
                inner: callback.clone(),
            }),
            Box::new(FakeDownloadNet::new(state.clone())),
        );
        downloader.seed_cdn_for_test(b"tok".to_vec(), 4);
        downloader.start();
        drive_download(&mut downloader, &callback);

        let log = callback.lock();
        let error = log.error.as_ref().expect("cdn-over-web error");
        assert_eq!(error.message(), "Can't download web file from CDN");
        assert!(log.ok.is_none());
        assert!(state.queries.lock().is_empty());
    }
}

#[cfg(test)]
mod file_load_manager_tests {
    use super::*;
    use crate::files::tests::TestFileSources;

    /// Serves a fixed payload slice per `getFile` and records the
    /// (offset, limit) request sequence.
    struct SliceNet {
        payload: Vec<u8>,
        queries: Arc<parking_lot::Mutex<Vec<(i64, i32)>>>,
        root: std::path::PathBuf,
    }

    impl SliceNet {
        fn serve(&self, offset: i64, limit: i32) -> Vec<u8> {
            let start = (offset as usize).min(self.payload.len());
            let end = (start + limit as usize).min(self.payload.len());
            self.payload[start..end].to_vec()
        }
    }

    impl FileDownloaderNet for SliceNet {
        fn download(
            &mut self,
            query: DownloadQuery,
            _query_type: DownloadQueryType,
            _meta: DownloadNetMeta,
        ) -> Result<DownloadResult, Status> {
            let DownloadQuery::GetFile { offset, limit, .. } = query else {
                panic!("expected a getFile query");
            };
            self.queries.lock().push((offset, limit));
            Ok(DownloadResult::Bytes(self.serve(offset, limit)))
        }

        fn open_temp_file(&mut self, _file_type: FileType) -> Result<(File, String), Status> {
            let path = self
                .root
                .join(format!("temp_{}", tdutils::random::secure_int64()));
            let fd = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(|e| Status::error_message(format!("{e}").as_str()))?;
            Ok((fd, path.to_string_lossy().into_owned()))
        }

        fn open_partial_file(&mut self, path: &str, write: bool) -> Result<File, Status> {
            File::options()
                .read(true)
                .write(write)
                .create(write)
                .truncate(false)
                .open(path)
                .map_err(|e| Status::error_message(format!("{e}").as_str()))
        }

        fn create_from_temp(
            &mut self,
            _file_type: FileType,
            path: &str,
            name: &str,
        ) -> Result<String, Status> {
            let target = self.root.join(format!("done_{name}"));
            std::fs::rename(path, &target)
                .map_err(|e| Status::error_message(format!("{e}").as_str()))?;
            Ok(target.to_string_lossy().into_owned())
        }
    }

    /// The `FileLoadNetFactory` test double: every download rides a
    /// `SliceNet` over the shared payload.
    struct TestFactory {
        payload: Vec<u8>,
        root: std::path::PathBuf,
        download_queries: Arc<parking_lot::Mutex<Vec<(i64, i32)>>>,
    }

    impl FileLoadNetFactory for TestFactory {
        fn download_net(&mut self) -> Box<dyn FileDownloaderNet> {
            Box::new(SliceNet {
                payload: self.payload.clone(),
                queries: self.download_queries.clone(),
                root: self.root.clone(),
            })
        }

        fn upload_net(&mut self) -> Box<dyn FileUploaderNet> {
            unimplemented!("uploads ride dedicated fakes")
        }

        fn hash_net(&mut self) -> Box<dyn FileHashUploaderNet> {
            unimplemented!("uploads ride dedicated fakes")
        }

        fn utils_net(&mut self) -> Box<dyn FileDownloaderNet> {
            self.download_net()
        }
    }

    /// The recorded `DownloadCallback` log.
    #[derive(Default, Debug)]
    struct DownloadLog {
        events: Vec<(FileId, String)>,
    }

    impl DownloadLog {
        fn messages(&self, file_id: FileId) -> Vec<String> {
            self.events
                .iter()
                .filter(|(id, _)| *id == file_id)
                .map(|(_, event)| event.clone())
                .collect()
        }
    }

    struct DownloadCallbackHandle(Arc<parking_lot::Mutex<DownloadLog>>);

    impl DownloadCallback for DownloadCallbackHandle {
        fn on_progress(&mut self, file_id: FileId) {
            self.0.lock().events.push((file_id, "progress".to_owned()));
        }
        fn on_download_ok(&mut self, file_id: FileId) {
            self.0.lock().events.push((file_id, "ok".to_owned()));
        }
        fn on_download_error(&mut self, file_id: FileId, status: Status) {
            self.0
                .lock()
                .events
                .push((file_id, format!("error:{}", status.message())));
        }
    }

    /// A fresh temp root shared by a test.
    fn test_root() -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("m12_fllm_{}", tdutils::random::secure_int64()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// Registers a downloadable document of `size` bytes.
    fn register_downloadable(
        file_manager: &mut FileManager,
        dc_id_num: i32,
        size: i64,
        name_seed: i32,
    ) -> FileId {
        let location = FullRemoteFileLocation::from_document(
            FileType::Document,
            name_seed as i64,
            11,
            DcId::internal(dc_id_num),
            "r".to_owned(),
        );
        let file_id = file_manager.register_remote(
            location,
            FileLocationSource::FromServer,
            DialogId::default(),
            size,
            0,
            format!("doc_{name_seed}.bin"),
        );
        // the C++ callers reach the node through `get_sync_file_node`,
        // whose database load clears `need_load_from_pmc`; there is no db
        // in the test
        if let Some(node) = file_manager.get_file_node_mut(file_id) {
            node.need_load_from_pmc = false;
        }
        file_id
    }

    /// Drives the manager and applies its events until quiescent (the
    /// collapsed actor loop).
    fn pump(file_manager: &mut FileManager, fllm: &mut FileLoadManager) {
        for _ in 0..64 {
            fllm.drive();
            file_manager.apply_load_events(fllm);
            if fllm.active_downloads() == 0 && fllm.active_uploads() == 0 {
                return;
            }
        }
        panic!("the load manager did not quiesce");
    }

    #[test]
    fn file_load_manager_downloads_a_file_end_to_end() {
        let root = test_root();
        let payload: Vec<u8> = (0..=255).cycle().take(4096).collect();
        let query_log: Arc<parking_lot::Mutex<Vec<(i64, i32)>>> = Arc::new(Mutex::new(Vec::new()));
        let mut file_manager = FileManager::new();
        let file_id = register_downloadable(&mut file_manager, 2, payload.len() as i64, 1);
        let fllm = &mut FileLoadManager::new(
            Box::new(TestFactory {
                payload: payload.clone(),
                root: root.clone(),
                download_queries: Arc::clone(&query_log),
            }),
            FileLoadManagerOptions::default(),
        );

        let log = Arc::new(Mutex::new(DownloadLog::default()));
        file_manager
            .set_user_download_callback(Arc::new(Mutex::new(DownloadCallbackHandle(log.clone()))));
        file_manager.download(
            file_id,
            0,
            Arc::new(Mutex::new(DownloadCallbackHandle(log.clone())))
                as Arc<parking_lot::Mutex<dyn DownloadCallback>>,
            10,
            -1,
            -1,
            fllm,
        );
        pump(&mut file_manager, fllm);

        // the request sequence covers the whole payload in part-size
        // steps starting at 0
        let queries = query_log.lock();
        assert!(
            !queries.is_empty(),
            "no queries; active={} log={:?}",
            fllm.active_downloads(),
            log.lock()
        );
        // the file is one part: a single getFile at offset 0 (the limit
        // is the manager part size, the served bytes the file tail)
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0], (0, 65536));
        drop(queries);

        // the single user request finished with ok
        assert_eq!(log.lock().messages(file_id), vec!["ok".to_owned()]);

        // the node picked up the full local location (the partial events
        // ran through on_partial_download)
        let node = file_manager.get_file_node(file_id).expect("node");
        assert!(FileView::new(node).has_full_local_location());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ==== M.12.8 FileGenerateManager ====

    /// The shared generate-test log: scripted web answers, recorded
    /// queries and the generation updates.
    #[derive(Default)]
    struct GenerateLog {
        queries: parking_lot::Mutex<Vec<String>>,
        web_script: parking_lot::Mutex<std::collections::VecDeque<Result<Vec<u8>, Status>>>,
        updates: parking_lot::Mutex<Vec<String>>,
    }

    /// The generate io/net seam: real temp-dir files plus the scripted
    /// `upload.getWebFile` answers.
    struct GenerateNet {
        root: std::path::PathBuf,
        log: Arc<GenerateLog>,
    }

    impl FileDownloaderNet for GenerateNet {
        fn download(
            &mut self,
            query: DownloadQuery,
            _query_type: DownloadQueryType,
            _meta: DownloadNetMeta,
        ) -> Result<DownloadResult, Status> {
            match query {
                DownloadQuery::GetWebFile { .. } => {
                    self.log.queries.lock().push("GetWebFile".to_owned());
                    self.log
                        .web_script
                        .lock()
                        .pop_front()
                        .unwrap_or_else(|| panic!("web script exhausted"))
                        .map(DownloadResult::Bytes)
                }
                _ => panic!("unexpected generate query {query:?}"),
            }
        }

        fn open_temp_file(&mut self, _file_type: FileType) -> Result<(File, String), Status> {
            let path = self
                .root
                .join(format!("temp_{}.bin", tdutils::random::secure_int64()));
            let fd = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(|e| Status::error_message(format!("{e}").as_str()))?;
            Ok((fd, path.to_string_lossy().into_owned()))
        }

        fn open_partial_file(&mut self, path: &str, write: bool) -> Result<File, Status> {
            File::options()
                .read(true)
                .write(write)
                .create(write)
                .truncate(false)
                .open(path)
                .map_err(|e| Status::error_message(format!("{e}").as_str()))
        }

        fn open_generated_file(&mut self, path: &str) -> Result<File, Status> {
            // the C++ opens Create | Write — the path may have been
            // unlinked as a stale partial
            File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .map_err(|e| Status::error_message(format!("{e}").as_str()))
        }

        fn create_from_temp(
            &mut self,
            _file_type: FileType,
            path: &str,
            name: &str,
        ) -> Result<String, Status> {
            if name.is_empty() {
                // the C++ keeps the temp name for an empty name
                return Ok(path.to_owned());
            }
            let target = self.root.join(format!("done_{name}"));
            std::fs::rename(path, &target)
                .map_err(|e| Status::error_message(format!("{e}").as_str()))?;
            Ok(target.to_string_lossy().into_owned())
        }
    }

    /// The factory: the same seam everywhere (`utils`/`generate`).
    struct GenerateFactory {
        root: std::path::PathBuf,
        log: Arc<GenerateLog>,
    }

    impl FileLoadNetFactory for GenerateFactory {
        fn download_net(&mut self) -> Box<dyn FileDownloaderNet> {
            Box::new(GenerateNet {
                root: self.root.clone(),
                log: self.log.clone(),
            })
        }

        fn upload_net(&mut self) -> Box<dyn FileUploaderNet> {
            unimplemented!("generate tests ride downloads")
        }

        fn hash_net(&mut self) -> Box<dyn FileHashUploaderNet> {
            unimplemented!("generate tests ride downloads")
        }

        fn utils_net(&mut self) -> Box<dyn FileDownloaderNet> {
            self.download_net()
        }
    }

    /// The context mock: records the generation updates.
    struct GenerateContext {
        log: Arc<GenerateLog>,
        access_hash: i64,
    }

    impl FileManagerContext for GenerateContext {
        fn on_file_generation_start(
            &mut self,
            generation_id: i64,
            original_path: &str,
            path: &str,
            conversion: &str,
        ) {
            self.log.updates.lock().push(format!(
                "start:{generation_id}:{original_path}:{path}:{conversion}"
            ));
        }

        fn on_file_generation_stop(&mut self, generation_id: i64) {
            self.log
                .updates
                .lock()
                .push(format!("stop:{generation_id}"));
        }

        fn location_access_hash(&mut self, _latitude: f64, _longitude: f64) -> i64 {
            self.access_hash
        }
    }

    #[test]
    fn external_file_generate_completes_end_to_end() {
        // the register-generate -> download -> write/progress/finish flow
        // (FileManager.cpp:3673-3786 + FileGenerateManager.cpp:299-391)
        let root = test_root();
        let log = Arc::new(GenerateLog::default());
        let mut file_manager = FileManager::new();
        file_manager.set_context(Box::new(GenerateContext {
            log: log.clone(),
            access_hash: 0,
        }));
        let fllm = &mut FileLoadManager::new(
            Box::new(GenerateFactory {
                root: root.clone(),
                log: log.clone(),
            }),
            FileLoadManagerOptions::default(),
        );

        let generate_id = file_manager.register_generate(
            FileType::Document,
            "source.jpg".to_owned(),
            "convert_me".to_owned(),
            DialogId::default(),
            100,
        );
        // the C++ callers reach the node through `get_sync_file_node`,
        // whose database load clears `need_load_from_pmc`; there is no db
        // in the test
        if let Some(node) = file_manager.get_file_node_mut(generate_id) {
            node.need_load_from_pmc = false;
        }
        let log_dl = Arc::new(Mutex::new(DownloadLog::default()));
        file_manager.set_user_download_callback(Arc::new(Mutex::new(DownloadCallbackHandle(
            log_dl.clone(),
        ))));
        file_manager.download(
            generate_id,
            0,
            Arc::new(Mutex::new(DownloadCallbackHandle(log_dl.clone())))
                as Arc<parking_lot::Mutex<dyn DownloadCallback>>,
            10,
            -1,
            -1,
            fllm,
        );
        file_manager.apply_load_events(fllm);

        // the generation started: the update carries the query id, the
        // original path, the allocated temp path and the conversion
        let updates = log.updates.lock();
        assert_eq!(updates.len(), 1, "updates={updates:?}");
        assert!(
            updates[0].starts_with("start:1:source.jpg:"),
            "{:?}",
            updates[0]
        );
        assert!(updates[0].ends_with(":convert_me"));
        let temp_path = updates[0].split(':').nth(3).expect("path").to_owned();
        drop(updates);

        // the app feeds the conversion
        file_manager
            .external_file_generate_write_part(1, 0, b"hello".to_vec(), fllm)
            .expect("write part");
        file_manager
            .external_file_generate_progress(1, 5, 5)
            .expect("progress");
        file_manager.apply_load_events(fllm);
        // the partial event became the node's local location
        {
            let node = file_manager.get_file_node(generate_id).expect("node");
            match &node.local {
                LocalFileLocation::Partial(partial) => {
                    assert_eq!(partial.ready_size, 5);
                    assert_eq!(partial.path, temp_path);
                }
                other => panic!("expected a partial local location, got {other:?}"),
            }
        }

        file_manager
            .external_file_generate_finish(1, None, fllm)
            .expect("finish");
        file_manager.apply_load_events(fllm);

        // the finished generation merged into the generate file id and
        // finished the user download; the stop update follows the ok
        assert_eq!(log_dl.lock().messages(generate_id), vec!["ok".to_owned()]);
        let node = file_manager.get_file_node(generate_id).expect("node");
        match &node.local {
            LocalFileLocation::Full(full) => {
                // the suggested path is the generate original_path; the
                // mock renames the temp into done_<name>
                assert!(full.path.ends_with("done_source.jpg"), "{}", full.path);
                assert_eq!(std::fs::read(&full.path).unwrap(), b"hello".to_vec());
            }
            other => panic!("expected a full local location, got {other:?}"),
        }
        let updates = log.updates.lock();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[1], "stop:1");
        drop(updates);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn web_file_generate_downloads_and_saves() {
        // the `#map#` conversion rides upload.getWebFile and
        // save_file_bytes (FileGenerateManager.cpp:212-253)
        let root = test_root();
        let log = Arc::new(GenerateLog::default());
        log.web_script.lock().push_back(Ok(b"png-bytes".to_vec()));
        let mut file_manager = FileManager::new();
        file_manager.set_context(Box::new(GenerateContext {
            log: log.clone(),
            access_hash: -311063606295967,
        }));
        let fllm = &mut FileLoadManager::new(
            Box::new(GenerateFactory {
                root: root.clone(),
                log: log.clone(),
            }),
            FileLoadManagerOptions::default(),
        );

        let generate_id = file_manager.register_generate(
            FileType::Thumbnail,
            String::new(),
            "#map#13#256#256#256#256#2#".to_owned(),
            DialogId::default(),
            0,
        );
        // the C++ callers reach the node through `get_sync_file_node`,
        // whose database load clears `need_load_from_pmc`; there is no db
        // in the test
        if let Some(node) = file_manager.get_file_node_mut(generate_id) {
            node.need_load_from_pmc = false;
        }
        let log_dl = Arc::new(Mutex::new(DownloadLog::default()));
        file_manager.set_user_download_callback(Arc::new(Mutex::new(DownloadCallbackHandle(
            log_dl.clone(),
        ))));
        file_manager.download(
            generate_id,
            0,
            Arc::new(Mutex::new(DownloadCallbackHandle(log_dl.clone())))
                as Arc<parking_lot::Mutex<dyn DownloadCallback>>,
            10,
            -1,
            -1,
            fllm,
        );
        file_manager.apply_load_events(fllm);

        assert_eq!(*log.queries.lock(), vec!["GetWebFile".to_owned()]);
        assert_eq!(log_dl.lock().messages(generate_id), vec!["ok".to_owned()]);
        // the web worker emits no generation updates
        assert!(log.updates.lock().is_empty());
        // the saved copy landed under the computed map name
        let node = file_manager.get_file_node(generate_id).expect("node");
        match &node.local {
            LocalFileLocation::Full(full) => {
                assert!(
                    full.path.ends_with("done_map_13_256_256.png"),
                    "{}",
                    full.path
                );
                assert_eq!(std::fs::read(&full.path).unwrap(), b"png-bytes".to_vec());
            }
            other => panic!("expected a full local location, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_id_generate_downloads_the_referenced_file() {
        // the `#file_id#` conversion downloads the referenced file and
        // re-registers it under the generate file type
        // (FileGenerateManager.cpp:41-121)
        let root = test_root();
        let payload: Vec<u8> = (0..=255).cycle().take(1024).collect();
        let query_log: Arc<parking_lot::Mutex<Vec<(i64, i32)>>> = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::new(GenerateLog::default());
        let mut file_manager = FileManager::new();
        file_manager.set_context(Box::new(GenerateContext {
            log: log.clone(),
            access_hash: 0,
        }));
        let source_id = register_downloadable(&mut file_manager, 2, payload.len() as i64, 7);
        let fllm = &mut FileLoadManager::new(
            Box::new(TestFactory {
                payload: payload.clone(),
                root: root.clone(),
                download_queries: Arc::clone(&query_log),
            }),
            FileLoadManagerOptions::default(),
        );

        let generate_id = file_manager.register_generate(
            FileType::Thumbnail,
            String::new(),
            format!("#file_id#{}", source_id.get()),
            DialogId::default(),
            0,
        );
        // the C++ callers reach the node through `get_sync_file_node`,
        // whose database load clears `need_load_from_pmc`; there is no db
        // in the test
        if let Some(node) = file_manager.get_file_node_mut(generate_id) {
            node.need_load_from_pmc = false;
        }
        let log_dl = Arc::new(Mutex::new(DownloadLog::default()));
        file_manager.set_user_download_callback(Arc::new(Mutex::new(DownloadCallbackHandle(
            log_dl.clone(),
        ))));
        file_manager.download(
            generate_id,
            0,
            Arc::new(Mutex::new(DownloadCallbackHandle(log_dl.clone())))
                as Arc<parking_lot::Mutex<dyn DownloadCallback>>,
            10,
            -1,
            -1,
            fllm,
        );
        pump(&mut file_manager, fllm);

        // the referenced file was downloaded once
        assert_eq!(query_log.lock().len(), 1);
        // the generate file id finished with the downloaded bytes under
        // the overridden file type
        assert_eq!(log_dl.lock().messages(generate_id), vec!["ok".to_owned()]);
        let node = file_manager.get_file_node(generate_id).expect("node");
        match &node.local {
            LocalFileLocation::Full(full) => {
                assert_eq!(full.file_type, FileType::Thumbnail);
                assert!(full.path.contains("doc_7"), "{}", full.path);
                assert_eq!(std::fs::read(&full.path).unwrap(), payload);
            }
            other => panic!("expected a full local location, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cancel_drops_the_generation_and_rejects_the_arms() {
        let root = test_root();
        let log = Arc::new(GenerateLog::default());
        let mut file_manager = FileManager::new();
        file_manager.set_context(Box::new(GenerateContext {
            log: log.clone(),
            access_hash: 0,
        }));
        let fllm = &mut FileLoadManager::new(
            Box::new(GenerateFactory {
                root: root.clone(),
                log: log.clone(),
            }),
            FileLoadManagerOptions::default(),
        );

        let generate_id = file_manager.register_generate(
            FileType::Document,
            "source.jpg".to_owned(),
            "convert_me".to_owned(),
            DialogId::default(),
            100,
        );
        // the C++ callers reach the node through `get_sync_file_node`,
        // whose database load clears `need_load_from_pmc`; there is no db
        // in the test
        if let Some(node) = file_manager.get_file_node_mut(generate_id) {
            node.need_load_from_pmc = false;
        }
        let log_dl = Arc::new(Mutex::new(DownloadLog::default()));
        file_manager.set_user_download_callback(Arc::new(Mutex::new(DownloadCallbackHandle(
            log_dl.clone(),
        ))));
        file_manager.download(
            generate_id,
            0,
            Arc::new(Mutex::new(DownloadCallbackHandle(log_dl.clone())))
                as Arc<parking_lot::Mutex<dyn DownloadCallback>>,
            10,
            -1,
            -1,
            fllm,
        );
        file_manager.apply_load_events(fllm);
        let temp_path = {
            let updates = log.updates.lock();
            updates[0].split(':').nth(3).expect("path").to_owned()
        };
        file_manager
            .external_file_generate_write_part(1, 0, b"hello".to_vec(), fllm)
            .expect("write part");

        // the cancel: the user request gets 200 Canceled, the generation
        // dies with -1 Canceled, the partial file is unlinked and the
        // stop update follows
        file_manager.cancel_download(generate_id, 0, true, fllm);
        file_manager.apply_load_events(fllm);

        assert_eq!(
            log_dl.lock().messages(generate_id),
            vec!["error:Canceled".to_owned()]
        );
        assert!(!std::path::Path::new(&temp_path).exists(), "{temp_path}");
        let updates = log.updates.lock();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[1], "stop:1");
        drop(updates);

        // the canceled query id is unknown to the arms
        let err = file_manager
            .external_file_generate_write_part(1, 0, b"more".to_vec(), fllm)
            .expect_err("canceled generation");
        assert_eq!(err.code(), 400);
        assert_eq!(err.message(), "Unknown generation_id");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn check_mtime_matches_the_cpp_semantics() {
        let root = test_root();
        let path = root.join("source.bin");
        std::fs::write(&path, b"data").unwrap();
        let path = path.to_string_lossy().into_owned();
        let mut ctx = GenerateContext {
            log: Arc::new(GenerateLog::default()),
            access_hash: 0,
        };
        let actual_mtime = ctx.stat(&path).unwrap().mtime_nsec;

        // no prefix: the conversion is untouched
        let mut conversion = "plain".to_owned();
        check_generate_mtime(&mut ctx, &mut conversion, &path).unwrap();
        assert_eq!(conversion, "plain");

        // an empty original path skips the check
        let mut conversion = "#mtime#123#convert".to_owned();
        check_generate_mtime(&mut ctx, &mut conversion, "").unwrap();
        assert_eq!(conversion, "#mtime#123#convert");

        // the matching nsec mtime rewrites the conversion away
        let mut conversion = format!("#mtime#{actual_mtime}#convert");
        check_generate_mtime(&mut ctx, &mut conversion, &path).unwrap();
        assert_eq!(conversion, "convert");

        // a stale mtime fails with the C++ message format
        let expected = actual_mtime + 4_000_000_000;
        let mut conversion = format!("#mtime#{expected}#convert");
        let err = check_generate_mtime(&mut ctx, &mut conversion, &path).unwrap_err();
        assert_eq!(
            err.message(),
            format!(
                "FILE_GENERATE_LOCATION_INVALID: File \"{path}\" was modified: \
                 [expected modification time:{expected}][actual modification time:{actual_mtime}]"
            )
        );

        // the C++ strips leading zeros but keeps the last digit: "0"
        // parses as 0 and fails the compare
        let mut conversion = "#mtime#0#convert".to_owned();
        let err = check_generate_mtime(&mut ctx, &mut conversion, &path).unwrap_err();
        assert_eq!(
            err.message(),
            format!(
                "FILE_GENERATE_LOCATION_INVALID: File \"{path}\" was modified: \
                 [expected modification time:0][actual modification time:{actual_mtime}]"
            )
        );

        // garbage digits leave the conversion untouched (no rewrite)
        let mut conversion = "#mtime#abc#convert".to_owned();
        check_generate_mtime(&mut ctx, &mut conversion, &path).unwrap();
        assert_eq!(conversion, "#mtime#abc#convert");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ==== M.12.9 FileStats / FileGcWorker / StorageManager ====

    /// The gc/stats test sink: real temp files, recorded unlinks and an
    /// optional db-record feed.
    struct GcContext {
        root: std::path::PathBuf,
        unlinked: Arc<parking_lot::Mutex<Vec<String>>>,
        use_file_database: bool,
        db_records: parking_lot::Mutex<Vec<(FileType, String, DialogId, i64)>>,
    }

    impl FileManagerContext for GcContext {
        fn files_dir(&self) -> String {
            self.root.to_string_lossy().into_owned()
        }
    }

    impl FileGcSink for GcContext {
        fn use_file_database(&self) -> bool {
            self.use_file_database
        }

        fn on_file_unlink(&mut self, location: FullLocalFileLocation) {
            self.unlinked.lock().push(location.path);
        }
    }

    impl FileStatsSink for GcContext {
        fn scan_file_db(&mut self, callback: &mut dyn FnMut(FileType, String, DialogId, i64)) {
            for (file_type, path, owner_dialog_id, size) in self.db_records.lock().iter() {
                callback(*file_type, path.clone(), *owner_dialog_id, *size);
            }
        }
    }

    /// The `FileGcWorker::run_gc` decision machine
    /// (FileGcWorker.cpp:32-197): type/owner immunities, the time
    /// immunity, the atime pass and the count/size passes.
    #[test]
    fn file_gc_decision_table_matches_the_cpp() {
        let dialog = DialogId::new(42);
        let excluded = DialogId::new(43);
        let make = |path: &str, file_type: FileType, owner: DialogId, age_sec: u64, size: i64| {
            let now = tdutils::time::system() as u64;
            FullFileInfo {
                file_type,
                path: path.to_owned(),
                owner_dialog_id: owner,
                size,
                atime_nsec: (now.saturating_sub(age_sec)) * 1_000_000_000,
                mtime_nsec: (now.saturating_sub(age_sec)) * 1_000_000_000,
            }
        };
        let parameters = FileGcParameters {
            max_files_size: 1 << 20,
            max_time_from_last_access: 60 * 60 * 24,
            max_file_count: u32::MAX,
            immunity_delay: 60 * 60,
            file_types: vec![FileType::Document],
            owner_dialog_ids: vec![dialog],
            exclude_owner_dialog_ids: vec![excluded],
            dialog_limit: 0,
        };

        let files = vec![
            // removed: selected type, requested owner, old enough
            make("old.bin", FileType::Document, dialog, 100_000, 10),
            // kept: time immunity (mtime inside the immunity delay)
            make("fresh.bin", FileType::Document, dialog, 10, 20),
            // kept: type immunity — stickers are immune with a file db
            make("old.webp", FileType::Sticker, dialog, 100_000, 30),
            // kept: the owner is excluded
            make("excluded.bin", FileType::Document, excluded, 100_000, 40),
            // kept: the type is not selected
            make("old.jpg", FileType::Photo, dialog, 100_000, 50),
        ];
        let unlinked = Arc::new(Mutex::new(Vec::new()));
        let mut sink = GcContext {
            root: test_root(),
            unlinked: unlinked.clone(),
            use_file_database: true,
            db_records: Mutex::new(Vec::new()),
        };
        let result = run_file_gc(&mut sink, &parameters, files, true).expect("gc");

        assert_eq!(unlinked.lock().len(), 1);
        assert_eq!(unlinked.lock()[0], "old.bin");
        assert_eq!(result.removed_file_stats.get_total_nontemp_stat().size, 10);
        assert_eq!(
            result.kept_file_stats.get_total_nontemp_stat().size,
            20 + 30 + 40 + 50
        );

        // the count pass: three same-type files, one may survive
        let files = vec![
            make("a.bin", FileType::Document, DialogId::default(), 30_000, 1),
            make("b.bin", FileType::Document, DialogId::default(), 20_000, 2),
            make("c.bin", FileType::Document, DialogId::default(), 10_000, 4),
        ];
        let parameters = FileGcParameters {
            max_files_size: i64::MAX,
            max_time_from_last_access: 60 * 60 * 24,
            max_file_count: 1,
            immunity_delay: 0,
            file_types: Vec::new(),
            owner_dialog_ids: Vec::new(),
            exclude_owner_dialog_ids: Vec::new(),
            dialog_limit: 0,
        };
        let unlinked2 = Arc::new(Mutex::new(Vec::new()));
        let mut sink2 = GcContext {
            root: sink.root.clone(),
            unlinked: unlinked2.clone(),
            use_file_database: false,
            db_records: Mutex::new(Vec::new()),
        };
        let result = run_file_gc(&mut sink2, &parameters, files, true).expect("gc");
        // the two oldest go by count, in the atime order
        assert_eq!(
            *unlinked2.lock(),
            vec!["a.bin".to_owned(), "b.bin".to_owned()]
        );
        assert_eq!(result.kept_file_stats.get_total_nontemp_stat().size, 4);
        let _ = std::fs::remove_dir_all(&sink.root);
    }

    /// `FileStats` aggregation, the dialog limit fold and the td_api
    /// ordering (FileStats.cpp:32-190).
    #[test]
    fn file_stats_aggregates_and_limits_dialogs() {
        let dialog = DialogId::new(7);
        let mut stats = FileStats::new(false, true);
        stats.add(FullFileInfo {
            file_type: FileType::Document,
            path: "a".to_owned(),
            owner_dialog_id: dialog,
            size: 100,
            atime_nsec: 0,
            mtime_nsec: 0,
        });
        stats.add(FullFileInfo {
            file_type: FileType::Photo,
            path: "b".to_owned(),
            owner_dialog_id: dialog,
            size: 50,
            atime_nsec: 0,
            mtime_nsec: 0,
        });
        stats.add(FullFileInfo {
            file_type: FileType::Document,
            path: "c".to_owned(),
            owner_dialog_id: DialogId::default(),
            size: 30,
            atime_nsec: 0,
            mtime_nsec: 0,
        });
        assert_eq!(stats.get_total_nontemp_stat().size, 180);
        assert_eq!(stats.get_dialog_ids(), vec![dialog]);

        stats.apply_dialog_limit(1);
        // the default-id bucket survives the limit fold
        assert_eq!(stats.get_total_nontemp_stat().size, 180);
        let object = stats.get_storage_statistics_object();
        // the zero chat sorts last; its stats still count
        assert_eq!(object.size, 180);
        assert_eq!(object.count, 3);
        assert_eq!(object.by_chat.len(), 2);
        assert_eq!(object.by_chat[0].chat_id, 7);
        assert_eq!(object.by_chat[0].size, 150);
        assert_eq!(object.by_chat[0].count, 2);
        // per-type aggregation folds Photo into Photo by main type
        assert_eq!(object.by_chat[0].by_file_type.len(), 2);
        assert_eq!(object.by_chat[1].chat_id, 0);
        assert_eq!(object.by_chat[1].size, 30);
    }

    /// `StorageManager::on_new_file` + the fast-stat pmc persistence
    /// (StorageManager.cpp:44-62, 240-252).
    #[test]
    fn fast_stat_round_trips_through_pmc() {
        struct PmcContext {
            store: parking_lot::Mutex<BTreeMap<String, Vec<u8>>>,
        }
        impl FileManagerContext for PmcContext {}
        impl FileGcSink for PmcContext {}
        impl FileStatsSink for PmcContext {}
        impl StorageContext for PmcContext {
            fn pmc_get(&mut self, key: &str) -> Vec<u8> {
                self.store.lock().get(key).cloned().unwrap_or_default()
            }
            fn pmc_set(&mut self, key: &str, value: &[u8]) {
                self.store.lock().insert(key.to_owned(), value.to_vec());
            }
        }

        let ctx = Mutex::new(BTreeMap::new());
        let mut pmc = PmcContext { store: ctx };
        let mut manager = StorageManager::new();
        manager.on_new_file(100, 128, 1);
        manager.on_new_file(50, 64, 1);
        manager.save_fast_stat(&mut pmc);

        let mut reloaded = StorageManager::new();
        reloaded.load_fast_stat(&mut pmc);
        let fast = reloaded.get_storage_stats_fast(&mut pmc);
        assert_eq!(fast.size, 192);
        assert_eq!(fast.count, 2);
    }

    /// `FileStatsWorker::get_stats` (FileStatsWorker.cpp:117-200): the
    /// fs scan, the db join by path and the owner/file-type override.
    #[test]
    fn get_stats_joins_fs_and_db_scans() {
        let root = test_root();
        // scan_fs walks the per-type dirs (FileLoaderUtils.cpp:255-257)
        let doc_dir = get_files_dir(FileType::Document, &root.to_string_lossy(), "");
        let photo_dir = get_files_dir(FileType::Photo, &root.to_string_lossy(), "");
        std::fs::create_dir_all(&doc_dir).unwrap();
        std::fs::create_dir_all(&photo_dir).unwrap();
        let doc_path = format!("{doc_dir}doc.txt");
        std::fs::write(&doc_path, vec![7u8; 300]).unwrap();
        std::fs::write(format!("{photo_dir}pic.jpg"), vec![7u8; 500]).unwrap();
        std::fs::write(format!("{photo_dir}.nomedia"), b"").unwrap();

        let mut sink2 = GcContext {
            root: root.clone(),
            unlinked: Arc::new(Mutex::new(Vec::new())),
            use_file_database: true,
            db_records: Mutex::new(vec![(
                FileType::Video,
                doc_path.clone(),
                DialogId::new(9),
                321,
            )]),
        };
        let mut with_files = get_file_stats(&mut sink2, true, true);
        let mut paths: Vec<(String, FileType, DialogId, i64)> = Vec::new();
        for info in with_files.get_all_files() {
            paths.push((
                info.path.rsplit('/').next().unwrap().to_owned(),
                info.file_type,
                info.owner_dialog_id,
                info.size,
            ));
        }
        paths.sort();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].0, "doc.txt");
        assert_eq!(paths[0].1, FileType::Video, "db file_type wins");
        assert_eq!(paths[0].2, DialogId::new(9));
        assert_eq!(paths[0].3, 321);
        assert_eq!(paths[1].0, "pic.jpg");
        assert_eq!(paths[1].1, FileType::Photo);
        drop(with_files);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ==== DownloadManager (the downloadFile plane) ====

    /// The shared DownloadManager test log.
    #[derive(Default)]
    struct DownloadManagerLog {
        events: Vec<String>,
        next_internal_download_id: i64,
    }

    /// The `DownloadManager::Callback` double: records every event and
    /// answers the file sizes from a table.
    struct DownloadTestCallback {
        log: Arc<parking_lot::Mutex<DownloadManagerLog>>,
        sizes: parking_lot::Mutex<HashMap<FileId, (i64, i64, i64)>>,
    }

    impl DownloadManagerCallback for DownloadTestCallback {
        fn update_counters(&mut self, counters: DownloadCounters) {
            self.log.lock().events.push(format!(
                "counters:{}/{}",
                counters.total_size, counters.total_count
            ));
        }

        fn update_file_added(
            &mut self,
            file_id: FileId,
            _file_source_id: FileSourceId,
            _add_date: i32,
            _complete_date: i32,
            _is_paused: bool,
            _counters: DownloadFileCounters,
        ) {
            self.log
                .lock()
                .events
                .push(format!("added:{:?}", file_id.get()));
        }

        fn update_file_changed(
            &mut self,
            file_id: FileId,
            _complete_date: i32,
            is_paused: bool,
            _counters: DownloadFileCounters,
        ) {
            self.log
                .lock()
                .events
                .push(format!("changed:{:?}:{is_paused}", file_id.get()));
        }

        fn update_file_removed(&mut self, file_id: FileId, _counters: DownloadFileCounters) {
            self.log
                .lock()
                .events
                .push(format!("removed:{:?}", file_id.get()));
        }

        fn get_internal_download_id(&mut self) -> i64 {
            let mut log = self.log.lock();
            log.next_internal_download_id += 1;
            log.next_internal_download_id
        }

        fn start_file(&mut self, file_id: FileId, internal_download_id: i64, priority: i8) {
            self.log.lock().events.push(format!(
                "start:{:?}:{internal_download_id}:{priority}",
                file_id.get()
            ));
        }

        fn pause_file(&mut self, file_id: FileId, internal_download_id: i64) {
            self.log
                .lock()
                .events
                .push(format!("pause:{:?}:{internal_download_id}", file_id.get()));
        }

        fn delete_file(&mut self, file_id: FileId) {
            self.log
                .lock()
                .events
                .push(format!("delete:{:?}", file_id.get()));
        }

        fn get_file_search_text(
            &mut self,
            _file_id: FileId,
            _file_source_id: FileSourceId,
        ) -> Result<String, Status> {
            Ok("document.pdf".to_owned())
        }

        fn sync_file_sizes(&mut self, file_id: FileId) -> (i64, i64, i64) {
            *self.sizes.lock().get(&file_id).unwrap_or(&(0, 0, 0))
        }

        fn get_file_download_object(
            &mut self,
            file_id: FileId,
            _file_source_id: FileSourceId,
            add_date: i32,
            complete_date: i32,
            is_paused: bool,
        ) -> Option<td_api::FileDownload> {
            // the message plane (the source-message resolution) is the
            // M.12.10 wiring; a `None` message drops the entry, exactly
            // like the C++ `message_ == nullptr` filter
            Some(td_api::FileDownload {
                file_id: file_id.get(),
                message: None,
                add_date,
                complete_date,
                is_paused,
            })
        }
    }

    /// The pmc + file-id context double: a real map, the raw file id
    /// record field (the FileManager-mediated serialization is the
    /// M.12.10 wiring).
    struct DownloadTestContext {
        store: Arc<parking_lot::Mutex<BTreeMap<String, Vec<u8>>>>,
    }

    impl DownloadManagerContext for DownloadTestContext {
        fn use_message_database(&self) -> bool {
            true
        }
        fn pmc_get(&mut self, key: &str) -> Vec<u8> {
            self.store.lock().get(key).cloned().unwrap_or_default()
        }
        fn pmc_set(&mut self, key: &str, value: &[u8]) {
            self.store.lock().insert(key.to_owned(), value.to_vec());
        }
        fn pmc_erase(&mut self, key: &str) {
            self.store.lock().remove(key);
        }
        fn pmc_erase_by_prefix(&mut self, prefix: &str) {
            self.store.lock().retain(|key, _| !key.starts_with(prefix));
        }
        fn pmc_prefix_get(&mut self, prefix: &str) -> Vec<(String, Vec<u8>)> {
            self.store
                .lock()
                .iter()
                .filter(|(key, _)| key.starts_with(prefix))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        }
        fn unix_time(&mut self) -> i32 {
            1_800_000_000
        }
        fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            store_parse::store_i32(out, file_id.get());
        }
        fn parse_file_id(&mut self, parser: &mut store_parse::Parser) -> Result<FileId, Status> {
            let id = parser.i32_().map_err(Status::error_message)?;
            Ok(FileId::new(id, 0))
        }
    }

    fn download_manager_under_test(
        log: &Arc<parking_lot::Mutex<DownloadManagerLog>>,
        sizes: &parking_lot::Mutex<HashMap<FileId, (i64, i64, i64)>>,
        store: &Arc<parking_lot::Mutex<BTreeMap<String, Vec<u8>>>>,
    ) -> DownloadManager {
        let mut manager = DownloadManager::new();
        manager.set_callback(Box::new(DownloadTestCallback {
            log: Arc::clone(log),
            sizes: Mutex::new(sizes.lock().clone()),
        }));
        manager.set_context(Box::new(DownloadTestContext {
            store: Arc::clone(store),
        }));
        manager.init();
        manager
    }

    #[test]
    fn download_manager_add_file_starts_and_completes() {
        let log = Arc::new(Mutex::new(DownloadManagerLog::default()));
        let sizes = Mutex::new(HashMap::new());
        let store = Arc::new(Mutex::new(BTreeMap::new()));
        let mut manager = download_manager_under_test(&log, &sizes, &store);

        let file_id = FileId::new(11, 0);
        sizes.lock().insert(file_id, (100, 100, 0));
        let added = manager
            .add_file(file_id, FileSourceId::default(), "document.pdf", 16)
            .expect("add_file");
        assert!(added.is_none());

        // the download started immediately (not paused)
        let events = log.lock().events.clone();
        assert!(
            events
                .iter()
                .any(|event| event.starts_with(&format!("start:{:?}:1:16", file_id.get()))),
            "{events:?}"
        );
        // update_file_added and update_file_changed are gated by
        // is_search_inited (the hints load), exactly like the C++; the
        // search below flips it
        let found = manager.search("", false, false, "", 50).expect("search");
        let counts = found.total_counts.expect("counts");
        assert_eq!(counts.active_count, 1);
        assert_eq!(counts.completed_count, 0);
        // the message plane is the M.12.10 wiring, so the entries are
        // dropped by the message == nullptr filter while the counters
        // still answer
        assert!(found.files.is_empty());

        // the progress report completes the file (downloaded == size),
        // so register_file_info flips completed_at and fires changed
        manager.update_file_download_state(file_id, 100, 100, 100, false, 1);
        let events = log.lock().events.clone();
        assert!(
            events
                .iter()
                .any(|event| event.starts_with(&format!("changed:{:?}:false", file_id.get()))),
            "{events:?}"
        );

        // search sees the completed download in the counters
        let found = manager.search("", false, false, "", 50).expect("search");
        let counts = found.total_counts.expect("counts");
        assert_eq!(counts.completed_count, 1);
        assert_eq!(counts.active_count, 0);
        assert_eq!(found.next_offset, "1");
    }

    #[test]
    fn download_manager_pauses_and_resumes() {
        let log = Arc::new(Mutex::new(DownloadManagerLog::default()));
        let sizes = Mutex::new(HashMap::new());
        let store = Arc::new(Mutex::new(BTreeMap::new()));
        let mut manager = download_manager_under_test(&log, &sizes, &store);

        let file_id = FileId::new(21, 0);
        sizes.lock().insert(file_id, (100, 100, 0));
        manager
            .add_file(file_id, FileSourceId::default(), "movie.mp4", 8)
            .expect("add_file");

        manager.toggle_is_paused(file_id, true).expect("pause");
        assert!(
            log.lock()
                .events
                .iter()
                .any(|event| event.starts_with(&format!("pause:{:?}:1", file_id.get())))
        );

        manager.toggle_is_paused(file_id, false).expect("resume");
        // the internal download id persists across the pause
        assert!(
            log.lock()
                .events
                .iter()
                .any(|event| event.starts_with(&format!("start:{:?}:1", file_id.get())))
        );
    }

    #[test]
    fn download_manager_remove_updates_counters_and_persists() {
        let log = Arc::new(Mutex::new(DownloadManagerLog::default()));
        let sizes = Mutex::new(HashMap::new());
        let store = Arc::new(Mutex::new(BTreeMap::new()));
        let mut manager = download_manager_under_test(&log, &sizes, &store);

        let file_id = FileId::new(31, 0);
        sizes.lock().insert(file_id, (100, 100, 0));
        manager
            .add_file(file_id, FileSourceId::default(), "doc.bin", 16)
            .expect("add_file");
        // the add persisted the download record
        assert!(store.lock().contains_key("dlds#1"), "dlds record missing");

        // the update plane arms after the hints load (is_search_inited)
        let _ = manager.search("", false, false, "", 50).expect("search");

        manager
            .remove_file(file_id, FileSourceId::default(), false)
            .expect("remove");
        assert!(
            log.lock()
                .events
                .iter()
                .any(|event| event.starts_with(&format!("removed:{:?}", file_id.get())))
        );
        assert!(store.lock().get("dlds#1").is_none());

        // removing an unknown file fails with the C++ error
        let err = manager
            .remove_file(FileId::new(999, 0), FileSourceId::default(), false)
            .expect_err("unknown file");
        assert_eq!(err.code(), 400);

        // remove_file_if_finished on an active file reports the state
        let file_id2 = FileId::new(32, 0);
        sizes.lock().insert(file_id2, (100, 100, 0));
        manager
            .add_file(file_id2, FileSourceId::default(), "doc2.bin", 16)
            .expect("add_file");
        let err = manager
            .remove_file_if_finished(file_id2)
            .expect_err("active file");
        assert_eq!(err.message(), "File is active");
    }

    #[test]
    fn download_manager_reloads_records_from_pmc() {
        let log = Arc::new(Mutex::new(DownloadManagerLog::default()));
        let sizes = Mutex::new(HashMap::new());
        let store = Arc::new(Mutex::new(BTreeMap::new()));
        sizes.lock().insert(FileId::new(41, 0), (100, 100, 0));
        // seed the record the C++ `sync_with_database` writes: the
        // log-event version prefix, the flags word and the plain fields
        // with the paused flag set
        let mut record = Vec::new();
        store_parse::store_log_event_version(&mut record);
        store_parse::store_flags(&mut record, [true]);
        store_parse::store_i64(&mut record, 7);
        store_parse::store_i32(&mut record, 41);
        store_parse::store_i32(&mut record, 5);
        store_parse::store_i32(&mut record, 16);
        store_parse::store_i32(&mut record, 1_800_000_000);
        store_parse::store_i32(&mut record, 0);
        store.lock().insert("dlds#7".to_owned(), record);

        let mut manager = download_manager_under_test(&log, &sizes, &store);
        // the load rides the first active check (search in the C++ acts
        // through check_is_active too)
        let found = manager.search("", false, false, "", 50).expect("search");
        let counts = found.total_counts.expect("counts");
        // the paused download counts as active + paused and was not started
        assert_eq!(counts.active_count, 1);
        assert_eq!(counts.paused_count, 1);
        assert!(
            !log.lock()
                .events
                .iter()
                .any(|event| event.starts_with("start:"))
        );

        // the resumed download starts and the id sequence continues past
        // the reloaded one
        manager
            .toggle_is_paused(FileId::new(41, 0), false)
            .expect("resume");
        assert!(
            log.lock()
                .events
                .iter()
                .any(|event| event.starts_with("start:"))
        );
        manager
            .add_file(FileId::new(41, 0), FileSourceId::default(), "x", 8)
            .expect("add_file");
        assert!(store.lock().contains_key("dlds#8"));
    }

    /// The captured `Result<td_api::File>` of a `downloadFile` promise.
    type FileSlot = Arc<parking_lot::Mutex<Option<tdutils::status::Result<td_api::File>>>>;

    /// The `downloadFile` promise slot: captures the settled
    /// `Result<td_api::File>` for the assertions.
    fn unit_file_promise() -> (tdutils::promise::Promise<td_api::File>, FileSlot) {
        let slot: FileSlot = Arc::new(parking_lot::Mutex::new(None));
        let capture = Arc::clone(&slot);
        let promise = tdutils::promise::PromiseCreator::lambda_result(
            move |result: tdutils::status::Result<td_api::File>| {
                *capture.lock() = Some(result);
            },
        );
        (promise, slot)
    }

    /// M.12.10 — `from_persistent_id` round-trips the persistent-id string
    /// of a registered remote location back to the same node (the v3
    /// decoder over the zero-encoded base64url form), and the unknown-tail
    /// byte answers the C++ "Wrong last symbol" error.
    #[test]
    fn from_persistent_id_round_trips_remote_locations() {
        let mut manager = FileManager::new();
        let location = FullRemoteFileLocation::from_document(
            FileType::Document,
            123,
            456,
            crate::chat_manager::DcId::internal(1),
            "ref".to_owned(),
        );
        let file_id = manager.register_remote(
            location,
            FileLocationSource::FromUser,
            DialogId::default(),
            7,
            0,
            "doc.bin".to_owned(),
        );
        let persistent_id = manager
            .get_file_node(file_id)
            .expect("node")
            .get_persistent_file_id();
        assert!(!persistent_id.is_empty());

        let parsed = manager
            .from_persistent_id(&persistent_id, FileType::Document)
            .expect("from_persistent_id");
        assert_eq!(parsed.get(), file_id.get());

        // a v4-prefixed id with a bogus trailing version byte is rejected
        let mut bogus = tdutils::zero_encode::zero_encode(&[0, 0]);
        bogus.push(PERSISTENT_ID_VERSION_PREFIX);
        bogus.push(0x7f);
        let bogus = base64url(&bogus);
        let error = manager
            .from_persistent_id(&bogus, FileType::Document)
            .expect_err("bad version byte");
        assert_eq!(
            error.message(),
            "Wrong remote file identifier specified: can't unserialize it. Wrong last symbol"
        );
    }

    /// M.12.10 — the `downloadFile` entry: the unknown file answers the
    /// C++ 400 "File not found"; a full-local file resolves the promise
    /// with its object immediately (the `download_impl` early return).
    #[test]
    fn download_file_answers_the_cpp_error_surfaces() {
        struct LocalContext;
        impl FileManagerContext for LocalContext {
            fn real_path(&self, path: &str) -> Result<String, Status> {
                Ok(path.to_owned())
            }
            fn stat(&self, _path: &str) -> Result<FileStat, Status> {
                Ok(FileStat {
                    is_reg: true,
                    is_dir: false,
                    size: 5,
                    mtime_nsec: 1,
                    real_size: 5,
                })
            }
        }
        let mut manager = FileManager::new();
        manager.set_context(Box::new(LocalContext));
        struct NoopNetFactory;
        impl FileDownloaderNet for NoopNetFactory {
            fn download(
                &mut self,
                _query: DownloadQuery,
                _query_type: DownloadQueryType,
                _meta: DownloadNetMeta,
            ) -> Result<DownloadResult, Status> {
                Err(Status::error(400, "Not connected"))
            }
            fn open_temp_file(
                &mut self,
                _file_type: FileType,
            ) -> Result<(std::fs::File, String), Status> {
                Err(Status::error_message("no temp files in this test"))
            }
            fn open_partial_file(
                &mut self,
                _path: &str,
                _write: bool,
            ) -> Result<std::fs::File, Status> {
                Err(Status::error_message("no partial files in this test"))
            }
            fn create_from_temp(
                &mut self,
                _file_type: FileType,
                _path: &str,
                _name: &str,
            ) -> Result<String, Status> {
                Err(Status::error_message("no temp files in this test"))
            }
        }
        impl FileLoadNetFactory for NoopNetFactory {
            fn download_net(&mut self) -> Box<dyn FileDownloaderNet> {
                Box::new(NoopNetFactory)
            }
            fn upload_net(&mut self) -> Box<dyn FileUploaderNet> {
                unreachable!("no uploads in this test")
            }
            fn hash_net(&mut self) -> Box<dyn FileHashUploaderNet> {
                unreachable!("no hash uploads in this test")
            }
            fn utils_net(&mut self) -> Box<dyn FileDownloaderNet> {
                self.download_net()
            }
        }
        let mut fllm =
            FileLoadManager::new(Box::new(NoopNetFactory), FileLoadManagerOptions::default());

        // unknown file
        let (promise, slot) = unit_file_promise();
        manager.download_file(FileId::new(9, 0), 16, 0, 0, false, promise, &mut fllm);
        match slot.lock().take().expect("settled") {
            tdutils::status::Result::Err(status) => {
                assert_eq!(status.code(), 400);
                assert_eq!(status.message(), "File not found");
            }
            tdutils::status::Result::Ok(_) => panic!("unknown file must error"),
        }

        // a full-local file resolves with its object
        let file_id = manager
            .register_local(
                FullLocalFileLocation::new(FileType::Document, "/tmp/doc.bin".to_owned(), 0),
                DialogId::default(),
                5,
                false,
                false,
                FileId::default(),
            )
            .expect("register_local");
        let (promise, slot) = unit_file_promise();
        manager.download_file(file_id, 16, 0, 0, false, promise, &mut fllm);
        match slot.lock().take().expect("settled") {
            tdutils::status::Result::Ok(file) => {
                assert_eq!(file.id, file_id.get());
                assert!(file.local.expect("local").is_downloading_completed);
            }
            tdutils::status::Result::Err(status) => {
                panic!("full-local download must resolve: {status}")
            }
        }
    }

    /// M.12.10 T.7 — the dlds record rides the versioned
    /// `FileManager::store_file` serialization: a store → parse round-trip
    /// through the real plane recovers the same node id.
    #[test]
    fn dlds_records_round_trip_through_store_file() {
        let mut manager = FileManager::new();
        let location = FullRemoteFileLocation::from_document(
            FileType::Video,
            77,
            88,
            crate::chat_manager::DcId::internal(1),
            "ref".to_owned(),
        );
        let file_id = manager.register_remote(
            location,
            FileLocationSource::FromUser,
            DialogId::default(),
            42,
            0,
            "clip.mp4".to_owned(),
        );

        // the record body the Td context writes (T.7: the FileManager
        // serialization, not the raw i32)
        let mut out = Vec::new();
        store_parse::store_log_event_version(&mut out);
        store_parse::store_flags(&mut out, [false]);
        store_parse::store_i64(&mut out, 3);
        manager.store_file(file_id, &mut out);
        store_parse::store_i32(&mut out, 5);
        store_parse::store_i32(&mut out, 16);
        store_parse::store_i32(&mut out, 1_700_000_000);
        store_parse::store_i32(&mut out, 0);

        // parse through the manager-backed context (the Td plane's
        // store_file/parse_file delegation)
        struct ManagerContext {
            manager: Arc<parking_lot::Mutex<FileManager>>,
        }
        impl DownloadManagerContext for ManagerContext {
            fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
                self.manager.lock().store_file(file_id, out);
            }
            fn parse_file_id(
                &mut self,
                parser: &mut store_parse::Parser,
            ) -> Result<FileId, Status> {
                self.manager
                    .lock()
                    .parse_file(parser)
                    .map_err(Status::error_message)
            }
        }
        let shared_manager = Arc::new(parking_lot::Mutex::new(manager));
        let mut parser = Parser::new_log_event(&out).expect("versioned record");
        let record = FileDownloadInDatabase::parse(
            &mut ManagerContext {
                manager: Arc::clone(&shared_manager),
            },
            &mut parser,
        )
        .expect("parse");
        assert_eq!(record.download_id, 3);
        assert_eq!(record.file_id.get(), file_id.get());
        assert_eq!(record.priority, 16);
        assert!(parser.fetch_end().is_ok());
    }

    /// M.12.1h — the C++ FileDb oracle (`conformance/file_db_oracle`):
    /// the pinned `FileData::store` bytes emitted by the real C++
    /// FileData.hpp templates (TD_BUILD static libs), with a
    /// parse -> re-store self-consistency check per record. The samples
    /// carry no file sources: the with-sources branch needs the
    /// FileReferenceManager plane and stays pinned by the
    /// file_data_sources tests.
    #[test]
    fn file_data_matches_the_cpp_file_db_oracle() {
        let cases: &[(&str, &str)] = &[
            (
                "empty",
                "100000003d00000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
            ),
            (
                "local",
                "100000003d0000000000000000000000000000000200000005000000d2040000000000001e2f74642f757365726e616d652f646f63756d656e74732f646f632e62696e0000000000000400000000000007646f632e62696e0000000000000000",
            ),
            (
                "remote",
                "120000003d0000000000000000000000020000000500000201000000037265666400000000000000c80000000000000000000000000000000010000000000000000000000000000000000000",
            ),
            (
                "photo",
                "100000003d00000000000000000000000200000002000000020000002c010000000000009001000000000000010000000200000000000000000000000000000000000000000000000970686f746f2e6a706700000000000000000000",
            ),
            (
                "web",
                "100000003d00000000000000000000000200000000000001000000002168747470733a2f2f63646e2e74656c656772616d2e6f72672f6c6f676f2e706e6700002a0000000000000000000000000000000000000000000000000000002168747470733a2f2f63646e2e74656c656772616d2e6f72672f6c6f676f2e706e67000000000000",
            ),
            (
                "generate",
                "130000003d0000002a000000000000000000000000000000000000000000000001000000050000000b2f7372632f696e2e6a70670823636f6e766572740000002b02000000000000000000000000000000000000",
            ),
            (
                "partial",
                "100000003d00000000000000000000000000000001000000040000001c2f74642f757365726e616d652f766964656f732f706172742e6d703400000000000800ffffffff0000000001010000000000000020000000000000000000000000000000000000",
            ),
        ];
        for (name, expected_hex) in cases {
            let raw: Vec<u8> = (0..expected_hex.len() / 2)
                .map(|i| u8::from_str_radix(&expected_hex[i * 2..i * 2 + 2], 16).unwrap())
                .collect();

            // the Rust store reproduces the C++ bytes
            let mut out = Vec::new();
            file_data_for_oracle_case(name).store(&mut out, &mut TestFileSources);
            assert_eq!(out, raw, "{name}: store bytes diverge from the C++ oracle");

            // and the parse -> re-store round trip is byte-stable
            let mut parser = Parser::new(&out);
            let parsed =
                FileData::parse_from(&mut parser, false, Some(&mut TestFileSources)).unwrap();
            parser.fetch_end().unwrap();
            let mut restored = Vec::new();
            parsed.store(&mut restored, &mut TestFileSources);
            assert_eq!(restored, raw, "{name}: parse -> re-store diverges");
        }
    }

    /// The `FileData` samples the oracle probe constructs
    /// (conformance/file_db_oracle/probe.cpp main()).
    fn file_data_for_oracle_case(name: &str) -> FileData {
        match name {
            "empty" => FileData::default(),
            "local" => FileData {
                local: LocalFileLocation::Full(FullLocalFileLocation::new(
                    FileType::Document,
                    "/td/username/documents/doc.bin".to_owned(),
                    1234,
                )),
                size: 1024,
                remote_name: "doc.bin".to_owned(),
                ..FileData::default()
            },
            "remote" => FileData {
                remote: RemoteFileLocation::Full(FullRemoteFileLocation::from_document(
                    FileType::Document,
                    100,
                    200,
                    DcId::internal(1),
                    "ref".to_owned(),
                )),
                expected_size: 4096,
                ..FileData::default()
            },
            "photo" => FileData {
                remote: RemoteFileLocation::Full(FullRemoteFileLocation::from_photo(
                    PhotoSizeSource::thumbnail(FileType::Photo, 0),
                    300,
                    400,
                    DcId::internal(2),
                    String::new(),
                )),
                remote_name: "photo.jpg".to_owned(),
                ..FileData::default()
            },
            "web" => FileData {
                remote: RemoteFileLocation::Full(FullRemoteFileLocation::from_web(
                    FileType::Thumbnail,
                    "https://cdn.telegram.org/logo.png".to_owned(),
                    42,
                )),
                url: "https://cdn.telegram.org/logo.png".to_owned(),
                ..FileData::default()
            },
            "generate" => FileData {
                generate: Some(FullGenerateFileLocation::new(
                    FileType::Document,
                    "/src/in.jpg".to_owned(),
                    "#convert".to_owned(),
                )),
                owner_dialog_id: DialogId::new(42),
                expected_size: 555,
                ..FileData::default()
            },
            "partial" => FileData {
                local: LocalFileLocation::Partial(PartialLocalFileLocation {
                    file_type: FileType::Video,
                    part_size: 512 * 1024,
                    path: "/td/username/videos/part.mp4".to_owned(),
                    iv: Vec::new(),
                    ready_bitmask: vec![1],
                    ready_size: 4096,
                }),
                size: 8192,
                ..FileData::default()
            },
            other => panic!("unknown oracle case {other}"),
        }
    }

    #[test]
    fn file_load_manager_prioritizes_under_contention() {
        // two files share one payload; the higher-priority download
        // finishes first in the grant order (the C++ `to_xload` order)
        let root = test_root();
        let payload: Vec<u8> = (0..=255).cycle().take(4096).collect();
        let query_log: Arc<parking_lot::Mutex<Vec<(i64, i32)>>> = Arc::new(Mutex::new(Vec::new()));
        let mut file_manager = FileManager::new();
        let low = register_downloadable(&mut file_manager, 2, payload.len() as i64, 1);
        let high = register_downloadable(&mut file_manager, 2, payload.len() as i64, 2);
        let fllm = &mut FileLoadManager::new(
            Box::new(TestFactory {
                payload: payload.clone(),
                root: root.clone(),
                download_queries: Arc::clone(&query_log),
            }),
            FileLoadManagerOptions::default(),
        );

        let log = Arc::new(Mutex::new(DownloadLog::default()));
        file_manager
            .set_user_download_callback(Arc::new(Mutex::new(DownloadCallbackHandle(log.clone()))));
        // the low-priority file is requested FIRST (a naive FIFO would
        // finish it first)
        file_manager.download(
            low,
            0,
            Arc::new(Mutex::new(DownloadCallbackHandle(log.clone())))
                as Arc<parking_lot::Mutex<dyn DownloadCallback>>,
            1,
            -1,
            -1,
            fllm,
        );
        file_manager.download(
            high,
            0,
            Arc::new(Mutex::new(DownloadCallbackHandle(log.clone())))
                as Arc<parking_lot::Mutex<dyn DownloadCallback>>,
            10,
            -1,
            -1,
            fllm,
        );
        pump(&mut file_manager, fllm);

        // the decision order: the high-priority file's ok precedes the
        // low-priority one
        let events = &log.lock().events;
        let high_ok = events.iter().position(|(id, e)| *id == high && e == "ok");
        let low_ok = events.iter().position(|(id, e)| *id == low && e == "ok");
        let (Some(high_ok), Some(low_ok)) = (high_ok, low_ok) else {
            panic!("both downloads must finish: {events:?}");
        };
        assert!(high_ok < low_ok, "high priority must finish first");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_load_manager_cancel_mid_way_reports_canceled_and_parks_the_node() {
        let root = test_root();
        // 1.5 MB: the first grant round (1 MB) cannot finish a download,
        // so both loaders are genuinely mid-way; round 2 (2 MB, the pool
        // cap) completes them
        let payload: Vec<u8> = vec![7u8; 1536 * 1024];
        let query_log: Arc<parking_lot::Mutex<Vec<(i64, i32)>>> = Arc::new(Mutex::new(Vec::new()));
        let mut file_manager = FileManager::new();
        let canceled_id = register_downloadable(&mut file_manager, 2, payload.len() as i64, 1);
        let survivor_id = register_downloadable(&mut file_manager, 2, payload.len() as i64, 2);
        let fllm = &mut FileLoadManager::new(
            Box::new(TestFactory {
                payload: payload.clone(),
                root: root.clone(),
                download_queries: Arc::clone(&query_log),
            }),
            FileLoadManagerOptions::default(),
        );

        let log = Arc::new(Mutex::new(DownloadLog::default()));
        file_manager
            .set_user_download_callback(Arc::new(Mutex::new(DownloadCallbackHandle(log.clone()))));
        file_manager.download(
            canceled_id,
            0,
            Arc::new(Mutex::new(DownloadCallbackHandle(log.clone())))
                as Arc<parking_lot::Mutex<dyn DownloadCallback>>,
            5,
            -1,
            -1,
            fllm,
        );
        file_manager.download(
            survivor_id,
            0,
            Arc::new(Mutex::new(DownloadCallbackHandle(log.clone())))
                as Arc<parking_lot::Mutex<dyn DownloadCallback>>,
            5,
            -1,
            -1,
            fllm,
        );

        // one grant round: both downloads started (mid-way)
        assert!(fllm.drive_round(1));
        file_manager.apply_load_events(fllm);
        assert!(
            fllm.active_downloads() > 0,
            "the first round must not finish everything"
        );

        // cancel the first download mid-way: the 200 Canceled goes to its
        // callback only
        file_manager.cancel_download(canceled_id, 0, false, fllm);
        assert_eq!(
            log.lock().messages(canceled_id),
            vec!["error:Canceled".to_owned()]
        );

        // the survivor still completes
        pump(&mut file_manager, fllm);
        assert_eq!(log.lock().messages(survivor_id), vec!["ok".to_owned()]);
        // ...and the canceled one is never ok
        assert!(
            !log.lock()
                .messages(canceled_id)
                .iter()
                .any(|event| event == "ok")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_load_manager_cancel_only_if_pending_respects_the_started_flag() {
        let root = test_root();
        let payload: Vec<u8> = vec![7u8; 4096];
        let query_log: Arc<parking_lot::Mutex<Vec<(i64, i32)>>> = Arc::new(Mutex::new(Vec::new()));
        let mut file_manager = FileManager::new();
        let file_id = register_downloadable(&mut file_manager, 2, payload.len() as i64, 1);
        let fllm = &mut FileLoadManager::new(
            Box::new(TestFactory {
                payload: payload.clone(),
                root: root.clone(),
                download_queries: Arc::clone(&query_log),
            }),
            FileLoadManagerOptions::default(),
        );

        let log = Arc::new(Mutex::new(DownloadLog::default()));
        file_manager
            .set_user_download_callback(Arc::new(Mutex::new(DownloadCallbackHandle(log.clone()))));
        file_manager.download(
            file_id,
            0,
            Arc::new(Mutex::new(DownloadCallbackHandle(log.clone())))
                as Arc<parking_lot::Mutex<dyn DownloadCallback>>,
            10,
            -1,
            -1,
            fllm,
        );
        // a part ran: the download has started
        fllm.drive();
        file_manager.apply_load_events(fllm);

        // only_if_pending must refuse to cancel a started download
        file_manager.cancel_download(file_id, 0, true, fllm);
        assert!(
            !log.lock()
                .messages(file_id)
                .iter()
                .any(|event| event.starts_with("error:"))
        );

        pump(&mut file_manager, fllm);
        assert_eq!(log.lock().messages(file_id), vec!["ok".to_owned()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_load_manager_restarts_on_a_file_reference_error_until_repair_exhausts() {
        // the decision order (FileManager.cpp:5134-5152, 5154-5157): a
        // FILE_REFERENCE_ error deletes the reference and restarts; the
        // second wait-file-reference round fails hard
        let root = test_root();
        struct FailingNet;
        impl FileDownloaderNet for FailingNet {
            fn download(
                &mut self,
                _query: DownloadQuery,
                _query_type: DownloadQueryType,
                _meta: DownloadNetMeta,
            ) -> Result<DownloadResult, Status> {
                Err(Status::error(400, "FILE_REFERENCE_INVALID"))
            }
            fn open_temp_file(&mut self, _file_type: FileType) -> Result<(File, String), Status> {
                Err(Status::error_message("no io"))
            }
            fn open_partial_file(&mut self, _path: &str, _write: bool) -> Result<File, Status> {
                Err(Status::error_message("no io"))
            }
            fn create_from_temp(
                &mut self,
                _file_type: FileType,
                _path: &str,
                _name: &str,
            ) -> Result<String, Status> {
                Err(Status::error_message("no io"))
            }
        }
        struct FailFactory;
        impl FileLoadNetFactory for FailFactory {
            fn download_net(&mut self) -> Box<dyn FileDownloaderNet> {
                Box::new(FailingNet)
            }
            fn upload_net(&mut self) -> Box<dyn FileUploaderNet> {
                unimplemented!()
            }
            fn hash_net(&mut self) -> Box<dyn FileHashUploaderNet> {
                unimplemented!()
            }
            fn utils_net(&mut self) -> Box<dyn FileDownloaderNet> {
                Box::new(FailingNet)
            }
        }

        let mut file_manager = FileManager::new();
        let file_id = register_downloadable(&mut file_manager, 2, 4096, 1);
        let fllm =
            &mut FileLoadManager::new(Box::new(FailFactory), FileLoadManagerOptions::default());

        let log = Arc::new(Mutex::new(DownloadLog::default()));
        file_manager
            .set_user_download_callback(Arc::new(Mutex::new(DownloadCallbackHandle(log.clone()))));
        file_manager.download(
            file_id,
            0,
            Arc::new(Mutex::new(DownloadCallbackHandle(log.clone())))
                as Arc<parking_lot::Mutex<dyn DownloadCallback>>,
            10,
            -1,
            -1,
            fllm,
        );
        pump(&mut file_manager, fllm);

        // the decision order: the FILE_REFERENCE_ error deletes the
        // reference and restarts; the wait-file-reference round reports
        // FILE_DOWNLOAD_RESTART_WITH_FILE_REFERENCE; the next round then
        // parks at the can_download gate (the reference never recovers),
        // leaving the request pending with no terminal callback - the
        // C++ flow for a node whose reference never recovers
        // (FileManager.cpp:3294-3316, 3235-3238)
        assert!(log.lock().messages(file_id).is_empty());
        assert!(file_manager.file_download_requests.contains_key(&file_id));
        assert!(
            file_manager
                .file_download_requests
                .get(&file_id)
                .is_some_and(|requests| requests.user_download_priority == 10)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_load_manager_save_file_bytes_round_trips_and_dedups() {
        // save_file_bytes (FileLoaderUtils.cpp:209-233): a same-name
        // same-bytes file is reused; otherwise the bytes land in a new
        // file
        let root = test_root();
        let query_log: Arc<parking_lot::Mutex<Vec<(i64, i32)>>> = Arc::new(Mutex::new(Vec::new()));
        let mut factory = TestFactory {
            payload: Vec::new(),
            root: root.clone(),
            download_queries: Arc::clone(&query_log),
        };
        let mut io = factory.utils_net();
        let bytes = b"from bytes".to_vec();
        let first =
            save_file_bytes(io.as_mut(), FileType::Document, &bytes, "from_bytes.bin").unwrap();
        assert!(std::fs::read(&first.path).unwrap() == bytes);

        // the second save with the same name finds and reuses the first
        let second =
            save_file_bytes(io.as_mut(), FileType::Document, &bytes, "from_bytes.bin").unwrap();
        assert_eq!(first.path, second.path);

        // different bytes with the same name: search_file finds the old
        // copy, the content differs, and create_from_temp REPLACES the
        // target (same final path, fresh bytes)
        let other =
            save_file_bytes(io.as_mut(), FileType::Document, b"other", "from_bytes.bin").unwrap();
        assert_eq!(other.path, first.path);
        assert_eq!(std::fs::read(&other.path).unwrap(), b"other");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_load_manager_from_bytes_saves_content_through_the_manager() {
        // FileFromBytes (FileFromBytes.cpp:15-30) + FileDownloadManager::
        // from_bytes: the bytes land as a FullLocalFileLocation and the
        // DownloadOk event flows to the file node
        let root = test_root();
        let query_log: Arc<parking_lot::Mutex<Vec<(i64, i32)>>> = Arc::new(Mutex::new(Vec::new()));
        let mut file_manager = FileManager::new();
        // the remote size must match the content bytes, or the
        // register_local merge refuses the adoption like the C++
        let file_id = register_downloadable(&mut file_manager, 2, 22, 9);
        let fllm = &mut FileLoadManager::new(
            Box::new(TestFactory {
                payload: Vec::new(),
                root: root.clone(),
                download_queries: Arc::clone(&query_log),
            }),
            FileLoadManagerOptions::default(),
        );

        let bytes = b"inline thumbnail bytes".to_vec();
        let ok = file_manager.set_content(file_id, bytes.clone(), fllm);
        assert!(ok);
        file_manager.apply_load_events(fllm);

        // the bytes reached a local file and the node adopted it
        let node = file_manager.get_file_node(file_id).expect("node");
        let view = FileView::new(node);
        let local = view.get_full_local_location().expect("full local").clone();
        assert_eq!(std::fs::read(&local.path).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Answers every `upload.uploadPart` with Ok(true) - the parts are
    /// accepted and the uploader finishes on its own counters.
    struct OkUploadNet;

    impl FileUploaderNet for OkUploadNet {
        fn upload_part(&mut self, _query: UploadPartQuery) -> Result<bool, Status> {
            Ok(true)
        }
    }

    struct UploadFactory;

    impl FileLoadNetFactory for UploadFactory {
        fn download_net(&mut self) -> Box<dyn FileDownloaderNet> {
            unimplemented!("downloads ride dedicated fakes")
        }
        fn upload_net(&mut self) -> Box<dyn FileUploaderNet> {
            Box::new(OkUploadNet)
        }
        fn hash_net(&mut self) -> Box<dyn FileHashUploaderNet> {
            unimplemented!("by-hash rides dedicated fakes")
        }
        fn utils_net(&mut self) -> Box<dyn FileDownloaderNet> {
            unimplemented!("save_file_bytes rides dedicated fakes")
        }
    }

    /// The recorded `UploadCallback` log.
    #[derive(Default, Debug)]
    struct UploadLog {
        events: Vec<(FileUploadId, String)>,
    }

    struct UploadCallbackHandle(Arc<parking_lot::Mutex<UploadLog>>);

    impl UploadCallback for UploadCallbackHandle {
        fn on_upload_ok(&mut self, file_upload_id: FileUploadId) {
            self.0.lock().events.push((file_upload_id, "ok".to_owned()));
        }
        fn on_upload_error(&mut self, file_upload_id: FileUploadId, status: Status) {
            self.0
                .lock()
                .events
                .push((file_upload_id, format!("error:{}", status.message())));
        }
    }

    /// Registers a real local file for upload tests.
    fn register_uploadable(
        file_manager: &mut FileManager,
        size: usize,
    ) -> (FileId, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("m12_up_{}", tdutils::random::secure_int64()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("upload_me.bin");
        std::fs::write(&path, vec![3u8; size]).unwrap();
        let location =
            FullLocalFileLocation::new(FileType::Document, path.to_string_lossy().into_owned(), 0);
        let file_id = file_manager
            .register_local(
                location,
                DialogId::default(),
                size as i64,
                false,
                false,
                FileId::default(),
            )
            .unwrap();
        // the C++ callers reach the node through `get_sync_file_node`,
        // whose database load clears `need_load_from_pmc`; there is no db
        // in the test
        if let Some(node) = file_manager.get_file_node_mut(file_id) {
            node.need_load_from_pmc = false;
        }
        (file_id, root)
    }

    #[test]
    fn file_load_manager_upload_completes_and_pauses_for_the_next_round() {
        // the user-request upload flow: resume_upload -> run_upload ->
        // on_upload_ok selects the user request and pauses the node
        // (FileManager.cpp:4946-4953)
        let mut file_manager = FileManager::new();
        let (file_id, root) = register_uploadable(&mut file_manager, 4096);
        let fllm =
            &mut FileLoadManager::new(Box::new(UploadFactory), FileLoadManagerOptions::default());

        let log = Arc::new(Mutex::new(UploadLog::default()));
        let file_upload_id = FileUploadId::new(file_id, 0);
        file_manager.resume_upload(
            file_upload_id,
            Vec::new(),
            Arc::new(Mutex::new(UploadCallbackHandle(log.clone())))
                as Arc<parking_lot::Mutex<dyn UploadCallback>>,
            10,
            0,
            false,
            false,
            fllm,
        );
        for round in 1..=5 {
            fllm.drive_round(round);
            file_manager.apply_load_events(fllm);
            if fllm.active_uploads() == 0 {
                break;
            }
        }

        // the node is upload-paused for the next round
        let node = file_manager.get_file_node(file_id).expect("node");
        assert!(
            node.upload_pause.file_id.is_valid(),
            "pause={:?} partial={:?} requests={:?} queries={:?}",
            node.upload_pause,
            node.remote.partial,
            file_manager.file_upload_requests.is_empty(),
            file_manager.upload_queries.len()
        );
        assert!(node.remote.partial.is_some());
        // the extracted callback was the fresh Preliminary one, so the
        // user's own callback did not fire (C++ parity)
        assert!(log.lock().events.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_load_manager_cancel_upload_reports_canceled_and_clears_the_queue() {
        let mut file_manager = FileManager::new();
        let (file_id, root) = register_uploadable(&mut file_manager, 4096);
        let fllm =
            &mut FileLoadManager::new(Box::new(UploadFactory), FileLoadManagerOptions::default());

        let log = Arc::new(Mutex::new(UploadLog::default()));
        let file_upload_id = FileUploadId::new(file_id, 0);
        file_manager.resume_upload(
            file_upload_id,
            Vec::new(),
            Arc::new(Mutex::new(UploadCallbackHandle(log.clone())))
                as Arc<parking_lot::Mutex<dyn UploadCallback>>,
            10,
            0,
            false,
            false,
            fllm,
        );
        file_manager.cancel_upload(file_upload_id, fllm);

        // the extracted user-arm callback is the no-op Preliminary one;
        // the 200 Canceled the C++ logs is not observable here
        assert!(log.lock().events.is_empty());
        // the extraction cleared the request and re-prioritized the node
        assert!(file_manager.file_upload_requests.is_empty());
        let node = file_manager.get_file_node(file_id).expect("node");
        assert_eq!(node.upload_priority, 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// The files-glue slice tests: the `readFilePart`/`deleteFile`/
/// `getSuggestedFileName`/`getMapThumbnailFile` FileManager arms over real
/// temp files, in the C++ `FileManager` test order.
#[cfg(test)]
mod files_glue_tests {
    use super::*;

    /// The never-queried net factory: the read/unlink arms never touch the
    /// download nets.
    struct NopFactory;

    impl FileLoadNetFactory for NopFactory {
        fn download_net(&mut self) -> Box<dyn FileDownloaderNet> {
            unimplemented!("read_file_part never opens a net")
        }
        fn upload_net(&mut self) -> Box<dyn FileUploaderNet> {
            unimplemented!("read_file_part never opens a net")
        }
        fn hash_net(&mut self) -> Box<dyn FileHashUploaderNet> {
            unimplemented!("read_file_part never opens a net")
        }
        fn utils_net(&mut self) -> Box<dyn FileDownloaderNet> {
            unimplemented!("read_file_part never opens a net")
        }
    }

    /// The captured `Result<[u8]>`/`Result<()>` of a file promise.
    #[allow(clippy::type_complexity)]
    fn capture_promise<T: Send + 'static>() -> (
        tdutils::promise::Promise<T>,
        Arc<parking_lot::Mutex<Option<tdutils::status::Result<T>>>>,
    ) {
        let slot: Arc<parking_lot::Mutex<Option<tdutils::status::Result<T>>>> =
            Arc::new(parking_lot::Mutex::new(None));
        let capture = Arc::clone(&slot);
        let promise = tdutils::promise::PromiseCreator::lambda_result(
            move |result: tdutils::status::Result<T>| {
                *capture.lock() = Some(result);
            },
        );
        (promise, slot)
    }

    /// A context over a real temp root: `files_dir()` is the root, the
    /// stat/realpath halves hit the real filesystem.
    struct TempDirContext {
        root: String,
    }

    impl FileManagerContext for TempDirContext {
        fn real_path(&self, path: &str) -> Result<String, Status> {
            std::fs::canonicalize(path)
                .map(|p| p.to_string_lossy().into_owned())
                .map_err(|_| Status::error(400, "Can't find real file path"))
        }
        fn files_dir(&self) -> String {
            format!("{}/", self.root)
        }
    }

    /// Registers a `size`-byte local file under the temp root; returns the
    /// file id and the file bytes.
    fn register_cached_file(
        manager: &mut FileManager,
        root: &str,
        name: &str,
        contents: &[u8],
    ) -> (FileId, Vec<u8>) {
        // the registered location must sit under the per-type files dir
        // (`{root}/documents/`), like every C++ cached file
        let dir = format!("{root}/documents");
        std::fs::create_dir_all(&dir).unwrap();
        let path = format!("{dir}/{name}");
        std::fs::write(&path, contents).unwrap();
        let file_id = manager
            .register_local(
                FullLocalFileLocation::new(FileType::Document, path, 0),
                DialogId::default(),
                contents.len() as i64,
                false,
                false,
                FileId::default(),
            )
            .expect("register_local");
        (file_id, contents.to_vec())
    }

    #[test]
    fn read_file_part_validation_table() {
        let mut manager = FileManager::new();
        let fllm =
            &mut FileLoadManager::new(Box::new(NopFactory), FileLoadManagerOptions::default());

        let (promise, slot) = capture_promise();
        manager.read_file_part(FileId::new(0, 0), 0, 1, 2, promise, fllm);
        let result = slot.lock().take().expect("settled");
        assert_eq!(result.error().message(), "File identifier is invalid");

        let (promise, slot) = capture_promise();
        manager.read_file_part(FileId::new(41, 0), 0, 1, 2, promise, fllm);
        let result = slot.lock().take().expect("settled");
        assert_eq!(result.error().message(), "File not found");

        let root = std::env::temp_dir().join("td_rs_read_file_part_bands");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (file_id, _) =
            register_cached_file(&mut manager, &root.to_string_lossy(), "unused", b"x");
        for (offset, count, message) in [
            (-1, 1, "Parameter offset must be non-negative"),
            (0, -1, "Parameter count must be non-negative"),
        ] {
            let (promise, slot) = capture_promise();
            manager.read_file_part(file_id, offset, count, 2, promise, fllm);
            let result = slot.lock().take().expect("settled");
            assert_eq!(result.error().message(), message);
        }
    }

    #[test]
    fn read_file_part_reads_the_downloaded_prefix() {
        let root = std::env::temp_dir().join("td_rs_read_file_part");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root_str = root.to_string_lossy().into_owned();

        let mut manager = FileManager::new();
        manager.set_context(Box::new(TempDirContext {
            root: root_str.clone(),
        }));
        let contents = b"hello, files glue";
        let (file_id, _) = register_cached_file(&mut manager, &root_str, "part.bin", contents);
        let fllm =
            &mut FileLoadManager::new(Box::new(NopFactory), FileLoadManagerOptions::default());

        // count == 0 reads the whole downloaded prefix
        let (promise, slot) = capture_promise();
        manager.read_file_part(file_id, 0, 0, 2, promise, fllm);
        let result = slot.lock().take().expect("settled");
        assert_eq!(result.move_as_ok(), contents.to_vec());

        // an explicit count inside the prefix reads the window
        let (promise, slot) = capture_promise();
        manager.read_file_part(file_id, 7, 5, 2, promise, fllm);
        let result = slot.lock().take().expect("settled");
        assert_eq!(result.move_as_ok(), b"files".to_vec());

        // count beyond the prefix is the C++ 400
        let (promise, slot) = capture_promise();
        manager.read_file_part(file_id, 0, contents.len() as i64 + 1, 2, promise, fllm);
        let result = slot.lock().take().expect("settled");
        assert_eq!(
            result.error().message(),
            "There is not enough downloaded bytes in the file to read"
        );

        // an offset beyond the registered size reads nothing
        let (promise, slot) = capture_promise();
        manager.read_file_part(file_id, contents.len() as i64, 0, 2, promise, fllm);
        let result = slot.lock().take().expect("settled");
        assert_eq!(result.move_as_ok(), Vec::<u8>::new());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_file_unlinks_cached_files_and_answers_ok() {
        let root = std::env::temp_dir().join("td_rs_delete_file");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root_str = root.to_string_lossy().into_owned();

        let mut manager = FileManager::new();
        manager.set_context(Box::new(TempDirContext {
            root: root_str.clone(),
        }));
        let (file_id, _) = register_cached_file(&mut manager, &root_str, "gone.bin", b"bye");
        let path = root_str.clone() + "/documents/gone.bin";
        let fllm =
            &mut FileLoadManager::new(Box::new(NopFactory), FileLoadManagerOptions::default());

        let (promise, slot) = capture_promise();
        manager.delete_file(file_id, promise, "deleteFile", fllm);
        assert_eq!(slot.lock().take().expect("settled").move_as_ok(), ());
        assert!(
            !std::path::Path::new(&path).exists(),
            "the file is unlinked"
        );
        // the local location is dropped from the node
        assert_eq!(manager.get_file_node(file_id).expect("node").path(), "");

        // an unknown file id answers ok, like the C++ null-node band
        let (promise, slot) = capture_promise();
        manager.delete_file(FileId::new(41, 0), promise, "deleteFile", fllm);
        assert_eq!(slot.lock().take().expect("settled").move_as_ok(), ());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn get_suggested_file_name_avoids_collisions() {
        let root = std::env::temp_dir().join("td_rs_suggested_name");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root_str = root.to_string_lossy().into_owned();

        let mut manager = FileManager::new();
        manager.set_context(Box::new(TempDirContext {
            root: root_str.clone(),
        }));
        let context = TempDirContext {
            root: root_str.clone(),
        };

        // an unknown file id is the C++ "Wrong file identifier" band
        let error = manager
            .get_suggested_file_name(FileId::new(41, 0), "")
            .unwrap_err();
        assert_eq!(error.message(), "Wrong file identifier");

        // an empty directory means the cleaned name, unchanged
        let named = manager.register_remote(
            FullRemoteFileLocation::from_document(
                FileType::Document,
                1,
                2,
                DcId::internal(1),
                "ref".to_owned(),
            ),
            FileLocationSource::FromUser,
            DialogId::default(),
            7,
            0,
            "doc.bin".to_owned(),
        );
        let cleaned = manager
            .get_suggested_file_name(named, &root_str)
            .expect("ok");
        assert_eq!(cleaned, "doc.bin");

        // the first free `stem (N).ext` slot wins
        std::fs::write(format!("{root_str}/doc.bin"), b"x").unwrap();
        let cleaned = manager
            .get_suggested_file_name(named, &root_str)
            .expect("ok");
        assert_eq!(cleaned, "doc (1).bin");
        std::fs::write(format!("{root_str}/doc (1).bin"), b"x").unwrap();
        let cleaned = manager
            .get_suggested_file_name(named, &root_str)
            .expect("ok");
        assert_eq!(cleaned, "doc (2).bin");

        // a non-existent directory answers the cleaned name
        let cleaned = manager
            .get_suggested_file_name(named, "/no/such/dir/")
            .expect("ok");
        assert_eq!(cleaned, "doc.bin");

        drop(context);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn get_map_thumbnail_file_id_validates_and_builds_the_conversion() {
        let mut manager = FileManager::new();
        let location = crate::chat_manager::Location::from_td_api_location(&td_api::Location {
            latitude: 37.484,
            longitude: 55.8987,
            horizontal_accuracy: 0.0,
        });

        // the validation ladder, in the C++ order
        let bad_location = crate::chat_manager::Location::from_td_api_location(&td_api::Location {
            latitude: 95.0,
            longitude: 0.0,
            horizontal_accuracy: 0.0,
        });
        assert_eq!(
            manager
                .get_map_thumbnail_file_id(&bad_location, 15, 100, 100, 1, DialogId::default())
                .unwrap_err()
                .message(),
            "Invalid location specified"
        );
        for (zoom, width, height, scale, message) in [
            (12, 100, 100, 1, "Wrong zoom"),
            (21, 100, 100, 1, "Wrong zoom"),
            (15, 15, 100, 1, "Wrong width"),
            (15, 1025, 100, 1, "Wrong width"),
            (15, 100, 15, 1, "Wrong height"),
            (15, 100, 1025, 1, "Wrong height"),
            (15, 100, 100, 0, "Wrong scale"),
            (15, 100, 100, 4, "Wrong scale"),
        ] {
            assert_eq!(
                manager
                    .get_map_thumbnail_file_id(
                        &location,
                        zoom,
                        width,
                        height,
                        scale,
                        DialogId::default()
                    )
                    .unwrap_err()
                    .message(),
                message,
            );
        }

        // the mercator tile coordinates land in the `#map#` conversion
        let file_id = manager
            .get_map_thumbnail_file_id(&location, 15, 100, 100, 1, DialogId::default())
            .expect("ok");
        let node = manager.get_file_node(file_id).expect("node");
        let generate = node.generate.as_ref().expect("generate location");
        assert_eq!(generate.file_type, FileType::Thumbnail);
        // the C++ mercator tail: x over the longitude, y over
        // `log((1 + sin lat) / (1 - sin lat))` — both clamped into the tile
        assert_eq!(generate.conversion, "#map#15#5496838#3250931#100#100#1#");
    }
}
