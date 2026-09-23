use crate::files::{FileId, FileManager};
use crate::photo::{self, DialogPhotoStoreEnv, Dimensions, PhotoSize};
use crate::store_parse::{Parser, store_flags, store_string};
use crate::td_api;
use std::collections::HashMap;
use std::sync::Arc;
use tdmtproto::telegram_api;

/// The `Td *td` slice of the collapsed VideoNotesManager.
pub trait VideoNotesManagerEnv: DialogPhotoStoreEnv {
    fn file_manager(&self) -> Arc<parking_lot::Mutex<FileManager>>;

    /// The `td_->option_manager_->get_option_integer` read of the
    /// round-video suggested length.
    fn get_option_integer(&mut self, name: &str, default_value: i64) -> i64;
}

/// `VideoNotesManager::VideoNote` (VideoNotesManager.h:68-79).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VideoNote {
    pub duration: i32,
    pub dimensions: Dimensions,
    pub waveform: String,
    pub minithumbnail: String,
    pub thumbnail: PhotoSize,
    pub file_id: FileId,
}

/// `VideoNotesManager` (VideoNotesManager.h:27-94).
#[derive(Default)]
pub struct VideoNotesManager {
    pub(crate) video_notes: HashMap<FileId, VideoNote>,
}

impl VideoNotesManager {
    pub fn new() -> VideoNotesManager {
        VideoNotesManager::default()
    }

    /// `VideoNotesManager::get_video_note` (VideoNotesManager.cpp:83-87).
    pub fn get_video_note(&self, file_id: FileId) -> Option<&VideoNote> {
        self.video_notes.get(&file_id)
    }

    /// `VideoNotesManager::get_video_note_duration`
    /// (VideoNotesManager.cpp:89-93).
    pub fn get_video_note_duration(&self, file_id: FileId) -> i32 {
        self.get_video_note(file_id).map_or(0, |note| note.duration)
    }

    /// `VideoNotesManager::create_video_note`
    /// (VideoNotesManager.cpp:148-174).
    /// `VideoNotesManager::create_video_note`
    /// (VideoNotesManager.cpp:120-139).
    #[allow(clippy::too_many_arguments)]
    pub fn create_video_note(
        &mut self,
        file_id: FileId,
        minithumbnail: String,
        thumbnail: PhotoSize,
        duration: i32,
        dimensions: Dimensions,
        waveform: String,
        replace: bool,
    ) {
        let note = VideoNote {
            duration,
            dimensions,
            waveform,
            minithumbnail,
            thumbnail,
            file_id,
        };
        if replace || !self.video_notes.contains_key(&file_id) {
            self.video_notes.insert(file_id, note);
        }
    }

    /// `VideoNotesManager::merge_video_notes`
    /// (VideoNotesManager.cpp:133-146).
    pub fn merge_video_notes(&mut self, new_id: FileId, old_id: FileId) {
        if new_id == old_id {
            return;
        }
        if let Some(old_note) = self.video_notes.remove(&old_id) {
            self.video_notes.entry(new_id).or_insert(old_note);
        }
    }

    /// `VideoNotesManager::delete_video_note_thumbnail`
    /// (VideoNotesManager.cpp:109-113).
    pub fn delete_video_note_thumbnail(&mut self, file_id: FileId) {
        if let Some(note) = self.video_notes.get_mut(&file_id) {
            note.thumbnail = PhotoSize::default();
        }
    }

    /// `VideoNotesManager::get_video_note_object`
    /// (VideoNotesManager.cpp:176-200).
    pub fn get_video_note_object(
        &self,
        env: &impl VideoNotesManagerEnv,
        file_id: FileId,
    ) -> Option<td_api::VideoNote> {
        if !file_id.is_valid() {
            return None;
        }
        let note = self.get_video_note(file_id)?;
        Some(td_api::VideoNote {
            duration: note.duration,
            waveform: note.waveform.as_bytes().to_vec(),
            length: note.dimensions.height as i32,
            minithumbnail: Some(photo::get_minithumbnail_object(
                note.minithumbnail.as_bytes(),
            )),
            thumbnail: photo::get_thumbnail_object(
                &note.thumbnail,
                crate::photo::PhotoFormat::Jpeg,
            ),
            speech_recognition_result: None,
            video: Some(env.file_manager().lock().get_file_object(file_id)),
        })
    }

    /// `VideoNotesManager::get_video_note_thumbnail_file_id`
    /// (VideoNotesManager.cpp:103-107).
    pub fn get_video_note_thumbnail_file_id(&self, file_id: FileId) -> FileId {
        self.get_video_note(file_id)
            .map_or(FileId::default(), |note| note.thumbnail.file_id)
    }

    /// `VideoNotesManager::dup_video_note` (VideoNotesManager.cpp:115-130).
    /// The TranscriptionInfo copy rides the M.25 transcription seam.
    pub fn dup_video_note(&mut self, new_id: FileId, old_id: FileId) -> FileId {
        if let Some(mut note) = self.video_notes.get(&old_id).cloned() {
            note.file_id = new_id;
            self.video_notes.entry(new_id).or_insert(note);
            new_id
        } else {
            FileId::default()
        }
    }

    /// `VideoNotesManager::get_input_media`
    /// (VideoNotesManager.cpp:199-248).
    pub fn get_input_media(
        &self,
        env: &mut impl VideoNotesManagerEnv,
        file_id: FileId,
        input_file: Option<telegram_api::InputFile>,
        input_thumbnail: Option<telegram_api::InputFile>,
        ttl: i32,
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
            let mut flags = 0;
            const INPUT_MEDIA_DOCUMENT_TTL_SECONDS_MASK: i32 = 1;
            if ttl != 0 {
                flags |= INPUT_MEDIA_DOCUMENT_TTL_SECONDS_MASK;
            }
            return Some(telegram_api::InputMedia::inputMediaDocument(
                telegram_api::inputMediaDocument {
                    flags,
                    spoiler: false,
                    id: Some(as_input_document(main_remote_location)),
                    video_cover: None,
                    video_timestamp: 0,
                    ttl_seconds: ttl,
                    query: String::new(),
                },
            ));
        }
        if let Some(url) = file_view.get_url() {
            let mut flags = 0;
            const INPUT_MEDIA_DOCUMENT_EXTERNAL_TTL_SECONDS_MASK: i32 = 1;
            if ttl != 0 {
                flags |= INPUT_MEDIA_DOCUMENT_EXTERNAL_TTL_SECONDS_MASK;
            }
            return Some(telegram_api::InputMedia::inputMediaDocumentExternal(
                telegram_api::inputMediaDocumentExternal {
                    flags,
                    spoiler: false,
                    url: url.to_owned(),
                    ttl_seconds: ttl,
                    video_cover: None,
                    video_timestamp: 0,
                },
            ));
        }

        if let Some(input_file) = input_file {
            let video_note = self.get_video_note(file_id)?;
            let mut attributes: Vec<telegram_api::DocumentAttribute> = Vec::new();
            let suggested_video_note_length =
                env.get_option_integer("suggested_video_note_length", 384);
            attributes.push(telegram_api::DocumentAttribute::documentAttributeVideo(
                telegram_api::documentAttributeVideo {
                    flags: 0,
                    round_message: true,
                    supports_streaming: false,
                    nosound: false,
                    duration: video_note.duration as f64,
                    w: if video_note.dimensions.width != 0 {
                        i32::from(video_note.dimensions.width)
                    } else {
                        suggested_video_note_length as i32
                    },
                    h: if video_note.dimensions.height != 0 {
                        i32::from(video_note.dimensions.height)
                    } else {
                        suggested_video_note_length as i32
                    },
                    preload_prefix_size: 0,
                    video_start_ts: 0.0,
                    video_codec: String::new(),
                },
            ));
            let mut flags = 0;
            const UPLOADED_TTL_SECONDS_MASK: i32 = 2;
            const UPLOADED_THUMB_MASK: i32 = 4;
            if ttl != 0 {
                flags |= UPLOADED_TTL_SECONDS_MASK;
            }
            if input_thumbnail.is_some() {
                flags |= UPLOADED_THUMB_MASK;
            }
            return Some(telegram_api::InputMedia::inputMediaUploadedDocument(
                telegram_api::inputMediaUploadedDocument {
                    flags,
                    nosound_video: true,
                    force_file: false,
                    spoiler: false,
                    file: Some(input_file),
                    thumb: input_thumbnail,
                    mime_type: "video/mp4".to_owned(),
                    attributes,
                    stickers: Vec::new(),
                    video_cover: None,
                    video_timestamp: 0,
                    ttl_seconds: ttl,
                },
            ));
        }
        None
    }

    /// `VideoNotesManager::store_video_note`
    /// (VideoNotesManager.hpp:21-53). The `is_transcribed` flag plane is
    /// written `false` — the TranscriptionInfo record rides the M.25
    /// transcription seam.
    pub fn store_video_note(
        &self,
        env: &mut impl VideoNotesManagerEnv,
        file_id: FileId,
        out: &mut Vec<u8>,
    ) {
        let note = self.get_video_note(file_id).expect("video note not found");
        let has_duration = note.duration != 0;
        let has_minithumbnail = !note.minithumbnail.is_empty();
        let has_thumbnail = note.thumbnail.file_id.is_valid();
        let is_transcribed = false;
        let has_waveform = !note.waveform.is_empty();
        store_flags(
            out,
            [
                has_duration,
                has_minithumbnail,
                has_thumbnail,
                is_transcribed,
                has_waveform,
            ],
        );
        if has_duration {
            crate::store_parse::store_i32(out, note.duration);
        }
        note.dimensions.store(out);
        if has_minithumbnail {
            store_string(out, &note.minithumbnail);
        }
        if has_thumbnail {
            note.thumbnail.store(env, out);
        }
        if has_waveform {
            store_string(out, &note.waveform);
        }
        env.store_file_id(file_id, out);
    }

    /// `VideoNotesManager::parse_video_note`
    /// (VideoNotesManager.hpp:56-99).
    pub fn parse_video_note(
        &mut self,
        env: &mut impl VideoNotesManagerEnv,
        parser: &mut Parser<'_>,
    ) -> Result<FileId, &'static str> {
        let (has_duration, has_minithumbnail, has_thumbnail, is_transcribed, has_waveform) =
            if parser.version() >= crate::store_parse::version::ADD_VIDEO_NOTE_FLAGS {
                let flags = parser.parse_flags::<5>()?;
                (flags[0], flags[1], flags[2], flags[3], flags[4])
            } else {
                (
                    true,
                    parser.version() >= crate::store_parse::version::SUPPORT_MINITHUMBNAILS,
                    true,
                    false,
                    false,
                )
            };
        if is_transcribed {
            // the TranscriptionInfo record rides the M.25 transcription
            // seam; this port never writes the flag
            return Err("Transcribed video notes ride the M.25 transcription seam");
        }
        let duration = if has_duration { parser.i32_()? } else { 0 };
        let dimensions = Dimensions::parse_from(parser)?;
        let minithumbnail = if has_minithumbnail {
            parser.string()?
        } else {
            String::new()
        };
        let thumbnail = if has_thumbnail {
            PhotoSize::parse_from(env, parser)?
        } else {
            PhotoSize::default()
        };
        let waveform = if has_waveform {
            parser.string()?
        } else {
            String::new()
        };
        let file_id = env.parse_file_id(parser)?;
        self.video_notes.insert(
            file_id,
            VideoNote {
                duration,
                dimensions,
                waveform,
                minithumbnail,
                thumbnail,
                file_id,
            },
        );
        Ok(file_id)
    }
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
    }

    impl crate::photo::DialogPhotoStoreEnv for TestEnv {
        fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            self.files.lock().store_file(file_id, out);
        }

        fn parse_file_id(&mut self, parser: &mut Parser<'_>) -> Result<FileId, &'static str> {
            self.files.lock().parse_file(parser)
        }
    }

    impl VideoNotesManagerEnv for TestEnv {
        fn file_manager(&self) -> Arc<parking_lot::Mutex<FileManager>> {
            Arc::clone(&self.files)
        }

        fn get_option_integer(&mut self, _name: &str, default_value: i64) -> i64 {
            default_value
        }
    }

    impl TestEnv {
        /// The store/parse plane round-trips only registered file ids
        /// (the C++ records always carry FileManager-backed ids).
        fn register_file(&self, remote_document_id: i64) -> FileId {
            self.files.lock().register_remote(
                crate::files::FullRemoteFileLocation::from_document(
                    crate::files::FileType::VideoNote,
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

    fn env() -> TestEnv {
        TestEnv {
            files: Arc::new(parking_lot::Mutex::new(FileManager::new())),
        }
    }

    #[test]
    fn create_and_merge() {
        let mut manager = VideoNotesManager::new();
        manager.create_video_note(
            FileId::new(1, 0),
            String::new(),
            PhotoSize::default(),
            6,
            Dimensions {
                width: 200,
                height: 200,
            },
            "wv".to_owned(),
            false,
        );
        assert_eq!(manager.get_video_note_duration(FileId::new(1, 0)), 6);
        manager.merge_video_notes(FileId::new(2, 0), FileId::new(1, 0));
        assert_eq!(manager.get_video_note_duration(FileId::new(2, 0)), 6);
    }

    #[test]
    fn store_parse_round_trips() {
        let mut manager = VideoNotesManager::new();
        let mut env = env();
        let file_id = env.register_file(3);
        manager.create_video_note(
            file_id,
            "mini".to_owned(),
            PhotoSize::default(),
            9,
            Dimensions {
                width: 200,
                height: 200,
            },
            "w".to_owned(),
            false,
        );
        let mut out = Vec::new();
        manager.store_video_note(&mut env, file_id, &mut out);

        let mut manager2 = VideoNotesManager::new();
        let mut parser = Parser::new(&out);
        let parsed_file_id = manager2.parse_video_note(&mut env, &mut parser).unwrap();
        assert_eq!(parsed_file_id, file_id);
        assert_eq!(
            manager2.get_video_note(parsed_file_id),
            manager.get_video_note(file_id)
        );
    }
}
