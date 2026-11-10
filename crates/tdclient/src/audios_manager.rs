use crate::files::{FileId, FileLoadManager, FileManager, FileType, FileUploadId, UploadCallback};
use crate::photo::{self, DialogPhotoStoreEnv, PhotoSize};
use crate::store_parse::{Parser, store_flags, store_string};
use crate::td_api;
use std::collections::HashMap;
use std::sync::Arc;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

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
    /// `being_uploaded_audio_files_` — the fresh-upload/reupload promises
    /// parked by the upload id (AudiosManager.h:88); the story-music tail
    /// rides the keyed story-side table.
    pub(crate) being_uploaded_audio_files: HashMap<FileUploadId, Promise<()>>,
}

/// The `AudiosManager::upload_audio` unfolding — the C++
/// `Promise<FileId>` becomes the two shapes the collapsed callers act on:
/// the remote fast path answers the file id inline (the C++ `set_value`
/// band), the fresh-upload tails return the registered upload id (the
/// promise settles later through the upload callback / the
/// `messages.uploadMedia` answer).
#[derive(Debug)]
pub enum AudioUploadOutcome {
    /// The remote fast path — continue with the file id inline
    /// (AudiosManager.cpp:392-395).
    Ready(FileId),
    /// The upload tail took over (AudiosManager.cpp:397-403). A `Some`
    /// `url_input_media` is the url band (AudiosManager.cpp:398-400) —
    /// the `messages.uploadMedia` query sends at once with the
    /// already-built media (the C++ `do_upload_audio_file(..., nullptr,
    /// ...)` leg); a `None` is the upload band registered with the file
    /// plane.
    Started {
        file_upload_id: FileUploadId,
        /// The built media rides boxed (the variant-size clippy gate;
        /// the media is moved into the query at the send).
        url_input_media: Option<Box<telegram_api::InputMedia>>,
    },
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
        self.get_input_media_with_files(&file_manager, file_id, input_file, input_thumbnail)
    }

    /// The `&FileManager` form of [`Self::get_input_media`] — the upload
    /// tails that already hold the manager borrow (the C++ reaches the
    /// same view through `td_` inside its single actor).
    pub fn get_input_media_with_files(
        &self,
        files: &FileManager,
        file_id: FileId,
        input_file: Option<telegram_api::InputFile>,
        input_thumbnail: Option<telegram_api::InputFile>,
    ) -> Option<telegram_api::InputMedia> {
        let file_view = files.get_file_view(file_id)?;
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

    /// `AudiosManager::upload_audio` (AudiosManager.cpp:367-405) — the
    /// null/UTF-8 gates, the `get_input_file_id` resolution, the
    /// encrypted/web gates, the `create_audio` registration and the
    /// remote fast path (`dup_audio` + the inline answer). The promise
    /// mirrors the C++ `Promise<FileId>`: the gates and the fast path
    /// settle it inline, the upload band parks it in the upload map, and
    /// the url band subsumes it into the manager's §5 query carrier. The
    /// story-music continuation keys on the returned upload id (the C++
    /// `PromiseCreator::lambda` capture). The C++ CHECK(file_id.is_valid())
    /// / CHECK(is_inserted) invariants are the saved-music twin's
    /// `debug_assert!`s (the registered-file invariant release builds
    /// trust; the table-build CHECKs in tdutils/emoji.rs stay `assert!`).
    pub fn upload_audio(
        &mut self,
        files: &mut FileManager,
        fllm: &mut FileLoadManager,
        upload_callback: &Arc<parking_lot::Mutex<dyn UploadCallback>>,
        input_audio: Option<td_api::InputAudio>,
        mut promise: Promise<()>,
    ) -> tdutils::status::Result<AudioUploadOutcome> {
        let Some(input_audio) = input_audio else {
            let status = Status::error(400, "Audio must be non-empty");
            promise.set_error(status.clone());
            return tdutils::status::Result::Err(status);
        };
        if crate::misc::clean_input_string(&input_audio.title).is_none()
            || crate::misc::clean_input_string(&input_audio.performer).is_none()
        {
            let status = Status::error(400, "Strings must be encoded in UTF-8");
            promise.set_error(status.clone());
            return tdutils::status::Result::Err(status);
        }
        let file_id = match files.get_input_file_id(
            FileType::Audio,
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
            Err(error) => {
                promise.set_error(error.clone());
                return tdutils::status::Result::Err(error);
            }
        };
        debug_assert!(file_id.is_valid(), "CHECK(file_id.is_valid())");

        let file_view = files.get_file_view(file_id);
        if file_view.as_ref().is_some_and(|view| view.is_encrypted()) {
            let status = Status::error(400, "Can't use encrypted file");
            promise.set_error(status.clone());
            return tdutils::status::Result::Err(status);
        }
        let main_remote_location = file_view
            .as_ref()
            .and_then(|view| view.get_main_remote_location())
            .cloned();
        if main_remote_location
            .as_ref()
            .is_some_and(|location| location.is_web())
        {
            let status = Status::error(400, "Can't use web file as an audio");
            promise.set_error(status.clone());
            return tdutils::status::Result::Err(status);
        }
        let main_file_id = file_view
            .as_ref()
            .map(|view| view.get_main_file_id())
            .unwrap_or(file_id);
        // the view borrow must end before the `files` mutations below
        // (FileView borrows the FileManager)
        let has_url = file_view.as_ref().is_some_and(|view| view.has_url());
        let _ = file_view;

        self.create_audio(
            file_id,
            String::new(),
            PhotoSize::default(),
            String::new(),
            String::new(),
            input_audio.duration,
            input_audio.title,
            input_audio.performer,
            0,
            false,
        );
        if main_remote_location.is_some() {
            self.dup_audio(main_file_id, file_id);
            promise.set_value(());
            return tdutils::status::Result::Ok(AudioUploadOutcome::Ready(file_id));
        }

        let file_upload_id = FileUploadId::new(file_id, files.get_internal_upload_id());
        if has_url {
            // `do_upload_audio_file({file_id, get_internal_upload_id()},
            // nullptr, ...)` (AudiosManager.cpp:398-400) — the url band's
            // uploadMedia query sends at once; the caller dispatches with
            // the built media (the url file never enters the upload map;
            // the §5 query carrier is the manager's push).
            let input_media = self
                .get_input_media_with_files(files, file_upload_id.file_id, None, None)
                .expect("CHECK(input_media != nullptr)");
            return tdutils::status::Result::Ok(AudioUploadOutcome::Started {
                file_upload_id,
                url_input_media: Some(Box::new(input_media)),
            });
        }

        // `upload_audio_file` (AudiosManager.cpp:417-425) — park the
        // promise on the upload id and ask the file plane (the
        // `upload_audio_file_callback_`, priority 2, order 0).
        debug_assert!(
            self.get_input_media_with_files(files, file_id, None, None)
                .is_none(),
            "CHECK(get_input_media(file_id, nullptr, nullptr) == nullptr)"
        );
        debug_assert!(
            self.being_uploaded_audio_files
                .insert(file_upload_id, promise)
                .is_none(),
            "CHECK(is_inserted)"
        );
        tdutils::log_info!("Ask to upload audio {file_upload_id:?}");
        files.upload(file_upload_id, upload_callback.clone(), 2, 0, fllm);
        tdutils::status::Result::Ok(AudioUploadOutcome::Started {
            file_upload_id,
            url_input_media: None,
        })
    }

    /// `AudiosManager::reupload_audio` (AudiosManager.cpp:409-415) — the
    /// repair-failure reupload chain (`resume_upload` over the bad
    /// parts `{-1}`); the resumed upload id keys the story-side tail (the
    /// collapsed keying of the C++ lambda capture).
    pub fn reupload_audio(
        &mut self,
        files: &mut FileManager,
        fllm: &mut FileLoadManager,
        upload_callback: &Arc<parking_lot::Mutex<dyn UploadCallback>>,
        file_id: FileId,
        promise: Promise<()>,
    ) -> FileUploadId {
        let file_upload_id = FileUploadId::new(file_id, files.get_internal_upload_id());
        debug_assert!(
            self.being_uploaded_audio_files
                .insert(file_upload_id, promise)
                .is_none(),
            "CHECK(is_inserted)"
        );
        tdutils::log_info!("Ask to reupload audio {file_upload_id:?}");
        files.resume_upload(
            file_upload_id,
            vec![-1],
            upload_callback.clone(),
            2,
            0,
            false,
            false,
            fllm,
        );
        file_upload_id
    }

    /// `AudiosManager::on_upload_audio_file`'s map pop
    /// (AudiosManager.cpp:431-437) — the layer-1 promise carrier.
    pub(crate) fn take_uploaded_audio_promise(
        &mut self,
        file_upload_id: FileUploadId,
    ) -> Promise<()> {
        self.being_uploaded_audio_files
            .remove(&file_upload_id)
            .expect("the callback fires for a registered upload")
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

    /// The typed `promise_result` slots (a type-complexity clippy
    /// alias, generic over the promise payload).
    type PromiseSlots = (
        Promise<()>,
        std::sync::Arc<std::sync::Mutex<Option<tdutils::status::Result<()>>>>,
    );
    /// The typed `promise_result` slots for the upload gates (the
    /// user_manager test helper shape).
    fn promise_result() -> PromiseSlots {
        let result = std::sync::Arc::new(std::sync::Mutex::new(None));
        let result_clone = result.clone();
        (
            tdutils::promise::PromiseCreator::lambda_result(move |r| {
                *result_clone.lock().unwrap() = Some(r)
            }),
            result,
        )
    }

    fn upload_env() -> (
        FileManager,
        crate::files::FileLoadManager,
        std::sync::Arc<parking_lot::Mutex<dyn UploadCallback>>,
    ) {
        let fllm = crate::files::FileLoadManager::new(
            Box::new(PanicNetFactory),
            crate::files::FileLoadManagerOptions::default(),
        );
        let callback: std::sync::Arc<parking_lot::Mutex<dyn UploadCallback>> =
            std::sync::Arc::new(parking_lot::Mutex::new(NoopUploadCallback));
        (FileManager::new(), fllm, callback)
    }

    struct PanicNetFactory;

    impl crate::files::FileLoadNetFactory for PanicNetFactory {
        fn download_net(&mut self) -> Box<dyn crate::files::FileDownloaderNet> {
            unimplemented!("the upload-gate tests start no downloads")
        }
        fn upload_net(&mut self) -> Box<dyn crate::files::FileUploaderNet> {
            unimplemented!("the upload-gate tests start no uploads")
        }
        fn hash_net(&mut self) -> Box<dyn crate::files::FileHashUploaderNet> {
            unimplemented!("the upload-gate tests start no hash uploads")
        }
        fn utils_net(&mut self) -> Box<dyn crate::files::FileDownloaderNet> {
            unimplemented!("the upload-gate tests touch no file io")
        }
    }

    struct NoopUploadCallback;

    impl UploadCallback for NoopUploadCallback {
        fn on_upload_ok(
            &mut self,
            _file_upload_id: FileUploadId,
            _input_file: Option<telegram_api::InputFile>,
        ) {
        }

        fn on_upload_error(&mut self, _file_upload_id: FileUploadId, _status: Status) {}
    }

    #[test]
    fn upload_audio_runs_the_input_gates() {
        let (mut files, mut fllm, callback) = upload_env();
        let mut manager = AudiosManager::new();

        // the null gate
        let (promise, result) = promise_result();
        let upload = manager.upload_audio(&mut files, &mut fllm, &callback, None, promise);
        assert!(
            matches!(
                upload,
                tdutils::status::Result::Err(ref s) if s.message() == "Audio must be non-empty"
            ),
            "{upload:?}"
        );
        assert!(matches!(
            *result.lock().unwrap(),
            Some(tdutils::status::Result::Err(ref s)) if s.message() == "Audio must be non-empty"
        ));

        // a clean input without a file is the get_input_file_id 400
        let (promise, _result) = promise_result();
        let upload = manager.upload_audio(
            &mut files,
            &mut fllm,
            &callback,
            Some(td_api::InputAudio {
                audio: None,
                album_cover_thumbnail: None,
                duration: 10,
                title: "Title".to_owned(),
                performer: "Performer".to_owned(),
            }),
            promise,
        );
        assert!(matches!(
            upload,
            tdutils::status::Result::Err(ref s) if s.message() == "InputFile is not specified"
        ));

        // a web main remote is the web gate (the promise slot rides the
        // gate error)
        let web_file_id = files.register_remote(
            crate::files::FullRemoteFileLocation::from_web(
                FileType::Audio,
                "https://example.com/x.mp3".to_owned(),
                1,
            ),
            crate::files::FileLocationSource::FromServer,
            crate::message_id::DialogId::default(),
            0,
            0,
            String::new(),
        );
        let (promise, _result) = promise_result();
        let upload = manager.upload_audio(
            &mut files,
            &mut fllm,
            &callback,
            Some(td_api::InputAudio {
                audio: Some(td_api::InputFile::inputFileId(td_api::inputFileId {
                    id: web_file_id.get(),
                })),
                album_cover_thumbnail: None,
                duration: 10,
                title: "Title".to_owned(),
                performer: "Performer".to_owned(),
            }),
            promise,
        );
        assert!(matches!(
            upload,
            tdutils::status::Result::Err(ref s) if s.message() == "Can't use web file as an audio"
        ));
    }

    #[test]
    fn upload_audio_remote_fast_path_answers_ready() {
        let (mut files, mut fllm, callback) = upload_env();
        let mut manager = AudiosManager::new();

        let remote_file_id = files.register_remote(
            crate::files::FullRemoteFileLocation::from_document(
                FileType::Audio,
                500,
                42,
                crate::chat_manager::DcId::main(),
                "ref".to_owned(),
            ),
            crate::files::FileLocationSource::FromServer,
            crate::message_id::DialogId::default(),
            128,
            128,
            "song.mp3".to_owned(),
        );
        let (promise, result) = promise_result();
        let upload = manager.upload_audio(
            &mut files,
            &mut fllm,
            &callback,
            Some(td_api::InputAudio {
                audio: Some(td_api::InputFile::inputFileId(td_api::inputFileId {
                    id: remote_file_id.get(),
                })),
                album_cover_thumbnail: None,
                duration: 180,
                title: "Song".to_owned(),
                performer: "Artist".to_owned(),
            }),
            promise,
        );
        // the C++ fast path: `dup_audio` + the inline set_value(file_id)
        let tdutils::status::Result::Ok(AudioUploadOutcome::Ready(file_id)) = upload else {
            panic!("expected the Ready fast path, got {upload:?}");
        };
        assert_eq!(file_id, remote_file_id);
        assert!(matches!(
            *result.lock().unwrap(),
            Some(tdutils::status::Result::Ok(()))
        ));
        assert!(manager.get_audio(remote_file_id).is_some());
        // the upload map stays empty (no upload tail took over)
        assert!(manager.being_uploaded_audio_files.is_empty());
    }
}
