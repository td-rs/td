//! `td::StoryAlbum` (StoryAlbum.h/.cpp, M.47). The icon video rides the
//! [blocked: Documents/Videos managers] env seam — the C++
//! `DocumentsManager::on_get_document` and `VideosManager::get_video_object`
//! calls collapse into [`StoryManagerEnv`]; the icon photo reuses the M.4
//! photo plane with the story `FileType::PhotoStory`.

use crate::dialog_id::DialogId;
use crate::files::FileId;
use crate::photo::{Photo, get_minithumbnail_object, get_photo_sizes_object};
use crate::story_ids::StoryAlbumId;
use crate::story_manager::StoryManagerEnv;
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::StoryAlbum` (StoryAlbum.h:24-59).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StoryAlbum {
    album_id: StoryAlbumId,
    title: String,
    icon_photo: Photo,
    icon_video_file_id: FileId,
}

/// `get_photo_object` (Photo.cpp:219-238) for the story plane — the base's
/// photo plane answers the size objects; the main-animation size arm is
/// [blocked: the animation-size object lives with the photo track].
pub fn get_story_photo_object(photo: &Photo) -> Option<td_api::Photo> {
    if !photo.is_empty() {
        return Some(td_api::Photo {
            has_stickers: photo.has_stickers,
            minithumbnail: if photo.minithumbnail.is_empty() {
                None
            } else {
                Some(get_minithumbnail_object(photo.minithumbnail.as_bytes()))
            },
            sizes: get_photo_sizes_object(&photo.photos),
        });
    }
    None
}

impl StoryAlbum {
    pub fn get_story_album_id(&self) -> StoryAlbumId {
        self.album_id
    }

    /// `StoryAlbum::is_valid` (StoryAlbum.h:37-39).
    pub fn is_valid(&self) -> bool {
        self.album_id.is_valid()
    }

    /// `StoryAlbum::StoryAlbum(Td *, DialogId, storyAlbum)`
    /// (StoryAlbum.cpp:20-55). The icon-video `on_get_document` arm is the
    /// [blocked: Documents/Videos managers] env seam.
    pub fn from_telegram(
        env: &mut impl StoryManagerEnv,
        owner_dialog_id: DialogId,
        story_album: telegram_api::StoryAlbum,
    ) -> StoryAlbum {
        let mut result = StoryAlbum {
            album_id: StoryAlbumId::new(story_album.album_id),
            ..StoryAlbum::default()
        };
        if !result.album_id.is_valid() {
            tdutils::log_error!("Receive {}", result.album_id);
            result.album_id = StoryAlbumId::default();
            return result;
        }
        result.title = story_album.title;
        result.icon_photo = match story_album.icon_photo {
            Some(icon_photo) => crate::photo::get_photo(icon_photo),
            None => Photo::default(),
        };
        if let Some(document_ptr) = story_album.icon_video {
            match document_ptr {
                telegram_api::Document::documentEmpty(_) => {
                    tdutils::log_error!("Receive a story album with empty document");
                }
                telegram_api::Document::document(document) => {
                    result.icon_video_file_id = match env
                        .on_get_story_document(document, owner_dialog_id)
                    {
                        Some(file_id) => file_id,
                        None => {
                            tdutils::log_error!("Receive a story album with an invalid document");
                            FileId::default()
                        }
                    };
                }
            }
        }
        result
    }

    /// `StoryAlbum::get_file_ids` (StoryAlbum.cpp:57-61); the video-document
    /// file ids ride the [blocked: Documents managers] seam (`None`).
    pub fn get_file_ids(&self, env: &mut impl StoryManagerEnv) -> Vec<FileId> {
        let mut file_ids = crate::photo::photo_get_file_ids(&self.icon_photo);
        if self.icon_video_file_id.is_valid()
            && let Some(video_file_ids) = env.get_story_document_file_ids(self.icon_video_file_id)
        {
            file_ids.extend(video_file_ids);
        }
        file_ids
    }

    /// `StoryAlbum::get_story_album_object` (StoryAlbum.cpp:63-67) —
    /// the video icon is the full video object of the icon video.
    pub fn get_story_album_object(&self, env: &mut impl StoryManagerEnv) -> td_api::StoryAlbum {
        td_api::StoryAlbum {
            id: self.album_id.get(),
            name: self.title.clone(),
            photo_icon: get_story_photo_object(&self.icon_photo),
            video_icon: env.videos_get_video_object(self.icon_video_file_id),
        }
    }
}
