use crate::files::{FileId, FileManager};
use crate::photo::{self, AnimationSize, DialogPhotoStoreEnv, Dimensions, PhotoSize};
use crate::store_parse::{Parser, store_flags, store_string};
use crate::td_api;
use std::collections::HashMap;
use std::sync::Arc;

/// The `Td *td` slice of the collapsed VideosManager.
pub trait VideosManagerEnv: DialogPhotoStoreEnv {
    /// The `td_->file_manager_` handle — `get_file_object` and the file
    /// views ride it (locked per call).
    fn file_manager(&self) -> Arc<parking_lot::Mutex<FileManager>>;
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
