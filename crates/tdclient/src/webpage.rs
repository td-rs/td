//! Port of `td/telegram/WebPageBlock.{h,cpp}` (M.17.1) plus
//! `WebPageId.h` and the `MessageContentDupType.h` shared enum — the
//! instant-view rich-text machinery: the 29 `RichText` node types, the 37
//! `WebPageBlock` types, the wire intake (`get_rich_text_from_telegram` /
//! `get_web_page_block`), the td_api intakes (`RichText::get_rich_text`
//! over `td_api::RichText` / `get_web_page_blocks` over `InputPageBlock`),
//! the td_api object builders (the two-pass anchor resolution included),
//! the clone plane and the byte-exact store/parse of every block.
//!
//! Manager-mediated FileId payloads (the C++
//! `DocumentsManager::store_document` / `AnimationsManager::store_animation`
//! / audio/video/voice-note/sticker framings) ride the
//! [`crate::photo::PhotoStoreEnv`] codec — the M.40 icon-plane
//! encoding, M.16's documented divergence; every non-FileId byte stays
//! C++-compatible. The InlineKeyboardButton/KeyboardButtonStyle/
//! RichButtonStyle/TargetDialogTypes value planes live in
//! [`crate::inline_keyboard_button`].
//!
//! The remaining `[blocked: …]` tails, recorded at each seam:
//! - `get_input_message_content` media conversion (the td_api
//!   InputPageBlock animation/audio/document/photo/video/voice-note
//!   arms) — the M.9/M.10 message-content send plane;
//! - `RichMessageMedia` (the `get_input_page_block` media arms) — M.9;
//! - the media object builders (documents/animations/audios/videos/
//!   voice-notes/stickers) and the ChatLink accent-color/photo-info
//!   builders — the M.15/M.28/M.5 manager seams;
//! - `LinkManager::get_instant_view_link` — M.23.

use crate::dialog_id::DialogId;
use crate::emoji_status::CustomEmojiId;
use crate::files::FileId;
use crate::formatted_date::FormattedDate;
use crate::inline_keyboard_button::{InlineKeyboardButton, InlineKeyboardButtonEnv};
use crate::message_content_dup_type::MessageContentDupType;
use crate::peer_color::AccentColorId;
use crate::photo::{
    DialogPhoto, Photo, PhotoStoreEnv, get_dimensions, parse_dimensions, parse_photo,
    store_dimensions, store_photo,
};
use crate::rich_button_style::RichButtonStyle;
use crate::store_parse::{Parser, store_flags, store_i32, store_i64, store_string, version};
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;
use tdutils::status::Status;

/// The `Td *td`/manager slice the WebPageBlock planes read through. The
/// media-object builders answer the collapsed `None` until their managers
/// land; the rest are fully local.
pub trait WebPageBlockEnv: PhotoStoreEnv {
    // -- the media object builders (M.15/M.28/M.5 manager seams)
    /// `documents_manager_->get_document_object(file_id, PhotoFormat::Jpeg)`.
    fn get_document_object(&self, file_id: FileId) -> Option<td_api::Document>;
    /// `animations_manager_->get_animation_object(file_id)`.
    fn get_animation_object(&self, file_id: FileId) -> Option<td_api::Animation>;
    /// `audios_manager_->get_audio_object(file_id)`.
    fn get_audio_object(&self, file_id: FileId) -> Option<td_api::Audio>;
    /// `videos_manager_->get_video_object(file_id)`.
    fn get_video_object(&self, file_id: FileId) -> Option<td_api::Video>;
    /// `video_notes_manager_->get_video_note_object(file_id)`.
    fn get_video_note_object(&self, file_id: FileId) -> Option<td_api::VideoNote>;
    /// `voice_notes_manager_->get_voice_note_object(file_id)`.
    fn get_voice_note_object(&self, file_id: FileId) -> Option<td_api::VoiceNote>;
    /// `theme_manager_->get_accent_color_id_object(accent_color_id, fallback)`.
    fn get_accent_color_id_object(
        &self,
        accent_color_id: AccentColorId,
        fallback: AccentColorId,
    ) -> Option<i32>;
    /// `get_chat_photo_info_object(file_manager, photo)` (DialogPhoto.h).
    fn get_chat_photo_info_object(&self, photo: &DialogPhoto) -> Option<td_api::ChatPhotoInfo>;
    // -- the LinkManager seam (M.23)
    /// `LinkManager::get_instant_view_link(url, rhash)`.
    fn get_instant_view_link(&self, url: &str, rhash: &str) -> String;
    /// `LinkManager::get_instant_view_link_rhash(real_url)` — the empty
    /// answer disables the instant-view rewrite, like the C++ empty rhash.
    fn get_instant_view_link_rhash(&self, real_url: &str) -> String;
    /// `LinkManager::get_instant_view_link_url(real_url)`.
    fn get_instant_view_link_url(&self, real_url: &str) -> String;
    // -- the FileManager main-remote codecs (the RichText::Icon and the
    // media input arms; the full RichMessageMedia framing is [blocked: M.9])
    /// `main_remote_location->as_input_document()` + `->get_id()`.
    fn as_input_document(&mut self, file_id: FileId) -> Option<(i64, telegram_api::InputDocument)>;
    /// `main_remote_location->as_input_photo()` + `->get_id()`.
    fn as_input_photo(&mut self, file_id: FileId) -> Option<(i64, telegram_api::InputPhoto)>;
    // -- the user seam (the UserManagerLike fold)
    /// `UserManager::get_input_user` validation half.
    fn ensure_input_user(&mut self, user_id: UserId) -> Result<(), Status>;
    /// `UserManager::get_user_id_object`.
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64;
    /// `UserManager::is_user_bot(get_my_id())` — the object-builder probe.
    fn is_my_user_bot(&self) -> bool;
    // -- the pageBlockChannel intake (chat_manager M.5)
    /// `chat_manager_->have_channel_force/on_get_chat/get_channel_*` +
    /// the min-channel `PeerColor` collapse; `None` drops the block like
    /// the C++ `nullptr` returns.
    fn on_get_page_block_channel(&mut self, channel: telegram_api::channel)
    -> Option<ChatLinkInfo>;
}

/// The WebPageBlockChatLink fields the `pageBlockChannel` intake builds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChatLinkInfo {
    pub title: String,
    pub photo: DialogPhoto,
    pub username: String,
    pub accent_color_id: AccentColorId,
    pub channel_id: crate::chat_manager::ChannelId,
}

/// The `GetInputPageBlockContext::media_` slot — the RichMessageMedia
/// vector the media arms consume (`[blocked: M.9]`); the offline line
/// carries the walk position only.
#[derive(Debug)]
pub struct RichMessageMediaSlot;

/// `GetWebPageBlockObjectContext` (WebPageBlock.cpp:69-79) — the
/// two-pass anchor state. The C++ `RichText *` pointer is only compared
/// against null, so the Rust map stores that null-ness.
pub struct WebPageBlockObjectContext<'a> {
    pub env: &'a mut dyn WebPageBlockEnv,
    pub base_url: String,
    pub real_url_host: String,
    pub real_url_rhash: String,
    pub skip_bot_commands: bool,
    pub is_first_pass: bool,
    pub has_anchor_urls: bool,
    /// anchor -> whether the anchor has text (the C++ `const RichText *`).
    pub anchors: std::collections::HashMap<String, bool>,
    /// The `UserManager *` of the object builders (the InlineButton type).
    pub user_manager: &'a mut dyn InlineKeyboardButtonEnv,
}

/// `GetInputPageBlockContext` (WebPageBlock.h:35-41).
pub struct GetInputPageBlockContext<'a> {
    pub env: &'a mut dyn WebPageBlockEnv,
    /// `InputContext::photos_`.
    pub photos: Vec<telegram_api::InputPhoto>,
    /// `InputContext::documents_`.
    pub documents: Vec<telegram_api::InputDocument>,
    /// `InputContext::media_` — `[blocked: M.9]`, the RichMessageMedia
    /// vector; the position walks one step per media block.
    pub media_pos: usize,
    pub user_manager: &'a mut dyn InlineKeyboardButtonEnv,
}

// ---------------------------------------------------------------------------
// WebPageId (WebPageId.h)
// ---------------------------------------------------------------------------

/// `td::WebPageId` (WebPageId.h:18).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WebPageId(i64);

impl WebPageId {
    pub fn new(web_page_id: i64) -> WebPageId {
        WebPageId(web_page_id)
    }

    /// `is_valid` (WebPageId.h:41-43).
    pub fn is_valid(self) -> bool {
        self.0 != 0
    }

    /// `get` (WebPageId.h:29-32).
    pub fn get(self) -> i64 {
        self.0
    }

    /// `store` (WebPageId.h:45-48).
    pub fn store(self, out: &mut Vec<u8>) {
        store_i64(out, self.0);
    }

    /// `parse` (WebPageId.h:50-53).
    pub fn parse(parser: &mut Parser<'_>) -> Result<WebPageId, &'static str> {
        Ok(WebPageId(parser.i64_()?))
    }
}

impl std::fmt::Display for WebPageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "link preview {}", self.0)
    }
}

/// The `CloneWebPageBlockContext` (WebPageBlock.h:43-48) — the clone
/// plane reads the dialog (the secret-chat check), the dup type and the
/// via-bot flag.
#[derive(Clone, Copy, Debug)]
pub struct CloneWebPageBlockContext {
    /// `dialog_id_.get_type() == DialogType::SecretChat`.
    pub dialog_id: DialogId,
    pub dup_type: MessageContentDupType,
    pub is_via_bot: bool,
}

impl CloneWebPageBlockContext {
    /// The C++ field defaults: `dup_type = Send`, `is_via_bot = false`.
    pub fn new(dialog_id: DialogId) -> Self {
        CloneWebPageBlockContext {
            dialog_id,
            dup_type: MessageContentDupType::Send,
            is_via_bot: false,
        }
    }
}

// ---------------------------------------------------------------------------
// RichText (WebPageBlock.cpp:88-803)
// ---------------------------------------------------------------------------

/// `RichText::Type` (WebPageBlock.cpp:116-147) — the stored i32 ordering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum RichTextType {
    #[default]
    Plain = 0,
    Bold,
    Italic,
    Underline,
    Strikethrough,
    Fixed,
    Url,
    EmailAddress,
    Concatenation,
    Subscript,
    Superscript,
    Marked,
    PhoneNumber,
    Icon,
    Anchor,
    Math,
    CustomEmoji,
    Spoiler,
    Mention,
    Hashtag,
    Cashtag,
    BotCommand,
    AutoUrl,
    AutoEmailAddress,
    AutoPhoneNumber,
    FormattedDate,
    BankCardNumber,
    MentionName,
    Diff,
    Button,
    TonAddress,
}

impl RichTextType {
    fn from_i32(value: i32) -> Option<RichTextType> {
        RichTextType::try_from(value).ok()
    }
}

impl TryFrom<i32> for RichTextType {
    type Error = ();

    fn try_from(value: i32) -> std::result::Result<Self, Self::Error> {
        match value {
            0 => Ok(RichTextType::Plain),
            1 => Ok(RichTextType::Bold),
            2 => Ok(RichTextType::Italic),
            3 => Ok(RichTextType::Underline),
            4 => Ok(RichTextType::Strikethrough),
            5 => Ok(RichTextType::Fixed),
            6 => Ok(RichTextType::Url),
            7 => Ok(RichTextType::EmailAddress),
            8 => Ok(RichTextType::Concatenation),
            9 => Ok(RichTextType::Subscript),
            10 => Ok(RichTextType::Superscript),
            11 => Ok(RichTextType::Marked),
            12 => Ok(RichTextType::PhoneNumber),
            13 => Ok(RichTextType::Icon),
            14 => Ok(RichTextType::Anchor),
            15 => Ok(RichTextType::Math),
            16 => Ok(RichTextType::CustomEmoji),
            17 => Ok(RichTextType::Spoiler),
            18 => Ok(RichTextType::Mention),
            19 => Ok(RichTextType::Hashtag),
            20 => Ok(RichTextType::Cashtag),
            21 => Ok(RichTextType::BotCommand),
            22 => Ok(RichTextType::AutoUrl),
            23 => Ok(RichTextType::AutoEmailAddress),
            24 => Ok(RichTextType::AutoPhoneNumber),
            25 => Ok(RichTextType::FormattedDate),
            26 => Ok(RichTextType::BankCardNumber),
            27 => Ok(RichTextType::MentionName),
            28 => Ok(RichTextType::Diff),
            29 => Ok(RichTextType::Button),
            30 => Ok(RichTextType::TonAddress),
            _ => Err(()),
        }
    }
}

/// `td::RichText` (WebPageBlock.cpp:108-157) — the field union mirrors
/// the C++ layout; which fields are meaningful depends on `r#type`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RichText {
    pub(crate) r#type: RichTextType,
    pub(crate) content: String,
    pub(crate) texts: Vec<RichText>,
    pub(crate) document_file_id: FileId,
    pub(crate) custom_emoji_id: CustomEmojiId,
    pub(crate) web_page_id: WebPageId,
    pub(crate) date: FormattedDate,
    pub(crate) user_id: UserId,
    pub(crate) button_style: RichButtonStyle,
    pub(crate) button: Option<InlineKeyboardButton>,
}

impl RichText {
    /// `empty` (WebPageBlock.cpp:415-417).
    pub fn empty(&self) -> bool {
        self.r#type == RichTextType::Plain && self.content.is_empty()
    }

    /// `RichText::create_plain` (RichText.cpp:540-547).
    pub fn create_plain(text: String) -> RichText {
        RichText {
            r#type: RichTextType::Plain,
            content: text,
            ..RichText::default()
        }
    }

    /// `RichText::create_math` (RichText.cpp:549-554) — the math text
    /// builder the quote plane uses for pageBlockMath sources (upstream
    /// df92bc788).
    pub fn create_math(source: String) -> RichText {
        RichText {
            r#type: RichTextType::Math,
            content: source,
            ..RichText::default()
        }
    }

    /// `append_file_ids` (WebPageBlock.cpp:419-428) — the General
    /// document's thumbnail plane arrives with the Document record
    /// (M.28); the file id itself appends locally.
    pub fn append_file_ids(&self, file_ids: &mut Vec<FileId>) {
        if self.r#type == RichTextType::Icon {
            if self.document_file_id.is_valid() {
                file_ids.push(self.document_file_id);
            }
        } else {
            for text in &self.texts {
                text.append_file_ids(file_ids);
            }
        }
    }

    /// `RichText::add_dependencies` (RichText.cpp:570-580) — the
    /// recursive walk the quote plane feeds the rich texts through
    /// (upstream 4e1635ac8). The C++ `dependencies.add(web_page_id_)`
    /// slot (the WebPagesManager dep resolve) has no Dependencies set in
    /// the port yet; the user/button ids walk exactly.
    pub fn add_dependencies(
        &self,
        dependencies: &mut dyn crate::inline_keyboard_button::DependenciesSink,
    ) {
        for text in &self.texts {
            text.add_dependencies(dependencies);
        }
        dependencies.add_user(self.user_id);
        if let Some(button) = &self.button {
            button.add_dependencies(dependencies);
        }
    }

    /// `for_each_rich_text` (WebPageBlock.cpp:441-448).
    pub fn for_each_rich_text<'a>(
        &'a self,
        recurse_text: bool,
        callback: &mut impl FnMut(&'a RichText),
    ) {
        if recurse_text {
            for text in &self.texts {
                text.for_each_rich_text(recurse_text, callback);
            }
        }
        callback(self);
    }

    /// `get_full_text` (WebPageBlock.cpp:450-459).
    pub fn get_full_text(&self) -> String {
        let mut result = String::new();
        self.for_each_rich_text(true, &mut |text| {
            if text.r#type == RichTextType::Plain
                || text.r#type == RichTextType::Math
                || text.r#type == RichTextType::CustomEmoji
            {
                result.push_str(&text.content);
            }
        });
        result
    }

    /// `get_quote_text` (RichText.cpp:602-621) — the text of the rich text
    /// as used in quotes (upstream df92bc788): Plain/Math/CustomEmoji keep
    /// the content, Icon is unquotable, Diff quotes only the new text.
    pub fn get_quote_text(&self) -> String {
        match self.r#type {
            RichTextType::Plain | RichTextType::Math | RichTextType::CustomEmoji => {
                self.content.clone()
            }
            RichTextType::Icon => String::new(),
            RichTextType::Diff => {
                // only the new text can be quoted
                assert!(!self.texts.is_empty(), "the C++ CHECKs the text count");
                self.texts[0].get_quote_text()
            }
            _ => {
                let mut result = String::new();
                for text in &self.texts {
                    result.push_str(&text.get_quote_text());
                }
                result
            }
        }
    }

    /// `get_quote_fragment` (RichText.cpp:624-627) — the part of the rich
    /// text between the `begin` and `end` UTF-8 characters of its quote
    /// text, shaped for a quote (upstream 23c7272d7).
    pub fn get_quote_fragment(&self, begin: usize, end: usize) -> RichText {
        let mut pos = 0usize;
        self.get_quote_fragment_impl(&mut pos, begin, end)
    }

    /// `get_quote_fragment_impl` (RichText.cpp:629-711) — the recursive
    /// walker sharing the quote-text position across the tree; the
    /// formatting-is-dropped case list is the simplified post-1e7c9a1a8
    /// fold (MentionName/Button/TonAddress ride the Diff arm).
    fn get_quote_fragment_impl(&self, pos: &mut usize, begin: usize, end: usize) -> RichText {
        match self.r#type {
            RichTextType::Plain => {
                let size = tdutils::utf8::utf8_length(&self.content);
                let from = begin.max(*pos);
                let to = end.min(*pos + size);
                let mut result = RichText::default();
                if from < to {
                    result.content =
                        tdutils::utf8::utf8_substr_range(&self.content, from - *pos, to - from)
                            .to_owned();
                }
                *pos += size;
                result
            }
            RichTextType::Math | RichTextType::CustomEmoji => {
                let size = tdutils::utf8::utf8_length(&self.content);
                let is_included = begin <= *pos && *pos + size <= end;
                *pos += size;
                if !is_included {
                    return RichText::default();
                }
                let mut context = CloneWebPageBlockContext::new(DialogId::default());
                self.clone_rich_text(&mut context, false)
            }
            RichTextType::Icon => {
                // icons can't be quoted
                RichText::default()
            }
            RichTextType::Concatenation => {
                let mut texts = Vec::new();
                for text in &self.texts {
                    let fragment = text.get_quote_fragment_impl(pos, begin, end);
                    if !fragment.empty() {
                        texts.push(fragment);
                    }
                }
                if texts.is_empty() {
                    return RichText::default();
                }
                RichText {
                    r#type: RichTextType::Concatenation,
                    texts,
                    ..RichText::default()
                }
            }
            RichTextType::Fixed
            | RichTextType::Url
            | RichTextType::EmailAddress
            | RichTextType::Marked
            | RichTextType::PhoneNumber
            | RichTextType::Anchor
            | RichTextType::Mention
            | RichTextType::Hashtag
            | RichTextType::Cashtag
            | RichTextType::BotCommand
            | RichTextType::AutoUrl
            | RichTextType::AutoEmailAddress
            | RichTextType::AutoPhoneNumber
            | RichTextType::BankCardNumber
            | RichTextType::MentionName
            | RichTextType::Button
            | RichTextType::TonAddress
            | RichTextType::Diff => {
                // the formatting isn't supported in quotes, so only the text is quoted
                assert!(!self.texts.is_empty(), "the C++ CHECKs the text count");
                self.texts[0].get_quote_fragment_impl(pos, begin, end)
            }
            _ => {
                if self.texts.len() != 1 {
                    tdutils::log_error!(
                        "Receive rich text of type {} with {} texts",
                        self.r#type as i32,
                        self.texts.len()
                    );
                    for text in &self.texts {
                        *pos += tdutils::utf8::utf8_length(&text.get_quote_text());
                    }
                    return RichText::default();
                }
                let fragment = self.texts[0].get_quote_fragment_impl(pos, begin, end);
                if fragment.empty() {
                    return RichText::default();
                }
                let mut context = CloneWebPageBlockContext::new(DialogId::default());
                let mut result = self.clone_rich_text(&mut context, true);
                result.texts.push(fragment);
                result
            }
        }
    }

    /// `remove_unallowed_quote_entities` (RichText.cpp:716-783) — drops the
    /// formatting that can't be kept in a quote, answering whether the text
    /// changed (upstream fd3863de4).
    pub fn remove_unallowed_quote_entities(&mut self) -> bool {
        match self.r#type {
            RichTextType::Plain | RichTextType::Math | RichTextType::CustomEmoji => false,
            RichTextType::Icon => {
                // icons can't be quoted
                *self = RichText::default();
                true
            }
            RichTextType::Concatenation => {
                let mut is_changed = false;
                for text in &mut self.texts {
                    if text.remove_unallowed_quote_entities() {
                        is_changed = true;
                    }
                }
                let before = self.texts.len();
                self.texts.retain(|text| !text.empty());
                if self.texts.len() != before {
                    is_changed = true;
                }
                if self.texts.is_empty() {
                    *self = RichText::default();
                    return true;
                }
                is_changed
            }
            RichTextType::Fixed
            | RichTextType::Url
            | RichTextType::EmailAddress
            | RichTextType::Marked
            | RichTextType::PhoneNumber
            | RichTextType::Anchor
            | RichTextType::Mention
            | RichTextType::Hashtag
            | RichTextType::Cashtag
            | RichTextType::BotCommand
            | RichTextType::AutoUrl
            | RichTextType::AutoEmailAddress
            | RichTextType::AutoPhoneNumber
            | RichTextType::BankCardNumber
            | RichTextType::MentionName
            | RichTextType::Button
            | RichTextType::TonAddress
            | RichTextType::Diff => {
                // the formatting isn't supported in quotes, so only the text is kept
                assert!(!self.texts.is_empty(), "the C++ CHECKs the text count");
                let mut text = std::mem::take(&mut self.texts[0]);
                text.remove_unallowed_quote_entities();
                *self = text;
                true
            }
            _ => {
                if self.texts.len() != 1 {
                    tdutils::log_error!(
                        "Receive rich text of type {} with {} texts",
                        self.r#type as i32,
                        self.texts.len()
                    );
                    *self = RichText::default();
                    return true;
                }
                if !self.texts[0].remove_unallowed_quote_entities() {
                    return false;
                }
                if self.texts[0].empty() {
                    *self = RichText::default();
                }
                true
            }
        }
    }

    /// `get_index_mask` (WebPageBlock.cpp:461-475).
    pub fn get_index_mask(&self) -> i32 {
        match self.r#type {
            RichTextType::Url => {
                if self.content.starts_with('#') {
                    return 0;
                }
                crate::dialog_id::message_search_filter_index_mask(
                    crate::dialog_id::MessageSearchFilter::Url,
                )
            }
            RichTextType::AutoUrl | RichTextType::EmailAddress | RichTextType::AutoEmailAddress => {
                crate::dialog_id::message_search_filter_index_mask(
                    crate::dialog_id::MessageSearchFilter::Url,
                )
            }
            _ => 0,
        }
    }

    /// `RichText::clone` (RichText.cpp:812-839) — `skip_texts` clones the
    /// node without the children (the get_quote_fragment default arm
    /// re-fills them with the fragment).
    fn clone_rich_text(
        &self,
        context: &mut CloneWebPageBlockContext,
        skip_texts: bool,
    ) -> RichText {
        let mut result = RichText {
            r#type: self.r#type,
            content: self.content.clone(),
            texts: if skip_texts {
                Vec::new()
            } else {
                self.texts
                    .iter()
                    .map(|t| t.clone_rich_text(context, false))
                    .collect()
            },
            document_file_id: self.document_file_id,
            custom_emoji_id: self.custom_emoji_id,
            web_page_id: self.web_page_id,
            date: self.date,
            user_id: self.user_id,
            button_style: self.button_style,
            button: None,
        };
        if let Some(button) = &self.button {
            let is_forward = context.dup_type == MessageContentDupType::Forward;
            if is_forward && !button.get_forward_text().is_empty() {
                assert!(result.texts.len() == 1, "the C++ CHECKs the text count");
                result.texts[0] = RichText {
                    r#type: RichTextType::Plain,
                    content: button.get_forward_text().to_owned(),
                    ..RichText::default()
                };
            }
            result.button = Some(button.clone_button(
                context.dialog_id,
                context.dup_type,
                context.is_via_bot,
                // is_rich_message: the RichText clone is always a rich message
                true,
            ));
        }
        result
    }

    /// `RichText::get_input_rich_text` (WebPageBlock.cpp:503-597).
    fn get_input_rich_text(
        &self,
        context: &mut GetInputPageBlockContext,
    ) -> telegram_api::RichText {
        match self.r#type {
            RichTextType::Plain => {
                if self.content.is_empty() {
                    telegram_api::RichText::textEmpty(telegram_api::textEmpty {})
                } else {
                    telegram_api::RichText::textPlain(telegram_api::textPlain {
                        text: self.content.clone(),
                    })
                }
            }
            RichTextType::Bold => telegram_api::RichText::textBold(telegram_api::textBold {
                text: Some(Box::new(self.texts[0].get_input_rich_text(context))),
            }),
            RichTextType::Italic => telegram_api::RichText::textItalic(telegram_api::textItalic {
                text: Some(Box::new(self.texts[0].get_input_rich_text(context))),
            }),
            RichTextType::Underline => {
                telegram_api::RichText::textUnderline(telegram_api::textUnderline {
                    text: Some(Box::new(self.texts[0].get_input_rich_text(context))),
                })
            }
            RichTextType::Strikethrough => {
                telegram_api::RichText::textStrike(telegram_api::textStrike {
                    text: Some(Box::new(self.texts[0].get_input_rich_text(context))),
                })
            }
            RichTextType::Fixed => telegram_api::RichText::textFixed(telegram_api::textFixed {
                text: Some(Box::new(self.texts[0].get_input_rich_text(context))),
            }),
            RichTextType::Url => telegram_api::RichText::textUrl(telegram_api::textUrl {
                text: Some(Box::new(self.texts[0].get_input_rich_text(context))),
                url: self.content.clone(),
                webpage_id: 0,
            }),
            RichTextType::EmailAddress => {
                telegram_api::RichText::textEmail(telegram_api::textEmail {
                    text: Some(Box::new(self.texts[0].get_input_rich_text(context))),
                    email: self.content.clone(),
                })
            }
            RichTextType::Concatenation => {
                telegram_api::RichText::textConcat(telegram_api::textConcat {
                    texts: self
                        .texts
                        .iter()
                        .map(|t| Box::new(t.get_input_rich_text(context)))
                        .collect(),
                })
            }
            RichTextType::Subscript => {
                telegram_api::RichText::textSubscript(telegram_api::textSubscript {
                    text: Some(Box::new(self.texts[0].get_input_rich_text(context))),
                })
            }
            RichTextType::Superscript => {
                telegram_api::RichText::textSuperscript(telegram_api::textSuperscript {
                    text: Some(Box::new(self.texts[0].get_input_rich_text(context))),
                })
            }
            RichTextType::Marked => telegram_api::RichText::textMarked(telegram_api::textMarked {
                text: Some(Box::new(self.texts[0].get_input_rich_text(context))),
            }),
            RichTextType::PhoneNumber => {
                telegram_api::RichText::textPhone(telegram_api::textPhone {
                    text: Some(Box::new(self.texts[0].get_input_rich_text(context))),
                    phone: self.content.clone(),
                })
            }
            RichTextType::Icon => {
                let dimensions: u32 = self.content.parse().unwrap_or(0);
                let width = (dimensions / 65536) as i32;
                let height = (dimensions % 65536) as i32;
                if let Some((document_id, input_document)) =
                    context.env.as_input_document(self.document_file_id)
                {
                    context.documents.push(input_document);
                    return telegram_api::RichText::textImage(telegram_api::textImage {
                        document_id,
                        w: width,
                        h: height,
                    });
                }
                tdutils::log_error!(
                    "Can't create textImage for icon {}",
                    self.document_file_id.get()
                );
                telegram_api::RichText::textEmpty(telegram_api::textEmpty {})
            }
            RichTextType::Anchor => telegram_api::RichText::textAnchor(telegram_api::textAnchor {
                text: Some(Box::new(self.texts[0].get_input_rich_text(context))),
                name: self.content.clone(),
            }),
            RichTextType::Math => telegram_api::RichText::textMath(telegram_api::textMath {
                source: self.content.clone(),
            }),
            RichTextType::CustomEmoji => {
                telegram_api::RichText::textCustomEmoji(telegram_api::textCustomEmoji {
                    document_id: self.custom_emoji_id.get(),
                    alt: self.content.clone(),
                })
            }
            RichTextType::Spoiler => {
                telegram_api::RichText::textSpoiler(telegram_api::textSpoiler {
                    text: Some(Box::new(self.texts[0].get_input_rich_text(context))),
                })
            }
            // the auto-detected entities and the mention family re-emit
            // the inner text, exactly like the C++
            RichTextType::Mention
            | RichTextType::Hashtag
            | RichTextType::Cashtag
            | RichTextType::BotCommand
            | RichTextType::AutoUrl
            | RichTextType::AutoEmailAddress
            | RichTextType::AutoPhoneNumber
            | RichTextType::BankCardNumber
            | RichTextType::TonAddress => self.texts[0].get_input_rich_text(context),
            RichTextType::FormattedDate => telegram_api::RichText::textDate(
                self.date
                    .get_input_text_date(Box::new(self.texts[0].get_input_rich_text(context))),
            ),
            RichTextType::MentionName => {
                // input users will be added separately
                telegram_api::RichText::textMentionName(telegram_api::textMentionName {
                    text: Some(Box::new(self.texts[0].get_input_rich_text(context))),
                    user_id: self.user_id.get(),
                })
            }
            RichTextType::Diff => {
                assert!(self.texts.len() == 2, "the C++ CHECKs the text count");
                telegram_api::RichText::textDiff(telegram_api::textDiff {
                    text: Some(Box::new(self.texts[0].get_input_rich_text(context))),
                    old_text: Some(Box::new(self.texts[1].get_input_rich_text(context))),
                })
            }
            RichTextType::Button => {
                let mut flags = 0;
                let style = self.button_style.get_input_rich_button_style();
                if style.is_some() {
                    // textButton::STYLE_MASK
                    flags |= 1 << 0;
                }
                let button = self.button.as_ref().expect("the C++ CHECKs the button");
                let input_button = button.get_input_keyboard_inline_button(context.user_manager);
                telegram_api::RichText::textButton(telegram_api::textButton {
                    flags,
                    text: Some(Box::new(self.texts[0].get_input_rich_text(context))),
                    r#type: input_button.r#type,
                    style,
                })
            }
        }
    }

    /// `RichText::get_rich_text_object` (WebPageBlock.cpp:599-737);
    /// `allow_null` collapses the empty plain text to `None`.
    fn get_rich_text_object(
        &self,
        context: &mut WebPageBlockObjectContext,
        allow_null: bool,
    ) -> Option<Box<td_api::RichText>> {
        let object: td_api::RichText = match self.r#type {
            RichTextType::Plain => {
                if allow_null && self.content.is_empty() {
                    return None;
                }
                td_api::RichText::richTextPlain(td_api::richTextPlain {
                    text: self.content.clone(),
                })
            }
            RichTextType::Bold => td_api::RichText::richTextBold(td_api::richTextBold {
                text: self.texts[0].get_rich_text_object(context, false),
            }),
            RichTextType::Italic => td_api::RichText::richTextItalic(td_api::richTextItalic {
                text: self.texts[0].get_rich_text_object(context, false),
            }),
            RichTextType::Underline => {
                td_api::RichText::richTextUnderline(td_api::richTextUnderline {
                    text: self.texts[0].get_rich_text_object(context, false),
                })
            }
            RichTextType::Strikethrough => {
                td_api::RichText::richTextStrikethrough(td_api::richTextStrikethrough {
                    text: self.texts[0].get_rich_text_object(context, false),
                })
            }
            RichTextType::Fixed => td_api::RichText::richTextFixed(td_api::richTextFixed {
                text: self.texts[0].get_rich_text_object(context, false),
            }),
            RichTextType::Url => {
                if context.base_url.len() <= self.content.len()
                    && self.content.starts_with(context.base_url.as_str())
                    && self.content.as_bytes()[context.base_url.len()] == b'#'
                {
                    if context.is_first_pass {
                        context.has_anchor_urls = true;
                    } else {
                        let mut anchor = self.content[context.base_url.len() + 1..].to_owned();
                        // https://html.spec.whatwg.org/multipage/browsing-the-web.html#the-indicated-part-of-the-document
                        for i in 0..2 {
                            if i == 1 {
                                // try to url_decode anchor
                                anchor = tdutils::http_url::url_decode(&anchor, false);
                            }
                            if let Some(&has_text) = context.anchors.get(anchor.as_str()) {
                                let text = self.texts[0].get_rich_text_object(context, false);
                                let content = self.content.clone();
                                return Some(Box::new(if has_text {
                                    td_api::RichText::richTextReferenceLink(
                                        td_api::richTextReferenceLink {
                                            text,
                                            reference_name: anchor,
                                            url: content,
                                        },
                                    )
                                } else {
                                    td_api::RichText::richTextAnchorLink(
                                        td_api::richTextAnchorLink {
                                            text,
                                            anchor_name: anchor,
                                            url: content,
                                        },
                                    )
                                }));
                            }
                        }
                    }
                }
                if !context.real_url_rhash.is_empty()
                    && tdutils::http_url::get_url_host(&self.content) == context.real_url_host
                {
                    if context.is_first_pass {
                        context.has_anchor_urls = true;
                    } else {
                        return Some(Box::new(td_api::RichText::richTextUrl(
                            td_api::richTextUrl {
                                text: self.texts[0].get_rich_text_object(context, false),
                                url: context
                                    .env
                                    .get_instant_view_link(&self.content, &context.real_url_rhash),
                                is_cached: true,
                            },
                        )));
                    }
                }
                td_api::RichText::richTextUrl(td_api::richTextUrl {
                    text: self.texts[0].get_rich_text_object(context, false),
                    url: self.content.clone(),
                    is_cached: self.web_page_id.is_valid(),
                })
            }
            RichTextType::EmailAddress => {
                td_api::RichText::richTextEmailAddress(td_api::richTextEmailAddress {
                    text: self.texts[0].get_rich_text_object(context, false),
                    email_address: self.content.clone(),
                })
            }
            RichTextType::Concatenation => td_api::RichText::richTexts(td_api::richTexts {
                texts: self
                    .texts
                    .iter()
                    .filter_map(|t| t.get_rich_text_object(context, false))
                    .collect(),
            }),
            RichTextType::Subscript => {
                td_api::RichText::richTextSubscript(td_api::richTextSubscript {
                    text: self.texts[0].get_rich_text_object(context, false),
                })
            }
            RichTextType::Superscript => {
                td_api::RichText::richTextSuperscript(td_api::richTextSuperscript {
                    text: self.texts[0].get_rich_text_object(context, false),
                })
            }
            RichTextType::Marked => td_api::RichText::richTextMarked(td_api::richTextMarked {
                text: self.texts[0].get_rich_text_object(context, false),
            }),
            RichTextType::PhoneNumber => {
                td_api::RichText::richTextPhoneNumber(td_api::richTextPhoneNumber {
                    text: self.texts[0].get_rich_text_object(context, false),
                    phone_number: self.content.clone(),
                })
            }
            RichTextType::Icon => {
                let dimensions: u32 = self.content.parse().unwrap_or(0);
                let width = (dimensions / 65536) as i32;
                let height = (dimensions % 65536) as i32;
                td_api::RichText::richTextIcon(td_api::richTextIcon {
                    document: context.env.get_document_object(self.document_file_id),
                    width,
                    height,
                })
            }
            RichTextType::Anchor => {
                if context.is_first_pass {
                    context
                        .anchors
                        .entry(self.content.clone())
                        .or_insert(!self.texts[0].empty());
                }
                if self.texts[0].empty() {
                    td_api::RichText::richTextAnchor(td_api::richTextAnchor {
                        name: self.content.clone(),
                    })
                } else {
                    td_api::RichText::richTextReference(td_api::richTextReference {
                        name: self.content.clone(),
                        text: self.texts[0].get_rich_text_object(context, false),
                    })
                }
            }
            RichTextType::Math => td_api::RichText::richTextMathematicalExpression(
                td_api::richTextMathematicalExpression {
                    expression: self.content.clone(),
                },
            ),
            RichTextType::CustomEmoji => {
                td_api::RichText::richTextCustomEmoji(td_api::richTextCustomEmoji {
                    custom_emoji_id: self.custom_emoji_id.get(),
                    alternative_text: self.content.clone(),
                })
            }
            RichTextType::Spoiler => td_api::RichText::richTextSpoiler(td_api::richTextSpoiler {
                text: self.texts[0].get_rich_text_object(context, false),
            }),
            RichTextType::Mention => td_api::RichText::richTextMention(td_api::richTextMention {
                text: self.texts[0].get_rich_text_object(context, false),
                username: trim_first(&self.texts[0].get_full_text(), '@'),
            }),
            RichTextType::Hashtag => td_api::RichText::richTextHashtag(td_api::richTextHashtag {
                text: self.texts[0].get_rich_text_object(context, false),
                hashtag: trim_first(&self.texts[0].get_full_text(), '#'),
            }),
            RichTextType::Cashtag => td_api::RichText::richTextCashtag(td_api::richTextCashtag {
                text: self.texts[0].get_rich_text_object(context, false),
                cashtag: trim_first(&self.texts[0].get_full_text(), '$'),
            }),
            RichTextType::BotCommand => {
                if context.skip_bot_commands {
                    return self.texts[0].get_rich_text_object(context, false);
                }
                td_api::RichText::richTextBotCommand(td_api::richTextBotCommand {
                    text: self.texts[0].get_rich_text_object(context, false),
                    bot_command: trim_first(&self.texts[0].get_full_text(), '/'),
                })
            }
            RichTextType::AutoUrl => td_api::RichText::richTextUrl(td_api::richTextUrl {
                text: self.texts[0].get_rich_text_object(context, false),
                url: self.texts[0].get_full_text(),
                is_cached: false,
            }),
            RichTextType::AutoEmailAddress => {
                td_api::RichText::richTextEmailAddress(td_api::richTextEmailAddress {
                    text: self.texts[0].get_rich_text_object(context, false),
                    email_address: self.texts[0].get_full_text(),
                })
            }
            RichTextType::AutoPhoneNumber => {
                td_api::RichText::richTextPhoneNumber(td_api::richTextPhoneNumber {
                    text: self.texts[0].get_rich_text_object(context, false),
                    phone_number: self.texts[0].get_full_text(),
                })
            }
            RichTextType::FormattedDate => {
                td_api::RichText::richTextDateTime(td_api::richTextDateTime {
                    text: self.texts[0].get_rich_text_object(context, false),
                    unix_time: self.date.get_date(),
                    formatting_type: self.date.get_date_time_formatting_type_object(),
                })
            }
            RichTextType::BankCardNumber => {
                td_api::RichText::richTextBankCardNumber(td_api::richTextBankCardNumber {
                    text: self.texts[0].get_rich_text_object(context, false),
                    bank_card_number: self.texts[0].get_full_text(),
                })
            }
            RichTextType::TonAddress => {
                td_api::RichText::richTextTonAddress(td_api::richTextTonAddress {
                    text: self.texts[0].get_rich_text_object(context, false),
                    address: self.texts[0].get_full_text(),
                })
            }
            RichTextType::MentionName => {
                td_api::RichText::richTextMentionName(td_api::richTextMentionName {
                    text: self.texts[0].get_rich_text_object(context, false),
                    user_id: context
                        .env
                        .get_user_id_object(self.user_id, "richTextMentionName"),
                })
            }
            RichTextType::Diff => {
                assert!(self.texts.len() == 2, "the C++ CHECKs the text count");
                td_api::RichText::richTextDiff(td_api::richTextDiff {
                    text: self.texts[0].get_rich_text_object(context, false),
                    old_text: self.texts[1].get_rich_text_object(context, false),
                })
            }
            RichTextType::Button => {
                let button = self.button.as_ref().expect("the C++ CHECKs the button");
                let button_object =
                    button.get_inline_keyboard_button_object(Some(&*context.user_manager));
                td_api::RichText::richTextButton(td_api::richTextButton {
                    button: Some(Box::new(td_api::InlineButton {
                        text: self.texts[0].get_rich_text_object(context, false),
                        style: self.button_style.get_button_style_object(),
                        r#type: button_object.r#type,
                    })),
                })
            }
        };
        Some(Box::new(object))
    }

    /// `RichText::store` (WebPageBlock.cpp:746-771).
    pub fn store(&self, env: &mut impl WebPageBlockEnv, out: &mut Vec<u8>) {
        store_i32(out, self.r#type as i32);
        store_string(out, &self.content);
        store_i32(out, self.texts.len() as i32);
        for text in &self.texts {
            text.store(env, out);
        }
        if self.r#type == RichTextType::Icon {
            // the documents_manager_->store_document framing
            env.store_file_id(self.document_file_id, out);
        }
        if self.r#type == RichTextType::Url {
            self.web_page_id.store(out);
        }
        if self.r#type == RichTextType::CustomEmoji {
            store_i64(out, self.custom_emoji_id.get());
        }
        if self.r#type == RichTextType::FormattedDate {
            self.date.store(out);
        }
        if self.r#type == RichTextType::MentionName {
            store_i64(out, self.user_id.get());
        }
        if self.r#type == RichTextType::Button {
            self.button_style.store(out);
            self.button
                .as_ref()
                .expect("the C++ stores the button")
                .store(out);
        }
    }

    /// `RichText::parse` (WebPageBlock.cpp:773-802).
    pub fn parse(
        env: &mut impl WebPageBlockEnv,
        parser: &mut Parser<'_>,
    ) -> Result<RichText, &'static str> {
        let mut result = RichText::default();
        let type_ = parser.i32_()?;
        result.r#type = RichTextType::from_i32(type_).ok_or("Can't parse unknown RichText type")?;
        result.content = parser.string()?;
        let texts_len = parser.i32_()?;
        if texts_len < 0 {
            return Err("Invalid vector size");
        }
        for _ in 0..texts_len {
            result.texts.push(RichText::parse(env, parser)?);
        }
        if result.r#type == RichTextType::Icon {
            result.document_file_id = env.parse_file_id(parser)?;
            if !result.document_file_id.is_valid() {
                tdutils::log_error!("Failed to load document from database");
                return Ok(RichText::default());
            }
        }
        if result.r#type == RichTextType::Url
            && parser.version() >= version::SUPPORT_INSTANT_VIEW_2_0
        {
            result.web_page_id = WebPageId::parse(parser)?;
        }
        if result.r#type == RichTextType::CustomEmoji {
            result.custom_emoji_id = CustomEmojiId::new(parser.i64_()?);
        }
        if result.r#type == RichTextType::FormattedDate {
            result.date = FormattedDate::parse(parser)?;
        }
        if result.r#type == RichTextType::MentionName {
            result.user_id = UserId::new(parser.i64_()?);
        }
        if result.r#type == RichTextType::Button {
            result.button_style = RichButtonStyle::parse(parser)?;
            result.button = Some(InlineKeyboardButton::parse(parser)?);
        }
        Ok(result)
    }
}

/// `RichText::trim_first` (WebPageBlock.cpp:100-106).
fn trim_first(s: &str, c: char) -> String {
    if let Some(rest) = s.strip_prefix(c) {
        return rest.to_owned();
    }
    tdutils::log_error!("Expected string starting with '{c}', but received \"{s}\"");
    s.to_owned()
}

// ---------------------------------------------------------------------------
// The wire RichText intake (WebPageBlock.cpp:4646-4875)
// ---------------------------------------------------------------------------

/// The `FlatHashMap<int64, FileId>` document map of the wire intake.
pub type DocumentsById = std::collections::HashMap<i64, FileId>;
/// The `FlatHashMap<int64, unique_ptr<Photo>>` photo map of the intake.
pub type PhotosById = std::collections::HashMap<i64, Photo>;

/// `get_rich_texts` (WebPageBlock.cpp:4870-4875).
#[allow(clippy::vec_box)] // the generated telegram_api vector fields box their elements
fn get_rich_texts_from_telegram(
    rich_text_ptrs: Vec<Box<telegram_api::RichText>>,
    documents: &DocumentsById,
) -> Vec<RichText> {
    rich_text_ptrs
        .into_iter()
        .map(|rich_text| get_rich_text_from_telegram(*rich_text, documents))
        .collect()
}

/// `get_rich_text(telegram_api::RichText)` (WebPageBlock.cpp:4649-4868).
pub fn get_rich_text_from_telegram(
    rich_text_ptr: telegram_api::RichText,
    documents: &DocumentsById,
) -> RichText {
    let mut result = RichText::default();
    match rich_text_ptr {
        telegram_api::RichText::textEmpty(_) => {}
        telegram_api::RichText::textPlain(rich_text) => {
            result.content = rich_text.text;
        }
        telegram_api::RichText::textBold(rich_text) => {
            result.r#type = RichTextType::Bold;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textItalic(rich_text) => {
            result.r#type = RichTextType::Italic;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textUnderline(rich_text) => {
            result.r#type = RichTextType::Underline;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textStrike(rich_text) => {
            result.r#type = RichTextType::Strikethrough;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textFixed(rich_text) => {
            result.r#type = RichTextType::Fixed;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textUrl(rich_text) => {
            result.r#type = RichTextType::Url;
            result.content = rich_text.url;
            push_wire_text(&mut result.texts, rich_text.text, documents);
            result.web_page_id = WebPageId::new(rich_text.webpage_id);
        }
        telegram_api::RichText::textEmail(rich_text) => {
            result.r#type = RichTextType::EmailAddress;
            result.content = rich_text.email;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textConcat(rich_text) => {
            result.r#type = RichTextType::Concatenation;
            result.texts = get_rich_texts_from_telegram(rich_text.texts, documents);
        }
        telegram_api::RichText::textSubscript(rich_text) => {
            result.r#type = RichTextType::Subscript;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textSuperscript(rich_text) => {
            result.r#type = RichTextType::Superscript;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textMarked(rich_text) => {
            result.r#type = RichTextType::Marked;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textPhone(rich_text) => {
            result.r#type = RichTextType::PhoneNumber;
            result.content = rich_text.phone;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textImage(rich_text) => {
            if let Some(&document_file_id) = documents.get(&rich_text.document_id) {
                result.r#type = RichTextType::Icon;
                result.document_file_id = document_file_id;
                let dimensions = get_dimensions(rich_text.w, rich_text.h, "textImage");
                result.content =
                    (dimensions.width as u32 * 65536 + dimensions.height as u32).to_string();
            } else {
                tdutils::log_error!("Can't find document {}", rich_text.document_id);
            }
        }
        telegram_api::RichText::textAnchor(rich_text) => {
            result.r#type = RichTextType::Anchor;
            result.content = rich_text.name;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textMath(rich_text) => {
            result.r#type = RichTextType::Math;
            result.content = rich_text.source;
        }
        telegram_api::RichText::textCustomEmoji(rich_text) => {
            result.r#type = RichTextType::CustomEmoji;
            result.custom_emoji_id = CustomEmojiId::new(rich_text.document_id);
            result.content = rich_text.alt;
        }
        telegram_api::RichText::textSpoiler(rich_text) => {
            result.r#type = RichTextType::Spoiler;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textMention(rich_text) => {
            result.r#type = RichTextType::Mention;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textHashtag(rich_text) => {
            result.r#type = RichTextType::Hashtag;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textBotCommand(rich_text) => {
            result.r#type = RichTextType::BotCommand;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textCashtag(rich_text) => {
            result.r#type = RichTextType::Cashtag;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textAutoUrl(rich_text) => {
            result.r#type = RichTextType::AutoUrl;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textAutoEmail(rich_text) => {
            result.r#type = RichTextType::AutoEmailAddress;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textAutoPhone(rich_text) => {
            result.r#type = RichTextType::AutoPhoneNumber;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textBankCard(rich_text) => {
            result.r#type = RichTextType::BankCardNumber;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textTonAddress(rich_text) => {
            // WebPageBlock.cpp:381-385 — its own type since richTextTonAddress
            // (43c09497a), no longer folded into BankCardNumber.
            result.r#type = RichTextType::TonAddress;
            push_wire_text(&mut result.texts, rich_text.text, documents);
        }
        telegram_api::RichText::textMentionName(rich_text) => {
            result.r#type = RichTextType::MentionName;
            push_wire_text(&mut result.texts, rich_text.text, documents);
            result.user_id = UserId::new(rich_text.user_id);
            if !result.user_id.is_valid() {
                tdutils::log_error!("Receive invalid user {}", result.user_id.get());
                result.user_id = UserId::default();
            }
        }
        telegram_api::RichText::textDate(rich_text) => {
            let date = FormattedDate::from_flags(
                rich_text.date,
                rich_text.relative,
                rich_text.short_time,
                rich_text.long_time,
                rich_text.short_date,
                rich_text.long_date,
                rich_text.day_of_week,
            );
            if !date.is_valid() {
                return result;
            }
            result.r#type = RichTextType::FormattedDate;
            push_wire_text(&mut result.texts, rich_text.text, documents);
            result.date = date;
        }
        telegram_api::RichText::textDiff(rich_text) => {
            result.r#type = RichTextType::Diff;
            push_wire_text(&mut result.texts, rich_text.text, documents);
            push_wire_text(&mut result.texts, rich_text.old_text, documents);
        }
        telegram_api::RichText::textButton(rich_text) => {
            result.r#type = RichTextType::Button;
            push_wire_text(&mut result.texts, rich_text.text, documents);
            result.button_style = RichButtonStyle::from_telegram(rich_text.style);
            let keyboard_button = telegram_api::KeyboardInlineButton {
                flags: 0,
                style: None,
                text: "1".to_owned(),
                r#type: rich_text.r#type,
            };
            result.button = Some(InlineKeyboardButton::from_telegram(keyboard_button));
        }
    }
    result
}

fn push_wire_text(
    texts: &mut Vec<RichText>,
    text: Option<Box<telegram_api::RichText>>,
    documents: &DocumentsById,
) {
    if let Some(text) = text {
        texts.push(get_rich_text_from_telegram(*text, documents));
    }
}

/// `get_page_block_caption` (WebPageBlock.cpp:4877-4884).
fn get_page_block_caption_from_telegram(
    page_caption: telegram_api::PageCaption,
    documents: &DocumentsById,
) -> WebPageBlockCaption {
    let mut text = RichText::default();
    let mut credit = RichText::default();
    if let Some(t) = page_caption.text {
        text = get_rich_text_from_telegram(*t, documents);
    }
    if let Some(c) = page_caption.credit {
        credit = get_rich_text_from_telegram(*c, documents);
    }
    WebPageBlockCaption { text, credit }
}

// ---------------------------------------------------------------------------
// The td_api RichText intake — `RichText::get_rich_text(const Td *,
// td_api::RichText)` (WebPageBlock.cpp:159-413)
// ---------------------------------------------------------------------------

/// `RichText::get_rich_text` — the td_api input conversion; the
/// `ensure_input_user` half of the MentionName check is the env seam.
pub fn get_rich_text_from_td_api(
    env: &mut dyn WebPageBlockEnv,
    rich_text: Option<Box<td_api::RichText>>,
) -> std::result::Result<RichText, Status> {
    let mut result = RichText::default();
    let Some(rich_text) = rich_text else {
        return Ok(result);
    };
    match *rich_text {
        td_api::RichText::richTextPlain(text) => {
            let Some(content) = crate::misc::clean_input_string(&text.text) else {
                return Err(Status::error(400, "Rich text must be encoded in UTF-8"));
            };
            result.r#type = RichTextType::Plain;
            result.content = content;
        }
        td_api::RichText::richTextBold(text) => {
            result.r#type = RichTextType::Bold;
            result
                .texts
                .push(get_rich_text_from_td_api(env, text.text)?);
        }
        td_api::RichText::richTextItalic(text) => {
            result.r#type = RichTextType::Italic;
            result
                .texts
                .push(get_rich_text_from_td_api(env, text.text)?);
        }
        td_api::RichText::richTextUnderline(text) => {
            result.r#type = RichTextType::Underline;
            result
                .texts
                .push(get_rich_text_from_td_api(env, text.text)?);
        }
        td_api::RichText::richTextStrikethrough(text) => {
            result.r#type = RichTextType::Strikethrough;
            result
                .texts
                .push(get_rich_text_from_td_api(env, text.text)?);
        }
        td_api::RichText::richTextSpoiler(text) => {
            result.r#type = RichTextType::Spoiler;
            result
                .texts
                .push(get_rich_text_from_td_api(env, text.text)?);
        }
        td_api::RichText::richTextSubscript(text) => {
            result.r#type = RichTextType::Subscript;
            result
                .texts
                .push(get_rich_text_from_td_api(env, text.text)?);
        }
        td_api::RichText::richTextSuperscript(text) => {
            result.r#type = RichTextType::Superscript;
            result
                .texts
                .push(get_rich_text_from_td_api(env, text.text)?);
        }
        td_api::RichText::richTextMarked(text) => {
            result.r#type = RichTextType::Marked;
            result
                .texts
                .push(get_rich_text_from_td_api(env, text.text)?);
        }
        td_api::RichText::richTextDateTime(text) => {
            let t = get_rich_text_from_td_api(env, text.text)?;
            let formatted_date =
                FormattedDate::get_formatted_date(text.unix_time, text.formatting_type.as_ref())?;
            result.r#type = RichTextType::FormattedDate;
            result.texts.push(t);
            result.date = formatted_date;
        }
        td_api::RichText::richTextMention(text) => {
            return get_rich_text_from_td_api(env, text.text);
        }
        td_api::RichText::richTextHashtag(text) => {
            return get_rich_text_from_td_api(env, text.text);
        }
        td_api::RichText::richTextCashtag(text) => {
            return get_rich_text_from_td_api(env, text.text);
        }
        td_api::RichText::richTextBankCardNumber(text) => {
            return get_rich_text_from_td_api(env, text.text);
        }
        td_api::RichText::richTextTonAddress(text) => {
            return get_rich_text_from_td_api(env, text.text);
        }
        td_api::RichText::richTextBotCommand(text) => {
            return get_rich_text_from_td_api(env, text.text);
        }
        td_api::RichText::richTextFixed(text) => {
            result.r#type = RichTextType::Fixed;
            result
                .texts
                .push(get_rich_text_from_td_api(env, text.text)?);
        }
        td_api::RichText::richTextMentionName(text) => {
            let t = get_rich_text_from_td_api(env, text.text)?;
            let user_id = UserId::new(text.user_id);
            env.ensure_input_user(user_id)?;
            result.r#type = RichTextType::MentionName;
            result.texts.push(t);
            result.user_id = user_id;
        }
        td_api::RichText::richTextUrl(text) => {
            let t = get_rich_text_from_td_api(env, text.text)?;
            let Some(url) = crate::misc::clean_input_string(&text.url) else {
                return Err(Status::error(400, "Rich text URL must be encoded in UTF-8"));
            };
            result.r#type = RichTextType::Url;
            result.content = url;
            result.texts.push(t);
        }
        td_api::RichText::richTextEmailAddress(text) => {
            let t = get_rich_text_from_td_api(env, text.text)?;
            let Some(email_address) = crate::misc::clean_input_string(&text.email_address) else {
                return Err(Status::error(
                    400,
                    "Rich text email address must be encoded in UTF-8",
                ));
            };
            result.r#type = RichTextType::EmailAddress;
            result.content = email_address;
            result.texts.push(t);
        }
        td_api::RichText::richTextPhoneNumber(text) => {
            let t = get_rich_text_from_td_api(env, text.text)?;
            let Some(phone_number) = crate::misc::clean_input_string(&text.phone_number) else {
                return Err(Status::error(
                    400,
                    "Rich text phone number must be encoded in UTF-8",
                ));
            };
            result.r#type = RichTextType::PhoneNumber;
            result.content = phone_number;
            result.texts.push(t);
        }
        td_api::RichText::richTextCustomEmoji(text) => {
            let Some(alternative_text) = crate::misc::clean_input_string(&text.alternative_text)
            else {
                return Err(Status::error(
                    400,
                    "Custom emoji alternative text must be encoded in UTF-8",
                ));
            };
            let custom_emoji_id = CustomEmojiId::new(text.custom_emoji_id);
            if !custom_emoji_id.is_valid() {
                return Err(Status::error(
                    400,
                    "Invalid custom emoji identifier specified",
                ));
            }
            result.r#type = RichTextType::CustomEmoji;
            result.content = alternative_text;
            result.custom_emoji_id = custom_emoji_id;
        }
        td_api::RichText::richTextIcon(_) => {
            return Err(Status::error(400, "Block is not allowed"));
        }
        td_api::RichText::richTextMathematicalExpression(text) => {
            let Some(expression) = crate::misc::clean_input_string(&text.expression) else {
                return Err(Status::error(
                    400,
                    "Mathematical expression must be encoded in UTF-8",
                ));
            };
            result.r#type = RichTextType::Math;
            result.content = expression;
        }
        td_api::RichText::richTextButton(text) => {
            let Some(button) = text.button else {
                return Err(Status::error(400, "Button must be non-empty"));
            };
            let t = get_rich_text_from_td_api(env, button.text.clone())?;
            let inline_button = InlineKeyboardButton::from_td_api(
                td_api::InlineKeyboardButton {
                    text: "1".to_owned(),
                    icon_custom_emoji_id: 0,
                    style: None,
                    r#type: button.r#type.clone(),
                },
                true,
                false,
            )?;
            result.r#type = RichTextType::Button;
            result.texts.push(t);
            result.button_style = RichButtonStyle::from_td_api(button.style);
            result.button = Some(inline_button);
        }
        td_api::RichText::richTextReference(text) => {
            let Some(name) = crate::misc::clean_input_string(&text.name) else {
                return Err(Status::error(
                    400,
                    "Reference name must be encoded in UTF-8",
                ));
            };
            let t = get_rich_text_from_td_api(env, text.text)?;
            result.r#type = RichTextType::Anchor;
            result.content = name;
            result.texts.push(t);
        }
        td_api::RichText::richTextReferenceLink(text) => {
            let Some(reference_name) = crate::misc::clean_input_string(&text.reference_name) else {
                return Err(Status::error(
                    400,
                    "Reference name must be encoded in UTF-8",
                ));
            };
            let t = get_rich_text_from_td_api(env, text.text)?;
            result.r#type = RichTextType::Url;
            result.content = format!("#{}", tdutils::http_url::url_encode(&reference_name));
            result.texts.push(t);
        }
        td_api::RichText::richTextAnchor(text) => {
            let Some(name) = crate::misc::clean_input_string(&text.name) else {
                return Err(Status::error(400, "Anchor name must be encoded in UTF-8"));
            };
            result.r#type = RichTextType::Anchor;
            result.content = name;
            result.texts.push(RichText::default());
        }
        td_api::RichText::richTextAnchorLink(text) => {
            let Some(anchor_name) = crate::misc::clean_input_string(&text.anchor_name) else {
                return Err(Status::error(400, "Anchor name must be encoded in UTF-8"));
            };
            let t = get_rich_text_from_td_api(env, text.text)?;
            result.r#type = RichTextType::Url;
            result.content = format!("#{}", tdutils::http_url::url_encode(&anchor_name));
            result.texts.push(t);
        }
        td_api::RichText::richTexts(text) => {
            if text.texts.is_empty() {
                // the empty concatenation stays the default Plain
            } else {
                let mut texts = Vec::with_capacity(text.texts.len());
                for subtext in text.texts {
                    texts.push(get_rich_text_from_td_api(env, Some(subtext))?);
                }
                result.r#type = RichTextType::Concatenation;
                result.texts = texts;
            }
        }
        td_api::RichText::richTextDiff(_) => {
            return Err(Status::error(400, "Block is not allowed"));
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// WebPageBlockCaption / WebPageBlockTableCell / RelatedArticle
// (WebPageBlock.cpp:807-1198)
// ---------------------------------------------------------------------------

/// `WebPageBlockCaption` (WebPageBlock.cpp:807).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebPageBlockCaption {
    pub(crate) text: RichText,
    pub(crate) credit: RichText,
}

impl WebPageBlockCaption {
    /// `get_web_page_block_caption` (WebPageBlock.cpp:812-820).
    pub fn get_web_page_block_caption(
        env: &mut dyn WebPageBlockEnv,
        caption: Option<td_api::PageBlockCaption>,
    ) -> std::result::Result<WebPageBlockCaption, Status> {
        let mut result = WebPageBlockCaption::default();
        if let Some(caption) = caption {
            result.text = get_rich_text_from_td_api(env, caption.text)?;
            result.credit = get_rich_text_from_td_api(env, caption.credit)?;
        }
        Ok(result)
    }

    /// `append_file_ids` (WebPageBlock.cpp:822-825).
    pub fn append_file_ids(&self, file_ids: &mut Vec<FileId>) {
        self.text.append_file_ids(file_ids);
        self.credit.append_file_ids(file_ids);
    }

    /// `for_each_rich_text` (WebPageBlock.cpp:832-835).
    pub fn for_each_rich_text<'a>(
        &'a self,
        recurse_text: bool,
        callback: &mut impl FnMut(&'a RichText),
    ) {
        self.text.for_each_rich_text(recurse_text, callback);
        self.credit.for_each_rich_text(recurse_text, callback);
    }

    /// `get_index_mask` (WebPageBlock.cpp:837-839).
    pub fn get_index_mask(&self) -> i32 {
        self.text.get_index_mask() | self.credit.get_index_mask()
    }

    /// `clone` (WebPageBlock.cpp:841-846).
    fn clone_caption(&self, context: &mut CloneWebPageBlockContext) -> WebPageBlockCaption {
        WebPageBlockCaption {
            text: self.text.clone_rich_text(context, false),
            credit: self.credit.clone_rich_text(context, false),
        }
    }

    /// `get_input_page_caption` (WebPageBlock.cpp:848-851).
    fn get_input_page_caption(
        &self,
        context: &mut GetInputPageBlockContext,
    ) -> telegram_api::PageCaption {
        telegram_api::PageCaption {
            text: Some(Box::new(self.text.get_input_rich_text(context))),
            credit: Some(Box::new(self.credit.get_input_rich_text(context))),
        }
    }

    /// `get_page_block_caption_object` (WebPageBlock.cpp:853-860).
    fn get_page_block_caption_object(
        &self,
        context: &mut WebPageBlockObjectContext,
    ) -> Option<td_api::PageBlockCaption> {
        if self.text.empty() && self.credit.empty() {
            return None;
        }
        Some(td_api::PageBlockCaption {
            text: self.text.get_rich_text_object(context, false),
            credit: self.credit.get_rich_text_object(context, true),
        })
    }

    /// `store` (WebPageBlock.cpp:866-871).
    fn store(&self, env: &mut impl WebPageBlockEnv, out: &mut Vec<u8>) {
        self.text.store(env, out);
        self.credit.store(env, out);
    }

    /// `parse` (WebPageBlock.cpp:873-882).
    fn parse(
        env: &mut impl WebPageBlockEnv,
        parser: &mut Parser<'_>,
    ) -> Result<Self, &'static str> {
        let text = RichText::parse(env, parser)?;
        let credit = if parser.version() >= version::SUPPORT_INSTANT_VIEW_2_0 {
            RichText::parse(env, parser)?
        } else {
            RichText::default()
        };
        Ok(WebPageBlockCaption { text, credit })
    }
}

/// `WebPageBlockTableCell` (WebPageBlock.cpp:885).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebPageBlockTableCell {
    pub(crate) text: RichText,
    pub(crate) is_header: bool,
    pub(crate) align_left: bool,
    pub(crate) align_center: bool,
    pub(crate) align_right: bool,
    pub(crate) valign_top: bool,
    pub(crate) valign_middle: bool,
    pub(crate) valign_bottom: bool,
    pub(crate) colspan: i32,
    pub(crate) rowspan: i32,
}

impl WebPageBlockTableCell {
    /// `clone` (WebPageBlock.cpp:898-911).
    fn clone_cell(&self, context: &mut CloneWebPageBlockContext) -> WebPageBlockTableCell {
        WebPageBlockTableCell {
            text: self.text.clone_rich_text(context, false),
            ..self.clone()
        }
    }

    /// `get_web_page_block_table_cell` (WebPageBlock.cpp:913-964).
    pub fn get_web_page_block_table_cell(
        env: &mut dyn WebPageBlockEnv,
        cell: Option<td_api::PageBlockTableCell>,
    ) -> std::result::Result<WebPageBlockTableCell, Status> {
        let mut result = WebPageBlockTableCell {
            colspan: 1,
            rowspan: 1,
            ..WebPageBlockTableCell::default()
        };
        let Some(cell) = cell else {
            return Err(Status::error(400, "Table cell must be non-empty"));
        };
        if cell.colspan < 0 {
            return Err(Status::error(400, "Invalid table cell colspan specified"));
        }
        if cell.rowspan < 0 {
            return Err(Status::error(400, "Invalid table cell rowspan specified"));
        }
        result.text = match cell.text {
            Some(text) => get_rich_text_from_td_api(env, Some(text))?,
            None => RichText::default(),
        };
        result.is_header = cell.is_header;
        result.colspan = cell.colspan.max(1);
        result.rowspan = cell.rowspan.max(1);
        match cell.align.as_ref() {
            None => result.align_left = true,
            Some(td_api::PageBlockHorizontalAlignment::pageBlockHorizontalAlignmentLeft(_)) => {
                result.align_left = true;
            }
            Some(td_api::PageBlockHorizontalAlignment::pageBlockHorizontalAlignmentCenter(_)) => {
                result.align_center = true;
            }
            Some(td_api::PageBlockHorizontalAlignment::pageBlockHorizontalAlignmentRight(_)) => {
                result.align_right = true;
            }
        }
        match cell.valign.as_ref() {
            None => result.valign_middle = true,
            Some(td_api::PageBlockVerticalAlignment::pageBlockVerticalAlignmentTop(_)) => {
                result.valign_top = true;
            }
            Some(td_api::PageBlockVerticalAlignment::pageBlockVerticalAlignmentMiddle(_)) => {
                result.valign_middle = true;
            }
            Some(td_api::PageBlockVerticalAlignment::pageBlockVerticalAlignmentBottom(_)) => {
                result.valign_bottom = true;
            }
        }
        Ok(result)
    }

    /// `get_input_page_table_cell` (WebPageBlock.cpp:982-997).
    fn get_input_page_table_cell(
        &self,
        context: &mut GetInputPageBlockContext,
    ) -> telegram_api::PageTableCell {
        let mut flags = 0;
        if self.colspan != 1 {
            // pageTableCell::COLSPAN_MASK
            flags |= 1 << 2;
        }
        if self.rowspan != 1 {
            // pageTableCell::ROWSPAN_MASK
            flags |= 1 << 3;
        }
        if !self.text.empty() {
            // pageTableCell::TEXT_MASK
            flags |= 1 << 4;
        }
        telegram_api::PageTableCell {
            flags,
            header: self.is_header,
            align_center: self.align_center,
            align_right: self.align_right,
            valign_middle: self.valign_middle,
            valign_bottom: self.valign_bottom,
            text: if self.text.empty() {
                None
            } else {
                Some(Box::new(self.text.get_input_rich_text(context)))
            },
            colspan: self.colspan,
            rowspan: self.rowspan,
        }
    }

    /// `get_page_block_table_cell_object` (WebPageBlock.cpp:999-1030).
    fn get_page_block_table_cell_object(
        &self,
        context: &mut WebPageBlockObjectContext,
    ) -> td_api::PageBlockTableCell {
        let align = if self.align_left {
            td_api::PageBlockHorizontalAlignment::pageBlockHorizontalAlignmentLeft(
                td_api::pageBlockHorizontalAlignmentLeft {},
            )
        } else if self.align_center {
            td_api::PageBlockHorizontalAlignment::pageBlockHorizontalAlignmentCenter(
                td_api::pageBlockHorizontalAlignmentCenter {},
            )
        } else {
            td_api::PageBlockHorizontalAlignment::pageBlockHorizontalAlignmentRight(
                td_api::pageBlockHorizontalAlignmentRight {},
            )
        };
        let valign = if self.valign_top {
            td_api::PageBlockVerticalAlignment::pageBlockVerticalAlignmentTop(
                td_api::pageBlockVerticalAlignmentTop {},
            )
        } else if self.valign_middle {
            td_api::PageBlockVerticalAlignment::pageBlockVerticalAlignmentMiddle(
                td_api::pageBlockVerticalAlignmentMiddle {},
            )
        } else {
            td_api::PageBlockVerticalAlignment::pageBlockVerticalAlignmentBottom(
                td_api::pageBlockVerticalAlignmentBottom {},
            )
        };
        td_api::PageBlockTableCell {
            text: if self.text.empty() {
                None
            } else {
                self.text.get_rich_text_object(context, false)
            },
            is_header: self.is_header,
            colspan: self.colspan,
            rowspan: self.rowspan,
            align: Some(align),
            valign: Some(valign),
        }
    }

    /// `store` (WebPageBlock.cpp:1039-1066).
    fn store(&self, env: &mut impl WebPageBlockEnv, out: &mut Vec<u8>) {
        let has_text = !self.text.empty();
        let has_colspan = self.colspan != 1;
        let has_rowspan = self.rowspan != 1;
        store_flags(
            out,
            [
                self.is_header,
                self.align_left,
                self.align_center,
                self.align_right,
                self.valign_top,
                self.valign_middle,
                self.valign_bottom,
                has_text,
                has_colspan,
                has_rowspan,
            ],
        );
        if has_text {
            self.text.store(env, out);
        }
        if has_colspan {
            store_i32(out, self.colspan);
        }
        if has_rowspan {
            store_i32(out, self.rowspan);
        }
    }

    /// `parse` (WebPageBlock.cpp:1068-1095).
    fn parse(
        env: &mut impl WebPageBlockEnv,
        parser: &mut Parser<'_>,
    ) -> Result<Self, &'static str> {
        let mut result = WebPageBlockTableCell {
            colspan: 1,
            rowspan: 1,
            ..WebPageBlockTableCell::default()
        };
        let [
            is_header,
            align_left,
            align_center,
            align_right,
            valign_top,
            valign_middle,
            valign_bottom,
            has_text,
            has_colspan,
            has_rowspan,
        ] = parser.parse_flags()?;
        result.is_header = is_header;
        result.align_left = align_left;
        result.align_center = align_center;
        result.align_right = align_right;
        result.valign_top = valign_top;
        result.valign_middle = valign_middle;
        result.valign_bottom = valign_bottom;
        if has_text {
            result.text = RichText::parse(env, parser)?;
        }
        if has_colspan {
            result.colspan = parser.i32_()?;
        }
        if has_rowspan {
            result.rowspan = parser.i32_()?;
        }
        Ok(result)
    }
}

/// `RelatedArticle` (WebPageBlock.cpp:1098).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RelatedArticle {
    pub(crate) url: String,
    pub(crate) web_page_id: WebPageId,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) photo: Photo,
    pub(crate) author: String,
    pub(crate) published_date: i32,
}

impl RelatedArticle {
    /// `get_page_block_related_article_object` (WebPageBlock.cpp:1119-1123).
    fn get_page_block_related_article_object(&self) -> td_api::PageBlockRelatedArticle {
        td_api::PageBlockRelatedArticle {
            url: self.url.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            photo: crate::photo::get_photo_object(&self.photo),
            author: self.author.clone(),
            publish_date: self.published_date,
        }
    }

    /// `store` (WebPageBlock.cpp:1131-1163).
    fn store(&self, env: &mut impl WebPageBlockEnv, out: &mut Vec<u8>) {
        let has_title = !self.title.is_empty();
        let has_description = !self.description.is_empty();
        let has_photo = !self.photo.is_empty();
        let has_author = !self.author.is_empty();
        let has_date = self.published_date != 0;
        store_flags(
            out,
            [has_title, has_description, has_photo, has_author, has_date],
        );
        store_string(out, &self.url);
        self.web_page_id.store(out);
        if has_title {
            store_string(out, &self.title);
        }
        if has_description {
            store_string(out, &self.description);
        }
        if has_photo {
            store_photo(env, out, &self.photo);
        }
        if has_author {
            store_string(out, &self.author);
        }
        if has_date {
            store_i32(out, self.published_date);
        }
    }

    /// `parse` (WebPageBlock.cpp:1165-1197).
    fn parse(
        env: &mut impl WebPageBlockEnv,
        parser: &mut Parser<'_>,
    ) -> Result<Self, &'static str> {
        let [has_title, has_description, has_photo, has_author, has_date] = parser.parse_flags()?;
        let url = parser.string()?;
        let web_page_id = WebPageId::parse(parser)?;
        let title = if has_title {
            parser.string()?
        } else {
            String::new()
        };
        let description = if has_description {
            parser.string()?
        } else {
            String::new()
        };
        let photo = if has_photo {
            parse_photo(env, parser)?
        } else {
            crate::photo::Photo::default()
        };
        let author = if has_author {
            parser.string()?
        } else {
            String::new()
        };
        let published_date = if has_date { parser.i32_()? } else { 0 };
        Ok(RelatedArticle {
            url,
            web_page_id,
            title,
            description,
            photo,
            author,
            published_date,
        })
    }
}

// ---------------------------------------------------------------------------
// WebPageBlockList / WebPageBlockButtonRow / WebPageBlockTable
// (WebPageBlock.cpp:2167-2489, 2491-2687, 3964-4083)
// ---------------------------------------------------------------------------

/// `get_ordered_list_type` (WebPageBlock.cpp:2089-2094).
fn get_ordered_list_type(type_: &str) -> String {
    if type_.len() != 1 || !"aAiI1".contains(type_.as_bytes()[0] as char) {
        return "1".to_owned();
    }
    type_.to_owned()
}

/// `get_ordered_list_label` (WebPageBlock.cpp:2096-2165).
fn get_ordered_list_label(mut num: i32, type_: &str) -> String {
    if (type_ == "a" || type_ == "A") && num > 0 {
        let mut result = String::new();
        while num > 0 {
            num -= 1;
            result.insert(0, (b'A' + (num % 26) as u8) as char);
            num /= 26;
        }
        if type_ == "a" {
            result = result.to_lowercase();
        }
        result.push('.');
        return result;
    }
    if (type_ == "i" || type_ == "I") && num > 0 && num < 4000 {
        let mut result = String::new();
        while num >= 1000 {
            num -= 1000;
            result.push('M');
        }
        if num >= 900 {
            result.push_str("CM");
            num -= 900;
        } else if num >= 500 {
            result.push('D');
            num -= 500;
        } else if num >= 400 {
            result.push_str("CD");
            num -= 400;
        }
        while num >= 100 {
            result.push('C');
            num -= 100;
        }
        if num >= 90 {
            result.push_str("XC");
            num -= 90;
        } else if num >= 50 {
            result.push('L');
            num -= 50;
        } else if num >= 40 {
            result.push_str("XL");
            num -= 40;
        }
        while num >= 10 {
            result.push('X');
            num -= 10;
        }
        if num >= 9 {
            result.push_str("IX");
            num -= 9;
        } else if num >= 5 {
            result.push('V');
            num -= 5;
        } else if num >= 4 {
            result.push_str("IV");
            num -= 4;
        }
        while num >= 1 {
            result.push('I');
            num -= 1;
        }
        if type_ == "i" {
            result = result.to_lowercase();
        }
        result.push('.');
        return result;
    }
    format!("{num}.")
}

/// `WebPageBlockList::Item` (WebPageBlock.cpp:2169).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebPageBlockListItem {
    pub(crate) label: String,
    pub(crate) page_blocks: Vec<WebPageBlock>,
    pub(crate) has_checkbox: bool,
    pub(crate) is_checked: bool,
    pub(crate) value: i32,
    pub(crate) r#type: String,
}

impl WebPageBlockListItem {
    /// `Item::get_item` (WebPageBlock.cpp:2177-2195) — the media-free
    /// input conversion; the blocks recurse into the td_api intake.
    pub fn get_item(
        env: &mut dyn WebPageBlockEnv,
        dialog_id: DialogId,
        item: Option<td_api::InputPageBlockListItem>,
    ) -> std::result::Result<WebPageBlockListItem, Status> {
        let mut result = WebPageBlockListItem {
            value: -1,
            ..WebPageBlockListItem::default()
        };
        let Some(item) = item else {
            return Err(Status::error(400, "List item must be non-empty"));
        };
        result.page_blocks = get_web_page_blocks_from_td_api(env, dialog_id, item.blocks)?;
        result.has_checkbox = item.has_checkbox;
        result.is_checked = item.is_checked;
        if !item.r#type.is_empty() {
            if item.r#type.len() != 1 || !"aAiI1".contains(item.r#type.as_bytes()[0] as char) {
                return Err(Status::error(400, "Invalid list item type specified"));
            }
            result.value = item.value;
            result.r#type = item.r#type;
            result.label = get_ordered_list_label(result.value, &result.r#type);
        }
        Ok(result)
    }

    fn get_index_mask(&self) -> i32 {
        get_web_page_blocks_index_mask(&self.page_blocks)
    }

    fn get_page_block_list_item_object(
        &self,
        context: &mut WebPageBlockObjectContext,
    ) -> td_api::PageBlockListItem {
        if self.label.is_empty() {
            // if label is empty, then Bullet U+2022 is used as a label
            return td_api::PageBlockListItem {
                label: "\u{2022}".to_owned(),
                blocks: get_page_blocks_object_vec(&self.page_blocks, context),
                has_checkbox: self.has_checkbox,
                is_checked: self.has_checkbox && self.is_checked,
                value: 0,
                r#type: String::new(),
            };
        }
        td_api::PageBlockListItem {
            label: self.label.clone(),
            blocks: get_page_blocks_object_vec(&self.page_blocks, context),
            has_checkbox: self.has_checkbox,
            is_checked: self.has_checkbox && self.is_checked,
            value: self.value,
            r#type: self.r#type.clone(),
        }
    }

    /// `Item::store` (WebPageBlock.cpp:2287-2306).
    fn store(&self, env: &mut impl WebPageBlockEnv, out: &mut Vec<u8>) {
        let has_value = self.value != -1;
        let has_type = !self.r#type.is_empty();
        store_flags(
            out,
            [self.has_checkbox, self.is_checked, has_value, has_type],
        );
        store_string(out, &self.label);
        store_page_blocks(env, &self.page_blocks, out);
        if has_value {
            store_i32(out, self.value);
        }
        if has_type {
            store_string(out, &self.r#type);
        }
    }

    /// `Item::parse` (WebPageBlock.cpp:2308-2329) — the flags are
    /// `SupportRichMessages`-gated.
    fn parse(
        env: &mut impl WebPageBlockEnv,
        parser: &mut Parser<'_>,
    ) -> Result<Self, &'static str> {
        let mut result = WebPageBlockListItem {
            value: -1,
            ..WebPageBlockListItem::default()
        };
        let [has_checkbox, is_checked, has_value, has_type] =
            if parser.version() >= version::SUPPORT_RICH_MESSAGES {
                parser.parse_flags()?
            } else {
                [false, false, false, false]
            };
        result.has_checkbox = has_checkbox;
        result.is_checked = is_checked;
        result.label = parser.string()?;
        result.page_blocks = parse_page_blocks(env, parser)?;
        if has_value {
            result.value = parser.i32_()?;
        }
        if has_type {
            result.r#type = parser.string()?;
        }
        Ok(result)
    }
}

/// `WebPageBlockList` (WebPageBlock.cpp:2167).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebPageBlockList {
    pub(crate) items: Vec<WebPageBlockListItem>,
    pub(crate) start: i32,
    pub(crate) is_reversed: bool,
    pub(crate) r#type: String,
}

impl WebPageBlockList {
    /// `clone` (WebPageBlock.cpp:2398-2401).
    fn clone_list(&self, context: &mut CloneWebPageBlockContext) -> WebPageBlockList {
        WebPageBlockList {
            items: self
                .items
                .iter()
                .map(|item| WebPageBlockListItem {
                    label: item.label.clone(),
                    page_blocks: clone_web_page_blocks(&item.page_blocks, context),
                    has_checkbox: item.has_checkbox,
                    is_checked: item.is_checked,
                    value: item.value,
                    r#type: item.r#type.clone(),
                })
                .collect(),
            start: self.start,
            is_reversed: self.is_reversed,
            r#type: self.r#type.clone(),
        }
    }

    /// `store` (WebPageBlock.cpp:2426-2443).
    fn store(&self, env: &mut impl WebPageBlockEnv, out: &mut Vec<u8>) {
        let has_start = self.start != 0;
        let has_type = !self.r#type.is_empty();
        store_flags(out, [self.is_reversed, has_start, has_type]);
        store_i32(out, self.items.len() as i32);
        for item in &self.items {
            item.store(env, out);
        }
        if has_start {
            store_i32(out, self.start);
        }
        if has_type {
            store_string(out, &self.r#type);
        }
    }

    /// `parse` (WebPageBlock.cpp:2445-2488) — the v1 pre-2.0 layout
    /// parses ordered-text items into paragraphs.
    fn parse(
        env: &mut impl WebPageBlockEnv,
        parser: &mut Parser<'_>,
    ) -> Result<Self, &'static str> {
        if parser.version() >= version::SUPPORT_INSTANT_VIEW_2_0 {
            let mut result = WebPageBlockList::default();
            let [is_reversed, has_start, has_type] =
                if parser.version() >= version::SUPPORT_RICH_MESSAGES {
                    parser.parse_flags()?
                } else {
                    [false, false, false]
                };
            result.is_reversed = is_reversed;
            let items_len = parser.i32_()?;
            if items_len < 0 {
                return Err("Invalid vector size");
            }
            for _ in 0..items_len {
                result.items.push(WebPageBlockListItem::parse(env, parser)?);
            }
            if has_start {
                result.start = parser.i32_()?;
            }
            if has_type {
                result.r#type = parser.string()?;
            }
            Ok(result)
        } else {
            // the legacy unordered/ordered text-item layout
            let [is_ordered] = parser.parse_flags()?;
            let text_items_len = parser.i32_()?;
            if text_items_len < 0 {
                return Err("Invalid vector size");
            }
            let mut items = Vec::with_capacity(text_items_len as usize);
            let mut pos = 0;
            for _ in 0..text_items_len {
                let text_item = RichText::parse(env, parser)?;
                let mut item = WebPageBlockListItem {
                    value: -1,
                    ..WebPageBlockListItem::default()
                };
                if is_ordered {
                    pos += 1;
                    item.label = format!("{pos}.");
                }
                item.page_blocks.push(WebPageBlock::Paragraph(text_item));
                items.push(item);
            }
            Ok(WebPageBlockList {
                items,
                start: 0,
                is_reversed: false,
                r#type: String::new(),
            })
        }
    }
}

/// `WebPageBlockButtonRow::Button` (WebPageBlock.cpp:2493).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebPageBlockRowButton {
    pub(crate) text: RichText,
    pub(crate) style: RichButtonStyle,
    pub(crate) button: InlineKeyboardButton,
}

impl WebPageBlockRowButton {
    /// `Button::get_button` (WebPageBlock.cpp:2498-2509).
    pub fn get_button(
        env: &mut dyn WebPageBlockEnv,
        button: Option<td_api::InlineButton>,
    ) -> std::result::Result<WebPageBlockRowButton, Status> {
        let mut result = WebPageBlockRowButton::default();
        let Some(button) = button else {
            return Err(Status::error(400, "Button must be non-empty"));
        };
        result.text = get_rich_text_from_td_api(env, button.text)?;
        result.button = InlineKeyboardButton::from_td_api(
            td_api::InlineKeyboardButton {
                text: "1".to_owned(),
                icon_custom_emoji_id: 0,
                style: None,
                r#type: button.r#type,
            },
            true,
            false,
        )?;
        result.style = RichButtonStyle::from_td_api(button.style);
        Ok(result)
    }
}

/// `WebPageBlockButtonRow` (WebPageBlock.cpp:2491).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebPageBlockButtonRow {
    pub(crate) buttons: Vec<WebPageBlockRowButton>,
    pub(crate) align_left: bool,
    pub(crate) align_center: bool,
    pub(crate) align_right: bool,
}

impl WebPageBlockButtonRow {
    /// `clone` (WebPageBlock.cpp:2632-2635).
    fn clone_row(&self, context: &mut CloneWebPageBlockContext) -> WebPageBlockButtonRow {
        WebPageBlockButtonRow {
            buttons: self
                .buttons
                .iter()
                .map(|button| {
                    let mut result = WebPageBlockRowButton {
                        style: button.style,
                        button: button.button.clone_button(
                            context.dialog_id,
                            context.dup_type,
                            context.is_via_bot,
                            true,
                        ),
                        ..button.clone()
                    };
                    if context.dup_type == MessageContentDupType::Forward
                        && !button.button.get_forward_text().is_empty()
                    {
                        result.text.r#type = RichTextType::Plain;
                        result.text.content = button.button.get_forward_text().to_owned();
                    } else {
                        result.text = button.text.clone_rich_text(context, false);
                    }
                    result
                })
                .collect(),
            align_left: self.align_left,
            align_center: self.align_center,
            align_right: self.align_right,
        }
    }

    /// `store` (WebPageBlock.cpp:2666-2675).
    fn store(&self, env: &mut impl WebPageBlockEnv, out: &mut Vec<u8>) {
        store_flags(out, [self.align_left, self.align_center, self.align_right]);
        store_i32(out, self.buttons.len() as i32);
        for button in &self.buttons {
            let has_style = !button.style.is_default();
            store_flags(out, [has_style]);
            button.text.store(env, out);
            button.button.store(out);
            if has_style {
                button.style.store(out);
            }
        }
    }

    /// `parse` (WebPageBlock.cpp:2677-2686).
    fn parse(
        env: &mut impl WebPageBlockEnv,
        parser: &mut Parser<'_>,
    ) -> Result<Self, &'static str> {
        let mut result = WebPageBlockButtonRow::default();
        let [align_left, align_center, align_right] = parser.parse_flags()?;
        result.align_left = align_left;
        result.align_center = align_center;
        result.align_right = align_right;
        let buttons_len = parser.i32_()?;
        if buttons_len < 0 {
            return Err("Invalid vector size");
        }
        for _ in 0..buttons_len {
            let mut button = WebPageBlockRowButton::default();
            let [has_style] = parser.parse_flags()?;
            button.text = RichText::parse(env, parser)?;
            button.button = InlineKeyboardButton::parse(parser)?;
            if has_style {
                button.style = RichButtonStyle::parse(parser)?;
            }
            result.buttons.push(button);
        }
        Ok(result)
    }
}

/// `WebPageBlockTable` (WebPageBlock.cpp:3964).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebPageBlockTable {
    pub(crate) title: RichText,
    pub(crate) cells: Vec<Vec<WebPageBlockTableCell>>,
    pub(crate) is_bordered: bool,
    pub(crate) is_striped: bool,
    pub(crate) is_compact: bool,
}

impl WebPageBlockTable {
    /// `clone` (WebPageBlock.cpp:4023-4034).
    fn clone_table(&self, context: &mut CloneWebPageBlockContext) -> WebPageBlockTable {
        WebPageBlockTable {
            title: self.title.clone_rich_text(context, false),
            cells: self
                .cells
                .iter()
                .map(|row| row.iter().map(|cell| cell.clone_cell(context)).collect())
                .collect(),
            is_bordered: self.is_bordered,
            is_striped: self.is_striped,
            is_compact: self.is_compact,
        }
    }

    /// `store` (WebPageBlock.cpp:4060-4070).
    fn store(&self, env: &mut impl WebPageBlockEnv, out: &mut Vec<u8>) {
        store_flags(out, [self.is_bordered, self.is_striped, self.is_compact]);
        self.title.store(env, out);
        store_i32(out, self.cells.len() as i32);
        for row in &self.cells {
            store_i32(out, row.len() as i32);
            for cell in row {
                cell.store(env, out);
            }
        }
    }

    /// `parse` (WebPageBlock.cpp:4072-4082).
    fn parse(
        env: &mut impl WebPageBlockEnv,
        parser: &mut Parser<'_>,
    ) -> Result<Self, &'static str> {
        let mut result = WebPageBlockTable::default();
        let [is_bordered, is_striped, is_compact] = parser.parse_flags()?;
        result.is_bordered = is_bordered;
        result.is_striped = is_striped;
        result.is_compact = is_compact;
        result.title = RichText::parse(env, parser)?;
        let rows_len = parser.i32_()?;
        if rows_len < 0 {
            return Err("Invalid vector size");
        }
        for _ in 0..rows_len {
            let row_len = parser.i32_()?;
            if row_len < 0 {
                return Err("Invalid vector size");
            }
            let mut row = Vec::with_capacity(row_len as usize);
            for _ in 0..row_len {
                row.push(WebPageBlockTableCell::parse(env, parser)?);
            }
            result.cells.push(row);
        }
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// WebPageBlock (WebPageBlock.h:50) — the 37 block types as one enum
// ---------------------------------------------------------------------------

/// `WebPageBlock::Type` (WebPageBlock.h:52-91) — the stored i32 ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum BlockType {
    Title = 0,
    Subtitle,
    AuthorDate,
    Header,
    Subheader,
    Paragraph,
    Preformatted,
    Footer,
    Divider,
    Anchor,
    List,
    BlockQuote,
    PullQuote,
    Animation,
    Photo,
    Video,
    Cover,
    Embedded,
    EmbeddedPost,
    Collage,
    Slideshow,
    ChatLink,
    Audio,
    Kicker,
    Table,
    Details,
    RelatedArticles,
    Map,
    VoiceNote,
    Heading,
    Math,
    Thinking,
    BlockQuoteBlocks,
    Document,
    ButtonRow,
    ExpandableBlockQuote,
    Unsupported,
    Size,
}

impl TryFrom<i32> for BlockType {
    type Error = ();

    fn try_from(value: i32) -> std::result::Result<Self, Self::Error> {
        if value < 0 || value >= BlockType::Size as i32 {
            return Err(());
        }
        Ok(match value {
            0 => BlockType::Title,
            1 => BlockType::Subtitle,
            2 => BlockType::AuthorDate,
            3 => BlockType::Header,
            4 => BlockType::Subheader,
            5 => BlockType::Paragraph,
            6 => BlockType::Preformatted,
            7 => BlockType::Footer,
            8 => BlockType::Divider,
            9 => BlockType::Anchor,
            10 => BlockType::List,
            11 => BlockType::BlockQuote,
            12 => BlockType::PullQuote,
            13 => BlockType::Animation,
            14 => BlockType::Photo,
            15 => BlockType::Video,
            16 => BlockType::Cover,
            17 => BlockType::Embedded,
            18 => BlockType::EmbeddedPost,
            19 => BlockType::Collage,
            20 => BlockType::Slideshow,
            21 => BlockType::ChatLink,
            22 => BlockType::Audio,
            23 => BlockType::Kicker,
            24 => BlockType::Table,
            25 => BlockType::Details,
            26 => BlockType::RelatedArticles,
            27 => BlockType::Map,
            28 => BlockType::VoiceNote,
            29 => BlockType::Heading,
            30 => BlockType::Math,
            31 => BlockType::Thinking,
            32 => BlockType::BlockQuoteBlocks,
            33 => BlockType::Document,
            34 => BlockType::ButtonRow,
            35 => BlockType::ExpandableBlockQuote,
            36 => BlockType::Unsupported,
            _ => return Err(()),
        })
    }
}

/// The `td::WebPageBlock` class family as one enum; the variants carry
/// the per-class fields in the C++ declaration order.
#[derive(Clone, Debug, PartialEq)]
pub enum WebPageBlock {
    /// `WebPageBlockTitle` (WebPageBlock.cpp:1261).
    Title(RichText),
    /// `WebPageBlockSubtitle` (WebPageBlock.cpp:1320).
    Subtitle(RichText),
    /// `WebPageBlockAuthorDate` (WebPageBlock.cpp:1378) — the
    /// constructor clamps the date non-negative.
    AuthorDate { author: RichText, date: i32 },
    /// `WebPageBlockHeader` (WebPageBlock.cpp:1439).
    Header(RichText),
    /// `WebPageBlockSubheader` (WebPageBlock.cpp:1497).
    Subheader(RichText),
    /// `WebPageBlockParagraph` (WebPageBlock.cpp:1695).
    Paragraph(RichText),
    /// `WebPageBlockPreformatted` (WebPageBlock.cpp:1752).
    Preformatted { text: RichText, language: String },
    /// `WebPageBlockFooter` (WebPageBlock.cpp:1812).
    Footer(RichText),
    /// `WebPageBlockDivider` (WebPageBlock.cpp:1930).
    Divider,
    /// `WebPageBlockAnchor` (WebPageBlock.cpp:2032).
    Anchor { name: String },
    /// `WebPageBlockList` (WebPageBlock.cpp:2167).
    List(WebPageBlockList),
    /// `WebPageBlockBlockQuote` (WebPageBlock.cpp:2689).
    BlockQuote { text: RichText, credit: RichText },
    /// `WebPageBlockPullQuote` (WebPageBlock.cpp:2826).
    PullQuote { text: RichText, credit: RichText },
    /// `WebPageBlockAnimation` (WebPageBlock.cpp:2891).
    Animation {
        animation_file_id: FileId,
        caption: WebPageBlockCaption,
        need_autoplay: bool,
        has_spoiler: bool,
    },
    /// `WebPageBlockPhoto` (WebPageBlock.cpp:3012).
    Photo {
        photo: Photo,
        caption: WebPageBlockCaption,
        url: String,
        web_page_id: WebPageId,
        has_spoiler: bool,
    },
    /// `WebPageBlockVideo` (WebPageBlock.cpp:3127).
    Video {
        video_file_id: FileId,
        caption: WebPageBlockCaption,
        need_autoplay: bool,
        is_looped: bool,
        has_spoiler: bool,
    },
    /// `WebPageBlockCover` (WebPageBlock.cpp:3255).
    Cover(Box<WebPageBlock>),
    /// `WebPageBlockEmbedded` (WebPageBlock.cpp:3325).
    Embedded {
        url: String,
        html: String,
        poster_photo: Photo,
        dimensions: crate::photo::Dimensions,
        caption: WebPageBlockCaption,
        is_full_width: bool,
        allow_scrolling: bool,
    },
    /// `WebPageBlockEmbeddedPost` (WebPageBlock.cpp:3425).
    EmbeddedPost {
        url: String,
        author: String,
        author_photo: Photo,
        date: i32,
        page_blocks: Vec<WebPageBlock>,
        caption: WebPageBlockCaption,
    },
    /// `WebPageBlockCollage` (WebPageBlock.cpp:3524).
    Collage {
        page_blocks: Vec<WebPageBlock>,
        caption: WebPageBlockCaption,
    },
    /// `WebPageBlockSlideshow` (WebPageBlock.cpp:3620).
    Slideshow {
        page_blocks: Vec<WebPageBlock>,
        caption: WebPageBlockCaption,
    },
    /// `WebPageBlockChatLink` (WebPageBlock.cpp:3716).
    ChatLink(ChatLinkInfo),
    /// `WebPageBlockAudio` (WebPageBlock.cpp:3845).
    Audio {
        audio_file_id: FileId,
        caption: WebPageBlockCaption,
    },
    /// `WebPageBlockKicker` (WebPageBlock.cpp:1637).
    Kicker(RichText),
    /// `WebPageBlockTable` (WebPageBlock.cpp:3964).
    Table(WebPageBlockTable),
    /// `WebPageBlockDetails` (WebPageBlock.cpp:4085).
    Details {
        header: RichText,
        page_blocks: Vec<WebPageBlock>,
        is_open: bool,
    },
    /// `WebPageBlockRelatedArticles` (WebPageBlock.cpp:4290).
    RelatedArticles {
        header: RichText,
        related_articles: Vec<RelatedArticle>,
    },
    /// `WebPageBlockMap` (WebPageBlock.cpp:4366).
    Map {
        location: crate::chat_manager::Location,
        zoom: i32,
        dimensions: crate::photo::Dimensions,
        caption: WebPageBlockCaption,
    },
    /// `WebPageBlockVoiceNote` (WebPageBlock.cpp:4437).
    VoiceNote {
        voice_note_file_id: FileId,
        caption: WebPageBlockCaption,
    },
    /// `WebPageBlockHeading` (WebPageBlock.cpp:1555) — the size clamps
    /// to 1..=6 on parse.
    Heading { text: RichText, size: i32 },
    /// `WebPageBlockMath` (WebPageBlock.cpp:1974).
    Math { source: String },
    /// `WebPageBlockThinking` (WebPageBlock.cpp:1869).
    Thinking(RichText),
    /// `WebPageBlockBlockQuoteBlocks` (WebPageBlock.cpp:4189).
    BlockQuoteBlocks {
        page_blocks: Vec<WebPageBlock>,
        caption: RichText,
    },
    /// `WebPageBlockDocument` (WebPageBlock.cpp:4545).
    Document {
        document_file_id: FileId,
        caption: WebPageBlockCaption,
    },
    /// `WebPageBlockButtonRow` (WebPageBlock.cpp:2491).
    ButtonRow(WebPageBlockButtonRow),
    /// `WebPageBlockExpandableBlockQuote` (WebPageBlock.cpp:2756).
    ExpandableBlockQuote { text: RichText, credit: RichText },
    /// `WebPageBlockUnsupported` (WebPageBlock.cpp:1200) — the
    /// block-format version holder.
    Unsupported { version: i32 },
}

/// `WebPageBlockUnsupported::CURRENT_VERSION` (WebPageBlock.cpp:1201).
pub const WEB_PAGE_BLOCK_UNSUPPORTED_VERSION: i32 = 1;

impl WebPageBlock {
    /// `get_type` (WebPageBlock.h:93).
    pub fn get_type(&self) -> BlockType {
        match self {
            WebPageBlock::Title(_) => BlockType::Title,
            WebPageBlock::Subtitle(_) => BlockType::Subtitle,
            WebPageBlock::AuthorDate { .. } => BlockType::AuthorDate,
            WebPageBlock::Header(_) => BlockType::Header,
            WebPageBlock::Subheader(_) => BlockType::Subheader,
            WebPageBlock::Paragraph(_) => BlockType::Paragraph,
            WebPageBlock::Preformatted { .. } => BlockType::Preformatted,
            WebPageBlock::Footer(_) => BlockType::Footer,
            WebPageBlock::Divider => BlockType::Divider,
            WebPageBlock::Anchor { .. } => BlockType::Anchor,
            WebPageBlock::List(_) => BlockType::List,
            WebPageBlock::BlockQuote { .. } => BlockType::BlockQuote,
            WebPageBlock::PullQuote { .. } => BlockType::PullQuote,
            WebPageBlock::Animation { .. } => BlockType::Animation,
            WebPageBlock::Photo { .. } => BlockType::Photo,
            WebPageBlock::Video { .. } => BlockType::Video,
            WebPageBlock::Cover(_) => BlockType::Cover,
            WebPageBlock::Embedded { .. } => BlockType::Embedded,
            WebPageBlock::EmbeddedPost { .. } => BlockType::EmbeddedPost,
            WebPageBlock::Collage { .. } => BlockType::Collage,
            WebPageBlock::Slideshow { .. } => BlockType::Slideshow,
            WebPageBlock::ChatLink(_) => BlockType::ChatLink,
            WebPageBlock::Audio { .. } => BlockType::Audio,
            WebPageBlock::Kicker(_) => BlockType::Kicker,
            WebPageBlock::Table(_) => BlockType::Table,
            WebPageBlock::Details { .. } => BlockType::Details,
            WebPageBlock::RelatedArticles { .. } => BlockType::RelatedArticles,
            WebPageBlock::Map { .. } => BlockType::Map,
            WebPageBlock::VoiceNote { .. } => BlockType::VoiceNote,
            WebPageBlock::Heading { .. } => BlockType::Heading,
            WebPageBlock::Math { .. } => BlockType::Math,
            WebPageBlock::Thinking(_) => BlockType::Thinking,
            WebPageBlock::BlockQuoteBlocks { .. } => BlockType::BlockQuoteBlocks,
            WebPageBlock::Document { .. } => BlockType::Document,
            WebPageBlock::ButtonRow(_) => BlockType::ButtonRow,
            WebPageBlock::ExpandableBlockQuote { .. } => BlockType::ExpandableBlockQuote,
            WebPageBlock::Unsupported { .. } => BlockType::Unsupported,
        }
    }

    /// `are_allowed_album_block_types` (WebPageBlock.cpp:5863-5876).
    pub fn are_allowed_album_block_types(page_blocks: &[WebPageBlock]) -> bool {
        page_blocks.iter().all(|block| {
            matches!(
                block.get_type(),
                BlockType::Title
                    | BlockType::AuthorDate
                    | BlockType::Collage
                    | BlockType::Slideshow
            )
        })
    }

    /// `need_reget` (WebPageBlock.h:150-152 + the Unsupported/Cover/List
    /// recursive overrides).
    pub fn need_reget(&self) -> bool {
        match self {
            WebPageBlock::Unsupported { version } => *version != WEB_PAGE_BLOCK_UNSUPPORTED_VERSION,
            WebPageBlock::Cover(cover) => cover.need_reget(),
            WebPageBlock::List(list) => list
                .items
                .iter()
                .any(|item| item.page_blocks.iter().any(Self::need_reget)),
            WebPageBlock::Details { page_blocks, .. }
            | WebPageBlock::BlockQuoteBlocks { page_blocks, .. }
            | WebPageBlock::Collage { page_blocks, .. }
            | WebPageBlock::Slideshow { page_blocks, .. } => {
                page_blocks.iter().any(Self::need_reget)
            }
            _ => false,
        }
    }

    /// `can_send` (WebPageBlock.h:146-148 + the media overrides).
    pub fn can_send(&self, rights: &crate::chat_manager::RestrictedRights) -> bool {
        match self {
            WebPageBlock::Animation { .. } => rights.can_send_animations(),
            WebPageBlock::Photo { .. } => rights.can_send_photos(),
            WebPageBlock::Video { .. } => rights.can_send_videos(),
            WebPageBlock::Cover(cover) => cover.can_send(rights),
            WebPageBlock::Embedded { poster_photo, .. } => {
                poster_photo.is_empty() || rights.can_send_photos()
            }
            WebPageBlock::EmbeddedPost { author_photo, .. } => {
                author_photo.is_empty() || rights.can_send_photos()
            }
            WebPageBlock::Audio { .. } => rights.can_send_audios(),
            WebPageBlock::VoiceNote { .. } => rights.can_send_voice_notes(),
            WebPageBlock::Document { .. } => rights.can_send_documents(),
            WebPageBlock::List(list) => list
                .items
                .iter()
                .all(|item| item.page_blocks.iter().all(|b| b.can_send(rights))),
            WebPageBlock::ButtonRow(_) => true,
            WebPageBlock::Details { page_blocks, .. }
            | WebPageBlock::BlockQuoteBlocks { page_blocks, .. }
            | WebPageBlock::Collage { page_blocks, .. }
            | WebPageBlock::Slideshow { page_blocks, .. } => {
                page_blocks.iter().all(|b| b.can_send(rights))
            }
            _ => true,
        }
    }

    /// `append_file_ids` (WebPageBlock.h:126 + overrides).
    pub fn append_file_ids(&self, file_ids: &mut Vec<FileId>) {
        match self {
            WebPageBlock::Title(text)
            | WebPageBlock::Subtitle(text)
            | WebPageBlock::Header(text)
            | WebPageBlock::Subheader(text)
            | WebPageBlock::Kicker(text)
            | WebPageBlock::Paragraph(text)
            | WebPageBlock::Footer(text)
            | WebPageBlock::Thinking(text) => text.append_file_ids(file_ids),
            WebPageBlock::AuthorDate { author, .. } => author.append_file_ids(file_ids),
            WebPageBlock::Preformatted { text, .. } => text.append_file_ids(file_ids),
            WebPageBlock::Divider => {}
            WebPageBlock::Anchor { .. } => {}
            WebPageBlock::Math { .. } => {}
            WebPageBlock::List(list) => {
                for item in &list.items {
                    for page_block in &item.page_blocks {
                        page_block.append_file_ids(file_ids);
                    }
                }
            }
            WebPageBlock::BlockQuote { text, credit }
            | WebPageBlock::PullQuote { text, credit }
            | WebPageBlock::ExpandableBlockQuote { text, credit } => {
                text.append_file_ids(file_ids);
                credit.append_file_ids(file_ids);
            }
            WebPageBlock::Animation {
                animation_file_id,
                caption,
                ..
            } => {
                caption.append_file_ids(file_ids);
                append_document_file_id(*animation_file_id, file_ids);
            }
            WebPageBlock::Photo { photo, caption, .. } => {
                crate::photo::photo_append_file_ids(photo, file_ids);
                caption.append_file_ids(file_ids);
            }
            WebPageBlock::Video {
                video_file_id,
                caption,
                ..
            } => {
                caption.append_file_ids(file_ids);
                append_document_file_id(*video_file_id, file_ids);
            }
            WebPageBlock::Cover(cover) => cover.append_file_ids(file_ids),
            WebPageBlock::Embedded {
                poster_photo,
                caption,
                ..
            } => {
                crate::photo::photo_append_file_ids(poster_photo, file_ids);
                caption.append_file_ids(file_ids);
            }
            WebPageBlock::EmbeddedPost {
                author_photo,
                page_blocks,
                caption,
                ..
            } => {
                crate::photo::photo_append_file_ids(author_photo, file_ids);
                for page_block in page_blocks {
                    page_block.append_file_ids(file_ids);
                }
                caption.append_file_ids(file_ids);
            }
            WebPageBlock::Collage {
                page_blocks,
                caption,
            }
            | WebPageBlock::Slideshow {
                page_blocks,
                caption,
            } => {
                for page_block in page_blocks {
                    page_block.append_file_ids(file_ids);
                }
                caption.append_file_ids(file_ids);
            }
            WebPageBlock::ChatLink(chat_link) => {
                for file_id in crate::photo::dialog_photo_get_file_ids(&chat_link.photo) {
                    file_ids.push(file_id);
                }
            }
            WebPageBlock::Audio {
                audio_file_id,
                caption,
            } => {
                append_document_file_id(*audio_file_id, file_ids);
                caption.append_file_ids(file_ids);
            }
            WebPageBlock::Table(table) => {
                table.title.append_file_ids(file_ids);
                for row in &table.cells {
                    for cell in row {
                        cell.text.append_file_ids(file_ids);
                    }
                }
            }
            WebPageBlock::Details {
                header,
                page_blocks,
                ..
            } => {
                header.append_file_ids(file_ids);
                for page_block in page_blocks {
                    page_block.append_file_ids(file_ids);
                }
            }
            WebPageBlock::RelatedArticles {
                header,
                related_articles,
            } => {
                header.append_file_ids(file_ids);
                for article in related_articles {
                    crate::photo::photo_append_file_ids(&article.photo, file_ids);
                }
            }
            WebPageBlock::Map { caption, .. } => caption.append_file_ids(file_ids),
            WebPageBlock::VoiceNote {
                voice_note_file_id,
                caption,
            } => {
                append_document_file_id(*voice_note_file_id, file_ids);
                caption.append_file_ids(file_ids);
            }
            WebPageBlock::Heading { text, .. } => text.append_file_ids(file_ids),
            WebPageBlock::BlockQuoteBlocks {
                page_blocks,
                caption,
            } => {
                for page_block in page_blocks {
                    page_block.append_file_ids(file_ids);
                }
                caption.append_file_ids(file_ids);
            }
            WebPageBlock::Document {
                document_file_id,
                caption,
            } => {
                append_document_file_id(*document_file_id, file_ids);
                caption.append_file_ids(file_ids);
            }
            WebPageBlock::ButtonRow(button_row) => {
                for button in &button_row.buttons {
                    button.text.append_file_ids(file_ids);
                }
            }
            WebPageBlock::Unsupported { .. } => {}
        }
    }

    /// `for_each_rich_text` (WebPageBlock.h:133-134 + overrides).
    pub fn for_each_rich_text<'a>(
        &'a self,
        recurse_text: bool,
        callback: &mut impl FnMut(&'a RichText),
    ) {
        match self {
            WebPageBlock::Title(text)
            | WebPageBlock::Subtitle(text)
            | WebPageBlock::Header(text)
            | WebPageBlock::Subheader(text)
            | WebPageBlock::Kicker(text)
            | WebPageBlock::Paragraph(text)
            | WebPageBlock::Footer(text)
            | WebPageBlock::Thinking(text) => text.for_each_rich_text(recurse_text, callback),
            WebPageBlock::AuthorDate { author, .. } => {
                author.for_each_rich_text(recurse_text, callback)
            }
            WebPageBlock::Preformatted { text, .. } => {
                text.for_each_rich_text(recurse_text, callback)
            }
            WebPageBlock::Divider => {}
            WebPageBlock::Anchor { .. } => {}
            WebPageBlock::Math { .. } => {}
            WebPageBlock::List(list) => {
                for item in &list.items {
                    for page_block in &item.page_blocks {
                        page_block.for_each_rich_text(recurse_text, callback);
                    }
                }
            }
            WebPageBlock::BlockQuote { text, credit }
            | WebPageBlock::PullQuote { text, credit }
            | WebPageBlock::ExpandableBlockQuote { text, credit } => {
                text.for_each_rich_text(recurse_text, callback);
                credit.for_each_rich_text(recurse_text, callback);
            }
            WebPageBlock::Animation { caption, .. }
            | WebPageBlock::Photo { caption, .. }
            | WebPageBlock::Video { caption, .. }
            | WebPageBlock::Embedded { caption, .. }
            | WebPageBlock::EmbeddedPost { caption, .. }
            | WebPageBlock::Collage { caption, .. }
            | WebPageBlock::Slideshow { caption, .. }
            | WebPageBlock::Audio { caption, .. }
            | WebPageBlock::Map { caption, .. }
            | WebPageBlock::VoiceNote { caption, .. }
            | WebPageBlock::Document { caption, .. } => {
                caption.for_each_rich_text(recurse_text, callback)
            }
            WebPageBlock::Cover(cover) => cover.for_each_rich_text(recurse_text, callback),
            WebPageBlock::ChatLink(_) => {}
            WebPageBlock::Table(table) => {
                table.title.for_each_rich_text(recurse_text, callback);
                for row in &table.cells {
                    for cell in row {
                        cell.text.for_each_rich_text(recurse_text, callback);
                    }
                }
            }
            WebPageBlock::Details {
                header,
                page_blocks,
                ..
            } => {
                header.for_each_rich_text(recurse_text, callback);
                for page_block in page_blocks {
                    page_block.for_each_rich_text(recurse_text, callback);
                }
            }
            WebPageBlock::RelatedArticles { header, .. } => {
                header.for_each_rich_text(recurse_text, callback)
            }
            WebPageBlock::Heading { text, .. } => text.for_each_rich_text(recurse_text, callback),
            WebPageBlock::BlockQuoteBlocks {
                page_blocks,
                caption,
            } => {
                for page_block in page_blocks {
                    page_block.for_each_rich_text(recurse_text, callback);
                }
                caption.for_each_rich_text(recurse_text, callback);
            }
            WebPageBlock::ButtonRow(button_row) => {
                for button in &button_row.buttons {
                    button.text.for_each_rich_text(recurse_text, callback);
                }
            }
            WebPageBlock::Unsupported { .. } => {}
        }
    }

    /// `for_each_quote_text` (WebPageBlock.h:148-152 + overrides) — calls
    /// the callback for each quotable text in the order of their
    /// appearance; the default answers `for_each_rich_text(false, …)`, the
    /// overrides collect the sub-blocks and captions (upstream ecb7fb163).
    /// The callback borrows are short-lived: the Math arm hands out a
    /// freshly built `create_math` node like the C++.
    pub fn for_each_quote_text(&self, callback: &mut impl FnMut(&RichText)) {
        match self {
            // the overrides: WebPageBlockMath, WebPageBlockList,
            // WebPageBlockButtonRow, WebPageBlockCover, WebPageBlockEmbeddedPost,
            // WebPageBlockCollage, WebPageBlockSlideshow, WebPageBlockDetails,
            // WebPageBlockBlockQuoteBlocks
            WebPageBlock::Math { source } => {
                let math = RichText::create_math(source.clone());
                callback(&math);
            }
            WebPageBlock::List(list) => {
                for item in &list.items {
                    for_each_web_page_blocks_quote_text(&item.page_blocks, callback);
                }
            }
            WebPageBlock::ButtonRow(_) => {}
            WebPageBlock::Cover(cover) => cover.for_each_quote_text(callback),
            WebPageBlock::EmbeddedPost {
                page_blocks,
                caption,
                ..
            }
            | WebPageBlock::Collage {
                page_blocks,
                caption,
                ..
            }
            | WebPageBlock::Slideshow {
                page_blocks,
                caption,
                ..
            } => {
                for_each_web_page_blocks_quote_text(page_blocks, callback);
                caption.for_each_rich_text(false, callback);
            }
            WebPageBlock::Details {
                header,
                page_blocks,
                ..
            } => {
                header.for_each_rich_text(false, callback);
                for_each_web_page_blocks_quote_text(page_blocks, callback);
            }
            WebPageBlock::BlockQuoteBlocks {
                page_blocks,
                caption,
            } => {
                for_each_web_page_blocks_quote_text(page_blocks, callback);
                caption.for_each_rich_text(false, callback);
            }
            // the virtual default: for_each_rich_text(false, callback)
            block => block.for_each_rich_text(false, callback),
        }
    }

    /// `remove_empty_media_blocks` (WebPageBlock.h:143 + overrides) —
    /// removes nested media blocks without files; answers whether the
    /// block itself has no files and must be removed (upstream
    /// a7ccc2344). The RichMessage ctor/parse call sites ride the
    /// unmodeled RichMessage class (Wave-2 WebPages).
    pub fn remove_empty_media_blocks(&mut self) -> bool {
        match self {
            WebPageBlock::List(list) => {
                for item in &mut list.items {
                    remove_empty_media_page_blocks(&mut item.page_blocks);
                }
                false
            }
            WebPageBlock::Animation {
                animation_file_id, ..
            } => !animation_file_id.is_valid(),
            WebPageBlock::Photo { photo, .. } => photo.is_empty(),
            WebPageBlock::Video { video_file_id, .. } => !video_file_id.is_valid(),
            WebPageBlock::Cover(cover) => cover.remove_empty_media_blocks(),
            WebPageBlock::Collage { page_blocks, .. } => {
                remove_empty_media_page_blocks(page_blocks);
                page_blocks.is_empty()
            }
            WebPageBlock::Slideshow { page_blocks, .. } => {
                remove_empty_media_page_blocks(page_blocks);
                page_blocks.is_empty()
            }
            WebPageBlock::Audio { audio_file_id, .. } => !audio_file_id.is_valid(),
            WebPageBlock::Details { page_blocks, .. } => {
                remove_empty_media_page_blocks(page_blocks);
                false
            }
            WebPageBlock::BlockQuoteBlocks { page_blocks, .. } => {
                remove_empty_media_page_blocks(page_blocks);
                false
            }
            WebPageBlock::VoiceNote {
                voice_note_file_id, ..
            } => !voice_note_file_id.is_valid(),
            WebPageBlock::Document {
                document_file_id, ..
            } => !document_file_id.is_valid(),
            // the virtual default
            _ => false,
        }
    }

    /// `get_index_mask` (WebPageBlock.h:154 + overrides).
    pub fn get_index_mask(&self) -> i32 {
        match self {
            WebPageBlock::Title(text)
            | WebPageBlock::Subtitle(text)
            | WebPageBlock::Header(text)
            | WebPageBlock::Subheader(text)
            | WebPageBlock::Kicker(text)
            | WebPageBlock::Paragraph(text)
            | WebPageBlock::Footer(text)
            | WebPageBlock::Preformatted { text, .. }
            | WebPageBlock::Thinking(text) => text.get_index_mask(),
            WebPageBlock::AuthorDate { author, .. } => author.get_index_mask(),
            WebPageBlock::Divider | WebPageBlock::Anchor { .. } | WebPageBlock::Math { .. } => 0,
            WebPageBlock::List(list) => {
                let mut index_mask = 0;
                for item in &list.items {
                    index_mask |= item.get_index_mask();
                }
                index_mask
            }
            WebPageBlock::BlockQuote { text, credit }
            | WebPageBlock::PullQuote { text, credit }
            | WebPageBlock::ExpandableBlockQuote { text, credit } => {
                text.get_index_mask() | credit.get_index_mask()
            }
            WebPageBlock::Animation {
                animation_file_id,
                caption,
                ..
            } => {
                let index_mask = caption.get_index_mask();
                if animation_file_id.is_valid() {
                    index_mask
                        | crate::dialog_id::message_search_filter_index_mask(
                            crate::dialog_id::MessageSearchFilter::Animation,
                        )
                } else {
                    index_mask
                }
            }
            WebPageBlock::Photo { photo, caption, .. } => {
                let index_mask = caption.get_index_mask();
                if !photo.is_empty() {
                    index_mask
                        | crate::dialog_id::message_search_filter_index_mask(
                            crate::dialog_id::MessageSearchFilter::Photo,
                        )
                        | crate::dialog_id::message_search_filter_index_mask(
                            crate::dialog_id::MessageSearchFilter::PhotoAndVideo,
                        )
                } else {
                    index_mask
                }
            }
            WebPageBlock::Video {
                video_file_id,
                caption,
                ..
            } => {
                let index_mask = caption.get_index_mask();
                if video_file_id.is_valid() {
                    index_mask
                        | crate::dialog_id::message_search_filter_index_mask(
                            crate::dialog_id::MessageSearchFilter::Video,
                        )
                        | crate::dialog_id::message_search_filter_index_mask(
                            crate::dialog_id::MessageSearchFilter::PhotoAndVideo,
                        )
                } else {
                    index_mask
                }
            }
            WebPageBlock::Cover(cover) => cover.get_index_mask(),
            WebPageBlock::Embedded { caption, .. } => caption.get_index_mask(),
            WebPageBlock::EmbeddedPost {
                caption,
                page_blocks,
                ..
            }
            | WebPageBlock::Collage {
                page_blocks,
                caption,
            }
            | WebPageBlock::Slideshow {
                page_blocks,
                caption,
            } => get_web_page_blocks_index_mask(page_blocks) | caption.get_index_mask(),
            WebPageBlock::ChatLink(_) => 0,
            WebPageBlock::Audio {
                audio_file_id,
                caption,
            } => {
                let index_mask = caption.get_index_mask();
                if audio_file_id.is_valid() {
                    index_mask
                        | crate::dialog_id::message_search_filter_index_mask(
                            crate::dialog_id::MessageSearchFilter::Audio,
                        )
                } else {
                    index_mask
                }
            }
            WebPageBlock::Table(table) => {
                let mut index_mask = table.title.get_index_mask();
                for row in &table.cells {
                    for cell in row {
                        index_mask |= cell.text.get_index_mask();
                    }
                }
                index_mask
            }
            WebPageBlock::Details {
                header,
                page_blocks,
                ..
            } => header.get_index_mask() | get_web_page_blocks_index_mask(page_blocks),
            WebPageBlock::RelatedArticles { header, .. } => header.get_index_mask(),
            WebPageBlock::Map { caption, .. } => caption.get_index_mask(),
            WebPageBlock::VoiceNote {
                voice_note_file_id,
                caption,
            } => {
                let index_mask = caption.get_index_mask();
                if voice_note_file_id.is_valid() {
                    index_mask
                        | crate::dialog_id::message_search_filter_index_mask(
                            crate::dialog_id::MessageSearchFilter::VoiceNote,
                        )
                        | crate::dialog_id::message_search_filter_index_mask(
                            crate::dialog_id::MessageSearchFilter::VoiceAndVideoNote,
                        )
                } else {
                    index_mask
                }
            }
            WebPageBlock::Heading { text, .. } => text.get_index_mask(),
            WebPageBlock::BlockQuoteBlocks {
                page_blocks,
                caption,
            } => get_web_page_blocks_index_mask(page_blocks) | caption.get_index_mask(),
            WebPageBlock::Document {
                document_file_id,
                caption,
            } => {
                let index_mask = caption.get_index_mask();
                if document_file_id.is_valid() {
                    index_mask
                        | crate::dialog_id::message_search_filter_index_mask(
                            crate::dialog_id::MessageSearchFilter::Document,
                        )
                } else {
                    index_mask
                }
            }
            WebPageBlock::ButtonRow(button_row) => {
                let mut index_mask = 0;
                for button in &button_row.buttons {
                    index_mask |= button.text.get_index_mask();
                }
                index_mask
            }
            WebPageBlock::Unsupported { .. } => 0,
        }
    }

    /// `clone` (WebPageBlock.h:156 + overrides).
    pub fn clone_block(&self, context: &mut CloneWebPageBlockContext) -> WebPageBlock {
        match self {
            WebPageBlock::Title(text) => WebPageBlock::Title(text.clone_rich_text(context, false)),
            WebPageBlock::Subtitle(text) => {
                WebPageBlock::Subtitle(text.clone_rich_text(context, false))
            }
            WebPageBlock::AuthorDate { author, date } => WebPageBlock::AuthorDate {
                author: author.clone_rich_text(context, false),
                date: *date,
            },
            WebPageBlock::Header(text) => {
                WebPageBlock::Header(text.clone_rich_text(context, false))
            }
            WebPageBlock::Subheader(text) => {
                WebPageBlock::Subheader(text.clone_rich_text(context, false))
            }
            WebPageBlock::Paragraph(text) => {
                WebPageBlock::Paragraph(text.clone_rich_text(context, false))
            }
            WebPageBlock::Preformatted { text, language } => WebPageBlock::Preformatted {
                text: text.clone_rich_text(context, false),
                language: language.clone(),
            },
            WebPageBlock::Footer(text) => {
                WebPageBlock::Footer(text.clone_rich_text(context, false))
            }
            WebPageBlock::Divider => WebPageBlock::Divider,
            WebPageBlock::Anchor { name } => WebPageBlock::Anchor { name: name.clone() },
            WebPageBlock::List(list) => WebPageBlock::List(list.clone_list(context)),
            WebPageBlock::BlockQuote { text, credit } => WebPageBlock::BlockQuote {
                text: text.clone_rich_text(context, false),
                credit: credit.clone_rich_text(context, false),
            },
            WebPageBlock::PullQuote { text, credit } => WebPageBlock::PullQuote {
                text: text.clone_rich_text(context, false),
                credit: credit.clone_rich_text(context, false),
            },
            WebPageBlock::Animation {
                animation_file_id,
                caption,
                need_autoplay,
                has_spoiler,
            } => WebPageBlock::Animation {
                animation_file_id: *animation_file_id,
                caption: caption.clone_caption(context),
                need_autoplay: *need_autoplay,
                has_spoiler: *has_spoiler,
            },
            WebPageBlock::Photo {
                photo,
                caption,
                url,
                web_page_id,
                has_spoiler,
            } => WebPageBlock::Photo {
                photo: photo.clone(),
                caption: caption.clone_caption(context),
                url: url.clone(),
                web_page_id: *web_page_id,
                has_spoiler: *has_spoiler,
            },
            WebPageBlock::Video {
                video_file_id,
                caption,
                need_autoplay,
                is_looped,
                has_spoiler,
            } => WebPageBlock::Video {
                video_file_id: *video_file_id,
                caption: caption.clone_caption(context),
                need_autoplay: *need_autoplay,
                is_looped: *is_looped,
                has_spoiler: *has_spoiler,
            },
            WebPageBlock::Cover(cover) => WebPageBlock::Cover(Box::new(cover.clone_block(context))),
            WebPageBlock::Embedded {
                url,
                html,
                poster_photo,
                dimensions,
                caption,
                is_full_width,
                allow_scrolling,
            } => WebPageBlock::Embedded {
                url: url.clone(),
                html: html.clone(),
                poster_photo: poster_photo.clone(),
                dimensions: *dimensions,
                caption: caption.clone_caption(context),
                is_full_width: *is_full_width,
                allow_scrolling: *allow_scrolling,
            },
            WebPageBlock::EmbeddedPost {
                url,
                author,
                author_photo,
                date,
                page_blocks,
                caption,
            } => WebPageBlock::EmbeddedPost {
                url: url.clone(),
                author: author.clone(),
                author_photo: author_photo.clone(),
                date: *date,
                page_blocks: clone_web_page_blocks(page_blocks, context),
                caption: caption.clone_caption(context),
            },
            WebPageBlock::Collage {
                page_blocks,
                caption,
            } => WebPageBlock::Collage {
                page_blocks: clone_web_page_blocks(page_blocks, context),
                caption: caption.clone_caption(context),
            },
            WebPageBlock::Slideshow {
                page_blocks,
                caption,
            } => WebPageBlock::Slideshow {
                page_blocks: clone_web_page_blocks(page_blocks, context),
                caption: caption.clone_caption(context),
            },
            WebPageBlock::ChatLink(chat_link) => WebPageBlock::ChatLink(chat_link.clone()),
            WebPageBlock::Audio {
                audio_file_id,
                caption,
            } => WebPageBlock::Audio {
                audio_file_id: *audio_file_id,
                caption: caption.clone_caption(context),
            },
            WebPageBlock::Kicker(kicker) => {
                WebPageBlock::Kicker(kicker.clone_rich_text(context, false))
            }
            WebPageBlock::Table(table) => WebPageBlock::Table(table.clone_table(context)),
            WebPageBlock::Details {
                header,
                page_blocks,
                is_open,
            } => WebPageBlock::Details {
                header: header.clone_rich_text(context, false),
                page_blocks: clone_web_page_blocks(page_blocks, context),
                is_open: *is_open,
            },
            WebPageBlock::RelatedArticles {
                header,
                related_articles,
            } => WebPageBlock::RelatedArticles {
                header: header.clone_rich_text(context, false),
                related_articles: related_articles.clone(),
            },
            WebPageBlock::Map {
                location,
                zoom,
                dimensions,
                caption,
            } => WebPageBlock::Map {
                location: *location,
                zoom: *zoom,
                dimensions: *dimensions,
                caption: caption.clone_caption(context),
            },
            WebPageBlock::VoiceNote {
                voice_note_file_id,
                caption,
            } => WebPageBlock::VoiceNote {
                voice_note_file_id: *voice_note_file_id,
                caption: caption.clone_caption(context),
            },
            WebPageBlock::Heading { text, size } => WebPageBlock::Heading {
                text: text.clone_rich_text(context, false),
                size: *size,
            },
            WebPageBlock::Math { source } => WebPageBlock::Math {
                source: source.clone(),
            },
            WebPageBlock::Thinking(text) => {
                WebPageBlock::Thinking(text.clone_rich_text(context, false))
            }
            WebPageBlock::BlockQuoteBlocks {
                page_blocks,
                caption,
            } => WebPageBlock::BlockQuoteBlocks {
                page_blocks: clone_web_page_blocks(page_blocks, context),
                caption: caption.clone_rich_text(context, false),
            },
            WebPageBlock::Document {
                document_file_id,
                caption,
            } => WebPageBlock::Document {
                document_file_id: *document_file_id,
                caption: caption.clone_caption(context),
            },
            WebPageBlock::ButtonRow(button_row) => {
                WebPageBlock::ButtonRow(button_row.clone_row(context))
            }
            WebPageBlock::ExpandableBlockQuote { text, credit } => {
                WebPageBlock::ExpandableBlockQuote {
                    text: text.clone_rich_text(context, false),
                    credit: credit.clone_rich_text(context, false),
                }
            }
            WebPageBlock::Unsupported { version } => {
                WebPageBlock::Unsupported { version: *version }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The wire PageBlock intake + the free functions
// (WebPageBlock.cpp:4886-5327, 5331-5385, 5495-5889)
// ---------------------------------------------------------------------------

/// `Document::append_file_ids`'s collapse for the typed file ids: the
/// thumbnail plane arrives with the Document record (M.28).
fn append_document_file_id(document_file_id: FileId, file_ids: &mut Vec<FileId>) {
    if document_file_id.is_valid() {
        file_ids.push(document_file_id);
    }
}

/// `for_each_text` (WebPageBlock.cpp:5331-5333).
pub fn for_each_text(page_block: &WebPageBlock, callback: &mut impl FnMut(&str)) {
    page_block.for_each_rich_text(false, &mut |text| callback(&text.get_full_text()));
}

/// `append_user_ids` (WebPageBlock.cpp:5335-5347).
pub fn append_user_ids(page_block: &WebPageBlock, user_ids: &mut Vec<UserId>) {
    page_block.for_each_rich_text(true, &mut |text| {
        if text.user_id.is_valid() {
            user_ids.push(text.user_id);
        }
        if let Some(button) = &text.button {
            let user_id = button.user_id;
            if user_id.is_valid() {
                user_ids.push(user_id);
            }
        }
    });
}

/// `has_bot_commands` (WebPageBlock.cpp:5349-5357).
pub fn has_bot_commands(page_block: &WebPageBlock) -> bool {
    let mut result = false;
    page_block.for_each_rich_text(true, &mut |text| {
        if text.r#type == RichTextType::BotCommand {
            result = true;
        }
    });
    result
}

/// `get_hashtags` (WebPageBlock.cpp:5359-5375).
pub fn get_hashtags(page_block: &WebPageBlock) -> Vec<String> {
    let mut result = Vec::new();
    page_block.for_each_rich_text(true, &mut |text| {
        if text.r#type == RichTextType::Hashtag {
            let mut hashtag = text.get_full_text();
            if !hashtag.is_empty() {
                if hashtag.starts_with('#') {
                    hashtag.remove(0);
                }
                if !hashtag.is_empty() {
                    result.push(hashtag);
                }
            }
        }
    });
    result
}

/// `get_custom_emoji_ids` (WebPageBlock.cpp:5377-5385).
pub fn get_custom_emoji_ids(page_block: &WebPageBlock) -> Vec<CustomEmojiId> {
    let mut custom_emoji_ids = Vec::new();
    page_block.for_each_rich_text(true, &mut |text| {
        if text.custom_emoji_id.is_valid() {
            custom_emoji_ids.push(text.custom_emoji_id);
        }
    });
    custom_emoji_ids
}

/// `get_web_page_blocks_index_mask` (WebPageBlock.cpp:5811-5817).
pub fn get_web_page_blocks_index_mask(page_blocks: &[WebPageBlock]) -> i32 {
    let mut index_mask = 0;
    for page_block in page_blocks {
        index_mask |= page_block.get_index_mask();
    }
    index_mask
}

/// `clone_web_page_blocks` (WebPageBlock.cpp:5819-5822).
pub fn clone_web_page_blocks(
    page_blocks: &[WebPageBlock],
    context: &mut CloneWebPageBlockContext,
) -> Vec<WebPageBlock> {
    page_blocks
        .iter()
        .map(|page_block| page_block.clone_block(context))
        .collect()
}

/// `for_each_web_page_blocks_quote_text` (WebPageBlock.cpp:4856-4861) —
/// the walker the `RichMessage::for_each_quote_text` quote plane runs on
/// (upstream ecb7fb163; the `RichMessage` method itself rides the
/// unported RichMessage class).
pub fn for_each_web_page_blocks_quote_text(
    page_blocks: &[WebPageBlock],
    callback: &mut impl FnMut(&RichText),
) {
    for page_block in page_blocks {
        page_block.for_each_quote_text(callback);
    }
}

/// `remove_empty_media_page_blocks` (WebPageBlock.cpp:4924-4928) — drops
/// the blocks whose `remove_empty_media_blocks` answers true (upstream
/// a7ccc2344); the recursive arms run first, like the C++ remove_if.
pub fn remove_empty_media_page_blocks(page_blocks: &mut Vec<WebPageBlock>) {
    let mut i = 0;
    while i < page_blocks.len() {
        if page_blocks[i].remove_empty_media_blocks() {
            page_blocks.remove(i);
        } else {
            i += 1;
        }
    }
}

/// `MessageQuote::get_rich_texts_object` (MessageQuote.cpp:246-251) — the
/// object build of the quote's rich texts: the C++ constructs the default
/// `GetWebPageBlockObjectContext` over Td with `skip_bot_commands_ =
/// true`; the quote-stripped rich texts only reach the env-free object
/// arms (Plain/Math/CustomEmoji/Concatenation and the basic formatting
/// nodes), so the env stands in as the no-op below.
pub fn get_quote_rich_texts_object(rich_texts: &[RichText]) -> Vec<Box<td_api::RichText>> {
    let mut env = NoObjectEnv;
    let mut user_manager = ();
    let mut context = WebPageBlockObjectContext {
        env: &mut env,
        base_url: String::new(),
        real_url_host: String::new(),
        real_url_rhash: String::new(),
        skip_bot_commands: true,
        is_first_pass: true,
        has_anchor_urls: false,
        anchors: std::collections::HashMap::new(),
        user_manager: &mut user_manager,
    };
    rich_texts
        .iter()
        .map(|rich_text| rich_text.get_rich_text_object(&mut context, false))
        .map(|object| object.expect("allow_null=false always answers"))
        .collect()
}

/// The binlog codec of a quote's rich texts — the C++
/// `td::store/parse(vector<RichText>)` inside MessageQuote.hpp over the
/// no-op env: the quote-stripped rich texts carry no Icon file ids (the
/// only env-armed store arm), so the env answers nothing. A corrupt row
/// with an icon zeroes out at parse (the no-op file id parses invalid)
/// instead of loading through the FileManager — the defensive divergence
/// on adversarial input only.
pub fn store_quote_rich_texts(rich_texts: &[RichText], out: &mut Vec<u8>) {
    let mut env = NoObjectEnv;
    store_i32(out, rich_texts.len() as i32);
    for rich_text in rich_texts {
        rich_text.store(&mut env, out);
    }
}

/// The parse twin of [`store_quote_rich_texts`].
pub fn parse_quote_rich_texts(parser: &mut Parser<'_>) -> Result<Vec<RichText>, &'static str> {
    let mut env = NoObjectEnv;
    let len = parser.i32_()?;
    if len < 0 {
        return Err("Invalid vector size");
    }
    let mut rich_texts = Vec::new();
    for _ in 0..len {
        rich_texts.push(RichText::parse(&mut env, parser)?);
    }
    Ok(rich_texts)
}

/// The no-op env behind [`get_quote_rich_texts_object`] — every method
/// answers the empty default; none is reachable for quote-stripped rich
/// texts (the env-armed arms — Icon/MentionName/Button and the
/// instant-view Url rewrite — are stripped from quotes).
struct NoObjectEnv;

impl crate::photo::PhotoStoreEnv for NoObjectEnv {
    fn store_file_id(&mut self, _file_id: FileId, _out: &mut Vec<u8>) {}
    fn parse_file_id(
        &mut self,
        _parser: &mut Parser<'_>,
    ) -> std::result::Result<FileId, &'static str> {
        Ok(FileId::default())
    }
}

impl WebPageBlockEnv for NoObjectEnv {
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
        _accent_color_id: crate::peer_color::AccentColorId,
        _fallback: crate::peer_color::AccentColorId,
    ) -> Option<i32> {
        None
    }
    fn get_chat_photo_info_object(&self, _photo: &DialogPhoto) -> Option<td_api::ChatPhotoInfo> {
        None
    }
    fn get_instant_view_link(&self, _url: &str, _rhash: &str) -> String {
        String::new()
    }
    fn get_instant_view_link_rhash(&self, _real_url: &str) -> String {
        String::new()
    }
    fn get_instant_view_link_url(&self, _real_url: &str) -> String {
        String::new()
    }
    fn as_input_document(
        &mut self,
        _file_id: FileId,
    ) -> Option<(i64, telegram_api::InputDocument)> {
        None
    }
    fn as_input_photo(&mut self, _file_id: FileId) -> Option<(i64, telegram_api::InputPhoto)> {
        None
    }
    fn ensure_input_user(&mut self, _user_id: crate::user_manager::UserId) -> Result<(), Status> {
        Ok(())
    }
    fn get_user_id_object(&mut self, _user_id: crate::user_manager::UserId, _source: &str) -> i64 {
        0
    }
    fn is_my_user_bot(&self) -> bool {
        false
    }
    fn on_get_page_block_channel(
        &mut self,
        _channel: telegram_api::channel,
    ) -> Option<ChatLinkInfo> {
        None
    }
}

/// `get_input_page_blocks` (WebPageBlock.cpp:5824-5829).
pub fn get_input_page_blocks(
    page_blocks: &[WebPageBlock],
    context: &mut GetInputPageBlockContext,
) -> Vec<Box<telegram_api::PageBlock>> {
    page_blocks
        .iter()
        .map(|page_block| Box::new(page_block.get_input_page_block(context)))
        .collect()
}

/// `get_page_blocks_object`'s single-pass inner transform
/// (WebPageBlock.cpp:81-86).
#[allow(clippy::vec_box)] // the generated td_api vector fields box their elements
fn get_page_blocks_object_vec(
    page_blocks: &[WebPageBlock],
    context: &mut WebPageBlockObjectContext,
) -> Vec<Box<td_api::PageBlock>> {
    page_blocks
        .iter()
        .map(|page_block| page_block.get_page_block_object(context))
        .collect()
}

/// `get_page_blocks_object` (WebPageBlock.cpp:5839-5861) — the two-pass
/// anchor resolution.
pub fn get_page_blocks_object(
    page_blocks: &[WebPageBlock],
    env: &mut dyn WebPageBlockEnv,
    user_manager: &mut dyn InlineKeyboardButtonEnv,
    base_url: &str,
    real_url: &str,
    skip_bot_commands: bool,
) -> Vec<Box<td_api::PageBlock>> {
    let mut real_url_rhash = env.get_instant_view_link_rhash(real_url);
    let mut real_url_host = String::new();
    if !real_url_rhash.is_empty() {
        real_url_host = tdutils::http_url::get_url_host(&env.get_instant_view_link_url(real_url));
        if real_url_host.is_empty() {
            real_url_rhash = String::new();
        }
    }
    let mut context = WebPageBlockObjectContext {
        env,
        base_url: base_url.to_owned(),
        real_url_host,
        real_url_rhash,
        skip_bot_commands,
        is_first_pass: true,
        has_anchor_urls: false,
        anchors: std::collections::HashMap::new(),
        user_manager,
    };
    let blocks = get_page_blocks_object_vec(page_blocks, &mut context);
    if !context.has_anchor_urls {
        return blocks;
    }

    context.is_first_pass = false;
    context.anchors.insert(String::new(), false); // back to top
    get_page_blocks_object_vec(page_blocks, &mut context)
}

/// `store_page_blocks` — the `td::store(vector<unique_ptr<WebPageBlock>>)`
/// framing (the size prefix, then `store_web_page_block` per element).
fn store_page_blocks(
    env: &mut impl WebPageBlockEnv,
    page_blocks: &[WebPageBlock],
    out: &mut Vec<u8>,
) {
    store_i32(out, page_blocks.len() as i32);
    for page_block in page_blocks {
        page_block.store(env, out);
    }
}

/// `parse_page_blocks` — the vector framing of [`store_page_blocks`].
fn parse_page_blocks(
    env: &mut impl WebPageBlockEnv,
    parser: &mut Parser<'_>,
) -> Result<Vec<WebPageBlock>, &'static str> {
    let len = parser.i32_()?;
    if len < 0 {
        return Err("Invalid vector size");
    }
    let mut result = Vec::with_capacity(len as usize);
    for _ in 0..len {
        result.push(WebPageBlock::parse_block(env, parser)?);
    }
    Ok(result)
}

/// `parse_related_articles` — the `td::parse(vector<RelatedArticle>)`
/// framing.
fn parse_related_articles(
    env: &mut impl WebPageBlockEnv,
    parser: &mut Parser<'_>,
) -> Result<Vec<RelatedArticle>, &'static str> {
    let len = parser.i32_()?;
    if len < 0 {
        return Err("Invalid vector size");
    }
    let mut result = Vec::with_capacity(len as usize);
    for _ in 0..len {
        result.push(RelatedArticle::parse(env, parser)?);
    }
    Ok(result)
}

impl WebPageBlock {
    /// `WebPageBlock::store` (WebPageBlock.cpp:5470-5475) — the type
    /// discriminant first, then the per-class body.
    pub fn store(&self, env: &mut impl WebPageBlockEnv, out: &mut Vec<u8>) {
        store_i32(out, self.get_type() as i32);
        match self {
            WebPageBlock::Unsupported { version } => {
                store_flags(out, []);
                store_i32(out, *version);
            }
            WebPageBlock::Title(title) => title.store(env, out),
            WebPageBlock::Subtitle(subtitle) => subtitle.store(env, out),
            WebPageBlock::AuthorDate { author, date } => {
                author.store(env, out);
                store_i32(out, *date);
            }
            WebPageBlock::Header(header) => header.store(env, out),
            WebPageBlock::Subheader(subheader) => subheader.store(env, out),
            WebPageBlock::Heading { text, size } => {
                store_flags(out, []);
                text.store(env, out);
                store_i32(out, *size);
            }
            WebPageBlock::Paragraph(text) => text.store(env, out),
            WebPageBlock::Preformatted { text, language } => {
                text.store(env, out);
                store_string(out, language);
            }
            WebPageBlock::Footer(footer) => footer.store(env, out),
            WebPageBlock::Kicker(kicker) => kicker.store(env, out),
            WebPageBlock::Thinking(text) => {
                store_flags(out, []);
                text.store(env, out);
            }
            WebPageBlock::Divider => {}
            WebPageBlock::Math { source } => {
                store_flags(out, []);
                store_string(out, source);
            }
            WebPageBlock::Anchor { name } => store_string(out, name),
            WebPageBlock::List(list) => list.store(env, out),
            WebPageBlock::ButtonRow(button_row) => button_row.store(env, out),
            WebPageBlock::BlockQuote { text, credit } => {
                text.store(env, out);
                credit.store(env, out);
            }
            WebPageBlock::ExpandableBlockQuote { text, credit } => {
                store_flags(out, []);
                text.store(env, out);
                credit.store(env, out);
            }
            WebPageBlock::PullQuote { text, credit } => {
                text.store(env, out);
                credit.store(env, out);
            }
            WebPageBlock::Animation {
                animation_file_id,
                caption,
                need_autoplay,
                has_spoiler,
            } => {
                let has_empty_animation = !animation_file_id.is_valid();
                store_flags(out, [*need_autoplay, has_empty_animation, *has_spoiler]);
                if !has_empty_animation {
                    // the animations_manager_->store_animation framing
                    env.store_file_id(*animation_file_id, out);
                }
                caption.store(env, out);
            }
            WebPageBlock::Photo {
                photo,
                caption,
                url,
                web_page_id,
                has_spoiler,
            } => {
                store_flags(out, [*has_spoiler]);
                store_photo(env, out, photo);
                caption.store(env, out);
                store_string(out, url);
                web_page_id.store(out);
            }
            WebPageBlock::Video {
                video_file_id,
                caption,
                need_autoplay,
                is_looped,
                has_spoiler,
            } => {
                let has_empty_video = !video_file_id.is_valid();
                store_flags(
                    out,
                    [*need_autoplay, *is_looped, has_empty_video, *has_spoiler],
                );
                if !has_empty_video {
                    // the videos_manager_->store_video framing
                    env.store_file_id(*video_file_id, out);
                }
                caption.store(env, out);
            }
            WebPageBlock::Cover(cover) => cover.store(env, out),
            WebPageBlock::Embedded {
                url,
                html,
                poster_photo,
                dimensions,
                caption,
                is_full_width,
                allow_scrolling,
            } => {
                store_flags(out, [*is_full_width, *allow_scrolling]);
                store_string(out, url);
                store_string(out, html);
                store_photo(env, out, poster_photo);
                store_dimensions(out, *dimensions);
                caption.store(env, out);
            }
            WebPageBlock::EmbeddedPost {
                url,
                author,
                author_photo,
                date,
                page_blocks,
                caption,
            } => {
                store_string(out, url);
                store_string(out, author);
                store_photo(env, out, author_photo);
                store_i32(out, *date);
                store_page_blocks(env, page_blocks, out);
                caption.store(env, out);
            }
            WebPageBlock::Collage {
                page_blocks,
                caption,
            } => {
                store_page_blocks(env, page_blocks, out);
                caption.store(env, out);
            }
            WebPageBlock::Slideshow {
                page_blocks,
                caption,
            } => {
                store_page_blocks(env, page_blocks, out);
                caption.store(env, out);
            }
            WebPageBlock::ChatLink(chat_link) => {
                let has_title = !chat_link.title.is_empty();
                let has_photo = chat_link.photo.small_file_id.is_valid();
                let has_username = !chat_link.username.is_empty();
                let has_accent_color_id = true;
                let has_channel_id = chat_link.channel_id.is_valid();
                store_flags(
                    out,
                    [
                        has_title,
                        has_photo,
                        has_username,
                        has_accent_color_id,
                        has_channel_id,
                    ],
                );
                if has_title {
                    store_string(out, &chat_link.title);
                }
                if has_photo {
                    // the DialogPhoto persistence plane rides the same codec
                    crate::photo::store_dialog_photo(env, out, &chat_link.photo);
                }
                if has_username {
                    store_string(out, &chat_link.username);
                }
                store_i32(out, chat_link.accent_color_id.get());
                if has_channel_id {
                    store_i64(out, chat_link.channel_id.get());
                }
            }
            WebPageBlock::Audio {
                audio_file_id,
                caption,
            } => {
                let has_empty_audio = !audio_file_id.is_valid();
                let is_voice_note_repaired = true;
                store_flags(out, [has_empty_audio, is_voice_note_repaired]);
                if !has_empty_audio {
                    // the audios_manager_->store_audio framing
                    env.store_file_id(*audio_file_id, out);
                }
                caption.store(env, out);
            }
            WebPageBlock::Table(table) => table.store(env, out),
            WebPageBlock::Details {
                header,
                page_blocks,
                is_open,
            } => {
                store_flags(out, [*is_open]);
                header.store(env, out);
                store_page_blocks(env, page_blocks, out);
            }
            WebPageBlock::RelatedArticles {
                header,
                related_articles,
            } => {
                header.store(env, out);
                store_i32(out, related_articles.len() as i32);
                for article in related_articles {
                    article.store(env, out);
                }
            }
            WebPageBlock::Map {
                location,
                zoom,
                dimensions,
                caption,
            } => {
                location.store(out);
                store_i32(out, *zoom);
                store_dimensions(out, *dimensions);
                caption.store(env, out);
            }
            WebPageBlock::VoiceNote {
                voice_note_file_id,
                caption,
            } => {
                let has_empty_voice_note = !voice_note_file_id.is_valid();
                store_flags(out, [has_empty_voice_note]);
                if !has_empty_voice_note {
                    // the voice_notes_manager_->store_voice_note framing
                    env.store_file_id(*voice_note_file_id, out);
                }
                caption.store(env, out);
            }
            WebPageBlock::BlockQuoteBlocks {
                page_blocks,
                caption,
            } => {
                store_flags(out, []);
                caption.store(env, out);
                store_page_blocks(env, page_blocks, out);
            }
            WebPageBlock::Document {
                document_file_id,
                caption,
            } => {
                let has_document = document_file_id.is_valid();
                store_flags(out, [has_document]);
                if has_document {
                    // the documents_manager_->store_document framing
                    env.store_file_id(*document_file_id, out);
                }
                caption.store(env, out);
            }
        }
    }

    /// `WebPageBlock::parse` (WebPageBlock.cpp:5477-5493 + call_impl) —
    /// the per-class bodies in the C++ arm order.
    pub fn parse_block(
        env: &mut impl WebPageBlockEnv,
        parser: &mut Parser<'_>,
    ) -> Result<WebPageBlock, &'static str> {
        let type_ = parser.i32_()?;
        let block_type = BlockType::try_from(type_)
            .map_err(|_| -> &'static str { "Can't parse unknown BlockType" })?;
        match block_type {
            BlockType::Title => Ok(WebPageBlock::Title(RichText::parse(env, parser)?)),
            BlockType::Subtitle => Ok(WebPageBlock::Subtitle(RichText::parse(env, parser)?)),
            BlockType::AuthorDate => Ok(WebPageBlock::AuthorDate {
                author: RichText::parse(env, parser)?,
                date: parser.i32_()?,
            }),
            BlockType::Header => Ok(WebPageBlock::Header(RichText::parse(env, parser)?)),
            BlockType::Subheader => Ok(WebPageBlock::Subheader(RichText::parse(env, parser)?)),
            BlockType::Kicker => Ok(WebPageBlock::Kicker(RichText::parse(env, parser)?)),
            BlockType::Paragraph => Ok(WebPageBlock::Paragraph(RichText::parse(env, parser)?)),
            BlockType::Preformatted => Ok(WebPageBlock::Preformatted {
                text: RichText::parse(env, parser)?,
                language: parser.string()?,
            }),
            BlockType::Footer => Ok(WebPageBlock::Footer(RichText::parse(env, parser)?)),
            BlockType::Divider => Ok(WebPageBlock::Divider),
            BlockType::Anchor => Ok(WebPageBlock::Anchor {
                name: parser.string()?,
            }),
            BlockType::List => Ok(WebPageBlock::List(WebPageBlockList::parse(env, parser)?)),
            BlockType::BlockQuote => Ok(WebPageBlock::BlockQuote {
                text: RichText::parse(env, parser)?,
                credit: RichText::parse(env, parser)?,
            }),
            BlockType::PullQuote => Ok(WebPageBlock::PullQuote {
                text: RichText::parse(env, parser)?,
                credit: RichText::parse(env, parser)?,
            }),
            BlockType::Animation => {
                let [need_autoplay, has_empty_animation, has_spoiler] = parser.parse_flags()?;
                let animation_file_id =
                    if parser.version() >= version::FIX_WEB_PAGE_INSTANT_VIEW_DATABASE {
                        if !has_empty_animation {
                            env.parse_file_id(parser)?
                        } else {
                            FileId::default()
                        }
                    } else {
                        return Err("Wrong stored object");
                    };
                let caption = WebPageBlockCaption::parse(env, parser)?;
                Ok(WebPageBlock::Animation {
                    animation_file_id,
                    caption,
                    need_autoplay,
                    has_spoiler,
                })
            }
            BlockType::Photo => {
                let has_spoiler = if parser.version() >= version::SUPPORT_RICH_MESSAGES {
                    let [has_spoiler] = parser.parse_flags()?;
                    has_spoiler
                } else {
                    false
                };
                let photo = parse_photo(env, parser)?;
                let caption = WebPageBlockCaption::parse(env, parser)?;
                if parser.version() >= version::SUPPORT_INSTANT_VIEW_2_0 {
                    let url = parser.string()?;
                    let web_page_id = WebPageId::parse(parser)?;
                    Ok(WebPageBlock::Photo {
                        photo,
                        caption,
                        url,
                        web_page_id,
                        has_spoiler,
                    })
                } else {
                    Ok(WebPageBlock::Photo {
                        photo,
                        caption,
                        url: String::new(),
                        web_page_id: WebPageId::default(),
                        has_spoiler,
                    })
                }
            }
            BlockType::Video => {
                let [need_autoplay, is_looped, has_empty_video, has_spoiler] =
                    parser.parse_flags()?;
                let video_file_id =
                    if parser.version() >= version::FIX_WEB_PAGE_INSTANT_VIEW_DATABASE {
                        if !has_empty_video {
                            env.parse_file_id(parser)?
                        } else {
                            FileId::default()
                        }
                    } else {
                        return Err("Wrong stored object");
                    };
                let caption = WebPageBlockCaption::parse(env, parser)?;
                Ok(WebPageBlock::Video {
                    video_file_id,
                    caption,
                    need_autoplay,
                    is_looped,
                    has_spoiler,
                })
            }
            BlockType::Cover => Ok(WebPageBlock::Cover(Box::new(WebPageBlock::parse_block(
                env, parser,
            )?))),
            BlockType::Embedded => {
                let [is_full_width, allow_scrolling] = parser.parse_flags()?;
                let url = parser.string()?;
                let html = parser.string()?;
                let poster_photo = parse_photo(env, parser)?;
                let dimensions = parse_dimensions(parser)?;
                let caption = WebPageBlockCaption::parse(env, parser)?;
                Ok(WebPageBlock::Embedded {
                    url,
                    html,
                    poster_photo,
                    dimensions,
                    caption,
                    is_full_width,
                    allow_scrolling,
                })
            }
            BlockType::EmbeddedPost => {
                let url = parser.string()?;
                let author = parser.string()?;
                let author_photo = parse_photo(env, parser)?;
                let date = parser.i32_()?;
                let page_blocks = parse_page_blocks(env, parser)?;
                let caption = WebPageBlockCaption::parse(env, parser)?;
                Ok(WebPageBlock::EmbeddedPost {
                    url,
                    author,
                    author_photo,
                    date: date.max(0),
                    page_blocks,
                    caption,
                })
            }
            BlockType::Collage => Ok(WebPageBlock::Collage {
                page_blocks: parse_page_blocks(env, parser)?,
                caption: WebPageBlockCaption::parse(env, parser)?,
            }),
            BlockType::Slideshow => Ok(WebPageBlock::Slideshow {
                page_blocks: parse_page_blocks(env, parser)?,
                caption: WebPageBlockCaption::parse(env, parser)?,
            }),
            BlockType::ChatLink => {
                let [
                    has_title,
                    has_photo,
                    has_username,
                    has_accent_color_id,
                    has_channel_id,
                ] = if parser.version() >= version::ADD_PAGE_BLOCK_CHAT_LINK_FLAGS {
                    parser.parse_flags()?
                } else {
                    [true, true, true, false, false]
                };
                let mut chat_link = ChatLinkInfo::default();
                if has_title {
                    chat_link.title = parser.string()?;
                }
                if has_photo {
                    chat_link.photo = crate::photo::parse_dialog_photo(env, parser)?;
                }
                if has_username {
                    chat_link.username = parser.string()?;
                }
                chat_link.accent_color_id = if has_accent_color_id {
                    AccentColorId::new(parser.i32_()?)
                } else {
                    AccentColorId::new(5) // blue
                };
                chat_link.channel_id = if has_channel_id {
                    crate::chat_manager::ChannelId::new(parser.i64_()?)
                } else {
                    crate::chat_manager::ChannelId::new(5) // blue
                };
                Ok(WebPageBlock::ChatLink(chat_link))
            }
            BlockType::Audio => {
                let pre_flags = parser.version() >= version::FIX_PAGE_BLOCK_AUDIO_EMPTY_FILE;
                let [has_empty_audio, is_voice_note_repaired] = if pre_flags {
                    parser.parse_flags()?
                } else {
                    [false, false]
                };
                let audio_file_id = if !has_empty_audio {
                    env.parse_file_id(parser)?
                } else {
                    if !is_voice_note_repaired {
                        return Err("Trying to repair WebPageBlockVoiceNote");
                    }
                    FileId::default()
                };
                let caption = WebPageBlockCaption::parse(env, parser)?;
                Ok(WebPageBlock::Audio {
                    audio_file_id,
                    caption,
                })
            }
            BlockType::Table => Ok(WebPageBlock::Table(WebPageBlockTable::parse(env, parser)?)),
            BlockType::Details => {
                let [is_open] = parser.parse_flags()?;
                let header = RichText::parse(env, parser)?;
                let page_blocks = parse_page_blocks(env, parser)?;
                Ok(WebPageBlock::Details {
                    header,
                    page_blocks,
                    is_open,
                })
            }
            BlockType::RelatedArticles => Ok(WebPageBlock::RelatedArticles {
                header: RichText::parse(env, parser)?,
                related_articles: parse_related_articles(env, parser)?,
            }),
            BlockType::Map => {
                let location = crate::chat_manager::Location::parse_impl(parser)?;
                let zoom = parser.i32_()?;
                let dimensions = parse_dimensions(parser)?;
                let caption = WebPageBlockCaption::parse(env, parser)?;
                Ok(WebPageBlock::Map {
                    location,
                    zoom,
                    dimensions,
                    caption,
                })
            }
            BlockType::VoiceNote => {
                let [has_empty_voice_note] = parser.parse_flags()?;
                let voice_note_file_id = if !has_empty_voice_note {
                    env.parse_file_id(parser)?
                } else {
                    FileId::default()
                };
                let caption = WebPageBlockCaption::parse(env, parser)?;
                Ok(WebPageBlock::VoiceNote {
                    voice_note_file_id,
                    caption,
                })
            }
            BlockType::Heading => {
                let _empty_flags = parser.parse_flags::<0>()?;
                let text = RichText::parse(env, parser)?;
                let size = parser.i32_()?;
                Ok(WebPageBlock::Heading {
                    text,
                    size: size.clamp(1, 6),
                })
            }
            BlockType::Math => {
                let _empty_flags = parser.parse_flags::<0>()?;
                Ok(WebPageBlock::Math {
                    source: parser.string()?,
                })
            }
            BlockType::Thinking => {
                let _empty_flags = parser.parse_flags::<0>()?;
                Ok(WebPageBlock::Thinking(RichText::parse(env, parser)?))
            }
            BlockType::BlockQuoteBlocks => {
                let _empty_flags = parser.parse_flags::<0>()?;
                let caption = RichText::parse(env, parser)?;
                let page_blocks = parse_page_blocks(env, parser)?;
                Ok(WebPageBlock::BlockQuoteBlocks {
                    page_blocks,
                    caption,
                })
            }
            BlockType::Document => {
                let [has_document] = parser.parse_flags()?;
                let document_file_id = if has_document {
                    env.parse_file_id(parser)?
                } else {
                    FileId::default()
                };
                let caption = WebPageBlockCaption::parse(env, parser)?;
                Ok(WebPageBlock::Document {
                    document_file_id,
                    caption,
                })
            }
            BlockType::ButtonRow => Ok(WebPageBlock::ButtonRow(WebPageBlockButtonRow::parse(
                env, parser,
            )?)),
            BlockType::ExpandableBlockQuote => {
                let _empty_flags = parser.parse_flags::<0>()?;
                let text = RichText::parse(env, parser)?;
                let credit = RichText::parse(env, parser)?;
                Ok(WebPageBlock::ExpandableBlockQuote { text, credit })
            }
            BlockType::Unsupported => {
                let _empty_flags = parser.parse_flags::<0>()?;
                let version_ = parser.i32_()?;
                Ok(WebPageBlock::Unsupported { version: version_ })
            }
            // excluded by the BlockType::try_from range check
            BlockType::Size => unreachable!("Size is not a valid block type"),
        }
    }
}

/// `get_page_block_object` (WebPageBlock.h:160 + overrides) — the giant
/// td_api builder.
impl WebPageBlock {
    pub fn get_page_block_object(
        &self,
        context: &mut WebPageBlockObjectContext,
    ) -> Box<td_api::PageBlock> {
        let object = match self {
            WebPageBlock::Unsupported { .. } => {
                td_api::PageBlock::pageBlockUnsupported(td_api::pageBlockUnsupported {})
            }
            WebPageBlock::Title(title) => {
                td_api::PageBlock::pageBlockTitle(td_api::pageBlockTitle {
                    title: title.get_rich_text_object(context, false),
                })
            }
            WebPageBlock::Subtitle(subtitle) => {
                td_api::PageBlock::pageBlockSubtitle(td_api::pageBlockSubtitle {
                    subtitle: subtitle.get_rich_text_object(context, false),
                })
            }
            WebPageBlock::AuthorDate { author, date } => {
                td_api::PageBlock::pageBlockAuthorDate(td_api::pageBlockAuthorDate {
                    author: author.get_rich_text_object(context, false),
                    publish_date: *date,
                })
            }
            WebPageBlock::Header(header) => {
                td_api::PageBlock::pageBlockHeader(td_api::pageBlockHeader {
                    header: header.get_rich_text_object(context, false),
                })
            }
            WebPageBlock::Subheader(subheader) => {
                td_api::PageBlock::pageBlockSubheader(td_api::pageBlockSubheader {
                    subheader: subheader.get_rich_text_object(context, false),
                })
            }
            WebPageBlock::Heading { text, size } => {
                td_api::PageBlock::pageBlockSectionHeading(td_api::pageBlockSectionHeading {
                    text: text.get_rich_text_object(context, false),
                    size: *size,
                })
            }
            WebPageBlock::Kicker(kicker) => {
                td_api::PageBlock::pageBlockKicker(td_api::pageBlockKicker {
                    kicker: kicker.get_rich_text_object(context, false),
                })
            }
            WebPageBlock::Paragraph(text) => {
                td_api::PageBlock::pageBlockParagraph(td_api::pageBlockParagraph {
                    text: text.get_rich_text_object(context, false),
                })
            }
            WebPageBlock::Preformatted { text, language } => {
                td_api::PageBlock::pageBlockPreformatted(td_api::pageBlockPreformatted {
                    text: text.get_rich_text_object(context, false),
                    language: language.clone(),
                })
            }
            WebPageBlock::Footer(footer) => {
                td_api::PageBlock::pageBlockFooter(td_api::pageBlockFooter {
                    footer: footer.get_rich_text_object(context, false),
                })
            }
            WebPageBlock::Divider => {
                td_api::PageBlock::pageBlockDivider(td_api::pageBlockDivider {})
            }
            WebPageBlock::Math { source } => td_api::PageBlock::pageBlockMathematicalExpression(
                td_api::pageBlockMathematicalExpression {
                    expression: source.clone(),
                },
            ),
            WebPageBlock::Thinking(text) => {
                td_api::PageBlock::pageBlockThinking(td_api::pageBlockThinking {
                    text: text.get_rich_text_object(context, false),
                })
            }
            WebPageBlock::Anchor { name } => {
                if context.is_first_pass {
                    context.anchors.entry(name.clone()).or_insert(false);
                }
                td_api::PageBlock::pageBlockAnchor(td_api::pageBlockAnchor { name: name.clone() })
            }
            WebPageBlock::List(list) => td_api::PageBlock::pageBlockList(td_api::pageBlockList {
                items: list
                    .items
                    .iter()
                    .map(|item| Box::new(item.get_page_block_list_item_object(context)))
                    .collect(),
            }),
            WebPageBlock::BlockQuote { text, credit } => {
                let blocks: Vec<Box<td_api::PageBlock>> = vec![Box::new(
                    td_api::PageBlock::pageBlockParagraph(td_api::pageBlockParagraph {
                        text: text.get_rich_text_object(context, false),
                    }),
                )];
                td_api::PageBlock::pageBlockBlockQuote(td_api::pageBlockBlockQuote {
                    blocks,
                    credit: credit.get_rich_text_object(context, true),
                })
            }
            WebPageBlock::ExpandableBlockQuote { text, credit } => {
                td_api::PageBlock::pageBlockExpandableBlockQuote(
                    td_api::pageBlockExpandableBlockQuote {
                        text: text.get_rich_text_object(context, false),
                        credit: credit.get_rich_text_object(context, true),
                    },
                )
            }
            WebPageBlock::PullQuote { text, credit } => {
                td_api::PageBlock::pageBlockPullQuote(td_api::pageBlockPullQuote {
                    text: text.get_rich_text_object(context, false),
                    credit: credit.get_rich_text_object(context, true),
                })
            }
            WebPageBlock::Animation {
                animation_file_id,
                caption,
                need_autoplay,
                has_spoiler,
            } => td_api::PageBlock::pageBlockAnimation(td_api::pageBlockAnimation {
                animation: context.env.get_animation_object(*animation_file_id),
                caption: caption.get_page_block_caption_object(context),
                need_autoplay: *need_autoplay,
                has_spoiler: *has_spoiler,
            }),
            WebPageBlock::Photo {
                photo,
                caption,
                url,
                has_spoiler,
                ..
            } => td_api::PageBlock::pageBlockPhoto(td_api::pageBlockPhoto {
                photo: crate::photo::get_photo_object(photo),
                caption: caption.get_page_block_caption_object(context),
                url: url.clone(),
                has_spoiler: *has_spoiler,
            }),
            WebPageBlock::Video {
                video_file_id,
                caption,
                need_autoplay,
                is_looped,
                has_spoiler,
            } => td_api::PageBlock::pageBlockVideo(td_api::pageBlockVideo {
                video: context.env.get_video_object(*video_file_id),
                caption: caption.get_page_block_caption_object(context),
                need_autoplay: *need_autoplay,
                is_looped: *is_looped,
                has_spoiler: *has_spoiler,
            }),
            WebPageBlock::Cover(cover) => {
                td_api::PageBlock::pageBlockCover(td_api::pageBlockCover {
                    cover: Some(cover.get_page_block_object(context)),
                })
            }
            WebPageBlock::Embedded {
                url,
                html,
                poster_photo,
                dimensions,
                caption,
                is_full_width,
                allow_scrolling,
            } => td_api::PageBlock::pageBlockEmbedded(td_api::pageBlockEmbedded {
                url: url.clone(),
                html: html.clone(),
                poster_photo: crate::photo::get_photo_object(poster_photo),
                width: dimensions.width as i32,
                height: dimensions.height as i32,
                caption: caption.get_page_block_caption_object(context),
                is_full_width: *is_full_width,
                allow_scrolling: *allow_scrolling,
            }),
            WebPageBlock::EmbeddedPost {
                url,
                author,
                author_photo,
                date,
                page_blocks,
                caption,
            } => td_api::PageBlock::pageBlockEmbeddedPost(td_api::pageBlockEmbeddedPost {
                url: url.clone(),
                author: author.clone(),
                author_photo: crate::photo::get_photo_object(author_photo),
                date: *date,
                blocks: get_page_blocks_object_vec(page_blocks, context),
                caption: caption.get_page_block_caption_object(context),
            }),
            WebPageBlock::Collage {
                page_blocks,
                caption,
            } => td_api::PageBlock::pageBlockCollage(td_api::pageBlockCollage {
                blocks: get_page_blocks_object_vec(page_blocks, context),
                caption: caption.get_page_block_caption_object(context),
            }),
            WebPageBlock::Slideshow {
                page_blocks,
                caption,
            } => td_api::PageBlock::pageBlockSlideshow(td_api::pageBlockSlideshow {
                blocks: get_page_blocks_object_vec(page_blocks, context),
                caption: caption.get_page_block_caption_object(context),
            }),
            WebPageBlock::ChatLink(chat_link) => {
                td_api::PageBlock::pageBlockChatLink(td_api::pageBlockChatLink {
                    title: chat_link.title.clone(),
                    photo: context.env.get_chat_photo_info_object(&chat_link.photo),
                    accent_color_id: context
                        .env
                        .get_accent_color_id_object(
                            chat_link.accent_color_id,
                            AccentColorId::from_peer_id(chat_link.channel_id.get()),
                        )
                        .unwrap_or(0),
                    username: chat_link.username.clone(),
                })
            }
            WebPageBlock::Audio {
                audio_file_id,
                caption,
            } => {
                let audio_object = context.env.get_audio_object(*audio_file_id);
                if audio_object.is_none() {
                    return Box::new(td_api::PageBlock::pageBlockUnsupported(
                        td_api::pageBlockUnsupported {},
                    ));
                }
                td_api::PageBlock::pageBlockAudio(td_api::pageBlockAudio {
                    audio: audio_object,
                    caption: caption.get_page_block_caption_object(context),
                })
            }
            WebPageBlock::Table(table) => {
                let cell_objects = table
                    .cells
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|cell| cell.get_page_block_table_cell_object(context))
                            .collect()
                    })
                    .collect();
                td_api::PageBlock::pageBlockTable(td_api::pageBlockTable {
                    caption: table.title.get_rich_text_object(context, true),
                    cells: cell_objects,
                    is_bordered: table.is_bordered,
                    is_striped: table.is_striped,
                    is_compact: table.is_compact,
                })
            }
            WebPageBlock::Details {
                header,
                page_blocks,
                is_open,
            } => td_api::PageBlock::pageBlockDetails(td_api::pageBlockDetails {
                header: header.get_rich_text_object(context, false),
                blocks: get_page_blocks_object_vec(page_blocks, context),
                is_open: *is_open,
            }),
            WebPageBlock::BlockQuoteBlocks {
                page_blocks,
                caption,
            } => td_api::PageBlock::pageBlockBlockQuote(td_api::pageBlockBlockQuote {
                blocks: get_page_blocks_object_vec(page_blocks, context),
                credit: caption.get_rich_text_object(context, true),
            }),
            WebPageBlock::RelatedArticles {
                header,
                related_articles,
            } => td_api::PageBlock::pageBlockRelatedArticles(td_api::pageBlockRelatedArticles {
                header: header.get_rich_text_object(context, false),
                articles: related_articles
                    .iter()
                    .map(|article| article.get_page_block_related_article_object())
                    .collect(),
            }),
            WebPageBlock::Map {
                location,
                zoom,
                dimensions,
                caption,
            } => td_api::PageBlock::pageBlockMap(td_api::pageBlockMap {
                location: location.get_location_object(),
                zoom: *zoom,
                width: dimensions.width as i32,
                height: dimensions.height as i32,
                caption: caption.get_page_block_caption_object(context),
            }),
            WebPageBlock::VoiceNote {
                voice_note_file_id,
                caption,
            } => {
                let voice_note_object = context.env.get_voice_note_object(*voice_note_file_id);
                if voice_note_object.is_none() {
                    return Box::new(td_api::PageBlock::pageBlockUnsupported(
                        td_api::pageBlockUnsupported {},
                    ));
                }
                td_api::PageBlock::pageBlockVoiceNote(td_api::pageBlockVoiceNote {
                    voice_note: voice_note_object,
                    caption: caption.get_page_block_caption_object(context),
                })
            }
            WebPageBlock::Document {
                document_file_id,
                caption,
            } => {
                let document_object = context.env.get_document_object(*document_file_id);
                if document_object.is_none() {
                    return Box::new(td_api::PageBlock::pageBlockUnsupported(
                        td_api::pageBlockUnsupported {},
                    ));
                }
                td_api::PageBlock::pageBlockDocument(td_api::pageBlockDocument {
                    document: document_object,
                    caption: caption.get_page_block_caption_object(context),
                })
            }
            WebPageBlock::ButtonRow(button_row) => {
                let align = if button_row.align_left {
                    Some(
                        td_api::PageBlockHorizontalAlignment::pageBlockHorizontalAlignmentLeft(
                            td_api::pageBlockHorizontalAlignmentLeft {},
                        ),
                    )
                } else if button_row.align_center {
                    Some(
                        td_api::PageBlockHorizontalAlignment::pageBlockHorizontalAlignmentCenter(
                            td_api::pageBlockHorizontalAlignmentCenter {},
                        ),
                    )
                } else if button_row.align_right {
                    Some(
                        td_api::PageBlockHorizontalAlignment::pageBlockHorizontalAlignmentRight(
                            td_api::pageBlockHorizontalAlignmentRight {},
                        ),
                    )
                } else {
                    None
                };
                td_api::PageBlock::pageBlockButtonRow(td_api::pageBlockButtonRow {
                    buttons: button_row
                        .buttons
                        .iter()
                        .map(|button| {
                            let button_object = button
                                .button
                                .get_inline_keyboard_button_object(Some(&*context.user_manager));
                            Box::new(td_api::InlineButton {
                                text: button.text.get_rich_text_object(context, false),
                                style: button.style.get_button_style_object(),
                                r#type: button_object.r#type,
                            })
                        })
                        .collect(),
                    align,
                })
            }
        };
        Box::new(object)
    }

    /// `get_input_page_block` (WebPageBlock.h:158 + overrides). The media
    /// arms consume the RichMessageMedia context (`[blocked: M.9]`) and
    /// answer the C++ divider fall-back.
    pub fn get_input_page_block(
        &self,
        context: &mut GetInputPageBlockContext,
    ) -> telegram_api::PageBlock {
        match self {
            WebPageBlock::Title(text) => {
                telegram_api::PageBlock::pageBlockTitle(telegram_api::pageBlockTitle {
                    text: Some(Box::new(text.get_input_rich_text(context))),
                })
            }
            WebPageBlock::Subtitle(text) => {
                telegram_api::PageBlock::pageBlockSubtitle(telegram_api::pageBlockSubtitle {
                    text: Some(Box::new(text.get_input_rich_text(context))),
                })
            }
            WebPageBlock::AuthorDate { author, date } => {
                telegram_api::PageBlock::pageBlockAuthorDate(telegram_api::pageBlockAuthorDate {
                    author: Some(Box::new(author.get_input_rich_text(context))),
                    published_date: *date,
                })
            }
            WebPageBlock::Header(text) => {
                telegram_api::PageBlock::pageBlockHeader(telegram_api::pageBlockHeader {
                    text: Some(Box::new(text.get_input_rich_text(context))),
                })
            }
            WebPageBlock::Subheader(text) => {
                telegram_api::PageBlock::pageBlockSubheader(telegram_api::pageBlockSubheader {
                    text: Some(Box::new(text.get_input_rich_text(context))),
                })
            }
            WebPageBlock::Paragraph(text) => {
                telegram_api::PageBlock::pageBlockParagraph(telegram_api::pageBlockParagraph {
                    text: Some(Box::new(text.get_input_rich_text(context))),
                })
            }
            WebPageBlock::Preformatted { text, language } => {
                telegram_api::PageBlock::pageBlockPreformatted(
                    telegram_api::pageBlockPreformatted {
                        text: Some(Box::new(text.get_input_rich_text(context))),
                        language: language.clone(),
                    },
                )
            }
            WebPageBlock::Footer(text) => {
                telegram_api::PageBlock::pageBlockFooter(telegram_api::pageBlockFooter {
                    text: Some(Box::new(text.get_input_rich_text(context))),
                })
            }
            WebPageBlock::Kicker(kicker) => {
                telegram_api::PageBlock::pageBlockKicker(telegram_api::pageBlockKicker {
                    text: Some(Box::new(kicker.get_input_rich_text(context))),
                })
            }
            WebPageBlock::Divider => {
                telegram_api::PageBlock::pageBlockDivider(telegram_api::pageBlockDivider {})
            }
            WebPageBlock::Anchor { name } => {
                telegram_api::PageBlock::pageBlockAnchor(telegram_api::pageBlockAnchor {
                    name: name.clone(),
                })
            }
            WebPageBlock::List(list) => {
                if !list.items.is_empty() && !list.items[0].label.is_empty() {
                    // pageBlockOrderedList::START_MASK | TYPE_MASK
                    let flags = (1 << 0) | (1 << 1);
                    telegram_api::PageBlock::pageBlockOrderedList(
                        telegram_api::pageBlockOrderedList {
                            flags,
                            reversed: list.is_reversed,
                            items: list
                                .items
                                .iter()
                                .map(|item| {
                                    Box::new(telegram_api::PageListOrderedItem::
                                        pageListOrderedItemBlocks(
                                            telegram_api::pageListOrderedItemBlocks {
                                                flags: (1 << 2) | (1 << 3) | (1 << 4),
                                                checkbox: item.has_checkbox,
                                                checked: item.has_checkbox && item.is_checked,
                                                num: item.label.clone(),
                                                blocks: get_input_page_blocks(
                                                    &item.page_blocks,
                                                    context,
                                                ),
                                                value: item.value,
                                                r#type: item.r#type.clone(),
                                            },
                                        ))
                                })
                                .collect(),
                            start: list.start,
                            r#type: list.r#type.clone(),
                        },
                    )
                } else {
                    telegram_api::PageBlock::pageBlockList(telegram_api::pageBlockList {
                        items: list
                            .items
                            .iter()
                            .map(|item| {
                                Box::new(telegram_api::PageListItem::pageListItemBlocks(
                                    telegram_api::pageListItemBlocks {
                                        flags: 0,
                                        checkbox: item.has_checkbox,
                                        checked: item.has_checkbox && item.is_checked,
                                        blocks: get_input_page_blocks(&item.page_blocks, context),
                                    },
                                ))
                            })
                            .collect(),
                    })
                }
            }
            WebPageBlock::BlockQuote { text, credit } => {
                telegram_api::PageBlock::pageBlockBlockquote(telegram_api::pageBlockBlockquote {
                    flags: 0,
                    collapsed: false,
                    text: Some(Box::new(text.get_input_rich_text(context))),
                    caption: Some(Box::new(credit.get_input_rich_text(context))),
                })
            }
            WebPageBlock::PullQuote { text, credit } => {
                telegram_api::PageBlock::pageBlockPullquote(telegram_api::pageBlockPullquote {
                    text: Some(Box::new(text.get_input_rich_text(context))),
                    caption: Some(Box::new(credit.get_input_rich_text(context))),
                })
            }
            WebPageBlock::Animation {
                animation_file_id,
                caption,
                need_autoplay,
                has_spoiler,
                ..
            } => {
                // the RichMessageMedia input-document hop is [blocked: M.9]
                if let Some((id, input_document)) =
                    context.env.as_input_document(*animation_file_id)
                {
                    context.documents.push(input_document);
                    return telegram_api::PageBlock::pageBlockVideo(telegram_api::pageBlockVideo {
                        flags: 0,
                        autoplay: *need_autoplay,
                        r#loop: true,
                        spoiler: *has_spoiler,
                        video_id: id,
                        caption: Some(caption.get_input_page_caption(context)),
                    });
                }
                tdutils::log_info!(
                    "Can't create pageBlockVideo for animation {}",
                    animation_file_id.get()
                );
                telegram_api::PageBlock::pageBlockDivider(telegram_api::pageBlockDivider {})
            }
            WebPageBlock::Photo {
                photo,
                caption,
                has_spoiler,
                ..
            } => {
                // [blocked: M.9] — the RichMessageMedia input-photo hop;
                // the photo's own file ids stand in for the media slot
                let photo_file_id = crate::photo::photo_get_file_ids(photo).first().copied();
                let input_photo =
                    photo_file_id.and_then(|file_id| context.env.as_input_photo(file_id));
                if let Some((id, input_photo)) = input_photo {
                    context.photos.push(input_photo);
                    return telegram_api::PageBlock::pageBlockPhoto(telegram_api::pageBlockPhoto {
                        flags: 0,
                        spoiler: *has_spoiler,
                        photo_id: id,
                        caption: Some(caption.get_input_page_caption(context)),
                        url: String::new(),
                        webpage_id: 0,
                    });
                }
                tdutils::log_info!("Can't create pageBlockPhoto");
                telegram_api::PageBlock::pageBlockDivider(telegram_api::pageBlockDivider {})
            }
            WebPageBlock::Video {
                video_file_id,
                caption,
                need_autoplay,
                is_looped,
                has_spoiler,
            } => {
                // [blocked: M.9] — the RichMessageMedia input-document hop
                if let Some((id, input_document)) = context.env.as_input_document(*video_file_id) {
                    context.documents.push(input_document);
                    return telegram_api::PageBlock::pageBlockVideo(telegram_api::pageBlockVideo {
                        flags: 0,
                        autoplay: *need_autoplay,
                        r#loop: *is_looped,
                        spoiler: *has_spoiler,
                        video_id: id,
                        caption: Some(caption.get_input_page_caption(context)),
                    });
                }
                tdutils::log_info!(
                    "Can't create pageBlockVideo for video {}",
                    video_file_id.get()
                );
                telegram_api::PageBlock::pageBlockDivider(telegram_api::pageBlockDivider {})
            }
            WebPageBlock::Cover(cover) => {
                tdutils::log_error!("Have pageBlockCover");
                telegram_api::PageBlock::pageBlockCover(telegram_api::pageBlockCover {
                    cover: Some(Box::new(cover.get_input_page_block(context))),
                })
            }
            WebPageBlock::Embedded { .. } => {
                tdutils::log_error!("Have pageBlockEmbedded");
                telegram_api::PageBlock::pageBlockDivider(telegram_api::pageBlockDivider {})
            }
            WebPageBlock::EmbeddedPost { .. } => {
                tdutils::log_error!("Have pageBlockEmbeddedPost");
                telegram_api::PageBlock::pageBlockDivider(telegram_api::pageBlockDivider {})
            }
            WebPageBlock::Collage {
                page_blocks,
                caption,
            } => telegram_api::PageBlock::pageBlockCollage(telegram_api::pageBlockCollage {
                items: get_input_page_blocks(page_blocks, context),
                caption: Some(caption.get_input_page_caption(context)),
            }),
            WebPageBlock::Slideshow {
                page_blocks,
                caption,
            } => telegram_api::PageBlock::pageBlockSlideshow(telegram_api::pageBlockSlideshow {
                items: get_input_page_blocks(page_blocks, context),
                caption: Some(caption.get_input_page_caption(context)),
            }),
            WebPageBlock::ChatLink(_) => {
                tdutils::log_error!("Have pageBlockChatLink");
                telegram_api::PageBlock::pageBlockDivider(telegram_api::pageBlockDivider {})
            }
            WebPageBlock::Audio {
                audio_file_id,
                caption,
            } => {
                if let Some((id, input_document)) = context.env.as_input_document(*audio_file_id) {
                    context.documents.push(input_document);
                    return telegram_api::PageBlock::pageBlockAudio(telegram_api::pageBlockAudio {
                        audio_id: id,
                        caption: Some(caption.get_input_page_caption(context)),
                    });
                }
                tdutils::log_info!(
                    "Can't create pageBlockAudio for audio {}",
                    audio_file_id.get()
                );
                telegram_api::PageBlock::pageBlockDivider(telegram_api::pageBlockDivider {})
            }
            WebPageBlock::Table(table) => {
                let cell_objects = table
                    .cells
                    .iter()
                    .map(|row| telegram_api::PageTableRow {
                        cells: row
                            .iter()
                            .map(|cell| cell.get_input_page_table_cell(context))
                            .collect(),
                    })
                    .collect();
                telegram_api::PageBlock::pageBlockTable(telegram_api::pageBlockTable {
                    flags: 0,
                    bordered: table.is_bordered,
                    striped: table.is_striped,
                    compact: table.is_compact,
                    title: Some(Box::new(table.title.get_input_rich_text(context))),
                    rows: cell_objects,
                })
            }
            WebPageBlock::Details {
                header,
                page_blocks,
                is_open,
            } => {
                let mut flags = 0;
                if *is_open {
                    // pageBlockDetails::OPEN_MASK
                    flags |= 1 << 0;
                }
                telegram_api::PageBlock::pageBlockDetails(telegram_api::pageBlockDetails {
                    flags,
                    open: *is_open,
                    blocks: get_input_page_blocks(page_blocks, context),
                    title: Some(Box::new(header.get_input_rich_text(context))),
                })
            }
            WebPageBlock::RelatedArticles { .. } => {
                tdutils::log_error!("Have pageBlockRelatedArticles");
                telegram_api::PageBlock::pageBlockDivider(telegram_api::pageBlockDivider {})
            }
            WebPageBlock::Map {
                location,
                zoom,
                dimensions,
                caption,
            } => telegram_api::PageBlock::inputPageBlockMap(telegram_api::inputPageBlockMap {
                geo: Some(location.get_input_geo_point()),
                zoom: *zoom,
                w: dimensions.width as i32,
                h: dimensions.height as i32,
                caption: Some(caption.get_input_page_caption(context)),
            }),
            WebPageBlock::VoiceNote {
                voice_note_file_id,
                caption,
            } => {
                if let Some((id, input_document)) =
                    context.env.as_input_document(*voice_note_file_id)
                {
                    context.documents.push(input_document);
                    return telegram_api::PageBlock::pageBlockAudio(telegram_api::pageBlockAudio {
                        audio_id: id,
                        caption: Some(caption.get_input_page_caption(context)),
                    });
                }
                tdutils::log_info!(
                    "Can't create pageBlockAudio for voice note {}",
                    voice_note_file_id.get()
                );
                telegram_api::PageBlock::pageBlockDivider(telegram_api::pageBlockDivider {})
            }
            WebPageBlock::Heading { text, size } => {
                let input_rich_text = text.get_input_rich_text(context);
                match size {
                    1 => telegram_api::PageBlock::pageBlockHeading1(
                        telegram_api::pageBlockHeading1 {
                            text: Some(Box::new(input_rich_text)),
                        },
                    ),
                    2 => telegram_api::PageBlock::pageBlockHeading2(
                        telegram_api::pageBlockHeading2 {
                            text: Some(Box::new(input_rich_text)),
                        },
                    ),
                    3 => telegram_api::PageBlock::pageBlockHeading3(
                        telegram_api::pageBlockHeading3 {
                            text: Some(Box::new(input_rich_text)),
                        },
                    ),
                    4 => telegram_api::PageBlock::pageBlockHeading4(
                        telegram_api::pageBlockHeading4 {
                            text: Some(Box::new(input_rich_text)),
                        },
                    ),
                    5 => telegram_api::PageBlock::pageBlockHeading5(
                        telegram_api::pageBlockHeading5 {
                            text: Some(Box::new(input_rich_text)),
                        },
                    ),
                    _ => telegram_api::PageBlock::pageBlockHeading6(
                        telegram_api::pageBlockHeading6 {
                            text: Some(Box::new(input_rich_text)),
                        },
                    ),
                }
            }
            WebPageBlock::Math { source } => {
                telegram_api::PageBlock::pageBlockMath(telegram_api::pageBlockMath {
                    source: source.clone(),
                })
            }
            WebPageBlock::Thinking(text) => {
                telegram_api::PageBlock::pageBlockThinking(telegram_api::pageBlockThinking {
                    text: Some(Box::new(text.get_input_rich_text(context))),
                })
            }
            WebPageBlock::BlockQuoteBlocks {
                page_blocks,
                caption,
            } => telegram_api::PageBlock::pageBlockBlockquoteBlocks(
                telegram_api::pageBlockBlockquoteBlocks {
                    blocks: get_input_page_blocks(page_blocks, context),
                    caption: Some(Box::new(caption.get_input_rich_text(context))),
                },
            ),
            WebPageBlock::Document {
                document_file_id,
                caption,
            } => {
                if let Some((id, input_document)) = context.env.as_input_document(*document_file_id)
                {
                    context.documents.push(input_document);
                    return telegram_api::PageBlock::pageBlockDocument(
                        telegram_api::pageBlockDocument {
                            document_id: id,
                            caption: Some(caption.get_input_page_caption(context)),
                        },
                    );
                }
                tdutils::log_info!(
                    "Can't create pageBlockDocument for document {}",
                    document_file_id.get()
                );
                telegram_api::PageBlock::pageBlockDivider(telegram_api::pageBlockDivider {})
            }
            WebPageBlock::ButtonRow(button_row) => {
                let mut flags = 0;
                if button_row.align_left {
                    flags |= 1 << 0;
                }
                if button_row.align_center {
                    flags |= 1 << 1;
                }
                if button_row.align_right {
                    flags |= 1 << 2;
                }
                telegram_api::PageBlock::pageBlockButtonRow(telegram_api::pageBlockButtonRow {
                    flags,
                    align_left: button_row.align_left,
                    align_center: button_row.align_center,
                    align_right: button_row.align_right,
                    buttons: button_row
                        .buttons
                        .iter()
                        .map(|button| {
                            let mut button_flags = 0;
                            let input_style = button.style.get_input_rich_button_style();
                            if input_style.is_some() {
                                // pageButton::STYLE_MASK
                                button_flags |= 1 << 0;
                            }
                            let input_button = button
                                .button
                                .get_input_keyboard_inline_button(context.user_manager);
                            telegram_api::PageButton {
                                flags: button_flags,
                                text: Some(Box::new(button.text.get_input_rich_text(context))),
                                r#type: input_button.r#type,
                                style: input_style,
                            }
                        })
                        .collect(),
                })
            }
            WebPageBlock::ExpandableBlockQuote { text, credit } => {
                telegram_api::PageBlock::pageBlockBlockquote(telegram_api::pageBlockBlockquote {
                    flags: 0,
                    collapsed: true,
                    text: Some(Box::new(text.get_input_rich_text(context))),
                    caption: Some(Box::new(credit.get_input_rich_text(context))),
                })
            }
            WebPageBlock::Unsupported { .. } => {
                // the unsupported block re-emits the divider
                telegram_api::PageBlock::pageBlockDivider(telegram_api::pageBlockDivider {})
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The wire PageBlock intake (WebPageBlock.cpp:4886-5327)
// ---------------------------------------------------------------------------

/// `get_web_page_block` (WebPageBlock.cpp:4886-5327) — `None` for the
/// blocks the C++ drops (unsupported channel, invalid map, empty cover).
#[allow(clippy::too_many_arguments)] // the C++ handler has the same shape
pub fn get_web_page_block(
    env: &mut dyn WebPageBlockEnv,
    page_block_ptr: telegram_api::PageBlock,
    animations: &DocumentsById,
    audios: &DocumentsById,
    documents: &DocumentsById,
    photos: &PhotosById,
    videos: &DocumentsById,
    voice_notes: &DocumentsById,
) -> Option<WebPageBlock> {
    match page_block_ptr {
        telegram_api::PageBlock::pageBlockUnsupported(_) => None,
        telegram_api::PageBlock::pageBlockTitle(page_block) => Some(WebPageBlock::Title(
            get_rich_text_from_telegram(*page_block.text?, documents),
        )),
        telegram_api::PageBlock::pageBlockSubtitle(page_block) => Some(WebPageBlock::Subtitle(
            get_rich_text_from_telegram(*page_block.text?, documents),
        )),
        telegram_api::PageBlock::pageBlockAuthorDate(page_block) => {
            Some(WebPageBlock::AuthorDate {
                author: get_rich_text_from_telegram(*page_block.author?, documents),
                date: page_block.published_date,
            })
        }
        telegram_api::PageBlock::pageBlockHeader(page_block) => Some(WebPageBlock::Header(
            get_rich_text_from_telegram(*page_block.text?, documents),
        )),
        telegram_api::PageBlock::pageBlockSubheader(page_block) => Some(WebPageBlock::Subheader(
            get_rich_text_from_telegram(*page_block.text?, documents),
        )),
        telegram_api::PageBlock::pageBlockKicker(page_block) => Some(WebPageBlock::Kicker(
            get_rich_text_from_telegram(*page_block.text?, documents),
        )),
        telegram_api::PageBlock::pageBlockParagraph(page_block) => Some(WebPageBlock::Paragraph(
            get_rich_text_from_telegram(*page_block.text?, documents),
        )),
        telegram_api::PageBlock::pageBlockPreformatted(page_block) => {
            Some(WebPageBlock::Preformatted {
                text: get_rich_text_from_telegram(*page_block.text?, documents),
                language: page_block.language,
            })
        }
        telegram_api::PageBlock::pageBlockFooter(page_block) => Some(WebPageBlock::Footer(
            get_rich_text_from_telegram(*page_block.text?, documents),
        )),
        telegram_api::PageBlock::pageBlockDivider(_) => Some(WebPageBlock::Divider),
        telegram_api::PageBlock::pageBlockAnchor(page_block) => Some(WebPageBlock::Anchor {
            name: page_block.name,
        }),
        telegram_api::PageBlock::pageBlockList(page_block) => {
            let items = page_block
                .items
                .into_iter()
                .map(|list_item_ptr| {
                    let mut item = WebPageBlockListItem {
                        value: -1,
                        ..WebPageBlockListItem::default()
                    };
                    match *list_item_ptr {
                        telegram_api::PageListItem::pageListItemText(list_item) => {
                            if let Some(text) = list_item.text {
                                item.page_blocks.push(WebPageBlock::Paragraph(
                                    get_rich_text_from_telegram(*text, documents),
                                ));
                            }
                            item.has_checkbox = list_item.checkbox;
                            item.is_checked = list_item.checked;
                        }
                        telegram_api::PageListItem::pageListItemBlocks(list_item) => {
                            item.page_blocks = get_web_page_blocks_from_telegram(
                                env,
                                list_item.blocks,
                                animations,
                                audios,
                                documents,
                                photos,
                                videos,
                                voice_notes,
                            );
                            item.has_checkbox = list_item.checkbox;
                            item.is_checked = list_item.checked;
                        }
                    }
                    if item.page_blocks.is_empty() {
                        item.page_blocks
                            .push(WebPageBlock::Paragraph(RichText::default()));
                    }
                    item
                })
                .collect();
            Some(WebPageBlock::List(WebPageBlockList {
                items,
                start: 0,
                is_reversed: false,
                r#type: String::new(),
            }))
        }
        telegram_api::PageBlock::pageBlockOrderedList(page_block) => {
            let mut next_value = 1;
            if page_block.flags & (1 << 0) != 0 {
                // pageBlockOrderedList::START_MASK
                next_value = page_block.start;
            }
            let base_type = get_ordered_list_type(&page_block.r#type);
            let items = page_block
                .items
                .into_iter()
                .map(|list_item_ptr| {
                    let mut item = WebPageBlockListItem {
                        value: -1,
                        ..WebPageBlockListItem::default()
                    };
                    match *list_item_ptr {
                        telegram_api::PageListOrderedItem::pageListOrderedItemText(list_item) => {
                            item.label = list_item.num;
                            if let Some(text) = list_item.text {
                                item.page_blocks.push(WebPageBlock::Paragraph(
                                    get_rich_text_from_telegram(*text, documents),
                                ));
                            }
                            item.has_checkbox = list_item.checkbox;
                            item.is_checked = list_item.checked;
                            if list_item.flags & (1 << 2) != 0 {
                                // pageListOrderedItemText::VALUE_MASK
                                item.value = list_item.value;
                            } else {
                                item.value = next_value;
                            }
                            item.r#type = if list_item.r#type.is_empty() {
                                base_type.clone()
                            } else {
                                get_ordered_list_type(&list_item.r#type)
                            };
                        }
                        telegram_api::PageListOrderedItem::pageListOrderedItemBlocks(list_item) => {
                            item.label = list_item.num;
                            item.page_blocks = get_web_page_blocks_from_telegram(
                                env,
                                list_item.blocks,
                                animations,
                                audios,
                                documents,
                                photos,
                                videos,
                                voice_notes,
                            );
                            item.has_checkbox = list_item.checkbox;
                            item.is_checked = list_item.checked;
                            if list_item.flags & (1 << 2) != 0 {
                                // pageListOrderedItemBlocks::VALUE_MASK
                                item.value = list_item.value;
                            } else {
                                item.value = next_value;
                            }
                            item.r#type = if list_item.r#type.is_empty() {
                                base_type.clone()
                            } else {
                                get_ordered_list_type(&list_item.r#type)
                            };
                        }
                    }
                    if item.page_blocks.is_empty() {
                        item.page_blocks
                            .push(WebPageBlock::Paragraph(RichText::default()));
                    }
                    if item.label.is_empty() {
                        item.label = get_ordered_list_label(item.value, &item.r#type);
                    } else {
                        item.label.push('.');
                    }
                    next_value = item.value + if page_block.reversed { -1 } else { 1 };
                    item
                })
                .collect();
            Some(WebPageBlock::List(WebPageBlockList {
                items,
                start: page_block.start,
                is_reversed: page_block.reversed,
                r#type: base_type,
            }))
        }
        telegram_api::PageBlock::pageBlockBlockquote(page_block) => {
            let text = get_rich_text_from_telegram(*page_block.text?, documents);
            let caption = get_rich_text_from_telegram(*page_block.caption?, documents);
            if page_block.collapsed {
                Some(WebPageBlock::ExpandableBlockQuote {
                    text,
                    credit: caption,
                })
            } else {
                Some(WebPageBlock::BlockQuote {
                    text,
                    credit: caption,
                })
            }
        }
        telegram_api::PageBlock::pageBlockPullquote(page_block) => {
            let text = get_rich_text_from_telegram(*page_block.text?, documents);
            let caption = get_rich_text_from_telegram(*page_block.caption?, documents);
            Some(WebPageBlock::PullQuote {
                text,
                credit: caption,
            })
        }
        telegram_api::PageBlock::pageBlockPhoto(page_block) => {
            let photo = photos
                .get(&page_block.photo_id)
                .cloned()
                .unwrap_or_default();
            let caption = page_block
                .caption
                .map(|caption| get_page_block_caption_from_telegram(caption, documents))
                .unwrap_or_default();
            Some(WebPageBlock::Photo {
                photo,
                caption,
                url: page_block.url,
                web_page_id: WebPageId::new(page_block.webpage_id),
                has_spoiler: page_block.spoiler,
            })
        }
        telegram_api::PageBlock::pageBlockVideo(page_block) => {
            let need_autoplay = page_block.autoplay;
            let is_looped = page_block.r#loop;
            let caption = page_block
                .caption
                .map(|caption| get_page_block_caption_from_telegram(caption, documents))
                .unwrap_or_default();
            if let Some(&animation_file_id) = animations.get(&page_block.video_id) {
                return Some(WebPageBlock::Animation {
                    animation_file_id,
                    caption,
                    need_autoplay,
                    has_spoiler: page_block.spoiler,
                });
            }
            let video_file_id = videos
                .get(&page_block.video_id)
                .copied()
                .unwrap_or_default();
            Some(WebPageBlock::Video {
                video_file_id,
                caption,
                need_autoplay,
                is_looped,
                has_spoiler: page_block.spoiler,
            })
        }
        telegram_api::PageBlock::pageBlockCover(page_block) => {
            let cover = *page_block.cover?;
            let cover = get_web_page_block(
                env,
                cover,
                animations,
                audios,
                documents,
                photos,
                videos,
                voice_notes,
            )?;
            Some(WebPageBlock::Cover(Box::new(cover)))
        }
        telegram_api::PageBlock::pageBlockEmbed(page_block) => {
            let poster_photo = photos
                .get(&page_block.poster_photo_id)
                .cloned()
                .unwrap_or_default();
            let dimensions = get_dimensions(page_block.w, page_block.h, "pageBlockEmbed");
            let caption = page_block
                .caption
                .map(|caption| get_page_block_caption_from_telegram(caption, documents))
                .unwrap_or_default();
            Some(WebPageBlock::Embedded {
                url: page_block.url,
                html: page_block.html,
                poster_photo,
                dimensions,
                caption,
                is_full_width: page_block.full_width,
                allow_scrolling: page_block.allow_scrolling,
            })
        }
        telegram_api::PageBlock::pageBlockEmbedPost(page_block) => {
            let author_photo = photos
                .get(&page_block.author_photo_id)
                .cloned()
                .unwrap_or_default();
            Some(WebPageBlock::EmbeddedPost {
                url: page_block.url,
                author: page_block.author,
                author_photo,
                date: page_block.date.max(0),
                page_blocks: get_web_page_blocks_from_telegram(
                    env,
                    page_block.blocks,
                    animations,
                    audios,
                    documents,
                    photos,
                    videos,
                    voice_notes,
                ),
                caption: page_block
                    .caption
                    .map(|caption| get_page_block_caption_from_telegram(caption, documents))
                    .unwrap_or_default(),
            })
        }
        telegram_api::PageBlock::pageBlockCollage(page_block) => Some(WebPageBlock::Collage {
            page_blocks: get_web_page_blocks_from_telegram(
                env,
                page_block.items,
                animations,
                audios,
                documents,
                photos,
                videos,
                voice_notes,
            ),
            caption: page_block
                .caption
                .map(|caption| get_page_block_caption_from_telegram(caption, documents))
                .unwrap_or_default(),
        }),
        telegram_api::PageBlock::pageBlockSlideshow(page_block) => Some(WebPageBlock::Slideshow {
            page_blocks: get_web_page_blocks_from_telegram(
                env,
                page_block.items,
                animations,
                audios,
                documents,
                photos,
                videos,
                voice_notes,
            ),
            caption: page_block
                .caption
                .map(|caption| get_page_block_caption_from_telegram(caption, documents))
                .unwrap_or_default(),
        }),
        telegram_api::PageBlock::pageBlockChannel(page_block) => {
            let telegram_api::Chat::channel(channel) = page_block.channel? else {
                tdutils::log_error!("Receive wrong channel in pageBlockChannel");
                return None;
            };
            env.on_get_page_block_channel(channel)
                .map(WebPageBlock::ChatLink)
        }
        telegram_api::PageBlock::pageBlockAudio(page_block) => {
            let caption = page_block
                .caption
                .map(|caption| get_page_block_caption_from_telegram(caption, documents))
                .unwrap_or_default();
            if let Some(&voice_note_file_id) = voice_notes.get(&page_block.audio_id) {
                return Some(WebPageBlock::VoiceNote {
                    voice_note_file_id,
                    caption,
                });
            }
            let audio_file_id = audios
                .get(&page_block.audio_id)
                .copied()
                .unwrap_or_default();
            Some(WebPageBlock::Audio {
                audio_file_id,
                caption,
            })
        }
        telegram_api::PageBlock::pageBlockTable(page_block) => {
            let cells = page_block
                .rows
                .into_iter()
                .map(|row| {
                    row.cells
                        .into_iter()
                        .map(|mut table_cell| {
                            let mut cell = WebPageBlockTableCell {
                                colspan: 1,
                                rowspan: 1,
                                ..WebPageBlockTableCell::default()
                            };
                            cell.is_header = table_cell.header;
                            cell.align_center = table_cell.align_center;
                            if !cell.align_center {
                                cell.align_right = table_cell.align_right;
                                if !cell.align_right {
                                    cell.align_left = true;
                                }
                            }
                            cell.valign_middle = table_cell.valign_middle;
                            if !cell.valign_middle {
                                cell.valign_bottom = table_cell.valign_bottom;
                                if !cell.valign_bottom {
                                    cell.valign_top = true;
                                }
                            }
                            if let Some(text) = table_cell.text.take() {
                                cell.text = get_rich_text_from_telegram(*text, documents);
                            }
                            cell.colspan = table_cell.colspan.max(1);
                            cell.rowspan = table_cell.rowspan.max(1);
                            cell
                        })
                        .collect()
                })
                .collect();
            Some(WebPageBlock::Table(WebPageBlockTable {
                title: get_rich_text_from_telegram(*page_block.title?, documents),
                cells,
                is_bordered: page_block.bordered,
                is_striped: page_block.striped,
                is_compact: page_block.compact,
            }))
        }
        telegram_api::PageBlock::pageBlockDetails(page_block) => Some(WebPageBlock::Details {
            header: get_rich_text_from_telegram(*page_block.title?, documents),
            page_blocks: get_web_page_blocks_from_telegram(
                env,
                page_block.blocks,
                animations,
                audios,
                documents,
                photos,
                videos,
                voice_notes,
            ),
            is_open: page_block.open,
        }),
        telegram_api::PageBlock::pageBlockRelatedArticles(page_block) => {
            let articles = page_block
                .articles
                .into_iter()
                .map(|related_article| {
                    let mut article = RelatedArticle {
                        url: related_article.url,
                        web_page_id: WebPageId::new(related_article.webpage_id),
                        title: related_article.title,
                        description: related_article.description,
                        photo: photos
                            .get(&related_article.photo_id)
                            .cloned()
                            .unwrap_or_default(),
                        author: related_article.author,
                        published_date: 0,
                    };
                    if related_article.published_date > 0 {
                        article.published_date = related_article.published_date;
                    }
                    article
                })
                .collect();
            Some(WebPageBlock::RelatedArticles {
                header: get_rich_text_from_telegram(*page_block.title?, documents),
                related_articles: articles,
            })
        }
        telegram_api::PageBlock::pageBlockMap(page_block) => {
            let location = page_block
                .geo
                .map(crate::chat_manager::Location::from_geo_point)
                .unwrap_or_default();
            let zoom = page_block.zoom;
            let dimensions = get_dimensions(page_block.w, page_block.h, "pageBlockMap");
            if location.is_empty() {
                tdutils::log_error!("Receive invalid map location");
                return None;
            }
            if !(0..=24).contains(&zoom) {
                tdutils::log_error!("Receive invalid map zoom {zoom}");
                return None;
            }
            if dimensions.width == 0 {
                tdutils::log_error!(
                    "Receive invalid map dimensions {} {}",
                    page_block.w,
                    page_block.h
                );
                return None;
            }
            Some(WebPageBlock::Map {
                location,
                zoom,
                dimensions,
                caption: page_block
                    .caption
                    .map(|caption| get_page_block_caption_from_telegram(caption, documents))
                    .unwrap_or_default(),
            })
        }
        page_block @ telegram_api::PageBlock::pageBlockHeading1(_)
        | page_block @ telegram_api::PageBlock::pageBlockHeading2(_)
        | page_block @ telegram_api::PageBlock::pageBlockHeading3(_)
        | page_block @ telegram_api::PageBlock::pageBlockHeading4(_)
        | page_block @ telegram_api::PageBlock::pageBlockHeading5(_)
        | page_block @ telegram_api::PageBlock::pageBlockHeading6(_) => {
            let (text, size) = match page_block {
                telegram_api::PageBlock::pageBlockHeading1(block) => (block.text, 1),
                telegram_api::PageBlock::pageBlockHeading2(block) => (block.text, 2),
                telegram_api::PageBlock::pageBlockHeading3(block) => (block.text, 3),
                telegram_api::PageBlock::pageBlockHeading4(block) => (block.text, 4),
                telegram_api::PageBlock::pageBlockHeading5(block) => (block.text, 5),
                telegram_api::PageBlock::pageBlockHeading6(block) => (block.text, 6),
                _ => unreachable!("matched above"),
            };
            Some(WebPageBlock::Heading {
                text: get_rich_text_from_telegram(*text?, documents),
                size,
            })
        }
        telegram_api::PageBlock::pageBlockMath(page_block) => Some(WebPageBlock::Math {
            source: page_block.source,
        }),
        telegram_api::PageBlock::pageBlockThinking(page_block) => Some(WebPageBlock::Thinking(
            get_rich_text_from_telegram(*page_block.text?, documents),
        )),
        // inputPageBlockMap is an input-only constructor
        telegram_api::PageBlock::inputPageBlockMap(_) => None,
        telegram_api::PageBlock::pageBlockBlockquoteBlocks(page_block) => {
            Some(WebPageBlock::BlockQuoteBlocks {
                page_blocks: get_web_page_blocks_from_telegram(
                    env,
                    page_block.blocks,
                    animations,
                    audios,
                    documents,
                    photos,
                    videos,
                    voice_notes,
                ),
                caption: get_rich_text_from_telegram(*page_block.caption?, documents),
            })
        }
        telegram_api::PageBlock::pageBlockButtonRow(page_block) => {
            Some(WebPageBlock::ButtonRow(WebPageBlockButtonRow {
                buttons: page_block
                    .buttons
                    .into_iter()
                    .map(|button| {
                        let mut result = WebPageBlockRowButton {
                            style: RichButtonStyle::from_telegram(button.style),
                            ..WebPageBlockRowButton::default()
                        };
                        if let Some(text) = button.text {
                            result.text = get_rich_text_from_telegram(*text, documents);
                        }
                        let keyboard_button = telegram_api::KeyboardInlineButton {
                            flags: 0,
                            style: None,
                            text: "1".to_owned(),
                            r#type: button.r#type,
                        };
                        result.button = InlineKeyboardButton::from_telegram(keyboard_button);
                        result
                    })
                    .collect(),
                align_left: page_block.align_left,
                align_center: page_block.align_center,
                align_right: page_block.align_right,
            }))
        }
        telegram_api::PageBlock::pageBlockDocument(page_block) => {
            let document_file_id = documents.get(&page_block.document_id).copied();
            let caption = page_block
                .caption
                .map(|caption| get_page_block_caption_from_telegram(caption, documents))
                .unwrap_or_default();
            Some(WebPageBlock::Document {
                document_file_id: document_file_id.unwrap_or_default(),
                caption,
            })
        }
    }
}

/// `get_web_page_blocks` over the wire blocks (WebPageBlock.cpp:5517-5533)
/// — the dropped/empty blocks become Unsupported.
#[allow(clippy::too_many_arguments)] // the C++ handler has the same shape
pub fn get_web_page_blocks_from_telegram(
    env: &mut dyn WebPageBlockEnv,
    page_block_ptrs: Vec<Box<telegram_api::PageBlock>>,
    animations: &DocumentsById,
    audios: &DocumentsById,
    documents: &DocumentsById,
    photos: &PhotosById,
    videos: &DocumentsById,
    voice_notes: &DocumentsById,
) -> Vec<WebPageBlock> {
    page_block_ptrs
        .into_iter()
        .map(|page_block_ptr| {
            get_web_page_block(
                env,
                *page_block_ptr,
                animations,
                audios,
                documents,
                photos,
                videos,
                voice_notes,
            )
            .unwrap_or(WebPageBlock::Unsupported {
                version: WEB_PAGE_BLOCK_UNSUPPORTED_VERSION,
            })
        })
        .collect()
}

/// The input-media seam of the td_api intake: the media arms convert
/// through `get_input_message_content` — the M.9/M.10 send plane.
fn get_input_media_file_id_seam(
    _env: &mut dyn WebPageBlockEnv,
    dialog_id: DialogId,
    media: Option<td_api::InputFile>,
    source: &str,
) -> std::result::Result<FileId, Status> {
    let _ = (dialog_id, media, source);
    // [blocked: M.9/M.10] get_input_message_content + the file
    // registration; unreachable until a rich-message send lands
    Err(Status::error(400, "Rich media input is not supported yet"))
}

/// `get_web_page_blocks` over the td_api InputPageBlocks
/// (WebPageBlock.cpp:5535-5809). The media arms ride the M.9/M.10 seam.
pub fn get_web_page_blocks_from_td_api(
    env: &mut dyn WebPageBlockEnv,
    dialog_id: DialogId,
    input_page_blocks: Vec<Box<td_api::InputPageBlock>>,
) -> std::result::Result<Vec<WebPageBlock>, Status> {
    let mut result: Vec<WebPageBlock> = Vec::new();
    for input_page_block in input_page_blocks {
        match *input_page_block {
            td_api::InputPageBlock::inputPageBlockSectionHeading(block) => {
                let text = get_rich_text_from_td_api(env, block.text)?;
                if !(1..=6).contains(&block.size) {
                    return Err(Status::error(400, "Invalid section heading size specified"));
                }
                result.push(WebPageBlock::Heading {
                    text,
                    size: block.size,
                });
            }
            td_api::InputPageBlock::inputPageBlockParagraph(block) => {
                let text = get_rich_text_from_td_api(env, block.text)?;
                result.push(WebPageBlock::Paragraph(text));
            }
            td_api::InputPageBlock::inputPageBlockPreformatted(block) => {
                let text = get_rich_text_from_td_api(env, block.text)?;
                let Some(language) = crate::misc::clean_input_string(&block.language) else {
                    return Err(Status::error(400, "Language must be encoded in UTF-8"));
                };
                result.push(WebPageBlock::Preformatted { text, language });
            }
            td_api::InputPageBlock::inputPageBlockFooter(block) => {
                let text = get_rich_text_from_td_api(env, block.footer)?;
                result.push(WebPageBlock::Footer(text));
            }
            td_api::InputPageBlock::inputPageBlockThinking(block) => {
                let text = get_rich_text_from_td_api(env, block.text)?;
                result.push(WebPageBlock::Thinking(text));
            }
            td_api::InputPageBlock::inputPageBlockDivider(_) => {
                result.push(WebPageBlock::Divider);
            }
            td_api::InputPageBlock::inputPageBlockMathematicalExpression(block) => {
                let Some(expression) = crate::misc::clean_input_string(&block.expression) else {
                    return Err(Status::error(
                        400,
                        "Mathematical expression must be encoded in UTF-8",
                    ));
                };
                result.push(WebPageBlock::Math { source: expression });
            }
            td_api::InputPageBlock::inputPageBlockAnchor(block) => {
                let Some(name) = crate::misc::clean_input_string(&block.name) else {
                    return Err(Status::error(400, "Anchor name must be encoded in UTF-8"));
                };
                result.push(WebPageBlock::Anchor { name });
            }
            td_api::InputPageBlock::inputPageBlockList(block) => {
                let mut items = Vec::with_capacity(block.items.len());
                for input_item in block.items {
                    items.push(WebPageBlockListItem::get_item(
                        env,
                        dialog_id,
                        Some(*input_item),
                    )?);
                }
                if items.is_empty() {
                    return Err(Status::error(400, "List must be non-empty"));
                }
                let is_unordered = items[0].label.is_empty();
                for item in &items {
                    if item.label.is_empty() != is_unordered {
                        return Err(Status::error(
                            400,
                            "List must be either ordered or unordered",
                        ));
                    }
                }
                result.push(WebPageBlock::List(WebPageBlockList {
                    items,
                    start: 0,
                    is_reversed: false,
                    r#type: String::new(),
                }));
            }
            td_api::InputPageBlock::inputPageBlockBlockQuote(block) => {
                let blocks = get_web_page_blocks_from_td_api(env, dialog_id, block.blocks)?;
                let credit = get_rich_text_from_td_api(env, block.credit)?;
                result.push(WebPageBlock::BlockQuoteBlocks {
                    page_blocks: blocks,
                    caption: credit,
                });
            }
            td_api::InputPageBlock::inputPageBlockExpandableBlockQuote(block) => {
                let text = get_rich_text_from_td_api(env, block.text)?;
                let credit = get_rich_text_from_td_api(env, block.credit)?;
                result.push(WebPageBlock::ExpandableBlockQuote { text, credit });
            }
            td_api::InputPageBlock::inputPageBlockPullQuote(block) => {
                let text = get_rich_text_from_td_api(env, block.text)?;
                let credit = get_rich_text_from_td_api(env, block.credit)?;
                result.push(WebPageBlock::PullQuote { text, credit });
            }
            td_api::InputPageBlock::inputPageBlockAnimation(block) => {
                let animation_file_id = get_input_media_file_id_seam(
                    env,
                    dialog_id,
                    block.animation.and_then(|a| a.animation),
                    "inputPageBlockAnimation",
                )?;
                let caption = WebPageBlockCaption::get_web_page_block_caption(env, block.caption)?;
                result.push(WebPageBlock::Animation {
                    animation_file_id,
                    caption,
                    need_autoplay: true,
                    has_spoiler: block.has_spoiler,
                });
            }
            td_api::InputPageBlock::inputPageBlockAudio(block) => {
                let audio_file_id = get_input_media_file_id_seam(
                    env,
                    dialog_id,
                    block.audio.and_then(|a| a.audio),
                    "inputPageBlockAudio",
                )?;
                let caption = WebPageBlockCaption::get_web_page_block_caption(env, block.caption)?;
                result.push(WebPageBlock::Audio {
                    audio_file_id,
                    caption,
                });
            }
            td_api::InputPageBlock::inputPageBlockDocument(block) => {
                let document_file_id = get_input_media_file_id_seam(
                    env,
                    dialog_id,
                    block.document.and_then(|d| d.document),
                    "inputPageBlockDocument",
                )?;
                let caption = WebPageBlockCaption::get_web_page_block_caption(env, block.caption)?;
                result.push(WebPageBlock::Document {
                    document_file_id,
                    caption,
                });
            }
            td_api::InputPageBlock::inputPageBlockPhoto(block) => {
                let _photo = get_input_media_file_id_seam(
                    env,
                    dialog_id,
                    block.photo.and_then(|p| p.photo),
                    "inputPageBlockPhoto",
                )?;
                let caption = WebPageBlockCaption::get_web_page_block_caption(env, block.caption)?;
                result.push(WebPageBlock::Photo {
                    photo: Photo::default(),
                    caption,
                    url: String::new(),
                    web_page_id: WebPageId::default(),
                    has_spoiler: block.has_spoiler,
                });
            }
            td_api::InputPageBlock::inputPageBlockVideo(block) => {
                let video_file_id = get_input_media_file_id_seam(
                    env,
                    dialog_id,
                    block.video.and_then(|v| v.video),
                    "inputPageBlockVideo",
                )?;
                let caption = WebPageBlockCaption::get_web_page_block_caption(env, block.caption)?;
                result.push(WebPageBlock::Video {
                    video_file_id,
                    caption,
                    need_autoplay: false,
                    is_looped: false,
                    has_spoiler: block.has_spoiler,
                });
            }
            td_api::InputPageBlock::inputPageBlockVoiceNote(block) => {
                let voice_note_file_id = get_input_media_file_id_seam(
                    env,
                    dialog_id,
                    block.voice_note.and_then(|v| v.voice_note),
                    "inputPageBlockVoiceNote",
                )?;
                let caption = WebPageBlockCaption::get_web_page_block_caption(env, block.caption)?;
                result.push(WebPageBlock::VoiceNote {
                    voice_note_file_id,
                    caption,
                });
            }
            td_api::InputPageBlock::inputPageBlockCollage(block) => {
                let blocks = get_web_page_blocks_from_td_api(env, dialog_id, block.blocks)?;
                let caption = WebPageBlockCaption::get_web_page_block_caption(env, block.caption)?;
                result.push(WebPageBlock::Collage {
                    page_blocks: blocks,
                    caption,
                });
            }
            td_api::InputPageBlock::inputPageBlockSlideshow(block) => {
                let blocks = get_web_page_blocks_from_td_api(env, dialog_id, block.blocks)?;
                let caption = WebPageBlockCaption::get_web_page_block_caption(env, block.caption)?;
                result.push(WebPageBlock::Slideshow {
                    page_blocks: blocks,
                    caption,
                });
            }
            td_api::InputPageBlock::inputPageBlockTable(block) => {
                let title = get_rich_text_from_td_api(env, block.caption)?;
                let mut cells = Vec::with_capacity(block.cells.len());
                for input_row in block.cells {
                    let mut row = Vec::with_capacity(input_row.len());
                    for input_cell in input_row {
                        row.push(WebPageBlockTableCell::get_web_page_block_table_cell(
                            env,
                            Some(input_cell),
                        )?);
                    }
                    cells.push(row);
                }
                result.push(WebPageBlock::Table(WebPageBlockTable {
                    title,
                    cells,
                    is_bordered: block.is_bordered,
                    is_striped: block.is_striped,
                    is_compact: block.is_compact,
                }));
            }
            td_api::InputPageBlock::inputPageBlockDetails(block) => {
                let header = get_rich_text_from_td_api(env, block.header)?;
                let blocks = get_web_page_blocks_from_td_api(env, dialog_id, block.blocks)?;
                result.push(WebPageBlock::Details {
                    header,
                    page_blocks: blocks,
                    is_open: block.is_open,
                });
            }
            td_api::InputPageBlock::inputPageBlockMap(block) => {
                let location = block
                    .location
                    .as_ref()
                    .map(crate::chat_manager::Location::from_td_api_location)
                    .unwrap_or_default();
                if location.is_empty() {
                    return Err(Status::error(400, "Invalid location specified"));
                }
                if !(0..25).contains(&block.zoom)
                    || !(0..=10000).contains(&block.width)
                    || !(0..=10000).contains(&block.height)
                    || block.width + block.height > 10000
                    || block.width > block.height * 20
                    || block.height > block.width * 20
                {
                    return Err(Status::error(400, "Invalid map properties specified"));
                }
                let caption = WebPageBlockCaption::get_web_page_block_caption(env, block.caption)?;
                result.push(WebPageBlock::Map {
                    location,
                    zoom: block.zoom,
                    dimensions: get_dimensions(block.width, block.height, "inputPageBlockMap"),
                    caption,
                });
            }
            td_api::InputPageBlock::inputPageBlockButtonRow(block) => {
                let mut buttons = Vec::with_capacity(block.buttons.len());
                for input_button in block.buttons {
                    buttons.push(WebPageBlockRowButton::get_button(env, Some(*input_button))?);
                }
                if buttons.is_empty() {
                    return Err(Status::error(400, "Button row must be non-empty"));
                }
                let mut align_left = false;
                let mut align_center = false;
                let mut align_right = false;
                if let Some(align) = block.align.as_ref() {
                    match align {
                        td_api::PageBlockHorizontalAlignment::pageBlockHorizontalAlignmentLeft(_) => {
                            align_left = true;
                        }
                        td_api::PageBlockHorizontalAlignment::pageBlockHorizontalAlignmentCenter(_) => {
                            align_center = true;
                        }
                        td_api::PageBlockHorizontalAlignment::pageBlockHorizontalAlignmentRight(_) => {
                            align_right = true;
                        }
                    }
                }
                result.push(WebPageBlock::ButtonRow(WebPageBlockButtonRow {
                    buttons,
                    align_left,
                    align_center,
                    align_right,
                }));
            }
        }
    }
    Ok(result)
}

/// The ordered-list label helpers re-exported for tests.
#[cfg(test)]
pub(crate) fn ordered_list_label_for_test(num: i32, type_: &str) -> String {
    get_ordered_list_label(num, type_)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::emoji_status::CustomEmojiId;
    use crate::files::FileId;
    use crate::formatted_date::FormattedDate;
    use crate::inline_keyboard_button::{InlineKeyboardButton, InlineKeyboardButtonType};
    use crate::keyboard_button_style::KeyboardButtonStyle;
    use crate::photo::{
        DialogPhoto, Dimensions, Photo, PhotoSize, parse_dialog_photo, parse_dimensions,
        parse_photo, store_dialog_photo, store_dimensions, store_photo,
    };
    use crate::rich_button_style::RichButtonStyle;
    use crate::store_parse::{Parser, version};
    use crate::target_dialog_types::TargetDialogTypes;
    use crate::td_api;
    use crate::user_manager::UserId;

    /// The test env: FileIds round-trip through the raw codec; every
    /// media-object builder answers `None` like the collapsed managers.
    #[derive(Default)]
    struct TestEnv {
        file_counter: i32,
    }

    impl crate::photo::PhotoStoreEnv for TestEnv {
        fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            crate::store_parse::store_i32(out, file_id.get());
        }

        fn parse_file_id(
            &mut self,
            parser: &mut Parser<'_>,
        ) -> std::result::Result<FileId, &'static str> {
            Ok(FileId::new(parser.i32_()?, 0))
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
            _photo: &DialogPhoto,
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
            self.file_counter += 1;
            let id = self.file_counter as i64;
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
            self.file_counter += 1;
            let id = self.file_counter as i64;
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
        ) -> Option<ChatLinkInfo> {
            let channel_id = crate::chat_manager::ChannelId::new(channel.id);
            if !channel_id.is_valid() {
                tdutils::log_error!("Receive invalid channel {}", channel.id);
                return None;
            }
            let photo = DialogPhoto::get_dialog_photo(channel.photo);
            Some(ChatLinkInfo {
                title: channel.title,
                photo,
                username: channel.username,
                accent_color_id: crate::peer_color::AccentColorId::from_peer_id(channel.id),
                channel_id,
            })
        }
    }

    fn round_trip<T>(
        f: impl FnOnce(&mut TestEnv, &mut Vec<u8>),
        g: impl FnOnce(&mut TestEnv, &mut Parser<'_>) -> std::result::Result<T, &'static str>,
        check: impl FnOnce(T),
    ) {
        let mut env = TestEnv::default();
        let mut out = Vec::new();
        f(&mut env, &mut out);
        let mut env = TestEnv::default();
        let mut parser = Parser::new(&out);
        let parsed = g(&mut env, &mut parser).expect("parse round trip");
        parser.fetch_end().expect("no trailing bytes");
        check(parsed);
    }

    fn plain(text: &str) -> RichText {
        RichText {
            r#type: RichTextType::Plain,
            content: text.to_owned(),
            ..RichText::default()
        }
    }

    /// A rich text node with the given type, content and children.
    fn node(r#type: RichTextType, content: &str, texts: Vec<RichText>) -> RichText {
        RichText {
            r#type,
            content: content.to_owned(),
            texts,
            ..RichText::default()
        }
    }

    fn icon() -> RichText {
        RichText {
            r#type: RichTextType::Icon,
            content: "131074".to_owned(),
            document_file_id: FileId::new(1, 0),
            ..RichText::default()
        }
    }

    // ===== the 1.8.70 quote plane (RichText.cpp) =====

    /// `RichText::create_math` + `get_quote_text` (upstream df92bc788) —
    /// the quote text of every node kind, worked out from the C++ switch.
    #[test]
    fn rich_text_create_math_and_get_quote_text() {
        let math = RichText::create_math("x^2 + 1".to_owned());
        assert_eq!(math.r#type, RichTextType::Math);
        assert_eq!(math.content, "x^2 + 1");
        assert_eq!(math.get_quote_text(), "x^2 + 1");

        let create_plain = RichText::create_plain("abc".to_owned());
        assert_eq!(create_plain, plain("abc"));
        assert_eq!(create_plain.get_quote_text(), "abc");

        // CustomEmoji keeps its content; Icon is unquotable.
        let emoji = RichText {
            r#type: RichTextType::CustomEmoji,
            content: "\u{1F600}".to_owned(),
            custom_emoji_id: CustomEmojiId::new(7),
            ..RichText::default()
        };
        assert_eq!(emoji.get_quote_text(), "\u{1F600}");
        assert_eq!(icon().get_quote_text(), "");

        // Concatenation joins the children's quote texts.
        let concat = node(
            RichTextType::Concatenation,
            "",
            vec![node(RichTextType::Bold, "", vec![plain("bo")]), plain("ld")],
        );
        assert_eq!(concat.get_quote_text(), "bold");

        // Diff quotes only the new text.
        let diff = node(RichTextType::Diff, "", vec![plain("new"), plain("old")]);
        assert_eq!(diff.get_quote_text(), "new");
    }

    /// `get_quote_fragment` (upstream 23c7272d7, 1e7c9a1a8, dbeb09f45) —
    /// the UTF-8-code-point slicing and per-node-kind shapes.
    #[test]
    fn rich_text_get_quote_fragment() {
        // Plain slices by code points; empty and out-of-range windows are
        // empty fragments.
        let hello = plain("hello world");
        assert_eq!(hello.get_quote_fragment(0, 5).get_quote_text(), "hello");
        assert_eq!(hello.get_quote_fragment(6, 11).get_quote_text(), "world");
        assert_eq!(hello.get_quote_fragment(3, 8).get_quote_text(), "lo wo");
        assert!(hello.get_quote_fragment(5, 5).empty());
        assert!(hello.get_quote_fragment(50, 60).empty());
        assert_eq!(plain("").get_quote_fragment(0, 5).get_quote_text(), "");

        // Multibyte characters stay whole.
        let emoji_text = plain("\u{1F600}\u{1F601}");
        assert_eq!(
            emoji_text.get_quote_fragment(0, 1).get_quote_text(),
            "\u{1F600}"
        );
        assert_eq!(
            emoji_text.get_quote_fragment(1, 2).get_quote_text(),
            "\u{1F601}"
        );

        // Concatenation keeps the non-empty fragments in order: the
        // (1, 5) window of "ab|cd|ef" is cut per child — the second child
        // lies fully inside and survives whole.
        let concat = node(
            RichTextType::Concatenation,
            "",
            vec![plain("ab"), plain("cd"), plain("ef")],
        );
        let fragment = concat.get_quote_fragment(1, 5);
        assert_eq!(fragment.r#type, RichTextType::Concatenation);
        let texts: Vec<String> = fragment.texts.iter().map(|t| t.get_quote_text()).collect();
        assert_eq!(texts, vec!["b", "cd", "e"]);

        // The keep-formatting default arm clones the node (skip_texts) and
        // re-fills it with the fragment: Bold survives, the window cut
        // applies to the inner Plain.
        let bold = node(RichTextType::Bold, "", vec![plain("abcd")]);
        let fragment = bold.get_quote_fragment(1, 3);
        assert_eq!(fragment.r#type, RichTextType::Bold);
        assert_eq!(fragment.texts.len(), 1);
        assert_eq!(fragment.texts[0].get_quote_text(), "bc");

        // The formatting-is-dropped case list keeps only the inner text:
        // Url with a cut window, Diff from the new text only, Button via
        // its single text.
        let url = node(
            RichTextType::Url,
            "https://example.com",
            vec![plain("link")],
        );
        let fragment = url.get_quote_fragment(0, 4);
        assert_eq!(fragment.r#type, RichTextType::Plain);
        assert_eq!(fragment.get_quote_text(), "link");
        let diff = node(RichTextType::Diff, "", vec![plain("new"), plain("old")]);
        let fragment = diff.get_quote_fragment(1, 2);
        assert_eq!(fragment.get_quote_text(), "e");
        let button = node(RichTextType::Button, "", vec![plain("Press")]);
        assert_eq!(button.get_quote_fragment(0, 5).get_quote_text(), "Press");

        // Math/CustomEmoji are all-or-nothing (cloned whole when fully
        // inside the window, dropped otherwise).
        let math = RichText::create_math("x^2".to_owned());
        let fragment = math.get_quote_fragment(0, 3);
        assert_eq!(fragment.r#type, RichTextType::Math);
        assert_eq!(fragment.content, "x^2");
        assert!(math.get_quote_fragment(0, 2).empty());
        assert!(math.get_quote_fragment(1, 3).empty());

        // Icons are never quoted.
        assert!(icon().get_quote_fragment(0, 100).empty());
    }

    /// `remove_unallowed_quote_entities` (upstream fd3863de4) — which
    /// formatting a quote keeps and the changed flag.
    #[test]
    fn rich_text_remove_unallowed_quote_entities() {
        // Plain/Math/CustomEmoji are kept as-is.
        let mut text = plain("abc");
        assert!(!text.remove_unallowed_quote_entities());
        assert_eq!(text, plain("abc"));
        let mut text = RichText::create_math("x".to_owned());
        assert!(!text.remove_unallowed_quote_entities());
        assert_eq!(text, RichText::create_math("x".to_owned()));

        // Icon is dropped.
        let mut text = icon();
        assert!(text.remove_unallowed_quote_entities());
        assert!(text.empty());

        // A dropped-format node unwraps to its single text.
        let mut text = node(
            RichTextType::Url,
            "https://example.com",
            vec![plain("link")],
        );
        assert!(text.remove_unallowed_quote_entities());
        assert_eq!(text, plain("link"));

        // Concatenation strips per child and drops the emptied ones: the
        // Icon dies and is removed, while the Bold child is allowed
        // formatting and stays.
        let mut text = node(
            RichTextType::Concatenation,
            "",
            vec![
                node(RichTextType::Bold, "", vec![plain("a")]),
                icon(),
                plain("b"),
            ],
        );
        assert!(text.remove_unallowed_quote_entities());
        assert_eq!(
            text,
            node(
                RichTextType::Concatenation,
                "",
                vec![node(RichTextType::Bold, "", vec![plain("a")]), plain("b")]
            )
        );

        // An all-emptied Concatenation collapses to the empty node.
        let mut text = node(RichTextType::Concatenation, "", vec![icon()]);
        assert!(text.remove_unallowed_quote_entities());
        assert!(text.empty());

        // The allowed formatting (the default arm) survives unchanged:
        // Bold/Italic/Underline/Strikethrough/Spoiler/DateTime keep their
        // node, the child is left untouched.
        let mut text = node(RichTextType::Bold, "", vec![plain("keep")]);
        assert!(!text.remove_unallowed_quote_entities());
        assert_eq!(text, node(RichTextType::Bold, "", vec![plain("keep")]));
    }

    /// `for_each_quote_text` + `for_each_web_page_blocks_quote_text`
    /// (upstream ecb7fb163) — the quotable-text walk order per block kind.
    #[test]
    fn for_each_web_page_blocks_quote_text_walk() {
        // The virtual default answers for_each_rich_text(false, …): the
        // top-level node only; Divider contributes nothing; Math
        // contributes its create_math source; ButtonRow nothing.
        let blocks = vec![
            WebPageBlock::Paragraph(plain("para")),
            WebPageBlock::Divider,
            WebPageBlock::Math {
                source: "x^2".to_owned(),
            },
            WebPageBlock::ButtonRow(WebPageBlockButtonRow::default()),
        ];
        let mut collected = Vec::new();
        for_each_web_page_blocks_quote_text(&blocks, &mut |text| {
            collected.push((text.r#type, text.get_quote_text()));
        });
        assert_eq!(
            collected,
            vec![
                (RichTextType::Plain, "para".to_owned()),
                (RichTextType::Math, "x^2".to_owned()),
            ]
        );

        // Cover delegates to the inner block; List and Details recurse
        // into their sub-blocks after the header.
        let blocks = vec![
            WebPageBlock::Cover(Box::new(WebPageBlock::Header(plain("cover")))),
            WebPageBlock::List(WebPageBlockList {
                items: vec![WebPageBlockListItem {
                    page_blocks: vec![WebPageBlock::Paragraph(plain("item"))],
                    ..WebPageBlockListItem::default()
                }],
                ..WebPageBlockList::default()
            }),
            WebPageBlock::Details {
                header: plain("details"),
                page_blocks: vec![WebPageBlock::Paragraph(plain("inner"))],
                is_open: false,
            },
        ];
        let mut collected = Vec::new();
        for_each_web_page_blocks_quote_text(&blocks, &mut |text| {
            collected.push(text.get_quote_text());
        });
        assert_eq!(collected, vec!["cover", "item", "details", "inner"]);

        // EmbeddedPost walks the sub-blocks, then the caption's top-level
        // texts (recurse_text=false keeps the caption node itself).
        let post = WebPageBlock::EmbeddedPost {
            url: String::new(),
            author: String::new(),
            author_photo: Photo::default(),
            date: 0,
            page_blocks: vec![WebPageBlock::Paragraph(plain("post"))],
            caption: WebPageBlockCaption {
                text: plain("caption"),
                credit: plain("credit"),
            },
        };
        let mut collected = Vec::new();
        post.for_each_quote_text(&mut |text| collected.push(text.get_quote_text()));
        assert_eq!(collected, vec!["post", "caption", "credit"]);

        // BlockQuoteBlocks has the same blocks-then-caption order.
        let quote = WebPageBlock::BlockQuoteBlocks {
            page_blocks: vec![WebPageBlock::Paragraph(plain("quoted"))],
            caption: plain("cap"),
        };
        let mut collected = Vec::new();
        quote.for_each_quote_text(&mut |text| collected.push(text.get_quote_text()));
        assert_eq!(collected, vec!["quoted", "cap"]);
    }

    /// `remove_empty_media_page_blocks` (upstream a7ccc2344) — the
    /// fileless media blocks drop, the nested containers recurse.
    #[test]
    fn remove_empty_media_page_blocks_drops_fileless_media() {
        let caption = WebPageBlockCaption {
            text: plain("cap"),
            credit: plain("credit"),
        };
        let non_empty_photo = crate::photo::Photo {
            id: 1,
            ..crate::photo::Photo::default()
        };
        // A fileless Animation, a real Photo, a fileless Document and a
        // paragraph survive-or-drop per their media validity.
        let mut blocks = vec![
            WebPageBlock::Animation {
                animation_file_id: FileId::default(),
                caption: caption.clone(),
                need_autoplay: false,
                has_spoiler: false,
            },
            WebPageBlock::Photo {
                photo: non_empty_photo,
                caption: caption.clone(),
                url: String::new(),
                web_page_id: WebPageId::default(),
                has_spoiler: false,
            },
            WebPageBlock::Paragraph(plain("kept")),
            WebPageBlock::Document {
                document_file_id: FileId::default(),
                caption: caption.clone(),
            },
        ];
        remove_empty_media_page_blocks(&mut blocks);
        assert_eq!(blocks.len(), 2);
        assert!(matches!(blocks[0], WebPageBlock::Photo { .. }));
        assert!(matches!(blocks[1], WebPageBlock::Paragraph(_)));

        // An emptied Collage drops itself (the last live block dies), a
        // Cover delegates to its inner block, and List/Details/
        // BlockQuoteBlocks recurse without dropping themselves.
        let mut blocks = vec![
            WebPageBlock::Collage {
                page_blocks: vec![WebPageBlock::VoiceNote {
                    voice_note_file_id: FileId::default(),
                    caption: caption.clone(),
                }],
                caption: caption.clone(),
            },
            WebPageBlock::Cover(Box::new(WebPageBlock::Audio {
                audio_file_id: FileId::default(),
                caption: caption.clone(),
            })),
            WebPageBlock::List(WebPageBlockList {
                items: vec![WebPageBlockListItem {
                    page_blocks: vec![WebPageBlock::Video {
                        video_file_id: FileId::default(),
                        caption: caption.clone(),
                        need_autoplay: false,
                        is_looped: false,
                        has_spoiler: false,
                    }],
                    ..WebPageBlockListItem::default()
                }],
                ..WebPageBlockList::default()
            }),
            WebPageBlock::Details {
                header: plain("h"),
                page_blocks: Vec::new(),
                is_open: true,
            },
        ];
        remove_empty_media_page_blocks(&mut blocks);
        // the Collage and the Audio-covered Cover die; the emptied List
        // and Details survive — containers never self-drop outside
        // Collage/Slideshow.
        assert_eq!(blocks.len(), 2);
        assert!(matches!(blocks[0], WebPageBlock::List(_)));
        assert!(matches!(
            blocks[1],
            WebPageBlock::Details { is_open: true, .. }
        ));
        let WebPageBlock::List(list) = &blocks[0] else {
            unreachable!()
        };
        assert!(list.items[0].page_blocks.is_empty());
    }

    /// `RichText::add_dependencies` (upstream 4e1635ac8) — the recursive
    /// user/button id walk the quote plane feeds rich texts through.
    #[test]
    fn rich_text_add_dependencies_walks_users() {
        #[derive(Default)]
        struct TestSink(std::collections::BTreeSet<crate::user_manager::UserId>);
        impl crate::inline_keyboard_button::DependenciesSink for TestSink {
            fn add_user(&mut self, user_id: crate::user_manager::UserId) {
                // the real Dependencies::add(UserId) filters invalid ids
                if user_id.is_valid() {
                    self.0.insert(user_id);
                }
            }
        }
        let text = node(
            RichTextType::Concatenation,
            "",
            vec![
                RichText {
                    r#type: RichTextType::MentionName,
                    user_id: crate::user_manager::UserId::new(7),
                    ..RichText::default()
                },
                plain("no users"),
            ],
        );
        let mut sink = TestSink::default();
        text.add_dependencies(&mut sink);
        assert_eq!(sink.0, [crate::user_manager::UserId::new(7)].into());
    }

    /// `get_quote_rich_texts_object` (upstream 4e1635ac8/1631ed57b) —
    /// the td_api objects of the quote's rich texts over the default
    /// object context.
    #[test]
    fn get_quote_rich_texts_object_builds_objects() {
        let rich_texts = vec![
            plain("hi"),
            node(RichTextType::Bold, "", vec![plain("b")]),
            RichText::create_math("x".to_owned()),
            RichText {
                r#type: RichTextType::CustomEmoji,
                content: "\u{1F600}".to_owned(),
                custom_emoji_id: CustomEmojiId::new(42),
                ..RichText::default()
            },
            node(
                RichTextType::Concatenation,
                "",
                vec![plain("a"), plain("b")],
            ),
        ];
        let objects = get_quote_rich_texts_object(&rich_texts);
        assert_eq!(objects.len(), 5);
        assert!(matches!(
            objects[0].as_ref(),
            td_api::RichText::richTextPlain(t) if t.text == "hi"
        ));
        assert!(matches!(
            objects[1].as_ref(),
            td_api::RichText::richTextBold(_)
        ));
        assert!(matches!(
            objects[2].as_ref(),
            td_api::RichText::richTextMathematicalExpression(t) if t.expression == "x"
        ));
        assert!(matches!(
            objects[3].as_ref(),
            td_api::RichText::richTextCustomEmoji(t) if t.custom_emoji_id == 42
        ));
        assert!(matches!(
            objects[4].as_ref(),
            td_api::RichText::richTexts(t) if t.texts.len() == 2
        ));
    }

    #[test]
    fn web_page_id_basics() {
        let id = WebPageId::new(123);
        assert_eq!(id.get(), 123);
        assert!(id.is_valid());
        assert!(!WebPageId::default().is_valid());
        assert_eq!(id.to_string(), "link preview 123");
        let mut out = Vec::new();
        id.store(&mut out);
        assert_eq!(out, 123i64.to_le_bytes());
        let mut parser = Parser::new(&out);
        assert_eq!(WebPageId::parse(&mut parser).unwrap(), id);
    }

    #[test]
    fn rich_text_plain_round_trip() {
        round_trip(
            |env, out| {
                plain("hello, world").store(env, out);
            },
            RichText::parse,
            |text| {
                assert_eq!(text.r#type, RichTextType::Plain);
                assert_eq!(text.content, "hello, world");
                assert!(text.texts.is_empty());
                assert!(!text.empty());
            },
        );
        // the default rich text is the empty plain
        assert!(RichText::default().empty());
    }

    #[test]
    fn rich_text_all_typed_fields_round_trip() {
        // the per-type fields: each typed rich text stores exactly the rows
        // the C++ type guard persists
        let rich_text = RichText {
            r#type: RichTextType::Button,
            texts: vec![
                plain("bold"),
                RichText {
                    r#type: RichTextType::Url,
                    content: "https://example.com".to_owned(),
                    texts: vec![plain("link")],
                    web_page_id: WebPageId::new(456),
                    ..RichText::default()
                },
            ],
            button_style: RichButtonStyle::from_td_api(Some(
                td_api::ButtonStyle::buttonStyleDanger(td_api::buttonStyleDanger {}),
            )),
            button: Some(InlineKeyboardButton {
                type_: InlineKeyboardButtonType::UrlAuth,
                id: -12345,
                user_id: UserId::new(77),
                style: KeyboardButtonStyle::from_td_api(
                    Some(td_api::ButtonStyle::buttonStylePrimary(
                        td_api::buttonStylePrimary {},
                    )),
                    536870912,
                ),
                text: "Press me".to_owned(),
                forward_text: "Forward!".to_owned(),
                data: "https://example.com".to_owned(),
            }),
            ..RichText::default()
        };
        round_trip(
            |env, out| rich_text.store(env, out),
            RichText::parse,
            |parsed| {
                assert_eq!(parsed, rich_text);
            },
        );

        // the scalar typed fields each survive their own type guard
        let icon = RichText {
            r#type: RichTextType::Icon,
            content: (640u32 * 65536 + 480).to_string(),
            document_file_id: FileId::new(7, 0),
            ..RichText::default()
        };
        round_trip(
            |env, out| icon.store(env, out),
            RichText::parse,
            |parsed| assert_eq!(parsed, icon),
        );
        let url = RichText {
            r#type: RichTextType::Url,
            content: "https://example.com".to_owned(),
            texts: vec![plain("link")],
            web_page_id: WebPageId::new(89),
            ..RichText::default()
        };
        round_trip(
            |env, out| url.store(env, out),
            RichText::parse,
            |parsed| assert_eq!(parsed, url),
        );
        let emoji = RichText {
            r#type: RichTextType::CustomEmoji,
            content: "alt".to_owned(),
            custom_emoji_id: CustomEmojiId::new(536870912),
            ..RichText::default()
        };
        round_trip(
            |env, out| emoji.store(env, out),
            RichText::parse,
            |parsed| assert_eq!(parsed, emoji),
        );
        let date = RichText {
            r#type: RichTextType::FormattedDate,
            texts: vec![plain("d")],
            date: FormattedDate::from_raw(1700000000, 3),
            ..RichText::default()
        };
        round_trip(
            |env, out| date.store(env, out),
            RichText::parse,
            |parsed| assert_eq!(parsed, date),
        );
        let mention_name = RichText {
            r#type: RichTextType::MentionName,
            texts: vec![plain("m")],
            user_id: UserId::new(42),
            ..RichText::default()
        };
        round_trip(
            |env, out| mention_name.store(env, out),
            RichText::parse,
            |parsed| assert_eq!(parsed, mention_name),
        );
    }

    #[test]
    fn rich_text_icon_recovers_from_bad_file_id() {
        // an Icon whose stored file id is invalid resets to the empty plain,
        // exactly like the C++ parse
        round_trip(
            |_env, out| {
                // hand-crafting the raw record with file id 0
                store_i32_raw(out, RichTextType::Icon as i32);
                crate::store_parse::store_string(out, "");
                crate::store_parse::store_i32(out, 0); // no texts
                crate::store_parse::store_i32(out, 0); // the invalid file id
            },
            RichText::parse,
            |parsed| {
                assert!(parsed.empty());
                assert_eq!(parsed.r#type, RichTextType::Plain);
            },
        );
    }

    fn store_i32_raw(out: &mut Vec<u8>, value: i32) {
        crate::store_parse::store_i32(out, value);
    }

    #[test]
    fn ordered_list_labels() {
        assert_eq!(ordered_list_label_for_test(1, "1"), "1.");
        assert_eq!(ordered_list_label_for_test(27, "1"), "27.");
        assert_eq!(ordered_list_label_for_test(1, "a"), "a.");
        assert_eq!(ordered_list_label_for_test(26, "a"), "z.");
        assert_eq!(ordered_list_label_for_test(27, "a"), "aa.");
        assert_eq!(ordered_list_label_for_test(1, "A"), "A.");
        assert_eq!(ordered_list_label_for_test(1, "i"), "i.");
        assert_eq!(ordered_list_label_for_test(4, "i"), "iv.");
        assert_eq!(ordered_list_label_for_test(9, "i"), "ix.");
        assert_eq!(ordered_list_label_for_test(14, "i"), "xiv.");
        assert_eq!(ordered_list_label_for_test(1900, "I"), "MCM.");
        assert_eq!(ordered_list_label_for_test(4000, "i"), "4000.");
        assert_eq!(ordered_list_label_for_test(2, "q"), "2.");
    }

    #[test]
    fn caption_and_table_cell_round_trip() {
        let caption = WebPageBlockCaption {
            text: plain("caption"),
            credit: plain("(c) someone"),
        };
        round_trip(
            |env, out| caption.store(env, out),
            WebPageBlockCaption::parse,
            |parsed| assert_eq!(parsed, caption),
        );

        let cell = WebPageBlockTableCell {
            text: plain("cell"),
            is_header: true,
            align_left: false,
            align_center: false,
            align_right: true,
            valign_top: false,
            valign_middle: false,
            valign_bottom: true,
            colspan: 3,
            rowspan: 2,
        };
        round_trip(
            |env, out| cell.store(env, out),
            WebPageBlockTableCell::parse,
            |parsed| {
                assert_eq!(parsed, cell);
                assert!(parsed.align_right && !parsed.align_left && !parsed.align_center);
            },
        );
        // the default cell stores only the flag block
        let default_cell = WebPageBlockTableCell::default();
        round_trip(
            |env, out| default_cell.store(env, out),
            WebPageBlockTableCell::parse,
            |parsed| {
                assert_eq!(parsed, default_cell);
            },
        );
    }

    #[test]
    fn related_article_round_trip() {
        let article = RelatedArticle {
            url: "https://example.com/a".to_owned(),
            web_page_id: WebPageId::new(1001),
            title: "A title".to_owned(),
            description: "A description".to_owned(),
            photo: Photo::default(),
            author: "An author".to_owned(),
            published_date: 1700000000,
        };
        round_trip(
            |env, out| article.store(env, out),
            RelatedArticle::parse,
            |parsed| {
                assert_eq!(parsed, article);
                assert!(parsed.photo.is_empty());
            },
        );
    }

    #[test]
    fn photo_record_round_trip() {
        let photo = Photo {
            id: 12345,
            date: 1700000001,
            minithumbnail: "\x01\x02\x03".to_owned(),
            photos: vec![PhotoSize {
                size_type: 'x' as i32,
                dimensions: Dimensions {
                    width: 640,
                    height: 480,
                },
                file_id: FileId::new(3, 0),
                size: 12,
                progressive_sizes: Vec::new(),
            }],
            has_stickers: true,
            sticker_file_ids: vec![FileId::new(4, 0)],
            ..Photo::default()
        };
        round_trip(
            |env, out| store_photo(env, out, &photo),
            parse_photo,
            |parsed| {
                assert_eq!(parsed.id, 12345);
                assert_eq!(parsed.date, 1700000001);
                assert_eq!(parsed.photos.len(), 1);
                assert_eq!(parsed.photos[0].file_id.get(), 3);
                assert!(parsed.has_stickers);
                assert_eq!(parsed.sticker_file_ids.len(), 1);
                assert_eq!(parsed.sticker_file_ids[0].get(), 4);
            },
        );
    }

    #[test]
    fn dialog_photo_and_dimensions_round_trip() {
        let dialog_photo = DialogPhoto {
            small_file_id: FileId::new(1, 0),
            big_file_id: FileId::new(2, 0),
            minithumbnail: String::new(),
            has_animation: true,
            is_personal: false,
        };
        round_trip(
            |env, out| store_dialog_photo(env, out, &dialog_photo),
            parse_dialog_photo,
            |parsed| {
                assert!(parsed.has_animation);
                assert!(!parsed.is_personal);
                assert_eq!(parsed.small_file_id.get(), 1);
                assert_eq!(parsed.big_file_id.get(), 2);
            },
        );

        round_trip(
            |_env, out| {
                store_dimensions(
                    out,
                    Dimensions {
                        width: 800,
                        height: 600,
                    },
                )
            },
            |_env, parser| parse_dimensions(parser),
            |parsed| {
                assert_eq!(parsed.width, 800);
                assert_eq!(parsed.height, 600);
            },
        );
    }

    #[test]
    fn text_blocks_round_trip() {
        // every one-field text block + the parametrized ones
        let blocks = vec![
            WebPageBlock::Title(plain("t")),
            WebPageBlock::Subtitle(plain("s")),
            WebPageBlock::AuthorDate {
                author: plain("a"),
                date: 1700000002,
            },
            WebPageBlock::Header(plain("h")),
            WebPageBlock::Subheader(plain("sh")),
            WebPageBlock::Paragraph(plain("p")),
            WebPageBlock::Preformatted {
                text: plain("code"),
                language: "rust".to_owned(),
            },
            WebPageBlock::Footer(plain("f")),
            WebPageBlock::Divider,
            WebPageBlock::Anchor {
                name: "anchor".to_owned(),
            },
            WebPageBlock::Heading {
                text: plain("big"),
                size: 3,
            },
            WebPageBlock::Math {
                source: "1+1".to_owned(),
            },
            WebPageBlock::Thinking(plain("hmm")),
            WebPageBlock::Kicker(plain("k")),
        ];
        for block in &blocks {
            let expected = block.clone();
            let expected_type = block.get_type();
            round_trip(
                |env, out| block.store(env, out),
                WebPageBlock::parse_block,
                move |parsed| {
                    assert_eq!(parsed.get_type(), expected_type);
                    assert_eq!(parsed, expected);
                },
            );
        }
    }

    #[test]
    fn media_and_structured_blocks_round_trip() {
        let photo = Photo {
            id: 777,
            ..Photo::default()
        };
        let caption = WebPageBlockCaption {
            text: plain("cap"),
            credit: RichText::default(),
        };
        let blocks = vec![
            WebPageBlock::Animation {
                animation_file_id: FileId::new(10, 0),
                caption: caption.clone(),
                need_autoplay: true,
                has_spoiler: true,
            },
            WebPageBlock::Animation {
                animation_file_id: FileId::default(),
                caption: caption.clone(),
                need_autoplay: false,
                has_spoiler: false,
            },
            WebPageBlock::Photo {
                photo,
                caption: caption.clone(),
                url: "https://example.com".to_owned(),
                web_page_id: WebPageId::new(5),
                has_spoiler: false,
            },
            WebPageBlock::Video {
                video_file_id: FileId::new(11, 0),
                caption: caption.clone(),
                need_autoplay: true,
                is_looped: true,
                has_spoiler: false,
            },
            WebPageBlock::Audio {
                audio_file_id: FileId::new(12, 0),
                caption: caption.clone(),
            },
            WebPageBlock::VoiceNote {
                voice_note_file_id: FileId::new(13, 0),
                caption: caption.clone(),
            },
            WebPageBlock::Document {
                document_file_id: FileId::new(14, 0),
                caption: caption.clone(),
            },
            WebPageBlock::Cover(Box::new(WebPageBlock::Paragraph(plain("cover")))),
            WebPageBlock::Embedded {
                url: "https://example.com/embed".to_owned(),
                html: "<b>hi</b>".to_owned(),
                poster_photo: Photo::default(),
                dimensions: Dimensions {
                    width: 100,
                    height: 50,
                },
                caption: caption.clone(),
                is_full_width: true,
                allow_scrolling: false,
            },
            WebPageBlock::EmbeddedPost {
                url: "https://example.com/post".to_owned(),
                author: "anon".to_owned(),
                author_photo: Photo::default(),
                date: 1700000003,
                page_blocks: vec![WebPageBlock::Paragraph(plain("post body"))],
                caption: caption.clone(),
            },
            WebPageBlock::Collage {
                page_blocks: vec![
                    WebPageBlock::Paragraph(plain("one")),
                    WebPageBlock::Paragraph(plain("two")),
                ],
                caption,
            },
            WebPageBlock::Details {
                header: plain("details"),
                page_blocks: vec![WebPageBlock::Paragraph(plain("inside"))],
                is_open: true,
            },
            WebPageBlock::BlockQuote {
                text: plain("quote"),
                credit: plain("who"),
            },
            WebPageBlock::ExpandableBlockQuote {
                text: plain("expand"),
                credit: plain("who2"),
            },
            WebPageBlock::PullQuote {
                text: plain("pull"),
                credit: plain("who3"),
            },
            WebPageBlock::BlockQuoteBlocks {
                page_blocks: vec![WebPageBlock::Paragraph(plain("quote blocks"))],
                caption: plain("caption text"),
            },
            WebPageBlock::Map {
                location: crate::chat_manager::Location::from_td_api_location(&td_api::Location {
                    latitude: 1.5,
                    longitude: 2.5,
                    horizontal_accuracy: 10.0,
                }),
                zoom: 13,
                dimensions: Dimensions {
                    width: 100,
                    height: 200,
                },
                caption: WebPageBlockCaption::default(),
            },
            WebPageBlock::ButtonRow(WebPageBlockButtonRow {
                buttons: vec![WebPageBlockRowButton {
                    text: plain("click"),
                    style: RichButtonStyle::from_td_api(Some(
                        td_api::ButtonStyle::buttonStyleSuccess(td_api::buttonStyleSuccess {}),
                    )),
                    button: InlineKeyboardButton {
                        type_: InlineKeyboardButtonType::Url,
                        id: 0,
                        user_id: UserId::default(),
                        style: KeyboardButtonStyle::default(),
                        text: "b".to_owned(),
                        forward_text: String::new(),
                        data: "https://example.com".to_owned(),
                    },
                }],
                align_left: false,
                align_center: true,
                align_right: false,
            }),
            WebPageBlock::RelatedArticles {
                header: plain("related"),
                related_articles: vec![RelatedArticle {
                    url: "u".to_owned(),
                    web_page_id: WebPageId::default(),
                    title: String::new(),
                    description: String::new(),
                    photo: Photo::default(),
                    author: String::new(),
                    published_date: 0,
                }],
            },
            WebPageBlock::List(WebPageBlockList {
                items: vec![
                    WebPageBlockListItem {
                        label: String::new(),
                        page_blocks: vec![WebPageBlock::Paragraph(plain("bullet"))],
                        has_checkbox: true,
                        is_checked: true,
                        value: -1,
                        r#type: String::new(),
                    },
                    WebPageBlockListItem {
                        label: "1.".to_owned(),
                        page_blocks: vec![WebPageBlock::Paragraph(plain("ordered"))],
                        has_checkbox: false,
                        is_checked: false,
                        value: 1,
                        r#type: "1".to_owned(),
                    },
                ],
                start: 5,
                is_reversed: true,
                r#type: "a".to_owned(),
            }),
            WebPageBlock::Table(WebPageBlockTable {
                title: plain("table"),
                cells: vec![vec![
                    WebPageBlockTableCell {
                        text: plain("r0c0"),
                        is_header: true,
                        align_left: true,
                        align_center: false,
                        align_right: false,
                        valign_top: false,
                        valign_middle: false,
                        valign_bottom: false,
                        colspan: 1,
                        rowspan: 1,
                    },
                    WebPageBlockTableCell::default(),
                ]],
                is_bordered: true,
                is_striped: true,
                is_compact: false,
            }),
            WebPageBlock::ChatLink(ChatLinkInfo {
                title: "Channel".to_owned(),
                photo: DialogPhoto::default(),
                username: "example".to_owned(),
                accent_color_id: crate::peer_color::AccentColorId::new(3),
                channel_id: crate::chat_manager::ChannelId::new(1001),
            }),
            WebPageBlock::Unsupported { version: 1 },
        ];
        for block in &blocks {
            let expected = block.clone();
            let expected_type = block.get_type();
            round_trip(
                |env, out| block.store(env, out),
                WebPageBlock::parse_block,
                move |parsed| {
                    assert_eq!(parsed.get_type(), expected_type);
                    assert_eq!(parsed, expected);
                },
            );
        }
    }

    #[test]
    fn unsupported_block_version_drives_need_reget() {
        let current = WebPageBlock::Unsupported { version: 1 };
        assert!(!current.need_reget());
        let stale = WebPageBlock::Unsupported { version: 0 };
        assert!(stale.need_reget());
        let covered = WebPageBlock::Cover(Box::new(stale));
        assert!(covered.need_reget());
    }

    #[test]
    fn heading_size_clamps_on_parse() {
        round_trip(
            |env, out| {
                // hand-craft a Heading with size 9 (out of the 1..=6 range);
                // the empty flag block precedes the body like the C++ store
                crate::store_parse::store_i32(out, BlockType::Heading as i32);
                crate::store_parse::store_flags(out, []);
                plain("x").store(env, out);
                crate::store_parse::store_i32(out, 9);
            },
            WebPageBlock::parse_block,
            |parsed| match parsed {
                WebPageBlock::Heading { size, .. } => assert_eq!(size, 6),
                other => panic!("wrong block {other:?}"),
            },
        );
    }

    #[test]
    fn unknown_block_type_rejected() {
        let mut env = TestEnv::default();
        let mut out = Vec::new();
        crate::store_parse::store_i32(&mut out, 37); // == BlockType::Size
        let mut parser = Parser::new(&out);
        let error = WebPageBlock::parse_block(&mut env, &mut parser).unwrap_err();
        assert_eq!(error, "Can't parse unknown BlockType");
    }

    #[test]
    fn rich_text_object_plain_and_anchor_two_pass() {
        // the two-pass anchor resolution: an Anchor named "a1" and a URL
        // "#a1" pointing at it resolve into richTextAnchorLink/richTextReference
        let blocks = vec![
            WebPageBlock::Anchor {
                name: "a1".to_owned(),
            },
            WebPageBlock::Paragraph(RichText {
                r#type: RichTextType::Url,
                content: "https://example.com/page#a1".to_owned(),
                texts: vec![plain("go")],
                ..RichText::default()
            }),
        ];
        let mut env = TestEnv::default();
        let mut user_manager = TestUserManager;
        let objects = get_page_blocks_object(
            &blocks,
            &mut env,
            &mut user_manager,
            "https://example.com/page",
            "",
            true,
        );
        assert_eq!(objects.len(), 2);
        // the anchor object + the resolved url object
        let td_api::PageBlock::pageBlockParagraph(paragraph) = objects[1].as_ref() else {
            panic!("expected the paragraph block");
        };
        let Some(text) = paragraph.text.as_deref() else {
            panic!("expected the paragraph text");
        };
        match text {
            td_api::RichText::richTextAnchorLink(link) => {
                assert_eq!(link.anchor_name, "a1");
                assert_eq!(link.url, "https://example.com/page#a1");
            }
            other => panic!("expected the anchor link resolution, got {other:?}"),
        }
    }

    /// The minimal [`InlineKeyboardButtonEnv`] for the object builders.
    struct TestUserManager;

    impl crate::inline_keyboard_button::InlineKeyboardButtonEnv for TestUserManager {
        fn is_test_dc(&self) -> bool {
            false
        }

        fn get_input_user(
            &self,
            _user_id: UserId,
        ) -> std::result::Result<tdmtproto::telegram_api::InputUser, tdutils::status::Status>
        {
            Ok(tdmtproto::telegram_api::InputUser::inputUserEmpty(
                tdmtproto::telegram_api::inputUserEmpty {},
            ))
        }

        fn is_my_user_bot(&self) -> bool {
            false
        }

        fn get_user_id_object(&self, user_id: UserId, _source: &str) -> i64 {
            user_id.get()
        }
    }

    #[test]
    fn wire_rich_text_intake_text_url() {
        let documents = DocumentsById::new();
        let wire = tdmtproto::telegram_api::RichText::textUrl(tdmtproto::telegram_api::textUrl {
            text: Some(Box::new(tdmtproto::telegram_api::RichText::textPlain(
                tdmtproto::telegram_api::textPlain {
                    text: "click".to_owned(),
                },
            ))),
            url: "https://example.com".to_owned(),
            webpage_id: 4242,
        });
        let rich_text = get_rich_text_from_telegram(wire, &documents);
        assert_eq!(rich_text.r#type, RichTextType::Url);
        assert_eq!(rich_text.content, "https://example.com");
        assert_eq!(rich_text.web_page_id, WebPageId::new(4242));
        assert_eq!(rich_text.texts.len(), 1);
        assert_eq!(rich_text.get_full_text(), "click");
    }

    #[test]
    fn wire_rich_text_intake_icon() {
        let mut documents = DocumentsById::new();
        documents.insert(555, FileId::new(9, 0));
        let wire =
            tdmtproto::telegram_api::RichText::textImage(tdmtproto::telegram_api::textImage {
                document_id: 555,
                w: 640,
                h: 480,
            });
        let rich_text = get_rich_text_from_telegram(wire, &documents);
        assert_eq!(rich_text.r#type, RichTextType::Icon);
        assert_eq!(rich_text.document_file_id, FileId::new(9, 0));
        // content packs width * 65536 + height
        assert_eq!(rich_text.content, (640u32 * 65536 + 480).to_string());
    }

    #[test]
    fn get_rich_text_from_td_api_validation() {
        let mut env = TestEnv::default();
        // bad UTF-8 is impossible through String; the emoji-id validation
        // and the icon ban are the observable checks
        let error = get_rich_text_from_td_api(
            &mut env,
            Some(Box::new(td_api::RichText::richTextCustomEmoji(
                td_api::richTextCustomEmoji {
                    custom_emoji_id: 0,
                    alternative_text: "alt".to_owned(),
                },
            ))),
        )
        .unwrap_err();
        assert_eq!(error.code(), 400);
        assert_eq!(error.message(), "Invalid custom emoji identifier specified");

        let error = get_rich_text_from_td_api(
            &mut env,
            Some(Box::new(td_api::RichText::richTextIcon(
                td_api::richTextIcon {
                    document: None,
                    width: 1,
                    height: 1,
                },
            ))),
        )
        .unwrap_err();
        assert_eq!(error.message(), "Block is not allowed");

        // the mention passthrough collapses to the inner text
        let mention = get_rich_text_from_td_api(
            &mut env,
            Some(Box::new(td_api::RichText::richTextMention(
                td_api::richTextMention {
                    text: Some(Box::new(td_api::RichText::richTextPlain(
                        td_api::richTextPlain {
                            text: "@user".to_owned(),
                        },
                    ))),
                    username: "@user".to_owned(),
                },
            ))),
        )
        .unwrap();
        assert_eq!(mention.r#type, RichTextType::Plain);
        assert_eq!(mention.content, "@user");
    }

    #[test]
    fn get_index_mask_over_url_family() {
        let mut url = plain("");
        url.r#type = RichTextType::Url;
        url.content = "https://example.com".to_owned();
        let url_mask = crate::dialog_id::message_search_filter_index_mask(
            crate::dialog_id::MessageSearchFilter::Url,
        );
        assert_eq!(url.get_index_mask(), url_mask);
        // anchor urls index nothing
        url.content = "#anchor".to_owned();
        assert_eq!(url.get_index_mask(), 0);
        // hashtags/plain index nothing
        assert_eq!(plain("#tag").get_index_mask(), 0);
    }

    #[test]
    fn inline_keyboard_button_store_parse_round_trip() {
        let button = InlineKeyboardButton {
            type_: InlineKeyboardButtonType::SwitchInline,
            id: TargetDialogTypes::from_mask(0).get_mask(),
            user_id: UserId::default(),
            style: KeyboardButtonStyle::from_td_api(None, 536870912),
            text: "switch".to_owned(),
            forward_text: String::new(),
            data: "query".to_owned(),
        };
        let mut out = Vec::new();
        button.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = InlineKeyboardButton::parse(&mut parser).unwrap();
        parser.fetch_end().unwrap();
        assert_eq!(parsed, button);
    }

    #[test]
    fn get_inline_keyboard_button_from_td_api_url() {
        let button = InlineKeyboardButton::from_td_api(
            td_api::InlineKeyboardButton {
                text: "go".to_owned(),
                icon_custom_emoji_id: 0,
                style: None,
                r#type: Some(
                    td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeUrl(
                        td_api::inlineKeyboardButtonTypeUrl {
                            url: "https://example.com".to_owned(),
                        },
                    ),
                ),
            },
            true,
            false,
        )
        .unwrap();
        assert_eq!(button.type_, InlineKeyboardButtonType::Url);
        // the ported check_link normalizes the host form with the trailing slash
        assert_eq!(button.data, "https://example.com/");
        assert_eq!(button.text, "go");
    }

    #[test]
    fn wire_page_block_intake_paragraph_and_list() {
        let mut env = TestEnv::default();
        let empty = DocumentsById::new();
        let photos = PhotosById::new();

        let paragraph = get_web_page_block(
            &mut env,
            tdmtproto::telegram_api::PageBlock::pageBlockParagraph(
                tdmtproto::telegram_api::pageBlockParagraph {
                    text: Some(Box::new(tdmtproto::telegram_api::RichText::textPlain(
                        tdmtproto::telegram_api::textPlain {
                            text: "para".to_owned(),
                        },
                    ))),
                },
            ),
            &empty,
            &empty,
            &empty,
            &photos,
            &empty,
            &empty,
        )
        .unwrap();
        match paragraph {
            WebPageBlock::Paragraph(text) => assert_eq!(text.content, "para"),
            other => panic!("wrong block {other:?}"),
        }

        let list = get_web_page_block(
            &mut env,
            tdmtproto::telegram_api::PageBlock::pageBlockList(
                tdmtproto::telegram_api::pageBlockList {
                    items: vec![Box::new(
                        tdmtproto::telegram_api::PageListItem::pageListItemText(
                            tdmtproto::telegram_api::pageListItemText {
                                flags: 0,
                                checkbox: true,
                                checked: false,
                                text: Some(Box::new(tdmtproto::telegram_api::RichText::textPlain(
                                    tdmtproto::telegram_api::textPlain {
                                        text: "item".to_owned(),
                                    },
                                ))),
                            },
                        ),
                    )],
                },
            ),
            &empty,
            &empty,
            &empty,
            &photos,
            &empty,
            &empty,
        )
        .unwrap();
        match list {
            WebPageBlock::List(list) => {
                assert_eq!(list.items.len(), 1);
                assert!(list.items[0].has_checkbox);
                assert!(!list.items[0].is_checked);
                assert_eq!(list.items[0].label, "");
            }
            other => panic!("wrong block {other:?}"),
        }

        // the unsupported constructor drops to None -> the vector wraps it
        let blocks = get_web_page_blocks_from_telegram(
            &mut env,
            vec![Box::new(
                tdmtproto::telegram_api::PageBlock::pageBlockUnsupported(
                    tdmtproto::telegram_api::pageBlockUnsupported {},
                ),
            )],
            &empty,
            &empty,
            &empty,
            &photos,
            &empty,
            &empty,
        );
        assert_eq!(blocks.len(), 1);
        assert!(matches!(
            blocks[0],
            WebPageBlock::Unsupported { version: 1 }
        ));
    }

    #[test]
    fn version_constants_match_the_cxx_enum() {
        use crate::store_parse::version as v;
        assert_eq!(v::FIX_WEB_PAGE_INSTANT_VIEW_DATABASE, 9);
        assert_eq!(v::FIX_PAGE_BLOCK_AUDIO_EMPTY_FILE, 11);
        assert_eq!(v::SUPPORT_INSTANT_VIEW_2_0, 18);
        assert_eq!(v::ADD_KEYBOARD_BUTTON_FLAGS, 35);
        assert_eq!(v::ADD_PAGE_BLOCK_CHAT_LINK_FLAGS, 50);
        assert_eq!(v::SUPPORT_RICH_MESSAGES, 60);
        assert_eq!(v::NEXT, 62);
    }

    #[test]
    fn log_event_version_framing_still_pins() {
        // the store/parse of the blocks is written behind the log-event
        // version prefix in production; pin the framing here
        let mut out = Vec::new();
        crate::store_parse::store_log_event_version(&mut out);
        let mut parser = Parser::new_log_event(&out).unwrap();
        assert_eq!(parser.version(), version::LOG_EVENT_STORE_VERSION);
        assert_eq!(version::LOG_EVENT_STORE_VERSION, version::NEXT - 1);
        let _ = &mut parser;
    }
}
