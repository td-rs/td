//! Port of `td/telegram/WebAppManager.{h,cpp}` — the M.46 bots-slice
//! manager over the Mini App requests: getGrossingWebAppBots,
//! searchWebApp, getWebAppLinkUrl, getMainWebApp, openWebApp, closeWebApp,
//! sendWebAppCustomRequest and checkWebAppFileDownload
//! (Requests.cpp:6919, :8255-8347), the opened-web-view registry and the
//! 60-second `messages.prolongWebView` ping loop.
//!
//! Seams:
//! - The eight `Td::ResultHandler` queries ride the
//!   [`WebAppManagerEnv`] net seams — `[blocked: net]`, the promises park
//!   (the M.41 convention); the answer handlers
//!   ([`WebAppManager::on_get_popular_app_bots`], [`on_get_bot_app`],
//!   [`on_get_app_web_view`], [`on_get_main_web_view`],
//!   [`on_get_web_view`], [`on_prolong_web_view`],
//!   [`on_get_custom_request_result`], [`on_check_download_file_params`])
//!   are real and unit-tested.
//! - The `StateManager` online callback (WebAppManager.cpp:406-422) is
//!   the [`WebAppManager::on_online`] entry — the StateManager lands
//!   with the net step.
//! - The `Timeout ping_web_view_timeout_` collapse is the
//!   `ping_web_view_at` deadline; the Td-level timeout slot drives
//!   [`WebAppManager::ping_web_view`] (the established single-slot
//!   convention).
//! - The message-plane reads (`get_dialog_silent_send_message`,
//!   `create_message_input_reply_to`,
//!   `get_dialog_default_send_message_as_dialog_id`) and the
//!   `MessageTopic::get_message_topic` dialog-state checks are
//!   [blocked: M.10] through the env trait; the local topic mapping and
//!   the send-message-topic validation rules are ported for real.
//! - `AttachMenuManager::reload_attach_menu_bots`
//!   (WebAppManager.cpp:284) and `on_dialog_used(BotApp, ...)`
//!   (TopDialogManager, WebAppManager.cpp:538 etc.) are env seams.

use crate::chat_manager::ChannelId;
use crate::dialog_id::DialogId;
use crate::files::FileSourceId;
use crate::message_id::MessageId;
use crate::td_api;
use crate::user_manager::{AccessRights, BotData, FileId, UserId};
use crate::web_app::{WebApp, WebAppOpenParameters};
use std::collections::{BTreeMap, HashMap};
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `WebAppManager::PING_WEB_VIEW_TIMEOUT` (WebAppManager.h:71).
const PING_WEB_VIEW_TIMEOUT: f64 = 60.0;

/// `td::ForumTopicId` (ForumTopicId.h) — the slice the topic mapping
/// needs; the full header is the forum-topic step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ForumTopicId(i32);

impl ForumTopicId {
    pub fn new(id: i32) -> ForumTopicId {
        ForumTopicId(id)
    }

    pub fn get(self) -> i32 {
        self.0
    }

    /// `ForumTopicId::general` (ForumTopicId.h).
    pub fn general() -> ForumTopicId {
        ForumTopicId(1)
    }

    /// `ForumTopicId::is_valid` (ForumTopicId.h).
    pub fn is_valid(self) -> bool {
        self.0 > 0
    }
}

/// `td::SavedMessagesTopicId` (SavedMessagesTopicId.h) — the slice the
/// topic mapping needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SavedMessagesTopicId(i64);

impl SavedMessagesTopicId {
    pub fn new(id: i64) -> SavedMessagesTopicId {
        SavedMessagesTopicId(id)
    }

    pub fn get(self) -> i64 {
        self.0
    }

    /// `SavedMessagesTopicId::is_valid` (SavedMessagesTopicId.h).
    pub fn is_valid(self) -> bool {
        self.0 > 0
    }
}

/// `td::MessageTopic` (MessageTopic.h:26-33) — the value shape the WebApp
/// manager carries; the full header (the update/draft surfaces) is the
/// MessagesManager step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageTopic {
    kind: MessageTopicKind,
    dialog_id: DialogId,
    top_thread_message_id: MessageId,
    forum_topic_id: ForumTopicId,
    saved_messages_topic_id: SavedMessagesTopicId,
}

/// `MessageTopic::Type` (MessageTopic.h:26).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum MessageTopicKind {
    #[default]
    None,
    Thread,
    Forum,
    Monoforum,
    SavedMessages,
}

impl MessageTopic {
    /// `MessageTopic::thread` (MessageTopic.cpp:106-114).
    pub fn thread(dialog_id: DialogId, top_thread_message_id: MessageId) -> MessageTopic {
        MessageTopic {
            kind: MessageTopicKind::Thread,
            dialog_id,
            top_thread_message_id,
            ..MessageTopic::default()
        }
    }

    /// `MessageTopic::forum` (MessageTopic.cpp:116-121).
    pub fn forum(dialog_id: DialogId, forum_topic_id: ForumTopicId) -> MessageTopic {
        MessageTopic {
            kind: MessageTopicKind::Forum,
            dialog_id,
            forum_topic_id,
            ..MessageTopic::default()
        }
    }

    /// `MessageTopic::monoforum` (MessageTopic.cpp:123-128).
    pub fn monoforum(
        dialog_id: DialogId,
        saved_messages_topic_id: SavedMessagesTopicId,
    ) -> MessageTopic {
        MessageTopic {
            kind: MessageTopicKind::Monoforum,
            dialog_id,
            saved_messages_topic_id,
            ..MessageTopic::default()
        }
    }

    /// `MessageTopic::saved_messages` (MessageTopic.cpp:130-135).
    pub fn saved_messages(
        dialog_id: DialogId,
        saved_messages_topic_id: SavedMessagesTopicId,
    ) -> MessageTopic {
        MessageTopic {
            kind: MessageTopicKind::SavedMessages,
            dialog_id,
            saved_messages_topic_id,
            ..MessageTopic::default()
        }
    }

    /// `MessageTopic::is_empty` (MessageTopic.h:104-106).
    pub fn is_empty(&self) -> bool {
        self.kind == MessageTopicKind::None
    }

    /// `MessageTopic::is_monoforum` (MessageTopic.h:119-121).
    pub fn is_monoforum(&self) -> bool {
        self.kind == MessageTopicKind::Monoforum
    }

    /// `MessageTopic::is_saved_messages` (MessageTopic.h:123-125).
    pub fn is_saved_messages(&self) -> bool {
        self.kind == MessageTopicKind::SavedMessages
    }

    /// `MessageTopic::get_input_top_msg_id` (MessageTopic.h:157-166) —
    /// the thread arm uses the C++ `get_server_message_id().get()`
    /// force-form.
    pub fn get_input_top_msg_id(&self) -> i32 {
        match self.kind {
            MessageTopicKind::Thread => self.top_thread_message_id.get_server_message_id_force(),
            MessageTopicKind::Forum => self.forum_topic_id.get(),
            _ => 0,
        }
    }
}

/// `MessageTopic::get_message_topic` (MessageTopic.cpp:122-207) — the
/// local mapping; the dialog/chat/forum/saved-messages state checks ride
/// the [`WebAppManagerEnv`] seams ([blocked: M.10 Messages] /
/// [blocked: ChatManager] / [blocked: ForumTopicManager]).
pub fn get_message_topic(
    env: &mut impl WebAppManagerEnv,
    dialog_id: DialogId,
    topic: Option<&td_api::MessageTopic>,
) -> Result<MessageTopic, Status> {
    let Some(topic) = topic else {
        return Ok(MessageTopic::default());
    };
    if !env.have_dialog_force(dialog_id, "get_message_topic") {
        return Err(Status::error(400, "Chat not found"));
    }
    match topic {
        td_api::MessageTopic::messageTopicThread(t) => {
            let top_thread_message_id = MessageId::new(t.message_thread_id);
            if dialog_id.get_type() != crate::dialog_id::DialogType::Channel
                || !env.is_megagroup_channel(dialog_id.get_channel_id())
                || env.is_dialog_monoforum_channel(dialog_id.get_channel_id())
            {
                return Err(Status::error(400, "Chat doesn't have threads"));
            }
            if !top_thread_message_id.is_server() {
                return Err(Status::error(
                    400,
                    "Invalid message thread identifier specified",
                ));
            }
            Ok(MessageTopic::thread(dialog_id, top_thread_message_id))
        }
        td_api::MessageTopic::messageTopicForum(t) => {
            let forum_topic_id = ForumTopicId::new(t.forum_topic_id);
            if !forum_topic_id.is_valid() {
                return Err(Status::error(400, "Invalid topic identifier specified"));
            }
            if !env.can_be_forum(dialog_id) {
                return Err(Status::error(400, "Chat is not a forum"));
            }
            Ok(MessageTopic::forum(dialog_id, forum_topic_id))
        }
        td_api::MessageTopic::messageTopicDirectMessages(t) => {
            if !env.is_admined_monoforum_channel(dialog_id) {
                return Err(Status::error(
                    400,
                    "Chat is not an administered channel direct messages chat",
                ));
            }
            let saved_messages_topic_id =
                env.get_saved_messages_topic_id(t.direct_messages_chat_topic_id);
            if !saved_messages_topic_id.is_valid() {
                return Err(Status::error(400, "Topic not found"));
            }
            Ok(MessageTopic::monoforum(dialog_id, saved_messages_topic_id))
        }
        td_api::MessageTopic::messageTopicSavedMessages(t) => {
            if dialog_id != env.get_my_dialog_id() {
                return Err(Status::error(400, "Chat is not the Saved Messages chat"));
            }
            let saved_messages_topic_id =
                env.get_saved_messages_topic_id(t.saved_messages_topic_id);
            if !saved_messages_topic_id.is_valid() {
                return Err(Status::error(400, "Topic not found"));
            }
            Ok(MessageTopic::saved_messages(
                dialog_id,
                saved_messages_topic_id,
            ))
        }
    }
}

/// `MessageTopic::get_send_message_topic(Td *, DialogId,
/// object_ptr<td_api::MessageTopic>)` (MessageTopic.cpp:210-232).
pub fn get_send_message_topic(
    env: &mut impl WebAppManagerEnv,
    dialog_id: DialogId,
    topic: Option<&td_api::MessageTopic>,
) -> Result<MessageTopic, Status> {
    let message_topic = get_message_topic(env, dialog_id, topic)?;
    // topic is required in administered direct messages chats
    if env.is_admined_monoforum_channel(dialog_id) && !message_topic.is_monoforum() {
        return Err(Status::error(
            400,
            "Channel direct messages topic must be specified",
        ));
    }

    // in other chats the topic can be specified implicitly
    if message_topic.is_empty() {
        return Ok(MessageTopic::default());
    }

    if message_topic.is_saved_messages() {
        return Err(Status::error(
            400,
            "Messages can't be explicitly sent to a Saved Messages topic",
        ));
    }

    Ok(message_topic)
}

/// `td::MessageInputReplyTo` (MessageInputReplyTo.h:22-58) — the slice
/// the opened-web-view registry carries; the quote/todo/poll/story
/// surfaces are the MessagesManager step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageInputReplyTo {
    message_id: MessageId,
    dialog_id: DialogId,
}

impl MessageInputReplyTo {
    /// `MessageInputReplyTo::regular` (MessageInputReplyTo.h:69-71).
    pub fn regular(message_id: MessageId) -> MessageInputReplyTo {
        MessageInputReplyTo {
            message_id,
            dialog_id: DialogId::new(0),
        }
    }

    /// `MessageInputReplyTo::is_empty` (MessageInputReplyTo.h:73-77).
    pub fn is_empty(&self) -> bool {
        !self.message_id.is_valid() && !self.message_id.is_valid_scheduled()
    }

    /// `MessageInputReplyTo::is_valid` (MessageInputReplyTo.h:79-81).
    pub fn is_valid(&self) -> bool {
        !self.is_empty()
    }
}

/// The `G()`/manager slice the collapsed manager reads through.
pub trait WebAppManagerEnv: crate::web_app::WebAppEnv {
    /// `td_->auth_manager_->is_authorized()`.
    fn is_authorized(&self) -> bool;

    /// `td_->auth_manager_->is_bot()`.
    fn is_bot(&self) -> bool;

    /// `G()->close_flag()`.
    fn close_flag(&self) -> bool;

    /// `G()->unix_time()`.
    fn unix_time(&self) -> i32;

    /// `td_->user_manager_->get_input_user(bot_user_id)`.
    fn get_input_user(&mut self, bot_user_id: UserId) -> Result<telegram_api::InputUser, Status>;

    /// `td_->user_manager_->get_bot_data(bot_user_id)`.
    fn get_bot_data(&mut self, bot_user_id: UserId) -> Result<BotData, Status>;

    /// `td_->user_manager_->is_user_bot(user_id)`.
    fn is_user_bot(&mut self, user_id: UserId) -> bool;

    /// `td_->user_manager_->on_get_user(user, source)` — the intake.
    fn on_get_user(&mut self, user: telegram_api::User, source: &str) -> UserId;

    /// `td_->user_manager_->get_user_id_object(user_id, source)`.
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64;

    /// `td_->dialog_manager_->have_input_peer(dialog_id, false, rights)`.
    fn have_input_peer(&mut self, dialog_id: DialogId, access_rights: AccessRights) -> bool;

    /// `td_->dialog_manager_->get_input_peer(dialog_id, rights)`.
    fn get_input_peer(
        &mut self,
        dialog_id: DialogId,
        access_rights: AccessRights,
    ) -> Option<telegram_api::InputPeer>;

    /// `td_->dialog_manager_->is_monoforum_channel(dialog_id)`.
    fn is_monoforum_channel(&mut self, dialog_id: DialogId) -> bool;

    /// `td_->dialog_manager_->is_admined_monoforum_channel(dialog_id)`.
    fn is_admined_monoforum_channel(&mut self, dialog_id: DialogId) -> bool;

    /// `td_->dialog_manager_->check_dialog_access(dialog_id, false,
    /// rights, source)`.
    fn check_dialog_access(
        &mut self,
        dialog_id: DialogId,
        access_rights: AccessRights,
        source: &str,
    ) -> Result<(), Status>;

    /// `td_->dialog_manager_->on_get_dialog_error(dialog_id, status,
    /// source)` — `true` when the error was consumed.
    fn on_get_dialog_error(&mut self, dialog_id: DialogId, status: &Status, source: &str) -> bool;

    /// `td_->dialog_manager_->have_dialog_force(dialog_id, source)`.
    fn have_dialog_force(&mut self, dialog_id: DialogId, source: &str) -> bool;

    /// `td_->chat_manager_->is_megagroup_channel(channel_id)` —
    /// [blocked: ChatManager channel registry].
    fn is_megagroup_channel(&mut self, channel_id: ChannelId) -> bool {
        let _ = channel_id;
        false
    }

    /// `td_->chat_manager_->is_monoforum_channel(channel_id)` —
    /// [blocked: ChatManager channel registry].
    fn is_dialog_monoforum_channel(&mut self, channel_id: ChannelId) -> bool {
        let _ = channel_id;
        false
    }

    /// `td_->forum_topic_manager_->can_be_forum(dialog_id)` —
    /// [blocked: ForumTopicManager].
    fn can_be_forum(&mut self, dialog_id: DialogId) -> bool {
        let _ = dialog_id;
        false
    }

    /// `td_->dialog_manager_->get_my_dialog_id()`.
    fn get_my_dialog_id(&mut self) -> DialogId {
        DialogId::new(0)
    }

    /// `td_->saved_messages_manager_->get_topic_id(DialogId(), id)` —
    /// [blocked: SavedMessagesManager].
    fn get_saved_messages_topic_id(&mut self, topic_id: i64) -> SavedMessagesTopicId {
        let _ = topic_id;
        SavedMessagesTopicId::new(0)
    }

    /// `td_->messages_manager_->get_dialog_silent_send_message(dialog_id)`
    /// — [blocked: M.10].
    fn get_dialog_silent_send_message(&mut self, dialog_id: DialogId) -> bool {
        let _ = dialog_id;
        false
    }

    /// `td_->messages_manager_->get_dialog_default_send_message_as_dialog_id(
    /// dialog_id)` — [blocked: M.10].
    fn get_dialog_default_send_message_as_dialog_id(&mut self, dialog_id: DialogId) -> DialogId {
        let _ = dialog_id;
        DialogId::new(0)
    }

    /// `td_->messages_manager_->create_message_input_reply_to(dialog_id,
    /// message_topic, reply_to, false)` — [blocked: M.10]; the default
    /// keeps the regular reply-to the topic points at.
    fn create_message_input_reply_to(
        &mut self,
        dialog_id: DialogId,
        message_topic: &MessageTopic,
    ) -> MessageInputReplyTo {
        let _ = dialog_id;
        MessageInputReplyTo::regular(message_topic.top_thread_message_id)
    }

    /// `td_->file_reference_manager_->create_web_app_file_source(user_id,
    /// short_name)`.
    fn create_web_app_file_source(&mut self, user_id: UserId, short_name: &str) -> FileSourceId;

    /// `td_->file_manager_->add_file_source(file_id, file_source_id,
    /// source)`.
    fn add_file_source(&mut self, file_id: FileId, file_source_id: FileSourceId, source: &str);

    /// `on_dialog_used(TopDialogCategory::BotApp, dialog_id, unix_time)`
    /// (WebAppManager.cpp:538, :556, :573) — [blocked: TopDialogManager].
    fn on_dialog_used(&mut self, dialog_id: DialogId) {
        let _ = dialog_id;
        tdutils::log_info!("on_dialog_used(BotApp) — [blocked: TopDialogManager]");
    }

    /// `td_->attach_menu_manager_->reload_attach_menu_bots(Promise<Unit>())`
    /// (WebAppManager.cpp:284) — [blocked: AttachMenuManager].
    fn reload_attach_menu_bots(&mut self) {
        tdutils::log_info!(
            "AttachMenuManager::reload_attach_menu_bots — [blocked: AttachMenuManager]"
        );
    }

    /// `GetPopularAppBotsQuery::send` (WebAppManager.cpp:40-42) —
    /// `[blocked: net]`; the answer arrives through
    /// [`WebAppManager::on_get_popular_app_bots`].
    fn send_get_popular_app_bots_query(&mut self, offset: &str, limit: i32) {
        let _ = (offset, limit);
        tdutils::log_info!("Send bots.getPopularAppBots — [blocked: net]");
    }

    /// `GetBotAppQuery::send` (WebAppManager.cpp:76-80) —
    /// `[blocked: net]`; the answer arrives through
    /// [`WebAppManager::on_get_bot_app`].
    fn send_get_bot_app_query(&mut self, bot_user_id: UserId, short_name: &str) {
        let _ = (bot_user_id, short_name);
        tdutils::log_info!("Send messages.getBotApp — [blocked: net]");
    }

    /// `RequestAppWebViewQuery::send` (WebAppManager.cpp:106-124) —
    /// `[blocked: net]`; the theme parameters arrive pre-wrapped in the
    /// `dataJSON` (the WebAppOpenParameters form).
    fn send_request_app_web_view_query(
        &mut self,
        dialog_id: DialogId,
        input_user: telegram_api::InputUser,
        web_app_short_name: &str,
        start_parameter: &str,
        parameters: &WebAppOpenParameters,
        allow_write_access: bool,
    ) {
        let _ = (
            dialog_id,
            input_user,
            web_app_short_name,
            start_parameter,
            parameters,
            allow_write_access,
        );
        tdutils::log_info!("Send messages.requestAppWebView — [blocked: net]");
    }

    /// `RequestMainWebViewQuery::send` (WebAppManager.cpp:152-169) —
    /// `[blocked: net]`.
    fn send_request_main_web_view_query(
        &mut self,
        dialog_id: DialogId,
        input_user: telegram_api::InputUser,
        start_parameter: &str,
        parameters: &WebAppOpenParameters,
    ) {
        let _ = (dialog_id, input_user, start_parameter, parameters);
        tdutils::log_info!("Send messages.requestMainWebView — [blocked: net]");
    }

    /// `RequestWebViewQuery::send` (WebAppManager.cpp:211-265) —
    /// `[blocked: net]`.
    #[allow(clippy::too_many_arguments)]
    fn send_request_web_view_query(
        &mut self,
        dialog_id: DialogId,
        bot_user_id: UserId,
        input_user: telegram_api::InputUser,
        url: &str,
        parameters: &WebAppOpenParameters,
        message_topic: &MessageTopic,
        input_reply_to: &MessageInputReplyTo,
        silent: bool,
        as_dialog_id: DialogId,
    ) {
        let _ = (
            dialog_id,
            bot_user_id,
            input_user,
            url,
            parameters,
            message_topic,
            input_reply_to,
            silent,
            as_dialog_id,
        );
        tdutils::log_info!("Send messages.requestWebView — [blocked: net]");
    }

    /// `ProlongWebViewQuery::send` (WebAppManager.cpp:295-322) —
    /// `[blocked: net]`.
    #[allow(clippy::too_many_arguments)]
    fn send_prolong_web_view_query(
        &mut self,
        dialog_id: DialogId,
        bot_user_id: UserId,
        query_id: i64,
        message_topic: &MessageTopic,
        input_reply_to: &MessageInputReplyTo,
        silent: bool,
        as_dialog_id: DialogId,
    ) {
        let _ = (
            dialog_id,
            bot_user_id,
            query_id,
            message_topic,
            input_reply_to,
            silent,
            as_dialog_id,
        );
        tdutils::log_info!("Send messages.prolongWebView — [blocked: net]");
    }

    /// `InvokeWebViewCustomMethodQuery::send`
    /// (WebAppManager.cpp:349-356) — `[blocked: net]`.
    fn send_invoke_web_view_custom_method_query(
        &mut self,
        input_user: telegram_api::InputUser,
        method: &str,
        parameters: &str,
    ) {
        let _ = (input_user, method, parameters);
        tdutils::log_info!("Send bots.invokeWebViewCustomMethod — [blocked: net]");
    }

    /// `CheckDownloadFileParamsQuery::send` (WebAppManager.cpp:380-384) —
    /// `[blocked: net]`.
    fn send_check_download_file_params_query(
        &mut self,
        input_user: telegram_api::InputUser,
        file_name: &str,
        url: &str,
    ) {
        let _ = (input_user, file_name, url);
        tdutils::log_info!("Send bots.checkDownloadFileParams — [blocked: net]");
    }
}

/// `WebAppManager::OpenedWebView` (WebAppManager.h:94-100).
#[derive(Clone, Debug, Default, PartialEq)]
struct OpenedWebView {
    dialog_id: DialogId,
    bot_user_id: UserId,
    message_topic: MessageTopic,
    input_reply_to: MessageInputReplyTo,
    as_dialog_id: DialogId,
}

/// A parked `bots.getPopularAppBots` query.
struct PendingPopularAppBotsQuery {
    promise: Promise<td_api::FoundUsers>,
}

/// A parked `messages.getBotApp` query — the C++ promise lambda hops
/// into `on_get_web_app` with the request coordinates.
struct PendingGetBotAppQuery {
    bot_user_id: UserId,
    web_app_short_name: String,
    promise: Promise<Option<td_api::FoundWebApp>>,
}

/// A parked `messages.requestAppWebView` query.
struct PendingRequestAppWebViewQuery {
    promise: Promise<td_api::WebAppUrl>,
}

/// A parked `messages.requestMainWebView` query — the ResultHandler
/// keeps `is_full_screen_` for the open-mode answer.
struct PendingRequestMainWebViewQuery {
    is_full_screen: bool,
    promise: Promise<td_api::MainWebApp>,
}

/// A parked `messages.requestWebView` query — the ResultHandler keeps
/// the open-web-view coordinates and the attach-menu reload flag.
struct PendingRequestWebViewQuery {
    dialog_id: DialogId,
    bot_user_id: UserId,
    message_topic: MessageTopic,
    input_reply_to: MessageInputReplyTo,
    as_dialog_id: DialogId,
    from_attach_menu: bool,
    promise: Promise<td_api::WebAppInfo>,
}

/// A parked `messages.prolongWebView` query.
struct PendingProlongWebViewQuery {
    dialog_id: DialogId,
}

/// A parked `bots.invokeWebViewCustomMethod` query.
struct PendingInvokeWebViewCustomMethodQuery {
    promise: Promise<td_api::CustomRequestResult>,
}

/// A parked `bots.checkDownloadFileParams` query.
struct PendingCheckDownloadFileParamsQuery {
    promise: Promise<()>,
}

/// The port of `td::WebAppManager`.
#[derive(Default)]
pub struct WebAppManager {
    /// `web_app_file_source_ids_` (WebAppManager.h:92).
    web_app_file_source_ids: HashMap<UserId, HashMap<String, FileSourceId>>,
    /// `opened_web_views_` (WebAppManager.h:101) — keyed by the launch
    /// id; ordered so the ping loop iterates deterministically.
    opened_web_views: BTreeMap<i64, OpenedWebView>,
    /// The `ping_web_view_timeout_` collapse — the armed deadline.
    ping_web_view_at: Option<f64>,
    /// The parked queries, in the C++ handler order.
    pending_popular_app_bots_queries: Vec<PendingPopularAppBotsQuery>,
    pending_get_bot_app_queries: Vec<PendingGetBotAppQuery>,
    pending_request_app_web_view_queries: Vec<PendingRequestAppWebViewQuery>,
    pending_request_main_web_view_queries: Vec<PendingRequestMainWebViewQuery>,
    pending_request_web_view_queries: Vec<PendingRequestWebViewQuery>,
    pending_prolong_web_view_queries: Vec<PendingProlongWebViewQuery>,
    pending_invoke_web_view_custom_method_queries: Vec<PendingInvokeWebViewCustomMethodQuery>,
    pending_check_download_file_params_queries: Vec<PendingCheckDownloadFileParamsQuery>,
}

impl WebAppManager {
    /// `WebAppManager::WebAppManager` (WebAppManager.cpp:403-404).
    pub fn new() -> WebAppManager {
        WebAppManager::default()
    }

    /// Whether any request promise is parked in the manager's queues
    /// (the test/request-layer probe for the net-parked flows).
    pub fn has_pending_queries(&self) -> bool {
        !self.pending_popular_app_bots_queries.is_empty()
            || !self.pending_get_bot_app_queries.is_empty()
            || !self.pending_request_app_web_view_queries.is_empty()
            || !self.pending_request_main_web_view_queries.is_empty()
            || !self.pending_request_web_view_queries.is_empty()
            || !self.pending_prolong_web_view_queries.is_empty()
            || !self
                .pending_invoke_web_view_custom_method_queries
                .is_empty()
            || !self.pending_check_download_file_params_queries.is_empty()
    }

    /// `on_online` (WebAppManager.cpp:428-434) — the StateCallback arm;
    /// a `false` online flip cancels the ping timeout.
    pub fn on_online(&mut self, env: &mut impl WebAppManagerEnv, is_online: bool) {
        if is_online {
            self.ping_web_view(env);
        } else {
            self.ping_web_view_at = None;
        }
    }

    /// The `ping_web_view_timeout_` deadline for the Td-level timeout
    /// slot; `None` when the timeout is canceled.
    pub fn ping_web_view_deadline(&self) -> Option<f64> {
        self.ping_web_view_at
    }

    /// `schedule_ping_web_view` (WebAppManager.cpp:463-467).
    fn schedule_ping_web_view(&mut self) {
        self.ping_web_view_at = Some(tdutils::time::now() + PING_WEB_VIEW_TIMEOUT);
    }

    /// `ping_web_view` (WebAppManager.cpp:447-461) — one prolong query
    /// per opened web view, then the re-arm; driven by the Td-level
    /// timeout slot at the armed deadline.
    pub fn ping_web_view(&mut self, env: &mut impl WebAppManagerEnv) {
        if self.opened_web_views.is_empty() {
            return;
        }

        let opened_web_views: BTreeMap<i64, OpenedWebView> = self.opened_web_views.clone();
        for (query_id, opened_web_view) in &opened_web_views {
            let silent = env.get_dialog_silent_send_message(opened_web_view.dialog_id);
            env.send_prolong_web_view_query(
                opened_web_view.dialog_id,
                opened_web_view.bot_user_id,
                *query_id,
                &opened_web_view.message_topic,
                &opened_web_view.input_reply_to,
                silent,
                opened_web_view.as_dialog_id,
            );
            self.pending_prolong_web_view_queries
                .push(PendingProlongWebViewQuery {
                    dialog_id: opened_web_view.dialog_id,
                });
        }

        self.schedule_ping_web_view();
    }

    /// `ProlongWebViewQuery::on_result`/`on_error`
    /// (WebAppManager.cpp:324-338).
    pub fn on_prolong_web_view(
        &mut self,
        env: &mut impl WebAppManagerEnv,
        result: Result<bool, Status>,
    ) {
        let Some(query) = self.pending_prolong_web_view_queries.pop() else {
            tdutils::log_info!("Receive an unexpected messages.prolongWebView answer");
            return;
        };
        match result {
            Ok(true) => {}
            Ok(false) => {
                tdutils::log_error!("Failed to prolong a web view");
            }
            Err(status) => {
                env.on_get_dialog_error(query.dialog_id, &status, "ProlongWebViewQuery");
            }
        }
    }

    /// `get_popular_app_bots` (WebAppManager.cpp:469-475).
    pub fn get_popular_app_bots(
        &mut self,
        env: &mut impl WebAppManagerEnv,
        offset: &str,
        limit: i32,
        mut promise: Promise<td_api::FoundUsers>,
    ) {
        if limit <= 0 {
            return promise.set_error(Status::error(400, "Limit must be positive"));
        }
        env.send_get_popular_app_bots_query(offset, limit);
        self.pending_popular_app_bots_queries
            .push(PendingPopularAppBotsQuery { promise });
    }

    /// `GetPopularAppBotsQuery::on_result`/`on_error`
    /// (WebAppManager.cpp:44-65).
    pub fn on_get_popular_app_bots(
        &mut self,
        env: &mut impl WebAppManagerEnv,
        result: Result<telegram_api::bots_PopularAppBots, Status>,
    ) {
        let Some(query) = self.pending_popular_app_bots_queries.pop() else {
            tdutils::log_info!("Receive an unexpected bots.getPopularAppBots answer");
            return;
        };
        let mut promise = query.promise;
        let Ok(ptr) = result else {
            return promise.set_error(result.unwrap_err());
        };

        let mut user_ids = Vec::new();
        for user in ptr.users {
            let user_id = env.on_get_user(user, "GetPopularAppBotsQuery");
            if env.is_user_bot(user_id) {
                user_ids.push(env.get_user_id_object(user_id, "GetPopularAppBotsQuery"));
            }
        }
        promise.set_value(td_api::FoundUsers {
            user_ids,
            next_offset: ptr.next_offset,
        });
    }

    /// `get_web_app` (WebAppManager.cpp:477-488).
    pub fn get_web_app(
        &mut self,
        env: &mut impl WebAppManagerEnv,
        bot_user_id: UserId,
        web_app_short_name: &str,
        mut promise: Promise<Option<td_api::FoundWebApp>>,
    ) {
        if let Err(error) = env.get_input_user(bot_user_id) {
            return promise.set_error(error);
        }
        if let Err(error) = env.get_bot_data(bot_user_id) {
            return promise.set_error(error);
        }
        env.send_get_bot_app_query(bot_user_id, web_app_short_name);
        self.pending_get_bot_app_queries
            .push(PendingGetBotAppQuery {
                bot_user_id,
                web_app_short_name: web_app_short_name.to_owned(),
                promise,
            });
    }

    /// `reload_web_app` (WebAppManager.cpp:516-526) — the lambda wrapper
    /// collapses into the promise adapter.
    pub fn reload_web_app(
        &mut self,
        env: &mut impl WebAppManagerEnv,
        bot_user_id: UserId,
        web_app_short_name: &str,
        promise: Promise<()>,
    ) {
        let inner = Promise::from_interface(UnitOnValuePromise { promise });
        self.get_web_app(env, bot_user_id, web_app_short_name, inner);
    }

    /// `GetBotAppQuery::on_result` (WebAppManager.cpp:82-95) +
    /// `on_get_web_app` (WebAppManager.cpp:490-514).
    pub fn on_get_bot_app(
        &mut self,
        env: &mut impl WebAppManagerEnv,
        result: Result<telegram_api::messages_BotApp, Status>,
    ) {
        let Some(query) = self.pending_get_bot_app_queries.pop() else {
            tdutils::log_info!("Receive an unexpected messages.getBotApp answer");
            return;
        };
        self.on_get_web_app(
            env,
            query.bot_user_id,
            query.web_app_short_name,
            result,
            query.promise,
        );
    }

    /// `on_get_web_app` (WebAppManager.cpp:490-514) — the
    /// `G()->ignore_result_if_closing` arm returns on a closing Td.
    fn on_get_web_app(
        &mut self,
        env: &mut impl WebAppManagerEnv,
        bot_user_id: UserId,
        web_app_short_name: String,
        result: Result<telegram_api::messages_BotApp, Status>,
        mut promise: Promise<Option<td_api::FoundWebApp>>,
    ) {
        if env.close_flag() {
            return;
        }
        if let Err(ref error) = result
            && error.message() == "BOT_APP_INVALID"
        {
            // the C++ `promise.set_value(nullptr)`
            return promise.set_value(None);
        }
        let bot_app = match result {
            Ok(bot_app) => bot_app,
            Err(error) => return promise.set_error(error),
        };
        let Some(telegram_api::BotApp::botApp(app)) = bot_app.app else {
            // the C++ CHECK(bot_app->app_->get_id() != botAppNotModified::ID)
            panic!("Receive messages.botAppNotModified");
        };

        let web_app = WebApp::from_telegram(env, app, DialogId::new(bot_user_id.get()));
        let file_ids = web_app.get_file_ids();
        if !file_ids.is_empty() {
            let file_source_id =
                self.get_web_app_file_source_id(env, bot_user_id, &web_app_short_name);
            for file_id in file_ids {
                env.add_file_source(file_id, file_source_id, "on_get_web_app");
            }
        }
        promise.set_value(Some(td_api::FoundWebApp {
            web_app: Some(web_app.get_web_app_object(env)),
            request_write_access: bot_app.request_write_access,
            skip_confirmation: !bot_app.inactive,
        }));
    }

    /// `request_app_web_view` (WebAppManager.cpp:528-542).
    // the C++ argument list is the port shape (env + 8 params)
    #[allow(clippy::too_many_arguments)]
    pub fn request_app_web_view(
        &mut self,
        env: &mut impl WebAppManagerEnv,
        dialog_id: DialogId,
        bot_user_id: UserId,
        web_app_short_name: &str,
        start_parameter: &str,
        parameters: &WebAppOpenParameters,
        allow_write_access: bool,
        mut promise: Promise<td_api::WebAppUrl>,
    ) {
        let mut dialog_id = dialog_id;
        if !env.have_input_peer(dialog_id, AccessRights::Read)
            || env.is_monoforum_channel(dialog_id)
        {
            dialog_id = DialogId::new(bot_user_id.get());
        }
        if let Err(error) = env.get_input_user(bot_user_id) {
            return promise.set_error(error);
        }
        if let Err(error) = env.get_bot_data(bot_user_id) {
            return promise.set_error(error);
        }
        env.on_dialog_used(DialogId::new(bot_user_id.get()));

        let input_user = env.get_input_user(bot_user_id).expect("checked above");
        env.send_request_app_web_view_query(
            dialog_id,
            input_user,
            web_app_short_name,
            start_parameter,
            parameters,
            allow_write_access,
        );
        self.pending_request_app_web_view_queries
            .push(PendingRequestAppWebViewQuery { promise });
    }

    /// `RequestAppWebViewQuery::on_result`/`on_error`
    /// (WebAppManager.cpp:126-140).
    pub fn on_get_app_web_view(&mut self, result: Result<telegram_api::WebViewResult, Status>) {
        let Some(query) = self.pending_request_app_web_view_queries.pop() else {
            tdutils::log_info!("Receive an unexpected messages.requestAppWebView answer");
            return;
        };
        let mut promise = query.promise;
        let ptr = match result {
            Ok(ptr) => ptr,
            Err(status) => return promise.set_error(status),
        };
        if ptr.query_id != 0 {
            tdutils::log_error!("Receive a non-empty query identifier");
        }
        promise.set_value(td_api::WebAppUrl {
            url: ptr.url,
            require_same_origin: ptr.same_origin,
        });
    }

    /// `request_main_web_view` (WebAppManager.cpp:544-560).
    pub fn request_main_web_view(
        &mut self,
        env: &mut impl WebAppManagerEnv,
        dialog_id: DialogId,
        bot_user_id: UserId,
        start_parameter: &str,
        parameters: &WebAppOpenParameters,
        mut promise: Promise<td_api::MainWebApp>,
    ) {
        let mut dialog_id = dialog_id;
        if !env.have_input_peer(dialog_id, AccessRights::Read)
            || env.is_monoforum_channel(dialog_id)
        {
            dialog_id = DialogId::new(bot_user_id.get());
        }
        if let Err(error) = env.get_input_user(bot_user_id) {
            return promise.set_error(error);
        }
        let bot_data = match env.get_bot_data(bot_user_id) {
            Ok(bot_data) => bot_data,
            Err(error) => return promise.set_error(error),
        };
        if !bot_data.has_main_app {
            return promise.set_error(Status::error(400, "The bot has no main Mini App"));
        }
        env.on_dialog_used(DialogId::new(bot_user_id.get()));

        let input_user = env.get_input_user(bot_user_id).expect("checked above");
        env.send_request_main_web_view_query(dialog_id, input_user, start_parameter, parameters);
        self.pending_request_main_web_view_queries
            .push(PendingRequestMainWebViewQuery {
                is_full_screen: parameters.is_full_screen(),
                promise,
            });
    }

    /// `RequestMainWebViewQuery::on_result`/`on_error`
    /// (WebAppManager.cpp:171-194).
    pub fn on_get_main_web_view(&mut self, result: Result<telegram_api::WebViewResult, Status>) {
        let Some(query) = self.pending_request_main_web_view_queries.pop() else {
            tdutils::log_info!("Receive an unexpected messages.requestMainWebView answer");
            return;
        };
        let mut promise = query.promise;
        let ptr = match result {
            Ok(ptr) => ptr,
            Err(status) => return promise.set_error(status),
        };
        if ptr.query_id != 0 {
            tdutils::log_error!("Receive a non-empty query identifier");
        }
        let mode = if query.is_full_screen {
            td_api::WebAppOpenMode::webAppOpenModeFullScreen(td_api::webAppOpenModeFullScreen)
        } else if ptr.fullsize {
            td_api::WebAppOpenMode::webAppOpenModeFullSize(td_api::webAppOpenModeFullSize)
        } else {
            td_api::WebAppOpenMode::webAppOpenModeCompact(td_api::webAppOpenModeCompact)
        };
        promise.set_value(td_api::MainWebApp {
            url: Some(td_api::WebAppUrl {
                url: ptr.url,
                require_same_origin: ptr.same_origin,
            }),
            mode: Some(mode),
        });
    }

    /// `request_web_view` (WebAppManager.cpp:562-584).
    // the C++ argument list is the port shape (env + 7 params)
    #[allow(clippy::too_many_arguments)]
    pub fn request_web_view(
        &mut self,
        env: &mut impl WebAppManagerEnv,
        dialog_id: DialogId,
        bot_user_id: UserId,
        topic_id: Option<&td_api::MessageTopic>,
        url: &str,
        parameters: &WebAppOpenParameters,
        mut promise: Promise<td_api::WebAppInfo>,
    ) {
        if let Err(error) = env.get_bot_data(bot_user_id) {
            return promise.set_error(error);
        }
        let input_user = match env.get_input_user(bot_user_id) {
            Ok(input_user) => input_user,
            Err(error) => return promise.set_error(error),
        };
        if let Err(error) =
            env.check_dialog_access(dialog_id, AccessRights::Write, "request_web_view")
        {
            return promise.set_error(error);
        }
        let message_topic = match get_send_message_topic(env, dialog_id, topic_id) {
            Ok(message_topic) => message_topic,
            Err(error) => return promise.set_error(error),
        };
        env.on_dialog_used(DialogId::new(bot_user_id.get()));

        let input_reply_to = env.create_message_input_reply_to(dialog_id, &message_topic);

        let silent = env.get_dialog_silent_send_message(dialog_id);
        let as_dialog_id = env.get_dialog_default_send_message_as_dialog_id(dialog_id);

        // the C++ `from_attach_menu_` flag: an empty URL from the attach
        // menu reloads the bots on an error (WebAppManager.cpp:239-240)
        let from_attach_menu = url.is_empty();

        env.send_request_web_view_query(
            dialog_id,
            bot_user_id,
            input_user,
            url,
            parameters,
            &message_topic,
            &input_reply_to,
            silent,
            as_dialog_id,
        );
        self.pending_request_web_view_queries
            .push(PendingRequestWebViewQuery {
                dialog_id,
                bot_user_id,
                message_topic,
                input_reply_to,
                as_dialog_id,
                from_attach_menu,
                promise,
            });
    }

    /// `RequestWebViewQuery::on_result`/`on_error`
    /// (WebAppManager.cpp:267-289).
    pub fn on_get_web_view(
        &mut self,
        env: &mut impl WebAppManagerEnv,
        result: Result<telegram_api::WebViewResult, Status>,
    ) {
        let Some(query) = self.pending_request_web_view_queries.pop() else {
            tdutils::log_info!("Receive an unexpected messages.requestWebView answer");
            return;
        };
        let mut promise = query.promise;
        let ptr = match result {
            Ok(ptr) => ptr,
            Err(status) => {
                if !env.on_get_dialog_error(query.dialog_id, &status, "RequestWebViewQuery")
                    && query.from_attach_menu
                {
                    env.reload_attach_menu_bots();
                }
                return promise.set_error(status);
            }
        };
        if ptr.query_id == 0 {
            tdutils::log_error!("Receive an empty query identifier");
        }
        self.open_web_view(
            ptr.query_id,
            query.dialog_id,
            query.bot_user_id,
            query.message_topic,
            query.input_reply_to,
            query.as_dialog_id,
        );
        promise.set_value(td_api::WebAppInfo {
            launch_id: ptr.query_id,
            url: Some(td_api::WebAppUrl {
                url: ptr.url,
                require_same_origin: ptr.same_origin,
            }),
        });
    }

    /// `open_web_view` (WebAppManager.cpp:586-603).
    pub fn open_web_view(
        &mut self,
        query_id: i64,
        dialog_id: DialogId,
        bot_user_id: UserId,
        message_topic: MessageTopic,
        input_reply_to: MessageInputReplyTo,
        as_dialog_id: DialogId,
    ) {
        if query_id == 0 {
            tdutils::log_error!("Receive Web App query identifier == 0");
            return;
        }

        if self.opened_web_views.is_empty() {
            self.schedule_ping_web_view();
        }
        self.opened_web_views.insert(
            query_id,
            OpenedWebView {
                dialog_id,
                bot_user_id,
                message_topic,
                input_reply_to,
                as_dialog_id,
            },
        );
    }

    /// `close_web_view` (WebAppManager.cpp:605-611).
    pub fn close_web_view(&mut self, query_id: i64, mut promise: Promise<()>) {
        self.opened_web_views.remove(&query_id);
        if self.opened_web_views.is_empty() {
            self.ping_web_view_at = None;
        }
        promise.set_value(());
    }

    /// `invoke_web_view_custom_method` (WebAppManager.cpp:613-616) — the
    /// input-user error is the real `send` on_error path
    /// (WebAppManager.cpp:350-353).
    pub fn invoke_web_view_custom_method(
        &mut self,
        env: &mut impl WebAppManagerEnv,
        bot_user_id: UserId,
        method: &str,
        parameters: &str,
        mut promise: Promise<td_api::CustomRequestResult>,
    ) {
        let input_user = match env.get_input_user(bot_user_id) {
            Ok(input_user) => input_user,
            Err(status) => return promise.set_error(status),
        };
        env.send_invoke_web_view_custom_method_query(input_user, method, parameters);
        self.pending_invoke_web_view_custom_method_queries
            .push(PendingInvokeWebViewCustomMethodQuery { promise });
    }

    /// `InvokeWebViewCustomMethodQuery::on_result`/`on_error`
    /// (WebAppManager.cpp:358-370).
    pub fn on_get_custom_request_result(&mut self, result: Result<telegram_api::DataJSON, Status>) {
        let Some(query) = self.pending_invoke_web_view_custom_method_queries.pop() else {
            tdutils::log_info!("Receive an unexpected bots.invokeWebViewCustomMethod answer");
            return;
        };
        let mut promise = query.promise;
        match result {
            Ok(result) => promise.set_value(td_api::CustomRequestResult {
                result: result.data,
            }),
            Err(status) => promise.set_error(status),
        }
    }

    /// `check_download_file_params` (WebAppManager.cpp:618-626).
    pub fn check_download_file_params(
        &mut self,
        env: &mut impl WebAppManagerEnv,
        bot_user_id: UserId,
        file_name: &str,
        url: &str,
        mut promise: Promise<()>,
    ) {
        let input_user = match env.get_input_user(bot_user_id) {
            Ok(input_user) => input_user,
            Err(status) => return promise.set_error(status),
        };
        if file_name.len() >= 256
            || url.len() > 32768
            || file_name.contains('/')
            || file_name.contains('\\')
        {
            return promise.set_error(Status::error(400, "The file can't be downloaded"));
        }
        env.send_check_download_file_params_query(input_user, file_name, url);
        self.pending_check_download_file_params_queries
            .push(PendingCheckDownloadFileParamsQuery { promise });
    }

    /// `CheckDownloadFileParamsQuery::on_result`/`on_error`
    /// (WebAppManager.cpp:386-400).
    pub fn on_check_download_file_params(&mut self, result: Result<bool, Status>) {
        let Some(query) = self.pending_check_download_file_params_queries.pop() else {
            tdutils::log_info!("Receive an unexpected bots.checkDownloadFileParams answer");
            return;
        };
        let mut promise = query.promise;
        match result {
            Ok(true) => promise.set_value(()),
            Ok(false) => promise.set_error(Status::error(400, "The file can't be downloaded")),
            Err(status) => promise.set_error(status),
        }
    }

    /// `get_web_app_file_source_id` (WebAppManager.cpp:628-642).
    pub fn get_web_app_file_source_id(
        &mut self,
        env: &mut impl WebAppManagerEnv,
        user_id: UserId,
        short_name: &str,
    ) -> FileSourceId {
        if env.close_flag() {
            return FileSourceId::new(0);
        }
        if !user_id.is_valid() || !env.is_authorized() || env.is_bot() {
            return FileSourceId::new(0);
        }

        // the C++ binds `auto &source_id = ...[user_id][short_name]` and
        // re-resolves an invalid id through the file-reference manager
        // (WebAppManager.cpp:636-639); the VLOG line is stripped
        let source_slot = self
            .web_app_file_source_ids
            .entry(user_id)
            .or_default()
            .entry(short_name.to_owned())
            .or_insert(FileSourceId::new(0));
        if !source_slot.is_valid() {
            *source_slot = env.create_web_app_file_source(user_id, short_name);
        }
        *source_slot
    }
}

/// The `reload_web_app` promise adapter: the found-web-app value is
/// dropped, errors pass through (the C++
/// `PromiseCreator::lambda` in WebAppManager.cpp:518-525).
struct UnitOnValuePromise {
    promise: Promise<()>,
}

impl tdutils::promise::PromiseInterface<Option<td_api::FoundWebApp>> for UnitOnValuePromise {
    fn set_value(&mut self, _value: Option<td_api::FoundWebApp>) {
        self.promise.set_value(());
    }

    fn set_error(&mut self, error: Status) {
        self.promise.set_error(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::photo::Photo;
    use crate::store_parse::{self, Parser};
    use tdutils::promise::PromiseInterface;

    /// The recording env: the fired query seams, the registered file
    /// sources, the dialog answers and the symmetric animation/file-row
    /// stand-ins.
    struct TestEnv {
        authorized: bool,
        bot: bool,
        closing: bool,
        queries: Vec<String>,
        file_sources: Vec<(i32, i32)>,
        next_file_source_id: i32,
        silent_answer: bool,
        can_be_forum_answer: bool,
        megagroup_answer: bool,
        my_dialog_id: DialogId,
        animation_rows: HashMap<i32, i32>,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                authorized: true,
                bot: false,
                closing: false,
                queries: Vec::new(),
                file_sources: Vec::new(),
                next_file_source_id: 1,
                silent_answer: false,
                can_be_forum_answer: false,
                megagroup_answer: false,
                my_dialog_id: DialogId::new(0),
                animation_rows: HashMap::new(),
            }
        }
    }

    impl crate::photo::PhotoStoreEnv for TestEnv {
        fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            store_parse::store_i32(out, file_id.get());
        }

        fn parse_file_id(&mut self, parser: &mut Parser<'_>) -> Result<FileId, &'static str> {
            Ok(FileId::new(parser.i32_()?, 0))
        }
    }

    impl crate::web_app::WebAppEnv for TestEnv {
        fn on_get_document_animation(
            &mut self,
            _document: telegram_api::Document,
            _owner_dialog_id: crate::dialog_id::DialogId,
        ) -> Option<FileId> {
            // the [blocked: Documents] stand-in: document id 7 is the
            // test animation
            let telegram_api::Document::document(document) = _document else {
                return None;
            };
            (document.id == 7).then_some(FileId::new(77, 0))
        }

        fn get_animation_object(&self, file_id: FileId) -> Option<td_api::Animation> {
            file_id.is_valid().then(|| td_api::Animation {
                duration: 1,
                width: 2,
                height: 3,
                file_name: "a.mp4".to_owned(),
                mime_type: "video/mp4".to_owned(),
                has_stickers: false,
                minithumbnail: None,
                thumbnail: None,
                animation: None,
            })
        }

        fn get_photo_object(&self, photo: &Photo) -> Option<td_api::Photo> {
            let first = photo.photos.first()?;
            Some(td_api::Photo {
                has_stickers: false,
                minithumbnail: None,
                sizes: vec![td_api::PhotoSize {
                    r#type: (first.size_type as u8 as char).to_string(),
                    photo: None,
                    width: first.dimensions.width as i32,
                    height: first.dimensions.height as i32,
                    progressive_sizes: Vec::new(),
                }],
            })
        }

        fn store_animation_row(&mut self, file_id: FileId, out: &mut Vec<u8>) -> bool {
            if !file_id.is_valid() {
                return false;
            }
            store_parse::store_i32(out, file_id.get());
            self.animation_rows.insert(file_id.get(), file_id.get());
            true
        }

        fn parse_animation_row(&mut self, parser: &mut Parser<'_>) -> Result<FileId, &'static str> {
            Ok(FileId::new(parser.i32_()?, 0))
        }
    }

    impl WebAppManagerEnv for TestEnv {
        fn is_authorized(&self) -> bool {
            self.authorized
        }

        fn is_bot(&self) -> bool {
            self.bot
        }

        fn close_flag(&self) -> bool {
            self.closing
        }

        fn unix_time(&self) -> i32 {
            1000
        }

        fn get_input_user(
            &mut self,
            bot_user_id: UserId,
        ) -> Result<telegram_api::InputUser, Status> {
            if !bot_user_id.is_valid() {
                return Err(Status::error(400, "User not found"));
            }
            Ok(telegram_api::InputUser::inputUser(
                telegram_api::inputUser {
                    user_id: bot_user_id.get(),
                    access_hash: 5,
                },
            ))
        }

        fn get_bot_data(&mut self, bot_user_id: UserId) -> Result<BotData, Status> {
            if bot_user_id.get() == 400 {
                return Err(Status::error(400, "Bot not found"));
            }
            Ok(BotData {
                has_main_app: bot_user_id.get() != 401,
                ..Default::default()
            })
        }

        fn is_user_bot(&mut self, user_id: UserId) -> bool {
            // the popolar-app-bots fixture: user 1 is the bot
            user_id.get() == 1
        }

        fn on_get_user(&mut self, user: telegram_api::User, source: &str) -> UserId {
            self.queries.push(format!("{source}:intake"));
            let telegram_api::User::user(user) = user else {
                return UserId::default();
            };
            UserId::new(user.id)
        }

        fn get_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
            user_id.get()
        }

        fn have_input_peer(&mut self, dialog_id: DialogId, _rights: AccessRights) -> bool {
            dialog_id.is_valid()
        }

        fn get_input_peer(
            &mut self,
            dialog_id: DialogId,
            _rights: AccessRights,
        ) -> Option<telegram_api::InputPeer> {
            dialog_id
                .is_valid()
                .then_some(telegram_api::InputPeer::inputPeerEmpty(
                    telegram_api::inputPeerEmpty {},
                ))
        }

        fn is_monoforum_channel(&mut self, _dialog_id: DialogId) -> bool {
            false
        }

        fn is_admined_monoforum_channel(&mut self, _dialog_id: DialogId) -> bool {
            false
        }

        fn check_dialog_access(
            &mut self,
            dialog_id: DialogId,
            _rights: AccessRights,
            _source: &str,
        ) -> Result<(), Status> {
            if dialog_id.is_valid() {
                Ok(())
            } else {
                Err(Status::error(400, "Chat not found"))
            }
        }

        fn on_get_dialog_error(
            &mut self,
            _dialog_id: DialogId,
            _status: &Status,
            _source: &str,
        ) -> bool {
            false
        }

        fn have_dialog_force(&mut self, dialog_id: DialogId, _source: &str) -> bool {
            dialog_id.is_valid()
        }

        fn is_megagroup_channel(&mut self, _channel_id: ChannelId) -> bool {
            self.megagroup_answer
        }

        fn can_be_forum(&mut self, _dialog_id: DialogId) -> bool {
            self.can_be_forum_answer
        }

        fn get_my_dialog_id(&mut self) -> DialogId {
            self.my_dialog_id
        }

        fn get_saved_messages_topic_id(&mut self, topic_id: i64) -> SavedMessagesTopicId {
            SavedMessagesTopicId::new(topic_id)
        }

        fn get_dialog_silent_send_message(&mut self, _dialog_id: DialogId) -> bool {
            self.silent_answer
        }

        fn get_dialog_default_send_message_as_dialog_id(
            &mut self,
            dialog_id: DialogId,
        ) -> DialogId {
            dialog_id
        }

        fn create_message_input_reply_to(
            &mut self,
            _dialog_id: DialogId,
            message_topic: &MessageTopic,
        ) -> MessageInputReplyTo {
            MessageInputReplyTo::regular(message_topic.top_thread_message_id)
        }

        fn create_web_app_file_source(
            &mut self,
            user_id: UserId,
            short_name: &str,
        ) -> FileSourceId {
            let id = FileSourceId::new(self.next_file_source_id);
            self.next_file_source_id += 1;
            self.queries.push(format!(
                "web_app_source:{user_id:?}:{short_name}:{}",
                id.get()
            ));
            id
        }

        fn add_file_source(
            &mut self,
            file_id: FileId,
            file_source_id: FileSourceId,
            _source: &str,
        ) {
            self.file_sources
                .push((file_id.get(), file_source_id.get()));
        }

        fn reload_attach_menu_bots(&mut self) {
            self.queries.push("reload_attach_menu_bots".to_owned());
        }

        fn send_get_popular_app_bots_query(&mut self, offset: &str, limit: i32) {
            self.queries
                .push(format!("getPopularAppBots:{offset}:{limit}"));
        }

        fn send_get_bot_app_query(&mut self, bot_user_id: UserId, short_name: &str) {
            self.queries
                .push(format!("getBotApp:{bot_user_id:?}:{short_name}"));
        }

        fn send_request_app_web_view_query(
            &mut self,
            _dialog_id: DialogId,
            _input_user: telegram_api::InputUser,
            web_app_short_name: &str,
            start_parameter: &str,
            _parameters: &WebAppOpenParameters,
            allow_write_access: bool,
        ) {
            self.queries.push(format!(
                "requestAppWebView:{web_app_short_name}:{start_parameter}:{allow_write_access}"
            ));
        }

        fn send_request_main_web_view_query(
            &mut self,
            _dialog_id: DialogId,
            _input_user: telegram_api::InputUser,
            start_parameter: &str,
            _parameters: &WebAppOpenParameters,
        ) {
            self.queries
                .push(format!("requestMainWebView:{start_parameter}"));
        }

        fn send_request_web_view_query(
            &mut self,
            _dialog_id: DialogId,
            _bot_user_id: UserId,
            _input_user: telegram_api::InputUser,
            url: &str,
            _parameters: &WebAppOpenParameters,
            message_topic: &MessageTopic,
            _input_reply_to: &MessageInputReplyTo,
            silent: bool,
            _as_dialog_id: DialogId,
        ) {
            self.queries.push(format!(
                "requestWebView:{url}:{}:{silent}",
                message_topic.get_input_top_msg_id()
            ));
        }

        fn send_prolong_web_view_query(
            &mut self,
            dialog_id: DialogId,
            _bot_user_id: UserId,
            query_id: i64,
            _message_topic: &MessageTopic,
            _input_reply_to: &MessageInputReplyTo,
            silent: bool,
            _as_dialog_id: DialogId,
        ) {
            self.queries.push(format!(
                "prolongWebView:{}:{query_id}:{silent}",
                dialog_id.get()
            ));
        }

        fn send_invoke_web_view_custom_method_query(
            &mut self,
            _input_user: telegram_api::InputUser,
            method: &str,
            parameters: &str,
        ) {
            self.queries
                .push(format!("invokeWebViewCustomMethod:{method}:{parameters}"));
        }

        fn send_check_download_file_params_query(
            &mut self,
            _input_user: telegram_api::InputUser,
            file_name: &str,
            url: &str,
        ) {
            self.queries
                .push(format!("checkDownloadFileParams:{file_name}:{url}"));
        }
    }

    // --- the test promise helpers (the M.45 RecordingPromise convention) ---

    type Recorded<T> = std::sync::Arc<std::sync::Mutex<Vec<std::result::Result<T, String>>>>;

    struct RecordingPromise<T: Send + 'static> {
        outcomes: Recorded<T>,
    }

    impl<T: Send + 'static> PromiseInterface<T> for RecordingPromise<T> {
        fn set_value(&mut self, value: T) {
            self.outcomes.lock().unwrap().push(Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            self.outcomes.lock().unwrap().push(Err(format!(
                "{} {}",
                error.code(),
                error.message()
            )));
        }
    }

    fn recording_promise<T: Send + 'static>() -> (Promise<T>, Recorded<T>) {
        let outcomes: Recorded<T> = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Promise::from_interface(RecordingPromise {
                outcomes: outcomes.clone(),
            }),
            outcomes,
        )
    }

    fn sample_web_app_object(short_name: &str) -> telegram_api::botApp {
        telegram_api::botApp {
            flags: 0,
            id: 100,
            access_hash: 200,
            short_name: short_name.to_owned(),
            title: "Test App".to_owned(),
            description: "app".to_owned(),
            photo: None,
            document: None,
            hash: 300,
        }
    }

    #[test]
    fn web_app_open_parameters_map_modes_and_theme() {
        let compact = WebAppOpenParameters::from_td_api(Some(td_api::WebAppOpenParameters {
            theme: Some(td_api::ThemeParameters {
                background_color: 0x00FF_FFFF,
                ..Default::default()
            }),
            application_name: "app".to_owned(),
            mode: Some(td_api::WebAppOpenMode::webAppOpenModeCompact(
                td_api::webAppOpenModeCompact,
            )),
        }));
        assert!(compact.is_compact());
        assert!(!compact.is_full_screen());
        assert_eq!(compact.get_application_name(), "app");
        let data = compact.get_input_theme_parameters().expect("theme");
        assert_eq!(
            data.data,
            "{\"bg_color\":\"#ffffff\",\"secondary_bg_color\":\"#000000\",\"text_color\":\"#000000\",\"hint_color\":\"#000000\",\"link_color\":\"#000000\",\"button_color\":\"#000000\",\"button_text_color\":\"#000000\",\"header_bg_color\":\"#000000\",\"bottom_bar_bg_color\":\"#000000\",\"section_bg_color\":\"#000000\",\"section_separator_color\":\"#000000\",\"accent_text_color\":\"#000000\",\"section_header_text_color\":\"#000000\",\"subtitle_text_color\":\"#000000\",\"destructive_text_color\":\"#000000\"}"
        );

        let full_screen = WebAppOpenParameters::from_td_api(Some(td_api::WebAppOpenParameters {
            theme: None,
            application_name: String::new(),
            mode: Some(td_api::WebAppOpenMode::webAppOpenModeFullScreen(
                td_api::webAppOpenModeFullScreen,
            )),
        }));
        assert!(full_screen.is_full_screen());
        assert!(!full_screen.is_compact());
        assert!(full_screen.get_input_theme_parameters().is_none());

        // the C++ clears an invalid UTF-8 application name; a Rust String
        // is always valid UTF-8, so the control-char form survives the
        // clean (the NUL becomes a space, misc.cpp:79-161)
        let nul_name = WebAppOpenParameters::from_td_api(Some(td_api::WebAppOpenParameters {
            theme: None,
            application_name: "a\u{0}b".to_owned(),
            mode: None,
        }));
        assert_eq!(nul_name.get_application_name(), "a b");
    }

    #[test]
    fn get_popular_app_bots_guards_limit_and_answers() {
        let mut env = TestEnv::default();
        let mut manager = WebAppManager::new();

        let (promise, outcomes) = recording_promise();
        manager.get_popular_app_bots(&mut env, "", 0, promise);
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 Limit must be positive")
        );
        assert!(env.queries.is_empty());

        let (promise, outcomes) = recording_promise();
        manager.get_popular_app_bots(&mut env, "off", 3, promise);
        assert_eq!(env.queries, vec!["getPopularAppBots:off:3"]);

        let answer = telegram_api::bots_PopularAppBots {
            flags: 0,
            next_offset: "next".to_owned(),
            users: vec![
                telegram_api::User::user(telegram_api::user {
                    flags: (1 << 14) | (1 << 15), // deleted + bot
                    id: 1,
                    ..Default::default()
                }),
                telegram_api::User::user(telegram_api::user {
                    flags: 0,
                    id: 2,
                    ..Default::default()
                }),
            ],
        };
        manager.on_get_popular_app_bots(&mut env, Ok(answer));
        let outcomes = outcomes.lock().unwrap();
        let found = outcomes[0].as_ref().expect("answered");
        // only the bot (flags bit 15) survives the filter
        assert_eq!(found.user_ids, vec![1]);
        assert_eq!(found.next_offset, "next");
    }

    #[test]
    fn get_web_app_invalid_app_answers_none() {
        let mut env = TestEnv::default();
        let mut manager = WebAppManager::new();

        let (promise, outcomes) = recording_promise();
        manager.get_web_app(&mut env, UserId::new(5), "app", promise);
        assert_eq!(env.queries, vec!["getBotApp:UserId(5):app"]);

        manager.on_get_bot_app(&mut env, Err(Status::error(400, "BOT_APP_INVALID")));
        assert!(outcomes.lock().unwrap()[0].is_ok());
        // the C++ `promise.set_value(nullptr)` — the request layer answers 404
    }

    #[test]
    fn get_web_app_builds_the_value_and_registers_sources() {
        let mut env = TestEnv::default();
        let mut manager = WebAppManager::new();

        let (promise, outcomes) = recording_promise();
        manager.get_web_app(&mut env, UserId::new(5), "app", promise);
        manager.on_get_bot_app(
            &mut env,
            Ok(telegram_api::messages_BotApp {
                flags: 0,
                inactive: false,
                request_write_access: true,
                has_settings: false,
                app: Some(telegram_api::BotApp::botApp(sample_web_app_object("app"))),
            }),
        );
        let outcomes = outcomes.lock().unwrap();
        let found = outcomes[0]
            .as_ref()
            .as_ref()
            .expect("found")
            .as_ref()
            .expect("some");
        assert!(found.request_write_access);
        assert!(found.skip_confirmation);
        let web_app = found.web_app.as_ref().expect("web app");
        assert_eq!(web_app.short_name, "app");
    }

    #[test]
    fn request_main_web_view_requires_the_main_app() {
        let mut env = TestEnv::default();
        let mut manager = WebAppManager::new();

        // bot 401 has_main_app = false in the env answers
        let (promise, outcomes) = recording_promise();
        manager.request_main_web_view(
            &mut env,
            DialogId::new(400),
            UserId::new(401),
            "start",
            &WebAppOpenParameters::default(),
            promise,
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 The bot has no main Mini App")
        );

        // an unknown bot fails the get_bot_data guard first
        let (promise, outcomes) = recording_promise();
        manager.request_main_web_view(
            &mut env,
            DialogId::new(400),
            UserId::new(400),
            "start",
            &WebAppOpenParameters::default(),
            promise,
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 Bot not found")
        );

        // a healthy bot parks the query
        let (promise, outcomes) = recording_promise();
        manager.request_main_web_view(
            &mut env,
            DialogId::new(7),
            UserId::new(5),
            "start",
            &WebAppOpenParameters::default(),
            promise,
        );
        assert!(
            env.queries
                .iter()
                .any(|q| q.starts_with("requestMainWebView:"))
        );
        assert!(outcomes.lock().unwrap().is_empty());

        // the full-screen request form keeps the C++ open-mode priority
        let parameters = WebAppOpenParameters::from_td_api(Some(td_api::WebAppOpenParameters {
            theme: None,
            application_name: String::new(),
            mode: Some(td_api::WebAppOpenMode::webAppOpenModeFullScreen(
                td_api::webAppOpenModeFullScreen,
            )),
        }));
        let (promise, outcomes) = recording_promise();
        manager.request_main_web_view(
            &mut env,
            DialogId::new(7),
            UserId::new(5),
            "start",
            &parameters,
            promise,
        );
        let result = telegram_api::WebViewResult {
            flags: 0,
            fullsize: true,
            fullscreen: false,
            same_origin: true,
            query_id: 9,
            url: "https://app".to_owned(),
        };
        manager.on_get_main_web_view(Ok(result));
        let outcomes = outcomes.lock().unwrap();
        let main = outcomes[0].as_ref().expect("answered");
        let url = main.url.as_ref().expect("url");
        assert_eq!(url.url, "https://app");
        assert!(url.require_same_origin);
        // is_full_screen wins over the server fullsize flag
        assert!(matches!(
            main.mode,
            Some(td_api::WebAppOpenMode::webAppOpenModeFullScreen(_))
        ));
    }

    #[test]
    fn request_web_view_topic_validation_and_answer() {
        let mut env = TestEnv::default();
        let mut manager = WebAppManager::new();

        // a forum topic on a non-forum chat fails the topic mapping
        let (promise, outcomes) = recording_promise();
        manager.request_web_view(
            &mut env,
            DialogId::new(7),
            UserId::new(5),
            Some(&td_api::MessageTopic::messageTopicForum(
                td_api::messageTopicForum { forum_topic_id: 3 },
            )),
            "start://deep",
            &WebAppOpenParameters::default(),
            promise,
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 Chat is not a forum")
        );

        // a plain URL parks the query with the attach-menu flag unset
        env.can_be_forum_answer = true;
        let (promise, outcomes) = recording_promise();
        manager.request_web_view(
            &mut env,
            DialogId::new(7),
            UserId::new(5),
            Some(&td_api::MessageTopic::messageTopicForum(
                td_api::messageTopicForum { forum_topic_id: 3 },
            )),
            "https://app",
            &WebAppOpenParameters::default(),
            promise,
        );
        assert!(
            env.queries
                .iter()
                .any(|q| q.starts_with("requestWebView:https://app:3:false"))
        );
        assert!(outcomes.lock().unwrap().is_empty());

        // the answer opens the web view and answers the info object
        manager.on_get_web_view(
            &mut env,
            Ok(telegram_api::WebViewResult {
                flags: 0,
                fullsize: false,
                fullscreen: false,
                same_origin: false,
                query_id: 42,
                url: "https://web".to_owned(),
            }),
        );
        let outcomes = outcomes.lock().unwrap();
        let info = outcomes[0].as_ref().expect("answered");
        assert_eq!(info.launch_id, 42);
        assert_eq!(info.url.as_ref().expect("url").url, "https://web");
        drop(outcomes);

        // the opened registry arms the 60-second ping
        assert!(manager.ping_web_view_deadline().is_some());

        // an error with an empty URL (the attach-menu shape) reloads the bots
        let (promise, outcomes) = recording_promise();
        manager.request_web_view(
            &mut env,
            DialogId::new(7),
            UserId::new(5),
            None,
            "",
            &WebAppOpenParameters::default(),
            promise,
        );
        assert!(outcomes.lock().unwrap().is_empty());
        manager.on_get_web_view(&mut env, Err(Status::error(400, "CHAT_EMPTY")));
        assert!(env.queries.contains(&"reload_attach_menu_bots".to_owned()));
        assert!(matches!(
            &outcomes.lock().unwrap()[0],
            Err(message) if message == "400 CHAT_EMPTY"
        ));
    }

    #[test]
    fn open_close_web_view_drive_the_ping_loop() {
        let mut env = TestEnv::default();
        let mut manager = WebAppManager::new();

        // a zero launch id is logged and dropped
        manager.open_web_view(
            0,
            DialogId::new(7),
            UserId::new(5),
            MessageTopic::default(),
            MessageInputReplyTo::default(),
            DialogId::new(0),
        );
        assert!(manager.ping_web_view_deadline().is_none());

        manager.open_web_view(
            42,
            DialogId::new(7),
            UserId::new(5),
            MessageTopic::default(),
            MessageInputReplyTo::default(),
            DialogId::new(0),
        );
        let deadline = manager.ping_web_view_deadline().expect("armed");
        assert!(deadline > tdutils::time::now());

        // the ping fires one prolong query per opened view and re-arms
        env.silent_answer = true;
        manager.ping_web_view(&mut env);
        assert!(
            env.queries
                .iter()
                .any(|q| q == &"prolongWebView:7:42:true".to_owned())
        );
        assert!(manager.ping_web_view_deadline().unwrap() > deadline);

        // an error answer consumes the dialog-error mapping
        manager.on_prolong_web_view(&mut env, Ok(false));
        manager.on_prolong_web_view(&mut env, Err(Status::error(400, "X")));

        // close cancels the timeout
        let (promise, outcomes) = recording_promise();
        manager.close_web_view(42, promise);
        assert_eq!(*outcomes.lock().unwrap(), vec![Ok(())]);
        assert!(manager.ping_web_view_deadline().is_none());
    }

    #[test]
    fn check_download_file_params_guards_and_answers() {
        let mut env = TestEnv::default();
        let mut manager = WebAppManager::new();

        let long_name = "a".repeat(256);
        for (file_name, url) in [
            (long_name.as_str(), "https://f"),
            ("a/b", "https://f"),
            ("a\\b", "https://f"),
            ("f", &"h".repeat(32769)),
        ] {
            let (promise, outcomes) = recording_promise();
            manager.check_download_file_params(&mut env, UserId::new(5), file_name, url, promise);
            assert!(
                matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 The file can't be downloaded")
            );
        }

        let (promise, outcomes) = recording_promise();
        manager.check_download_file_params(&mut env, UserId::new(5), "f.bin", "https://f", promise);
        assert!(
            env.queries
                .contains(&"checkDownloadFileParams:f.bin:https://f".to_owned())
        );
        assert!(outcomes.lock().unwrap().is_empty());

        // the Bool answer carries the server verdict
        manager.on_check_download_file_params(Ok(false));
        assert!(matches!(
            &outcomes.lock().unwrap()[0],
            Err(message) if message == "400 The file can't be downloaded"
        ));

        let (promise, outcomes) = recording_promise();
        manager.check_download_file_params(&mut env, UserId::new(5), "f.bin", "https://f", promise);
        manager.on_check_download_file_params(Ok(true));
        assert_eq!(*outcomes.lock().unwrap(), vec![Ok(())]);
    }

    #[test]
    fn invoke_web_view_custom_method_maps_the_data_json() {
        let mut env = TestEnv::default();
        let mut manager = WebAppManager::new();

        let (promise, outcomes) = recording_promise();
        manager.invoke_web_view_custom_method(&mut env, UserId::new(5), "method", "{}", promise);
        assert!(
            env.queries
                .contains(&"invokeWebViewCustomMethod:method:{}".to_owned())
        );
        manager.on_get_custom_request_result(Ok(telegram_api::DataJSON {
            data: "\"ok\"".to_owned(),
        }));
        let outcomes = outcomes.lock().unwrap();
        assert_eq!(outcomes[0].as_ref().expect("answered").result, "\"ok\"");

        // an invalid bot errors before the send (the real send on_error path)
        let (promise, outcomes) = recording_promise();
        manager.invoke_web_view_custom_method(&mut env, UserId::new(0), "m", "{}", promise);
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 User not found")
        );
    }

    #[test]
    fn web_app_file_source_ids_cache_per_short_name() {
        let mut env = TestEnv::default();
        let mut manager = WebAppManager::new();

        let first = manager.get_web_app_file_source_id(&mut env, UserId::new(5), "app");
        let second = manager.get_web_app_file_source_id(&mut env, UserId::new(5), "app");
        let other = manager.get_web_app_file_source_id(&mut env, UserId::new(5), "other");
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert_eq!(env.next_file_source_id, 3);

        // bots and unauthorized clients get no source
        env.bot = true;
        assert!(
            !manager
                .get_web_app_file_source_id(&mut env, UserId::new(5), "app")
                .is_valid()
        );
        env.bot = false;
        env.authorized = false;
        assert!(
            !manager
                .get_web_app_file_source_id(&mut env, UserId::new(5), "app")
                .is_valid()
        );
    }

    #[test]
    fn web_app_value_store_parse_is_byte_symmetric() {
        let mut env = TestEnv::default();
        let web_app = WebApp::from_telegram(
            &mut env,
            telegram_api::botApp {
                document: Some(telegram_api::Document::document(telegram_api::document {
                    id: 7,
                    ..Default::default()
                })),
                ..sample_web_app_object("app")
            },
            crate::dialog_id::DialogId::new(5),
        );
        // the test env classifies document id 7 as the animation
        assert!(web_app.get_file_ids().iter().any(|f| f.get() == 77));

        let mut stored = Vec::new();
        web_app.store(&mut env, &mut stored);
        let mut parser = Parser::new(&stored);
        let parsed = WebApp::parse(&mut env, &mut parser).expect("parses");
        parser.fetch_end().expect("fully consumed");
        assert_eq!(parsed, web_app);
    }
}
