//! `td::ForumTopicManager` (ForumTopicManager.h/.cpp, M.49).
//!
//! The topic registry (the per-dialog topic maps, the create/edit/get/
//! toggle/pin/delete flows with the C++ local validation, the
//! topic-intake machine) is real; the seven net-drain 41 query families
//! (`ForumTopicNetKind`) ride the manager-held net handle, the
//! delete-topic-history tail routes through the landed message-query
//! affected-history plane, and the notification-settings/draft halves
//! ride the env seams.

use crate::dialog_id::{DialogId, DialogType};
use crate::emoji_status::CustomEmojiId;
use crate::forum_topic::{ForumTopicEditedData, ForumTopicId, ForumTopicInfo};
use crate::message_id::MessageId;
use crate::store_parse::{self, Parser};
use crate::td_api;
use std::collections::HashMap;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The in-flight query kinds of the net-drain 41 forum-topic families —
/// the C++ `Td::ResultHandler` classes (`CreateForumTopicQuery`,
/// `EditForumTopicQuery` (all three wire forms),
/// `UpdatePinnedForumTopicQuery`, `ReorderPinnedForumTopicsQuery`,
/// `GetForumTopicQuery`, `GetForumTopicsQuery`, ForumTopicManager.cpp:45-437)
/// collapsed onto one dispatcher tag; the answers drain through
/// `Td::on_forum_topic_net_answer`. The dialog ids key the per-handler
/// `on_error` heads; the promises park in the manager's
/// [`PendingForumTopicQueries`] lots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForumTopicNetKind {
    /// `CreateForumTopicQuery` (cpp:45) — `messages.createForumTopic`;
    /// the handler members the answer needs (`dialog_id_`,
    /// `creator_dialog_id_`, `random_id_`) ride the kind.
    CreateForumTopic {
        dialog_id: DialogId,
        creator_dialog_id: DialogId,
        random_id: i64,
    },
    /// `GetForumTopicQuery` (cpp:303) — `messages.getForumTopicsByID`;
    /// `forum_topic_id` is the expected-id band of `on_get_forum_topic`.
    GetForumTopic {
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
    },
    /// `GetForumTopicsQuery` (cpp:369) — `messages.getForumTopics`.
    GetForumTopics { dialog_id: DialogId },
    /// `EditForumTopicQuery` (cpp:136) — `messages.editForumTopic`, the
    /// title/icon, closed and hidden send forms (one C++ class, one
    /// answer tail).
    EditForumTopic { dialog_id: DialogId },
    /// `UpdatePinnedForumTopicQuery` (cpp:220) —
    /// `messages.updatePinnedForumTopic`.
    UpdatePinnedForumTopic { dialog_id: DialogId },
    /// `ReorderPinnedForumTopicsQuery` (cpp:261) —
    /// `messages.reorderPinnedForumTopics`.
    ReorderPinnedForumTopics { dialog_id: DialogId },
}

/// The parked query promises — the `ResultHandler::promise_` members
/// (FIFO-paired per family, §5; the C++ carries each promise on its own
/// query, the collapsed port parks it next to the dispatcher tag).
#[derive(Default)]
pub(crate) struct PendingForumTopicQueries {
    /// `CreateForumTopicQuery::promise_`.
    pub(crate) create_topic: Vec<Promise<Option<td_api::ForumTopicInfo>>>,
    /// `GetForumTopicQuery::promise_`.
    pub(crate) get_topic: Vec<Promise<Option<td_api::ForumTopic>>>,
    /// `GetForumTopicsQuery::promise_`.
    pub(crate) get_topics: Vec<Promise<td_api::ForumTopics>>,
    /// `EditForumTopicQuery::promise_` (all three send forms).
    pub(crate) edit_topic: Vec<Promise<()>>,
    /// `UpdatePinnedForumTopicQuery::promise_`.
    pub(crate) update_pinned: Vec<Promise<()>>,
    /// `ReorderPinnedForumTopicsQuery::promise_`.
    pub(crate) reorder_pinned: Vec<Promise<()>>,
    /// `delete_forum_topic`'s validated wire tails (the C++
    /// `delete_promise` lambda's captures, ForumTopicManager.cpp:1028-1037)
    /// — the td-level runner drains them into the message-query
    /// affected-history plane.
    pub(crate) delete_tails: Vec<(DialogId, ForumTopicId, Promise<()>)>,
}

/// The `G()`/`td_` surface the forum plane reads through. The
/// notification-settings/draft/database halves keep their defaults until
/// their tracks land; the net-drain 41 query families dispatch through the
/// manager-held net handle over the peer/my-id/permission reads below;
/// the delete-topic-history tail routes through the message-query plane
/// env seam. The seams keep the C++-faithful argument lists (the
/// convention's too_many_arguments note).
#[allow(clippy::too_many_arguments)]
pub trait ForumTopicEnv {
    fn is_bot(&self) -> bool;
    fn unix_time(&mut self) -> i32;
    fn close_status(&self) -> Status {
        Status::ok()
    }

    // -- the chat/channel surfaces --
    fn is_forum(&self, dialog_id: DialogId, allow_bots: bool) -> Status {
        let _ = (dialog_id, allow_bots);
        Status::ok()
    }
    fn get_channel_permissions_can_create_topics(&self, dialog_id: DialogId) -> bool {
        let _ = dialog_id;
        true
    }
    fn get_channel_permissions_can_edit_topics(&self, dialog_id: DialogId) -> bool {
        let _ = dialog_id;
        true
    }
    /// `chat_manager_->get_channel_permissions(channel_id).can_pin_topics()`
    /// (cpp:991/1006).
    fn get_channel_permissions_can_pin_topics(&self, dialog_id: DialogId) -> bool {
        let _ = dialog_id;
        true
    }
    /// `chat_manager_->get_channel_permissions(channel_id).can_delete_messages()`
    /// (cpp:1026).
    fn get_channel_permissions_can_delete_messages(&self, dialog_id: DialogId) -> bool {
        let _ = dialog_id;
        true
    }
    fn get_channel_first_username(&self, dialog_id: DialogId) -> String {
        let _ = dialog_id;
        String::new()
    }
    /// `messages_manager_->get_dialog_default_send_message_as_dialog_id`
    /// (MessagesManager.cpp:20886-20890) — the dialog record's field,
    /// invalid until set; the real read lands with the messages plane.
    fn get_default_send_message_as_dialog_id(&self, dialog_id: DialogId) -> DialogId {
        let _ = dialog_id;
        DialogId::default()
    }
    fn get_message_order(&mut self, dialog_id: DialogId, message_id: MessageId) -> i64 {
        let _ = (dialog_id, message_id);
        0
    }
    fn send_update(&mut self, update: td_api::Update) {
        tdutils::log_info!("send_update({update:?}) — seam default");
    }

    // -- the send-leg reads of the net-drain 41 families --
    /// `td_->dialog_manager_->get_my_dialog_id()` — the create-topic
    /// creator id (cpp:57).
    fn get_my_dialog_id(&mut self) -> DialogId {
        DialogId::default()
    }
    /// `td_->dialog_manager_->get_input_peer(dialog_id, rights)` — the
    /// per-family Write/Read peer legs (the `StoryEnv` split shape).
    fn get_input_peer(
        &mut self,
        dialog_id: DialogId,
        write: bool,
    ) -> Option<telegram_api::InputPeer> {
        let _ = (dialog_id, write);
        None
    }
    /// `chat_manager_->can_be_forum(dialog_id)` (cpp:1276-1296) — the
    /// intake-side band (no `have_dialog_force` read).
    fn can_be_forum(&self, dialog_id: DialogId) -> bool {
        let _ = dialog_id;
        true
    }

    // -- the notification-settings / draft seams --
    //    [blocked: the notification-settings / draft planes] --
    fn store_notification_settings(&self, out: &mut Vec<u8>) {
        tdutils::log_info!(
            "store_notification_settings — [blocked: the notification-settings plane]"
        );
        store_parse::store_flags(out, []);
    }
    fn parse_notification_settings(
        &self,
        parser: &mut Parser,
    ) -> std::result::Result<(), &'static str> {
        tdutils::log_info!(
            "parse_notification_settings — [blocked: the notification-settings plane]"
        );
        parser.parse_flags::<0>()?;
        Ok(())
    }
    fn store_draft_message(&self, out: &mut Vec<u8>) {
        tdutils::log_info!("store_draft_message — [blocked: the draft plane]");
        store_parse::store_flags(out, []);
    }
    fn parse_draft_message(&self, parser: &mut Parser) -> std::result::Result<(), &'static str> {
        tdutils::log_info!("parse_draft_message — [blocked: the draft plane]");
        parser.parse_flags::<0>()?;
        Ok(())
    }
    fn get_draft_message_order(&self) -> i64 {
        0
    }
}

/// `ForumTopicManager::Topic` (ForumTopicManager.h:151-165).
struct Topic {
    info: Option<ForumTopicInfo>,
    has_topic: bool,
    receive_date: i32,
    /// The C++ per-topic message count — the reads land with the message
    /// plane.
    #[allow(dead_code)]
    message_count: i32,
    need_save_to_database: bool,
}

/// The C++ `Topic::store`/`parse` — the database round-trip rides the
/// env seams until the topic-db plane lands.
#[allow(dead_code)]
impl Topic {
    /// `Topic::store` (ForumTopicManager.cpp:467-490) — the MAGIC + info
    /// + topic halves; the notification-settings/draft ride the env.
    fn store(&self, env: &impl ForumTopicEnv, out: &mut Vec<u8>) {
        store_parse::store_i32(out, 0x1fac3901u32 as i32); // the C++ MAGIC
        let has_info = self.info.is_some();
        let has_topic = self.has_topic;
        store_parse::store_flags(out, [has_info, has_topic]);
        store_parse::store_i32(out, self.receive_date);
        store_parse::store_i32(out, self.message_count);
        if let Some(info) = &self.info {
            info.store(out);
        }
        if has_topic {
            // the ForumTopic store/parse rides the env for the
            // notification-settings and draft halves.
            env.store_notification_settings(out);
            env.store_draft_message(out);
        }
    }

    /// `Topic::parse` (ForumTopicManager.cpp:491-503).
    fn parse(
        env: &impl ForumTopicEnv,
        parser: &mut Parser,
    ) -> std::result::Result<Topic, &'static str> {
        let magic = parser.i32_()?;
        if magic != 0x1fac3901u32 as i32 {
            return Err("invalid forum topic magic");
        }
        let [has_info, has_topic] = parser.parse_flags()?;
        let mut result = Topic {
            info: None,
            has_topic,
            receive_date: parser.i32_()?,
            message_count: parser.i32_()?,
            need_save_to_database: true,
        };
        if has_info {
            result.info = Some(ForumTopicInfo::parse(parser)?);
        }
        if has_topic {
            env.parse_notification_settings(parser)?;
            env.parse_draft_message(parser)?;
        }
        Ok(result)
    }
}

/// `ForumTopicManager::MAX_FORUM_TOPIC_TITLE_LENGTH` (ForumTopicManager.h:149).
const MAX_FORUM_TOPIC_TITLE_LENGTH: usize = 128;

/// `td::ForumTopicManager` (ForumTopicManager.h:19-150).
#[derive(Default)]
pub struct ForumTopicManager {
    topics: HashMap<DialogId, HashMap<ForumTopicId, Topic>>,
    deleted_topic_ids: HashMap<DialogId, Vec<ForumTopicId>>,
    pinned_topic_ids: HashMap<DialogId, Vec<ForumTopicId>>,
    /// The manager-held dispatch surface of the net-drain 41 families —
    /// `None` keeps the net-inert posture (the pre-init windows and the
    /// net-inert tests; the `GroupCallManager::net` shape).
    pub(crate) net: Option<crate::td::ForumTopicNetHandle>,
    /// The parked family promises (the `ResultHandler::promise_` members).
    pub(crate) pending: PendingForumTopicQueries,
}

// The request methods keep the C++-faithful argument lists (the
// convention's too_many_arguments note).
#[allow(clippy::too_many_arguments)]
impl ForumTopicManager {
    pub fn new() -> ForumTopicManager {
        ForumTopicManager::default()
    }

    /// `can_be_forum_topic_id` (ForumTopicManager.cpp:449-452).
    pub fn can_be_forum_topic_id(forum_topic_id: ForumTopicId) -> Status {
        if !forum_topic_id.is_valid() {
            return Status::error(400, "Invalid forum topic identifier specified");
        }
        Status::ok()
    }

    /// `get_forum_topic_id_object` (ForumTopicManager.cpp:441-447) — the
    /// general topic answers as the C++ message-thread mapping.
    pub fn get_forum_topic_id_object(dialog_id: DialogId, forum_topic_id: ForumTopicId) -> i32 {
        let _ = dialog_id;
        forum_topic_id.get()
    }

    fn get_topic(&self, dialog_id: DialogId, forum_topic_id: ForumTopicId) -> Option<&Topic> {
        self.topics
            .get(&dialog_id)
            .and_then(|topics| topics.get(&forum_topic_id))
    }

    /// Test-only: the registry probe (the netted round-trips' intake
    /// assertions).
    #[cfg(test)]
    pub(crate) fn has_topic_for_test(
        &self,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
    ) -> bool {
        self.get_topic(dialog_id, forum_topic_id).is_some()
    }

    /// Test-only: the deleted-id probe (the delete round-trip's mark
    /// assertion).
    #[cfg(test)]
    pub(crate) fn is_deleted_for_test(
        &self,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
    ) -> bool {
        self.deleted_topic_ids
            .get(&dialog_id)
            .is_some_and(|deleted| deleted.contains(&forum_topic_id))
    }

    fn get_topic_mut(
        &mut self,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
    ) -> Option<&mut Topic> {
        self.topics
            .get_mut(&dialog_id)
            .and_then(|topics| topics.get_mut(&forum_topic_id))
    }

    fn add_topic(
        &mut self,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
    ) -> Option<&mut Topic> {
        if self
            .deleted_topic_ids
            .get(&dialog_id)
            .is_some_and(|deleted| deleted.contains(&forum_topic_id))
        {
            return None;
        }
        Some(
            self.topics
                .entry(dialog_id)
                .or_default()
                .entry(forum_topic_id)
                .or_insert_with(|| Topic {
                    info: None,
                    has_topic: false,
                    receive_date: 0,
                    message_count: 0,
                    need_save_to_database: true,
                }),
        )
    }

    /// The `td_->create_handler<…>()->send(...)` collapse — the promise
    /// parks in the family lot first (the handler's `promise_`), then the
    /// peer resolves and the query dispatches (the `StoryEnv` convention:
    /// a peer miss files the C++ `on_error(400, "Can't access the chat")`
    /// inline settle through the completed queue, so the full `on_error`
    /// tail runs — the dialog-error head + the promise error). `None` on
    /// the handle keeps the net-inert posture.
    fn send_forum_topic_net_query(
        &mut self,
        kind: ForumTopicNetKind,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        write: bool,
        what: &str,
        build: impl FnOnce(telegram_api::InputPeer) -> telegram_api::Function,
    ) {
        let peer = env.get_input_peer(dialog_id, write);
        match (self.net.clone(), peer) {
            (Some(net), Some(peer)) => net.dispatch(kind, build(peer)),
            (Some(net), None) => net.complete_error(kind, 400, "Can't access the chat"),
            (None, _) => tdutils::log_info!("Send {what} — [net-inert]"),
        }
    }

    /// `create_forum_topic` (ForumTopicManager.cpp:515-547).
    pub fn create_forum_topic(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        title: &str,
        title_missing: bool,
        icon: Option<td_api::ForumTopicIcon>,
        promise: Promise<Option<td_api::ForumTopicInfo>>,
    ) {
        let mut promise = promise;
        let status = env.is_forum(dialog_id, true);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        if dialog_id.get_type() == DialogType::Channel
            && !env.get_channel_permissions_can_create_topics(dialog_id)
        {
            promise.set_error(Status::error(400, "Not enough rights to create a topic"));
            return;
        }
        let new_title = crate::misc::clean_name(title, MAX_FORUM_TOPIC_TITLE_LENGTH);
        if new_title.is_empty() {
            promise.set_error(Status::error(400, "Name must be non-empty"));
            return;
        }
        let mut icon_color = -1;
        let mut icon_custom_emoji_id = CustomEmojiId::default();
        if let Some(icon) = icon {
            icon_color = icon.color;
            if !(0..=0xFFFFFF).contains(&icon_color) {
                promise.set_error(Status::error(400, "Invalid icon color specified"));
                return;
            }
            icon_custom_emoji_id = CustomEmojiId::new(icon.custom_emoji_id);
        }
        let as_dialog_id = env.get_default_send_message_as_dialog_id(dialog_id);
        // `CreateForumTopicQuery::send` (cpp:57-93) — the flags, the
        // send-as peer override and the random id; the main peer miss
        // settles through the completed queue's `on_error` tail.
        let mut creator_dialog_id = env.get_my_dialog_id();
        let mut flags = 0;
        if icon_color != -1 {
            flags |= 1; // ICON_COLOR_MASK
        }
        if icon_custom_emoji_id.is_valid() {
            flags |= 1 << 3; // ICON_EMOJI_ID_MASK
        }
        let mut as_input_peer = None;
        if as_dialog_id.is_valid()
            && let Some(peer) = env.get_input_peer(as_dialog_id, true)
        {
            flags |= 1 << 2; // SEND_AS_MASK
            creator_dialog_id = as_dialog_id;
            as_input_peer = Some(peer);
        }
        let mut random_id = tdutils::random::secure_int64();
        while random_id == 0 {
            random_id = tdutils::random::secure_int64();
        }
        let kind = ForumTopicNetKind::CreateForumTopic {
            dialog_id,
            creator_dialog_id,
            random_id,
        };
        self.pending.create_topic.push(promise);
        let title = new_title;
        self.send_forum_topic_net_query(
            kind,
            env,
            dialog_id,
            true,
            "messages.createForumTopic",
            |peer| {
                telegram_api::Function::messages_createForumTopic(
                    telegram_api::messages_createForumTopic {
                        flags,
                        title_missing,
                        peer: Some(Box::new(peer)),
                        title,
                        icon_color,
                        icon_emoji_id: icon_custom_emoji_id.get(),
                        random_id,
                        send_as: as_input_peer.map(Box::new),
                    },
                )
            },
        );
    }

    /// `on_forum_topic_created` (ForumTopicManager.cpp:548-564).
    pub fn on_forum_topic_created(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        forum_topic_info: ForumTopicInfo,
        promise: Promise<Option<td_api::ForumTopicInfo>>,
    ) {
        let mut promise = promise;
        let status = env.close_status();
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let forum_topic_id = forum_topic_info.get_forum_topic_id();
        // `add_topic` answers `None` for a deleted topic — the promise
        // settles with the fresh info without the registry intake
        // (cpp:556-559).
        let Some(topic) = self.add_topic(dialog_id, forum_topic_id) else {
            promise.set_value(Some(forum_topic_info.get_forum_topic_info_object()));
            return;
        };
        // `set_topic_info` (cpp:1356-1368) — the None branch only (the
        // C++ guard); the update skips bots, the save flag rides the
        // database seam.
        if topic.info.is_none() {
            topic.info = Some(forum_topic_info);
            topic.need_save_to_database = true;
            let info = topic.info.as_ref().expect("topic info").clone();
            Self::send_update_forum_topic_info(env, &info);
        }
        let info = topic.info.as_ref().expect("topic info").clone();
        promise.set_value(Some(info.get_forum_topic_info_object()));
    }

    /// `edit_forum_topic` (ForumTopicManager.cpp:565-593).
    pub fn edit_forum_topic(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
        title: &str,
        edit_icon_custom_emoji: bool,
        icon_custom_emoji_id: CustomEmojiId,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let status = env.is_forum(dialog_id, true);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let status = Self::can_be_forum_topic_id(forum_topic_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        if dialog_id.get_type() == DialogType::Channel
            && !env.get_channel_permissions_can_edit_topics(dialog_id)
            && let Some(topic) = self.get_topic(dialog_id, forum_topic_id)
            && let Some(info) = &topic.info
            && !info.is_outgoing()
        {
            promise.set_error(Status::error(400, "Not enough rights to edit the topic"));
            return;
        }
        let edit_title = !title.is_empty();
        let new_title = crate::misc::clean_name(title, MAX_FORUM_TOPIC_TITLE_LENGTH);
        if edit_title && new_title.is_empty() {
            promise.set_error(Status::error(400, "Name must be non-empty"));
            return;
        }
        if !edit_title && !edit_icon_custom_emoji {
            promise.set_value(());
            return;
        }
        // `EditForumTopicQuery::send` (cpp:150-167) — the title/icon wire
        // form; the answer walks the Updates engine with the promise.
        let kind = ForumTopicNetKind::EditForumTopic { dialog_id };
        self.pending.edit_topic.push(promise);
        let mut flags = 0;
        if edit_title {
            flags |= 1; // TITLE_MASK
        }
        if edit_icon_custom_emoji {
            flags |= 1 << 1; // ICON_EMOJI_ID_MASK
        }
        self.send_forum_topic_net_query(
            kind,
            env,
            dialog_id,
            true,
            "messages.editForumTopic",
            |peer| {
                telegram_api::Function::messages_editForumTopic(
                    telegram_api::messages_editForumTopic {
                        flags,
                        peer: Some(Box::new(peer)),
                        topic_id: forum_topic_id.get(),
                        title: new_title.clone(),
                        icon_emoji_id: icon_custom_emoji_id.get(),
                        closed: false,
                        hidden: false,
                    },
                )
            },
        );
    }

    /// `get_forum_topic` (ForumTopicManager.cpp:829-847) — the bot cache
    /// answers when the topic was received within the last 60 seconds;
    /// otherwise the `GetForumTopicQuery` wire read.
    pub fn get_forum_topic(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
        promise: Promise<Option<td_api::ForumTopic>>,
    ) {
        let mut promise = promise;
        let status = env.is_forum(dialog_id, true);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let status = Self::can_be_forum_topic_id(forum_topic_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        if env.is_bot()
            && let Some(topic) = self.get_topic(dialog_id, forum_topic_id)
            && topic.has_topic
            && topic.receive_date > env.unix_time() - 60
            && let Some(info) = &topic.info
        {
            let object = td_api::ForumTopic {
                info: Some(info.get_forum_topic_info_object()),
                ..td_api::ForumTopic::default()
            };
            promise.set_value(Some(object));
            return;
        }
        // `GetForumTopicQuery::send` (cpp:315-328) — the Read peer leg.
        let kind = ForumTopicNetKind::GetForumTopic {
            dialog_id,
            forum_topic_id,
        };
        self.pending.get_topic.push(promise);
        self.send_forum_topic_net_query(
            kind,
            env,
            dialog_id,
            false,
            "messages.getForumTopicsByID",
            |peer| {
                telegram_api::Function::messages_getForumTopicsByID(
                    telegram_api::messages_getForumTopicsByID {
                        peer: Some(Box::new(peer)),
                        topics: vec![forum_topic_id.get()],
                    },
                )
            },
        );
    }

    /// `get_forum_topic_link` (ForumTopicManager.cpp:881-901).
    pub fn get_forum_topic_link(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
        mut promise: Promise<td_api::MessageLink>,
    ) {
        let status = env.is_forum(dialog_id, false);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let status = Self::can_be_forum_topic_id(forum_topic_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let mut link = "https://t.me/".to_owned();
        let mut is_public = false;
        let dialog_username = env.get_channel_first_username(dialog_id);
        if !dialog_username.is_empty() {
            link.push_str(&dialog_username);
            is_public = true;
        } else {
            link.push_str(&format!("c/{}", dialog_id.get_channel_id().get()));
        }
        link.push_str(&format!("/{}", forum_topic_id.get()));
        promise.set_value(td_api::MessageLink { link, is_public });
    }

    /// `get_forum_topics` (ForumTopicManager.cpp:903-923).
    pub fn get_forum_topics(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        query: String,
        offset_date: i32,
        offset_message_id: MessageId,
        offset_forum_topic_id: ForumTopicId,
        limit: i32,
        promise: Promise<td_api::ForumTopics>,
    ) {
        let mut promise = promise;
        let status = env.is_forum(dialog_id, true);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        if offset_date < 0 {
            promise.set_error(Status::error(400, "Invalid offset date specified"));
            return;
        }
        if offset_message_id != MessageId::default() && !offset_message_id.is_server() {
            promise.set_error(Status::error(
                400,
                "Invalid offset message identifier specified",
            ));
            return;
        }
        if offset_forum_topic_id != ForumTopicId::default() {
            let status = Self::can_be_forum_topic_id(offset_forum_topic_id);
            if status.is_error() {
                promise.set_error(status);
                return;
            }
        }
        if limit <= 0 {
            promise.set_error(Status::error(400, "Invalid limit specified"));
            return;
        }
        // `GetForumTopicsQuery::send` (cpp:397-418) — the Q flag rides
        // the non-empty query band.
        let kind = ForumTopicNetKind::GetForumTopics { dialog_id };
        self.pending.get_topics.push(promise);
        let flags = if query.is_empty() { 0 } else { 1 }; // Q_MASK
        self.send_forum_topic_net_query(
            kind,
            env,
            dialog_id,
            false,
            "messages.getForumTopics",
            |peer| {
                telegram_api::Function::messages_getForumTopics(
                    telegram_api::messages_getForumTopics {
                        flags,
                        peer: Some(Box::new(peer)),
                        q: query.clone(),
                        offset_date,
                        offset_id: offset_message_id.get_server_message_id().get(),
                        offset_topic: offset_forum_topic_id.get(),
                        limit,
                    },
                )
            },
        );
    }

    /// `toggle_forum_topic_is_closed` (ForumTopicManager.cpp:957-971).
    pub fn toggle_forum_topic_is_closed(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
        is_closed: bool,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        let status = env.is_forum(dialog_id, false);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let status = Self::can_be_forum_topic_id(forum_topic_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        if !env.get_channel_permissions_can_edit_topics(dialog_id)
            && let Some(topic) = self.get_topic(dialog_id, forum_topic_id)
            && let Some(info) = &topic.info
            && !info.is_outgoing()
        {
            promise.set_error(Status::error(
                400,
                "Not enough rights to close or open the topic",
            ));
            return;
        }
        // `EditForumTopicQuery::send` (cpp:169-186) — the CLOSED wire
        // form; the answer walks the Updates engine with the promise.
        let kind = ForumTopicNetKind::EditForumTopic { dialog_id };
        self.pending.edit_topic.push(promise);
        self.send_forum_topic_net_query(
            kind,
            env,
            dialog_id,
            true,
            "messages.editForumTopic (closed)",
            |peer| {
                telegram_api::Function::messages_editForumTopic(
                    telegram_api::messages_editForumTopic {
                        flags: 1 << 2, // CLOSED_MASK
                        peer: Some(Box::new(peer)),
                        topic_id: forum_topic_id.get(),
                        title: String::new(),
                        icon_emoji_id: 0,
                        closed: is_closed,
                        hidden: false,
                    },
                )
            },
        );
    }

    /// `toggle_forum_topic_is_hidden` (ForumTopicManager.cpp:973-982).
    pub fn toggle_forum_topic_is_hidden(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        is_hidden: bool,
        mut promise: Promise<()>,
    ) {
        let status = env.is_forum(dialog_id, false);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        if !env.get_channel_permissions_can_edit_topics(dialog_id) {
            promise.set_error(Status::error(
                400,
                "Not enough rights to close or open the topic",
            ));
            return;
        }
        // `EditForumTopicQuery::send` (cpp:188-205) — the HIDDEN wire
        // form over the general topic id (cpp:191).
        let kind = ForumTopicNetKind::EditForumTopic { dialog_id };
        self.pending.edit_topic.push(promise);
        self.send_forum_topic_net_query(
            kind,
            env,
            dialog_id,
            true,
            "messages.editForumTopic (hidden)",
            |peer| {
                telegram_api::Function::messages_editForumTopic(
                    telegram_api::messages_editForumTopic {
                        flags: 1 << 3, // HIDDEN_MASK
                        peer: Some(Box::new(peer)),
                        topic_id: ForumTopicId::general().get(),
                        title: String::new(),
                        icon_emoji_id: 0,
                        closed: false,
                        hidden: is_hidden,
                    },
                )
            },
        );
    }

    /// `toggle_forum_topic_is_pinned` (ForumTopicManager.cpp:984-998).
    pub fn toggle_forum_topic_is_pinned(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
        is_pinned: bool,
        mut promise: Promise<()>,
    ) {
        let status = env.is_forum(dialog_id, true);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let status = Self::can_be_forum_topic_id(forum_topic_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        if dialog_id.get_type() == DialogType::Channel
            && !env.get_channel_permissions_can_pin_topics(dialog_id)
        {
            promise.set_error(Status::error(
                400,
                "Not enough rights to pin or unpin the topic",
            ));
            return;
        }
        // `UpdatePinnedForumTopicQuery::send` (cpp:229-240). The C++
        // never optimistically flips the local pin — the server's
        // `updatePinnedForumTopics` walk (the updates engine's Wave-2
        // intake) owns the local state.
        let kind = ForumTopicNetKind::UpdatePinnedForumTopic { dialog_id };
        self.pending.update_pinned.push(promise);
        self.send_forum_topic_net_query(
            kind,
            env,
            dialog_id,
            true,
            "messages.updatePinnedForumTopic",
            |peer| {
                telegram_api::Function::messages_updatePinnedForumTopic(
                    telegram_api::messages_updatePinnedForumTopic {
                        peer: Some(Box::new(peer)),
                        topic_id: forum_topic_id.get(),
                        pinned: is_pinned,
                    },
                )
            },
        );
    }

    /// `set_pinned_forum_topics` (ForumTopicManager.cpp:999-1015).
    pub fn set_pinned_forum_topics(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        forum_topic_ids: Vec<ForumTopicId>,
        mut promise: Promise<()>,
    ) {
        let status = env.is_forum(dialog_id, true);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        for forum_topic_id in &forum_topic_ids {
            let status = Self::can_be_forum_topic_id(*forum_topic_id);
            if status.is_error() {
                promise.set_error(status);
                return;
            }
        }
        if dialog_id.get_type() == DialogType::Channel
            && !env.get_channel_permissions_can_pin_topics(dialog_id)
        {
            promise.set_error(Status::error(
                400,
                "Not enough rights to reorder forum topics",
            ));
            return;
        }
        // `ReorderPinnedForumTopicsQuery::send` (cpp:273-286) — the wire
        // order rides the request ids (cpp:283); the local pin state
        // updates through the server's update walk, like the toggle.
        let kind = ForumTopicNetKind::ReorderPinnedForumTopics { dialog_id };
        self.pending.reorder_pinned.push(promise);
        let order: Vec<i32> = forum_topic_ids.iter().map(|id| id.get()).collect();
        self.send_forum_topic_net_query(
            kind,
            env,
            dialog_id,
            true,
            "messages.reorderPinnedForumTopics",
            |peer| {
                telegram_api::Function::messages_reorderPinnedForumTopics(
                    telegram_api::messages_reorderPinnedForumTopics {
                        flags: 0,
                        force: true,
                        peer: Some(Box::new(peer)),
                        order: order.clone(),
                    },
                )
            },
        );
    }

    /// `delete_forum_topic` (ForumTopicManager.cpp:1016-1039).
    pub fn delete_forum_topic(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
        mut promise: Promise<()>,
    ) {
        let status = env.is_forum(dialog_id, true);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let status = Self::can_be_forum_topic_id(forum_topic_id);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        if dialog_id.get_type() == DialogType::Channel
            && !env.get_channel_permissions_can_delete_messages(dialog_id)
            && let Some(topic) = self.get_topic(dialog_id, forum_topic_id)
            && let Some(info) = &topic.info
            && !info.is_outgoing()
        {
            promise.set_error(Status::error(400, "Not enough rights to delete the topic"));
            return;
        }
        // The `MessageQueryManager::delete_topic_history` tail — the
        // wire query rides the landed affected-history plane; the C++
        // delete-promise lambda routes its completion into
        // `on_delete_forum_topic` (the td.rs env seam owns the chain).
        // The `MessageQueryManager::delete_topic_history` tail — the
        // wire query rides the landed affected-history plane; the C++
        // delete-promise lambda routes its completion into
        // `on_delete_forum_topic` (the td-level tail runner owns the
        // chain, ForumTopicManager.cpp:1028-1037).
        self.pending
            .delete_tails
            .push((dialog_id, forum_topic_id, promise));
    }

    /// `on_forum_topic_edited` (ForumTopicManager.cpp:1064-1080) — the
    /// local info application.
    pub fn on_forum_topic_edited(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
        edited_data: &ForumTopicEditedData,
    ) {
        if let Some(topic) = self.get_topic_mut(dialog_id, forum_topic_id)
            && let Some(info) = &mut topic.info
            && info.apply_edited_data(edited_data)
        {
            let info = info.clone();
            topic.need_save_to_database = true;
            Self::send_update_forum_topic_info(env, &info);
        }
    }

    /// `on_delete_forum_topic` (ForumTopicManager.cpp:1040-1050) — the
    /// registry erase + the deleted-id mark (the topic's pin state dies
    /// with the record, like the C++ `topics_.erase`); the database half
    /// settles immediately (the thread-db seam, cpp:1413-1422).
    pub fn on_delete_forum_topic(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
        mut promise: Promise<()>,
    ) {
        let status = env.close_status();
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        if let Some(topics) = self.topics.get_mut(&dialog_id) {
            topics.remove(&forum_topic_id);
            let deleted = self.deleted_topic_ids.entry(dialog_id).or_default();
            if !deleted.contains(&forum_topic_id) {
                deleted.push(forum_topic_id);
            }
        }
        if let Some(pinned) = self.pinned_topic_ids.get_mut(&dialog_id) {
            pinned.retain(|id| *id != forum_topic_id);
        }
        promise.set_value(());
    }

    /// `set_topic_info` (ForumTopicManager.cpp:1356-1368) — the
    /// None-or-differs replace with the `updateForumTopicInfo` emission
    /// (bots skip) and the save flag. An associated fn — the callers
    /// hold the `&mut Topic` borrow.
    fn set_topic_info(
        env: &mut impl ForumTopicEnv,
        topic: &mut Topic,
        forum_topic_info: ForumTopicInfo,
    ) {
        if topic.info.as_ref() != Some(&forum_topic_info) {
            topic.info = Some(forum_topic_info);
            topic.need_save_to_database = true;
            let info = topic.info.as_ref().expect("topic info").clone();
            Self::send_update_forum_topic_info(env, &info);
        }
    }

    /// `on_get_forum_topic_info` (ForumTopicManager.cpp:1082-1098) — the
    /// quiet cache intake (no `updateForumTopic` object emission).
    pub fn on_get_forum_topic_info(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        topic_info: &ForumTopicInfo,
        source: &str,
    ) {
        if !env.can_be_forum(dialog_id) {
            tdutils::log_error!("Receive forum topics in {dialog_id:?} from {source}");
            return;
        }
        let forum_topic_id = topic_info.get_forum_topic_id();
        if let Some(topic) = self.add_topic(dialog_id, forum_topic_id) {
            Self::set_topic_info(env, topic, topic_info.clone());
        }
    }

    /// `on_get_forum_topic_infos` (ForumTopicManager.cpp:1102-1127) — the
    /// `messages_Messages` topics intake: the null-dialog form resolves
    /// each topic's dialog off its wire peer, the fixed form pins it.
    pub fn on_get_forum_topic_infos(
        &mut self,
        env: &mut impl ForumTopicEnv,
        mut dialog_id: DialogId,
        forum_topics: Vec<telegram_api::ForumTopic>,
        source: &str,
    ) {
        if forum_topics.is_empty() {
            return;
        }
        if dialog_id == DialogId::default() {
            for forum_topic in forum_topics {
                let forum_topic_info =
                    ForumTopicInfo::from_telegram(&forum_topic, DialogId::default());
                let forum_topic_id = forum_topic_info.get_forum_topic_id();
                if !forum_topic_id.is_valid() {
                    continue;
                }
                dialog_id = forum_topic_info.get_dialog_id();
                if !env.can_be_forum(dialog_id) {
                    tdutils::log_error!("Receive forum topics in {dialog_id:?} from {source}");
                    return;
                }
                if let Some(topic) = self.add_topic(dialog_id, forum_topic_id) {
                    Self::set_topic_info(env, topic, forum_topic_info);
                }
            }
            return;
        }
        if !env.can_be_forum(dialog_id) {
            tdutils::log_error!("Receive forum topics in {dialog_id:?} from {source}");
            return;
        }
        for forum_topic in forum_topics {
            let forum_topic_info = ForumTopicInfo::from_telegram(&forum_topic, dialog_id);
            let forum_topic_id = forum_topic_info.get_forum_topic_id();
            if !forum_topic_id.is_valid() {
                continue;
            }
            if let Some(topic) = self.add_topic(dialog_id, forum_topic_id) {
                Self::set_topic_info(env, topic, forum_topic_info);
            }
        }
    }

    /// `on_get_forum_topic_impl` (ForumTopicManager.cpp:1129-1192) — the
    /// full topic intake: the deleted form runs the delete tail, the
    /// live form fills the record, updates the pin membership (the wire
    /// `pinned` flag — the C++ `ForumTopic` constructor's
    /// `is_pinned_`), emits both updates and answers the id. The
    /// `topic_` object halves (the notification settings, the draft,
    /// the unread counts, the last message) collapse onto `has_topic`.
    pub(crate) fn on_get_forum_topic_impl(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        forum_topic: telegram_api::ForumTopic,
    ) -> ForumTopicId {
        match forum_topic {
            telegram_api::ForumTopic::forumTopicDeleted(deleted) => {
                let forum_topic_id = ForumTopicId::new(deleted.id);
                if !forum_topic_id.is_valid() {
                    tdutils::log_error!("Receive forumTopicDeleted with id {}", deleted.id);
                    return ForumTopicId::default();
                }
                self.on_delete_forum_topic(env, dialog_id, forum_topic_id, Promise::default());
                ForumTopicId::default()
            }
            telegram_api::ForumTopic::forumTopic(topic) => {
                let is_short = topic.short;
                let is_pinned = topic.pinned;
                let forum_topic_info = ForumTopicInfo::from_telegram(
                    &telegram_api::ForumTopic::forumTopic(topic),
                    dialog_id,
                );
                let forum_topic_id = forum_topic_info.get_forum_topic_id();
                // the collapsed `ForumTopic::set_is_pinned` runs before
                // the record borrow
                self.set_pinned(dialog_id, forum_topic_id, is_pinned);
                // the C++ `add_topic` CHECK band (cpp:1151) — the reset
                // info's invalid id answers the invalid marker here
                let Some(topic) = self.add_topic(dialog_id, forum_topic_id) else {
                    return ForumTopicId::default();
                };
                if is_short && !env.is_bot() {
                    tdutils::log_error!("Receive short forum topic");
                    return ForumTopicId::default();
                }
                topic.has_topic = true;
                topic.need_save_to_database = true;
                topic.receive_date = env.unix_time();
                Self::set_topic_info(env, topic, forum_topic_info);
                Self::send_update_forum_topic(env, dialog_id, forum_topic_id, is_pinned);
                forum_topic_id
            }
        }
    }

    /// The collapsed `ForumTopic::set_is_pinned` — the wire flag flips
    /// the dialog's pin membership (front insert for the pin order, the
    /// `on_update_pinned_forum_topics` order vector's head).
    fn set_pinned(&mut self, dialog_id: DialogId, forum_topic_id: ForumTopicId, is_pinned: bool) {
        let mut pinned = self
            .pinned_topic_ids
            .get(&dialog_id)
            .cloned()
            .unwrap_or_default();
        if is_pinned {
            if !pinned.contains(&forum_topic_id) {
                pinned.insert(0, forum_topic_id);
            }
        } else {
            pinned.retain(|id| *id != forum_topic_id);
        }
        self.pinned_topic_ids.insert(dialog_id, pinned);
    }

    /// `get_forum_topic_object` (ForumTopicManager.cpp:1325-1333) — the
    /// collapsed object: the info + the defaults (the last-message and
    /// count halves ride the messages/notification planes).
    fn get_forum_topic_object(
        &self,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
    ) -> Option<td_api::ForumTopic> {
        let topic = self.get_topic(dialog_id, forum_topic_id)?;
        if !topic.has_topic {
            return None;
        }
        let info = topic.info.as_ref()?;
        Some(td_api::ForumTopic {
            info: Some(info.get_forum_topic_info_object()),
            is_pinned: self
                .pinned_topic_ids
                .get(&dialog_id)
                .is_some_and(|pinned| pinned.contains(&forum_topic_id)),
            ..td_api::ForumTopic::default()
        })
    }

    /// `on_get_forum_topic` (ForumTopicManager.cpp:863-879) — the
    /// `GetForumTopicQuery` answer half over the intake; the
    /// `on_get_messages` walk rides the messages plane (the C++
    /// cpp:871-872; the collapsed port drops the messages with the
    /// `get_messages_info` convention).
    pub fn on_get_forum_topic(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        expected_forum_topic_id: ForumTopicId,
        topic: telegram_api::ForumTopic,
        mut promise: Promise<Option<td_api::ForumTopic>>,
    ) {
        let status = env.is_forum(dialog_id, true);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let forum_topic_id = self.on_get_forum_topic_impl(env, dialog_id, topic);
        if !forum_topic_id.is_valid() {
            promise.set_value(None);
            return;
        }
        if forum_topic_id != expected_forum_topic_id {
            promise.set_error(Status::error(500, "Wrong forum topic received"));
            return;
        }
        promise.set_value(self.get_forum_topic_object(dialog_id, forum_topic_id));
    }

    /// `on_get_forum_topics` (ForumTopicManager.cpp:924-955) — the
    /// `GetForumTopicsQuery` answer half: the per-topic intake walk and
    /// the next-offset computation (the collapsed model's topics carry
    /// no last message, so the creation date answers every offset date
    /// and the message id stays zero, like the C++ null-message band).
    pub fn on_get_forum_topics(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        _order_by_creation_date: bool,
        total_count: i32,
        topics: Vec<telegram_api::ForumTopic>,
        mut promise: Promise<td_api::ForumTopics>,
    ) {
        let status = env.is_forum(dialog_id, true);
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let mut forum_topics = Vec::new();
        let mut next_offset_date = 0;
        let mut next_offset_forum_topic_id = ForumTopicId::default();
        for topic in topics {
            let forum_topic_id = self.on_get_forum_topic_impl(env, dialog_id, topic);
            if !forum_topic_id.is_valid() {
                continue;
            }
            let Some(forum_topic_object) = self.get_forum_topic_object(dialog_id, forum_topic_id)
            else {
                // the C++ CHECK band (cpp:941-942) — the intake above
                // guarantees the record; skip defensively
                continue;
            };
            // the C++ null-last-message band answers the info's creation
            // date (cpp:944-948) — the collapsed model has no last
            // message, so both the order-by and the default branches read
            // it (the parameter returns with the messages plane)
            next_offset_date = forum_topic_object
                .info
                .as_ref()
                .map(|info| info.creation_date)
                .unwrap_or(next_offset_date);
            next_offset_forum_topic_id = forum_topic_id;
            forum_topics.push(forum_topic_object);
        }
        promise.set_value(td_api::ForumTopics {
            total_count,
            topics: forum_topics,
            next_offset_date,
            next_offset_message_id: 0,
            next_offset_forum_topic_id: next_offset_forum_topic_id.get(),
        });
    }

    /// `on_topic_message_count_changed` (ForumTopicManager.cpp:1424-1444).
    pub fn on_topic_message_count_changed(
        &mut self,
        _dialog_id: DialogId,
        _forum_topic_id: ForumTopicId,
        _diff: i32,
    ) {
        // the per-topic count rides the message plane
    }

    /// `get_current_state` (ForumTopicManager.cpp:1511-1518) — the
    /// local scheme's updateForumTopic carries the topic-state fields.
    pub fn get_current_state(&self, updates: &mut Vec<td_api::Update>) {
        for (dialog_id, topics) in &self.topics {
            for forum_topic_id in topics.keys() {
                let is_pinned = self
                    .pinned_topic_ids
                    .get(dialog_id)
                    .is_some_and(|pinned| pinned.contains(forum_topic_id));
                updates.push(td_api::Update::updateForumTopic(td_api::updateForumTopic {
                    chat_id: dialog_id.get(),
                    forum_topic_id: forum_topic_id.get(),
                    is_pinned,
                    last_read_inbox_message_id: 0,
                    last_read_outbox_message_id: 0,
                    unread_mention_count: 0,
                    unread_reaction_count: 0,
                    unread_poll_vote_count: 0,
                    notification_settings: None,
                    draft_message: None,
                }));
            }
        }
    }

    /// `send_update_forum_topic` (ForumTopicManager.cpp:1382-1388) —
    /// the local scheme's `updateForumTopic` carries the topic-state
    /// fields (the collapsed form of `get_update_forum_topic_object`;
    /// bots skip). An associated fn — the callers hold the record
    /// borrow; the pinned flag rides the caller's read.
    fn send_update_forum_topic(
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
        is_pinned: bool,
    ) {
        if env.is_bot() {
            return;
        }
        env.send_update(td_api::Update::updateForumTopic(td_api::updateForumTopic {
            chat_id: dialog_id.get(),
            forum_topic_id: forum_topic_id.get(),
            is_pinned,
            last_read_inbox_message_id: 0,
            last_read_outbox_message_id: 0,
            unread_mention_count: 0,
            unread_reaction_count: 0,
            unread_poll_vote_count: 0,
            notification_settings: None,
            draft_message: None,
        }));
    }

    /// `send_update_forum_topic_info` (ForumTopicManager.cpp:1369-1380) —
    /// the local scheme's updateForumTopicInfo carries only the info
    /// (bots skip). An associated fn — the callers hold the record
    /// borrow.
    fn send_update_forum_topic_info(env: &mut impl ForumTopicEnv, topic_info: &ForumTopicInfo) {
        if env.is_bot() {
            return;
        }
        env.send_update(td_api::Update::updateForumTopicInfo(
            td_api::updateForumTopicInfo {
                info: Some(topic_info.get_forum_topic_info_object()),
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdutils::promise::PromiseInterface;

    /// The promise interface that absorbs every completion.
    #[derive(Clone, Copy)]
    struct Absorbing;

    impl<T: Send + 'static> PromiseInterface<T> for Absorbing {
        fn set_value(&mut self, _value: T) {}
        fn set_error(&mut self, _error: tdutils::status::Status) {}
    }

    /// The recording promise — settles into a slot.
    type Slot<T> = std::sync::Arc<std::sync::Mutex<Option<tdutils::status::Result<T>>>>;

    fn record<T: Send + 'static>() -> (Promise<T>, Slot<T>) {
        struct Rec<T>(Slot<T>);
        impl<T: Send + 'static> PromiseInterface<T> for Rec<T> {
            fn set_value(&mut self, value: T) {
                *self.0.lock().unwrap() = Some(tdutils::status::Result::Ok(value));
            }
            fn set_error(&mut self, error: tdutils::status::Status) {
                *self.0.lock().unwrap() = Some(tdutils::status::Result::Err(error));
            }
        }
        let slot: Slot<T> = std::sync::Arc::new(std::sync::Mutex::new(None));
        (Promise::from_interface(Rec(slot.clone())), slot)
    }

    /// The minimal test env — the permission seams flip per band.
    struct TestEnv {
        bot: bool,
        unix_time: i32,
        updates: Vec<td_api::Update>,
        can_create_topics: bool,
        can_edit_topics: bool,
        can_pin_topics: bool,
        can_delete_messages: bool,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                bot: false,
                unix_time: 1000,
                updates: Vec::new(),
                can_create_topics: true,
                can_edit_topics: true,
                can_pin_topics: true,
                can_delete_messages: true,
            }
        }
    }

    impl ForumTopicEnv for TestEnv {
        fn is_bot(&self) -> bool {
            self.bot
        }

        fn unix_time(&mut self) -> i32 {
            self.unix_time
        }

        fn get_channel_permissions_can_create_topics(&self, _dialog_id: DialogId) -> bool {
            self.can_create_topics
        }

        fn get_channel_permissions_can_edit_topics(&self, _dialog_id: DialogId) -> bool {
            self.can_edit_topics
        }

        fn get_channel_permissions_can_pin_topics(&self, _dialog_id: DialogId) -> bool {
            self.can_pin_topics
        }

        fn get_channel_permissions_can_delete_messages(&self, _dialog_id: DialogId) -> bool {
            self.can_delete_messages
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }
    }

    /// A live wire `forumTopic` record for the intake tests.
    fn wire_topic(id: i32, date: i32, pinned: bool) -> telegram_api::ForumTopic {
        telegram_api::ForumTopic::forumTopic(telegram_api::forumTopic {
            flags: 0,
            my: true,
            closed: false,
            pinned,
            short: false,
            hidden: false,
            title_missing: false,
            id,
            date,
            peer: Some(telegram_api::Peer::peerChannel(telegram_api::peerChannel {
                channel_id: 777,
            })),
            title: format!("Topic {id}"),
            icon_color: 0,
            icon_emoji_id: 0,
            top_message: 0,
            read_inbox_max_id: 0,
            read_outbox_max_id: 0,
            unread_count: 0,
            unread_mentions_count: 0,
            unread_reactions_count: 0,
            unread_poll_votes_count: 0,
            from_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                user_id: 9,
            })),
            notify_settings: None,
            draft: None,
        })
    }

    /// The channel dialog of the test wire peer (`peerChannel` id 777).
    const FORUM: DialogId = DialogId::new(-1_000_000_000_777);

    fn topic_info(forum_topic_id: ForumTopicId) -> ForumTopicInfo {
        ForumTopicInfo::new(
            FORUM,
            forum_topic_id,
            "First".to_owned(),
            crate::forum_topic::ForumTopicIcon::default_icon(),
            100,
            DialogId::new(11),
            true,
            false,
            false,
            false,
        )
    }

    fn foreign_topic_info(forum_topic_id: ForumTopicId) -> ForumTopicInfo {
        ForumTopicInfo::new(
            FORUM,
            forum_topic_id,
            "Theirs".to_owned(),
            crate::forum_topic::ForumTopicIcon::default_icon(),
            100,
            DialogId::new(11),
            false,
            false,
            false,
            false,
        )
    }

    #[test]
    fn can_be_forum_topic_id_rejects_invalid_ids() {
        assert_eq!(
            ForumTopicManager::can_be_forum_topic_id(ForumTopicId::new(1)).code(),
            0
        );
        assert_eq!(
            ForumTopicManager::can_be_forum_topic_id(ForumTopicId::default()).code(),
            400
        );
    }

    #[test]
    fn create_topic_validates_the_name_and_rights() {
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv::default();
        // the empty (cleaned) name band (cpp:532-534)
        let (promise, slot) = record::<Option<td_api::ForumTopicInfo>>();
        manager.create_forum_topic(&mut env, FORUM, "", false, None, promise);
        assert!(
            matches!(&*slot.lock().unwrap(), Some(tdutils::status::Result::Err(e)) if e.message() == "Name must be non-empty")
        );
        // the rights band (cpp:524-527)
        env.can_create_topics = false;
        let (promise, slot) = record::<Option<td_api::ForumTopicInfo>>();
        manager.create_forum_topic(&mut env, FORUM, "Hi", false, None, promise);
        assert!(
            matches!(&*slot.lock().unwrap(), Some(tdutils::status::Result::Err(e)) if e.message() == "Not enough rights to create a topic")
        );
        // the icon color band (cpp:535-538)
        env.can_create_topics = true;
        let (promise, slot) = record::<Option<td_api::ForumTopicInfo>>();
        manager.create_forum_topic(
            &mut env,
            FORUM,
            "Hi",
            false,
            Some(td_api::ForumTopicIcon {
                color: 0x1_00_00_00,
                custom_emoji_id: 0,
            }),
            promise,
        );
        assert!(
            matches!(&*slot.lock().unwrap(), Some(tdutils::status::Result::Err(e)) if e.message() == "Invalid icon color specified")
        );
    }

    #[test]
    fn topic_created_fills_the_registry_and_sends_updates() {
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv::default();
        let info = topic_info(ForumTopicId::new(3));
        let (promise, slot) = record::<Option<td_api::ForumTopicInfo>>();
        manager.on_forum_topic_created(&mut env, FORUM, info, promise);
        assert_eq!(env.updates.len(), 1, "the updateForumTopicInfo emission");
        let object = match slot.lock().unwrap().take().unwrap() {
            tdutils::status::Result::Ok(Some(object)) => object,
            other => panic!("the topic info answer: {other:?}"),
        };
        assert_eq!(object.forum_topic_id, 3);
        assert!(manager.get_topic(FORUM, ForumTopicId::new(3)).is_some());
    }

    #[test]
    fn topic_created_on_deleted_topic_settles_without_intake() {
        // cpp:556-559 — the deleted-id band answers the fresh object
        // without the registry intake or the update
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv::default();
        manager
            .deleted_topic_ids
            .insert(FORUM, vec![ForumTopicId::new(3)]);
        let info = topic_info(ForumTopicId::new(3));
        let (promise, slot) = record::<Option<td_api::ForumTopicInfo>>();
        manager.on_forum_topic_created(&mut env, FORUM, info, promise);
        assert!(env.updates.is_empty(), "no update on the deleted band");
        let object = match slot.lock().unwrap().take().unwrap() {
            tdutils::status::Result::Ok(Some(object)) => object,
            other => panic!("the deleted-band answer: {other:?}"),
        };
        assert_eq!(object.forum_topic_id, 3);
        assert!(
            manager.get_topic(FORUM, ForumTopicId::new(3)).is_none(),
            "the deleted id stays deleted"
        );
    }

    #[test]
    fn topic_edited_applies_and_reports() {
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv::default();
        let info = foreign_topic_info(ForumTopicId::new(3));
        let promise = Promise::from_interface(Absorbing);
        manager.on_forum_topic_created(&mut env, FORUM, info, promise);
        let edited = ForumTopicEditedData::get_edited_data(
            true,
            false,
            CustomEmojiId::default(),
            false,
            Some("New"),
        );
        manager.on_forum_topic_edited(&mut env, FORUM, ForumTopicId::new(3), &edited);
        assert_eq!(env.updates.len(), 2);
    }

    #[test]
    fn edit_topic_title_flag_bands() {
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv::default();
        // the no-title/no-icon early ok (cpp:589-591)
        let (promise, slot) = record::<()>();
        manager.edit_forum_topic(
            &mut env,
            FORUM,
            ForumTopicId::new(3),
            "",
            false,
            CustomEmojiId::default(),
            promise,
        );
        assert!(matches!(
            &*slot.lock().unwrap(),
            Some(tdutils::status::Result::Ok(()))
        ));
        // the rights band against a non-outgoing local topic (cpp:574-582)
        env.can_edit_topics = false;
        let info = foreign_topic_info(ForumTopicId::new(3));
        let promise = Promise::from_interface(Absorbing);
        manager.on_forum_topic_created(&mut env, FORUM, info, promise);
        let (promise, slot) = record::<()>();
        manager.edit_forum_topic(
            &mut env,
            FORUM,
            ForumTopicId::new(3),
            "New",
            false,
            CustomEmojiId::default(),
            promise,
        );
        assert!(
            matches!(&*slot.lock().unwrap(), Some(tdutils::status::Result::Err(e)) if e.message() == "Not enough rights to edit the topic")
        );
    }

    #[test]
    fn toggle_closed_and_hidden_rights_bands() {
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv {
            can_edit_topics: false,
            ..TestEnv::default()
        };
        // the closed band against a non-outgoing local topic (cpp:965-969)
        let info = foreign_topic_info(ForumTopicId::new(3));
        let promise = Promise::from_interface(Absorbing);
        manager.on_forum_topic_created(&mut env, FORUM, info, promise);
        let (promise, slot) = record::<()>();
        manager.toggle_forum_topic_is_closed(&mut env, FORUM, ForumTopicId::new(3), true, promise);
        assert!(
            matches!(&*slot.lock().unwrap(), Some(tdutils::status::Result::Err(e)) if e.message() == "Not enough rights to close or open the topic")
        );
        // the hidden band is unconditional (cpp:978-980)
        let (promise, slot) = record::<()>();
        manager.toggle_forum_topic_is_hidden(&mut env, FORUM, true, promise);
        assert!(
            matches!(&*slot.lock().unwrap(), Some(tdutils::status::Result::Err(e)) if e.message() == "Not enough rights to close or open the topic")
        );
    }

    #[test]
    fn pin_families_rights_bands_and_no_local_flip() {
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv {
            can_pin_topics: false,
            ..TestEnv::default()
        };
        let (promise, slot) = record::<()>();
        manager.toggle_forum_topic_is_pinned(&mut env, FORUM, ForumTopicId::new(3), true, promise);
        assert!(
            matches!(&*slot.lock().unwrap(), Some(tdutils::status::Result::Err(e)) if e.message() == "Not enough rights to pin or unpin the topic")
        );
        let (promise, slot) = record::<()>();
        manager.set_pinned_forum_topics(&mut env, FORUM, vec![ForumTopicId::new(3)], promise);
        assert!(
            matches!(&*slot.lock().unwrap(), Some(tdutils::status::Result::Err(e)) if e.message() == "Not enough rights to reorder forum topics")
        );
        // the valid path never optimistically flips the local pin
        // (cpp:993-998 — the server's update walk owns it)
        env.can_pin_topics = true;
        let promise = Promise::from_interface(Absorbing);
        manager.toggle_forum_topic_is_pinned(&mut env, FORUM, ForumTopicId::new(3), true, promise);
        assert!(
            !manager.pinned_topic_ids.contains_key(&FORUM),
            "no local pin mutation at the send leg"
        );
    }

    #[test]
    fn delete_topic_rights_band_and_tail() {
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv {
            can_delete_messages: false,
            ..TestEnv::default()
        };
        // the outgoing local topic passes the band (cpp:1028-1033)
        let info = topic_info(ForumTopicId::new(3));
        let promise = Promise::from_interface(Absorbing);
        manager.on_forum_topic_created(&mut env, FORUM, info, promise);
        let promise = Promise::from_interface(Absorbing);
        manager.delete_forum_topic(&mut env, FORUM, ForumTopicId::new(3), promise);
        assert_eq!(
            manager.pending.delete_tails.len(),
            1,
            "the outgoing topic reaches the wire tail"
        );
        // the non-outgoing local topic hits the band
        let info = foreign_topic_info(ForumTopicId::new(4));
        let promise = Promise::from_interface(Absorbing);
        manager.on_forum_topic_created(&mut env, FORUM, info, promise);
        let (promise, slot) = record::<()>();
        manager.delete_forum_topic(&mut env, FORUM, ForumTopicId::new(4), promise);
        assert!(
            matches!(&*slot.lock().unwrap(), Some(tdutils::status::Result::Err(e)) if e.message() == "Not enough rights to delete the topic")
        );
        assert_eq!(manager.pending.delete_tails.len(), 1);
    }

    #[test]
    fn get_forum_topics_validation_bands() {
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv::default();
        let cases: [(&str, i32, MessageId, ForumTopicId, i32, &str); 4] = [
            (
                "q",
                -1,
                MessageId::default(),
                ForumTopicId::default(),
                10,
                "Invalid offset date specified",
            ),
            (
                "q",
                0,
                MessageId::new(1000),
                ForumTopicId::default(),
                10,
                "Invalid offset message identifier specified",
            ),
            (
                "q",
                0,
                MessageId::default(),
                ForumTopicId::new(-5),
                10,
                "Invalid forum topic identifier specified",
            ),
            (
                "q",
                0,
                MessageId::default(),
                ForumTopicId::default(),
                0,
                "Invalid limit specified",
            ),
        ];
        for (query, offset_date, offset_message_id, offset_topic, limit, message) in cases {
            let (promise, slot) = record::<td_api::ForumTopics>();
            manager.get_forum_topics(
                &mut env,
                FORUM,
                query.to_owned(),
                offset_date,
                offset_message_id,
                offset_topic,
                limit,
                promise,
            );
            assert!(
                matches!(&*slot.lock().unwrap(), Some(tdutils::status::Result::Err(e)) if e.message() == message),
                "the {message} band"
            );
        }
    }

    #[test]
    fn wire_topic_intake_updates_the_registry_and_pin() {
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv::default();
        let id = manager.on_get_forum_topic_impl(&mut env, FORUM, wire_topic(5, 1234, true));
        assert_eq!(id, ForumTopicId::new(5));
        let topic = manager.get_topic(FORUM, ForumTopicId::new(5)).unwrap();
        assert!(topic.has_topic);
        assert_eq!(topic.receive_date, 1000);
        assert!(
            manager
                .pinned_topic_ids
                .get(&FORUM)
                .is_some_and(|p| p == &vec![ForumTopicId::new(5)]),
            "the wire pinned flag flips the membership"
        );
        assert_eq!(env.updates.len(), 2, "both update emissions");
        // the deleted wire form runs the delete tail (cpp:1135-1141)
        let id = manager.on_get_forum_topic_impl(
            &mut env,
            FORUM,
            telegram_api::ForumTopic::forumTopicDeleted(telegram_api::forumTopicDeleted { id: 5 }),
        );
        assert_eq!(id, ForumTopicId::default());
        assert!(manager.get_topic(FORUM, ForumTopicId::new(5)).is_none());
        assert!(
            manager
                .deleted_topic_ids
                .get(&FORUM)
                .is_some_and(|d| d.contains(&ForumTopicId::new(5)))
        );
    }

    #[test]
    fn short_topic_band_resets_for_users() {
        // cpp:1166-1169 — the short form is bot-only
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv::default();
        let mut topic = match wire_topic(6, 1234, false) {
            telegram_api::ForumTopic::forumTopic(topic) => topic,
            _ => unreachable!(),
        };
        topic.short = true;
        let id = manager.on_get_forum_topic_impl(
            &mut env,
            FORUM,
            telegram_api::ForumTopic::forumTopic(topic),
        );
        assert_eq!(id, ForumTopicId::default());
        // the add_topic ran before the band — the record exists (the C++
        // order, cpp:1150-1157) but the id answers invalid
        assert!(manager.get_topic(FORUM, ForumTopicId::new(6)).is_some());
    }

    #[test]
    fn on_get_forum_topic_bands() {
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv::default();
        // the wrong-id 500 band (cpp:876-878)
        let (promise, slot) = record::<Option<td_api::ForumTopic>>();
        manager.on_get_forum_topic(
            &mut env,
            FORUM,
            ForumTopicId::new(9),
            wire_topic(7, 1234, false),
            promise,
        );
        assert!(
            matches!(&*slot.lock().unwrap(), Some(tdutils::status::Result::Err(e)) if e.code() == 500 && e.message() == "Wrong forum topic received")
        );
        // the matching id answers the object
        let (promise, slot) = record::<Option<td_api::ForumTopic>>();
        manager.on_get_forum_topic(
            &mut env,
            FORUM,
            ForumTopicId::new(7),
            wire_topic(7, 1234, false),
            promise,
        );
        let object = match slot.lock().unwrap().take().unwrap() {
            tdutils::status::Result::Ok(Some(object)) => object,
            other => panic!("the topic answer: {other:?}"),
        };
        assert_eq!(object.info.unwrap().forum_topic_id, 7);
    }

    #[test]
    fn on_get_forum_topics_walks_and_offsets() {
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv::default();
        let (promise, slot) = record::<td_api::ForumTopics>();
        manager.on_get_forum_topics(
            &mut env,
            FORUM,
            false,
            42,
            vec![wire_topic(5, 1000, true), wire_topic(6, 2000, false)],
            promise,
        );
        let topics = match slot.lock().unwrap().take().unwrap() {
            tdutils::status::Result::Ok(topics) => topics,
            other => panic!("the topics answer: {other:?}"),
        };
        assert_eq!(topics.total_count, 42);
        assert_eq!(topics.topics.len(), 2);
        // the collapsed null-last-message band: the last topic's creation
        // date, a zero message id, the last topic id (cpp:944-951)
        assert_eq!(topics.next_offset_date, 2000);
        assert_eq!(topics.next_offset_message_id, 0);
        assert_eq!(topics.next_offset_forum_topic_id, 6);
    }

    #[test]
    fn on_get_forum_topic_infos_resolves_the_dialog_off_the_peer() {
        // the null-dialog form (cpp:1106-1123): each topic's dialog comes
        // from its wire peer
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv::default();
        manager.on_get_forum_topic_infos(
            &mut env,
            DialogId::default(),
            vec![wire_topic(5, 1234, false)],
            "test",
        );
        assert!(manager.get_topic(FORUM, ForumTopicId::new(5)).is_some());
        // the fixed-dialog form keeps the request dialog (a mismatching
        // peer resets the info, ForumTopicInfo.cpp:38-40 — the invalid id
        // skips)
        manager.on_get_forum_topic_infos(
            &mut env,
            DialogId::new(9),
            vec![wire_topic(6, 1234, false)],
            "test",
        );
        assert!(
            manager
                .get_topic(DialogId::new(9), ForumTopicId::new(6))
                .is_none()
        );
    }
}
