//! Port of `td/telegram/WebPagesManager.{h,cpp}` (M.17.2/3) — the link
//! preview / instant-view cache: the `WebPage`/`WebPageInstantView`
//! records with byte-exact store/parse, the page map and the URL cache,
//! the wire intake (`on_get_web_page`), the register/unregister planes,
//! the pmc/binlog persistence, the pending-preview timeout machinery and
//! the `td_api::linkPreview`/`webPageInstantView` builders.
//!
//! The manager is the collapsed actor: `Td` owns the struct and the
//! [`WebPagesManagerEnv`] trait carries the `G()`/pmc/binlog/cross-manager
//! surfaces. Every `create_handler<...>` network tail (the
//! `GetWebPagePreviewQuery`/`GetWebPageQuery` sends) is an env seam
//! marked `[blocked: net]`; the promise parks exactly like the C++.
//!
//! Divergences, all recorded at the seams:
//! - sticker FileIds of `webPageAttributeStickerSet`/`StarGiftCollection`
//!   store through the bare FileManager codec (M.16's documented
//!   divergence from `StickersManager::store_sticker`);
//! - the `ThemeSettings`/`StarGift` payloads (`webPageAttributeTheme`,
//!   `webPageAttributeUniqueStarGift`, `webPageAttributeStarGiftAuction`)
//!   keep their C++ byte-layout presence flags, permanently false, until
//!   BackgroundManager/StarGift land (Wave 3); the intakes log and drop;
//! - the poll web-page linkage (`register_poll_web_pages` +
//!   `web_page_polls_`) is `[blocked: M.19]` (PollManager owns `PollId`);
//! - the welcome-message linkage is `[blocked: Wave 3]`
//!   (WelcomeMessageManager owns `EphemeralMessageFullId`).

use crate::document::Document;
use crate::dialog_id::DialogId;
use crate::emoji_status::CustomEmojiId;
use crate::files::{FileId, FileSourceId, QuickReplyMessageFullId, StoryFullId};
use crate::message_entity::{find_entities, get_first_url, is_visible_url};
use crate::message_id::MessageFullId;
use crate::photo::{DialogPhotoStoreEnv, Photo};
use crate::store_parse::{store_flags, store_i32, store_i64, store_log_event_version, store_string, version, Parser};
use crate::td_api;
use crate::user_manager::UserId;
use crate::webpage::{
    get_page_blocks_object, get_rich_text_from_telegram, get_web_page_block,
    get_web_page_blocks_from_telegram, DocumentType as _, CloneWebPageBlockContext, DocumentsById,
    MessageContentDupType, PhotosById, RichText, RichTextType, WebPageBlock,
    WebPageBlockEnv, WebPageId,
};
use std::collections::{HashMap, HashSet};
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The binlog handler id of the WebPages events
/// (`LogEvent::HandlerType::WebPages`, LogEvent.h:70).
pub const LOG_EVENT_HANDLER_TYPE_WEB_PAGES: i32 = 0x10;

/// The `Td *td`/manager slice WebPagesManager reads through.
pub trait WebPagesManagerEnv: WebPageBlockEnv {
    // -- the G()/auth slice
    fn auth_is_bot(&self) -> bool;
    fn close_flag(&self) -> bool;
    fn use_message_database(&self) -> bool;
    fn server_time(&self) -> f64;

    // -- the pmc slice (`G()->td_db()->get_sqlite_pmc()`)
    fn sqlite_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;
    fn sqlite_pmc_set_raw(&mut self, key: &[u8], value: &[u8]);
    fn sqlite_pmc_erase(&mut self, key: &[u8]);

    // -- the binlog slice (`G()->td_db()->get_binlog()`, the WebPages
    // handler events)
    fn binlog_add_web_page(&mut self, data: &[u8]) -> u64;
    fn binlog_rewrite_web_page(&mut self, event_id: u64, data: &[u8]);
    fn binlog_erase(&mut self, event_id: u64);

    // -- the MultiTimeout plane (`pending_web_pages_timeout_`), driven by
    // `Td::timeout_expired`
    fn add_pending_web_page_timeout(&mut self, web_page_id: i64, timeout_in: f64);
    fn cancel_pending_web_page_timeout(&mut self, web_page_id: i64);

    // -- the file-source plane
    /// `file_reference_manager_->create_web_page_file_source(url)`.
    fn create_web_page_file_source(&mut self, url: &str) -> FileSourceId;
    /// `file_manager_->change_files_source(source_id, old, new, source)`.
    fn change_files_source(
        &mut self,
        source_id: FileSourceId,
        old_file_ids: &[FileId],
        new_file_ids: &[FileId],
        source: &str,
    );

    // -- the cross-manager nets (each collapsed behind its owner's line)
    /// `messages_manager_->delete_pending_message_web_page` [blocked: M.10].
    fn delete_pending_message_web_page(&mut self, message_full_id: MessageFullId);
    /// `messages_manager_->on_external_update_message_content` [blocked: M.10].
    fn on_external_update_message_content(&mut self, message_full_id: MessageFullId, source: &str);
    /// `quick_reply_manager_->delete_pending_message_web_page`.
    fn delete_quick_reply_pending_message_web_page(&mut self, message_full_id: QuickReplyMessageFullId);
    /// `quick_reply_manager_->on_external_update_message_content`.
    fn quick_reply_on_external_update_message_content(
        &mut self,
        message_full_id: QuickReplyMessageFullId,
        source: &str,
    );
    /// `messages_manager_->get_messages_from_server` [blocked: M.10/net].
    fn get_messages_from_server(&mut self, message_full_ids: &[MessageFullId]);
    /// `quick_reply_manager_->reload_quick_reply_message` [blocked: net].
    fn reload_quick_reply_message(
        &mut self,
        shortcut_id: crate::files::QuickReplyShortcutId,
        message_id: crate::message_id::MessageId,
    );
    /// `dialog_manager_->force_create_dialog` [blocked: M.6].
    fn force_create_dialog(&mut self, dialog_id: DialogId);
    /// `story_manager_->on_get_story` [blocked: Wave 3]; answers `None`
    /// (the story id mismatch check degrades to the received id).
    fn on_get_story(
        &mut self,
        dialog_id: DialogId,
        story: Box<telegram_api::StoryItem>,
    ) -> Option<i32>;
    /// `dialog_manager_->get_chat_id_object`.
    fn get_chat_id_object(&self, dialog_id: DialogId, source: &str) -> i64;
    /// `story_manager_->get_story_duration` [blocked: Wave 3]; answers -1.
    fn get_story_duration(&self, story_full_id: StoryFullId) -> i32;
    /// `stickers_manager_->on_get_sticker_document(...).second` [blocked:
    /// M.15]; answers an invalid FileId.
    fn on_get_sticker_document(&mut self, document: Box<telegram_api::Document>) -> FileId;
    /// `documents_manager_->on_get_document` [blocked: M.28]; the wire
    /// document classification. Answers `None` until that plane lands.
    fn on_get_document(
        &mut self,
        document: Box<telegram_api::Document>,
        owner_dialog_id: DialogId,
    ) -> Option<Document>;
    /// `documents_manager_->get_document_mime_type` [blocked: M.28].
    fn get_document_mime_type(&self, file_id: FileId) -> String;
    /// `LinkManager::get_message_link_info(url).media_timestamp`
    /// [blocked: M.23].
    fn get_message_link_media_timestamp(&self, url: &str) -> i32;
    /// `LinkManager::get_background_type_object(url, is_pattern)`
    /// [blocked: M.23].
    fn get_background_type_object(&self, url: &str, is_pattern: bool) -> Option<td_api::BackgroundType>;
    /// `LinkManager::has_video_chat_invite_hash(url)` [blocked: M.23].
    fn has_video_chat_invite_hash(&self, url: &str) -> bool;
    /// `option_manager_->get_option_boolean("is_premium")`.
    fn is_premium(&self) -> bool;
    /// `td_->user_manager_->get_user_id_object` — inherited via
    /// WebPageBlockEnv.

    // -- the `[blocked: net]` query tails; each drops/parks the promise,
    // -- keeping the C++ request unanswered offline
    /// `GetWebPagePreviewQuery` (WebPagesManager.cpp:77-115).
    fn send_get_web_page_preview_query(
        &mut self,
        text: crate::message_entity::FormattedText,
        options: Box<GetWebPagePreviewOptions>,
        promise: Promise<Option<td_api::LinkPreview>>,
    );
    /// `GetWebPageQuery` (WebPagesManager.cpp:117-166) — the reload
    /// fetch; `hash` is the instant-view hash (0 reloads in full).
    fn send_get_web_page_query(
        &mut self,
        web_page_id: WebPageId,
        url: String,
        hash: i32,
        promise: Promise<WebPageId>,
    );
}

/// `WebPagesManager::GetWebPagePreviewOptions` (WebPagesManager.h:52-56).
#[derive(Debug, Default)]
pub struct GetWebPagePreviewOptions {
    pub(crate) first_url: String,
    pub(crate) skip_confirmation: bool,
    pub(crate) link_preview_options: Option<Box<td_api::LinkPreviewOptions>>,
}

// ---------------------------------------------------------------------------
// WebPageInstantView (WebPagesManager.cpp:168-247)
// ---------------------------------------------------------------------------

/// `WebPagesManager::WebPageInstantView`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebPageInstantView {
    pub(crate) page_blocks: Vec<WebPageBlock>,
    pub(crate) url: String,
    pub(crate) view_count: i32,
    pub(crate) hash: i32,
    pub(crate) is_v2: bool,
    pub(crate) is_rtl: bool,
    pub(crate) is_empty: bool,
    pub(crate) is_full: bool,
    pub(crate) is_loaded: bool,
    pub(crate) was_loaded_from_database: bool,
}

impl WebPageInstantView {
    /// `store` (WebPagesManager.cpp:187-210).
    pub fn store(&self, env: &mut impl WebPageBlockEnv, out: &mut Vec<u8>) {
        let has_url = !self.url.is_empty();
        let has_view_count = self.view_count > 0;
        store_flags(
            out,
            [self.is_full, self.is_loaded, self.is_rtl, self.is_v2, has_url, has_view_count],
        );

        store_i32(out, self.page_blocks.len() as i32);
        for page_block in &self.page_blocks {
            page_block.store(env, out);
        }
        store_i32(out, self.hash);
        if has_url {
            store_string(out, &self.url);
        }
        if has_view_count {
            store_i32(out, self.view_count);
        }
        assert!(!self.is_empty, "the C++ CHECKs the instant view is not empty");
    }

    /// `parse` (WebPagesManager.cpp:212-235).
    pub fn parse(
        env: &mut impl WebPageBlockEnv,
        parser: &mut Parser<'_>,
    ) -> Result<WebPageInstantView, &'static str> {
        let mut result = WebPageInstantView::default();
        let [is_full, is_loaded, is_rtl, is_v2, has_url, has_view_count] = parser.parse_flags()?;
        result.is_full = is_full;
        result.is_loaded = is_loaded;
        result.is_rtl = is_rtl;
        result.is_v2 = is_v2;

        let blocks_len = parser.i32_()?;
        if blocks_len < 0 {
            return Err("Invalid vector size");
        }
        for _ in 0..blocks_len {
            result.page_blocks.push(WebPageBlock::parse_block(env, parser)?);
        }
        result.hash = parser.i32_()?;
        if has_url {
            result.url = parser.string()?;
        }
        if has_view_count {
            result.view_count = parser.i32_()?;
        }
        result.is_empty = false;
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// WebPage (WebPagesManager.cpp:249-540)
// ---------------------------------------------------------------------------

/// `WebPagesManager::WebPage`. The `ThemeSettings`/`StarGift` payload
/// fields exist only as their C++ byte-layout presence flags (module
/// doc); `is_album`/`is_album_checked`/`was_reloaded` are the C++
/// mutable runtime flags.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebPage {
    pub(crate) url: String,
    pub(crate) display_url: String,
    pub(crate) type_: String,
    pub(crate) site_name: String,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) photo: Photo,
    pub(crate) embed_url: String,
    pub(crate) embed_type: String,
    pub(crate) embed_dimensions: crate::photo::Dimensions,
    pub(crate) duration: i32,
    pub(crate) author: String,
    pub(crate) has_large_media: bool,
    pub(crate) video_cover_photo: bool,
    pub(crate) is_album: bool,
    pub(crate) is_album_checked: bool,
    pub(crate) was_reloaded: bool,
    pub(crate) document: Document,
    pub(crate) documents: Vec<Document>,
    pub(crate) has_theme_settings: bool,
    pub(crate) story_full_ids: Vec<StoryFullId>,
    pub(crate) sticker_ids: Vec<FileId>,
    pub(crate) has_star_gifts: bool,
    pub(crate) custom_emoji_ids: Vec<CustomEmojiId>,
    pub(crate) instant_view: WebPageInstantView,
    pub(crate) auction_end_date: i32,
    pub(crate) has_gift_background: bool,

    pub(crate) file_source_id: FileSourceId,
    pub(crate) log_event_id: u64,
}

impl WebPage {
    /// `store` (WebPagesManager.cpp:283-394) — the 23 presence flags,
    /// then the body in the C++ order.
    pub fn store(&self, env: &mut impl WebPageBlockEnv, out: &mut Vec<u8>) {
        let has_type = !self.type_.is_empty();
        let has_site_name = !self.site_name.is_empty();
        let has_title = !self.title.is_empty();
        let has_description = !self.description.is_empty();
        let has_photo = !self.photo.is_empty();
        let has_embed = !self.embed_url.is_empty();
        let has_embed_dimensions = has_embed && self.embed_dimensions != crate::photo::Dimensions::default();
        let has_duration = self.duration > 0;
        let has_author = !self.author.is_empty();
        let has_document = !self.document.is_empty();
        let has_instant_view = !self.instant_view.is_empty;
        let is_instant_view_v2 = self.instant_view.is_v2;
        let has_no_hash = true;
        let has_documents = !self.documents.is_empty();
        let has_story_full_ids = !self.story_full_ids.is_empty();
        let has_sticker_ids = !self.sticker_ids.is_empty();
        let has_theme_settings = self.has_theme_settings;
        let has_star_gifts = self.has_star_gifts;
        let has_auction_end_date = self.auction_end_date != 0;
        let has_gift_background = self.has_gift_background;
        let has_custom_emoji_ids = !self.custom_emoji_ids.is_empty();
        store_flags(
            out,
            [
                has_type,
                has_site_name,
                has_title,
                has_description,
                has_photo,
                has_embed,
                has_embed_dimensions,
                has_duration,
                has_author,
                has_document,
                has_instant_view,
                has_no_hash,
                is_instant_view_v2,
                has_documents,
                has_story_full_ids,
                self.has_large_media,
                has_sticker_ids,
                has_theme_settings,
                has_star_gifts,
                self.video_cover_photo,
                has_auction_end_date,
                has_gift_background,
                has_custom_emoji_ids,
            ],
        );

        store_string(out, &self.url);
        store_string(out, &self.display_url);
        if has_type {
            store_string(out, &self.type_);
        }
        if has_site_name {
            store_string(out, &self.site_name);
        }
        if has_title {
            store_string(out, &self.title);
        }
        if has_description {
            store_string(out, &self.description);
        }
        if has_photo {
            crate::photo::store_photo(env, &self.photo, out);
        }
        if has_embed {
            store_string(out, &self.embed_url);
            store_string(out, &self.embed_type);
        }
        if has_embed_dimensions {
            crate::photo::store_dimensions(out, self.embed_dimensions);
        }
        if has_duration {
            store_i32(out, self.duration);
        }
        if has_author {
            store_string(out, &self.author);
        }
        if has_document {
            self.document.store(env, out);
        }
        if has_documents {
            store_i32(out, self.documents.len() as i32);
            for document in &self.documents {
                document.store(env, out);
            }
        }
        if has_story_full_ids {
            store_i32(out, self.story_full_ids.len() as i32);
            for story_full_id in &self.story_full_ids {
                story_full_id.store(out);
            }
        }
        if has_sticker_ids {
            // the StickersManager::store_sticker framing rides the bare
            // FileManager codec (M.16 divergence)
            store_i32(out, self.sticker_ids.len() as u32 as i32);
            for sticker_id in &self.sticker_ids {
                env.store_file_id(*sticker_id, out);
            }
        }
        // has_theme_settings: the ThemeSettings payload is the
        // [blocked: BackgroundManager] seam — never set, never stored
        // has_star_gifts: the StarGift payload is the
        // [blocked: M.42/Wave 3] seam — never set, never stored
        if has_auction_end_date {
            store_i32(out, self.auction_end_date);
        }
        // has_gift_background: kept false until the flag-setting intake
        // (webPageAttributeStarGiftAuction) lands with StarGift
        if has_custom_emoji_ids {
            store_i32(out, self.custom_emoji_ids.len() as i32);
            for custom_emoji_id in &self.custom_emoji_ids {
                store_i64(out, custom_emoji_id.get());
            }
        }
    }

    /// `parse` (WebPagesManager.cpp:396-523).
    pub fn parse(env: &mut impl WebPageBlockEnv, parser: &mut Parser<'_>) -> Result<WebPage, &'static str> {
        let mut result = WebPage::default();
        let [
            has_type,
            has_site_name,
            has_title,
            has_description,
            has_photo,
            has_embed,
            has_embed_dimensions,
            has_duration,
            has_author,
            has_document,
            has_instant_view,
            has_no_hash,
            is_instant_view_v2,
            has_documents,
            has_story_full_ids,
            has_large_media,
            has_sticker_ids,
            has_theme_settings,
            has_star_gifts,
            video_cover_photo,
            has_auction_end_date,
            has_gift_background,
            has_custom_emoji_ids,
        ] = parser.parse_flags()?;
        result.has_large_media = has_large_media;
        result.video_cover_photo = video_cover_photo;
        result.has_theme_settings = has_theme_settings;
        result.has_star_gifts = has_star_gifts;
        result.has_gift_background = has_gift_background;

        result.url = parser.string()?;
        result.display_url = parser.string()?;
        if !has_no_hash {
            // the pre-Support64BitIds hash row, discarded like the C++
            let _hash = parser.i32_()?;
        }
        if has_type {
            result.type_ = parser.string()?;
        }
        if has_site_name {
            result.site_name = parser.string()?;
        }
        if has_title {
            result.title = parser.string()?;
        }
        if has_description {
            result.description = parser.string()?;
        }
        if has_photo {
            result.photo = crate::photo::parse_photo(env, parser)?;
        }
        if has_embed {
            result.embed_url = parser.string()?;
            result.embed_type = parser.string()?;
        }
        if has_embed_dimensions {
            result.embed_dimensions = crate::photo::parse_dimensions(parser)?;
        }
        if has_duration {
            result.duration = parser.i32_()?;
        }
        if has_author {
            result.author = parser.string()?;
        }
        if has_document {
            result.document = Document::parse(env, parser)?;
        }
        if has_documents {
            let documents_len = parser.i32_()?;
            if documents_len < 0 {
                return Err("Invalid vector size");
            }
            for _ in 0..documents_len {
                result.documents.push(Document::parse(env, parser)?);
            }
        }
        if has_story_full_ids {
            let story_full_ids_len = parser.i32_()?;
            if story_full_ids_len < 0 {
                return Err("Invalid vector size");
            }
            for _ in 0..story_full_ids_len {
                let story_full_id = StoryFullId::parse_from(parser)?;
                // the C++ remove_if keeps server stories only
                if story_full_id.get_dialog_id().get_type() == crate::dialog_id::DialogType::Channel {
                    result.story_full_ids.push(story_full_id);
                }
            }
        }
        if has_sticker_ids {
            let sticker_count = parser.u32_()? as usize;
            for _ in 0..sticker_count {
                let sticker_id = env.parse_file_id(parser)?;
                if sticker_id.is_valid() {
                    result.sticker_ids.push(sticker_id);
                }
            }
        }
        if has_theme_settings || has_star_gifts {
            // the foreign payloads are the recorded seams; a legacy
            // C++ binlog carrying them cannot be parsed here
            return Err("Foreign ThemeSettings/StarGift WebPage payload");
        }
        if has_auction_end_date {
            result.auction_end_date = parser.i32_()?;
        }
        if has_gift_background {
            // the payload rows would follow here; unreachable while the
            // flag is never set on this line
            return Err("Foreign StarGiftBackground WebPage payload");
        }
        if has_custom_emoji_ids {
            let custom_emoji_ids_len = parser.i32_()?;
            if custom_emoji_ids_len < 0 {
                return Err("Invalid vector size");
            }
            for _ in 0..custom_emoji_ids_len {
                result.custom_emoji_ids.push(CustomEmojiId::new(parser.i64_()?));
            }
        }

        if has_instant_view {
            result.instant_view.is_empty = false;
        }
        if is_instant_view_v2 {
            result.instant_view.is_v2 = true;
        }
        Ok(result)
    }

    /// `operator==` (WebPagesManager.cpp:525-539) — the C++ field list
    /// (including its duplicated `type_` comparison and the instant-view
    /// is_empty/is_v2 restriction).
    pub fn content_eq(&self, other: &WebPage) -> bool {
        self.url == other.url
            && self.display_url == other.display_url
            && self.type_ == other.type_
            && self.site_name == other.site_name
            && self.title == other.title
            && self.description == other.description
            && self.photo == other.photo
            && self.embed_url == other.embed_url
            && self.embed_type == other.embed_type
            && self.embed_dimensions == other.embed_dimensions
            && self.duration == other.duration
            && self.author == other.author
            && self.has_large_media == other.has_large_media
            && self.video_cover_photo == other.video_cover_photo
            && self.document == other.document
            && self.documents == other.documents
            && self.has_theme_settings == other.has_theme_settings
            && self.story_full_ids == other.story_full_ids
            && self.sticker_ids == other.sticker_ids
            && self.has_star_gifts == other.has_star_gifts
            && self.auction_end_date == other.auction_end_date
            && self.has_gift_background == other.has_gift_background
            && self.custom_emoji_ids == other.custom_emoji_ids
            && self.instant_view.is_empty == other.instant_view.is_empty
            && self.instant_view.is_v2 == other.instant_view.is_v2
    }

    /// `can_web_page_be_album` (WebPagesManager.cpp:1572-1578).
    pub fn can_web_page_be_album(&self) -> bool {
        if self.type_ == "telegram_album" {
            return true;
        }
        let site_name = self.site_name.to_lowercase();
        site_name == "instagram" || site_name == "twitter" || site_name == "x"
    }

    /// `is_web_page_album` (WebPagesManager.cpp:1580-1594) — the C++
    /// mutable flags become `&mut self`.
    pub fn is_web_page_album(&mut self) -> bool {
        if !self.is_album_checked {
            self.is_album_checked = true;
            if self.type_ == "telegram_album" {
                self.is_album = true;
            } else if self.can_web_page_be_album() && !self.instant_view.is_empty {
                if !self.instant_view.is_loaded {
                    tdutils::log_error!("Have no instant view for {}", self.url);
                } else {
                    self.is_album =
                        WebPageBlock::are_allowed_album_block_types(&self.instant_view.page_blocks);
                }
            }
        }
        self.is_album
    }
}
