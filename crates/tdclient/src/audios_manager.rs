use crate::files::{FileId, FileManager, FileType};
use crate::photo::{self, DialogPhotoStoreEnv, PhotoSize};
use crate::store_parse::{Parser, store_flags, store_string};
use crate::td_api;
use std::collections::HashMap;
use std::sync::Arc;
use tdmtproto::telegram_api;

/// The `Td *td` slice of the collapsed AudiosManager.
pub trait AudiosManagerEnv: DialogPhotoStoreEnv {
    fn file_manager(&self) -> Arc<parking_lot::Mutex<FileManager>>;
}

/// `AudiosManager::Audio` (AudiosManager.h:70-90).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Audio {
    pub file_name: String,
    pub mime_type: String,
    pub duration: i32,
    pub date: i32,
    pub title: String,
    pub performer: String,
    pub minithumbnail: String,
    pub thumbnail: PhotoSize,
    pub file_id: FileId,
}

/// `AudiosManager` (AudiosManager.h:23-92).
#[derive(Default)]
pub struct AudiosManager {
    pub(crate) audios: HashMap<FileId, Audio>,
}

impl AudiosManager {
    pub fn new() -> AudiosManager {
        AudiosManager::default()
    }

    /// `AudiosManager::get_audio` (AudiosManager.cpp:113-117).
    pub fn get_audio(&self, file_id: FileId) -> Option<&Audio> {
        self.audios.get(&file_id)
    }

    /// `AudiosManager::get_audio_duration` (AudiosManager.cpp:119-123).
    pub fn get_audio_duration(&self, file_id: FileId) -> i32 {
        self.get_audio(file_id).map_or(0, |audio| audio.duration)
    }

    /// `AudiosManager::create_audio` (AudiosManager.cpp:201-230).
    #[allow(clippy::too_many_arguments)]
    pub fn create_audio(
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
    ) {
        let audio = Audio {
            file_name,
            mime_type,
            duration,
            date,
            title,
            performer,
            minithumbnail,
            thumbnail,
            file_id,
        };
        if replace || !self.audios.contains_key(&file_id) {
            self.audios.insert(file_id, audio);
        }
    }

    /// `AudiosManager::merge_audios` (AudiosManager.cpp:145-174).
    pub fn merge_audios(&mut self, new_id: FileId, old_id: FileId) {
        if new_id == old_id {
            return;
        }
        if let Some(old_audio) = self.audios.remove(&old_id) {
            self.audios.entry(new_id).or_insert(old_audio);
        }
    }

    /// `AudiosManager::delete_audio_thumbnail` (AudiosManager.cpp:195-199).
    pub fn delete_audio_thumbnail(&mut self, file_id: FileId) {
        if let Some(audio) = self.audios.get_mut(&file_id) {
            audio.thumbnail = PhotoSize::default();
        }
    }

    /// `AudiosManager::get_audio_object` (AudiosManager.cpp:232-270).
    pub fn get_audio_object(
        &self,
        env: &impl AudiosManagerEnv,
        file_id: FileId,
    ) -> Option<td_api::Audio> {
        if !file_id.is_valid() {
            return None;
        }
        let audio = self.get_audio(file_id)?;
        Some(td_api::Audio {
            duration: audio.duration,
            title: audio.title.clone(),
            performer: audio.performer.clone(),
            file_name: audio.file_name.clone(),
            mime_type: audio.mime_type.clone(),
            album_cover_minithumbnail: Some(photo::get_minithumbnail_object(
                audio.minithumbnail.as_bytes(),
            )),
            album_cover_thumbnail: photo::get_thumbnail_object(
                &audio.thumbnail,
                crate::photo::PhotoFormat::Jpeg,
            ),
            external_album_covers: Vec::new(),
            audio: Some(env.file_manager().lock().get_file_object(file_id)),
        })
    }

    /// `AudiosManager::get_notification_sound_object`
    /// (AudiosManager.cpp:75-94). The C++ `nullptr` for the invalid id is
    /// the module's empty-struct null collapse.
    pub fn get_notification_sound_object(
        &self,
        env: &impl AudiosManagerEnv,
        file_id: FileId,
    ) -> td_api::NotificationSound {
        if !file_id.is_valid() {
            return td_api::NotificationSound {
                id: 0,
                duration: 0,
                date: 0,
                title: String::new(),
                data: String::new(),
                sound: None,
            };
        }
        let audio = self
            .get_audio(file_id)
            .expect("get_notification_sound_object: missing audio");
        let file_manager = env.file_manager();
        let file_manager = file_manager.lock();
        let file_view = file_manager
            .get_file_view(file_id)
            .expect("get_notification_sound_object: empty file view");
        assert_eq!(file_view.get_type(), FileType::Ringtone);
        let document_id = file_view
            .get_full_remote_location()
            .expect("get_notification_sound_object: no full remote location")
            .get_id();
        drop(file_manager);
        let mut title = audio.title.clone();
        if title.is_empty() && !audio.file_name.is_empty() {
            // the C++ `PathView(file_name).file_name_without_extension()`
            title = tdutils::path_view::PathView::new(&audio.file_name)
                .file_stem()
                .to_owned();
        }
        td_api::NotificationSound {
            id: document_id,
            duration: audio.duration,
            date: audio.date,
            title,
            data: audio.performer.clone(),
            sound: Some(env.file_manager().lock().get_file_object(file_id)),
        }
    }

    /// `AudiosManager::get_notification_sounds_object`
    /// (AudiosManager.cpp:312-325).
    pub fn get_notification_sounds_object(
        &self,
        env: &impl AudiosManagerEnv,
        file_ids: &[FileId],
    ) -> td_api::NotificationSounds {
        td_api::NotificationSounds {
            notification_sounds: file_ids
                .iter()
                .map(|&file_id| self.get_notification_sound_object(env, file_id))
                .collect(),
        }
    }

    /// `AudiosManager::get_input_media` (AudiosManager.cpp:256-300).
    pub fn get_input_media(
        &self,
        env: &impl AudiosManagerEnv,
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
            let audio = self.get_audio(file_id)?;
            let mut attributes: Vec<telegram_api::DocumentAttribute> = Vec::new();
            // the pinned scheme: TITLE_MASK = 1, PERFORMER_MASK = 2
            const TITLE_MASK: i32 = 1;
            const PERFORMER_MASK: i32 = 2;
            attributes.push(telegram_api::DocumentAttribute::documentAttributeAudio(
                telegram_api::documentAttributeAudio {
                    flags: TITLE_MASK | PERFORMER_MASK,
                    voice: false,
                    duration: audio.duration,
                    title: audio.title.clone(),
                    performer: audio.performer.clone(),
                    waveform: Vec::new(),
                },
            ));
            if !audio.file_name.is_empty() {
                attributes.push(telegram_api::DocumentAttribute::documentAttributeFilename(
                    telegram_api::documentAttributeFilename {
                        file_name: audio.file_name.clone(),
                    },
                ));
            }
            let mut mime_type = audio.mime_type.clone();
            if !mime_type.starts_with("audio/") {
                mime_type = "audio/mpeg".to_owned();
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
                    force_file: false,
                    spoiler: false,
                    file: Some(input_file),
                    thumb: input_thumbnail,
                    mime_type,
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

    /// `AudiosManager::get_audio_thumbnail_file_id`
    /// (AudiosManager.cpp:170-174).
    pub fn get_audio_thumbnail_file_id(&self, file_id: FileId) -> FileId {
        self.get_audio(file_id)
            .map_or(FileId::default(), |audio| audio.thumbnail.file_id)
    }

    /// `AudiosManager::dup_audio` (AudiosManager.cpp:133-143).
    pub fn dup_audio(&mut self, new_id: FileId, old_id: FileId) -> FileId {
        if let Some(mut audio) = self.audios.get(&old_id).cloned() {
            audio.file_id = new_id;
            self.audios.entry(new_id).or_insert(audio);
            new_id
        } else {
            FileId::default()
        }
    }

    /// `AudiosManager::get_audio_search_text` (AudiosManager.cpp:164-168).
    pub fn get_audio_search_text(&self, file_id: FileId) -> String {
        self.get_audio(file_id).map_or_else(String::new, |audio| {
            format!("{} {} {}", audio.file_name, audio.title, audio.performer)
        })
    }

    /// `AudiosManager::store_audio` (AudiosManager.hpp:22-74).
    pub fn store_audio(&self, env: &mut impl AudiosManagerEnv, file_id: FileId, out: &mut Vec<u8>) {
        let audio = self.get_audio(file_id).expect("audio not found");
        let has_file_name = !audio.file_name.is_empty();
        let has_mime_type = !audio.mime_type.is_empty();
        let has_duration = audio.duration != 0;
        let has_title = !audio.title.is_empty();
        let has_performer = !audio.performer.is_empty();
        let has_minithumbnail = !audio.minithumbnail.is_empty();
        let has_thumbnail = audio.thumbnail.file_id.is_valid();
        let has_date = audio.date != 0;
        store_flags(
            out,
            [
                has_file_name,
                has_mime_type,
                has_duration,
                has_title,
                has_performer,
                has_minithumbnail,
                has_thumbnail,
                has_date,
            ],
        );
        if has_file_name {
            store_string(out, &audio.file_name);
        }
        if has_mime_type {
            store_string(out, &audio.mime_type);
        }
        if has_duration {
            crate::store_parse::store_i32(out, audio.duration);
        }
        if has_title {
            store_string(out, &audio.title);
        }
        if has_performer {
            store_string(out, &audio.performer);
        }
        if has_minithumbnail {
            store_string(out, &audio.minithumbnail);
        }
        if has_thumbnail {
            audio.thumbnail.store(env, out);
        }
        if has_date {
            crate::store_parse::store_i32(out, audio.date);
        }
        env.store_file_id(file_id, out);
    }

    /// `AudiosManager::parse_audio` (AudiosManager.hpp:76-122).
    pub fn parse_audio(
        &mut self,
        env: &mut impl AudiosManagerEnv,
        parser: &mut Parser<'_>,
    ) -> Result<FileId, &'static str> {
        let (
            has_file_name,
            has_mime_type,
            has_duration,
            has_title,
            has_performer,
            has_minithumbnail,
            has_thumbnail,
            has_date,
        ) = if parser.version() >= crate::store_parse::version::ADD_AUDIO_FLAGS {
            let [
                has_file_name,
                has_mime_type,
                has_duration,
                has_title,
                has_performer,
                has_minithumbnail,
                has_thumbnail,
                has_date,
            ] = parser.parse_flags()?;
            (
                has_file_name,
                has_mime_type,
                has_duration,
                has_title,
                has_performer,
                has_minithumbnail,
                has_thumbnail,
                has_date,
            )
        } else {
            (
                true,
                true,
                true,
                true,
                true,
                parser.version() >= crate::store_parse::version::SUPPORT_MINITHUMBNAILS,
                true,
                false,
            )
        };
        let file_name = if has_file_name {
            parser.string()?
        } else {
            String::new()
        };
        let mime_type = if has_mime_type {
            parser.string()?
        } else {
            String::new()
        };
        let duration = if has_duration { parser.i32_()? } else { 0 };
        let title = if has_title {
            parser.string()?
        } else {
            String::new()
        };
        let performer = if has_performer {
            parser.string()?
        } else {
            String::new()
        };
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
        let date = if has_date { parser.i32_()? } else { 0 };
        let file_id = env.parse_file_id(parser)?;
        self.audios.insert(
            file_id,
            Audio {
                file_name,
                mime_type,
                duration,
                date,
                title,
                performer,
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

    impl AudiosManagerEnv for TestEnv {
        fn file_manager(&self) -> Arc<parking_lot::Mutex<FileManager>> {
            Arc::clone(&self.files)
        }
    }

    impl TestEnv {
        /// The store/parse plane round-trips only registered file ids
        /// (the C++ records always carry FileManager-backed ids).
        fn register_file(
            &self,
            remote_document_id: i64,
            file_type: crate::files::FileType,
        ) -> FileId {
            self.files.lock().register_remote(
                crate::files::FullRemoteFileLocation::from_document(
                    file_type,
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
        let mut manager = AudiosManager::new();
        manager.create_audio(
            FileId::new(1, 0),
            String::new(),
            PhotoSize::default(),
            "a.mp3".to_owned(),
            "audio/mpeg".to_owned(),
            10,
            "t".to_owned(),
            "p".to_owned(),
            1700000000,
            false,
        );
        assert_eq!(manager.get_audio_duration(FileId::new(1, 0)), 10);
        manager.merge_audios(FileId::new(2, 0), FileId::new(1, 0));
        assert_eq!(manager.get_audio_duration(FileId::new(2, 0)), 10);
        manager.delete_audio_thumbnail(FileId::new(2, 0));
    }

    #[test]
    fn store_parse_round_trips() {
        let mut manager = AudiosManager::new();
        let mut env = env();
        let file_id = env.register_file(7, crate::files::FileType::Audio);
        manager.create_audio(
            file_id,
            "mini".to_owned(),
            PhotoSize::default(),
            "f.mp3".to_owned(),
            "audio/mpeg".to_owned(),
            42,
            "Title".to_owned(),
            "Performer".to_owned(),
            1700000001,
            true,
        );
        let mut out = Vec::new();
        manager.store_audio(&mut env, file_id, &mut out);

        let mut manager2 = AudiosManager::new();
        let mut parser = Parser::new(&out);
        let parsed_file_id = manager2.parse_audio(&mut env, &mut parser).unwrap();
        assert_eq!(parsed_file_id, file_id);
        assert_eq!(
            manager2.get_audio(parsed_file_id),
            manager.get_audio(file_id)
        );
    }

    #[test]
    fn notification_sound_objects() {
        let mut manager = AudiosManager::new();
        let env = env();
        let ringtone_file_id = env.register_file(9, crate::files::FileType::Ringtone);
        manager.create_audio(
            ringtone_file_id,
            String::new(),
            PhotoSize::default(),
            String::new(),
            "audio/ogg".to_owned(),
            3,
            "sound".to_owned(),
            "ringtone".to_owned(),
            1700000002,
            false,
        );
        let sound = manager.get_notification_sound_object(&env, ringtone_file_id);
        assert_eq!(sound.id, 9);
        assert_eq!(sound.duration, 3);
        assert_eq!(sound.title, "sound");
        // the invalid id answers the C++ nullptr collapsed to the empty
        // struct
        let sounds =
            manager.get_notification_sounds_object(&env, &[ringtone_file_id, FileId::default()]);
        assert_eq!(sounds.notification_sounds.len(), 2);
        assert_eq!(sounds.notification_sounds[1].id, 0);
    }
}
