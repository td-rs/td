//! `DocumentsManager.{h,cpp,hpp}` — the GeneralDocument plane: the
//! on_get_document attribute machine, the td_api builders, the
//! store/parse pair and the input-media legs.

use std::collections::HashMap;

use parking_lot::Mutex;
use std::sync::Arc;
use tdmtproto::secret_api;
use tdmtproto::telegram_api;
use tdutils::status::Result as TdResult;

use crate::chat_manager::DcId;
use crate::document::{Document, DocumentType};
use crate::files::{
    FileEncryptionKey, FileLoadManager, FileLocationSource, FileManager, FileType,
    FullRemoteFileLocation,
};
use crate::message_id::{DialogId, DialogType};
use crate::photo::{
    self, AnimationSize, Dimensions, PhotoFormat, PhotoSize, PhotoSizeOrMinithumbnail,
    get_dimensions,
};
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::FileId;

/// `td::StickerFormat` (StickerFormat.h:19) — the M.15-owned value plane;
/// the local copy carries only the half the document intake reads and
/// folds into the M.15 stickers_manager line at its integration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StickerFormat {
    #[default]
    Unknown,
    Webp,
    Tgs,
    Webm,
}

/// `get_sticker_format_photo_format` (StickerFormat.cpp:104-117) — the
/// M.15 table, local for the same reason as [`StickerFormat`].
pub fn get_sticker_format_photo_format(sticker_format: StickerFormat) -> PhotoFormat {
    match sticker_format {
        StickerFormat::Unknown | StickerFormat::Webp => PhotoFormat::Webp,
        StickerFormat::Tgs => PhotoFormat::Tgs,
        StickerFormat::Webm => PhotoFormat::Webm,
    }
}

/// `get_max_sticker_file_size` (StickerFormat.cpp:151-166) — the M.15
/// table; the `sticker_type` half collapses to the `is_custom_emoji`
/// bool it feeds (the intake only passes `StickerType::Regular`).
pub fn get_max_sticker_file_size(
    sticker_format: StickerFormat,
    is_custom_emoji: bool,
    for_thumbnail: bool,
) -> i64 {
    match sticker_format {
        StickerFormat::Unknown | StickerFormat::Webp => {
            if for_thumbnail || !is_custom_emoji {
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
    }
}

/// `StickersManager::has_webp_thumbnail` (StickersManager.cpp:2956-2969)
/// — the server replaces user thumbnails with WEBP ones; a stripped or
/// progressive size means the thumbnail is not a WEBP file.
pub fn has_webp_thumbnail(thumbnails: &[telegram_api::PhotoSize]) -> bool {
    for size in thumbnails {
        match size {
            telegram_api::PhotoSize::photoStrippedSize(_)
            | telegram_api::PhotoSize::photoSizeProgressive(_) => return false,
            _ => {}
        }
    }
    true
}

/// `td::EncryptedFile` (EncryptedFile.h:16-24) — the encrypted-file id
/// plane the secret intake reads. The full header (store/parse,
/// `get_encrypted_file`'s producer) is the Files plane; the value and
/// the wire filter land here because [`RemoteDocument::Secret`] carries
/// them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncryptedFile {
    pub id: i64,
    pub access_hash: i64,
    pub size: i64,
    pub dc_id: i32,
    pub key_fingerprint: i32,
}

impl EncryptedFile {
    /// `EncryptedFile::get_encrypted_file` (EncryptedFile.h:31-41) — an
    /// absent or empty file, or a negative size, yields `None`.
    pub fn get_encrypted_file(file: Option<telegram_api::EncryptedFile>) -> Option<EncryptedFile> {
        let telegram_api::EncryptedFile::encryptedFile(file) = file? else {
            return None;
        };
        if file.size < 0 {
            return None;
        }
        Some(EncryptedFile {
            id: file.id,
            access_hash: file.access_hash,
            size: file.size,
            dc_id: file.dc_id,
            key_fingerprint: file.key_fingerprint,
        })
    }
}

/// The `DocumentsManager::RemoteDocument` ctor set (DocumentsManager.h:42-83)
/// — a server document (attributes split out of the wire object, exactly
/// like the C++ constructor moves them), a web document with a
/// pre-classified thumbnail, or a secret encrypted document.
pub enum RemoteDocument {
    Server {
        document: telegram_api::document,
        attributes: Vec<telegram_api::DocumentAttribute>,
    },
    Web {
        web_document: telegram_api::WebDocument,
        thumbnail: PhotoSize,
        attributes: Vec<telegram_api::DocumentAttribute>,
    },
    Secret {
        secret_file: EncryptedFile,
        secret_document: secret_api::decryptedMessageMediaDocument,
        attributes: Vec<telegram_api::DocumentAttribute>,
    },
}

/// The `on_get_document` source payload: the three remote shapes the
/// attribute machine folds into the `Document` record.
enum SourceDocument {
    Server(telegram_api::document),
    Web {
        web_document: telegram_api::WebDocument,
        thumbnail: PhotoSize,
    },
    Secret {
        secret_file: EncryptedFile,
        secret_document: secret_api::decryptedMessageMediaDocument,
    },
}

impl RemoteDocument {
    /// The `RemoteDocument(server_document)` constructor
    /// (DocumentsManager.h:54-61) — the attributes move out of the
    /// document.
    pub fn from_server_document(mut document: telegram_api::document) -> RemoteDocument {
        let attributes = std::mem::take(&mut document.attributes);
        RemoteDocument::Server {
            document,
            attributes,
        }
    }

    /// The `RemoteDocument(web_document, thumbnail, attributes)`
    /// constructor (DocumentsManager.h:63-71).
    pub fn from_web_document(
        web_document: telegram_api::WebDocument,
        thumbnail: PhotoSize,
        attributes: Vec<telegram_api::DocumentAttribute>,
    ) -> RemoteDocument {
        RemoteDocument::Web {
            web_document,
            thumbnail,
            attributes,
        }
    }

    /// The `RemoteDocument(secret_file, secret_document, attributes)`
    /// constructor (DocumentsManager.h:73-82).
    pub fn from_secret_document(
        secret_file: EncryptedFile,
        secret_document: secret_api::decryptedMessageMediaDocument,
        attributes: Vec<telegram_api::DocumentAttribute>,
    ) -> RemoteDocument {
        RemoteDocument::Secret {
            secret_file,
            secret_document,
            attributes,
        }
    }
}

/// `DocumentsManager::Subtype` (DocumentsManager.h:90) — the caller-side
/// refinement the intake cross-checks the attributes against.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Subtype {
    Background,
    Pattern,
    Ringtone,
    Story,
    #[default]
    Other,
}

/// `DocumentsManager::GeneralDocument` (DocumentsManager.h:129-137).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GeneralDocument {
    pub file_name: String,
    pub mime_type: String,
    pub minithumbnail: String,
    pub thumbnail: PhotoSize,
    pub dimensions: Dimensions,
    pub file_id: FileId,
}

/// The `Td *td_` slice the collapsed manager reads (see the module doc).
pub trait DocumentsManagerEnv {
    /// `td_->auth_manager_->is_bot()`.
    fn auth_is_bot(&self) -> bool;

    /// The `td_->file_manager_` handle — `register_remote`,
    /// `from_persistent_id`, `set_encryption_key`, `merge`,
    /// `get_file_object` and the file views ride it (locked per call).
    fn file_manager(&self) -> Arc<Mutex<FileManager>>;

    /// The `td_->file_load_manager_` handle — the `set_content` tail of
    /// the FileManager-integrated photo-size helpers (the C++
    /// FileManager reaches it on its own).
    fn file_load_manager(&self) -> Arc<Mutex<FileLoadManager>>;

    // -- the sibling-record dispatch of the on_get_document switch
    // (Td.cpp constructor order: audios/documents/videos in
    // init_managers, voice/video notes as actors)

    /// `animations_manager_->create_animation` — the M.20 seam; the
    /// collapsed line answers nothing and the animation record is not
    /// built until its integration.
    #[allow(clippy::too_many_arguments)]
    fn create_animation(
        &mut self,
        file_id: FileId,
        minithumbnail: String,
        thumbnail: PhotoSize,
        animated_thumbnail: AnimationSize,
        has_stickers: bool,
        file_name: String,
        mime_type: String,
        duration: i32,
        dimensions: Dimensions,
        replace: bool,
    );

    /// `stickers_manager_->create_sticker` — the M.15 seam; the C++
    /// also forwards the sticker-load `MultiPromiseActor *` here, which
    /// the seam drops together with the record.
    #[allow(clippy::too_many_arguments)]
    fn create_sticker(
        &mut self,
        file_id: FileId,
        premium_animation_file_id: FileId,
        minithumbnail: String,
        thumbnail: PhotoSize,
        dimensions: Dimensions,
        sticker: Option<telegram_api::documentAttributeSticker>,
        custom_emoji: Option<telegram_api::documentAttributeCustomEmoji>,
        sticker_format: StickerFormat,
    );

    /// `audios_manager_->create_audio` (M.28.2).
    #[allow(clippy::too_many_arguments)]
    fn create_audio(
        &mut self,
        file_id: FileId,
        minithumbnail: String,
        thumbnail: PhotoSize,
        file_name: String,
        mime_type: String,
        duration: i32,
        title: String,
        performer: String,
        date: i32,
        replace: bool,
    );

    /// `videos_manager_->create_video` (M.28.1).
    #[allow(clippy::too_many_arguments)]
    fn create_video(
        &mut self,
        file_id: FileId,
        minithumbnail: String,
        thumbnail: PhotoSize,
        animated_thumbnail: AnimationSize,
        has_stickers: bool,
        sticker_file_ids: Vec<FileId>,
        file_name: String,
        mime_type: String,
        duration: i32,
        precise_duration: f64,
        dimensions: Dimensions,
        supports_streaming: bool,
        is_animation: bool,
        preload_prefix_size: i32,
        start_ts: f64,
        codec: String,
        replace: bool,
    );

    /// `video_notes_manager_->create_video_note` (M.28.2).
    #[allow(clippy::too_many_arguments)]
    fn create_video_note(
        &mut self,
        file_id: FileId,
        minithumbnail: String,
        thumbnail: PhotoSize,
        duration: i32,
        dimensions: Dimensions,
        waveform: String,
        replace: bool,
    );

    /// `voice_notes_manager_->create_voice_note` (M.28.2).
    fn create_voice_note(
        &mut self,
        file_id: FileId,
        mime_type: String,
        duration: i32,
        waveform: String,
        replace: bool,
    );
}

/// `DocumentsManager` (DocumentsManager.h:33-145).
#[derive(Default)]
pub struct DocumentsManager {
    documents: HashMap<FileId, GeneralDocument>,
}

impl std::fmt::Debug for DocumentsManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentsManager")
            .field("document_count", &self.documents.len())
            .finish()
    }
}

impl DocumentsManager {
    pub fn new() -> DocumentsManager {
        DocumentsManager::default()
    }

    /// `DocumentsManager::get_document` (DocumentsManager.cpp:648-650).
    fn get_document(&self, file_id: FileId) -> Option<&GeneralDocument> {
        self.documents.get(&file_id)
    }

    /// `DocumentsManager::on_get_document(unique_ptr<GeneralDocument>,
    /// bool)` (DocumentsManager.cpp:611-632) — the id-map insert/replace
    /// machine.
    fn on_get_document_record(
        &mut self,
        mut new_document: GeneralDocument,
        replace: bool,
    ) -> FileId {
        let file_id = new_document.file_id;
        assert!(file_id.is_valid());
        tdutils::log_info!("Receive document {}", file_id.get());
        match self.documents.get_mut(&file_id) {
            None => {
                self.documents.insert(file_id, new_document);
            }
            Some(d) => {
                if replace
                    && (d.mime_type != new_document.mime_type
                        || d.file_name != new_document.file_name
                        || d.minithumbnail != new_document.minithumbnail
                        || d.thumbnail != new_document.thumbnail
                        || d.dimensions != new_document.dimensions)
                {
                    d.mime_type = std::mem::take(&mut new_document.mime_type);
                    d.file_name = std::mem::take(&mut new_document.file_name);
                    d.minithumbnail = std::mem::take(&mut new_document.minithumbnail);
                    d.thumbnail = std::mem::take(&mut new_document.thumbnail);
                    d.dimensions = new_document.dimensions;
                }
            }
        }

        file_id
    }

    /// `DocumentsManager::create_document` (DocumentsManager.cpp:634-646).
    /// `DocumentsManager::create_document` (DocumentsManager.cpp:632-643).
    #[allow(clippy::too_many_arguments)]
    pub fn create_document(
        &mut self,
        env: &impl DocumentsManagerEnv,
        file_id: FileId,
        minithumbnail: String,
        thumbnail: PhotoSize,
        file_name: String,
        mime_type: String,
        dimensions: Dimensions,
        replace: bool,
    ) {
        let mut d = GeneralDocument {
            file_id,
            file_name,
            mime_type,
            minithumbnail: String::new(),
            thumbnail,
            dimensions,
        };
        if !env.auth_is_bot() {
            d.minithumbnail = minithumbnail;
        }
        self.on_get_document_record(d, replace);
    }

    /// `DocumentsManager::get_document_object` (DocumentsManager.cpp:61-73).
    /// The C++ `nullptr` for an invalid file id is the `None` arm.
    pub fn get_document_object(
        &self,
        env: &impl DocumentsManagerEnv,
        file_id: FileId,
        thumbnail_format: PhotoFormat,
    ) -> Option<td_api::Document> {
        if !file_id.is_valid() {
            return None;
        }

        let document = self.get_document(file_id)?;
        Some(td_api::Document {
            file_name: document.file_name.clone(),
            mime_type: document.mime_type.clone(),
            minithumbnail: Some(photo::get_minithumbnail_object(
                document.minithumbnail.as_bytes(),
            )),
            thumbnail: photo::get_thumbnail_object(&document.thumbnail, thumbnail_format),
            document: Some(env.file_manager().lock().get_file_object(file_id)),
        })
    }

    /// `DocumentsManager::get_video_storyboard_object`
    /// (DocumentsManager.cpp:75-94) — the storyboard video pairs with the
    /// map document registered under the "mtproto:<document_id>" name.
    pub fn get_video_storyboard_object(
        &self,
        env: &impl DocumentsManagerEnv,
        file_id: FileId,
        map_file_ids: &[FileId],
    ) -> Option<td_api::VideoStoryboard> {
        let file_manager = env.file_manager();
        let file_manager = file_manager.lock();
        let file_view = file_manager.get_file_view(file_id)?;
        // the C++ `document == nullptr` guard
        self.get_document(file_id)?;
        let full_remote_location = file_view.get_full_remote_location()?;
        assert!(full_remote_location.is_document());
        let document_id = full_remote_location.get_id();
        let name = format!("mtproto:{document_id}");
        for map_file_id in map_file_ids {
            let map = self.get_document(*map_file_id)?;
            if map.file_name == name && map.dimensions != Dimensions::default() {
                return Some(td_api::VideoStoryboard {
                    storyboard_file: Some(file_manager.get_file_object(file_id)),
                    width: i32::from(map.dimensions.width),
                    height: i32::from(map.dimensions.height),
                    map_file: Some(file_manager.get_file_object(*map_file_id)),
                });
            }
        }
        None
    }

    /// `fix_tgs_sticker_type` (DocumentsManager.cpp:341-358) — the tgs
    /// sticker-type fixup lambda of the server-document intake.
    #[allow(clippy::too_many_arguments)]
    fn fix_tgs_sticker_type(
        mime_type: &str,
        sticker_format: &mut StickerFormat,
        default_extension: &mut &str,
        document_type: &mut DocumentType,
        file_type: &mut FileType,
        owner_dialog_id: &mut DialogId,
        file_name: &mut String,
        thumbnail_format: &mut PhotoFormat,
    ) {
        if mime_type != "application/x-tgsticker" {
            return;
        }
        *sticker_format = StickerFormat::Tgs;
        *default_extension = "tgs";
        if *document_type == DocumentType::General {
            *document_type = DocumentType::Sticker;
            *file_type = FileType::Sticker;
            *owner_dialog_id = DialogId::default();
            file_name.clear();
            *thumbnail_format = PhotoFormat::Webp;
        }
    }

    /// `DocumentsManager::on_get_document(RemoteDocument, ...)`
    /// (DocumentsManager.cpp:96-609) — the attribute machine. The C++
    /// `nullptr`-returns (the `{}` empty Document) are the `None` arms;
    /// the dimension-source log strings collapse to the attribute names
    /// (log text only). The C++ zero-initializes the server-document
    /// locals before the web/secret arms reassign them — the dead
    /// initializers are the C++ shape.
    #[allow(clippy::too_many_lines)]
    #[allow(unused_assignments)]
    #[allow(clippy::too_many_arguments)]
    pub fn on_get_document(
        &mut self,
        env: &mut impl DocumentsManagerEnv,
        remote_document: RemoteDocument,
        mut owner_dialog_id: DialogId,
        is_self_destructing: bool,
        is_live_photo: bool,
        default_document_type: DocumentType,
        document_subtype: Subtype,
    ) -> Document {
        let (source, attributes) = match remote_document {
            RemoteDocument::Server {
                document,
                attributes,
            } => (SourceDocument::Server(document), attributes),
            RemoteDocument::Web {
                web_document,
                thumbnail,
                attributes,
            } => (
                SourceDocument::Web {
                    web_document,
                    thumbnail,
                },
                attributes,
            ),
            RemoteDocument::Secret {
                secret_file,
                secret_document,
                attributes,
            } => (
                SourceDocument::Secret {
                    secret_file,
                    secret_document,
                },
                attributes,
            ),
        };

        let mut animated: Option<telegram_api::documentAttributeAnimated> = None;
        let mut video: Option<telegram_api::documentAttributeVideo> = None;
        let mut audio: Option<telegram_api::documentAttributeAudio> = None;
        let mut sticker: Option<telegram_api::documentAttributeSticker> = None;
        let mut custom_emoji: Option<telegram_api::documentAttributeCustomEmoji> = None;
        let mut dimensions = Dimensions::default();
        let mut file_name = String::new();
        let mut has_stickers = false;
        let mut type_attributes = 0i32;
        for attribute in attributes {
            match attribute {
                telegram_api::DocumentAttribute::documentAttributeImageSize(image_size) => {
                    dimensions =
                        get_dimensions(image_size.w, image_size.h, "documentAttributeImageSize");
                }
                telegram_api::DocumentAttribute::documentAttributeAnimated(value) => {
                    animated = Some(value.clone());
                    type_attributes += 1;
                }
                telegram_api::DocumentAttribute::documentAttributeSticker(value) => {
                    sticker = Some(value.clone());
                    type_attributes += 1;
                }
                telegram_api::DocumentAttribute::documentAttributeVideo(value) => {
                    video = Some(value.clone());
                    type_attributes += 1;
                }
                telegram_api::DocumentAttribute::documentAttributeAudio(value) => {
                    audio = Some(value.clone());
                    type_attributes += 1;
                }
                telegram_api::DocumentAttribute::documentAttributeFilename(value) => {
                    file_name = value.file_name.clone();
                }
                telegram_api::DocumentAttribute::documentAttributeHasStickers(_) => {
                    has_stickers = true;
                }
                telegram_api::DocumentAttribute::documentAttributeCustomEmoji(value) => {
                    custom_emoji = Some(value.clone());
                    type_attributes += 1;
                }
            }
        }

        let mut video_is_animation = false;
        let mut video_precise_duration = 0.0f64;
        let mut video_duration = 0i32;
        let mut video_preload_prefix_size = 0i32;
        let mut video_start_ts = 0.0f64;
        let mut video_waveform = String::new();
        if let Some(video_attribute) = &video {
            video_precise_duration = video_attribute.duration;
            video_duration = video_attribute.duration.ceil() as i32;
            if document_subtype == Subtype::Story {
                video_preload_prefix_size = video_attribute.preload_prefix_size;
                video_start_ts = video_attribute.video_start_ts;
            }
            video_is_animation = video_attribute.nosound;
            let video_dimensions = get_dimensions(
                video_attribute.w,
                video_attribute.h,
                "documentAttributeVideo",
            );
            if dimensions.width == 0
                || (video_dimensions.width != 0 && video_dimensions != dimensions)
            {
                if dimensions.width != 0 {
                    tdutils::log_error!(
                        "Receive ambiguous video dimensions {:?} and {:?}",
                        dimensions,
                        video_dimensions
                    );
                }
                dimensions = video_dimensions;
            }
            if let Some(audio_attribute) = &audio {
                video_waveform = String::from_utf8_lossy(&audio_attribute.waveform).into_owned();
                type_attributes -= 1;
                audio = None;
            }
            if animated.is_some() && default_document_type == DocumentType::Video {
                type_attributes -= 1;
                animated = None;
            }

            if animated.is_some() {
                type_attributes -= 1;
                if video_attribute.round_message {
                    // video note without sound
                    animated = None;
                } else if sticker.is_some() || custom_emoji.is_some() {
                    // sticker
                    type_attributes -= 1;
                    animated = None;
                    video = None;
                } else {
                    // video animation
                    video = None;
                }
            } else if sticker.is_some() || custom_emoji.is_some() {
                // some stickers uploaded before release
                type_attributes -= 1;
                video = None;
            }
        } else if animated.is_some() && default_document_type == DocumentType::Video {
            tdutils::log_error!(
                "Receive a document with documentAttributeAnimated and without video"
            );
            type_attributes -= 1;
            animated = None;
        }
        if animated.is_some() && audio.is_some() {
            // animation send as audio
            type_attributes -= 1;
            audio = None;
        }
        if animated.is_some() && sticker.is_some() {
            // animation send as sticker
            type_attributes -= 1;
            sticker = None;
        }
        if animated.is_some() && custom_emoji.is_some() {
            // just in case
            type_attributes -= 1;
            custom_emoji = None;
        }

        let mut document_type = default_document_type;
        let mut file_type = FileType::Document;
        let mut default_extension = "";
        let mut supports_streaming = false;
        let mut video_codec = String::new();
        let mut sticker_format = StickerFormat::Unknown;
        let mut thumbnail_format = PhotoFormat::Jpeg;
        if type_attributes == 1 || default_document_type != DocumentType::General {
            // not a general document
            if animated.is_some() || default_document_type == DocumentType::Animation {
                document_type = DocumentType::Animation;
                file_type = FileType::Animation;
                default_extension = "mp4";
            } else if audio.is_some()
                || default_document_type == DocumentType::Audio
                || default_document_type == DocumentType::VoiceNote
            {
                let mut is_voice_note = default_document_type == DocumentType::VoiceNote;
                if let Some(audio_attribute) = &audio {
                    is_voice_note = audio_attribute.voice;
                }
                if is_voice_note {
                    document_type = DocumentType::VoiceNote;
                    file_type = if is_self_destructing {
                        FileType::SelfDestructingVoiceNote
                    } else {
                        FileType::VoiceNote
                    };
                    default_extension = "oga";
                    file_name.clear();
                } else {
                    document_type = DocumentType::Audio;
                    file_type = FileType::Audio;
                    default_extension = "mp3";
                }
            } else if sticker.is_some()
                || custom_emoji.is_some()
                || default_document_type == DocumentType::Sticker
            {
                document_type = DocumentType::Sticker;
                file_type = FileType::Sticker;
                sticker_format = StickerFormat::Webp;
                default_extension = "webp";
                owner_dialog_id = DialogId::default();
                file_name.clear();
            } else if video.is_some()
                || default_document_type == DocumentType::Video
                || default_document_type == DocumentType::VideoNote
            {
                let mut is_video_note = default_document_type == DocumentType::VideoNote;
                if let Some(video_attribute) = &video {
                    is_video_note = video_attribute.round_message;
                    if !is_video_note {
                        supports_streaming = video_attribute.supports_streaming;
                        video_codec = video_attribute.video_codec.clone();
                    }
                }
                if is_video_note {
                    document_type = DocumentType::VideoNote;
                    file_type = if is_self_destructing {
                        FileType::SelfDestructingVideoNote
                    } else {
                        FileType::VideoNote
                    };
                    file_name.clear();
                } else {
                    document_type = DocumentType::Video;
                    if is_live_photo {
                        file_type = if is_self_destructing {
                            FileType::SelfDestructingLivePhoto
                        } else {
                            FileType::LivePhoto
                        };
                    } else {
                        file_type = if is_self_destructing {
                            FileType::SelfDestructingVideo
                        } else {
                            FileType::Video
                        };
                    }
                }
                default_extension = "mp4";
            }
        } else if type_attributes >= 2 {
            tdutils::log_warning!(
                "Receive document with more than 1 type attribute: animated = {}, sticker = {}, \
                 custom_emoji = {}, video = {}, audio = {}, file_name = {}, dimensions = {:?}, \
                 has_stickers = {}",
                animated.is_some(),
                sticker.is_some(),
                custom_emoji.is_some(),
                video.is_some(),
                audio.is_some(),
                file_name,
                dimensions,
                has_stickers
            );
        }

        match document_subtype {
            Subtype::Background => {
                if document_type != DocumentType::General {
                    tdutils::log_error!("Receive background of type {:?}", document_type);
                    document_type = DocumentType::General;
                }
                file_type = FileType::Background;
                default_extension = "jpg";
            }
            Subtype::Pattern => {
                if document_type != DocumentType::General {
                    tdutils::log_error!("Receive background of type {:?}", document_type);
                    document_type = DocumentType::General;
                }
                file_type = FileType::Background;
                default_extension = "png";
                thumbnail_format = PhotoFormat::Png;
            }
            Subtype::Ringtone => {
                if document_type != DocumentType::Audio {
                    tdutils::log_error!("Receive notification tone of type {:?}", document_type);
                    document_type = DocumentType::Audio;
                }
                file_type = FileType::Ringtone;
                default_extension = "mp3";
            }
            Subtype::Story => {
                if document_type != DocumentType::Video {
                    tdutils::log_error!("Receive story of type {:?}", document_type);
                    document_type = DocumentType::Video;
                }
                file_type = FileType::VideoStory;
                default_extension = "mp4";
            }
            Subtype::Other => {}
        }

        let mut id = 0i64;
        let mut access_hash = 0i64;
        let mut dc_id_value = 0i32;
        let mut size = 0i64;
        let mut date = 0i32;
        let mut mime_type = String::new();
        let mut file_reference = String::new();
        let mut minithumbnail = String::new();
        let mut thumbnail = PhotoSize::default();
        let mut animated_thumbnail = AnimationSize::default();
        let mut premium_animation_file_id = FileId::default();
        let mut encryption_key = FileEncryptionKey::default();
        let mut is_web = false;
        let mut is_web_no_proxy = false;
        let mut url = String::new();
        let mut source_location = FileLocationSource::FromServer;

        match &source {
            SourceDocument::Server(document) => {
                id = document.id;
                access_hash = document.access_hash;
                dc_id_value = document.dc_id;
                size = document.size;
                if document_subtype == Subtype::Ringtone {
                    date = document.date;
                }
                mime_type = document.mime_type.clone();
                file_reference = String::from_utf8_lossy(&document.file_reference).into_owned();

                if document_type == DocumentType::Sticker && has_webp_thumbnail(&document.thumbs) {
                    thumbnail_format = PhotoFormat::Webp;
                }
                Self::fix_tgs_sticker_type(
                    &mime_type,
                    &mut sticker_format,
                    &mut default_extension,
                    &mut document_type,
                    &mut file_type,
                    &mut owner_dialog_id,
                    &mut file_name,
                    &mut thumbnail_format,
                );

                if owner_dialog_id.get_type() == DialogType::SecretChat {
                    // secret_api::decryptedMessageMediaExternalDocument
                    if document_type != DocumentType::Sticker {
                        tdutils::log_error!(
                            "Receive {:?} in a secret chat {:?}",
                            document_type,
                            owner_dialog_id
                        );
                        return Document::default();
                    }
                    source_location = FileLocationSource::FromUser;
                }

                if document_type != DocumentType::VoiceNote {
                    for thumbnail_ptr in &document.thumbs {
                        let photo_size = photo::get_photo_size_registered(
                            &mut env.file_manager().lock(),
                            &mut env.file_load_manager().lock(),
                            crate::files::PhotoSizeSource::thumbnail(FileType::Thumbnail, 0),
                            id,
                            access_hash,
                            file_reference.clone(),
                            DcId::create(dc_id_value),
                            owner_dialog_id,
                            thumbnail_ptr.clone(),
                            thumbnail_format,
                        );
                        match photo_size {
                            PhotoSizeOrMinithumbnail::Size(new_thumbnail) => {
                                if !thumbnail.file_id.is_valid() {
                                    thumbnail = new_thumbnail;
                                }
                            }
                            PhotoSizeOrMinithumbnail::Minithumbnail(packed) => {
                                minithumbnail = packed;
                            }
                        }
                    }
                }
                for thumbnail_ptr in &document.video_thumbs {
                    let telegram_api::VideoSize::videoSize(video_size) = thumbnail_ptr else {
                        continue;
                    };
                    match video_size.r#type.as_str() {
                        "v" if !animated_thumbnail.photo_size.file_id.is_valid() => {
                            animated_thumbnail = photo::get_animation_size_registered(
                                &mut env.file_manager().lock(),
                                crate::files::PhotoSizeSource::thumbnail(FileType::Thumbnail, 0),
                                id,
                                access_hash,
                                file_reference.clone(),
                                DcId::create(dc_id_value),
                                owner_dialog_id,
                                video_size.clone(),
                            );
                        }
                        "f" if !premium_animation_file_id.is_valid() => {
                            premium_animation_file_id = photo::register_photo_size(
                                &mut env.file_manager().lock(),
                                crate::files::PhotoSizeSource::thumbnail(
                                    FileType::Thumbnail,
                                    b'f' as i32,
                                ),
                                id,
                                access_hash,
                                file_reference.clone(),
                                owner_dialog_id,
                                video_size.size,
                                DcId::create(dc_id_value),
                                get_sticker_format_photo_format(sticker_format),
                            );
                        }
                        _ => {}
                    }
                }
            }
            SourceDocument::Secret {
                secret_file,
                secret_document,
            } => {
                id = secret_file.id;
                access_hash = secret_file.access_hash;
                dc_id_value = secret_file.dc_id;
                size = secret_document.size;
                mime_type = String::from_utf8_lossy(&secret_document.mime_type).into_owned();
                file_type = FileType::Encrypted;
                encryption_key = FileEncryptionKey::new(&secret_document.key, &secret_document.iv);
                if encryption_key.empty() {
                    return Document::default();
                }

                // do not allow encrypted TGS stickers
                // fix_tgs_sticker_type();

                if document_type != DocumentType::VoiceNote {
                    thumbnail = photo::get_secret_thumbnail_photo_size(
                        &mut env.file_manager().lock(),
                        &mut env.file_load_manager().lock(),
                        secret_document.thumb.clone(),
                        owner_dialog_id,
                        secret_document.thumb_w,
                        secret_document.thumb_h,
                    );
                }
            }
            SourceDocument::Web {
                web_document,
                thumbnail: web_thumbnail,
            } => {
                is_web = true;
                id = i64::from(tdutils::random::fast(0, i32::MAX));
                dc_id_value = 0;
                access_hash = 0;
                if web_thumbnail.size_type == b'v' as i32 {
                    animated_thumbnail.photo_size = web_thumbnail.clone();
                } else {
                    if web_thumbnail.size_type == b'g' as i32 {
                        thumbnail_format = PhotoFormat::Gif;
                    }
                    thumbnail = web_thumbnail.clone();
                }

                match web_document {
                    telegram_api::WebDocument::webDocument(web) => {
                        let r_http_url = tdutils::http_url::parse_url(&web.url);
                        let http_url = match r_http_url {
                            TdResult::Ok(http_url) => http_url,
                            TdResult::Err(_) => {
                                tdutils::log_error!("Can't parse URL {}", web.url);
                                return Document::default();
                            }
                        };

                        access_hash = web.access_hash;
                        url = http_url.get_url();
                        file_name = tdutils::http_url::get_url_query_file_name(&http_url.query);
                        mime_type = web.mime_type.clone();
                        size = i64::from(web.size);
                    }
                    telegram_api::WebDocument::webDocumentNoProxy(web) => {
                        is_web_no_proxy = true;

                        if !web.url.contains('.') {
                            tdutils::log_error!("Receive invalid URL {}", web.url);
                            return Document::default();
                        }

                        url = web.url.clone();
                        file_name = tdutils::http_url::get_url_file_name(&url);
                        mime_type = web.mime_type.clone();
                        size = i64::from(web.size);
                    }
                }

                // do not allow web TGS stickers
                // fix_tgs_sticker_type();
            }
        }
        if document_type == DocumentType::Sticker && mime_type == "video/webm" {
            sticker_format = StickerFormat::Webm;
            default_extension = "webm";
        }
        if file_type == FileType::Encrypted
            && document_type == DocumentType::Sticker
            && size > get_max_sticker_file_size(sticker_format, false, false)
        {
            document_type = DocumentType::General;
        }

        tdutils::log_debug!(
            "Receive document with ID = {id} of type {:?}",
            document_type
        );
        if !is_web && !DcId::is_valid_raw(dc_id_value) {
            tdutils::log_error!("Wrong dc_id = {dc_id_value}");
            return Document::default();
        }

        file_name = crate::misc::strip_empty_characters(&file_name, 255, true);

        let mut suggested_file_name = file_name.clone();
        if suggested_file_name.is_empty() {
            suggested_file_name = (id as u64).to_string();
            let extension = tdutils::mime_type::to_extension_or(&mime_type, default_extension);
            if !extension.is_empty() {
                suggested_file_name.push('.');
                suggested_file_name.push_str(&extension);
            }
        }

        let file_id;
        if !is_web {
            file_id = env.file_manager().lock().register_remote(
                FullRemoteFileLocation::from_document(
                    file_type,
                    id,
                    access_hash,
                    DcId::internal(dc_id_value),
                    file_reference,
                ),
                source_location,
                owner_dialog_id,
                size,
                0,
                suggested_file_name,
            );
            if !encryption_key.empty() {
                env.file_manager()
                    .lock()
                    .set_encryption_key(file_id, encryption_key);
            }
        } else if !is_web_no_proxy {
            file_id = env.file_manager().lock().register_remote(
                FullRemoteFileLocation::from_web(file_type, url, access_hash),
                source_location,
                owner_dialog_id,
                0,
                size,
                file_name.clone(),
            );
        } else {
            let r_file_id = env
                .file_manager()
                .lock()
                .from_persistent_id(&url, file_type);
            file_id = match r_file_id {
                Ok(file_id) => file_id,
                Err(error) => {
                    tdutils::log_error!("Can't register URL: {}", error.message());
                    return Document::default();
                }
            };
        }

        if dimensions.width != 0
            && thumbnail.dimensions.width != 0
            && ((thumbnail.dimensions.width < thumbnail.dimensions.height
                && dimensions.width > dimensions.height)
                || (thumbnail.dimensions.width > thumbnail.dimensions.height
                    && dimensions.width < dimensions.height))
        {
            // fix for wrong dimensions specified by the Android application
            std::mem::swap(&mut dimensions.width, &mut dimensions.height);
        }

        let is_not_web = !is_web;
        match document_type {
            DocumentType::Animation => {
                env.create_animation(
                    file_id,
                    minithumbnail,
                    thumbnail,
                    animated_thumbnail,
                    has_stickers,
                    file_name,
                    mime_type,
                    video_duration,
                    dimensions,
                    is_not_web,
                );
            }
            DocumentType::Audio => {
                let mut duration = 0;
                let mut title = String::new();
                let mut performer = String::new();
                if let Some(audio_attribute) = audio {
                    duration = audio_attribute.duration;
                    title = audio_attribute.title;
                    performer = audio_attribute.performer;
                }
                env.create_audio(
                    file_id,
                    minithumbnail,
                    thumbnail,
                    file_name,
                    mime_type,
                    duration,
                    title,
                    performer,
                    date,
                    is_not_web,
                );
            }
            DocumentType::General => {
                self.create_document(
                    env,
                    file_id,
                    minithumbnail,
                    thumbnail,
                    file_name,
                    mime_type,
                    dimensions,
                    is_not_web,
                );
            }
            DocumentType::Sticker => {
                let minithumbnail = if thumbnail_format == PhotoFormat::Jpeg {
                    String::new()
                } else {
                    minithumbnail
                };
                env.create_sticker(
                    file_id,
                    premium_animation_file_id,
                    minithumbnail,
                    thumbnail,
                    dimensions,
                    sticker,
                    custom_emoji,
                    sticker_format,
                );
            }
            DocumentType::Video => {
                env.create_video(
                    file_id,
                    minithumbnail,
                    thumbnail,
                    animated_thumbnail,
                    has_stickers,
                    Vec::new(),
                    file_name,
                    mime_type,
                    video_duration,
                    video_precise_duration,
                    dimensions,
                    supports_streaming,
                    video_is_animation,
                    video_preload_prefix_size,
                    video_start_ts,
                    video_codec,
                    is_not_web,
                );
            }
            DocumentType::VideoNote => {
                env.create_video_note(
                    file_id,
                    minithumbnail,
                    thumbnail,
                    video_duration,
                    dimensions,
                    video_waveform,
                    is_not_web,
                );
            }
            DocumentType::VoiceNote => {
                let mut duration = 0;
                let mut waveform = String::new();
                if let Some(audio_attribute) = audio {
                    duration = audio_attribute.duration;
                    waveform = String::from_utf8_lossy(&audio_attribute.waveform).into_owned();
                }
                env.create_voice_note(file_id, mime_type, duration, waveform, is_not_web);
            }
            DocumentType::Unknown => unreachable!("UNREACHABLE: unknown document type"),
        }
        Document::new(document_type, file_id)
    }

    /// `DocumentsManager::get_document_thumbnail_file_id`
    /// (DocumentsManager.cpp:729-733).
    pub fn get_document_thumbnail_file_id(&self, file_id: FileId) -> FileId {
        self.get_document(file_id).unwrap().thumbnail.file_id
    }

    /// `DocumentsManager::delete_document_thumbnail`
    /// (DocumentsManager.cpp:735-739).
    pub fn delete_document_thumbnail(&mut self, file_id: FileId) {
        let document = self.documents.get_mut(&file_id).unwrap();
        document.thumbnail = PhotoSize::default();
    }

    /// `DocumentsManager::get_document_file_name` (DocumentsManager.cpp:741-745).
    pub fn get_document_file_name(&self, file_id: FileId) -> &str {
        self.get_document(file_id).unwrap().file_name.as_str()
    }

    /// `DocumentsManager::get_document_mime_type` (DocumentsManager.cpp:747-751).
    pub fn get_document_mime_type(&self, file_id: FileId) -> &str {
        self.get_document(file_id).unwrap().mime_type.as_str()
    }

    /// `DocumentsManager::dup_document` (DocumentsManager.cpp:753-763).
    pub fn dup_document(&mut self, new_id: FileId, old_id: FileId) -> FileId {
        let old_document = self.get_document(old_id).unwrap().clone();
        if self.documents.contains_key(&new_id) {
            return new_id;
        }
        let mut new_document = old_document;
        new_document.file_id = new_id;
        self.documents.insert(new_id, new_document);
        new_id
    }

    /// `DocumentsManager::merge_documents` (DocumentsManager.cpp:765-778).
    pub fn merge_documents(
        &mut self,
        env: &impl DocumentsManagerEnv,
        new_id: FileId,
        old_id: FileId,
    ) {
        assert!(old_id.is_valid() && new_id.is_valid());
        assert!(new_id != old_id);

        self.merge_document_records(new_id, old_id);
        if let Err(error) = env.file_manager().lock().merge(new_id, old_id) {
            tdutils::log_error!("{}", error.message());
        }
    }

    /// The record half of `merge_documents`
    /// (DocumentsManager.cpp:643-653) — the field-split form the Td-side
    /// flip calls; the FileManager merge rides the caller's handle.
    pub fn merge_document_records(&mut self, new_id: FileId, old_id: FileId) {
        assert!(old_id.is_valid() && new_id.is_valid());
        assert!(new_id != old_id);

        tdutils::log_info!("Merge documents {} and {}", new_id.get(), old_id.get());
        assert!(self.get_document(old_id).is_some());

        if self.get_document(new_id).is_none() {
            self.dup_document(new_id, old_id);
        }
    }

    /// `DocumentsManager::get_document_search_text` (DocumentsManager.cpp:780-799)
    /// — the file name plus its UTF-8 stem prefixes.
    pub fn get_document_search_text(&self, file_id: FileId) -> String {
        let document = self.get_document(file_id).unwrap();
        if document.file_name.len() > 32 {
            return document.file_name.clone();
        }
        let mut result = document.file_name.clone();
        let path_view = tdutils::path_view::PathView::new(&document.file_name);
        let stem = path_view.file_stem();
        let mut i = 1;
        while i + 1 < stem.len() {
            if is_utf8_character_first_code_unit(stem.as_bytes()[i]) {
                result.push(' ');
                result.push_str(&stem[..i]);
            }
            i += 1;
        }
        result
    }

    /// `DocumentsManager::get_input_media` (DocumentsManager.cpp:689-730).
    pub fn get_input_media(
        &self,
        env: &impl DocumentsManagerEnv,
        file_id: FileId,
        input_file: Option<telegram_api::InputFile>,
        input_thumbnail: Option<telegram_api::InputFile>,
    ) -> Option<telegram_api::InputMedia> {
        let file_manager = env.file_manager();
        let file_manager = file_manager.lock();
        let file_view = file_manager.get_file_view(file_id)?;
        if file_view.is_encrypted() {
            return None;
        }
        if let Some(main_remote_location) = file_view.get_main_remote_location()
            && !main_remote_location.is_web()
            && input_file.is_none()
        {
            return Some(telegram_api::InputMedia::inputMediaDocument(
                telegram_api::inputMediaDocument {
                    flags: 0,
                    spoiler: false,
                    id: Some(as_input_document(main_remote_location)),
                    video_cover: None,
                    video_timestamp: 0,
                    ttl_seconds: 0,
                    query: String::new(),
                },
            ));
        }
        if let Some(url) = file_view.get_url() {
            return Some(telegram_api::InputMedia::inputMediaDocumentExternal(
                telegram_api::inputMediaDocumentExternal {
                    flags: 0,
                    spoiler: false,
                    url: url.to_owned(),
                    ttl_seconds: 0,
                    video_cover: None,
                    video_timestamp: 0,
                },
            ));
        }

        if let Some(input_file) = input_file {
            let document = self.get_document(file_id)?;
            let mut attributes: Vec<telegram_api::DocumentAttribute> = Vec::new();
            if !document.file_name.is_empty() {
                attributes.push(telegram_api::DocumentAttribute::documentAttributeFilename(
                    telegram_api::documentAttributeFilename {
                        file_name: document.file_name.clone(),
                    },
                ));
            }
            let mut flags = 0;
            const UPLOADED_THUMB_MASK: i32 = 4;
            if input_thumbnail.is_some() {
                flags |= UPLOADED_THUMB_MASK;
            }
            return Some(telegram_api::InputMedia::inputMediaUploadedDocument(
                telegram_api::inputMediaUploadedDocument {
                    flags,
                    nosound_video: false,
                    force_file: file_view.get_type() == FileType::DocumentAsFile,
                    spoiler: false,
                    file: Some(input_file),
                    thumb: input_thumbnail,
                    mime_type: document.mime_type.clone(),
                    attributes,
                    stickers: Vec::new(),
                    video_cover: None,
                    video_timestamp: 0,
                    ttl_seconds: 0,
                },
            ));
        }
        None
    }

    /// `DocumentsManager::store_document` (DocumentsManager.hpp:20-51) —
    /// the FileIds ride the [`photo::DialogPhotoStoreEnv`] codec; every
    /// other field is byte-exact with the C++.
    pub fn store_document(
        &self,
        file_id: FileId,
        env: &mut impl photo::DialogPhotoStoreEnv,
        out: &mut Vec<u8>,
    ) {
        use crate::store_parse::{store_flags, store_string};
        let document = self.get_document(file_id).unwrap();
        let has_file_name = !document.file_name.is_empty();
        let has_mime_type = !document.mime_type.is_empty();
        let has_minithumbnail = !document.minithumbnail.is_empty();
        let has_thumbnail = document.thumbnail.file_id.is_valid();
        let has_dimensions = document.dimensions != Dimensions::default();
        store_flags(
            out,
            [
                has_file_name,
                has_mime_type,
                has_minithumbnail,
                has_thumbnail,
                has_dimensions,
            ],
        );
        if has_file_name {
            store_string(out, &document.file_name);
        }
        if has_mime_type {
            store_string(out, &document.mime_type);
        }
        if has_minithumbnail {
            store_string(out, &document.minithumbnail);
        }
        if has_thumbnail {
            document.thumbnail.store(env, out);
        }
        if has_dimensions {
            document.dimensions.store(out);
        }
        env.store_file_id(file_id, out);
    }

    /// `DocumentsManager::parse_document` (DocumentsManager.hpp:53-96) —
    /// pre-47 blobs assume name/mime/thumbnail present and no
    /// dimensions; a failed or invalid-id parse answers the default id.
    pub fn parse_document(
        &mut self,
        env: &mut impl photo::DialogPhotoStoreEnv,
        parser: &mut Parser<'_>,
    ) -> FileId {
        let mut document = GeneralDocument::default();
        let has_file_name;
        let has_mime_type;
        let has_minithumbnail;
        let has_thumbnail;
        let has_dimensions;
        if parser.version() >= store_parse::version::ADD_DOCUMENT_FLAGS {
            let flags = parser.parse_flags::<5>();
            let Ok(
                [
                    h_file_name,
                    h_mime_type,
                    h_minithumbnail,
                    h_thumbnail,
                    h_dimensions,
                ],
            ) = flags
            else {
                return FileId::default();
            };
            has_file_name = h_file_name;
            has_mime_type = h_mime_type;
            has_minithumbnail = h_minithumbnail;
            has_thumbnail = h_thumbnail;
            has_dimensions = h_dimensions;
        } else {
            has_file_name = true;
            has_mime_type = true;
            has_minithumbnail = parser.version() >= store_parse::version::SUPPORT_MINITHUMBNAILS;
            has_thumbnail = true;
            has_dimensions = false;
        }
        if has_file_name {
            match parser.string() {
                Ok(value) => document.file_name = value,
                Err(_) => return FileId::default(),
            }
        }
        if has_mime_type {
            match parser.string() {
                Ok(value) => document.mime_type = value,
                Err(_) => return FileId::default(),
            }
        }
        if has_minithumbnail {
            match parser.string() {
                Ok(value) => document.minithumbnail = value,
                Err(_) => return FileId::default(),
            }
        }
        if has_thumbnail {
            match PhotoSize::parse_from(env, parser) {
                Ok(value) => document.thumbnail = value,
                Err(_) => return FileId::default(),
            }
        }
        if has_dimensions {
            match Dimensions::parse_from(parser) {
                Ok(value) => document.dimensions = value,
                Err(_) => return FileId::default(),
            }
        }
        match env.parse_file_id(parser) {
            Ok(value) => document.file_id = value,
            Err(_) => return FileId::default(),
        }
        if !document.file_id.is_valid() {
            return FileId::default();
        }
        self.on_get_document_record(document, false)
    }
}

/// `td::is_utf8_character_first_code_unit` (utf8.h) — a byte that starts
/// a UTF-8 character (not a continuation byte).
fn is_utf8_character_first_code_unit(first_byte: u8) -> bool {
    first_byte & 0xC0 != 0x80
}

/// `FullRemoteFileLocation::as_input_document` (FileLocation.h) — the
/// animations_manager local copy; the caller validated `is_document`.
fn as_input_document(
    location: &crate::files::FullRemoteFileLocation,
) -> telegram_api::InputDocument {
    debug_assert!(location.is_document());
    telegram_api::InputDocument::inputDocument(telegram_api::inputDocument {
        id: location.get_id(),
        access_hash: location.get_access_hash(),
        file_reference: location.get_file_reference().as_bytes().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::FileManager;
    use std::sync::Arc;

    struct TestEnv {
        files: Arc<parking_lot::Mutex<FileManager>>,
        loads: Arc<parking_lot::Mutex<FileLoadManager>>,
    }

    /// The download/upload net is never driven by these tests — the
    /// `FileLoadManager` only backs the `set_content` tail of the cached
    /// photo-size intake.
    struct StubLoadNet;
    impl crate::files::FileDownloaderNet for StubLoadNet {
        fn download(
            &mut self,
            _query: crate::files::DownloadQuery,
            _query_type: crate::files::DownloadQueryType,
            _meta: crate::files::DownloadNetMeta,
        ) -> std::result::Result<crate::files::DownloadResult, tdutils::status::Status> {
            Err(tdutils::status::Status::error(400, "Not connected"))
        }
        fn open_temp_file(
            &mut self,
            _file_type: FileType,
        ) -> std::result::Result<(std::fs::File, String), tdutils::status::Status> {
            Err(tdutils::status::Status::error_message(
                "no temp files in this test",
            ))
        }
        fn open_partial_file(
            &mut self,
            _path: &str,
            _write: bool,
        ) -> std::result::Result<std::fs::File, tdutils::status::Status> {
            Err(tdutils::status::Status::error_message(
                "no partial files in this test",
            ))
        }
        fn create_from_temp(
            &mut self,
            _file_type: FileType,
            _path: &str,
            _name: &str,
        ) -> std::result::Result<String, tdutils::status::Status> {
            Err(tdutils::status::Status::error_message(
                "no temp files in this test",
            ))
        }
    }
    impl crate::files::FileLoadNetFactory for StubLoadNet {
        fn download_net(&mut self) -> Box<dyn crate::files::FileDownloaderNet> {
            Box::new(StubLoadNet)
        }
        fn upload_net(&mut self) -> Box<dyn crate::files::FileUploaderNet> {
            unreachable!("no uploads in these tests")
        }
        fn hash_net(&mut self) -> Box<dyn crate::files::FileHashUploaderNet> {
            unreachable!("no hash uploads in these tests")
        }
        fn utils_net(&mut self) -> Box<dyn crate::files::FileDownloaderNet> {
            self.download_net()
        }
    }

    impl crate::photo::DialogPhotoStoreEnv for TestEnv {
        fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            self.files.lock().store_file(file_id, out);
        }

        fn parse_file_id(&mut self, parser: &mut Parser<'_>) -> Result<FileId, &'static str> {
            self.files.lock().parse_file(parser)
        }
    }

    impl DocumentsManagerEnv for TestEnv {
        fn auth_is_bot(&self) -> bool {
            false
        }

        fn file_manager(&self) -> Arc<parking_lot::Mutex<FileManager>> {
            Arc::clone(&self.files)
        }

        fn file_load_manager(&self) -> Arc<parking_lot::Mutex<FileLoadManager>> {
            Arc::clone(&self.loads)
        }

        fn create_animation(
            &mut self,
            _file_id: FileId,
            _minithumbnail: String,
            _thumbnail: PhotoSize,
            _animated_thumbnail: AnimationSize,
            _has_stickers: bool,
            _file_name: String,
            _mime_type: String,
            _duration: i32,
            _dimensions: Dimensions,
            _replace: bool,
        ) {
        }

        fn create_sticker(
            &mut self,
            _file_id: FileId,
            _premium_animation_file_id: FileId,
            _minithumbnail: String,
            _thumbnail: PhotoSize,
            _dimensions: Dimensions,
            _sticker: Option<telegram_api::documentAttributeSticker>,
            _custom_emoji: Option<telegram_api::documentAttributeCustomEmoji>,
            _sticker_format: StickerFormat,
        ) {
        }

        fn create_audio(
            &mut self,
            _file_id: FileId,
            _minithumbnail: String,
            _thumbnail: PhotoSize,
            _file_name: String,
            _mime_type: String,
            _duration: i32,
            _title: String,
            _performer: String,
            _date: i32,
            _replace: bool,
        ) {
        }

        fn create_video(
            &mut self,
            _file_id: FileId,
            _minithumbnail: String,
            _thumbnail: PhotoSize,
            _animated_thumbnail: AnimationSize,
            _has_stickers: bool,
            _sticker_file_ids: Vec<FileId>,
            _file_name: String,
            _mime_type: String,
            _duration: i32,
            _precise_duration: f64,
            _dimensions: Dimensions,
            _supports_streaming: bool,
            _is_animation: bool,
            _preload_prefix_size: i32,
            _start_ts: f64,
            _codec: String,
            _replace: bool,
        ) {
        }

        fn create_video_note(
            &mut self,
            _file_id: FileId,
            _minithumbnail: String,
            _thumbnail: PhotoSize,
            _duration: i32,
            _dimensions: Dimensions,
            _waveform: String,
            _replace: bool,
        ) {
        }

        fn create_voice_note(
            &mut self,
            _file_id: FileId,
            _mime_type: String,
            _duration: i32,
            _waveform: String,
            _replace: bool,
        ) {
        }
    }

    fn env() -> TestEnv {
        TestEnv {
            files: Arc::new(parking_lot::Mutex::new(FileManager::new())),
            loads: Arc::new(parking_lot::Mutex::new(crate::files::FileLoadManager::new(
                Box::new(StubLoadNet),
                crate::files::FileLoadManagerOptions::default(),
            ))),
        }
    }

    impl TestEnv {
        /// The store/parse plane round-trips only registered file ids
        /// (the C++ records always carry FileManager-backed ids).
        fn register_file(&self, remote_document_id: i64) -> FileId {
            self.files.lock().register_remote(
                crate::files::FullRemoteFileLocation::from_document(
                    FileType::Document,
                    remote_document_id,
                    42,
                    crate::chat_manager::DcId::internal(2),
                    String::new(),
                ),
                crate::files::FileLocationSource::FromServer,
                crate::message_id::DialogId::default(),
                1000,
                0,
                format!("doc{remote_document_id}.bin"),
            )
        }
    }

    #[test]
    fn create_and_accessors() {
        let mut manager = DocumentsManager::new();
        let env = env();
        manager.create_document(
            &env,
            FileId::new(1, 0),
            String::new(),
            PhotoSize::default(),
            "a.txt".to_owned(),
            "text/plain".to_owned(),
            Dimensions::default(),
            false,
        );
        assert_eq!(manager.get_document_file_name(FileId::new(1, 0)), "a.txt");
        assert_eq!(
            manager.get_document_mime_type(FileId::new(1, 0)),
            "text/plain"
        );
        assert_eq!(manager.get_document_search_text(FileId::new(1, 0)), "a.txt");
        assert_eq!(
            manager.dup_document(FileId::new(2, 0), FileId::new(1, 0)),
            FileId::new(2, 0)
        );
        manager.merge_documents(&env, FileId::new(3, 0), FileId::new(2, 0));
        assert_eq!(
            manager.get_document_mime_type(FileId::new(3, 0)),
            "text/plain"
        );
    }

    #[test]
    fn store_parse_round_trips() {
        let mut manager = DocumentsManager::new();
        let mut env = env();
        let file_id = env.register_file(5);
        manager.create_document(
            &env,
            file_id,
            "mini".to_owned(),
            PhotoSize::default(),
            "f.bin".to_owned(),
            "application/octet-stream".to_owned(),
            Dimensions {
                width: 1,
                height: 2,
            },
            false,
        );
        let mut out = Vec::new();
        manager.store_document(file_id, &mut env, &mut out);

        let mut manager2 = DocumentsManager::new();
        let mut parser = Parser::new(&out);
        let parsed_file_id = manager2.parse_document(&mut env, &mut parser);
        assert_eq!(parsed_file_id, file_id);
        assert_eq!(
            manager2.get_document_mime_type(parsed_file_id),
            manager.get_document_mime_type(file_id)
        );
    }
}
