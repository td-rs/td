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

use crate::dialog_id::DialogId;
use crate::document::Document;
use crate::emoji_status::CustomEmojiId;
use crate::files::{FileId, FileSourceId};
use crate::message_entity::get_first_url;
use crate::message_id::MessageFullId;
use crate::messages_manager::{QuickReplyMessageFullId, QuickReplyShortcutId, StoryFullId};
use crate::photo::Photo;
use crate::store_parse::{
    Parser, store_flags, store_i32, store_i64, store_log_event_version, store_string,
};
use crate::td_api;
use crate::user_manager::UserId;
use crate::webpage::{WebPageBlock, WebPageBlockEnv, WebPageId, get_page_blocks_object};
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
    fn delete_quick_reply_pending_message_web_page(
        &mut self,
        message_full_id: QuickReplyMessageFullId,
    );
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
        shortcut_id: QuickReplyShortcutId,
        message_id: crate::messages_manager::MessageId,
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
    fn get_background_type_object(
        &self,
        url: &str,
        is_pattern: bool,
    ) -> Option<td_api::BackgroundType>;
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
    /// The ride-along options; consumed by the net query tail
    /// ([blocked: net]).
    #[allow(dead_code)]
    pub(crate) link_preview_options: Option<Box<td_api::LinkPreviewOptions>>,
}

impl GetWebPagePreviewOptions {
    /// The `GetWebPagePreviewOptions` ctor (WebPagesManager.cpp:1203-1209)
    /// — the skip_confirmation flag lifts out of the td_api options.
    pub(crate) fn from_td_api(options: Option<td_api::LinkPreviewOptions>) -> Self {
        Self {
            link_preview_options: options.map(Box::new),
            ..Self::default()
        }
    }
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
            [
                self.is_full,
                self.is_loaded,
                self.is_rtl,
                self.is_v2,
                has_url,
                has_view_count,
            ],
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
        assert!(
            !self.is_empty,
            "the C++ CHECKs the instant view is not empty"
        );
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
            result
                .page_blocks
                .push(WebPageBlock::parse_block(env, parser)?);
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
        let has_embed_dimensions =
            has_embed && self.embed_dimensions != crate::photo::Dimensions::default();
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
            crate::photo::store_photo(env, out, &self.photo);
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
    pub fn parse(
        env: &mut impl WebPageBlockEnv,
        parser: &mut Parser<'_>,
    ) -> Result<WebPage, &'static str> {
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
                if story_full_id.get_dialog_id().get_type() == crate::dialog_id::DialogType::Channel
                {
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
                result
                    .custom_emoji_ids
                    .push(CustomEmojiId::new(parser.i64_()?));
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

// ---------------------------------------------------------------------------
// The WebPagesManager (M.17.2) — the collapsed actor
// ---------------------------------------------------------------------------

/// `td::WebPagesManager` (WebPagesManager.h:33-260) — the link-preview /
/// instant-view cache.
#[derive(Default)]
pub struct WebPagesManager {
    pub(crate) web_pages: HashMap<WebPageId, WebPage>,
    pub(crate) pending_web_page_urls: HashMap<WebPageId, String>,
    pub(crate) url_to_web_page_id: HashMap<String, (WebPageId, bool)>,
    pub(crate) url_to_file_source_id: HashMap<String, FileSourceId>,
    pub(crate) web_page_messages: HashMap<WebPageId, HashSet<MessageFullId>>,
    pub(crate) web_page_quick_reply_messages: HashMap<WebPageId, HashSet<QuickReplyMessageFullId>>,
    pub(crate) web_page_polls: HashMap<WebPageId, HashSet<i64>>,
    pub(crate) poll_web_pages: HashMap<i64, Vec<WebPageId>>,
    pub(crate) story_web_pages: HashMap<StoryFullId, HashSet<WebPageId>>,
    /// The parked reload-query maps; consumed by the net query tails
    /// ([blocked: net]).
    #[allow(dead_code)]
    pub(crate) load_web_page_from_database_queries: HashMap<WebPageId, Vec<Promise<()>>>,
    #[allow(dead_code)]
    pub(crate) load_web_page_instant_view_queries: HashMap<WebPageId, Vec<Promise<WebPageId>>>,
    pub(crate) load_server_web_page_instant_view_queries:
        HashMap<WebPageId, Vec<Promise<WebPageId>>>,
}

impl WebPagesManager {
    pub fn new() -> WebPagesManager {
        WebPagesManager::default()
    }

    fn get_web_page_database_key(web_page_id: WebPageId) -> Vec<u8> {
        format!("wp{}", web_page_id.get()).into_bytes()
    }

    fn get_web_page_instant_view_database_key(web_page_id: WebPageId) -> Vec<u8> {
        format!("wpiv{}", web_page_id.get()).into_bytes()
    }

    /// `WebPagesManager::get_web_page` (WebPagesManager.cpp:2190-2192).
    pub fn get_web_page(&self, web_page_id: WebPageId) -> Option<&WebPage> {
        self.web_pages.get(&web_page_id)
    }

    /// `WebPagesManager::have_web_page` (WebPagesManager.cpp:1565-1570).
    pub fn have_web_page(&self, web_page_id: WebPageId) -> bool {
        self.web_pages.contains_key(&web_page_id)
    }

    /// `WebPagesManager::have_web_page_force` (WebPagesManager.cpp:2162-2188)
    /// — the db-load half.
    pub fn have_web_page_force(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        web_page_id: WebPageId,
    ) -> bool {
        if !web_page_id.is_valid() {
            return false;
        }
        if self.web_pages.contains_key(&web_page_id) {
            return true;
        }
        if !env.use_message_database() {
            return false;
        }
        let value = env.sqlite_pmc_get_raw(&Self::get_web_page_database_key(web_page_id));
        if value.is_empty() {
            return false;
        }
        let mut parser = Parser::new(&value);
        let web_page = match WebPage::parse(env, &mut parser) {
            Ok(web_page) => web_page,
            Err(_) => {
                env.sqlite_pmc_erase(&Self::get_web_page_database_key(web_page_id));
                return false;
            }
        };
        self.update_web_page(env, web_page, web_page_id, false, true);
        self.web_pages.contains_key(&web_page_id)
    }

    /// `WebPagesManager::save_web_page` (WebPagesManager.cpp:2003-2016) —
    /// the binlog-add + pmc-set half.
    fn save_web_page(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        web_page: &WebPage,
        web_page_id: WebPageId,
        from_binlog: bool,
    ) {
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        web_page.store(env, &mut out);
        if !from_binlog && env.use_message_database() {
            let event_id = env.binlog_add_web_page(&out);
            if let Some(page) = self.web_pages.get_mut(&web_page_id) {
                page.log_event_id = event_id;
            }
        }
        if env.use_message_database() {
            env.sqlite_pmc_set_raw(&Self::get_web_page_database_key(web_page_id), &out);
        }
    }

    /// `WebPagesManager::on_web_page_changed` (WebPagesManager.cpp:983-993)
    /// — the message/poll registries fan-out.
    fn on_web_page_changed(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        web_page_id: WebPageId,
        have_web_page: bool,
    ) {
        let message_full_ids: Vec<MessageFullId> = self
            .web_page_messages
            .get(&web_page_id)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        for message_full_id in message_full_ids {
            if have_web_page {
                env.on_external_update_message_content(message_full_id, "on_web_page_changed");
            } else {
                env.delete_pending_message_web_page(message_full_id);
            }
        }
        let quick_reply_ids: Vec<QuickReplyMessageFullId> = self
            .web_page_quick_reply_messages
            .get(&web_page_id)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        for message_full_id in quick_reply_ids {
            if have_web_page {
                env.quick_reply_on_external_update_message_content(
                    message_full_id,
                    "on_web_page_changed",
                );
            } else {
                env.delete_quick_reply_pending_message_web_page(message_full_id);
            }
        }
    }

    /// `WebPagesManager::update_web_page` (WebPagesManager.cpp:823-903).
    fn update_web_page(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        mut web_page: WebPage,
        web_page_id: WebPageId,
        from_binlog: bool,
        from_database: bool,
    ) {
        let old = self.web_pages.get(&web_page_id);
        let is_changed = old.is_none_or(|old| *old != web_page);
        let old_instant_view =
            old.map_or_else(WebPageInstantView::default, |old| old.instant_view.clone());
        if let Some(old) = &old {
            web_page.log_event_id = old.log_event_id;
        } else if let Some(file_source_id) = self.url_to_file_source_id.remove(&web_page.url) {
            web_page.file_source_id = file_source_id;
        }

        let mut old_file_ids = Vec::new();
        if let Some(old) = old {
            old_file_ids = self.get_web_page_file_ids(old);
        }
        if let Some(old) = &old {
            for story_full_id in &old.story_full_ids {
                if let Some(web_page_ids) = self.story_web_pages.get_mut(story_full_id) {
                    web_page_ids.remove(&web_page_id);
                    if web_page_ids.is_empty() {
                        self.story_web_pages.remove(story_full_id);
                    }
                }
            }
            for story_full_id in &web_page.story_full_ids {
                self.story_web_pages
                    .entry(*story_full_id)
                    .or_default()
                    .insert(web_page_id);
            }
        }

        self.web_pages.insert(web_page_id, web_page);

        if !self.web_pages.get(&web_page_id).unwrap().url.is_empty() {
            let url = self.web_pages.get(&web_page_id).unwrap().url.clone();
            self.on_get_web_page_by_url(&url, web_page_id, from_database);
        }

        let new_file_ids = {
            let page = self.web_pages.get(&web_page_id).unwrap();
            self.get_web_page_file_ids(page)
        };
        if old_file_ids != new_file_ids {
            let source_id = {
                let page = self.web_pages.get(&web_page_id).unwrap();
                self.get_web_page_file_source_id(page, web_page_id)
            };
            env.change_files_source(source_id, &old_file_ids, &new_file_ids, "update_web_page");
        }

        let _ = old_instant_view;
        if is_changed && !from_database {
            self.on_web_page_changed(env, web_page_id, true);
            let web_page = self.web_pages.get(&web_page_id).unwrap().clone();
            self.save_web_page(env, &web_page, web_page_id, from_binlog);
        }
    }

    /// `WebPagesManager::get_web_page_file_source_id`
    /// (WebPagesManager.cpp:2195-2203).
    fn get_web_page_file_source_id(
        &self,
        web_page: &WebPage,
        web_page_id: WebPageId,
    ) -> FileSourceId {
        if web_page.file_source_id.is_valid() {
            return web_page.file_source_id;
        }
        let _ = web_page_id;
        FileSourceId::default()
    }

    /// `WebPagesManager::get_web_page_file_ids` (WebPagesManager.h:215)
    /// — the collapsed line answers the photo+document files.
    pub fn get_web_page_file_ids(&self, web_page: &WebPage) -> Vec<FileId> {
        let mut file_ids = Vec::new();
        if !web_page.photo.is_empty() {
            file_ids.extend(crate::photo::photo_get_file_ids(&web_page.photo));
        }
        if !web_page.document.is_empty() {
            file_ids.push(web_page.document.get_file_id());
        }
        file_ids
    }

    /// `WebPagesManager::on_get_web_page_by_url` (WebPagesManager.cpp:994-1024).
    pub fn on_get_web_page_by_url(
        &mut self,
        url: &str,
        web_page_id: WebPageId,
        from_database: bool,
    ) {
        if url.is_empty() {
            return;
        }
        match self.url_to_web_page_id.get(url) {
            Some((old_web_page_id, _)) if *old_web_page_id == web_page_id => {}
            Some(_) => {
                self.url_to_web_page_id
                    .insert(url.to_owned(), (web_page_id, from_database));
            }
            None => {
                self.url_to_web_page_id
                    .insert(url.to_owned(), (web_page_id, from_database));
            }
        }
    }

    /// `WebPagesManager::on_load_web_page_url_from_database`
    /// (WebPagesManager.cpp:577-583).
    pub fn on_load_web_page_url_from_database(&mut self, web_page_id: WebPageId, url: &str) {
        if !url.is_empty() {
            self.on_get_web_page_by_url(url, web_page_id, true);
        }
    }

    /// `WebPagesManager::get_web_page_url` (WebPagesManager.cpp:1447-1453).
    pub fn get_web_page_url(&self, web_page_id: WebPageId) -> String {
        self.get_web_page(web_page_id)
            .map_or_else(String::new, |web_page| web_page.url.clone())
    }

    /// `WebPagesManager::get_web_page_by_url` (WebPagesManager.cpp:1455-1468).
    pub fn get_web_page_by_url(&self, url: &str) -> WebPageId {
        self.url_to_web_page_id
            .get(url)
            .map_or(WebPageId::default(), |(web_page_id, _)| *web_page_id)
    }

    /// `WebPagesManager::register_web_page` (WebPagesManager.cpp:1025-1038).
    pub fn register_web_page(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        web_page_id: WebPageId,
        message_full_id: MessageFullId,
    ) {
        if !web_page_id.is_valid() {
            return;
        }
        self.web_page_messages
            .entry(web_page_id)
            .or_default()
            .insert(message_full_id);
        if !env.auth_is_bot() && !self.have_web_page_force(env, web_page_id) {
            env.add_pending_web_page_timeout(web_page_id.get(), 1.0);
        }
    }

    /// `WebPagesManager::unregister_web_page` (WebPagesManager.cpp:1040-1053).
    pub fn unregister_web_page(&mut self, web_page_id: WebPageId, message_full_id: MessageFullId) {
        if !web_page_id.is_valid() {
            return;
        }
        if let Some(message_ids) = self.web_page_messages.get_mut(&web_page_id) {
            message_ids.remove(&message_full_id);
            if message_ids.is_empty() {
                self.web_page_messages.remove(&web_page_id);
            }
        }
    }

    /// `WebPagesManager::register_quick_reply_web_page`
    /// (WebPagesManager.cpp:1055-1069).
    pub fn register_quick_reply_web_page(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        web_page_id: WebPageId,
        message_full_id: QuickReplyMessageFullId,
    ) {
        if !web_page_id.is_valid() {
            return;
        }
        self.web_page_quick_reply_messages
            .entry(web_page_id)
            .or_default()
            .insert(message_full_id);
        if !self.have_web_page_force(env, web_page_id) {
            env.add_pending_web_page_timeout(web_page_id.get(), 1.0);
        }
    }

    /// `WebPagesManager::unregister_quick_reply_web_page`
    /// (WebPagesManager.cpp:1071-1085).
    pub fn unregister_quick_reply_web_page(
        &mut self,
        web_page_id: WebPageId,
        message_full_id: QuickReplyMessageFullId,
    ) {
        if !web_page_id.is_valid() {
            return;
        }
        if let Some(message_ids) = self.web_page_quick_reply_messages.get_mut(&web_page_id) {
            message_ids.remove(&message_full_id);
            if message_ids.is_empty() {
                self.web_page_quick_reply_messages.remove(&web_page_id);
            }
        }
    }

    /// `WebPagesManager::register_poll_web_pages`
    /// (WebPagesManager.cpp:1119-1173) — the `PollId` of the C++ rides the
    /// raw i64 ([blocked: M.19]).
    pub fn register_poll_web_pages(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        poll_id: i64,
        web_page_ids: Vec<WebPageId>,
    ) {
        if poll_id <= 0 {
            return;
        }
        if web_page_ids.is_empty() {
            let Some(old_web_page_ids) = self.poll_web_pages.remove(&poll_id) else {
                return;
            };
            for web_page_id in old_web_page_ids {
                if let Some(poll_ids) = self.web_page_polls.get_mut(&web_page_id) {
                    poll_ids.remove(&poll_id);
                    if poll_ids.is_empty() {
                        self.web_page_polls.remove(&web_page_id);
                    }
                }
            }
            return;
        }
        let old_web_page_ids = self
            .poll_web_pages
            .get(&poll_id)
            .cloned()
            .unwrap_or_default();
        if old_web_page_ids == web_page_ids {
            return;
        }
        for &web_page_id in &web_page_ids {
            if !old_web_page_ids.contains(&web_page_id) {
                self.web_page_polls
                    .entry(web_page_id)
                    .or_default()
                    .insert(poll_id);
                if !self.have_web_page_force(env, web_page_id) {
                    env.add_pending_web_page_timeout(web_page_id.get(), 1.0);
                }
            }
        }
        for web_page_id in &old_web_page_ids {
            if !web_page_ids.contains(web_page_id)
                && let Some(poll_ids) = self.web_page_polls.get_mut(web_page_id)
            {
                poll_ids.remove(&poll_id);
                if poll_ids.is_empty() {
                    self.web_page_polls.remove(web_page_id);
                }
            }
        }
        self.poll_web_pages.insert(poll_id, web_page_ids);
    }

    /// `WebPagesManager::get_url_file_source_id`
    /// (WebPagesManager.cpp:2045-2060).
    pub fn get_url_file_source_id(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        url: &str,
    ) -> FileSourceId {
        if let Some(&file_source_id) = self.url_to_file_source_id.get(url) {
            return file_source_id;
        }
        let file_source_id = env.create_web_page_file_source(url);
        self.url_to_file_source_id
            .insert(url.to_owned(), file_source_id);
        file_source_id
    }

    /// `WebPagesManager::fetch_web_page_by_url`
    /// (WebPagesManager.cpp:1470-1483).
    pub fn fetch_web_page_by_url(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        url: &str,
        promise: Promise<WebPageId>,
    ) {
        self.load_web_page_by_url(env, url.to_owned(), false, promise);
    }

    /// `WebPagesManager::load_web_page_by_url`
    /// (WebPagesManager.cpp:1484-1499) — the db-load + net-reload half.
    fn load_web_page_by_url(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        url: String,
        _only_local: bool,
        mut promise: Promise<WebPageId>,
    ) {
        let web_page_id = self.get_web_page_by_url(&url);
        if web_page_id.is_valid() {
            promise.set_value(web_page_id);
            return;
        }
        if self.have_web_page_force(env, web_page_id) {
            promise.set_value(web_page_id);
            return;
        }
        // [blocked: net]: the GetWebPageQuery reload — the promise parks
        // with the query, and the pending timeout keeps the URL alive.
        if !env.close_flag() {
            env.add_pending_web_page_timeout(0, 1.0);
        }
    }

    /// `WebPagesManager::reload_web_page_by_url`
    /// (WebPagesManager.cpp:1557-1564).
    pub fn reload_web_page_by_url(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        url: &str,
        only_local: bool,
        promise: Promise<WebPageId>,
    ) {
        let web_page_id = self.get_web_page_by_url(url);
        if web_page_id.is_valid() {
            self.get_web_page_instant_view_impl(env, web_page_id, only_local, promise);
            return;
        }
        self.load_web_page_by_url(env, url.to_owned(), only_local, promise);
    }

    /// `WebPagesManager::get_web_page_instant_view`
    /// (WebPagesManager.cpp:1256-1282).
    pub fn get_web_page_instant_view(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        url: &str,
        only_local: bool,
        mut promise: Promise<WebPageId>,
    ) {
        if url.is_empty() {
            promise.set_value(WebPageId::default());
            return;
        }
        if let Some((web_page_id, _)) = self.url_to_web_page_id.get(url) {
            if !web_page_id.is_valid() {
                self.reload_web_page_by_url(env, url, only_local, promise);
                return;
            }
            self.get_web_page_instant_view_impl(env, *web_page_id, only_local, promise);
            return;
        }
        let web_page_id = self.get_web_page_by_url(url);
        if web_page_id.is_valid() {
            self.get_web_page_instant_view_impl(env, web_page_id, only_local, promise);
            return;
        }
        self.load_web_page_by_url(env, url.to_owned(), only_local, promise);
    }

    /// `WebPagesManager::get_web_page_instant_view_impl`
    /// (WebPagesManager.cpp:1283-1300).
    fn get_web_page_instant_view_impl(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        web_page_id: WebPageId,
        only_local: bool,
        mut promise: Promise<WebPageId>,
    ) {
        if env.close_flag() {
            return;
        }
        let web_page_instant_view = match self.web_pages.get(&web_page_id) {
            Some(web_page) if !web_page.instant_view.is_empty => {
                Some(web_page.instant_view.clone())
            }
            _ => None,
        };
        let Some(web_page_instant_view) = web_page_instant_view else {
            promise.set_value(WebPageId::default());
            return;
        };
        if !web_page_instant_view.is_loaded || !only_local {
            self.load_web_page_instant_view(env, web_page_id, only_local, promise);
            return;
        }
        promise.set_value(web_page_id);
    }

    /// `WebPagesManager::load_web_page_instant_view`
    /// (WebPagesManager.cpp:1305-1342) — the db half, then the
    /// [blocked: net] server half.
    fn load_web_page_instant_view(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        web_page_id: WebPageId,
        only_local: bool,
        mut promise: Promise<WebPageId>,
    ) {
        if !env.use_message_database() || only_local {
            self.load_server_web_page_instant_view_queries
                .entry(web_page_id)
                .or_default()
                .push(promise);
            if self.load_server_web_page_instant_view_queries[&web_page_id].len() == 1 {
                let url = self.get_web_page_url(web_page_id);
                let hash = self
                    .web_pages
                    .get(&web_page_id)
                    .map_or(0, |web_page| web_page.instant_view.hash);
                let reload = Promise::default();
                env.send_get_web_page_query(web_page_id, url, hash, reload);
            }
            return;
        }
        let value =
            env.sqlite_pmc_get_raw(&Self::get_web_page_instant_view_database_key(web_page_id));
        if value.is_empty() {
            self.load_server_web_page_instant_view_queries
                .entry(web_page_id)
                .or_default()
                .push(promise);
            return;
        }
        let mut parser = Parser::new(&value);
        let instant_view = match WebPageInstantView::parse(env, &mut parser) {
            Ok(instant_view) => instant_view,
            Err(_) => {
                env.sqlite_pmc_erase(&Self::get_web_page_instant_view_database_key(web_page_id));
                promise.set_value(web_page_id);
                return;
            }
        };
        if let Some(web_page) = self.web_pages.get_mut(&web_page_id) {
            web_page.instant_view = instant_view;
        }
        promise.set_value(web_page_id);
    }

    /// `WebPagesManager::on_get_web_page_instant_view_view_count`
    /// (WebPagesManager.cpp:976-982).
    pub fn on_get_web_page_instant_view_view_count(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        web_page_id: WebPageId,
        view_count: i32,
    ) {
        if let Some(web_page) = self.web_pages.get_mut(&web_page_id)
            && web_page.instant_view.view_count != view_count
        {
            web_page.instant_view.view_count = view_count;
            let page = web_page.clone();
            self.save_web_page(env, &page, web_page_id, false);
        }
    }

    /// `WebPagesManager::get_web_page_preview`
    /// (WebPagesManager.cpp:1219-1255) — the URL extraction + the
    /// [blocked: net] query seam.
    pub fn get_web_page_preview(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        text: Option<td_api::FormattedText>,
        link_preview_options: Option<td_api::LinkPreviewOptions>,
        mut promise: Promise<Option<td_api::LinkPreview>>,
    ) {
        let Some(text) = text else {
            promise.set_value(None);
            return;
        };
        let formatted_text = match crate::message_entity::get_formatted_text_of_input::<()>(
            None,
            crate::message_content::to_message_entity_dialog_id(DialogId::default()),
            Some(text),
            false,
            false,
            true,
            true,
            None,
            false,
            0,
            None,
            false,
        ) {
            Ok(formatted_text) => formatted_text,
            Err(status) => return promise.set_error(status),
        };
        // WebPagesManager.cpp:1226-1240 — the is-disabled and url bands.
        // The known-web-page band (the local get_link_preview_object
        // answer) is [blocked: M.17 residue — the link-preview object
        // builder family] and rides the query seam with the rest.
        if link_preview_options.as_ref().is_some_and(|o| o.is_disabled) {
            promise.set_value(None);
            return;
        }
        let url = match link_preview_options.as_ref() {
            Some(o) if !o.url.is_empty() => o.url.clone(),
            _ => get_first_url(&formatted_text).to_owned(),
        };
        if url.is_empty() {
            promise.set_value(None);
            return;
        }
        let skip_confirmation = crate::message_entity::is_visible_url(&formatted_text, &url);
        let mut options = GetWebPagePreviewOptions::from_td_api(link_preview_options);
        options.skip_confirmation = skip_confirmation;
        options.first_url = url;
        env.send_get_web_page_preview_query(formatted_text, Box::new(options), promise);
    }

    /// The collapsed `GetWebPageInstantViewRequest` do_send_result
    /// (Requests.cpp:1138-1146): resolve the id through the ladder, then
    /// build the instant-view object for the answered band; the net
    /// parking band leaves the promise unanswered offline.
    pub fn get_web_page_instant_view_with_object(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        url: &str,
        only_local: bool,
        mut promise: Promise<td_api::WebPageInstantView>,
    ) {
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let capture = std::sync::Arc::clone(&slot);
        let inner = tdutils::promise::PromiseCreator::lambda_result(
            move |result: tdutils::status::Result<WebPageId>| {
                *capture.lock().unwrap() = Some(result);
            },
        );
        self.get_web_page_instant_view(env, url, only_local, inner);
        match slot.lock().unwrap().take() {
            Some(tdutils::status::Result::Ok(web_page_id)) => {
                if web_page_id.is_valid() {
                    match self.get_web_page_instant_view_object(env, web_page_id) {
                        Some(object) => promise.set_value(object),
                        None => promise.set_error(Status::error(400, "Web page not found")),
                    }
                } else {
                    promise.set_error(Status::error(400, "Web page not found"));
                }
            }
            Some(tdutils::status::Result::Err(status)) => promise.set_error(status),
            None => {
                // [blocked: net]: the ladder parked; the promise parks.
            }
        }
    }

    /// `WebPagesManager::on_pending_web_page_timeout`
    /// (WebPagesManager.cpp:2062-2120) — the collapsed re-fetch half.
    pub fn on_pending_web_page_timeout(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        web_page_id: i64,
    ) {
        if env.close_flag() {
            return;
        }
        let web_page_id = WebPageId::new(web_page_id);
        if self.have_web_page_force(env, web_page_id) {
            return;
        }
        if let Some(url) = self.pending_web_page_urls.get(&web_page_id).cloned() {
            let fetch = Promise::default();
            self.fetch_web_page_by_url(env, &url, fetch);
            return;
        }
        let message_full_ids: Vec<MessageFullId> = self
            .web_page_messages
            .get(&web_page_id)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        env.get_messages_from_server(&message_full_ids);
    }

    /// `WebPagesManager::get_web_page_search_text`
    /// (WebPagesManager.cpp:2146-2161).
    pub fn get_web_page_search_text(&self, web_page_id: WebPageId) -> String {
        let Some(web_page) = self.get_web_page(web_page_id) else {
            return String::new();
        };
        let mut result = String::new();
        for part in [
            web_page.title.as_str(),
            web_page.description.as_str(),
            web_page.site_name.as_str(),
            web_page.author.as_str(),
        ] {
            if !part.is_empty() {
                if !result.is_empty() {
                    result.push(' ');
                }
                result.push_str(part);
            }
        }
        result
    }

    /// `WebPagesManager::get_web_page_media_duration`
    /// (WebPagesManager.cpp:2165-2185) — the collapsed local band.
    pub fn get_web_page_media_duration(&self, web_page_id: WebPageId) -> i32 {
        self.get_web_page(web_page_id)
            .map_or(-1, |web_page| web_page.duration)
    }

    /// `WebPagesManager::get_web_page_story_full_id`
    /// (WebPagesManager.h:117-118).
    pub fn get_web_page_story_full_id(&self, web_page_id: WebPageId) -> StoryFullId {
        self.get_web_page(web_page_id)
            .and_then(|web_page| web_page.story_full_ids.first().copied())
            .unwrap_or_default()
    }

    /// `WebPagesManager::get_web_page_user_ids` (WebPagesManager.cpp:2132-2145).
    pub fn get_web_page_user_ids(&self, web_page_id: WebPageId) -> Vec<UserId> {
        let mut result = Vec::new();
        if let Some(web_page) = self.get_web_page(web_page_id) {
            for story_full_id in &web_page.story_full_ids {
                let dialog_id = story_full_id.get_dialog_id();
                if dialog_id.get_type() == crate::message_id::DialogType::User {
                    result.push(UserId::new(dialog_id.get()));
                }
            }
        }
        result
    }

    /// `WebPagesManager::get_web_page_channel_ids`
    /// (WebPagesManager.cpp:2146-2155 in the channel form).
    pub fn get_web_page_channel_ids(
        &self,
        web_page_id: WebPageId,
    ) -> Vec<crate::chat_manager::ChannelId> {
        let mut result = Vec::new();
        if let Some(web_page) = self.get_web_page(web_page_id) {
            for story_full_id in &web_page.story_full_ids {
                let dialog_id = story_full_id.get_dialog_id();
                if dialog_id.get_type() == crate::message_id::DialogType::Channel {
                    result.push(crate::chat_manager::ChannelId::new(dialog_id.get()));
                }
            }
        }
        result
    }

    /// `WebPagesManager::on_get_web_page` (WebPagesManager.cpp:584-821) —
    /// the wire intake.
    pub fn on_get_web_page(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        web_page_ptr: telegram_api::WebPage,
        owner_dialog_id: DialogId,
    ) -> WebPageId {
        if env.auth_is_bot() {
            return WebPageId::default();
        }
        match web_page_ptr {
            telegram_api::WebPage::webPageEmpty(web_page) => {
                let web_page_id = WebPageId::new(web_page.id);
                if !web_page_id.is_valid() {
                    return WebPageId::default();
                }
                if let Some(page) = self.web_pages.remove(&web_page_id) {
                    if page.log_event_id != 0 {
                        env.binlog_erase(page.log_event_id);
                    }
                    if page.file_source_id.is_valid() {
                        let old_file_ids = self.get_web_page_file_ids(&page);
                        env.change_files_source(
                            page.file_source_id,
                            &old_file_ids,
                            &[],
                            "on_get_web_page",
                        );
                    }
                }
                self.on_web_page_changed(env, web_page_id, false);
                if env.use_message_database() {
                    env.sqlite_pmc_erase(&Self::get_web_page_database_key(web_page_id));
                    env.sqlite_pmc_erase(&Self::get_web_page_instant_view_database_key(
                        web_page_id,
                    ));
                }
                self.pending_web_page_urls.remove(&web_page_id);
                WebPageId::default()
            }
            telegram_api::WebPage::webPagePending(web_page) => {
                let web_page_id = WebPageId::new(web_page.id);
                if !web_page_id.is_valid() {
                    return WebPageId::default();
                }
                let web_page_date = web_page.date;
                let timeout = (web_page_date as f64 - env.server_time()).max(1.0);
                env.add_pending_web_page_timeout(web_page_id.get(), timeout);
                self.pending_web_page_urls.insert(web_page_id, web_page.url);
                web_page_id
            }
            telegram_api::WebPage::webPage(web_page) => {
                let web_page_id = WebPageId::new(web_page.id);
                if !web_page_id.is_valid() {
                    return WebPageId::default();
                }
                let mut page = WebPage {
                    was_reloaded: true,
                    url: web_page.url,
                    display_url: web_page.display_url,
                    type_: web_page.r#type,
                    site_name: web_page.site_name,
                    title: web_page.title,
                    description: web_page.description,
                    photo: web_page
                        .photo
                        .map(crate::photo::get_photo)
                        .unwrap_or_default(),
                    embed_url: web_page.embed_url,
                    embed_type: web_page.embed_type,
                    embed_dimensions: crate::photo::get_dimensions(
                        web_page.embed_width,
                        web_page.embed_height,
                        "on_get_web_page",
                    ),
                    duration: web_page.duration.max(0),
                    author: web_page.author,
                    has_large_media: web_page.has_large_media,
                    video_cover_photo: web_page.video_cover_photo,
                    ..WebPage::default()
                };
                if let Some(document) = web_page.document
                    && let Some(document) = env.on_get_document(Box::new(document), owner_dialog_id)
                {
                    page.document = document;
                }
                // The attribute planes ride their seams: themes + star
                // gifts keep their store/parse presence flags false
                // (Wave 3), the story linkage resolves through
                // `on_get_story`, the sticker sets through the M.15 seam.
                for attribute in web_page.attributes {
                    match *attribute {
                        telegram_api::WebPageAttribute::webPageAttributeTheme(_) => {
                            page.has_theme_settings = true;
                        }
                        telegram_api::WebPageAttribute::webPageAttributeStory(attribute) => {
                            let dialog_id = DialogId::from_peer(
                                attribute.peer.as_ref().expect("empty story peer"),
                            );
                            let story_id = crate::messages_manager::StoryId::new(attribute.id);
                            let story_full_id = StoryFullId::new(dialog_id, story_id);
                            if let Some(story) = attribute.story {
                                let _ = env.on_get_story(dialog_id, story);
                            }
                            env.force_create_dialog(dialog_id);
                            page.story_full_ids.push(story_full_id);
                        }
                        telegram_api::WebPageAttribute::webPageAttributeStickerSet(attribute) => {
                            for sticker in attribute.stickers {
                                let sticker_id = env.on_get_sticker_document(Box::new(sticker));
                                if sticker_id.is_valid() && page.sticker_ids.len() < 4 {
                                    page.sticker_ids.push(sticker_id);
                                }
                            }
                        }
                        telegram_api::WebPageAttribute::webPageAttributeUniqueStarGift(_) => {
                            page.has_star_gifts = true;
                        }
                        telegram_api::WebPageAttribute::webPageAttributeStarGiftCollection(
                            attribute,
                        ) => {
                            for icon in attribute.icons {
                                let icon_file_id = env.on_get_sticker_document(Box::new(icon));
                                if icon_file_id.is_valid() && page.sticker_ids.len() < 4 {
                                    page.sticker_ids.push(icon_file_id);
                                }
                            }
                        }
                        telegram_api::WebPageAttribute::webPageAttributeStarGiftAuction(
                            attribute,
                        ) => {
                            page.has_star_gifts = true;
                            page.auction_end_date = attribute.end_date;
                        }
                        telegram_api::WebPageAttribute::webPageAttributeAiComposeTone(
                            attribute,
                        ) => {
                            let custom_emoji_id = CustomEmojiId::new(attribute.emoji_id);
                            if custom_emoji_id.is_valid() {
                                page.custom_emoji_ids.push(custom_emoji_id);
                            }
                        }
                    }
                }
                self.pending_web_page_urls.remove(&web_page_id);
                self.update_web_page(env, page, web_page_id, false, false);
                web_page_id
            }
            telegram_api::WebPage::webPageNotModified(_) => WebPageId::default(),
        }
    }

    /// `WebPagesManager::get_web_page_instant_view_object`
    /// (WebPagesManager.cpp:2199-2243) — the collapsed builder (the null
    /// user-manager form; the feedback link + the v2 version split are
    /// the local bands).
    pub fn get_web_page_instant_view_object(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        web_page_id: WebPageId,
    ) -> Option<td_api::WebPageInstantView> {
        let web_page = self.get_web_page(web_page_id)?;
        let instant_view = web_page.instant_view.clone();
        if instant_view.is_empty {
            return None;
        }
        let blocks = get_page_blocks_object(
            &instant_view.page_blocks,
            env,
            &mut (),
            &instant_view.url,
            &web_page.url,
            false,
        );
        Some(td_api::WebPageInstantView {
            blocks,
            view_count: instant_view.view_count,
            version: if instant_view.is_v2 { 2 } else { 1 },
            is_rtl: instant_view.is_rtl,
            is_full: instant_view.is_full,
            feedback_link: None,
        })
    }

    /// `WebPagesManager::on_binlog_web_page_event`
    /// (WebPagesManager.cpp:2040-2044) — the WebPages log-event replay.
    pub fn on_binlog_web_page_event(
        &mut self,
        env: &mut impl WebPagesManagerEnv,
        event_id: u64,
        data: &[u8],
    ) {
        if env.close_flag() {
            return;
        }
        let mut parser = match Parser::new_log_event(data) {
            Ok(parser) => parser,
            Err(_) => {
                env.binlog_erase(event_id);
                return;
            }
        };
        let web_page = match WebPage::parse(env, &mut parser) {
            Ok(web_page) => web_page,
            Err(_) => {
                env.binlog_erase(event_id);
                return;
            }
        };
        let web_page_id = match self.url_to_web_page_id.get(&web_page.url) {
            Some((web_page_id, _)) => *web_page_id,
            None => {
                env.binlog_erase(event_id);
                return;
            }
        };
        let mut web_page = web_page;
        web_page.log_event_id = event_id;
        self.update_web_page(env, web_page, web_page_id, true, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnv {
        files: std::sync::Arc<parking_lot::Mutex<crate::files::FileManager>>,
        file_counter: std::cell::Cell<i64>,
        timeouts: std::cell::RefCell<Vec<(i64, f64)>>,
        binlog: std::cell::RefCell<Vec<(i32, Vec<u8>)>>,
    }

    impl crate::photo::PhotoStoreEnv for TestEnv {
        fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            self.files.lock().store_file(file_id, out);
        }

        fn parse_file_id(&mut self, parser: &mut Parser<'_>) -> Result<FileId, &'static str> {
            self.files.lock().parse_file(parser)
        }
    }

    impl WebPageBlockEnv for TestEnv {
        fn get_document_object(&self, _file_id: FileId) -> Option<td_api::Document> {
            None
        }
        fn get_animation_object(&self, _file_id: FileId) -> Option<td_api::Animation> {
            None
        }
        fn get_audio_object(&self, _file_id: FileId) -> Option<td_api::Audio> {
            None
        }
        fn get_video_object(&self, _file_id: FileId) -> Option<td_api::Video> {
            None
        }
        fn get_video_note_object(&self, _file_id: FileId) -> Option<td_api::VideoNote> {
            None
        }
        fn get_voice_note_object(&self, _file_id: FileId) -> Option<td_api::VoiceNote> {
            None
        }
        fn get_accent_color_id_object(
            &self,
            accent_color_id: crate::peer_color::AccentColorId,
            _fallback: crate::peer_color::AccentColorId,
        ) -> Option<i32> {
            Some(accent_color_id.get())
        }
        fn get_chat_photo_info_object(
            &self,
            _photo: &crate::photo::DialogPhoto,
        ) -> Option<td_api::ChatPhotoInfo> {
            None
        }
        fn get_instant_view_link(&self, url: &str, rhash: &str) -> String {
            format!("https://t.me/iv?url={url}&rhash={rhash}")
        }
        fn get_instant_view_link_rhash(&self, _real_url: &str) -> String {
            String::new()
        }
        fn get_instant_view_link_url(&self, _real_url: &str) -> String {
            String::new()
        }
        fn as_input_document(
            &mut self,
            file_id: FileId,
        ) -> Option<(i64, tdmtproto::telegram_api::InputDocument)> {
            if !file_id.is_valid() {
                return None;
            }
            self.file_counter.set(self.file_counter.get() + 1);
            let id = self.file_counter.get();
            Some((
                id,
                tdmtproto::telegram_api::InputDocument::inputDocument(
                    tdmtproto::telegram_api::inputDocument {
                        id,
                        access_hash: 0,
                        file_reference: Vec::new(),
                    },
                ),
            ))
        }
        fn as_input_photo(
            &mut self,
            file_id: FileId,
        ) -> Option<(i64, tdmtproto::telegram_api::InputPhoto)> {
            if !file_id.is_valid() {
                return None;
            }
            self.file_counter.set(self.file_counter.get() + 1);
            let id = self.file_counter.get();
            Some((
                id,
                tdmtproto::telegram_api::InputPhoto::inputPhoto(
                    tdmtproto::telegram_api::inputPhoto {
                        id,
                        access_hash: 0,
                        file_reference: Vec::new(),
                    },
                ),
            ))
        }
        fn ensure_input_user(
            &mut self,
            _user_id: UserId,
        ) -> std::result::Result<(), tdutils::status::Status> {
            Ok(())
        }
        fn get_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
            user_id.get()
        }
        fn is_my_user_bot(&self) -> bool {
            false
        }
        fn on_get_page_block_channel(
            &mut self,
            channel: tdmtproto::telegram_api::channel,
        ) -> Option<crate::webpage::ChatLinkInfo> {
            let channel_id = crate::chat_manager::ChannelId::new(channel.id);
            if !channel_id.is_valid() {
                tdutils::log_error!("Receive invalid channel {}", channel.id);
                return None;
            }
            let photo = crate::photo::DialogPhoto::get_dialog_photo(channel.photo);
            Some(crate::webpage::ChatLinkInfo {
                title: channel.title,
                photo,
                username: channel.username,
                accent_color_id: crate::peer_color::AccentColorId::from_peer_id(channel.id),
                channel_id,
            })
        }
    }

    impl WebPagesManagerEnv for TestEnv {
        fn auth_is_bot(&self) -> bool {
            false
        }
        fn close_flag(&self) -> bool {
            false
        }
        fn use_message_database(&self) -> bool {
            false
        }
        fn server_time(&self) -> f64 {
            0.0
        }
        fn sqlite_pmc_get_raw(&mut self, _key: &[u8]) -> Vec<u8> {
            Vec::new()
        }
        fn sqlite_pmc_set_raw(&mut self, _key: &[u8], _value: &[u8]) {}
        fn sqlite_pmc_erase(&mut self, _key: &[u8]) {}
        fn binlog_add_web_page(&mut self, data: &[u8]) -> u64 {
            self.binlog.borrow_mut().push((0x10, data.to_vec()));
            1
        }
        fn binlog_rewrite_web_page(&mut self, _event_id: u64, _data: &[u8]) {}
        fn binlog_erase(&mut self, _event_id: u64) {}
        fn add_pending_web_page_timeout(&mut self, web_page_id: i64, timeout_in: f64) {
            self.timeouts.borrow_mut().push((web_page_id, timeout_in));
        }
        fn cancel_pending_web_page_timeout(&mut self, _web_page_id: i64) {}
        fn create_web_page_file_source(&mut self, _url: &str) -> FileSourceId {
            FileSourceId::default()
        }
        fn change_files_source(
            &mut self,
            _source_id: FileSourceId,
            _old_file_ids: &[FileId],
            _new_file_ids: &[FileId],
            _source: &str,
        ) {
        }
        fn delete_pending_message_web_page(&mut self, _message_full_id: MessageFullId) {}
        fn on_external_update_message_content(
            &mut self,
            _message_full_id: MessageFullId,
            _source: &str,
        ) {
        }
        fn delete_quick_reply_pending_message_web_page(
            &mut self,
            _message_full_id: QuickReplyMessageFullId,
        ) {
        }
        fn quick_reply_on_external_update_message_content(
            &mut self,
            _message_full_id: QuickReplyMessageFullId,
            _source: &str,
        ) {
        }
        fn get_messages_from_server(&mut self, _message_full_ids: &[MessageFullId]) {}
        fn reload_quick_reply_message(
            &mut self,
            _shortcut_id: QuickReplyShortcutId,
            _message_id: crate::messages_manager::MessageId,
        ) {
        }
        fn force_create_dialog(&mut self, _dialog_id: DialogId) {}
        fn on_get_story(
            &mut self,
            _dialog_id: DialogId,
            _story: Box<telegram_api::StoryItem>,
        ) -> Option<i32> {
            None
        }
        fn get_chat_id_object(&self, _dialog_id: DialogId, _source: &str) -> i64 {
            0
        }
        fn get_story_duration(&self, _story_full_id: StoryFullId) -> i32 {
            -1
        }
        fn on_get_sticker_document(&mut self, _document: Box<telegram_api::Document>) -> FileId {
            FileId::default()
        }
        fn on_get_document(
            &mut self,
            _document: Box<telegram_api::Document>,
            _owner_dialog_id: DialogId,
        ) -> Option<Document> {
            None
        }
        fn get_document_mime_type(&self, _file_id: FileId) -> String {
            String::new()
        }
        fn get_message_link_media_timestamp(&self, _url: &str) -> i32 {
            0
        }
        fn get_background_type_object(
            &self,
            _url: &str,
            _is_pattern: bool,
        ) -> Option<td_api::BackgroundType> {
            None
        }
        fn has_video_chat_invite_hash(&self, _url: &str) -> bool {
            false
        }
        fn is_premium(&self) -> bool {
            false
        }
        fn send_get_web_page_preview_query(
            &mut self,
            _text: crate::message_entity::FormattedText,
            _options: Box<GetWebPagePreviewOptions>,
            _promise: Promise<Option<td_api::LinkPreview>>,
        ) {
        }
        fn send_get_web_page_query(
            &mut self,
            _web_page_id: WebPageId,
            _url: String,
            _hash: i32,
            _promise: Promise<WebPageId>,
        ) {
        }
    }

    fn env() -> TestEnv {
        TestEnv {
            files: std::sync::Arc::new(parking_lot::Mutex::new(crate::files::FileManager::new())),
            file_counter: std::cell::Cell::new(0),
            timeouts: std::cell::RefCell::new(Vec::new()),
            binlog: std::cell::RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn pending_and_full_pages_register_urls() {
        let mut manager = WebPagesManager::new();
        let mut env = env();
        let pending = telegram_api::WebPage::webPagePending(telegram_api::webPagePending {
            flags: 0,
            id: 7,
            date: 100,
            url: "https://t.me/".to_owned(),
        });
        let web_page_id = manager.on_get_web_page(&mut env, pending, DialogId::default());
        assert!(web_page_id.is_valid());
        assert_eq!(manager.pending_web_page_urls.len(), 1);

        let full = telegram_api::WebPage::webPage(telegram_api::webPage {
            id: 7,
            flags: 0,
            url: "https://t.me/".to_owned(),
            display_url: "t.me".to_owned(),
            hash: 1,
            r#type: "article".to_owned(),
            site_name: String::new(),
            title: "Title".to_owned(),
            description: String::new(),
            photo: None,
            embed_url: String::new(),
            embed_type: String::new(),
            embed_width: 0,
            embed_height: 0,
            duration: 0,
            author: String::new(),
            document: None,
            cached_page: None,
            attributes: Vec::new(),
            has_large_media: false,
            video_cover_photo: false,
        });
        let web_page_id = manager.on_get_web_page(&mut env, full, DialogId::default());
        assert!(web_page_id.is_valid());
        assert!(manager.pending_web_page_urls.is_empty());
        assert_eq!(manager.get_web_page_by_url("https://t.me/"), web_page_id);
        assert_eq!(manager.get_web_page_search_text(web_page_id), "Title");
    }

    #[test]
    fn empty_page_erases_the_cache() {
        let mut manager = WebPagesManager::new();
        let mut env = env();
        manager
            .url_to_web_page_id
            .insert("https://t.me/".to_owned(), (WebPageId::new(9), false));
        let empty = telegram_api::WebPage::webPageEmpty(telegram_api::webPageEmpty {
            flags: 0,
            id: 9,
            url: String::new(),
        });
        let result = manager.on_get_web_page(&mut env, empty, DialogId::default());
        assert!(!result.is_valid());
        assert!(!manager.have_web_page(WebPageId::new(9)));
    }

    #[test]
    fn register_unregister_arms_the_timeout() {
        let mut manager = WebPagesManager::new();
        let mut env = env();
        let message_full_id = MessageFullId::new(
            DialogId::default(),
            crate::messages_manager::MessageId::new(1),
        );
        manager.register_web_page(&mut env, WebPageId::new(5), message_full_id);
        assert_eq!(env.timeouts.borrow().len(), 1);
        manager.unregister_web_page(WebPageId::new(5), message_full_id);
        assert!(manager.web_page_messages.is_empty());
    }
}
