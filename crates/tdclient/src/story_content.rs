//! `td::StoryContent` + `td::StoryContentType` (StoryContent.h/.cpp,
//! StoryContentType.h/.cpp, M.47). The four content variants ride the M.12
//! file plane through [`StoryManagerEnv`]: the video file ids persist with
//! the `VideosManager::store_video`/`parse_video` collapse
//! (`FileManager::store_file`/`parse_file`), the story documents intake
//! through the [blocked: Documents/Videos managers] seam, and the live
//! streams carry the `InputGroupCallId` value plane.

use crate::dialog_id::DialogId;
use crate::files::{FileId, FileType};
use crate::photo::{Photo, PhotoSize, get_dimensions, get_photo};
use crate::store_parse::{self, Parser};
use crate::story_manager::StoryManagerEnv;
use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::status::{Result, Status};

/// `td::StoryContentType` (StoryContentType.h:13).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum StoryContentType {
    Photo,
    Video,
    /// `StoryContentUnsupported` — the default mirrors the parse fallback.
    #[default]
    Unsupported,
    LiveStream,
}

/// `can_send_story_content` (StoryContentType.cpp:13-26).
pub fn can_send_story_content(content_type: StoryContentType) -> bool {
    matches!(
        content_type,
        StoryContentType::Photo | StoryContentType::Video
    )
}

/// `can_edit_story_content` (StoryContentType.cpp:28-41).
pub fn can_edit_story_content(content_type: StoryContentType) -> bool {
    matches!(
        content_type,
        StoryContentType::Photo | StoryContentType::Video
    )
}

/// `td::InputGroupCallId` (InputGroupCallId.h:16-64) — the value-plane
/// slice the story live streams need; the group-call id resolution is the
/// GroupCallManager track.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct InputGroupCallId {
    group_call_id: i64,
    access_hash: i64,
}

impl InputGroupCallId {
    pub fn new(group_call_id: i64, access_hash: i64) -> Self {
        InputGroupCallId {
            group_call_id,
            access_hash,
        }
    }

    /// `InputGroupCallId(InputGroupCall)` (InputGroupCallId.cpp:14-23).
    pub fn from_telegram(input_group_call: &telegram_api::InputGroupCall) -> Self {
        match input_group_call {
            telegram_api::InputGroupCall::inputGroupCall(call) => {
                InputGroupCallId::new(call.id, call.access_hash)
            }
            telegram_api::InputGroupCall::inputGroupCallSlug(_) => {
                tdutils::log_error!("Receive unsupported inputGroupCallSlug");
                InputGroupCallId::default()
            }
            telegram_api::InputGroupCall::inputGroupCallInviteMessage(_) => {
                tdutils::log_error!("Receive unsupported inputGroupCallInviteMessage");
                InputGroupCallId::default()
            }
        }
    }

    /// `InputGroupCallId::get_group_call_id` (InputGroupCallId.h:49-51).
    pub fn get_group_call_id(self) -> i64 {
        self.group_call_id
    }

    /// `InputGroupCallId::store` (InputGroupCallId.h:56-59).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.group_call_id);
        store_parse::store_i64(out, self.access_hash);
    }

    /// `InputGroupCallId::parse` (InputGroupCallId.h:62-65).
    pub fn parse(parser: &mut Parser) -> std::result::Result<InputGroupCallId, &'static str> {
        let group_call_id = parser.i64_()?;
        let access_hash = parser.i64_()?;
        Ok(InputGroupCallId {
            group_call_id,
            access_hash,
        })
    }
}

/// `StoryContentUnsupported::CURRENT_VERSION` (StoryContent.cpp:63).
pub const STORY_CONTENT_UNSUPPORTED_CURRENT_VERSION: i32 = 2;

/// The four content variants of `td::StoryContent` (StoryContent.cpp:34-88)
/// as a Rust enum; areas/caption stay on the owner `Story` like the C++.
#[derive(Clone, Debug, PartialEq)]
pub enum StoryContent {
    /// `StoryContentPhoto` (StoryContent.cpp:34-45).
    Photo(Photo),
    /// `StoryContentVideo` (StoryContent.cpp:47-59).
    Video {
        file_id: FileId,
        alt_file_id: FileId,
    },
    /// `StoryContentUnsupported` (StoryContent.cpp:61-73).
    Unsupported { version: i32 },
    /// `StoryContentLiveStream` (StoryContent.cpp:75-88).
    LiveStream {
        input_group_call_id: InputGroupCallId,
        is_rtmp_stream: bool,
    },
}

impl Default for StoryContent {
    fn default() -> Self {
        StoryContent::Unsupported {
            version: STORY_CONTENT_UNSUPPORTED_CURRENT_VERSION,
        }
    }
}

impl StoryContent {
    /// `StoryContent::get_type` (StoryContent.h:23).
    pub fn get_type(&self) -> StoryContentType {
        match self {
            StoryContent::Photo(_) => StoryContentType::Photo,
            StoryContent::Video { .. } => StoryContentType::Video,
            StoryContent::Unsupported { .. } => StoryContentType::Unsupported,
            StoryContent::LiveStream { .. } => StoryContentType::LiveStream,
        }
    }

    /// The `store(content, storer)` switch (StoryContent.cpp:90-136).
    pub fn store(&self, env: &mut impl StoryManagerEnv, out: &mut Vec<u8>) {
        let content_type = self.get_type();
        store_parse::store_i32(out, content_type as i32);
        match self {
            StoryContent::Photo(photo) => {
                store_parse::store_flags(out, []);
                photo_store(photo, out);
            }
            StoryContent::Video {
                file_id,
                alt_file_id,
            } => {
                let has_alt_file_id = alt_file_id.is_valid();
                store_parse::store_flags(out, [has_alt_file_id]);
                env.videos_store_video(*file_id, out);
                if has_alt_file_id {
                    env.videos_store_video(*alt_file_id, out);
                }
            }
            StoryContent::Unsupported { version } => {
                store_parse::store_i32(out, *version);
            }
            StoryContent::LiveStream {
                input_group_call_id,
                is_rtmp_stream,
            } => {
                store_parse::store_flags(out, [*is_rtmp_stream]);
                input_group_call_id.store(out);
            }
        }
    }

    /// The `parse(content, parser)` switch (StoryContent.cpp:138-195).
    pub fn parse(
        env: &mut impl StoryManagerEnv,
        parser: &mut Parser,
    ) -> std::result::Result<StoryContent, &'static str> {
        let content_type_int = parser.i32_()?;
        let content_type = match content_type_int {
            0 => StoryContentType::Photo,
            1 => StoryContentType::Video,
            2 => StoryContentType::Unsupported,
            3 => StoryContentType::LiveStream,
            _ => {
                tdutils::log_error!(
                    "Load a story with an invalid content of type {content_type_int}"
                );
                return Ok(StoryContent::Unsupported { version: 0 });
            }
        };
        let content = match content_type {
            StoryContentType::Photo => {
                parser.parse_flags::<0>()?;
                StoryContent::Photo(photo_parse(parser)?)
            }
            StoryContentType::Video => {
                let [has_alt_file_id] = parser.parse_flags()?;
                let file_id = env.videos_parse_video(parser)?;
                let mut alt_file_id = FileId::default();
                if has_alt_file_id {
                    alt_file_id = env.videos_parse_video(parser)?;
                    if !alt_file_id.is_valid() {
                        tdutils::log_error!("Failed to parse alternative video");
                    }
                }
                StoryContent::Video {
                    file_id,
                    alt_file_id,
                }
            }
            StoryContentType::Unsupported => {
                let version = parser.i32_()?;
                StoryContent::Unsupported { version }
            }
            StoryContentType::LiveStream => {
                let [is_rtmp_stream] = parser.parse_flags()?;
                let input_group_call_id = InputGroupCallId::parse(parser)?;
                StoryContent::LiveStream {
                    input_group_call_id,
                    is_rtmp_stream,
                }
            }
        };
        Ok(content)
    }
}

/// The `Photo` byte plane (Photo.hpp store/parse) — the base's photo module
/// keeps the row bytes out of `Photo`; the story rows need them, so they
/// live here until the photo track owns the row. The `animations` and
/// `sticker_photo_size` members never occur in story content (photo stories
/// carry still sizes only, StoryContent.cpp:236); the store asserts that.
pub(crate) fn photo_store(photo: &Photo, out: &mut Vec<u8>) {
    store_parse::store_i64(out, photo.id);
    store_parse::store_i32(out, photo.date);
    store_parse::store_string(out, &photo.minithumbnail);
    store_parse::store_i32(out, photo.photos.len() as i32);
    for photo_size in &photo.photos {
        store_parse::store_i32(out, photo_size.size_type);
        store_parse::store_i32(out, photo_size.size);
        store_parse::store_i32(out, photo_size.dimensions.width as i32);
        store_parse::store_i32(out, photo_size.dimensions.height as i32);
        // the raw `FileId` pair, matching the FileId.h store contract
        store_parse::store_i32(out, photo_size.file_id.get());
        store_parse::store_i32(out, photo_size.file_id.get_remote());
        store_parse::store_i32_vec(out, &photo_size.progressive_sizes);
    }
}

/// The inverse of [`photo_store`].
pub(crate) fn photo_parse(parser: &mut Parser) -> std::result::Result<Photo, &'static str> {
    let mut photo = Photo {
        id: parser.i64_()?,
        date: parser.i32_()?,
        minithumbnail: parser.string()?,
        ..Photo::default()
    };
    let size = parser.i32_()?;
    for _ in 0..size {
        let size_type = parser.i32_()?;
        let size = parser.i32_()?;
        let width = parser.i32_()?;
        let height = parser.i32_()?;
        let id = parser.i32_()?;
        let remote_id = parser.i32_()?;
        let file_id = FileId::new(id, remote_id);
        let progressive_sizes = parser.i32_vec()?;
        photo.photos.push(PhotoSize {
            size_type,
            size,
            dimensions: crate::photo::Dimensions {
                width: width.clamp(0, u16::MAX as i32) as u16,
                height: height.clamp(0, u16::MAX as i32) as u16,
            },
            file_id,
            progressive_sizes,
        });
    }
    Ok(photo)
}

/// `get_photo_any_file_id` (Photo.cpp:264-275) — the first size's file id.
pub fn get_photo_any_file_id(photo: &Photo) -> FileId {
    photo
        .photos
        .first()
        .map(|photo_size| photo_size.file_id)
        .unwrap_or_default()
}

/// `create_photo(FileManager *, FileId, PhotoSize(), 720, 1280, stickers)`
/// (Photo.cpp:277-330) for the story geometry: one "i"-typed size with the
/// given dimensions. The minithumbnail/animation arms and the
/// sticker-callback plane are [blocked: the photo track].
fn create_story_photo(
    file_id: FileId,
    width: i32,
    height: i32,
    _sticker_file_ids: Vec<FileId>,
) -> Photo {
    let mut photo = Photo::default();
    photo.photos.push(PhotoSize {
        size_type: 'i' as i32,
        size: 0,
        dimensions: get_dimensions(width, height, "create_story_photo"),
        file_id,
        progressive_sizes: Vec::new(),
    });
    photo
}

/// `get_input_story_content` (StoryContent.cpp:302-349). The file-id halves
/// ride the M.12 plane (`FileManager::get_input_file_id`/`copy_file_id`);
/// the attached-sticker ids are the [blocked: StickersManager] seam and the
/// photo/video record builders keep the C++ geometry.
pub fn get_input_story_content(
    env: &mut impl StoryManagerEnv,
    input_story_content: Option<td_api::InputStoryContent>,
    owner_dialog_id: DialogId,
) -> Result<StoryContent> {
    let Some(input_story_content) = input_story_content else {
        return Result::Err(Status::error(400, "Input story content must be non-empty"));
    };
    match input_story_content {
        td_api::InputStoryContent::inputStoryContentPhoto(input_story) => {
            let file_id = match env.get_input_file_id(
                FileType::Photo,
                input_story.photo.as_ref(),
                owner_dialog_id,
                false,
                false,
            ) {
                Result::Ok(value) => value,
                Result::Err(error) => return Result::Err(error),
            };
            let file_id = env.copy_file_id(
                file_id,
                FileType::PhotoStory,
                owner_dialog_id,
                "get_input_story_content",
            );
            let sticker_file_ids =
                env.get_attached_sticker_file_ids(&input_story.added_sticker_file_ids);
            Result::Ok(StoryContent::Photo(create_story_photo(
                file_id,
                720,
                1280,
                sticker_file_ids,
            )))
        }
        td_api::InputStoryContent::inputStoryContentVideo(input_story) => {
            let file_id = match env.get_input_file_id(
                FileType::Video,
                input_story.video.as_ref(),
                owner_dialog_id,
                false,
                false,
            ) {
                Result::Ok(value) => value,
                Result::Err(error) => return Result::Err(error),
            };
            if !(0.0..=60.0).contains(&input_story.duration) {
                return Result::Err(Status::error(400, "Invalid video duration specified"));
            }
            if input_story.cover_frame_timestamp < 0.0 {
                return Result::Err(Status::error(400, "Wrong cover timestamp specified"));
            }
            let file_id = env.copy_file_id(
                file_id,
                FileType::VideoStory,
                owner_dialog_id,
                "get_input_story_content",
            );
            let sticker_file_ids =
                env.get_attached_sticker_file_ids(&input_story.added_sticker_file_ids);
            let has_stickers = !sticker_file_ids.is_empty();
            env.videos_create_story_video(
                file_id,
                (input_story.duration).ceil() as i32,
                input_story.duration,
                get_dimensions(720, 1280, ""),
                input_story.is_animation,
                input_story.cover_frame_timestamp,
                has_stickers,
                sticker_file_ids,
            );
            Result::Ok(StoryContent::Video {
                file_id,
                alt_file_id: FileId::default(),
            })
        }
    }
}

/// `get_story_content` (StoryContent.cpp:225-300).
pub fn get_story_content(
    env: &mut impl StoryManagerEnv,
    media_ptr: telegram_api::MessageMedia,
    owner_dialog_id: DialogId,
    is_bot_preview: bool,
) -> Option<StoryContent> {
    match media_ptr {
        telegram_api::MessageMedia::messageMediaPhoto(media) => {
            if media.photo.is_none() || media.ttl_seconds != 0 || media.spoiler || media.live_photo
            {
                tdutils::log_error!("Receive a story with content messageMediaPhoto");
                return None;
            }
            let photo = get_photo(media.photo.expect("checked above"));
            if photo.is_empty() {
                tdutils::log_error!("Receive a story with empty photo");
                return None;
            }
            Some(StoryContent::Photo(photo))
        }
        telegram_api::MessageMedia::messageMediaDocument(media) => {
            if media.document.is_none() || media.ttl_seconds != 0 || media.spoiler {
                tdutils::log_error!("Receive a story with content messageMediaDocument");
                return None;
            }
            let document_ptr = media.document.expect("checked above");
            let document = match document_ptr {
                telegram_api::Document::documentEmpty(_) => {
                    tdutils::log_error!("Receive a story with empty document");
                    return None;
                }
                telegram_api::Document::document(document) => document,
            };
            let file_id = match env.on_get_story_document(document, owner_dialog_id) {
                Some(file_id) if file_id.is_valid() => file_id,
                _ => {
                    tdutils::log_error!("Receive a story with an invalid document");
                    return None;
                }
            };

            let mut alt_file_id = FileId::default();
            if media.alt_documents.len() == 1 {
                match &media.alt_documents[0] {
                    telegram_api::Document::documentEmpty(_) => {
                        tdutils::log_error!("Receive alternative documentEmpty");
                    }
                    telegram_api::Document::document(alt) => {
                        match env.on_get_story_document(alt.clone(), owner_dialog_id) {
                            Some(parsed) if parsed.is_valid() => alt_file_id = parsed,
                            _ => {
                                tdutils::log_error!("Receive invalid alternative document");
                            }
                        }
                    }
                }
            }

            Some(StoryContent::Video {
                file_id,
                alt_file_id,
            })
        }
        telegram_api::MessageMedia::messageMediaVideoStream(media) => {
            if is_bot_preview {
                return None;
            }
            let input_group_call_id = match &media.call {
                Some(call) => InputGroupCallId::from_telegram(call),
                None => InputGroupCallId::default(),
            };
            Some(StoryContent::LiveStream {
                input_group_call_id,
                is_rtmp_stream: media.rtmp_stream,
            })
        }
        telegram_api::MessageMedia::messageMediaUnsupported(_) => Some(StoryContent::Unsupported {
            version: STORY_CONTENT_UNSUPPORTED_CURRENT_VERSION,
        }),
        _ => None,
    }
}

/// `compare_story_contents` (StoryContent.cpp:389-434) — the C++
/// out-parameters become the returned `(is_content_changed, need_update)`
/// pair.
pub fn compare_story_contents(
    old_content: &StoryContent,
    new_content: &StoryContent,
) -> (bool, bool) {
    let content_type = new_content.get_type();
    if old_content.get_type() != content_type {
        return (false, true);
    }
    match (old_content, new_content) {
        (StoryContent::Photo(old_), StoryContent::Photo(new_)) => (false, old_ != new_),
        (
            StoryContent::Video {
                file_id,
                alt_file_id,
            },
            StoryContent::Video {
                file_id: new_file_id,
                alt_file_id: new_alt_file_id,
            },
        ) => (
            false,
            file_id != new_file_id || alt_file_id != new_alt_file_id,
        ),
        (
            StoryContent::Unsupported { version },
            StoryContent::Unsupported {
                version: new_version,
            },
        ) => (version != new_version, false),
        (
            StoryContent::LiveStream {
                input_group_call_id,
                is_rtmp_stream,
            },
            StoryContent::LiveStream {
                input_group_call_id: new_input_group_call_id,
                is_rtmp_stream: new_is_rtmp_stream,
            },
        ) => (
            false,
            input_group_call_id != new_input_group_call_id || is_rtmp_stream != new_is_rtmp_stream,
        ),
        _ => {
            tdutils::log_error!("Compare mismatched story contents");
            (false, false)
        }
    }
}

/// `merge_story_contents` (StoryContent.cpp:436-476) — the photo merge is
/// the [blocked: photo merge] env seam; the other variants only compare.
pub fn merge_story_contents(
    env: &mut impl StoryManagerEnv,
    old_content: &StoryContent,
    new_content: &mut StoryContent,
    dialog_id: DialogId,
    is_content_changed: &mut bool,
    need_update: &mut bool,
) {
    match (old_content, new_content) {
        (StoryContent::Photo(old_), StoryContent::Photo(new_)) => {
            let (is_changed, need_update_) = {
                let mut old_photo = old_.clone();
                let mut new_photo = new_.clone();
                env.merge_photos(&mut old_photo, &mut new_photo, dialog_id, false)
            };
            *is_content_changed |= is_changed;
            *need_update |= need_update_;
        }
        (
            StoryContent::Video {
                file_id,
                alt_file_id,
            },
            StoryContent::Video {
                file_id: new_file_id,
                alt_file_id: new_alt_file_id,
            },
        ) => {
            if file_id != new_file_id || alt_file_id != new_alt_file_id {
                *need_update = true;
            }
        }
        (
            StoryContent::Unsupported { version },
            StoryContent::Unsupported {
                version: new_version,
            },
        ) => {
            if version != new_version {
                *is_content_changed = true;
            }
        }
        (
            StoryContent::LiveStream {
                input_group_call_id,
                is_rtmp_stream,
            },
            StoryContent::LiveStream {
                input_group_call_id: new_input_group_call_id,
                is_rtmp_stream: new_is_rtmp_stream,
            },
        ) => {
            if input_group_call_id != new_input_group_call_id
                || is_rtmp_stream != new_is_rtmp_stream
            {
                *need_update = true;
            }
        }
        _ => {
            tdutils::log_error!("Merge mismatched story contents");
        }
    }
}

/// `copy_story_content` (StoryContent.cpp:478-503) — the enum clones.
pub fn copy_story_content(content: &StoryContent) -> StoryContent {
    content.clone()
}

/// `get_story_content_object` (StoryContent.cpp:505-535).
pub fn get_story_content_object(
    env: &mut impl StoryManagerEnv,
    content: &StoryContent,
    owner_dialog_id: DialogId,
) -> td_api::StoryContent {
    match content {
        StoryContent::Photo(photo) => match crate::story_album::get_story_photo_object(photo) {
            Some(photo_object) => {
                td_api::StoryContent::storyContentPhoto(td_api::storyContentPhoto {
                    photo: Some(photo_object),
                })
            }
            None => td_api::StoryContent::storyContentUnsupported(td_api::storyContentUnsupported),
        },
        StoryContent::Video {
            file_id,
            alt_file_id,
        } => td_api::StoryContent::storyContentVideo(td_api::storyContentVideo {
            video: env.videos_get_story_video_object(*file_id),
            alternative_video: env.videos_get_story_video_object(*alt_file_id),
        }),
        StoryContent::Unsupported { .. } => {
            td_api::StoryContent::storyContentUnsupported(td_api::storyContentUnsupported)
        }
        StoryContent::LiveStream {
            input_group_call_id,
            is_rtmp_stream,
        } => td_api::StoryContent::storyContentLive(td_api::storyContentLive {
            group_call_id: env.get_group_call_id(*input_group_call_id, owner_dialog_id),
            is_rtmp_stream: *is_rtmp_stream,
        }),
    }
}

/// `get_story_content_any_file_id` (StoryContent.cpp:537-548).
pub fn get_story_content_any_file_id(content: &StoryContent) -> FileId {
    match content {
        StoryContent::Photo(photo) => get_photo_any_file_id(photo),
        StoryContent::Video { file_id, .. } => *file_id,
        StoryContent::Unsupported { .. } | StoryContent::LiveStream { .. } => FileId::default(),
    }
}

/// `get_story_content_file_ids` (StoryContent.cpp:550-566).
pub fn get_story_content_file_ids(
    env: &mut impl StoryManagerEnv,
    content: &StoryContent,
) -> Vec<FileId> {
    match content {
        StoryContent::Photo(photo) => crate::photo::photo_get_file_ids(photo),
        StoryContent::Video {
            file_id,
            alt_file_id,
        } => {
            let mut result = Vec::new();
            if let Some(file_ids) = env.get_story_document_file_ids(*file_id) {
                result.extend(file_ids);
            }
            if let Some(file_ids) = env.get_story_document_file_ids(*alt_file_id) {
                result.extend(file_ids);
            }
            result
        }
        StoryContent::Unsupported { .. } | StoryContent::LiveStream { .. } => Vec::new(),
    }
}

/// `get_story_content_duration` (StoryContent.cpp:568-578).
pub fn get_story_content_duration(env: &mut impl StoryManagerEnv, content: &StoryContent) -> i32 {
    match content {
        StoryContent::Video { file_id, .. } => env.videos_get_video_duration(*file_id),
        _ => -1,
    }
}
