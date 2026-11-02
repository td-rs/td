use crate::files::{FileId, FileManager};
use crate::photo::{self, AnimationSize, DialogPhotoStoreEnv, Dimensions, PhotoSize};
use crate::store_parse::{Parser, store_flags, store_string};
use crate::td_api;
use std::collections::HashMap;
use std::sync::Arc;
use tdmtproto::telegram_api;

/// The `Td *td` slice of the collapsed VideosManager.
pub trait VideosManagerEnv: DialogPhotoStoreEnv {
    /// The `td_->file_manager_` handle — `get_file_object` and the file
    /// views ride it (locked per call).
    fn file_manager(&self) -> Arc<parking_lot::Mutex<FileManager>>;

    /// The `td_->documents_manager_` handle of the alternative-video
    /// name check (the storyboard/HLS map documents).
    fn documents(&self) -> &crate::documents_manager::DocumentsManager;
}

/// `VideosManager::Video` (VideosManager.h:80-111).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Video {
    pub file_name: String,
    pub mime_type: String,
    pub duration: i32,
    pub precise_duration: f64,
    pub dimensions: Dimensions,
    pub minithumbnail: String,
    pub thumbnail: PhotoSize,
    pub animated_thumbnail: AnimationSize,
    pub preload_prefix_size: i32,
    pub start_ts: f64,
    pub codec: String,
    pub supports_streaming: bool,
    pub is_animation: bool,
    pub has_stickers: bool,
    pub sticker_file_ids: Vec<FileId>,
    pub file_id: FileId,
}

/// `VideosManager` (VideosManager.h:27-112).
#[derive(Default)]
pub struct VideosManager {
    pub(crate) videos: HashMap<FileId, Video>,
}

impl VideosManager {
    pub fn new() -> VideosManager {
        VideosManager::default()
    }

    /// `VideosManager::get_video` (VideosManager.cpp:165-169).
    pub fn get_video(&self, file_id: FileId) -> Option<&Video> {
        self.videos.get(&file_id)
    }

    /// `VideosManager::get_video_duration` (VideosManager.cpp:171-175).
    pub fn get_video_duration(&self, file_id: FileId) -> i32 {
        self.get_video(file_id).map_or(0, |video| video.duration)
    }

    /// `VideosManager::get_video_mime_type` (VideosManager.cpp:177-181).
    pub fn get_video_mime_type(&self, file_id: FileId) -> &str {
        self.get_video(file_id)
            .map_or("", |video| video.mime_type.as_str())
    }

    /// `VideosManager::create_video` (VideosManager.cpp:214-247).
    #[allow(clippy::too_many_arguments)]
    pub fn create_video(
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
    ) {
        let video = Video {
            file_name,
            mime_type,
            duration,
            precise_duration,
            dimensions,
            minithumbnail,
            thumbnail,
            animated_thumbnail,
            preload_prefix_size,
            start_ts,
            codec,
            supports_streaming,
            is_animation,
            has_stickers,
            sticker_file_ids,
            file_id,
        };
        if replace || !self.videos.contains_key(&file_id) {
            self.videos.insert(file_id, video);
        }
    }

    /// `VideosManager::get_video_thumbnail_file_id`
    /// (VideosManager.cpp:391-395).
    pub fn get_video_thumbnail_file_id(&self, file_id: FileId) -> FileId {
        self.get_video(file_id)
            .map_or(FileId::default(), |video| video.thumbnail.file_id)
    }

    /// `VideosManager::get_video_animated_thumbnail_file_id`
    /// (VideosManager.cpp:397-401).
    pub fn get_video_animated_thumbnail_file_id(&self, file_id: FileId) -> FileId {
        self.get_video(file_id).map_or(FileId::default(), |video| {
            video.animated_thumbnail.photo_size.file_id
        })
    }

    /// `VideosManager::delete_video_thumbnail` (VideosManager.cpp:178-184).
    pub fn delete_video_thumbnail(&mut self, file_id: FileId) {
        if let Some(video) = self.videos.get_mut(&file_id) {
            video.thumbnail = PhotoSize::default();
            video.animated_thumbnail = AnimationSize::default();
        }
    }

    /// `VideosManager::dup_video` (VideosManager.cpp:186-195).
    pub fn dup_video(&mut self, new_id: FileId, old_id: FileId) -> FileId {
        if let Some(mut video) = self.videos.get(&old_id).cloned() {
            video.file_id = new_id;
            self.videos.insert(new_id, video);
            new_id
        } else {
            FileId::default()
        }
    }

    /// `VideosManager::merge_videos` (VideosManager.cpp:197-212).
    pub fn merge_videos(&mut self, new_id: FileId, old_id: FileId) {
        if new_id == old_id {
            return;
        }
        if let Some(old_video) = self.videos.remove(&old_id) {
            self.videos.entry(new_id).or_insert(old_video);
        }
    }

    /// `VideosManager::get_video_search_text` (VideosManager.cpp:403-409).
    pub fn get_video_search_text(&self, file_id: FileId) -> String {
        self.get_video(file_id)
            .map_or_else(String::new, |video| video.file_name.clone())
    }

    /// `VideosManager::get_video_object` (VideosManager.cpp:249-283).
    pub fn get_video_object(
        &self,
        env: &impl VideosManagerEnv,
        file_id: FileId,
    ) -> Option<td_api::Video> {
        if !file_id.is_valid() {
            return None;
        }
        let video = self.get_video(file_id)?;
        Some(td_api::Video {
            duration: video.duration,
            width: video.dimensions.width as i32,
            height: video.dimensions.height as i32,
            file_name: video.file_name.clone(),
            mime_type: video.mime_type.clone(),
            has_stickers: video.has_stickers,
            supports_streaming: video.supports_streaming,
            minithumbnail: Some(photo::get_minithumbnail_object(
                video.minithumbnail.as_bytes(),
            )),
            thumbnail: photo::get_thumbnail_object_with_animation(
                &video.thumbnail,
                &video.animated_thumbnail.photo_size,
            ),
            video: Some(env.file_manager().lock().get_file_object(file_id)),
        })
    }

    /// `VideosManager::get_story_video_object` (VideosManager.cpp:87-99).
    pub fn get_story_video_object(
        &self,
        env: &impl VideosManagerEnv,
        file_id: FileId,
    ) -> Option<td_api::StoryVideo> {
        if !file_id.is_valid() {
            return None;
        }
        let video = self.get_video(file_id)?;
        let thumbnail = photo::get_thumbnail_object_with_animation(
            &video.thumbnail,
            &video.animated_thumbnail.photo_size,
        );
        Some(td_api::StoryVideo {
            duration: video.precise_duration,
            width: i32::from(video.dimensions.width),
            height: i32::from(video.dimensions.height),
            has_stickers: video.has_stickers,
            is_animation: video.is_animation,
            minithumbnail: Some(photo::get_minithumbnail_object(
                video.minithumbnail.as_bytes(),
            )),
            thumbnail,
            preload_prefix_size: video.preload_prefix_size,
            cover_frame_timestamp: video.start_ts,
            video: Some(env.file_manager().lock().get_file_object(file_id)),
        })
    }

    /// `VideosManager::get_alternative_video_object`
    /// (VideosManager.cpp:101-119) — the HLS alternative whose map
    /// document is registered under the `mtproto:<document_id>` name.
    pub fn get_alternative_video_object(
        &self,
        env: &impl VideosManagerEnv,
        file_id: FileId,
        hls_file_ids: &[FileId],
    ) -> Option<td_api::AlternativeVideo> {
        let video = self.get_video(file_id)?;
        let file_manager = env.file_manager();
        let file_manager = file_manager.lock();
        let file_view = file_manager.get_file_view(file_id)?;
        let full_remote_location = file_view.get_full_remote_location()?;
        assert!(full_remote_location.is_document());
        let document_id = full_remote_location.get_id();
        drop(file_manager);
        let name = format!("mtproto:{document_id}");
        for &hls_file_id in hls_file_ids {
            if env.documents().get_document_file_name(hls_file_id) == name {
                let file_manager = env.file_manager();
                let file_manager = file_manager.lock();
                return Some(td_api::AlternativeVideo {
                    id: document_id,
                    width: i32::from(video.dimensions.width),
                    height: i32::from(video.dimensions.height),
                    codec: video.codec.clone(),
                    hls_file: Some(file_manager.get_file_object(hls_file_id)),
                    video: Some(file_manager.get_file_object(file_id)),
                });
            }
        }
        None
    }

    /// `VideosManager::get_input_media` (VideosManager.cpp:272-381).
    #[allow(clippy::too_many_arguments)]
    pub fn get_input_media(
        &self,
        env: &impl VideosManagerEnv,
        file_id: FileId,
        input_file: Option<telegram_api::InputFile>,
        input_thumbnail: Option<telegram_api::InputFile>,
        cover: &crate::photo::Photo,
        start_timestamp: i32,
        ttl: i32,
        has_spoiler: bool,
    ) -> Option<telegram_api::InputMedia> {
        if !file_id.is_valid() {
            return None;
        }
        // the C++ video-cover probe: the photo's any file id, decrypted
        // main document location only
        let mut input_cover: Option<telegram_api::InputPhoto> = None;
        if !cover.is_empty() {
            let cover_file_id = photo::get_photo_any_file_id(cover);
            if cover_file_id.is_valid() {
                let file_manager = env.file_manager();
                let file_manager = file_manager.lock();
                if let Some(cover_file_view) = file_manager.get_file_view(cover_file_id)
                    && !cover_file_view.is_encrypted()
                    && let Some(main_remote_location) = cover_file_view.get_main_remote_location()
                    && !main_remote_location.is_web()
                {
                    input_cover = Some(as_input_photo(main_remote_location));
                }
            }
        }
        let file_manager = env.file_manager();
        let file_manager = file_manager.lock();
        let file_view = file_manager.get_file_view(file_id)?;
        if file_view.is_encrypted() {
            return None;
        }
        let main_remote_location = file_view.get_main_remote_location();
        if let Some(main_remote_location) = main_remote_location
            && !main_remote_location.is_web()
            && input_file.is_none()
        {
            let mut flags = 0;
            const INPUT_MEDIA_DOCUMENT_TTL_SECONDS_MASK: i32 = 1;
            const INPUT_MEDIA_DOCUMENT_VIDEO_TIMESTAMP_MASK: i32 = 16;
            const INPUT_MEDIA_DOCUMENT_VIDEO_COVER_MASK: i32 = 8;
            if ttl != 0 {
                flags |= INPUT_MEDIA_DOCUMENT_TTL_SECONDS_MASK;
            }
            if start_timestamp != 0 {
                flags |= INPUT_MEDIA_DOCUMENT_VIDEO_TIMESTAMP_MASK;
            }
            if input_cover.is_some() {
                flags |= INPUT_MEDIA_DOCUMENT_VIDEO_COVER_MASK;
            }
            return Some(telegram_api::InputMedia::inputMediaDocument(
                telegram_api::inputMediaDocument {
                    flags,
                    spoiler: has_spoiler,
                    id: Some(as_input_document(main_remote_location)),
                    video_cover: input_cover,
                    video_timestamp: start_timestamp,
                    ttl_seconds: ttl,
                    query: String::new(),
                },
            ));
        }
        if let Some(url) = file_view.get_url() {
            let mut flags = 0;
            const INPUT_MEDIA_DOCUMENT_EXTERNAL_TTL_SECONDS_MASK: i32 = 1;
            const INPUT_MEDIA_DOCUMENT_EXTERNAL_VIDEO_TIMESTAMP_MASK: i32 = 8;
            const INPUT_MEDIA_DOCUMENT_EXTERNAL_VIDEO_COVER_MASK: i32 = 4;
            if ttl != 0 {
                flags |= INPUT_MEDIA_DOCUMENT_EXTERNAL_TTL_SECONDS_MASK;
            }
            if start_timestamp != 0 {
                flags |= INPUT_MEDIA_DOCUMENT_EXTERNAL_VIDEO_TIMESTAMP_MASK;
            }
            if input_cover.is_some() {
                flags |= INPUT_MEDIA_DOCUMENT_EXTERNAL_VIDEO_COVER_MASK;
            }
            return Some(telegram_api::InputMedia::inputMediaDocumentExternal(
                telegram_api::inputMediaDocumentExternal {
                    flags,
                    spoiler: has_spoiler,
                    url: url.to_owned(),
                    ttl_seconds: ttl,
                    video_cover: input_cover,
                    video_timestamp: start_timestamp,
                },
            ));
        }

        if let Some(input_file) = input_file {
            let video = self.get_video(file_id)?;
            let mut attributes: Vec<telegram_api::DocumentAttribute> = Vec::new();
            {
                let mut attribute_flags = 0;
                const ATTRIBUTE_VIDEO_VIDEO_START_TS_MASK: i32 = 16;
                if video.start_ts > 0.0 {
                    attribute_flags |= ATTRIBUTE_VIDEO_VIDEO_START_TS_MASK;
                }
                attributes.push(telegram_api::DocumentAttribute::documentAttributeVideo(
                    telegram_api::documentAttributeVideo {
                        flags: attribute_flags,
                        round_message: false,
                        supports_streaming: video.supports_streaming,
                        nosound: video.is_animation,
                        duration: video.precise_duration,
                        w: i32::from(video.dimensions.width),
                        h: i32::from(video.dimensions.height),
                        preload_prefix_size: 0,
                        video_start_ts: video.start_ts,
                        video_codec: String::new(),
                    },
                ));
            }
            if !video.file_name.is_empty() {
                attributes.push(telegram_api::DocumentAttribute::documentAttributeFilename(
                    telegram_api::documentAttributeFilename {
                        file_name: video.file_name.clone(),
                    },
                ));
            }
            let mut flags = 0;
            const UPLOADED_STICKERS_MASK: i32 = 1;
            const UPLOADED_TTL_SECONDS_MASK: i32 = 2;
            const UPLOADED_THUMB_MASK: i32 = 4;
            const UPLOADED_VIDEO_TIMESTAMP_MASK: i32 = 128;
            const UPLOADED_VIDEO_COVER_MASK: i32 = 64;
            let added_stickers = if video.has_stickers {
                flags |= UPLOADED_STICKERS_MASK;
                get_input_documents(&file_manager, &video.sticker_file_ids)
            } else {
                Vec::new()
            };
            let mut mime_type = video.mime_type.clone();
            if !mime_type.starts_with("video/") || ttl > 0 {
                mime_type = "video/mp4".to_owned();
            }
            if ttl != 0 {
                flags |= UPLOADED_TTL_SECONDS_MASK;
            }
            if input_thumbnail.is_some() {
                flags |= UPLOADED_THUMB_MASK;
            }
            if start_timestamp != 0 {
                flags |= UPLOADED_VIDEO_TIMESTAMP_MASK;
            }
            if input_cover.is_some() {
                flags |= UPLOADED_VIDEO_COVER_MASK;
            }
            return Some(telegram_api::InputMedia::inputMediaUploadedDocument(
                telegram_api::inputMediaUploadedDocument {
                    flags,
                    nosound_video: true,
                    force_file: false,
                    spoiler: has_spoiler,
                    file: Some(input_file),
                    thumb: input_thumbnail,
                    mime_type,
                    attributes,
                    stickers: added_stickers,
                    video_cover: input_cover,
                    video_timestamp: start_timestamp,
                    ttl_seconds: ttl,
                },
            ));
        }
        None
    }

    /// `VideosManager::get_story_document_input_media`
    /// (VideosManager.cpp:380-423) — the story-video re-upload of an
    /// already-main-document video.
    pub fn get_story_document_input_media(
        &self,
        env: &impl VideosManagerEnv,
        file_id: FileId,
        main_frame_timestamp: f64,
    ) -> Option<telegram_api::InputMedia> {
        let file_manager = env.file_manager();
        let file_manager = file_manager.lock();
        let file_view = file_manager.get_file_view(file_id)?;
        if file_view.is_encrypted() {
            return None;
        }
        let main_remote_location = file_view.get_main_remote_location()?;
        if main_remote_location.is_web() {
            return None;
        }
        let video = self.get_video(file_id)?;

        let mut attributes: Vec<telegram_api::DocumentAttribute> = Vec::new();
        let mut attribute_flags = 0;
        const ATTRIBUTE_VIDEO_VIDEO_START_TS_MASK: i32 = 16;
        if main_frame_timestamp > 0.0 {
            attribute_flags |= ATTRIBUTE_VIDEO_VIDEO_START_TS_MASK;
        }
        attributes.push(telegram_api::DocumentAttribute::documentAttributeVideo(
            telegram_api::documentAttributeVideo {
                flags: attribute_flags,
                round_message: false,
                supports_streaming: video.supports_streaming,
                nosound: video.is_animation,
                duration: video.precise_duration,
                w: i32::from(video.dimensions.width),
                h: i32::from(video.dimensions.height),
                preload_prefix_size: 0,
                video_start_ts: main_frame_timestamp,
                video_codec: String::new(),
            },
        ));
        if !video.file_name.is_empty() {
            attributes.push(telegram_api::DocumentAttribute::documentAttributeFilename(
                telegram_api::documentAttributeFilename {
                    file_name: video.file_name.clone(),
                },
            ));
        }
        let mut flags = 0;
        const UPLOADED_STICKERS_MASK: i32 = 1;
        let added_stickers = if video.has_stickers {
            flags |= UPLOADED_STICKERS_MASK;
            get_input_documents(&file_manager, &video.sticker_file_ids)
        } else {
            Vec::new()
        };
        let mut mime_type = video.mime_type.clone();
        if !mime_type.starts_with("video/") {
            mime_type = "video/mp4".to_owned();
        }
        Some(telegram_api::InputMedia::inputMediaUploadedDocument(
            telegram_api::inputMediaUploadedDocument {
                flags,
                nosound_video: true,
                force_file: false,
                spoiler: false,
                file: Some(telegram_api::InputFile::inputFileStoryDocument(
                    telegram_api::inputFileStoryDocument {
                        id: Some(as_input_document(main_remote_location)),
                    },
                )),
                thumb: None,
                mime_type,
                attributes,
                stickers: added_stickers,
                video_cover: None,
                video_timestamp: 0,
                ttl_seconds: 0,
            },
        ))
    }

    /// `VideosManager::store_video` (VideosManager.hpp:22-62).
    pub fn store_video(&self, env: &mut impl VideosManagerEnv, file_id: FileId, out: &mut Vec<u8>) {
        let video = self.get_video(file_id).expect("video not found");
        let has_animated_thumbnail = video.animated_thumbnail.photo_size.file_id.is_valid();
        let has_preload_prefix_size = video.preload_prefix_size != 0;
        let has_precise_duration =
            video.precise_duration != 0.0 && video.precise_duration != video.duration as f64;
        let has_start_ts = video.start_ts != 0.0;
        let has_codec = !video.codec.is_empty();
        store_flags(
            out,
            [
                video.has_stickers,
                video.supports_streaming,
                has_animated_thumbnail,
                has_preload_prefix_size,
                has_precise_duration,
                video.is_animation,
                has_start_ts,
                has_codec,
            ],
        );
        store_string(out, &video.file_name);
        store_string(out, &video.mime_type);
        crate::store_parse::store_i32(out, video.duration);
        store_dimensions(out, &video.dimensions);
        store_string(out, &video.minithumbnail);
        video.thumbnail.store(env, out);
        env.store_file_id(file_id, out);
        if video.has_stickers {
            store_file_ids(env, out, &video.sticker_file_ids);
        }
        if has_animated_thumbnail {
            video.animated_thumbnail.store(env, out);
        }
        if has_preload_prefix_size {
            crate::store_parse::store_i32(out, video.preload_prefix_size);
        }
        if has_precise_duration {
            crate::store_parse::store_f64(out, video.precise_duration);
        }
        if has_start_ts {
            crate::store_parse::store_f64(out, video.start_ts);
        }
        if has_codec {
            store_string(out, &video.codec);
        }
    }

    /// `VideosManager::parse_video` (VideosManager.hpp:64-108).
    pub fn parse_video(
        &mut self,
        env: &mut impl VideosManagerEnv,
        parser: &mut Parser<'_>,
    ) -> Result<FileId, &'static str> {
        let [
            has_stickers,
            supports_streaming,
            has_animated_thumbnail,
            has_preload_prefix_size,
            has_precise_duration,
            is_animation,
            has_start_ts,
            has_codec,
        ] = parser.parse_flags()?;
        let file_name = parser.string()?;
        let mime_type = parser.string()?;
        let duration = parser.i32_()?;
        let dimensions = parse_dimensions(parser)?;
        let minithumbnail =
            if parser.version() >= crate::store_parse::version::SUPPORT_MINITHUMBNAILS {
                parser.string()?
            } else {
                String::new()
            };
        let thumbnail = PhotoSize::parse_from(env, parser)?;
        let file_id = env.parse_file_id(parser)?;
        let sticker_file_ids = if has_stickers {
            parse_file_ids(env, parser)?
        } else {
            Vec::new()
        };
        let animated_thumbnail = if has_animated_thumbnail {
            AnimationSize::parse_from(env, parser)?
        } else {
            AnimationSize::default()
        };
        let preload_prefix_size = if has_preload_prefix_size {
            parser.i32_()?
        } else {
            0
        };
        let precise_duration = if has_precise_duration {
            parser.f64_()?
        } else {
            // the C++ `video->precise_duration = video->duration` fallback
            duration as f64
        };
        let start_ts = if has_start_ts { parser.f64_()? } else { 0.0 };
        let codec = if has_codec {
            parser.string()?
        } else {
            String::new()
        };
        self.videos.insert(
            file_id,
            Video {
                file_name,
                mime_type,
                duration,
                precise_duration,
                dimensions,
                minithumbnail,
                thumbnail,
                animated_thumbnail,
                preload_prefix_size,
                start_ts,
                codec,
                supports_streaming,
                is_animation,
                has_stickers,
                sticker_file_ids,
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

/// `FullRemoteFileLocation::as_input_photo` (FileLocation.h) — the
/// video-cover probe; the caller validated `is_photo`.
pub(crate) fn as_input_photo(
    location: &crate::files::FullRemoteFileLocation,
) -> telegram_api::InputPhoto {
    debug_assert!(location.is_photo());
    telegram_api::InputPhoto::inputPhoto(telegram_api::inputPhoto {
        id: location.get_id(),
        access_hash: location.get_access_hash(),
        file_reference: location.get_file_reference().as_bytes().to_vec(),
    })
}

/// `FileManager::get_input_documents` over the full-remote-location
/// probes; the C++ CHECKs every location.
fn get_input_documents(
    file_manager: &FileManager,
    file_ids: &[FileId],
) -> Vec<telegram_api::InputDocument> {
    file_ids
        .iter()
        .map(|file_id| {
            let file_view = file_manager
                .get_file_view(*file_id)
                .expect("get_input_documents: missing file view");
            let full_remote_location = file_view
                .get_full_remote_location()
                .expect("get_input_documents: no full remote location");
            as_input_document(full_remote_location)
        })
        .collect()
}

/// `store(Dimensions)` / `parse(Dimensions)` — the photo module's pair
/// (PhotoSize.hpp).
fn store_dimensions(out: &mut Vec<u8>, dimensions: &Dimensions) {
    dimensions.store(out);
}

fn parse_dimensions(parser: &mut Parser<'_>) -> Result<Dimensions, &'static str> {
    Dimensions::parse_from(parser)
}

fn store_file_ids(env: &mut impl VideosManagerEnv, out: &mut Vec<u8>, file_ids: &[FileId]) {
    crate::store_parse::store_i32(out, file_ids.len() as i32);
    for &file_id in file_ids {
        env.store_file_id(file_id, out);
    }
}

fn parse_file_ids(
    env: &mut impl VideosManagerEnv,
    parser: &mut Parser<'_>,
) -> Result<Vec<FileId>, &'static str> {
    let len = parser.i32_()?;
    if len < 0 {
        return Err("Wrong vector length");
    }
    let mut result = Vec::with_capacity(len as usize);
    for _ in 0..len {
        result.push(env.parse_file_id(parser)?);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::FileManager;
    use std::sync::Arc;

    struct TestEnv {
        files: Arc<parking_lot::Mutex<FileManager>>,
        documents: crate::documents_manager::DocumentsManager,
    }

    impl crate::photo::DialogPhotoStoreEnv for TestEnv {
        fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            self.files.lock().store_file(file_id, out);
        }

        fn parse_file_id(&mut self, parser: &mut Parser<'_>) -> Result<FileId, &'static str> {
            self.files.lock().parse_file(parser)
        }
    }

    impl VideosManagerEnv for TestEnv {
        fn file_manager(&self) -> Arc<parking_lot::Mutex<FileManager>> {
            Arc::clone(&self.files)
        }

        fn documents(&self) -> &crate::documents_manager::DocumentsManager {
            &self.documents
        }
    }

    impl TestEnv {
        /// The store/parse plane round-trips only registered file ids
        /// (the C++ records always carry FileManager-backed ids).
        fn register_file(&self, remote_document_id: i64) -> FileId {
            self.files.lock().register_remote(
                crate::files::FullRemoteFileLocation::from_document(
                    crate::files::FileType::Video,
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
            documents: crate::documents_manager::DocumentsManager::new(),
        }
    }

    #[test]
    fn create_and_accessors() {
        let mut manager = VideosManager::new();
        manager.create_video(
            FileId::new(1, 0),
            "mini".to_owned(),
            PhotoSize::default(),
            AnimationSize::default(),
            false,
            Vec::new(),
            "name.mp4".to_owned(),
            "video/mp4".to_owned(),
            12,
            12.5,
            Dimensions {
                width: 640,
                height: 480,
            },
            true,
            false,
            0,
            0.0,
            String::new(),
            false,
        );
        assert_eq!(manager.get_video_duration(FileId::new(1, 0)), 12);
        assert_eq!(manager.get_video_mime_type(FileId::new(1, 0)), "video/mp4");
        assert_eq!(manager.get_video_search_text(FileId::new(1, 0)), "name.mp4");
        assert_eq!(manager.get_video_duration(FileId::new(2, 0)), 0);
    }

    #[test]
    fn merge_and_dup() {
        let mut manager = VideosManager::new();
        manager.create_video(
            FileId::new(1, 0),
            String::new(),
            PhotoSize::default(),
            AnimationSize::default(),
            false,
            Vec::new(),
            "a".to_owned(),
            "video/mp4".to_owned(),
            1,
            0.0,
            Dimensions::default(),
            false,
            false,
            0,
            0.0,
            String::new(),
            false,
        );
        assert_eq!(
            manager.dup_video(FileId::new(2, 0), FileId::new(1, 0)),
            FileId::new(2, 0)
        );
        assert_eq!(manager.get_video_duration(FileId::new(2, 0)), 1);
        manager.merge_videos(FileId::new(3, 0), FileId::new(2, 0));
        assert_eq!(manager.get_video_mime_type(FileId::new(3, 0)), "video/mp4");
    }

    #[test]
    fn store_parse_round_trips() {
        let mut manager = VideosManager::new();
        let mut env = env();
        let file_id = env.register_file(5);
        let sticker_file_id = env.register_file(6);
        manager.create_video(
            file_id,
            "mini".to_owned(),
            PhotoSize::default(),
            AnimationSize::default(),
            true,
            vec![sticker_file_id],
            "f.mp4".to_owned(),
            "video/mp4".to_owned(),
            30,
            30.25,
            Dimensions {
                width: 10,
                height: 20,
            },
            true,
            true,
            1024,
            1.5,
            "avc".to_owned(),
            true,
        );
        let mut out = Vec::new();
        manager.store_video(&mut env, file_id, &mut out);

        let mut manager2 = VideosManager::new();
        let mut parser = Parser::new(&out);
        let parsed_file_id = manager2.parse_video(&mut env, &mut parser).unwrap();
        assert_eq!(parsed_file_id, file_id);
        assert_eq!(
            manager2.get_video(parsed_file_id),
            manager.get_video(file_id)
        );
    }
}
