//! Port of `td/telegram/AnimationsManager.{h,cpp,hpp}` (M.20) — the
//! saved-animations ring, the animation record plane and the animation
//! search-parameters surface.
//!
//! The manager is the collapsed actor: `Td` owns the struct, the
//! [`AnimationsManagerEnv`] trait carries the `G()`/option/pmc/
//! cross-manager surfaces, and every `create_handler<...>` network tail
//! is an env seam marked `[blocked: net]` — the C++ promise lands in the
//! query and stays unanswered offline, exactly like the collapsed-line
//! convention.
//!
//! The saved-animations pmc record (the sqlite-pmc `"ans"` key) is the
//! byte-exact `AnimationListLogEvent` framing: the log-event version
//! prefix, the `int32` size, then `store_animation` per item
//! (AnimationsManager.hpp).
//!
//! Deferred intakes:
//! - `on_get_saved_animations`'s per-document conversion is the
//!   [`AnimationsManagerEnv::on_get_document_animation`] seam
//!   `[blocked: M.28 DocumentsManager]`; the manager-side control flow
//!   (ctor checks, the empty/non-animation skips, the repair split and
//!   the hash fold) is fully ported and tested against wire shapes.
//! - `get_secret_input_media` is not ported — it builds the
//!   `SecretInputMedia` value (SecretInputMedia.h), which lands with the
//!   secret-chat line (`[blocked: SecretInputMedia]`).

use crate::files::{FileId, FileSourceId, FullRemoteFileLocation};
use crate::photo::{AnimationSize, DialogPhotoStoreEnv, Dimensions, PhotoFormat, PhotoSize};
use crate::store_parse::{Parser, store_flags, store_i32, store_log_event_version, version};
use crate::td_api;
use std::collections::HashMap;
use std::sync::Arc;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;

/// The `Td *td` slice of the collapsed AnimationsManager.
pub trait AnimationsManagerEnv: DialogPhotoStoreEnv {
    // -- the G()/auth slice
    fn auth_is_bot(&self) -> bool;
    fn close_flag(&self) -> bool;

    // -- options
    fn get_option_string(&self, name: &str, default_value: &str) -> String;
    fn get_option_integer(&self, name: &str, default_value: i64) -> i64;

    // -- the pmc slice
    /// `G()->td_db()->get_binlog_pmc()->erase` — the legacy
    /// `saved_animations_limit` key erase of the constructor.
    fn binlog_pmc_erase(&mut self, key: &str);
    /// `G()->use_sqlite_pmc()`.
    fn use_sqlite_pmc(&self) -> bool;
    /// `G()->td_db()->get_sqlite_pmc()->get` — the `"ans"` record read;
    /// the async callback is inlined like the collapsed-line convention.
    fn sqlite_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;
    /// `G()->td_db()->get_sqlite_pmc()->set`.
    fn sqlite_pmc_set_raw(&mut self, key: &[u8], value: &[u8]);

    // -- the update plane
    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    // -- the cross-manager surfaces
    /// The `FileManager` handle behind the file-view probes
    /// (`td_->file_manager_->get_file_view/get_file_object/merge`).
    fn file_manager(&self) -> Arc<parking_lot::Mutex<crate::files::FileManager>>;
    /// `td_->file_reference_manager_->create_saved_animations_file_source()`.
    fn create_saved_animations_file_source(&mut self) -> FileSourceId;
    /// `td_->file_manager_->change_files_source(...)` — the registered
    /// file-source bookkeeping (`FileManager.cpp:change_files_source`).
    fn change_files_source(
        &mut self,
        file_source_id: FileSourceId,
        old_file_ids: &[FileId],
        new_file_ids: &[FileId],
        source: &str,
    );

    // -- the `[blocked: net]` query tails; each one drops the promise,
    // -- which keeps the C++ request unanswered offline
    /// `GetSavedGifsQuery::send(is_repair, hash)`
    /// (AnimationsManager.cpp:39-64).
    fn send_get_saved_gifs_query(&mut self, is_repair: bool, hash: i64);
    /// `SaveGifQuery::send(file_id, input_document, unsave)`
    /// (AnimationsManager.cpp:66-124); the input document is resolved
    /// locally, the query send and the file-reference repair half are
    /// inside the net seam.
    fn send_save_gif_query_net(
        &mut self,
        file_id: FileId,
        unsave: bool,
        input_document: telegram_api::InputDocument,
        promise: Promise<()>,
    );

    // -- the `[blocked: M.28 DocumentsManager]` intake seam
    /// `td_->documents_manager_->on_get_document(std::move(document), ...)`
    /// — answers `None` until the document plane lands, which the caller
    /// handles exactly like the C++ non-animation case.
    fn on_get_document_animation(&mut self, document: telegram_api::document) -> Option<FileId>;
}

/// `AnimationsManager::Animation` (AnimationsManager.h:103-117).
#[derive(Clone, Debug, Default)]
pub struct Animation {
    pub(crate) file_name: String,
    pub(crate) mime_type: String,
    pub(crate) duration: i32,
    pub(crate) dimensions: Dimensions,
    pub(crate) minithumbnail: String,
    pub(crate) thumbnail: PhotoSize,
    pub(crate) animated_thumbnail: AnimationSize,

    pub(crate) has_stickers: bool,
    pub(crate) sticker_file_ids: Vec<FileId>,

    pub(crate) file_id: FileId,
}

/// The request continuations the collapsed line parks while the saved
/// animations load — the C++ parks the request actor's promise in
/// `load_saved_animations_queries_` and the actor re-runs `do_run` on
/// completion; the collapsed line completes the final result directly
/// (the M.16 pending-promise form).
pub(crate) enum SavedAnimationCall {
    /// `get_saved_animations` — the ids are re-fetched and the object
    /// built at completion.
    Get(Promise<td_api::Animations>),
    /// `add_saved_animation` — the whole request re-runs.
    Add(Option<td_api::InputFile>, Promise<()>),
    /// `remove_saved_animation` — the whole request re-runs.
    Remove(Option<td_api::InputFile>, Promise<()>),
    /// `add_saved_animation_impl`'s self-continuation
    /// (AnimationsManager.cpp:683-694).
    AddImpl(FileId, bool, Promise<()>),
}

/// `AnimationsManager` (AnimationsManager.h:29-166) — the collapsed actor.
#[derive(Default)]
pub struct AnimationsManager {
    animations: HashMap<FileId, Animation>,

    saved_animations_limit: i32,
    saved_animation_ids: Vec<FileId>,
    saved_animation_file_ids: Vec<FileId>,
    next_saved_animations_load_time: f64,
    are_saved_animations_being_loaded: bool,
    are_saved_animations_loaded: bool,
    load_saved_animations_queries: Vec<Promise<()>>,
    repair_saved_animations_queries: Vec<Promise<()>>,
    pending_saved_animation_calls: Vec<SavedAnimationCall>,
    saved_animations_file_source_id: FileSourceId,

    animation_search_emojis: String,
    animation_search_provider: String,
    is_animation_search_emojis_inited: bool,
    is_animation_search_provider_inited: bool,
}

impl std::fmt::Debug for AnimationsManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnimationsManager")
            .field("animation_count", &self.animations.len())
            .field("saved_animations_limit", &self.saved_animations_limit)
            .field("saved_animation_count", &self.saved_animation_ids.len())
            .field(
                "are_saved_animations_loaded",
                &self.are_saved_animations_loaded,
            )
            .field("search_emojis", &self.animation_search_emojis)
            .field("search_provider", &self.animation_search_provider)
            .finish()
    }
}

fn set_promises(promises: &mut Vec<Promise<()>>) {
    for mut promise in promises.drain(..) {
        promise.set_value(());
    }
}

fn fail_promises(promises: &mut Vec<Promise<()>>, error: &tdutils::status::Status) {
    for mut promise in promises.drain(..) {
        promise.set_error(error.clone());
    }
}

/// `td::add_to_top_if` (tdutils/algorithm.h:140-162).
fn add_to_top_if(
    v: &mut Vec<FileId>,
    max_size: usize,
    value: FileId,
    is_equal_to_value: &dyn Fn(FileId) -> bool,
) {
    let size = v.len();
    let mut i = 0;
    let mut value = value;
    while i < size {
        if is_equal_to_value(v[i]) {
            value = v[i];
            break;
        }
        i += 1;
    }
    if i == size {
        if size < max_size || i == 0 {
            v.push(value);
        } else {
            i -= 1;
        }
    }
    while i > 0 {
        v[i] = v[i - 1];
        i -= 1;
    }
    v[0] = value;
}

/// `FullRemoteFileLocation::as_input_document` (FileLocation.h:473-478)
/// without the C++ macro CHECKs — the caller validated `is_document`.
fn as_input_document(location: &FullRemoteFileLocation) -> telegram_api::InputDocument {
    debug_assert!(location.is_document());
    telegram_api::InputDocument::inputDocument(telegram_api::inputDocument {
        id: location.get_id(),
        access_hash: location.get_access_hash(),
        file_reference: location.get_file_reference().as_bytes().to_vec(),
    })
}

/// `FileManager::get_input_documents` (FileManager.cpp) over the
/// full-remote-location probes; the C++ CHECKs every location.
fn get_input_documents(
    file_manager: &crate::files::FileManager,
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

impl AnimationsManager {
    pub fn new() -> AnimationsManager {
        AnimationsManager {
            saved_animations_limit: 200,
            ..AnimationsManager::default()
        }
    }

    /// The constructor body (AnimationsManager.cpp:126-134): the three
    /// option loads, the reload clock and the legacy limit-key erase.
    pub fn init(&mut self, env: &mut impl AnimationsManagerEnv) {
        self.on_update_animation_search_emojis(env);
        self.on_update_animation_search_provider(env);
        self.on_update_saved_animations_limit(env);

        self.next_saved_animations_load_time = tdutils::time::now();

        env.binlog_pmc_erase("saved_animations_limit"); // legacy
    }

    /// `get_animation_duration` (AnimationsManager.cpp:144-148).
    pub fn get_animation_duration(&self, file_id: FileId) -> i32 {
        self.get_animation(file_id)
            .expect("get_animation_duration: missing animation")
            .duration
    }

    /// `get_animation_object` (AnimationsManager.cpp:150-162) — an
    /// invalid id answers `None` (the C++ `nullptr`).
    pub fn get_animation_object(
        &self,
        env: &impl AnimationsManagerEnv,
        file_id: FileId,
    ) -> Option<td_api::Animation> {
        if !file_id.is_valid() {
            return None;
        }

        let animation = self
            .get_animation(file_id)
            .expect("get_animation_object: missing animation");
        let thumbnail = get_animation_thumbnail_object(env, animation);
        Some(td_api::Animation {
            duration: animation.duration,
            width: animation.dimensions.width as i32,
            height: animation.dimensions.height as i32,
            file_name: animation.file_name.clone(),
            mime_type: animation.mime_type.clone(),
            has_stickers: animation.has_stickers,
            minithumbnail: Some(get_minithumbnail_object(animation)),
            thumbnail,
            animation: Some(env.file_manager().lock().get_file_object(file_id)),
        })
    }

    /// `get_animations_object` (AnimationsManager.cpp:164-167).
    pub fn get_animations_object(
        &self,
        env: &impl AnimationsManagerEnv,
        file_ids: &[FileId],
    ) -> td_api::Animations {
        td_api::Animations {
            animations: file_ids
                .iter()
                .map(|file_id| self.get_animation_object(env, *file_id).unwrap_or_default())
                .collect(),
        }
    }

    /// `on_get_animation` (AnimationsManager.cpp:169-201).
    pub fn on_get_animation(&mut self, new_animation: Animation, replace: bool) -> FileId {
        let file_id = new_animation.file_id;
        assert!(file_id.is_valid());
        let has_old = self.animations.contains_key(&file_id);
        tdutils::log_info!(
            "{} animation {} of size {}x{}",
            if !has_old {
                "Add"
            } else if replace {
                "Replace"
            } else {
                "Ignore"
            },
            file_id,
            new_animation.dimensions.width,
            new_animation.dimensions.height
        );
        if !has_old {
            self.animations.insert(file_id, new_animation);
        } else if replace {
            let a = self
                .animations
                .get_mut(&file_id)
                .expect("on_get_animation: entry vanished");
            debug_assert!(a.file_id == file_id);
            if a.mime_type != new_animation.mime_type
                || a.file_name != new_animation.file_name
                || a.dimensions != new_animation.dimensions
                || a.duration != new_animation.duration
                || a.minithumbnail != new_animation.minithumbnail
                || a.thumbnail != new_animation.thumbnail
                || a.animated_thumbnail != new_animation.animated_thumbnail
            {
                tdutils::log_debug!("Animation {file_id} info has changed");
                a.mime_type = new_animation.mime_type;
                a.file_name = new_animation.file_name;
                a.dimensions = new_animation.dimensions;
                a.duration = new_animation.duration;
                a.minithumbnail = new_animation.minithumbnail;
                a.thumbnail = new_animation.thumbnail;
                a.animated_thumbnail = new_animation.animated_thumbnail;
            }
            if a.has_stickers != new_animation.has_stickers && new_animation.has_stickers {
                a.has_stickers = new_animation.has_stickers;
            }
            if a.sticker_file_ids != new_animation.sticker_file_ids
                && !new_animation.sticker_file_ids.is_empty()
            {
                a.sticker_file_ids = new_animation.sticker_file_ids;
            }
        }

        file_id
    }

    /// `get_animation` (AnimationsManager.cpp:203-205).
    fn get_animation(&self, file_id: FileId) -> Option<&Animation> {
        self.animations.get(&file_id)
    }

    /// `get_animation_thumbnail_file_id` (AnimationsManager.cpp:207-211).
    pub fn get_animation_thumbnail_file_id(&self, file_id: FileId) -> FileId {
        self.get_animation(file_id)
            .expect("get_animation_thumbnail_file_id: missing animation")
            .thumbnail
            .file_id
    }

    /// `get_animation_animated_thumbnail_file_id`
    /// (AnimationsManager.cpp:213-217).
    pub fn get_animation_animated_thumbnail_file_id(&self, file_id: FileId) -> FileId {
        self.get_animation(file_id)
            .expect("get_animation_animated_thumbnail_file_id: missing animation")
            .animated_thumbnail
            .photo_size
            .file_id
    }

    /// `delete_animation_thumbnail` (AnimationsManager.cpp:219-224).
    pub fn delete_animation_thumbnail(&mut self, file_id: FileId) {
        let Some(animation) = self.animations.get_mut(&file_id) else {
            panic!("delete_animation_thumbnail: missing animation");
        };
        animation.thumbnail = PhotoSize::default();
        animation.animated_thumbnail = AnimationSize::default();
    }

    /// `dup_animation` (AnimationsManager.cpp:226-237).
    pub fn dup_animation(&mut self, new_id: FileId, old_id: FileId) -> FileId {
        tdutils::log_info!("Dup animation {} to {}", old_id, new_id);
        let old_animation = self
            .get_animation(old_id)
            .expect("dup_animation: missing animation")
            .clone();
        if self.animations.contains_key(&new_id) {
            return new_id;
        }
        let mut new_animation = old_animation;
        new_animation.file_id = new_id;
        self.animations.insert(new_id, new_animation);
        new_id
    }

    /// `merge_animations` (AnimationsManager.cpp:239-262).
    pub fn merge_animations(
        &mut self,
        env: &mut impl AnimationsManagerEnv,
        new_id: FileId,
        old_id: FileId,
    ) {
        assert!(old_id.is_valid() && new_id.is_valid());
        assert!(new_id != old_id);

        tdutils::log_info!("Merge animations {} and {}", new_id, old_id);
        let old_ = self
            .get_animation(old_id)
            .expect("merge_animations: missing animation")
            .clone();

        let mut need_merge = true;
        if let Some(new_) = self.get_animation(new_id) {
            // the C++ compares `file_name == file_name + extension`, i.e.
            // the new name carrying a ".mp4"/".webm" suffix the old lacks
            let mp4_suffix = format!("{}.mp4", old_.file_name);
            let webm_suffix = format!("{}.webm", old_.file_name);
            if new_.file_name == mp4_suffix || new_.file_name == webm_suffix {
                need_merge = false;
            }
        } else {
            self.dup_animation(new_id, old_id);
        }
        if need_merge && let Err(status) = env.file_manager().lock().merge(new_id, old_id) {
            tdutils::log_error!("Merge animations failed: {}", status);
        }
    }

    /// `create_animation` (AnimationsManager.cpp:264-282).
    #[allow(clippy::too_many_arguments)]
    pub fn create_animation(
        &mut self,
        env: &mut impl AnimationsManagerEnv,
        file_id: FileId,
        minithumbnail: String,
        thumbnail: PhotoSize,
        animated_thumbnail: AnimationSize,
        has_stickers: bool,
        sticker_file_ids: Vec<FileId>,
        file_name: String,
        mime_type: String,
        duration: i32,
        dimensions: Dimensions,
        replace: bool,
    ) {
        let mut a = Animation {
            file_id,
            file_name,
            mime_type,
            duration: duration.max(0),
            dimensions,
            minithumbnail: String::new(),
            thumbnail,
            animated_thumbnail,
            has_stickers,
            sticker_file_ids,
        };
        if !env.auth_is_bot() {
            a.minithumbnail = minithumbnail;
        }
        self.on_get_animation(a, replace);
    }

    /// `get_input_media` (AnimationsManager.cpp:284-338) — `None` is the
    /// C++ `nullptr`.
    pub fn get_input_media(
        &self,
        env: &impl AnimationsManagerEnv,
        file_id: FileId,
        input_file: Option<telegram_api::InputFile>,
        input_thumbnail: Option<telegram_api::InputFile>,
        has_spoiler: bool,
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
                    spoiler: has_spoiler,
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
                    spoiler: has_spoiler,
                    url: url.to_owned(),
                    ttl_seconds: 0,
                    video_cover: None,
                    video_timestamp: 0,
                },
            ));
        }

        if let Some(input_file) = input_file {
            let animation = self
                .get_animation(file_id)
                .expect("get_input_media: missing animation");

            let mut attributes: Vec<telegram_api::DocumentAttribute> = Vec::new();
            if !animation.file_name.is_empty() {
                attributes.push(telegram_api::DocumentAttribute::documentAttributeFilename(
                    telegram_api::documentAttributeFilename {
                        file_name: animation.file_name.clone(),
                    },
                ));
            }
            let mut mime_type = animation.mime_type.clone();
            if mime_type == "video/mp4" {
                attributes.push(telegram_api::DocumentAttribute::documentAttributeVideo(
                    telegram_api::documentAttributeVideo {
                        flags: 0,
                        round_message: false,
                        supports_streaming: false,
                        nosound: false,
                        duration: animation.duration as f64,
                        w: animation.dimensions.width as i32,
                        h: animation.dimensions.height as i32,
                        preload_prefix_size: 0,
                        video_start_ts: 0.0,
                        video_codec: String::new(),
                    },
                ));
            } else if animation.dimensions.width != 0 && animation.dimensions.height != 0 {
                if !mime_type.starts_with("image/") {
                    mime_type = "image/gif".to_owned();
                }
                attributes.push(telegram_api::DocumentAttribute::documentAttributeImageSize(
                    telegram_api::documentAttributeImageSize {
                        w: animation.dimensions.width as i32,
                        h: animation.dimensions.height as i32,
                    },
                ));
            }
            let mut flags = 0;
            // inputMediaUploadedDocument::STICKERS_MASK (flags.0) and
            // ::THUMB_MASK (flags.2) of the pinned scheme
            const STICKERS_MASK: i32 = 1;
            const THUMB_MASK: i32 = 1 << 2;
            let added_stickers = if animation.has_stickers {
                flags |= STICKERS_MASK;
                get_input_documents(&file_manager, &animation.sticker_file_ids)
            } else {
                Vec::new()
            };
            if input_thumbnail.is_some() {
                flags |= THUMB_MASK;
            }
            return Some(telegram_api::InputMedia::inputMediaUploadedDocument(
                telegram_api::inputMediaUploadedDocument {
                    flags,
                    nosound_video: false,
                    force_file: false,
                    spoiler: has_spoiler,
                    file: Some(input_file),
                    thumb: input_thumbnail,
                    mime_type,
                    attributes,
                    stickers: added_stickers,
                    video_cover: None,
                    video_timestamp: 0,
                    ttl_seconds: 0,
                },
            ));
        }
        assert!(file_view.get_main_remote_location().is_none());

        None
    }

    /// `on_update_animation_search_emojis`
    /// (AnimationsManager.cpp:383-399).
    pub fn on_update_animation_search_emojis(&mut self, env: &mut impl AnimationsManagerEnv) {
        if env.close_flag() {
            return;
        }
        if env.auth_is_bot() {
            return;
        }

        let animation_search_emojis = env.get_option_string("animation_search_emojis", "");
        self.is_animation_search_emojis_inited = true;
        if self.animation_search_emojis == animation_search_emojis {
            return;
        }
        self.animation_search_emojis = animation_search_emojis;

        self.try_send_update_animation_search_parameters(env);
    }

    /// `on_update_animation_search_provider`
    /// (AnimationsManager.cpp:401-417).
    pub fn on_update_animation_search_provider(&mut self, env: &mut impl AnimationsManagerEnv) {
        if env.close_flag() {
            return;
        }
        if env.auth_is_bot() {
            return;
        }

        let animation_search_provider = env.get_option_string("animation_search_provider", "");
        self.is_animation_search_provider_inited = true;
        if self.animation_search_provider == animation_search_provider {
            return;
        }
        self.animation_search_provider = animation_search_provider;

        self.try_send_update_animation_search_parameters(env);
    }

    /// `on_update_saved_animations_limit`
    /// (AnimationsManager.cpp:419-440).
    pub fn on_update_saved_animations_limit(&mut self, env: &mut impl AnimationsManagerEnv) {
        if env.close_flag() {
            return;
        }
        if env.auth_is_bot() {
            return;
        }
        let saved_animations_limit = env.get_option_integer("saved_animations_limit", 200) as i32;
        if saved_animations_limit != self.saved_animations_limit {
            if saved_animations_limit > 0 {
                tdutils::log_info!("Update saved animations limit to {saved_animations_limit}");
                self.saved_animations_limit = saved_animations_limit;
                if self.saved_animation_ids.len() as i32 > self.saved_animations_limit {
                    self.saved_animation_ids
                        .resize(self.saved_animations_limit as usize, FileId::default());
                    self.send_update_saved_animations(env, false);
                }
            } else {
                tdutils::log_error!(
                    "Receive wrong saved animations limit = {saved_animations_limit}"
                );
            }
        }
    }

    /// `reload_saved_animations` (AnimationsManager.cpp:471-482); the
    /// `GetSavedGifsQuery` send is the `[blocked: net]` seam.
    pub fn reload_saved_animations(&mut self, env: &mut impl AnimationsManagerEnv, force: bool) {
        if env.close_flag() {
            return;
        }

        if !env.auth_is_bot()
            && !self.are_saved_animations_being_loaded
            && (self.next_saved_animations_load_time < tdutils::time::now() || force)
        {
            if force {
                tdutils::log_info!("Reload saved animations");
            }
            self.are_saved_animations_being_loaded = true;
            let hash = self.get_saved_animations_hash(env, "reload_saved_animations");
            env.send_get_saved_gifs_query(false, hash);
        }
    }

    /// `repair_saved_animations` (AnimationsManager.cpp:484-493).
    pub fn repair_saved_animations(
        &mut self,
        env: &mut impl AnimationsManagerEnv,
        mut promise: Promise<()>,
    ) {
        if env.auth_is_bot() {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Bots have no saved animations",
            ));
            return;
        }

        let is_first = self.repair_saved_animations_queries.is_empty();
        self.repair_saved_animations_queries.push(promise);
        if is_first {
            env.send_get_saved_gifs_query(true, 0);
        }
    }

    /// `get_saved_animations` (AnimationsManager.cpp:495-504) — the C++
    /// returns the ids and re-runs the request actor after the load; the
    /// collapsed line parks the final object promise.
    pub fn get_saved_animations(
        &mut self,
        env: &mut impl AnimationsManagerEnv,
        mut promise: Promise<td_api::Animations>,
    ) {
        if !self.are_saved_animations_loaded {
            self.pending_saved_animation_calls
                .push(SavedAnimationCall::Get(promise));
            self.load_saved_animations(env, Promise::default());
            return;
        }
        self.reload_saved_animations(env, false);

        promise.set_value(self.get_animations_object(env, &self.saved_animation_ids));
    }

    /// `load_saved_animations` (AnimationsManager.cpp:506-528) with the
    /// sqlite-pmc callback inlined; `promise` keeps the C++ queue role
    /// (the initiation gate and the failure fan-out) — the re-runnable
    /// callers pass the empty promise and park typed continuations
    /// instead.
    fn load_saved_animations(
        &mut self,
        env: &mut impl AnimationsManagerEnv,
        mut promise: Promise<()>,
    ) {
        if env.auth_is_bot() {
            self.are_saved_animations_loaded = true;
        }
        if self.are_saved_animations_loaded {
            promise.set_value(());
            return;
        }
        let is_first = self.load_saved_animations_queries.is_empty();
        self.load_saved_animations_queries.push(promise);
        if is_first {
            if env.use_sqlite_pmc() {
                tdutils::log_info!("Trying to load saved animations from database");
                let value = env.sqlite_pmc_get_raw(b"ans");
                self.on_load_saved_animations_from_database(env, &value);
            } else {
                tdutils::log_info!("Trying to load saved animations from server");
                self.reload_saved_animations(env, true);
            }
        }
    }

    /// `on_load_saved_animations_from_database`
    /// (AnimationsManager.cpp:530-546).
    pub fn on_load_saved_animations_from_database(
        &mut self,
        env: &mut impl AnimationsManagerEnv,
        value: &[u8],
    ) {
        if env.close_flag() {
            return;
        }
        if value.is_empty() {
            tdutils::log_info!("Saved animations aren't found in database");
            self.reload_saved_animations(env, true);
            return;
        }

        tdutils::log_info!(
            "Successfully loaded saved animations of size {} from database",
            value.len()
        );

        let animation_ids = match parse_animation_list_log_event(self, env, value) {
            Ok(animation_ids) => animation_ids,
            Err(error) => {
                // the C++ `log_event_parse(...).ensure()` aborts
                panic!("Failed to parse saved animations from database: {error}");
            }
        };

        self.on_load_saved_animations_finished(env, animation_ids, true);
    }

    /// `on_load_saved_animations_finished`
    /// (AnimationsManager.cpp:548-556) plus the parked-continuation
    /// drain (the C++ request-actor re-runs).
    fn on_load_saved_animations_finished(
        &mut self,
        env: &mut impl AnimationsManagerEnv,
        mut saved_animation_ids: Vec<FileId>,
        from_database: bool,
    ) {
        if saved_animation_ids.len() as i32 > self.saved_animations_limit {
            saved_animation_ids.resize(self.saved_animations_limit as usize, FileId::default());
        }
        self.saved_animation_ids = saved_animation_ids;
        self.are_saved_animations_loaded = true;
        self.send_update_saved_animations(env, from_database);
        set_promises(&mut self.load_saved_animations_queries);

        for call in std::mem::take(&mut self.pending_saved_animation_calls) {
            match call {
                SavedAnimationCall::Get(promise) => {
                    let mut promise = promise;
                    promise.set_value(self.get_animations_object(env, &self.saved_animation_ids));
                }
                SavedAnimationCall::Add(input_file, promise) => {
                    self.add_saved_animation(env, input_file.as_ref(), promise);
                }
                SavedAnimationCall::Remove(input_file, promise) => {
                    self.remove_saved_animation(env, input_file.as_ref(), promise);
                }
                SavedAnimationCall::AddImpl(animation_id, add_on_server, promise) => {
                    self.add_saved_animation_impl(env, animation_id, add_on_server, promise);
                }
            }
        }
    }

    /// `on_get_saved_animations` (AnimationsManager.cpp:558-608); the
    /// per-document conversion rides the DocumentsManager seam.
    pub fn on_get_saved_animations(
        &mut self,
        env: &mut impl AnimationsManagerEnv,
        is_repair: bool,
        saved_animations_ptr: telegram_api::messages_SavedGifs,
    ) {
        assert!(!env.auth_is_bot());
        if !is_repair {
            self.are_saved_animations_being_loaded = false;
            self.next_saved_animations_load_time =
                tdutils::time::now_cached() + tdutils::random::fast(30 * 60, 50 * 60) as f64;
        }

        match saved_animations_ptr {
            telegram_api::messages_SavedGifs::messages_savedGifsNotModified(_) => {
                if is_repair {
                    return self.on_get_saved_animations_failed(
                        true,
                        tdutils::status::Status::error(500, "Failed to reload saved animations"),
                    );
                }
                tdutils::log_info!("Saved animations are not modified");
            }
            telegram_api::messages_SavedGifs::messages_savedGifs(saved_animations) => {
                tdutils::log_info!(
                    "Receive {} saved animations from server",
                    saved_animations.gifs.len()
                );

                let mut saved_animation_ids: Vec<FileId> = Vec::new();
                for document in saved_animations.gifs {
                    match document {
                        telegram_api::Document::documentEmpty(_) => {
                            tdutils::log_error!("Empty saved animation document received");
                        }
                        telegram_api::Document::document(document) => {
                            let Some(document_file_id) = env.on_get_document_animation(document)
                            else {
                                tdutils::log_error!("Receive non-animation as saved animation");
                                continue;
                            };
                            if !is_repair {
                                saved_animation_ids.push(document_file_id);
                            }
                        }
                    }
                }

                if is_repair {
                    set_promises(&mut self.repair_saved_animations_queries);
                } else {
                    self.on_load_saved_animations_finished(env, saved_animation_ids, false);

                    let source = "on_get_saved_animations";
                    let calculated_hash = self.get_saved_animations_hash(env, source);
                    if calculated_hash != saved_animations.hash {
                        tdutils::log_error!(
                            "Saved animations hash mismatch: {} vs {}",
                            saved_animations.hash,
                            calculated_hash
                        );
                    }
                }
            }
        }
    }

    /// `on_get_saved_animations_failed` (AnimationsManager.cpp:610-617).
    pub fn on_get_saved_animations_failed(
        &mut self,
        is_repair: bool,
        error: tdutils::status::Status,
    ) {
        debug_assert!(!error.is_ok());
        if !is_repair {
            self.are_saved_animations_being_loaded = false;
            self.next_saved_animations_load_time =
                tdutils::time::now_cached() + tdutils::random::fast(5, 10) as f64;
        }
        if is_repair {
            fail_promises(&mut self.repair_saved_animations_queries, &error);
        } else {
            fail_promises(&mut self.load_saved_animations_queries, &error);
            fail_saved_animation_calls(&mut self.pending_saved_animation_calls, &error);
        }
    }

    /// `get_saved_animations_hash` (AnimationsManager.cpp:619-635).
    fn get_saved_animations_hash(&self, env: &impl AnimationsManagerEnv, source: &str) -> i64 {
        let mut numbers: Vec<u64> = Vec::with_capacity(self.saved_animation_ids.len());
        let file_manager = env.file_manager();
        let file_manager = file_manager.lock();
        for animation_id in &self.saved_animation_ids {
            let _ = self
                .get_animation(*animation_id)
                .expect("get_saved_animations_hash: missing animation");
            let file_view = file_manager
                .get_file_view(*animation_id)
                .expect("get_saved_animations_hash: missing file view");
            let full_remote_location = file_view
                .get_full_remote_location()
                .expect("get_saved_animations_hash: no full remote location");
            if !full_remote_location.is_document() {
                tdutils::log_error!("Saved animation remote location is not a document: {source}");
                continue;
            }
            numbers.push(full_remote_location.get_id() as u64);
        }
        crate::misc::get_vector_hash(&numbers)
    }

    /// `add_saved_animation` (AnimationsManager.cpp:637-648).
    pub fn add_saved_animation(
        &mut self,
        env: &mut impl AnimationsManagerEnv,
        input_file: Option<&td_api::InputFile>,
        mut promise: Promise<()>,
    ) {
        if !self.are_saved_animations_loaded {
            self.pending_saved_animation_calls
                .push(SavedAnimationCall::Add(input_file.cloned(), promise));
            self.load_saved_animations(env, Promise::default());
            return;
        }

        let file_id = match get_input_animation_file_id(env, input_file) {
            Ok(file_id) => file_id,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        self.add_saved_animation_impl(env, file_id, true, promise);
    }

    /// `send_save_gif_query` (AnimationsManager.cpp:650-661); the query
    /// send is the `[blocked: net]` seam.
    pub fn send_save_gif_query(
        &mut self,
        env: &mut impl AnimationsManagerEnv,
        animation_id: FileId,
        unsave: bool,
        promise: Promise<()>,
    ) {
        // TODO invokeAfter and log event
        let input_document = {
            let file_manager = env.file_manager();
            let file_manager = file_manager.lock();
            let file_view = file_manager
                .get_file_view(animation_id)
                .expect("send_save_gif_query: missing file view");
            let full_remote_location = file_view
                .get_full_remote_location()
                .expect("send_save_gif_query: no full remote location");
            assert!(full_remote_location.is_document());
            assert!(!full_remote_location.is_web());
            as_input_document(full_remote_location)
        };
        env.send_save_gif_query_net(animation_id, unsave, input_document, promise);
    }

    /// `add_saved_animation_by_id` (AnimationsManager.cpp:663-672).
    pub fn add_saved_animation_by_id(&mut self, animation_id: FileId) {
        let has_stickers = self
            .get_animation(animation_id)
            .expect("add_saved_animation_by_id: missing animation")
            .has_stickers;
        if has_stickers {
            return;
        }

        // TODO log event
        self.pending_saved_animation_calls
            .push(SavedAnimationCall::AddImpl(
                animation_id,
                false,
                Promise::default(),
            ));
    }

    /// `add_saved_animation_impl` (AnimationsManager.cpp:674-741).
    fn add_saved_animation_impl(
        &mut self,
        env: &mut impl AnimationsManagerEnv,
        animation_id: FileId,
        add_on_server: bool,
        mut promise: Promise<()>,
    ) {
        assert!(!env.auth_is_bot());
        // `file_view.empty()` — `!node_`, i.e. the missing-file-view case
        let is_empty_file = env
            .file_manager()
            .lock()
            .get_file_view(animation_id)
            .is_none();
        if is_empty_file {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Animation file not found",
            ));
            return;
        }

        tdutils::log_info!("Add saved animation {animation_id}");
        if !self.are_saved_animations_loaded {
            self.pending_saved_animation_calls
                .push(SavedAnimationCall::AddImpl(
                    animation_id,
                    add_on_server,
                    promise,
                ));
            self.load_saved_animations(env, Promise::default());
            return;
        }

        let is_equal = |file_id: FileId| {
            file_id == animation_id
                || (file_id.get_remote() == animation_id.get_remote()
                    && animation_id.get_remote() != 0)
        };

        if !self.saved_animation_ids.is_empty() && is_equal(self.saved_animation_ids[0]) {
            // fast path
            if self.saved_animation_ids[0].get_remote() == 0 && animation_id.get_remote() != 0 {
                self.saved_animation_ids[0] = animation_id;
                self.save_saved_animations_to_database(env);
            }

            promise.set_value(());
            return;
        }

        let Some(mime_type) = self
            .get_animation(animation_id)
            .map(|animation| animation.mime_type.clone())
        else {
            promise.set_error(tdutils::status::Status::error(400, "Animation not found"));
            return;
        };
        if mime_type != "video/mp4" {
            promise.set_error(tdutils::status::Status::error(
                400,
                "Only MPEG4 animations can be saved",
            ));
            return;
        }

        let location_ok = {
            let file_manager = env.file_manager();
            let file_manager = file_manager.lock();
            let file_view = file_manager
                .get_file_view(animation_id)
                .expect("add_saved_animation_impl: missing file view");
            let Some(full_remote_location) = file_view.get_full_remote_location() else {
                promise.set_error(tdutils::status::Status::error(
                    400,
                    "Can save only sent animations",
                ));
                return;
            };
            if full_remote_location.is_web() {
                promise.set_error(tdutils::status::Status::error(
                    400,
                    "Can't save web animations",
                ));
                return;
            }
            if !full_remote_location.is_document() {
                promise.set_error(tdutils::status::Status::error(
                    400,
                    "Can't save encrypted animations",
                ));
                return;
            }
            true
        };
        debug_assert!(location_ok);

        let limit = self.saved_animations_limit as usize;
        add_to_top_if(
            &mut self.saved_animation_ids,
            limit,
            animation_id,
            &is_equal,
        );

        assert!(is_equal(self.saved_animation_ids[0]));
        if self.saved_animation_ids[0].get_remote() == 0 && animation_id.get_remote() != 0 {
            self.saved_animation_ids[0] = animation_id;
        }

        self.send_update_saved_animations(env, false);
        if add_on_server {
            self.send_save_gif_query(env, animation_id, false, promise);
        }
    }

    /// `remove_saved_animation` (AnimationsManager.cpp:743-769).
    pub fn remove_saved_animation(
        &mut self,
        env: &mut impl AnimationsManagerEnv,
        input_file: Option<&td_api::InputFile>,
        mut promise: Promise<()>,
    ) {
        if !self.are_saved_animations_loaded {
            self.pending_saved_animation_calls
                .push(SavedAnimationCall::Remove(input_file.cloned(), promise));
            self.load_saved_animations(env, Promise::default());
            return;
        }

        let file_id = match get_input_animation_file_id(env, input_file) {
            Ok(file_id) => file_id,
            Err(status) => {
                promise.set_error(status);
                return;
            }
        };

        let is_equal = |other: FileId| {
            other == file_id
                || (other.get_remote() == file_id.get_remote() && file_id.get_remote() != 0)
        };
        let before_len = self.saved_animation_ids.len();
        self.saved_animation_ids.retain(|id| !is_equal(*id));
        if self.saved_animation_ids.len() == before_len {
            promise.set_value(());
            return;
        }

        if self.get_animation(file_id).is_none() {
            promise.set_error(tdutils::status::Status::error(400, "Animation not found"));
            return;
        }

        self.send_save_gif_query(env, file_id, true, promise);

        self.send_update_saved_animations(env, false);
    }

    /// `try_send_update_animation_search_parameters`
    /// (AnimationsManager.cpp:771-776).
    fn try_send_update_animation_search_parameters(&self, env: &mut impl AnimationsManagerEnv) {
        if let Some(update) = self.get_update_animation_search_parameters_object() {
            env.send_update(td_api::Update::updateAnimationSearchParameters(update));
        }
    }

    /// `get_update_animation_search_parameters_object`
    /// (AnimationsManager.cpp:778-785).
    pub fn get_update_animation_search_parameters_object(
        &self,
    ) -> Option<td_api::updateAnimationSearchParameters> {
        if !self.is_animation_search_emojis_inited || !self.is_animation_search_provider_inited {
            return None;
        }
        Some(td_api::updateAnimationSearchParameters {
            provider: self.animation_search_provider.clone(),
            emojis: full_split(&self.animation_search_emojis, ','),
        })
    }

    /// `get_update_saved_animations_object`
    /// (AnimationsManager.cpp:787-790).
    pub fn get_update_saved_animations_object(&self) -> td_api::updateSavedAnimations {
        td_api::updateSavedAnimations {
            animation_ids: self
                .saved_animation_ids
                .iter()
                .map(|file_id| file_id.get())
                .collect(),
        }
    }

    /// `send_update_saved_animations` (AnimationsManager.cpp:792-818).
    fn send_update_saved_animations(
        &mut self,
        env: &mut impl AnimationsManagerEnv,
        from_database: bool,
    ) {
        if self.are_saved_animations_loaded {
            let mut new_saved_animation_file_ids = self.saved_animation_ids.clone();
            for animation_id in &self.saved_animation_ids {
                let animation = self
                    .get_animation(*animation_id)
                    .expect("send_update_saved_animations: missing animation");
                if animation.thumbnail.file_id.is_valid() {
                    new_saved_animation_file_ids.push(animation.thumbnail.file_id);
                }
                if animation.animated_thumbnail.photo_size.file_id.is_valid() {
                    new_saved_animation_file_ids
                        .push(animation.animated_thumbnail.photo_size.file_id);
                }
            }
            new_saved_animation_file_ids.sort();
            if new_saved_animation_file_ids != self.saved_animation_file_ids {
                let file_source_id = self.get_saved_animations_file_source_id(env);
                env.change_files_source(
                    file_source_id,
                    &self.saved_animation_file_ids,
                    &new_saved_animation_file_ids,
                    "send_update_saved_animations",
                );
                self.saved_animation_file_ids = new_saved_animation_file_ids;
            }

            env.send_update(td_api::Update::updateSavedAnimations(
                self.get_update_saved_animations_object(),
            ));

            if !from_database {
                self.save_saved_animations_to_database(env);
            }
        }
    }

    /// `save_saved_animations_to_database`
    /// (AnimationsManager.cpp:820-826).
    fn save_saved_animations_to_database(&mut self, env: &mut impl AnimationsManagerEnv) {
        if env.use_sqlite_pmc() {
            tdutils::log_info!("Save saved animations to database");
            let mut out = Vec::new();
            store_log_event_version(&mut out);
            store_i32(&mut out, self.saved_animation_ids.len() as i32);
            for animation_id in &self.saved_animation_ids {
                self.store_animation(env, *animation_id, &mut out);
            }
            env.sqlite_pmc_set_raw(b"ans", &out);
        }
    }

    /// `get_saved_animations_file_source_id`
    /// (AnimationsManager.cpp:828-833).
    pub fn get_saved_animations_file_source_id(
        &mut self,
        env: &mut impl AnimationsManagerEnv,
    ) -> FileSourceId {
        if !self.saved_animations_file_source_id.is_valid() {
            self.saved_animations_file_source_id = env.create_saved_animations_file_source();
        }
        self.saved_animations_file_source_id
    }

    /// `get_animation_search_text` (AnimationsManager.cpp:835-839).
    pub fn get_animation_search_text(&self, file_id: FileId) -> String {
        self.get_animation(file_id)
            .expect("get_animation_search_text: missing animation")
            .file_name
            .clone()
    }

    /// `get_current_state` (AnimationsManager.cpp:841-853).
    pub fn get_current_state(
        &self,
        env: &impl AnimationsManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if env.auth_is_bot() {
            return;
        }

        if self.are_saved_animations_loaded {
            updates.push(td_api::Update::updateSavedAnimations(
                self.get_update_saved_animations_object(),
            ));
        }
        if let Some(update) = self.get_update_animation_search_parameters_object() {
            updates.push(td_api::Update::updateAnimationSearchParameters(update));
        }
    }

    /// `store_animation` (AnimationsManager.hpp:21-42).
    pub fn store_animation(
        &self,
        env: &mut impl DialogPhotoStoreEnv,
        file_id: FileId,
        out: &mut Vec<u8>,
    ) {
        let animation = self
            .get_animation(file_id)
            .expect("store_animation: missing animation");
        let has_animated_thumbnail = animation.animated_thumbnail.photo_size.file_id.is_valid();
        store_flags(out, [animation.has_stickers, has_animated_thumbnail]);
        store_i32(out, animation.duration);
        animation.dimensions.store(out);
        crate::store_parse::store_string(out, &animation.file_name);
        crate::store_parse::store_string(out, &animation.mime_type);
        crate::store_parse::store_string(out, &animation.minithumbnail);
        animation.thumbnail.store(env, out);
        env.store_file_id(file_id, out);
        if animation.has_stickers {
            crate::store_parse::store_i32(out, animation.sticker_file_ids.len() as i32);
            for sticker_file_id in &animation.sticker_file_ids {
                env.store_file_id(*sticker_file_id, out);
            }
        }
        if has_animated_thumbnail {
            animation.animated_thumbnail.store(env, out);
        }
    }

    /// `parse_animation` (AnimationsManager.hpp:45-75) — an invalid or
    /// failed parse answers [`FileId::default`] like the C++ `FileId()`.
    pub fn parse_animation(
        &mut self,
        env: &mut impl DialogPhotoStoreEnv,
        parser: &mut Parser<'_>,
    ) -> FileId {
        self.parse_animation_impl(env, parser).unwrap_or_default()
    }

    fn parse_animation_impl(
        &mut self,
        env: &mut impl DialogPhotoStoreEnv,
        parser: &mut Parser<'_>,
    ) -> Result<FileId, ()> {
        let mut animation = Animation::default();
        let mut has_animated_thumbnail = false;
        if parser.version() >= version::ADD_ANIMATION_STICKERS {
            let [has_stickers, has_animated] = parser.parse_flags().map_err(|_| ())?;
            animation.has_stickers = has_stickers;
            has_animated_thumbnail = has_animated;
        }
        if parser.version() >= version::ADD_DURATION_TO_ANIMATION {
            animation.duration = parser.i32_().map_err(|_| ())?;
        }
        animation.dimensions = Dimensions::parse_from(parser).map_err(|_| ())?;
        animation.file_name = parser.string().map_err(|_| ())?;
        animation.mime_type = parser.string().map_err(|_| ())?;
        if parser.version() >= version::SUPPORT_MINITHUMBNAILS {
            animation.minithumbnail = parser.string().map_err(|_| ())?;
        }
        animation.thumbnail = PhotoSize::parse_from(env, parser).map_err(|_| ())?;
        animation.file_id = env.parse_file_id(parser).map_err(|_| ())?;
        if animation.has_stickers {
            let len = parser.i32_().map_err(|_| ())?;
            if len < 0 {
                return Err(());
            }
            animation.sticker_file_ids.reserve(len as usize);
            for _ in 0..len {
                animation
                    .sticker_file_ids
                    .push(env.parse_file_id(parser).map_err(|_| ())?);
            }
        }
        if has_animated_thumbnail {
            animation.animated_thumbnail =
                AnimationSize::parse_from(env, parser).map_err(|_| ())?;
        }
        // the C++ `parser.get_error() != nullptr || !file_id.is_valid()`
        // tail — parser errors already surfaced through the `?` chain, the
        // invalid file id folds to the default `FileId()` without the
        // `on_get_animation` intake
        if !animation.file_id.is_valid() {
            return Err(());
        }
        Ok(self.on_get_animation(animation, false))
    }
}

// The `SaveGifQuery` send seam's local half resolved the input document;
// the dropped promise keeps the request unanswered offline (the
// collapsed-line convention, recorded at
// `AnimationsManagerEnv::send_save_gif_query_net`).

/// `td_->file_manager_->get_input_file_id(FileType::Animation,
/// input_file, DialogId(), false, false)` (AnimationsManager.cpp:644-645,
/// 750-751) with the C++ default arguments.
fn get_input_animation_file_id(
    env: &mut impl AnimationsManagerEnv,
    input_file: Option<&td_api::InputFile>,
) -> Result<FileId, tdutils::status::Status> {
    env.file_manager().lock().get_input_file_id(
        crate::files::FileType::Animation,
        input_file,
        crate::message_id::DialogId::default(),
        // allow_zero, is_encrypted, get_by_hash, is_secure, force_reuse,
        // never_reuse — the C++ default arguments
        false,
        false,
        false,
        false,
        false,
        false,
    )
}

/// The minithumbnail half of `get_animation_object`
/// (AnimationsManager.cpp:160) — the lossy-String convention means the
/// packed bytes go in as `&[u8]`.
fn get_minithumbnail_object(animation: &Animation) -> td_api::Minithumbnail {
    crate::photo::get_minithumbnail_object(animation.minithumbnail.as_bytes())
}

/// `get_thumbnail_object(FileManager, PhotoSize, PhotoFormat)`
/// (PhotoSize.cpp:483-496) — the M.20 consumer placement; moves to
/// photo.rs when the M.28 media-metadata plane lands.
pub fn get_thumbnail_object(
    env: &impl AnimationsManagerEnv,
    photo_size: &PhotoSize,
    format: PhotoFormat,
) -> Option<td_api::Thumbnail> {
    if !photo_size.file_id.is_valid() {
        return None;
    }

    let mut format = format;
    if format == PhotoFormat::Jpeg && photo_size.size_type == b'g' as i32 {
        format = PhotoFormat::Gif;
    }

    Some(td_api::Thumbnail {
        format: Some(get_thumbnail_format_object(format)),
        width: photo_size.dimensions.width as i32,
        height: photo_size.dimensions.height as i32,
        file: Some(
            env.file_manager()
                .lock()
                .get_file_object(photo_size.file_id),
        ),
    })
}

/// `get_thumbnail_object(FileManager, PhotoSize, AnimationSize)`
/// (PhotoSize.cpp:498-504), the two-argument C++ overload.
pub fn get_animation_thumbnail_object(
    env: &impl AnimationsManagerEnv,
    animation: &Animation,
) -> Option<td_api::Thumbnail> {
    if animation.animated_thumbnail.photo_size.file_id.is_valid() {
        return get_thumbnail_object(
            env,
            &animation.animated_thumbnail.photo_size,
            PhotoFormat::Mpeg4,
        );
    }
    get_thumbnail_object(env, &animation.thumbnail, PhotoFormat::Jpeg)
}

/// `get_thumbnail_format_object` (PhotoSize.cpp:82-101).
fn get_thumbnail_format_object(format: PhotoFormat) -> td_api::ThumbnailFormat {
    match format {
        PhotoFormat::Jpeg => {
            td_api::ThumbnailFormat::thumbnailFormatJpeg(td_api::thumbnailFormatJpeg)
        }
        PhotoFormat::Png => td_api::ThumbnailFormat::thumbnailFormatPng(td_api::thumbnailFormatPng),
        PhotoFormat::Webp => {
            td_api::ThumbnailFormat::thumbnailFormatWebp(td_api::thumbnailFormatWebp)
        }
        PhotoFormat::Gif => td_api::ThumbnailFormat::thumbnailFormatGif(td_api::thumbnailFormatGif),
        PhotoFormat::Tgs => td_api::ThumbnailFormat::thumbnailFormatTgs(td_api::thumbnailFormatTgs),
        PhotoFormat::Mpeg4 => {
            td_api::ThumbnailFormat::thumbnailFormatMpeg4(td_api::thumbnailFormatMpeg4)
        }
        PhotoFormat::Webm => {
            td_api::ThumbnailFormat::thumbnailFormatWebm(td_api::thumbnailFormatWebm)
        }
    }
}

/// `full_split` (tdutils/td/utils/misc.h:35-50) — the C++ returns an
/// empty vector for an empty string, which Rust's `split` does not.
fn full_split(s: &str, delimiter: char) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(delimiter).map(|part| part.to_owned()).collect()
}

/// The `AnimationListLogEvent::parse` framing (AnimationsManager.hpp:442-469
/// + the log-event version prefix of `log_event_store`/`log_event_parse`).
fn parse_animation_list_log_event(
    manager: &mut AnimationsManager,
    env: &mut impl AnimationsManagerEnv,
    value: &[u8],
) -> Result<Vec<FileId>, &'static str> {
    let mut parser = Parser::new_log_event(value)?;
    let size = parser.i32_()?;
    if size < 0 {
        return Err("Invalid vector size");
    }
    let mut animation_ids = Vec::with_capacity(size as usize);
    for _ in 0..size {
        animation_ids.push(manager.parse_animation(env, &mut parser));
    }
    Ok(animation_ids)
}

/// The failed-load fan-out over the parked continuations.
fn fail_saved_animation_calls(
    calls: &mut Vec<SavedAnimationCall>,
    error: &tdutils::status::Status,
) {
    for mut call in calls.drain(..) {
        match &mut call {
            SavedAnimationCall::Get(promise) => {
                promise.set_error(error.clone());
            }
            SavedAnimationCall::Add(_, promise)
            | SavedAnimationCall::Remove(_, promise)
            | SavedAnimationCall::AddImpl(_, _, promise) => {
                promise.set_error(error.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::chat_manager::DcId;
    use crate::files::{FileLocationSource, FileType, FullRemoteFileLocation};
    use crate::store_parse::version;
    use tdutils::promise::{Promise, PromiseInterface};

    /// A [`PromiseInterface`] that flips flags instead of answering requests —
    /// the offline stand-in for the deferred outbox.
    struct FlagPromise<T: Send + 'static> {
        value_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
        error_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
        value: std::sync::Arc<parking_lot::Mutex<Option<T>>>,
    }

    impl<T: Send + 'static> PromiseInterface<T> for FlagPromise<T> {
        fn set_value(&mut self, value: T) {
            *self.value.lock() = Some(value);
            self.value_flag
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn set_error(&mut self, _error: tdutils::status::Status) {
            self.error_flag
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A flag-promise factory returning the promise plus its completion
    /// views: the value/error flags and the captured result slot.
    /// The flag-promise return shape: the promise, its value/error flags
    /// and the captured result slot.
    type FlagPromiseViews<T> = (
        Promise<T>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<parking_lot::Mutex<Option<T>>>,
    );

    fn flag_promise<T: Send + Default + 'static>() -> FlagPromiseViews<T> {
        let value_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let error_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let value = std::sync::Arc::new(parking_lot::Mutex::new(None));
        (
            Promise::from_interface(FlagPromise::<T> {
                value_flag: value_flag.clone(),
                error_flag: error_flag.clone(),
                value: value.clone(),
            }),
            value_flag,
            error_flag,
            value,
        )
    }

    struct TestEnv {
        files: std::sync::Arc<parking_lot::Mutex<crate::files::FileManager>>,
        file_reference: crate::files::FileReferenceManager,
        erased_binlog_keys: Vec<String>,
        sqlite: std::collections::HashMap<Vec<u8>, Vec<u8>>,
        options: std::collections::HashMap<String, String>,
        updates: Vec<td_api::Update>,
        queries: Vec<String>,
        is_bot: bool,
        use_sqlite: bool,
        /// the `[blocked: M.28 DocumentsManager]` seam answer
        document_animation: Option<FileId>,
    }

    impl Default for TestEnv {
        fn default() -> TestEnv {
            TestEnv {
                files: std::sync::Arc::new(parking_lot::Mutex::new(
                    crate::files::FileManager::new(),
                )),
                file_reference: crate::files::FileReferenceManager::new(),
                erased_binlog_keys: Vec::new(),
                sqlite: std::collections::HashMap::new(),
                options: std::collections::HashMap::new(),
                updates: Vec::new(),
                queries: Vec::new(),
                is_bot: false,
                use_sqlite: true,
                document_animation: None,
            }
        }
    }

    impl TestEnv {
        /// Registers a remote animation document and returns its FileId.
        fn register_animation_file(&mut self, remote_document_id: i64) -> FileId {
            self.files.lock().register_remote(
                FullRemoteFileLocation::from_document(
                    FileType::Animation,
                    remote_document_id,
                    42,
                    DcId::internal(2),
                    String::new(),
                ),
                FileLocationSource::FromServer,
                crate::message_id::DialogId::default(),
                1000,
                0,
                "animation.mp4".to_owned(),
            )
        }

        fn save_gif_queries(&self) -> Vec<&String> {
            self.queries
                .iter()
                .filter(|query| query.starts_with("save_gif"))
                .collect()
        }

        fn has_query(&self, prefix: &str) -> bool {
            self.queries.iter().any(|query| query.starts_with(prefix))
        }

        /// Builds the `"ans"` pmc record over the manager's current ring —
        /// the `save_saved_animations_to_database` framing — without going
        /// through the private writer.
        #[allow(dead_code)] // the oracle builder for the ans-record tests
        fn seed_saved_animations_blob(manager: &mut AnimationsManager, env: &mut TestEnv) {
            let mut out = Vec::new();
            store_log_event_version(&mut out);
            store_i32(&mut out, manager.saved_animation_ids.len() as i32);
            for animation_id in manager.saved_animation_ids.clone() {
                manager.store_animation(env, animation_id, &mut out);
            }
            env.sqlite.insert(b"ans".to_vec(), out);
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

    impl AnimationsManagerEnv for TestEnv {
        fn auth_is_bot(&self) -> bool {
            self.is_bot
        }

        fn close_flag(&self) -> bool {
            false
        }

        fn get_option_string(&self, name: &str, default_value: &str) -> String {
            self.options
                .get(name)
                .cloned()
                .unwrap_or_else(|| default_value.to_owned())
        }

        fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
            self.options
                .get(name)
                .and_then(|value| value.parse().ok())
                .unwrap_or(default_value)
        }

        fn binlog_pmc_erase(&mut self, key: &str) {
            self.erased_binlog_keys.push(key.to_owned());
        }

        fn use_sqlite_pmc(&self) -> bool {
            self.use_sqlite
        }

        fn sqlite_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.sqlite.get(key).cloned().unwrap_or_default()
        }

        fn sqlite_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.sqlite.insert(key.to_vec(), value.to_vec());
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn file_manager(&self) -> std::sync::Arc<parking_lot::Mutex<crate::files::FileManager>> {
            self.files.clone()
        }

        fn create_saved_animations_file_source(&mut self) -> crate::files::FileSourceId {
            self.file_reference
                .intern(crate::files::FileSource::SavedAnimations)
        }

        fn change_files_source(
            &mut self,
            file_source_id: crate::files::FileSourceId,
            old_file_ids: &[FileId],
            new_file_ids: &[FileId],
            _source: &str,
        ) {
            // `FileManager::change_files_source` (FileManager.cpp) over the
            // main-file-id sets
            let main_file_ids = |file_manager: &crate::files::FileManager, file_ids: &[FileId]| {
                let main: std::collections::HashSet<FileId> = file_ids
                    .iter()
                    .map(|file_id| {
                        file_manager
                            .get_file_view(*file_id)
                            .map(|view| view.get_main_file_id())
                            .unwrap_or_default()
                    })
                    .collect();
                main
            };
            let old_main = main_file_ids(&self.files.lock(), old_file_ids);
            let mut new_main = main_file_ids(&self.files.lock(), new_file_ids);
            for file_id in old_main {
                if !new_main.remove(&file_id) {
                    self.file_reference
                        .remove_file_source(file_id, file_source_id);
                }
            }
            for file_id in new_main {
                self.file_reference.add_file_source(file_id, file_source_id);
            }
        }

        fn send_get_saved_gifs_query(&mut self, is_repair: bool, hash: i64) {
            self.queries
                .push(format!("get_saved_gifs {is_repair} {hash}"));
        }

        fn send_save_gif_query_net(
            &mut self,
            file_id: FileId,
            unsave: bool,
            _input_document: telegram_api::InputDocument,
            promise: Promise<()>,
        ) {
            self.queries
                .push(format!("save_gif {} unsave={unsave}", file_id.get()));
            drop(promise);
        }

        fn on_get_document_animation(
            &mut self,
            _document: telegram_api::document,
        ) -> Option<FileId> {
            self.document_animation
        }
    }

    /// Builds a fully populated animation over `file_id` and returns the
    /// (manager, env) pair plus the file ids the record carries.
    fn make_full_animation(
        env: &mut TestEnv,
        manager: &mut AnimationsManager,
        file_id: FileId,
        thumbnail_file_id: FileId,
        animated_thumbnail_file_id: FileId,
        mime_type: &str,
    ) {
        let sticker_ids = vec![
            env.register_animation_file(900),
            env.register_animation_file(901),
        ];
        let thumbnail = PhotoSize {
            size_type: b'm' as i32,
            size: 321,
            dimensions: crate::photo::get_dimensions(120, 90, "test"),
            file_id: thumbnail_file_id,
            progressive_sizes: vec![100, 200],
        };
        let animated_thumbnail = AnimationSize {
            photo_size: PhotoSize {
                size_type: b'u' as i32,
                size: 777,
                dimensions: crate::photo::get_dimensions(120, 90, "test animated"),
                file_id: animated_thumbnail_file_id,
                progressive_sizes: Vec::new(),
            },
            main_frame_timestamp: 1.5,
        };
        manager.create_animation(
            env,
            file_id,
            "\u{1}\u{0}\u{0}packed".to_owned(),
            thumbnail,
            animated_thumbnail,
            true,
            sticker_ids,
            "cat.mp4".to_owned(),
            mime_type.to_owned(),
            3,
            crate::photo::get_dimensions(320, 240, "test main"),
            true,
        );
    }

    /// `store_animation`/`parse_animation` (AnimationsManager.hpp:21-75) — the
    /// byte-exact round trip pinning every field, with the pmc log-event
    /// framing around them.
    #[test]
    fn animation_store_parse_round_trips_every_field() {
        let mut env = TestEnv::default();
        let mut manager = AnimationsManager::new();
        let animation_file_id = env.register_animation_file(10);
        let thumbnail_file_id = env.register_animation_file(11);
        let animated_file_id = env.register_animation_file(12);
        make_full_animation(
            &mut env,
            &mut manager,
            animation_file_id,
            thumbnail_file_id,
            animated_file_id,
            "video/mp4",
        );

        // the stored pmc blob: version + the store_animation payload
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        manager.store_animation(&mut env, animation_file_id, &mut out);

        let mut parse_manager = AnimationsManager::new();
        let mut parser = Parser::new_log_event(&out).unwrap();
        assert_eq!(parser.version(), version::LOG_EVENT_STORE_VERSION);
        let parsed_file_id = parse_manager.parse_animation(&mut env, &mut parser);
        parser.fetch_end().unwrap();
        assert_eq!(parsed_file_id, animation_file_id);

        let original = manager.get_animation(animation_file_id).unwrap();
        let parsed = parse_manager.get_animation(parsed_file_id).unwrap();
        assert_eq!(parsed.file_name, original.file_name);
        assert_eq!(parsed.file_name, "cat.mp4");
        assert_eq!(parsed.mime_type, original.mime_type);
        assert_eq!(parsed.mime_type, "video/mp4");
        assert_eq!(parsed.duration, original.duration);
        assert_eq!(parsed.duration, 3);
        assert_eq!(parsed.dimensions, original.dimensions);
        assert_eq!(
            parsed.dimensions,
            crate::photo::get_dimensions(320, 240, "test main")
        );
        assert_eq!(parsed.minithumbnail, original.minithumbnail);
        assert_eq!(parsed.minithumbnail, "\u{1}\u{0}\u{0}packed");
        assert_eq!(parsed.thumbnail, original.thumbnail);
        assert_eq!(parsed.thumbnail.file_id, thumbnail_file_id);
        assert_eq!(parsed.thumbnail.size_type, b'm' as i32);
        assert_eq!(parsed.thumbnail.size, 321);
        assert_eq!(parsed.thumbnail.progressive_sizes, vec![100, 200]);
        assert_eq!(parsed.animated_thumbnail, original.animated_thumbnail);
        assert_eq!(
            parsed.animated_thumbnail.photo_size.file_id,
            animated_file_id
        );
        assert_eq!(parsed.animated_thumbnail.main_frame_timestamp, 1.5);
        assert!(parsed.has_stickers);
        assert_eq!(parsed.sticker_file_ids, original.sticker_file_ids);
        assert_eq!(parsed.sticker_file_ids.len(), 2);
    }

    /// The pre-`AddAnimationStickers`/pre-`AddDurationToAnimation`/
    /// pre-`SupportMinithumbnails` legacy layouts (AnimationsManager.hpp:48-62):
    /// no flags block, no duration, no minithumbnail; a bare thumbnail without
    /// progressive sizes (pre-`AddPhotoProgressiveSizes`).
    #[test]
    fn animation_parse_handles_legacy_layouts() {
        let mut env = TestEnv::default();
        let animation_file_id = env.register_animation_file(20);
        let thumbnail_file_id = env.register_animation_file(21);

        // hand-roll the pre-stickers layout: the flags block is version-gated
        // away; the on-disk order is duration, dimensions, file_name,
        // mime_type, minithumbnail, thumbnail{type, dimensions, size, file_id,
        // (progressive)}, file_id
        let legacy_version = version::ADD_ANIMATION_STICKERS - 1; // 25: duration+minithumbnail in, stickers out
        let mut out = Vec::new();
        crate::store_parse::store_i32(&mut out, legacy_version);
        crate::store_parse::store_i32(&mut out, 7); // duration (>= AddDurationToAnimation)
        crate::photo::get_dimensions(64, 48, "x").store(&mut out);
        crate::store_parse::store_string(&mut out, "old.gif");
        crate::store_parse::store_string(&mut out, "image/gif");
        crate::store_parse::store_string(&mut out, "\u{1}mini"); // >= SupportMinithumbnails
        crate::store_parse::store_i32(&mut out, b'm' as i32); // thumbnail type
        crate::photo::get_dimensions(64, 48, "x").store(&mut out); // thumbnail dimensions
        crate::store_parse::store_i32(&mut out, 55); // thumbnail size
        env.store_file_id(thumbnail_file_id, &mut out);
        // no progressive sizes (< AddPhotoProgressiveSizes)
        env.store_file_id(animation_file_id, &mut out);

        let mut parse_manager = AnimationsManager::new();
        let mut parser = Parser::new_log_event(&out).unwrap();
        assert_eq!(parser.version(), legacy_version);
        let parsed_file_id = parse_manager.parse_animation(&mut env, &mut parser);
        parser.fetch_end().unwrap();
        assert_eq!(parsed_file_id, animation_file_id);

        let parsed = parse_manager.get_animation(parsed_file_id).unwrap();
        assert_eq!(parsed.duration, 7);
        assert_eq!(parsed.file_name, "old.gif");
        assert_eq!(parsed.mime_type, "image/gif");
        assert_eq!(parsed.minithumbnail, "\u{1}mini");
        assert_eq!(parsed.thumbnail.file_id, thumbnail_file_id);
        assert!(parsed.thumbnail.progressive_sizes.is_empty());
        assert!(!parsed.has_stickers);
        assert!(parsed.sticker_file_ids.is_empty());
        assert!(!parsed.animated_thumbnail.photo_size.file_id.is_valid());
    }

    /// The add/remove ring: the top insertion, the remote-id dedup, the
    /// limit clamp, the error bands and the recorded net save queries.
    #[test]
    fn saved_ring_add_remove_and_limit() {
        let mut env = TestEnv::default();
        let mut manager = AnimationsManager::new();
        let id1 = env.register_animation_file(100);
        let id2 = env.register_animation_file(200);
        let id3 = env.register_animation_file(300);
        for file_id in [id1, id2, id3] {
            manager.create_animation(
                &mut env,
                file_id,
                String::new(),
                PhotoSize::default(),
                AnimationSize::default(),
                false,
                Vec::new(),
                format!("a{file_id}.mp4"),
                "video/mp4".to_owned(),
                1,
                crate::photo::get_dimensions(10, 10, "test ring"),
                true,
            );
        }

        let input = |file_id: FileId| {
            Some(td_api::InputFile::inputFileId(td_api::inputFileId {
                id: file_id.get(),
            }))
        };

        // load the ring from an empty database (the C++ load-on-first-use)
        env.sqlite.insert(b"ans".to_vec(), {
            let mut blob = Vec::new();
            store_log_event_version(&mut blob);
            store_i32(&mut blob, 0);
            blob
        });
        let (load_promise, load_fulfilled, _, _) = flag_promise::<td_api::Animations>();
        manager.get_saved_animations(&mut env, load_promise);
        assert!(load_fulfilled.load(std::sync::atomic::Ordering::SeqCst));

        let (promise, fulfilled, failed, _) = flag_promise();
        manager.add_saved_animation(&mut env, input(id1).as_ref(), promise);
        // the promise rode the `[blocked: net]` SaveGifQuery seam — the C++
        // request stays unanswered offline, so the flag never flips
        assert!(!fulfilled.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!failed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(manager.saved_animation_ids, vec![id1]);
        assert!(env.has_query(&format!("save_gif {} unsave=false", id1.get())));
        // the pmc record landed
        assert!(env.sqlite.contains_key(b"ans".as_slice()));

        manager.add_saved_animation(&mut env, input(id2).as_ref(), flag_promise().0);
        manager.add_saved_animation(&mut env, input(id3).as_ref(), flag_promise().0);
        assert_eq!(manager.saved_animation_ids, vec![id3, id2, id1]);

        // the remote-id fast path: re-adding the head swaps in the remote id
        // and answers without a new update
        let updates_before = env.updates.len();
        let (promise, fulfilled, failed, _) = flag_promise();
        manager.add_saved_animation(&mut env, input(id3).as_ref(), promise);
        assert!(fulfilled.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!failed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(manager.saved_animation_ids, vec![id3, id2, id1]);
        assert_eq!(env.updates.len(), updates_before);

        // the limit clamp
        env.options
            .insert("saved_animations_limit".to_owned(), "2".to_owned());
        manager.on_update_saved_animations_limit(&mut env);
        assert_eq!(manager.saved_animations_limit, 2);
        assert_eq!(manager.saved_animation_ids, vec![id3, id2]);
        assert!(matches!(
            env.updates.last(),
            Some(td_api::Update::updateSavedAnimations(_))
        ));

        // remove
        let (promise, fulfilled, failed, _) = flag_promise();
        manager.remove_saved_animation(&mut env, input(id2).as_ref(), promise);
        // the unsave query rides the `[blocked: net]` seam — the promise stays
        // pending offline
        assert!(!fulfilled.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!failed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(manager.saved_animation_ids, vec![id3]);
        assert!(env.has_query(&format!("save_gif {} unsave=true", id2.get())));

        // remove of a non-member answers ok without a query
        let queries_before = env.queries.len();
        let (promise, fulfilled, failed, _) = flag_promise();
        manager.remove_saved_animation(&mut env, input(id1).as_ref(), promise);
        assert!(fulfilled.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!failed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(env.queries.len(), queries_before);

        // only MPEG4 animations can be saved
        let webm_id = env.register_animation_file(400);
        manager.create_animation(
            &mut env,
            webm_id,
            String::new(),
            PhotoSize::default(),
            AnimationSize::default(),
            false,
            Vec::new(),
            "b.webm".to_owned(),
            "video/webm".to_owned(),
            1,
            crate::photo::get_dimensions(10, 10, "test ring webm"),
            true,
        );
        let (promise, fulfilled, failed, _) = flag_promise();
        manager.add_saved_animation(&mut env, input(webm_id).as_ref(), promise);
        assert!(!fulfilled.load(std::sync::atomic::Ordering::SeqCst));
        assert!(failed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(manager.saved_animation_ids, vec![id3]);

        // id1 + id2 + id3 adds and the id2 unsave rode the net seam; the id3
        // re-add took the fast path and the webm add failed before the query
        assert_eq!(env.save_gif_queries().len(), 4);
    }

    /// The search-parameters plane: the constructor loads, the
    /// `updateAnimationSearchParameters` emission and the bot gate.
    #[test]
    fn animation_search_parameters_updates() {
        let mut env = TestEnv::default();
        let mut manager = AnimationsManager::new();
        manager.init(&mut env);

        // the constructor reads + the legacy key erase
        assert!(manager.is_animation_search_emojis_inited);
        assert!(manager.is_animation_search_provider_inited);
        assert_eq!(env.erased_binlog_keys, vec!["saved_animations_limit"]);
        assert!(env.updates.is_empty());

        env.options.insert(
            "animation_search_emojis".to_owned(),
            "\u{1F600},\u{1F389}".to_owned(),
        );
        env.options
            .insert("animation_search_provider".to_owned(), "gif_bot".to_owned());
        // each option change emits its own update (the C++ hook fires per
        // option); the first one carries the empty provider
        manager.on_update_animation_search_emojis(&mut env);
        manager.on_update_animation_search_provider(&mut env);
        assert_eq!(env.updates.len(), 2);
        match env.updates.last() {
            Some(td_api::Update::updateAnimationSearchParameters(update)) => {
                assert_eq!(update.provider, "gif_bot");
                assert_eq!(update.emojis, vec!["\u{1F600}", "\u{1F389}"]);
            }
            other => panic!("unexpected update: {other:?}"),
        }

        // no change — no new update
        manager.on_update_animation_search_emojis(&mut env);
        manager.on_update_animation_search_provider(&mut env);
        assert_eq!(env.updates.len(), 2);

        // the built object matches
        let object = manager
            .get_update_animation_search_parameters_object()
            .unwrap();
        assert_eq!(object.provider, "gif_bot");
        assert_eq!(object.emojis.len(), 2);

        // bots never see the plane
        env.is_bot = true;
        env.options.insert(
            "animation_search_provider".to_owned(),
            "other_bot".to_owned(),
        );
        manager.on_update_animation_search_provider(&mut env);
        assert_eq!(env.updates.len(), 2);
        assert_eq!(manager.animation_search_provider, "gif_bot");
    }

    /// The database load path: the parked `get_saved_animations` promise
    /// completes with the built objects, and `get_current_state` starts
    /// emitting `updateSavedAnimations`.
    #[test]
    fn saved_animations_db_load_completes_the_parked_promise() {
        let mut env = TestEnv::default();
        let mut manager = AnimationsManager::new();
        let id1 = env.register_animation_file(500);
        let id2 = env.register_animation_file(600);
        for file_id in [id1, id2] {
            manager.create_animation(
                &mut env,
                file_id,
                String::new(),
                PhotoSize::default(),
                AnimationSize::default(),
                false,
                Vec::new(),
                format!("c{file_id}.mp4"),
                "video/mp4".to_owned(),
                2,
                crate::photo::get_dimensions(20, 20, "test db"),
                true,
            );
        }

        // pre-load the ring from an empty database, then let the adds write
        // the "ans" record (the C++ save_saved_animations_to_database flow)
        env.sqlite.insert(b"ans".to_vec(), {
            let mut blob = Vec::new();
            store_log_event_version(&mut blob);
            store_i32(&mut blob, 0);
            blob
        });
        manager.add_saved_animation(
            &mut env,
            Some(&td_api::InputFile::inputFileId(td_api::inputFileId {
                id: id1.get(),
            })),
            flag_promise().0,
        );
        manager.add_saved_animation(
            &mut env,
            Some(&td_api::InputFile::inputFileId(td_api::inputFileId {
                id: id2.get(),
            })),
            flag_promise().0,
        );

        // a fresh manager over the same database
        let mut manager2 = AnimationsManager::new();
        let (promise, fulfilled, failed, value) = flag_promise::<td_api::Animations>();
        assert!(!fulfilled.load(std::sync::atomic::Ordering::SeqCst));
        manager2.get_saved_animations(&mut env, promise);
        assert!(fulfilled.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!failed.load(std::sync::atomic::Ordering::SeqCst));
        let animations = value.lock().take().unwrap();
        assert_eq!(animations.animations.len(), 2);
        assert_eq!(animations.animations[0].file_name, format!("c{id2}.mp4"));
        assert_eq!(animations.animations[0].duration, 2);
        assert_eq!(animations.animations[1].file_name, format!("c{id1}.mp4"));

        // loaded managers emit the state update
        let mut updates = Vec::new();
        manager2.get_current_state(&env, &mut updates);
        assert_eq!(updates.len(), 1);
        assert!(matches!(
            updates.first(),
            Some(td_api::Update::updateSavedAnimations(_))
        ));

        // a fresh unloaded manager emits nothing
        let mut updates = Vec::new();
        AnimationsManager::new().get_current_state(&env, &mut updates);
        assert!(updates.is_empty());
    }

    /// `on_get_saved_animations` (AnimationsManager.cpp:558-608): the
    /// documentEmpty skip, the DocumentsManager seam answering non-animations,
    /// the loaded flip and the hash fold over the real remote locations.
    #[test]
    fn saved_animations_intake_skips_and_folds() {
        let mut env = TestEnv::default();
        let mut manager = AnimationsManager::new();
        let id1 = env.register_animation_file(700);
        manager.create_animation(
            &mut env,
            id1,
            String::new(),
            PhotoSize::default(),
            AnimationSize::default(),
            false,
            Vec::new(),
            "d.mp4".to_owned(),
            "video/mp4".to_owned(),
            1,
            crate::photo::get_dimensions(30, 30, "test intake"),
            true,
        );

        // the intake seam answers id1 for any document (the M.28 DocumentsManager
        // half); both wire shapes are exercised
        env.document_animation = Some(id1);
        let minimal_document = telegram_api::document {
            flags: 0,
            id: 700,
            access_hash: 42,
            file_reference: Vec::new(),
            date: 0,
            mime_type: "video/mp4".to_owned(),
            size: 1000,
            thumbs: Vec::new(),
            video_thumbs: Vec::new(),
            dc_id: 2,
            attributes: Vec::new(),
        };
        let saved_gifs = telegram_api::messages_SavedGifs::messages_savedGifs(
            telegram_api::messages_savedGifs {
                hash: crate::misc::get_vector_hash(&[700]),
                gifs: vec![
                    telegram_api::Document::documentEmpty(telegram_api::documentEmpty { id: 1 }),
                    telegram_api::Document::document(minimal_document.clone()),
                ],
            },
        );
        manager.on_get_saved_animations(&mut env, false, saved_gifs);
        assert!(manager.are_saved_animations_loaded);
        assert_eq!(manager.saved_animation_ids, vec![id1]);
        // the hash check passed (no error-only updates in between)
        assert!(matches!(
            env.updates.last(),
            Some(td_api::Update::updateSavedAnimations(_))
        ));

        // the seam answering None is the C++ non-animation skip
        env.document_animation = None;
        let mut manager2 = AnimationsManager::new();
        let saved_gifs = telegram_api::messages_SavedGifs::messages_savedGifs(
            telegram_api::messages_savedGifs {
                hash: 0,
                gifs: vec![telegram_api::Document::document(minimal_document)],
            },
        );
        manager2.on_get_saved_animations(&mut env, false, saved_gifs);
        assert!(manager2.are_saved_animations_loaded);
        assert!(manager2.saved_animation_ids.is_empty());

        // savedGifsNotModified on a repair fails the repair promises
        let mut manager3 = AnimationsManager::new();
        let (promise, fulfilled, failed, _) = flag_promise();
        manager3.repair_saved_animations(&mut env, promise);
        assert!(!fulfilled.load(std::sync::atomic::Ordering::SeqCst));
        let not_modified = telegram_api::messages_SavedGifs::messages_savedGifsNotModified(
            telegram_api::messages_savedGifsNotModified,
        );
        manager3.on_get_saved_animations(&mut env, true, not_modified);
        assert!(failed.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// `get_saved_animations_hash` (AnimationsManager.cpp:619-635) folds the
    /// remote document ids of every saved animation.
    #[test]
    fn saved_animations_hash_folds_remote_ids() {
        let mut env = TestEnv::default();
        let mut manager = AnimationsManager::new();
        let id1 = env.register_animation_file(800);
        let id2 = env.register_animation_file(900);
        for file_id in [id1, id2] {
            manager.create_animation(
                &mut env,
                file_id,
                String::new(),
                PhotoSize::default(),
                AnimationSize::default(),
                false,
                Vec::new(),
                format!("e{file_id}.mp4"),
                "video/mp4".to_owned(),
                1,
                crate::photo::get_dimensions(40, 40, "test hash"),
                true,
            );
        }
        // pre-load the ring from an empty database, then add both animations
        env.sqlite.insert(b"ans".to_vec(), {
            let mut blob = Vec::new();
            store_log_event_version(&mut blob);
            store_i32(&mut blob, 0);
            blob
        });
        for file_id in [id1, id2] {
            manager.add_saved_animation(
                &mut env,
                Some(&td_api::InputFile::inputFileId(td_api::inputFileId {
                    id: file_id.get(),
                })),
                flag_promise().0,
            );
        }
        assert_eq!(manager.saved_animation_ids, vec![id2, id1]);

        // the recorded reload query carries the same fold; the empty-db load's
        // auto-reload left the being-loaded flag set (only the server answer
        // clears it in the C++) — simulate the answer clearing it
        manager.are_saved_animations_being_loaded = false;
        let expected = crate::misc::get_vector_hash(&[900, 800]);
        manager.reload_saved_animations(&mut env, true);
        assert!(env.has_query(&format!("get_saved_gifs false {expected}")));
    }

    /// `get_animation_object` (AnimationsManager.cpp:150-162): the built
    /// td_api shape, the minithumbnail and the thumbnail format split —
    /// the animated thumbnail wins as `thumbnailFormatMpeg4`.
    #[test]
    fn animation_object_shape() {
        let mut env = TestEnv::default();
        let mut manager = AnimationsManager::new();
        let animation_file_id = env.register_animation_file(1000);
        let thumbnail_file_id = env.register_animation_file(1001);
        let animated_file_id = env.register_animation_file(1002);
        make_full_animation(
            &mut env,
            &mut manager,
            animation_file_id,
            thumbnail_file_id,
            animated_file_id,
            "video/mp4",
        );

        let object = manager
            .get_animation_object(&env, animation_file_id)
            .unwrap();
        assert_eq!(object.duration, 3);
        assert_eq!(object.width, 320);
        assert_eq!(object.height, 240);
        assert_eq!(object.file_name, "cat.mp4");
        assert_eq!(object.mime_type, "video/mp4");
        assert!(object.has_stickers);
        assert!(object.minithumbnail.is_some());
        let thumbnail = object.thumbnail.as_ref().unwrap();
        assert!(matches!(
            thumbnail.format,
            Some(td_api::ThumbnailFormat::thumbnailFormatMpeg4(_))
        ));
        assert_eq!(thumbnail.width, 120);
        assert_eq!(thumbnail.height, 90);
        assert!(object.animation.is_some());

        // an invalid id answers None (the C++ nullptr)
        assert!(
            manager
                .get_animation_object(&env, FileId::default())
                .is_none()
        );

        // the duration read and the search text
        assert_eq!(manager.get_animation_duration(animation_file_id), 3);
        assert_eq!(
            manager.get_animation_search_text(animation_file_id),
            "cat.mp4"
        );
        assert_eq!(
            manager.get_animation_thumbnail_file_id(animation_file_id),
            thumbnail_file_id
        );
        assert_eq!(
            manager.get_animation_animated_thumbnail_file_id(animation_file_id),
            animated_file_id
        );

        // the animations list builder
        let animations = manager.get_animations_object(&env, &[animation_file_id]);
        assert_eq!(animations.animations.len(), 1);
        assert_eq!(animations.animations[0].file_name, "cat.mp4");
    }

    /// `dup_animation`/`merge_animations` (AnimationsManager.cpp:226-262):
    /// the dup keeps the record, the suffix names skip the file merge and
    /// the different names trigger it.
    #[test]
    fn dup_and_merge_animations() {
        let mut env = TestEnv::default();
        let mut manager = AnimationsManager::new();
        let old_id = env.register_animation_file(1100);
        manager.create_animation(
            &mut env,
            old_id,
            String::new(),
            PhotoSize::default(),
            AnimationSize::default(),
            false,
            Vec::new(),
            "base".to_owned(),
            "video/mp4".to_owned(),
            1,
            crate::photo::get_dimensions(50, 50, "test dup"),
            true,
        );

        let new_id = env.register_animation_file(1101);
        let duped = manager.dup_animation(new_id, old_id);
        assert_eq!(duped, new_id);
        assert_eq!(manager.get_animation(new_id).unwrap().file_name, "base");
        // a second dup is a no-op
        assert_eq!(manager.dup_animation(new_id, old_id), new_id);

        // the same name with the ".mp4" suffix — no file merge needed
        manager.create_animation(
            &mut env,
            new_id,
            String::new(),
            PhotoSize::default(),
            AnimationSize::default(),
            false,
            Vec::new(),
            "base.mp4".to_owned(),
            "video/mp4".to_owned(),
            1,
            crate::photo::get_dimensions(50, 50, "test dup"),
            true,
        );
        manager.merge_animations(&mut env, new_id, old_id);
        assert!(manager.get_animation(new_id).is_some());

        // a mismatching name merges (the file ids here are distinct remote
        // nodes, so the C++ merge would fold them — the collapsed line runs
        // the real FileManager merge)
        let other_id = env.register_animation_file(1102);
        manager.create_animation(
            &mut env,
            other_id,
            String::new(),
            PhotoSize::default(),
            AnimationSize::default(),
            false,
            Vec::new(),
            "unrelated".to_owned(),
            "video/mp4".to_owned(),
            1,
            crate::photo::get_dimensions(50, 50, "test dup"),
            true,
        );
        manager.merge_animations(&mut env, other_id, old_id);
        // the merged node survives under the surviving id
        assert!(
            manager.get_animation(other_id).is_some() || manager.get_animation(old_id).is_some()
        );
    }

    /// The failed-load fan-out: `on_get_saved_animations_failed` fails the
    /// parked continuations and re-arms the reload clock.
    #[test]
    fn failed_load_fails_parked_promises() {
        let mut env = TestEnv {
            use_sqlite: false,
            ..TestEnv::default()
        };
        let mut manager = AnimationsManager::new();
        let id1 = env.register_animation_file(1200);
        manager.create_animation(
            &mut env,
            id1,
            String::new(),
            PhotoSize::default(),
            AnimationSize::default(),
            false,
            Vec::new(),
            "f.mp4".to_owned(),
            "video/mp4".to_owned(),
            1,
            crate::photo::get_dimensions(60, 60, "test fail"),
            true,
        );

        let (get_promise, get_fulfilled, get_failed, _) = flag_promise::<td_api::Animations>();
        manager.get_saved_animations(&mut env, get_promise);
        // the load path's forced reload still sends is_repair=false
        // (AnimationsManager.cpp:480); the empty hash folds to 0
        assert!(env.has_query("get_saved_gifs false 0"));

        let error = tdutils::status::Status::error(429, "Too Many Requests: retry later");
        manager.on_get_saved_animations_failed(false, error);
        assert!(!get_fulfilled.load(std::sync::atomic::Ordering::SeqCst));
        assert!(get_failed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!manager.are_saved_animations_being_loaded);
    }
}
