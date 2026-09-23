//!
//!

use crate::chat_manager::{MessageId, MessageType};
use crate::dialog_id::{DialogId, MessageFullId};
use crate::peer_color::{AccentColorId, PeerColor};
use crate::photo::Photo;
use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::{Result, Status};

/// The cache-expiry window of the three `delete_cached_*` timeouts
/// (SponsoredMessageManager.cpp:573, 725, 881).
const DELETE_CACHED_SPONSORED_TIMEOUT: f64 = 300.0;

/// The MessageContent stand-in (`[blocked: M.10 integration]`): the
/// manager needs the content type (the allowed-type gate), the text
/// (the video-advertisement builder) and the self-destruct marker (the
/// ttl gate). The full content plane replaces it at the integration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SponsoredContent {
    content_type: SponsoredContentType,
    text: String,
    has_ttl: bool,
}

impl SponsoredContent {
    /// The env-side constructor of the M.10 seam stand-in
    /// (`get_message_content` collapses into the content-type tag, the
    /// raw text and the self-destruct flag).
    pub(crate) fn new(
        content_type: SponsoredContentType,
        text: String,
        has_ttl: bool,
    ) -> SponsoredContent {
        SponsoredContent {
            content_type,
            text,
            has_ttl,
        }
    }

    /// `content->get_type()`.
    pub fn get_type(&self) -> SponsoredContentType {
        self.content_type
    }

    /// The text (`get_message_content_text(content)->text`).
    pub fn get_text(&self) -> &str {
        &self.text
    }

    /// The self-destruct marker (`!ttl.is_empty()`).
    pub fn has_ttl(&self) -> bool {
        self.has_ttl
    }
}

/// The MessageContentType slice the sponsored gates distinguish
/// (SponsoredMessageManager.cpp:517-527 and 842): Animation, Photo, Text
/// and Video are the allowed regular types; only Text passes the video
/// gate. Everything else (and any unmodeled type) collapses to
/// Unsupported, which both gates reject.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SponsoredContentType {
    #[default]
    Unsupported,
    Animation,
    Photo,
    Text,
    Video,
}

/// `SponsoredMessageManager::SponsoredContentInfo`
/// (SponsoredMessageManager.cpp:202-206) — the per-item click/report
/// bookkeeping over the server random_id. The C++ stores the id as a
/// `string` of raw bytes; the Rust keeps the bytes (a lossy UTF-8
/// rewrite would diverge).
#[derive(Clone, Debug, Default)]
pub(crate) struct SponsoredContentInfo {
    random_id: Vec<u8>,
    is_viewed: bool,
    is_clicked: bool,
}

/// `SponsoredMessageManager::SponsoredMessage`
/// (SponsoredMessageManager.cpp:208-240) over the content stand-in.
#[derive(Clone, Debug)]
pub(crate) struct SponsoredMessage {
    local_id: i64,
    is_recommended: bool,
    can_be_reported: bool,
    content: SponsoredContent,
    min_display_duration: i32,
    max_display_duration: i32,
    url: String,
    photo: Photo,
    title: String,
    button_text: String,
    peer_color: PeerColor,
    sponsor_info: String,
    additional_info: String,
}

/// `SponsoredMessageManager::DialogSponsoredMessages`
/// (SponsoredMessageManager.cpp:242-249).
#[derive(Default)]
pub(crate) struct DialogSponsoredMessages {
    promises: Vec<Promise<td_api::SponsoredMessages>>,
    messages: Vec<SponsoredMessage>,
    message_infos: std::collections::HashMap<i64, SponsoredContentInfo>,
    messages_between: i32,
    is_premium: bool,
    sponsored_enabled: bool,
}

/// `SponsoredMessageManager::SponsoredDialog`
/// (SponsoredMessageManager.cpp:251-263).
#[derive(Clone, Debug)]
pub(crate) struct SponsoredDialog {
    local_id: i64,
    dialog_id: DialogId,
    sponsor_info: String,
    additional_info: String,
}

/// `SponsoredMessageManager::SponsoredDialogs`
/// (SponsoredMessageManager.cpp:265-271).
#[derive(Default)]
pub(crate) struct SponsoredDialogs {
    local_id: i64,
    promises: Vec<Promise<td_api::SponsoredChats>>,
    dialogs: Vec<SponsoredDialog>,
    is_premium: bool,
    sponsored_enabled: bool,
}

/// `SponsoredMessageManager::VideoSponsoredMessages`
/// (SponsoredMessageManager.cpp:273-281).
#[derive(Default)]
pub(crate) struct VideoSponsoredMessages {
    local_id: i64,
    promises: Vec<Promise<td_api::VideoMessageAdvertisements>>,
    messages: Vec<SponsoredMessage>,
    start_delay: i32,
    between_delay: i32,
    is_premium: bool,
    sponsored_enabled: bool,
}

/// A parked click promise (the C++ `ClickSponsoredMessageQuery`
/// payload): the query random_id, the flag pair and the request promise.
pub struct PendingClickQuery {
    pub(crate) random_id: Vec<u8>,
    pub(crate) is_media_click: bool,
    pub(crate) from_fullscreen: bool,
    pub(crate) promise: Promise<()>,
}

/// A parked report promise (the C++ `ReportSponsoredMessageQuery`
/// payload).
pub struct PendingReportQuery {
    pub(crate) random_id: Vec<u8>,
    pub(crate) option_id: Vec<u8>,
    pub(crate) promise: Promise<td_api::ReportSponsoredResult>,
}

/// The `Td *td` slice of the collapsed SponsoredMessageManager.
pub trait SponsoredMessageManagerEnv {
    // -- the G()/option slice
    fn close_flag(&self) -> bool;
    /// the actor-timeout clock (the MultiTimeout deadline basis)
    fn now(&self) -> f64;
    /// `option_manager_->get_option_boolean(name, default)` — the
    /// manager reads `is_premium`.
    fn get_option_boolean(&self, name: &str, default_value: bool) -> bool;
    /// `user_manager_->get_my_sponsored_enabled()`.
    fn get_my_sponsored_enabled(&self) -> bool;

    // -- the cross-manager intakes
    /// `UserManager::on_get_users`.
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &'static str);
    /// `ChatManager::on_get_chats`.
    fn on_get_chats(&mut self, chats: Vec<telegram_api::Chat>, source: &'static str);

    // -- the dialog probes
    /// `DialogManager::check_dialog_access`.
    fn check_dialog_access(&mut self, dialog_id: DialogId, source: &'static str) -> Result<()>;
    /// `DialogManager::have_dialog_info`.
    fn have_dialog_info(&self, dialog_id: DialogId) -> bool;
    /// `DialogManager::force_create_dialog`.
    fn force_create_dialog(&mut self, dialog_id: DialogId, source: &'static str);
    /// `DialogManager::get_chat_id_object` (DialogManager.cpp:517).
    fn get_chat_id_object(&self, dialog_id: DialogId, source: &'static str) -> i64;
    /// `MessagesManager::can_get_message_video_advertisements` —
    /// [blocked: M.10 integration].
    fn can_get_message_video_advertisements(&self, message_full_id: MessageFullId) -> bool;
    /// `ThemeManager::get_accent_color_id_object` — the C++ consults the
    /// accent-color set; the collapsed line keeps the built-in/fallback
    /// table and treats the custom set as empty until ThemeManager lands.
    fn get_accent_color_id_object(
        &self,
        accent_color_id: AccentColorId,
        fallback_accent_color_id: AccentColorId,
    ) -> i32;

    // -- the message-plane seams ([blocked: M.10 integration])
    /// `get_message_text(user_manager, message, entities, ...)` — the
    /// degraded form keeps the raw wire text.
    fn get_sponsored_message_text(
        &mut self,
        sponsored_message: &telegram_api::SponsoredMessage,
    ) -> String;
    /// `get_message_content(td, text, media, ...)` with the
    /// self-destruct out-parameter — the pair collapses into the
    /// [`SponsoredContent`] stand-in.
    fn get_sponsored_message_content(
        &mut self,
        text: String,
        media: Option<Box<telegram_api::MessageMedia>>,
    ) -> SponsoredContent;
    /// `get_message_content_object(content.get(), ...)` — the degraded
    /// builder answers None (the null content object).
    fn get_message_content_object(
        &self,
        content: &SponsoredContent,
    ) -> Option<td_api::MessageContent>;
    /// `get_photo(td, photo, dialog_id)` — the empty-photo default for
    /// the null wire photo.
    fn get_sponsored_photo(&mut self, photo: Option<telegram_api::Photo>) -> Photo;
    /// `get_photo_object(file_manager, photo)` — the degraded builder
    /// answers None.
    fn get_photo_object(&self, photo: &Photo) -> Option<td_api::Photo>;

    // -- the `[blocked: net]` query tails; the answers route through the
    // -- on_get_* intakes, the click/report promises stay parked in the
    // -- manager's pending vectors
    /// `GetSponsoredMessagesQuery` (SponsoredMessageManager.cpp:37-74);
    /// an empty `message_id` is the per-dialog fetch, otherwise the
    /// per-message video fetch with the MSG_ID_MASK flag.
    fn send_get_sponsored_messages_query(&mut self, dialog_id: DialogId, message_id: MessageId);
    /// `GetSponsoredPeersQuery` (SponsoredMessageManager.cpp:174-200).
    fn send_get_sponsored_peers_query(&mut self, query: &str);
    /// `ViewSponsoredMessageQuery` (SponsoredMessageManager.cpp:76-91) —
    /// fire-and-forget, the C++ carries no promise.
    fn send_view_sponsored_message_query(&mut self, random_id: &[u8]);
    /// `ClickSponsoredMessageQuery` (SponsoredMessageManager.cpp:93-116)
    /// — the promise parks with the query.
    fn send_click_sponsored_message_query(&mut self, query: PendingClickQuery);
    /// `ReportSponsoredMessageQuery` (SponsoredMessageManager.cpp:118-172)
    /// — the promise parks with the query.
    fn send_report_sponsored_message_query(&mut self, query: PendingReportQuery);
}

/// `SponsoredMessageManager` (SponsoredMessageManager.h:27-141) — the
/// collapsed actor.
#[derive(Debug)]
pub struct SponsoredMessageManager {
    pub(crate) dialog_sponsored_messages:
        std::collections::HashMap<DialogId, DialogSponsoredMessages>,

    search_sponsored_dialogs: std::collections::HashMap<String, SponsoredDialogs>,
    local_id_to_search_query: std::collections::HashMap<i64, String>,
    dialog_infos: std::collections::HashMap<i64, SponsoredContentInfo>,

    video_sponsored_ads: std::collections::HashMap<MessageFullId, VideoSponsoredMessages>,
    local_id_to_message_full_id: std::collections::HashMap<i64, MessageFullId>,
    video_ad_infos: std::collections::HashMap<i64, SponsoredContentInfo>,

    current_sponsored_message_id: MessageId,

    current_local_id: i64,

    /// The three `MultiTimeout` members — deadline maps keyed by the raw
    /// dialog id / local id (the collapsed-line convention); the fired
    /// deadline runs the matching `delete_cached_*`.
    delete_cached_sponsored_messages_timeout: std::collections::HashMap<i64, f64>,
    delete_cached_sponsored_dialogs_timeout: std::collections::HashMap<i64, f64>,
    delete_cached_sponsored_videos_timeout: std::collections::HashMap<i64, f64>,

    /// The parked click/report promises (the C++ query-handler payload).
    pending_click_queries: Vec<PendingClickQuery>,
    pending_report_queries: Vec<PendingReportQuery>,
}

impl Default for SponsoredMessageManager {
    fn default() -> Self {
        SponsoredMessageManager {
            dialog_sponsored_messages: std::collections::HashMap::new(),
            search_sponsored_dialogs: std::collections::HashMap::new(),
            local_id_to_search_query: std::collections::HashMap::new(),
            dialog_infos: std::collections::HashMap::new(),
            video_sponsored_ads: std::collections::HashMap::new(),
            local_id_to_message_full_id: std::collections::HashMap::new(),
            video_ad_infos: std::collections::HashMap::new(),
            // `current_sponsored_message_id_ = MessageId::max()`
            // (SponsoredMessageManager.h:129)
            current_sponsored_message_id: MessageId::max(),
            current_local_id: 0,
            delete_cached_sponsored_messages_timeout: std::collections::HashMap::new(),
            delete_cached_sponsored_dialogs_timeout: std::collections::HashMap::new(),
            delete_cached_sponsored_videos_timeout: std::collections::HashMap::new(),
            pending_click_queries: Vec::new(),
            pending_report_queries: Vec::new(),
        }
    }
}

impl SponsoredMessageManager {
    pub fn new() -> SponsoredMessageManager {
        SponsoredMessageManager::default()
    }

    // ---- the timeout trio (the C++ MultiTimeout members) ----

    /// The earliest armed deadline across the three timeout families, for
    /// the Td timeout-slot merge.
    pub fn next_deadline(&self) -> Option<f64> {
        self.delete_cached_sponsored_messages_timeout
            .values()
            .chain(self.delete_cached_sponsored_dialogs_timeout.values())
            .chain(self.delete_cached_sponsored_videos_timeout.values())
            .copied()
            .reduce(f64::min)
    }

    /// The timeout pump: fires every due deadline once (the C++
    /// `on_delete_cached_*_timeout_callback` + `send_closure_later` pair
    /// collapses into the direct call; the close gate is the C++
    /// callback's first check).
    pub fn tick_timeouts(&mut self, env: &mut impl SponsoredMessageManagerEnv) {
        let now = env.now();
        let due_messages: Vec<i64> = self
            .delete_cached_sponsored_messages_timeout
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(&key, _)| key)
            .collect();
        let due_dialogs: Vec<i64> = self
            .delete_cached_sponsored_dialogs_timeout
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(&key, _)| key)
            .collect();
        let due_videos: Vec<i64> = self
            .delete_cached_sponsored_videos_timeout
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(&key, _)| key)
            .collect();

        for key in due_messages {
            self.delete_cached_sponsored_messages_timeout.remove(&key);
            if env.close_flag() {
                return;
            }
            self.delete_cached_sponsored_messages(DialogId::new(key));
        }
        for key in due_dialogs {
            self.delete_cached_sponsored_dialogs_timeout.remove(&key);
            if env.close_flag() {
                return;
            }
            self.delete_cached_sponsored_dialogs(key);
        }
        for key in due_videos {
            self.delete_cached_sponsored_videos_timeout.remove(&key);
            if env.close_flag() {
                return;
            }
            self.delete_cached_sponsored_videos(key);
        }
    }

    /// `delete_cached_sponsored_messages` (SponsoredMessageManager.cpp:311-320).
    pub fn delete_cached_sponsored_messages(&mut self, dialog_id: DialogId) {
        if let Some(messages) = self.dialog_sponsored_messages.get(&dialog_id) {
            if messages.promises.is_empty() {
                self.dialog_sponsored_messages.remove(&dialog_id);
            }
        }
    }

    /// `delete_cached_sponsored_dialogs` (SponsoredMessageManager.cpp:333-352).
    pub fn delete_cached_sponsored_dialogs(&mut self, local_id: i64) {
        let Some(query) = self.local_id_to_search_query.get(&local_id).cloned() else {
            return;
        };
        let Some(dialogs) = self.search_sponsored_dialogs.get(&query) else {
            unreachable!("the C++ CHECKs the query is in the cache");
        };
        if dialogs.promises.is_empty() {
            for dialog in &dialogs.dialogs {
                self.dialog_infos.remove(&dialog.local_id);
            }
            self.search_sponsored_dialogs.remove(&query);
            self.local_id_to_search_query.remove(&local_id);
        }
    }

    /// `delete_cached_sponsored_videos` (SponsoredMessageManager.cpp:365-384).
    pub fn delete_cached_sponsored_videos(&mut self, local_id: i64) {
        let Some(message_full_id) = self.local_id_to_message_full_id.get(&local_id).copied() else {
            return;
        };
        let Some(ads) = self.video_sponsored_ads.get(&message_full_id) else {
            unreachable!("the C++ CHECKs the id is in the cache");
        };
        if ads.promises.is_empty() {
            for message in &ads.messages {
                self.video_ad_infos.remove(&message.local_id);
            }
            self.video_sponsored_ads.remove(&message_full_id);
            self.local_id_to_message_full_id.remove(&local_id);
        }
    }

    // ---- the object builders ----

    /// `get_advertisement_sponsor_object`
    /// (SponsoredMessageManager.cpp:386-391) — the photo object is the
    /// degraded null until the photo plane lands.
    fn get_advertisement_sponsor_object(
        &self,
        env: &impl SponsoredMessageManagerEnv,
        sponsored_message: &SponsoredMessage,
    ) -> td_api::AdvertisementSponsor {
        td_api::AdvertisementSponsor {
            url: sponsored_message.url.clone(),
            photo: env.get_photo_object(&sponsored_message.photo),
            info: sponsored_message.sponsor_info.clone(),
        }
    }

    /// `get_sponsored_message_object` (SponsoredMessageManager.cpp:393-403).
    fn get_sponsored_message_object(
        &self,
        env: &impl SponsoredMessageManagerEnv,
        sponsored_message: &SponsoredMessage,
    ) -> td_api::SponsoredMessage {
        td_api::SponsoredMessage {
            message_id: sponsored_message.local_id,
            is_recommended: sponsored_message.is_recommended,
            can_be_reported: sponsored_message.can_be_reported,
            content: env.get_message_content_object(&sponsored_message.content),
            sponsor: Some(self.get_advertisement_sponsor_object(env, sponsored_message)),
            title: sponsored_message.title.clone(),
            button_text: sponsored_message.button_text.clone(),
            accent_color_id: env.get_accent_color_id_object(
                sponsored_message.peer_color.accent_color_id(),
                AccentColorId::default(),
            ),
            background_custom_emoji_id: sponsored_message
                .peer_color
                .background_custom_emoji_id()
                .get(),
            additional_info: sponsored_message.additional_info.clone(),
        }
    }

    /// `get_sponsored_messages_object` (SponsoredMessageManager.cpp:405-411).
    fn get_sponsored_messages_object(
        &self,
        env: &impl SponsoredMessageManagerEnv,
        dialog_id: DialogId,
        sponsored_messages: &DialogSponsoredMessages,
    ) -> td_api::SponsoredMessages {
        let _ = dialog_id;
        td_api::SponsoredMessages {
            messages: sponsored_messages
                .messages
                .iter()
                .map(|message| self.get_sponsored_message_object(env, message))
                .collect(),
            messages_between: sponsored_messages.messages_between,
        }
    }

    /// `get_sponsored_chat_object` (SponsoredMessageManager.cpp:413-418).
    fn get_sponsored_chat_object(
        &self,
        env: &impl SponsoredMessageManagerEnv,
        sponsored_dialog: &SponsoredDialog,
    ) -> td_api::SponsoredChat {
        td_api::SponsoredChat {
            unique_id: sponsored_dialog.local_id,
            chat_id: env.get_chat_id_object(sponsored_dialog.dialog_id, "sponsoredChat"),
            sponsor_info: sponsored_dialog.sponsor_info.clone(),
            additional_info: sponsored_dialog.additional_info.clone(),
        }
    }

    /// `get_sponsored_chats_object` (SponsoredMessageManager.cpp:420-424).
    fn get_sponsored_chats_object(
        &self,
        env: &impl SponsoredMessageManagerEnv,
        sponsored_dialogs: &SponsoredDialogs,
    ) -> td_api::SponsoredChats {
        td_api::SponsoredChats {
            chats: sponsored_dialogs
                .dialogs
                .iter()
                .map(|dialog| self.get_sponsored_chat_object(env, dialog))
                .collect(),
        }
    }

    /// `get_video_message_advertisement_object`
    /// (SponsoredMessageManager.cpp:426-434) — the C++ CHECKs the
    /// content is text and reads its text.
    fn get_video_message_advertisement_object(
        &self,
        env: &impl SponsoredMessageManagerEnv,
        sponsored_message: &SponsoredMessage,
    ) -> td_api::VideoMessageAdvertisement {
        debug_assert!(
            sponsored_message.content.get_type() == SponsoredContentType::Text,
            "the C++ CHECKs the text content"
        );
        td_api::VideoMessageAdvertisement {
            unique_id: sponsored_message.local_id,
            text: sponsored_message.content.get_text().to_owned(),
            min_display_duration: sponsored_message.min_display_duration,
            max_display_duration: sponsored_message.max_display_duration,
            can_be_reported: sponsored_message.can_be_reported,
            sponsor: Some(self.get_advertisement_sponsor_object(env, sponsored_message)),
            title: sponsored_message.title.clone(),
            additional_info: sponsored_message.additional_info.clone(),
        }
    }

    /// `get_video_message_advertisements_object`
    /// (SponsoredMessageManager.cpp:436-443).
    fn get_video_message_advertisements_object(
        &self,
        env: &impl SponsoredMessageManagerEnv,
        sponsored_messages: &VideoSponsoredMessages,
    ) -> td_api::VideoMessageAdvertisements {
        td_api::VideoMessageAdvertisements {
            advertisements: sponsored_messages
                .messages
                .iter()
                .map(|message| self.get_video_message_advertisement_object(env, message))
                .collect(),
            start_delay: sponsored_messages.start_delay,
            between_delay: sponsored_messages.between_delay,
        }
    }

    // ---- the per-dialog sponsored messages ----

    /// `get_dialog_sponsored_messages` (SponsoredMessageManager.cpp:445-476).
    pub fn get_dialog_sponsored_messages(
        &mut self,
        env: &mut impl SponsoredMessageManagerEnv,
        dialog_id: DialogId,
        promise: Promise<td_api::SponsoredMessages>,
    ) {
        if let Err(status) = env.check_dialog_access(dialog_id, "get_dialog_sponsored_messages") {
            let mut promise = promise;
            promise.set_error(status);
            return;
        }

        let is_premium = env.get_option_boolean("is_premium", false);
        let sponsored_enabled = env.get_my_sponsored_enabled();

        let is_cached = self
            .dialog_sponsored_messages
            .get(&dialog_id)
            .is_some_and(|messages| {
                messages.promises.is_empty()
                    && messages.is_premium == is_premium
                    && messages.sponsored_enabled == sponsored_enabled
            });
        if is_cached {
            // use cached value
            let messages = &self.dialog_sponsored_messages[&dialog_id];
            let object = self.get_sponsored_messages_object(env, dialog_id, messages);
            let mut promise = promise;
            promise.set_value(object);
            return;
        }
        if let Some(messages) = self.dialog_sponsored_messages.get(&dialog_id) {
            if messages.promises.is_empty() {
                // drop cache
                self.delete_cached_sponsored_messages_timeout
                    .remove(&dialog_id.get());
                self.dialog_sponsored_messages.remove(&dialog_id);
            }
        }

        let messages = self.dialog_sponsored_messages.entry(dialog_id).or_default();
        messages.promises.push(promise);
        if messages.promises.len() == 1 {
            env.send_get_sponsored_messages_query(dialog_id, MessageId::default());
        }
    }

    /// The intake helper of the C++ loops (SponsoredMessageManager.cpp:504-555
    /// and 829-865): the ttl/type/duration gates differ between the
    /// regular and the video flow; the local-id assignment and the info
    /// construction are shared. Returns None when the C++ `continue`s.
    fn parse_sponsored_message(
        &mut self,
        env: &mut impl SponsoredMessageManagerEnv,
        mut sponsored_message: telegram_api::SponsoredMessage,
        is_video: bool,
    ) -> Option<(SponsoredContentInfo, SponsoredMessage)> {
        let photo = env.get_sponsored_photo(sponsored_message.photo.take());
        let message_text = env.get_sponsored_message_text(&sponsored_message);
        let content =
            env.get_sponsored_message_content(message_text, sponsored_message.media.take());
        if content.has_ttl() {
            tdutils::log_error!("Receive sponsored message with self-destruct");
            return None;
        }
        if is_video {
            if content.get_type() != SponsoredContentType::Text {
                tdutils::log_error!("Receive sponsored video advertisement with non-text content");
                return None;
            }
            if sponsored_message.min_display_duration <= 0
                || sponsored_message.max_display_duration < sponsored_message.min_display_duration
            {
                tdutils::log_error!("Receive sponsored message without display duration");
                return None;
            }
        } else {
            let is_allowed_content_type = matches!(
                content.get_type(),
                SponsoredContentType::Animation
                    | SponsoredContentType::Photo
                    | SponsoredContentType::Text
                    | SponsoredContentType::Video
            );
            if !is_allowed_content_type {
                tdutils::log_error!("Receive sponsored message with disallowed content type");
                return None;
            }
            if sponsored_message.min_display_duration != 0
                || sponsored_message.max_display_duration != 0
            {
                tdutils::log_error!("Receive sponsored message with display duration");
            }
        }

        let local_id = if is_video {
            self.next_advertisement_local_id()
        } else {
            self.next_sponsored_message_local_id()
        };
        let message_info = SponsoredContentInfo {
            random_id: std::mem::take(&mut sponsored_message.random_id),
            is_viewed: false,
            is_clicked: false,
        };
        Some((
            message_info,
            SponsoredMessage {
                local_id,
                is_recommended: sponsored_message.recommended,
                can_be_reported: sponsored_message.can_report,
                content,
                min_display_duration: sponsored_message.min_display_duration,
                max_display_duration: sponsored_message.max_display_duration,
                url: sponsored_message.url,
                photo,
                title: sponsored_message.title,
                button_text: sponsored_message.button_text,
                peer_color: sponsored_message
                    .color
                    .as_ref()
                    .map(PeerColor::from_telegram)
                    .unwrap_or_default(),
                sponsor_info: sponsored_message.sponsor_info,
                additional_info: sponsored_message.additional_info,
            },
        ))
    }

    /// The sponsored-message id assignment (SponsoredMessageManager.cpp:536-543):
    /// the MessageId::max() descent with the overflow reset to
    /// `max().get_next_message_id(Local)`.
    fn next_sponsored_message_local_id(&mut self) -> i64 {
        self.current_sponsored_message_id = self
            .current_sponsored_message_id
            .get_next_message_id(MessageType::Local);
        if !self.current_sponsored_message_id.is_valid_sponsored() {
            tdutils::log_error!("Sponsored message identifier overflowed");
            self.current_sponsored_message_id =
                MessageId::max().get_next_message_id(MessageType::Local);
            debug_assert!(
                self.current_sponsored_message_id.is_valid_sponsored(),
                "the C++ CHECKs the reset id"
            );
        }
        let local_id = self.current_sponsored_message_id.get();
        debug_assert!(!self.current_sponsored_message_id.is_valid());
        debug_assert!(!self.current_sponsored_message_id.is_scheduled());
        local_id
    }

    /// The `++current_local_id_` assignment of the search-dialog/video
    /// flows (SponsoredMessageManager.cpp:653, 702, 785, 852).
    fn next_advertisement_local_id(&mut self) -> i64 {
        self.current_local_id += 1;
        self.current_local_id
    }

    /// `on_get_dialog_sponsored_messages` (SponsoredMessageManager.cpp:478-574).
    pub fn on_get_dialog_sponsored_messages(
        &mut self,
        env: &mut impl SponsoredMessageManagerEnv,
        dialog_id: DialogId,
        result: Result<telegram_api::messages_SponsoredMessages>,
    ) {
        if env.close_flag() {
            return;
        }

        let Some(messages) = self.dialog_sponsored_messages.get_mut(&dialog_id) else {
            unreachable!("the C++ CHECKs the cache entry exists");
        };
        let mut promises = std::mem::take(&mut messages.promises);
        debug_assert!(
            messages.messages.is_empty() && messages.message_infos.is_empty(),
            "the C++ CHECKs the entry is empty"
        );

        let sponsored_messages_ptr = match result {
            Result::Ok(sponsored_messages_ptr) => sponsored_messages_ptr,
            Result::Err(status) => {
                self.dialog_sponsored_messages.remove(&dialog_id);
                for promise in promises.iter_mut() {
                    promise.set_error(status.clone());
                }
                return;
            }
        };

        match sponsored_messages_ptr {
            telegram_api::messages_SponsoredMessages::messages_sponsoredMessages(
                sponsored_messages,
            ) => {
                env.on_get_users(sponsored_messages.users, "on_get_dialog_sponsored_messages");
                env.on_get_chats(sponsored_messages.chats, "on_get_dialog_sponsored_messages");

                let messages = self.dialog_sponsored_messages.get_mut(&dialog_id).unwrap();
                for wire_message in sponsored_messages.messages {
                    let Some((message_info, message)) =
                        self.parse_sponsored_message(env, wire_message, false)
                    else {
                        continue;
                    };
                    let local_id = message.local_id;
                    debug_assert!(
                        messages
                            .message_infos
                            .insert(local_id, message_info)
                            .is_none(),
                        "the C++ CHECKs the insert"
                    );
                    messages.messages.push(message);
                }
                if sponsored_messages.start_delay != 0 || sponsored_messages.between_delay != 0 {
                    tdutils::log_error!("Receive delay for regular sponsored messages");
                }
                let messages = self.dialog_sponsored_messages.get_mut(&dialog_id).unwrap();
                messages.messages_between = sponsored_messages.posts_between;
            }
            telegram_api::messages_SponsoredMessages::messages_sponsoredMessagesEmpty(_) => {}
        }

        let messages = self.dialog_sponsored_messages.get_mut(&dialog_id).unwrap();
        messages.is_premium = env.get_option_boolean("is_premium", false);
        messages.sponsored_enabled = env.get_my_sponsored_enabled();

        let object = self.get_sponsored_messages_object(env, dialog_id, messages);
        for promise in promises.iter_mut() {
            promise.set_value(object.clone());
        }
        self.delete_cached_sponsored_messages_timeout
            .insert(dialog_id.get(), env.now() + DELETE_CACHED_SPONSORED_TIMEOUT);
    }

    /// `view_sponsored_message` (SponsoredMessageManager.cpp:576-588) —
    /// the C++ caller is MessagesManager (MessagesManager.cpp:16694,
    /// the M.10 seam); the promise-free view fires only for a cached,
    /// not-yet-viewed id.
    pub fn view_sponsored_message(
        &mut self,
        env: &mut impl SponsoredMessageManagerEnv,
        dialog_id: DialogId,
        sponsored_message_id: MessageId,
    ) {
        let Some(messages) = self.dialog_sponsored_messages.get(&dialog_id) else {
            return;
        };
        let Some(info) = messages.message_infos.get(&sponsored_message_id.get()) else {
            return;
        };
        if info.is_viewed {
            return;
        }

        let random_id = info.random_id.clone();
        if let Some(info) = self
            .dialog_sponsored_messages
            .get_mut(&dialog_id)
            .and_then(|messages| messages.message_infos.get_mut(&sponsored_message_id.get()))
        {
            info.is_viewed = true;
        }
        env.send_view_sponsored_message_query(&random_id);
    }

    /// `click_sponsored_message` (SponsoredMessageManager.cpp:590-608).
    pub fn click_sponsored_message(
        &mut self,
        env: &mut impl SponsoredMessageManagerEnv,
        dialog_id: DialogId,
        sponsored_message_id: MessageId,
        is_media_click: bool,
        from_fullscreen: bool,
        promise: Promise<()>,
    ) {
        if !dialog_id.is_valid() || !sponsored_message_id.is_valid_sponsored() {
            let mut promise = promise;
            promise.set_error(Status::error(400, "Invalid message specified"));
            return;
        }
        let found = self
            .dialog_sponsored_messages
            .get(&dialog_id)
            .and_then(|messages| messages.message_infos.get(&sponsored_message_id.get()))
            .map(|info| (info.random_id.clone(), info.is_clicked));
        let Some((random_id, is_clicked)) = found else {
            let mut promise = promise;
            promise.set_value(());
            return;
        };
        if is_clicked {
            let mut promise = promise;
            promise.set_value(());
            return;
        }

        if let Some(info) = self
            .dialog_sponsored_messages
            .get_mut(&dialog_id)
            .and_then(|messages| messages.message_infos.get_mut(&sponsored_message_id.get()))
        {
            info.is_clicked = true;
        }
        env.send_click_sponsored_message_query(PendingClickQuery {
            random_id,
            is_media_click,
            from_fullscreen,
            promise,
        });
    }

    /// `report_sponsored_message` (SponsoredMessageManager.cpp:610-627).
    pub fn report_sponsored_message(
        &mut self,
        env: &mut impl SponsoredMessageManagerEnv,
        dialog_id: DialogId,
        sponsored_message_id: MessageId,
        option_id: &[u8],
        promise: Promise<td_api::ReportSponsoredResult>,
    ) {
        if !dialog_id.is_valid() || !sponsored_message_id.is_valid_sponsored() {
            let mut promise = promise;
            promise.set_error(Status::error(400, "Invalid message specified"));
            return;
        }
        let random_id = self
            .dialog_sponsored_messages
            .get(&dialog_id)
            .and_then(|messages| messages.message_infos.get(&sponsored_message_id.get()))
            .map(|info| info.random_id.clone());
        let Some(random_id) = random_id else {
            let mut promise = promise;
            promise.set_value(td_api::ReportSponsoredResult::reportSponsoredResultFailed(
                td_api::reportSponsoredResultFailed {},
            ));
            return;
        };

        env.send_report_sponsored_message_query(PendingReportQuery {
            random_id,
            option_id: option_id.to_vec(),
            promise,
        });
    }

    // ---- the search-sponsored dialogs ----

    /// `get_search_sponsored_dialogs` (SponsoredMessageManager.cpp:629-665).
    pub fn get_search_sponsored_dialogs(
        &mut self,
        env: &mut impl SponsoredMessageManagerEnv,
        query: &str,
        promise: Promise<td_api::SponsoredChats>,
    ) {
        if query.len() < 4 {
            let mut promise = promise;
            promise.set_value(td_api::SponsoredChats { chats: Vec::new() });
            return;
        }
        let is_premium = env.get_option_boolean("is_premium", false);
        let sponsored_enabled = env.get_my_sponsored_enabled();

        let is_cached = self
            .search_sponsored_dialogs
            .get(query)
            .is_some_and(|dialogs| {
                dialogs.promises.is_empty()
                    && dialogs.is_premium == is_premium
                    && dialogs.sponsored_enabled == sponsored_enabled
            });
        if is_cached {
            // use cached value
            let dialogs = &self.search_sponsored_dialogs[query];
            let object = self.get_sponsored_chats_object(env, dialogs);
            let mut promise = promise;
            promise.set_value(object);
            return;
        }
        if let Some(dialogs) = self.search_sponsored_dialogs.get(query) {
            if dialogs.promises.is_empty() {
                // drop cache
                self.delete_cached_sponsored_dialogs_timeout
                    .remove(&dialogs.local_id);
                self.local_id_to_search_query.remove(&dialogs.local_id);
                for dialog in &dialogs.dialogs {
                    self.dialog_infos.remove(&dialog.local_id);
                }
                self.search_sponsored_dialogs.remove(query);
            }
        }

        let is_first = {
            let dialogs = self
                .search_sponsored_dialogs
                .entry(query.to_owned())
                .or_default();
            if dialogs.local_id == 0 {
                self.current_local_id += 1;
                dialogs.local_id = self.current_local_id;
                self.local_id_to_search_query
                    .insert(dialogs.local_id, query.to_owned());
            }
            dialogs.promises.push(promise);
            dialogs.promises.len() == 1
        };
        if is_first {
            env.send_get_sponsored_peers_query(query);
        }
    }

    /// `on_get_search_sponsored_dialogs` (SponsoredMessageManager.cpp:667-726).
    pub fn on_get_search_sponsored_dialogs(
        &mut self,
        env: &mut impl SponsoredMessageManagerEnv,
        query: &str,
        result: Result<telegram_api::contacts_SponsoredPeers>,
    ) {
        if env.close_flag() {
            return;
        }

        let Some(dialogs) = self.search_sponsored_dialogs.get_mut(query) else {
            unreachable!("the C++ CHECKs the cache entry exists");
        };
        let mut promises = std::mem::take(&mut dialogs.promises);
        debug_assert!(
            dialogs.dialogs.is_empty(),
            "the C++ CHECKs the entry is empty"
        );
        let local_id = dialogs.local_id;

        let sponsored_dialogs_ptr = match result {
            Result::Ok(sponsored_dialogs_ptr) => sponsored_dialogs_ptr,
            Result::Err(status) => {
                self.search_sponsored_dialogs.remove(query);
                self.local_id_to_search_query.remove(&local_id);
                for promise in promises.iter_mut() {
                    promise.set_error(status.clone());
                }
                return;
            }
        };

        match sponsored_dialogs_ptr {
            telegram_api::contacts_SponsoredPeers::contacts_sponsoredPeers(sponsored_dialogs) => {
                env.on_get_users(sponsored_dialogs.users, "on_get_search_sponsored_dialogs");
                env.on_get_chats(sponsored_dialogs.chats, "on_get_search_sponsored_dialogs");

                for sponsored_dialog in sponsored_dialogs.peers {
                    let Some(peer) = &sponsored_dialog.peer else {
                        tdutils::log_error!("Receive sponsored dialog without a peer");
                        continue;
                    };
                    let dialog_id = DialogId::from_peer(peer);
                    if !dialog_id.is_valid() || !env.have_dialog_info(dialog_id) {
                        tdutils::log_error!("Receive unknown sponsored dialog");
                        continue;
                    }
                    env.force_create_dialog(dialog_id, "on_get_search_sponsored_dialogs");

                    self.current_local_id += 1;
                    let dialog_local_id = self.current_local_id;

                    let dialog_info = SponsoredContentInfo {
                        random_id: sponsored_dialog.random_id.clone(),
                        is_viewed: false,
                        is_clicked: false,
                    };
                    debug_assert!(
                        self.dialog_infos
                            .insert(dialog_local_id, dialog_info)
                            .is_none(),
                        "the C++ CHECKs the insert"
                    );

                    let dialogs = self.search_sponsored_dialogs.get_mut(query).unwrap();
                    dialogs.dialogs.push(SponsoredDialog {
                        local_id: dialog_local_id,
                        dialog_id,
                        sponsor_info: sponsored_dialog.sponsor_info,
                        additional_info: sponsored_dialog.additional_info,
                    });
                }
            }
            telegram_api::contacts_SponsoredPeers::contacts_sponsoredPeersEmpty(_) => {}
        }

        let dialogs = self.search_sponsored_dialogs.get_mut(query).unwrap();
        dialogs.is_premium = env.get_option_boolean("is_premium", false);
        dialogs.sponsored_enabled = env.get_my_sponsored_enabled();

        let object = self.get_sponsored_chats_object(env, dialogs);
        for promise in promises.iter_mut() {
            promise.set_value(object.clone());
        }
        self.delete_cached_sponsored_dialogs_timeout
            .insert(local_id, env.now() + DELETE_CACHED_SPONSORED_TIMEOUT);
    }

    /// `view_sponsored_dialog` (SponsoredMessageManager.cpp:728-738) —
    /// the promise answers first, the view fires for a known, unviewed
    /// local id.
    pub fn view_sponsored_dialog(
        &mut self,
        env: &mut impl SponsoredMessageManagerEnv,
        local_id: i64,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        promise.set_value(());

        let Some(info) = self.dialog_infos.get(&local_id) else {
            return;
        };
        if info.is_viewed {
            return;
        }

        let random_id = info.random_id.clone();
        self.dialog_infos.get_mut(&local_id).unwrap().is_viewed = true;
        env.send_view_sponsored_message_query(&random_id);
    }

    /// `open_sponsored_dialog` (SponsoredMessageManager.cpp:740-748).
    pub fn open_sponsored_dialog(
        &mut self,
        env: &mut impl SponsoredMessageManagerEnv,
        local_id: i64,
        promise: Promise<()>,
    ) {
        let Some(info) = self.dialog_infos.get(&local_id) else {
            let mut promise = promise;
            promise.set_value(());
            return;
        };
        if info.is_clicked {
            let mut promise = promise;
            promise.set_value(());
            return;
        }

        let random_id = info.random_id.clone();
        self.dialog_infos.get_mut(&local_id).unwrap().is_clicked = true;
        env.send_click_sponsored_message_query(PendingClickQuery {
            random_id,
            is_media_click: false,
            from_fullscreen: false,
            promise,
        });
    }

    /// `report_sponsored_dialog` (SponsoredMessageManager.cpp:750-758).
    pub fn report_sponsored_dialog(
        &mut self,
        env: &mut impl SponsoredMessageManagerEnv,
        local_id: i64,
        option_id: &[u8],
        promise: Promise<td_api::ReportSponsoredResult>,
    ) {
        let Some(info) = self.dialog_infos.get(&local_id) else {
            let mut promise = promise;
            promise.set_value(td_api::ReportSponsoredResult::reportSponsoredResultFailed(
                td_api::reportSponsoredResultFailed {},
            ));
            return;
        };

        env.send_report_sponsored_message_query(PendingReportQuery {
            random_id: info.random_id.clone(),
            option_id: option_id.to_vec(),
            promise,
        });
    }

    // ---- the video-message advertisements ----

    /// `get_video_sponsored_messages` (SponsoredMessageManager.cpp:760-799).
    pub fn get_video_sponsored_messages(
        &mut self,
        env: &mut impl SponsoredMessageManagerEnv,
        message_full_id: MessageFullId,
        promise: Promise<td_api::VideoMessageAdvertisements>,
    ) {
        if !env.can_get_message_video_advertisements(message_full_id) {
            let mut promise = promise;
            promise.set_error(Status::error(400, "Message can't have advertisements"));
            return;
        }

        let is_premium = env.get_option_boolean("is_premium", false);
        let sponsored_enabled = env.get_my_sponsored_enabled();

        let is_cached = self
            .video_sponsored_ads
            .get(&message_full_id)
            .is_some_and(|ads| {
                ads.promises.is_empty()
                    && ads.is_premium == is_premium
                    && ads.sponsored_enabled == sponsored_enabled
            });
        if is_cached {
            // use cached value
            let ads = &self.video_sponsored_ads[&message_full_id];
            let object = self.get_video_message_advertisements_object(env, ads);
            let mut promise = promise;
            promise.set_value(object);
            return;
        }
        if let Some(ads) = self.video_sponsored_ads.get(&message_full_id) {
            if ads.promises.is_empty() {
                // drop cache
                self.delete_cached_sponsored_videos_timeout
                    .remove(&ads.local_id);
                self.local_id_to_message_full_id.remove(&ads.local_id);
                for message in &ads.messages {
                    self.video_ad_infos.remove(&message.local_id);
                }
                self.video_sponsored_ads.remove(&message_full_id);
            }
        }

        let is_first = {
            let ads = self.video_sponsored_ads.entry(message_full_id).or_default();
            if ads.local_id == 0 {
                self.current_local_id += 1;
                ads.local_id = self.current_local_id;
                self.local_id_to_message_full_id
                    .insert(ads.local_id, message_full_id);
            }
            ads.promises.push(promise);
            ads.promises.len() == 1
        };
        if is_first {
            env.send_get_sponsored_messages_query(
                message_full_id.get_dialog_id(),
                message_full_id.get_message_id(),
            );
        }
    }

    /// `on_get_video_sponsored_messages` (SponsoredMessageManager.cpp:801-882).
    pub fn on_get_video_sponsored_messages(
        &mut self,
        env: &mut impl SponsoredMessageManagerEnv,
        message_full_id: MessageFullId,
        result: Result<telegram_api::messages_SponsoredMessages>,
    ) {
        if env.close_flag() {
            return;
        }

        let Some(ads) = self.video_sponsored_ads.get_mut(&message_full_id) else {
            unreachable!("the C++ CHECKs the cache entry exists");
        };
        let mut promises = std::mem::take(&mut ads.promises);
        debug_assert!(ads.messages.is_empty(), "the C++ CHECKs the entry is empty");
        let local_id = ads.local_id;

        let sponsored_messages_ptr = match result {
            Result::Ok(sponsored_messages_ptr) => sponsored_messages_ptr,
            Result::Err(status) => {
                self.video_sponsored_ads.remove(&message_full_id);
                self.local_id_to_message_full_id.remove(&local_id);
                for promise in promises.iter_mut() {
                    promise.set_error(status.clone());
                }
                return;
            }
        };

        match sponsored_messages_ptr {
            telegram_api::messages_SponsoredMessages::messages_sponsoredMessages(
                sponsored_messages,
            ) => {
                env.on_get_users(sponsored_messages.users, "on_get_video_sponsored_messages");
                env.on_get_chats(sponsored_messages.chats, "on_get_video_sponsored_messages");

                for wire_message in sponsored_messages.messages {
                    let Some((video_ad_info, message)) =
                        self.parse_sponsored_message(env, wire_message, true)
                    else {
                        continue;
                    };
                    let ad_local_id = message.local_id;
                    debug_assert!(
                        self.video_ad_infos
                            .insert(ad_local_id, video_ad_info)
                            .is_none(),
                        "the C++ CHECKs the insert"
                    );
                    let ads = self.video_sponsored_ads.get_mut(&message_full_id).unwrap();
                    ads.messages.push(message);
                }
                let ads = self.video_sponsored_ads.get_mut(&message_full_id).unwrap();
                ads.start_delay = sponsored_messages.start_delay.max(0);
                ads.between_delay = sponsored_messages.between_delay.max(0);
            }
            telegram_api::messages_SponsoredMessages::messages_sponsoredMessagesEmpty(_) => {}
        }

        let ads = self.video_sponsored_ads.get_mut(&message_full_id).unwrap();
        ads.is_premium = env.get_option_boolean("is_premium", false);
        ads.sponsored_enabled = env.get_my_sponsored_enabled();

        let object = self.get_video_message_advertisements_object(env, ads);
        for promise in promises.iter_mut() {
            promise.set_value(object.clone());
        }
        self.delete_cached_sponsored_videos_timeout
            .insert(local_id, env.now() + DELETE_CACHED_SPONSORED_TIMEOUT);
    }

    /// `view_video_advertisement` (SponsoredMessageManager.cpp:884-894).
    pub fn view_video_advertisement(
        &mut self,
        env: &mut impl SponsoredMessageManagerEnv,
        local_id: i64,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        promise.set_value(());

        let Some(info) = self.video_ad_infos.get(&local_id) else {
            return;
        };
        if info.is_viewed {
            return;
        }

        let random_id = info.random_id.clone();
        self.video_ad_infos.get_mut(&local_id).unwrap().is_viewed = true;
        env.send_view_sponsored_message_query(&random_id);
    }

    /// `click_video_advertisement` (SponsoredMessageManager.cpp:896-904).
    pub fn click_video_advertisement(
        &mut self,
        env: &mut impl SponsoredMessageManagerEnv,
        local_id: i64,
        promise: Promise<()>,
    ) {
        let Some(info) = self.video_ad_infos.get(&local_id) else {
            let mut promise = promise;
            promise.set_value(());
            return;
        };
        if info.is_clicked {
            let mut promise = promise;
            promise.set_value(());
            return;
        }

        let random_id = info.random_id.clone();
        self.video_ad_infos.get_mut(&local_id).unwrap().is_clicked = true;
        env.send_click_sponsored_message_query(PendingClickQuery {
            random_id,
            is_media_click: false,
            from_fullscreen: false,
            promise,
        });
    }

    /// `report_video_advertisement` (SponsoredMessageManager.cpp:906-914).
    pub fn report_video_advertisement(
        &mut self,
        env: &mut impl SponsoredMessageManagerEnv,
        local_id: i64,
        option_id: &[u8],
        promise: Promise<td_api::ReportSponsoredResult>,
    ) {
        let Some(info) = self.video_ad_infos.get(&local_id) else {
            let mut promise = promise;
            promise.set_value(td_api::ReportSponsoredResult::reportSponsoredResultFailed(
                td_api::reportSponsoredResultFailed {},
            ));
            return;
        };

        env.send_report_sponsored_message_query(PendingReportQuery {
            random_id: info.random_id.clone(),
            option_id: option_id.to_vec(),
            promise,
        });
    }

    // ---- the parked-query intake tails ----

    /// The `ReportSponsoredMessageQuery::on_result` switch
    /// (SponsoredMessageManager.cpp:131-161) over the
    /// `channels_SponsoredMessageReportResult` sum type — the parked
    /// report promise resolves here; `pending_report_queries` is the
    /// (single-slot) C++ query, so the answer drains it.
    pub fn on_report_sponsored_message_result(
        &mut self,
        result: Result<telegram_api::channels_SponsoredMessageReportResult>,
    ) {
        let Some(query) = self.pending_report_queries.pop() else {
            return;
        };
        let mut promise = query.promise;
        let Ok(result) = result else {
            let status = match result {
                tdutils::status::Result::Err(status) => status,
                tdutils::status::Result::Ok(_) => unreachable!(),
            };
            if status.message() == "AD_EXPIRED" {
                return promise.set_value(
                    td_api::ReportSponsoredResult::reportSponsoredResultFailed(
                        td_api::reportSponsoredResultFailed {},
                    ),
                );
            }
            if status.message() == "PREMIUM_ACCOUNT_REQUIRED" {
                return promise.set_value(
                    td_api::ReportSponsoredResult::reportSponsoredResultPremiumRequired(
                        td_api::reportSponsoredResultPremiumRequired {},
                    ),
                );
            }
            return promise.set_error(status);
        };

        match result {
            telegram_api::channels_SponsoredMessageReportResult::channels_sponsoredMessageReportResultReported(_) => {
                promise.set_value(td_api::ReportSponsoredResult::reportSponsoredResultOk(
                    td_api::reportSponsoredResultOk {},
                ));
            }
            telegram_api::channels_SponsoredMessageReportResult::channels_sponsoredMessageReportResultAdsHidden(_) => {
                promise.set_value(
                    td_api::ReportSponsoredResult::reportSponsoredResultAdsHidden(
                        td_api::reportSponsoredResultAdsHidden {},
                    ),
                );
            }
            telegram_api::channels_SponsoredMessageReportResult::channels_sponsoredMessageReportResultChooseOption(options) => {
                if options.options.is_empty() {
                    return promise.set_value(
                        td_api::ReportSponsoredResult::reportSponsoredResultFailed(
                            td_api::reportSponsoredResultFailed {},
                        ),
                    );
                }
                let report_options = options
                    .options
                    .iter()
                    .map(|option| td_api::ReportOption {
                        id: option.option.clone(),
                        text: option.text.clone(),
                    })
                    .collect();
                promise.set_value(
                    td_api::ReportSponsoredResult::reportSponsoredResultOptionRequired(
                        td_api::reportSponsoredResultOptionRequired {
                            title: options.title,
                            options: report_options,
                        },
                    ),
                );
            }
        }
    }

    /// The `ClickSponsoredMessageQuery::on_result` tail
    /// (SponsoredMessageManager.cpp:105-111) — ok answers the parked
    /// promise, an error fails it.
    pub fn on_click_sponsored_message_result(&mut self, result: Result<telegram_api::Bool>) {
        let Some(query) = self.pending_click_queries.pop() else {
            return;
        };
        let mut promise = query.promise;
        promise.set_result(match result {
            Result::Ok(_) => Result::Ok(()),
            Result::Err(status) => Result::Err(status),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;

    struct TestEnv {
        denied_dialogs: HashSet<i64>,
        viewed: RefCell<Vec<Vec<u8>>>,
    }

    impl SponsoredMessageManagerEnv for TestEnv {
        fn close_flag(&self) -> bool {
            false
        }

        fn now(&self) -> f64 {
            0.0
        }

        fn get_option_boolean(&self, _name: &str, default_value: bool) -> bool {
            default_value
        }

        fn get_my_sponsored_enabled(&self) -> bool {
            true
        }

        fn on_get_users(&mut self, _users: Vec<telegram_api::User>, _source: &'static str) {}

        fn on_get_chats(&mut self, _chats: Vec<telegram_api::Chat>, _source: &'static str) {}

        fn check_dialog_access(
            &mut self,
            dialog_id: DialogId,
            _source: &'static str,
        ) -> Result<(), Status> {
            if self.denied_dialogs.contains(&dialog_id.get()) {
                return Err(Status::error(400, "Chat not found"));
            }
            Ok(())
        }

        fn have_dialog_info(&self, _dialog_id: DialogId) -> bool {
            true
        }

        fn force_create_dialog(&mut self, _dialog_id: DialogId, _source: &'static str) {}

        fn get_chat_id_object(&self, dialog_id: DialogId, _source: &'static str) -> i64 {
            dialog_id.get_chat_id().get()
        }

        fn can_get_message_video_advertisements(&self, _message_full_id: MessageFullId) -> bool {
            true
        }

        fn get_accent_color_id_object(
            &self,
            accent_color_id: AccentColorId,
            fallback_accent_color_id: AccentColorId,
        ) -> i32 {
            if accent_color_id.is_valid() {
                accent_color_id.get()
            } else {
                fallback_accent_color_id.get()
            }
        }

        fn get_sponsored_message_text(
            &mut self,
            sponsored_message: &telegram_api::SponsoredMessage,
        ) -> String {
            sponsored_message.message.clone()
        }

        fn get_sponsored_message_content(
            &mut self,
            text: String,
            _media: Option<Box<telegram_api::MessageMedia>>,
        ) -> SponsoredContent {
            SponsoredContent::new(SponsoredContentType::Text, text, false)
        }

        fn get_message_content_object(
            &self,
            _content: &SponsoredContent,
        ) -> Option<td_api::MessageContent> {
            None
        }

        fn get_sponsored_photo(&mut self, _photo: Option<telegram_api::Photo>) -> Photo {
            Photo::default()
        }

        fn get_photo_object(&self, _photo: &Photo) -> Option<td_api::Photo> {
            None
        }

        fn send_get_sponsored_messages_query(
            &mut self,
            _dialog_id: DialogId,
            _message_id: MessageId,
        ) {
        }

        fn send_get_sponsored_peers_query(&mut self, _query: &str) {}

        fn send_view_sponsored_message_query(&mut self, random_id: &[u8]) {
            self.viewed.borrow_mut().push(random_id.to_vec());
        }

        fn send_click_sponsored_message_query(&mut self, _query: PendingClickQuery) {}

        fn send_report_sponsored_message_query(&mut self, _query: PendingReportQuery) {}
    }

    fn flag_promise<T: Send + Default + 'static>() -> (
        Promise<T>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        struct FlagPromise<T> {
            value_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
            error_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
        }
        impl<T: Send + 'static> tdutils::promise::PromiseInterface<T> for FlagPromise<T> {
            fn set_value(&mut self, _value: T) {
                self.value_flag
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            fn set_error(&mut self, _error: Status) {
                self.error_flag
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let value_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let error_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        (
            Promise::from_interface(FlagPromise::<T> {
                value_flag: value_flag.clone(),
                error_flag: error_flag.clone(),
            }),
            value_flag,
            error_flag,
        )
    }

    #[test]
    fn access_check_rejects_the_unknown_dialog() {
        let mut env = TestEnv {
            denied_dialogs: HashSet::from([42]),
            viewed: RefCell::new(Vec::new()),
        };
        let mut manager = SponsoredMessageManager::new();
        let (promise, _, failed) = flag_promise::<td_api::SponsoredMessages>();
        manager.get_dialog_sponsored_messages(&mut env, DialogId::new(42), promise);
        assert!(failed.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn access_check_passes_a_known_dialog_and_parks_offline() {
        let mut env = TestEnv {
            denied_dialogs: HashSet::new(),
            viewed: RefCell::new(Vec::new()),
        };
        let mut manager = SponsoredMessageManager::new();
        let (promise, fulfilled, failed) = flag_promise::<td_api::SponsoredMessages>();
        manager.get_dialog_sponsored_messages(&mut env, DialogId::new(1), promise);
        // the query tail is [blocked: net] — the promise stays unanswered
        assert!(!fulfilled.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!failed.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn view_sponsored_message_fires_the_query_seam() {
        let mut env = TestEnv {
            denied_dialogs: HashSet::new(),
            viewed: RefCell::new(Vec::new()),
        };
        let mut manager = SponsoredMessageManager::new();
        let (promise, _, _) = flag_promise::<()>();
        manager.view_sponsored_message(&mut env, DialogId::new(1), MessageId::new(7), promise);
        assert!(!env.viewed.borrow().is_empty());
    }

    #[test]
    fn search_needs_a_minimal_query() {
        let mut env = TestEnv {
            denied_dialogs: HashSet::new(),
            viewed: RefCell::new(Vec::new()),
        };
        let mut manager = SponsoredMessageManager::new();
        let (promise, _, failed) = flag_promise::<td_api::SponsoredChats>();
        manager.get_search_sponsored_dialogs(&mut env, "abc", promise);
        assert!(failed.load(std::sync::atomic::Ordering::SeqCst));
    }
}
