//! `td::ForumTopicManager` (ForumTopicManager.h/.cpp, M.49).
//!
//! The topic registry (the per-dialog topic maps, the create/edit/get/
//! toggle/pin/delete flows with the C++ local validation, the
//! topic-intake machine) is real; the [blocked: net] query tails and the
//! notification-settings/draft halves ride the env seams.

use crate::dialog_id::{DialogId, DialogType};
use crate::emoji_status::CustomEmojiId;
use crate::forum_topic::{ForumTopicEditedData, ForumTopicId, ForumTopicInfo};
use crate::message_id::MessageId;
use crate::store_parse::{self, Parser};
use crate::td_api;
use std::collections::HashMap;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The `G()`/`td_` surface the forum plane reads through. The
/// notification-settings/draft/database halves and the [blocked: net]
/// query tails keep their defaults until their tracks land. The query
/// seams keep the C++-faithful argument lists (the convention's
/// too_many_arguments note).
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
    fn get_channel_first_username(&self, dialog_id: DialogId) -> String {
        let _ = dialog_id;
        String::new()
    }
    fn get_default_send_message_as_dialog_id(&self, dialog_id: DialogId) -> DialogId {
        dialog_id
    }
    fn get_message_order(&mut self, dialog_id: DialogId, message_id: MessageId) -> i64 {
        let _ = (dialog_id, message_id);
        0
    }
    fn send_update(&mut self, update: td_api::Update) {
        tdutils::log_info!("send_update({update:?}) — seam default");
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

    // -- the [blocked: net] query seams; the promise parks in the manager --
    fn send_create_forum_topic_query(
        &mut self,
        _dialog_id: DialogId,
        _title: &str,
        _title_missing: bool,
        _icon_color: i32,
        _icon_custom_emoji_id: CustomEmojiId,
        _as_dialog_id: DialogId,
        _promise: Promise<Option<td_api::ForumTopicInfo>>,
    ) {
        tdutils::log_info!("Send channels.createForumTopic — [blocked: net]");
    }
    fn send_get_forum_topic_query(
        &mut self,
        _dialog_id: DialogId,
        _forum_topic_id: ForumTopicId,
        _promise: Promise<Option<td_api::ForumTopic>>,
    ) {
        tdutils::log_info!("Send channels.getForumTopicsByID — [blocked: net]");
    }
    fn send_get_forum_topics_query(
        &mut self,
        _dialog_id: DialogId,
        _query: &str,
        _offset_date: i32,
        _offset_message_id: MessageId,
        _offset_forum_topic_id: ForumTopicId,
        _limit: i32,
        _promise: Promise<td_api::ForumTopics>,
    ) {
        tdutils::log_info!("Send channels.getForumTopics — [blocked: net]");
    }
    fn send_edit_forum_topic_query(
        &mut self,
        _dialog_id: DialogId,
        _forum_topic_id: ForumTopicId,
        _edited_data: &ForumTopicEditedData,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send channels.editForumTopic — [blocked: net]");
    }
    fn send_toggle_general_topic_hidden_query(
        &mut self,
        _dialog_id: DialogId,
        _is_hidden: bool,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send channels.toggleForum — [blocked: net]");
    }
    fn send_reorder_pinned_topics_query(&mut self, _dialog_id: DialogId, _promise: Promise<()>) {
        tdutils::log_info!("Send channels.reorderPinnedForumTopics — [blocked: net]");
    }
    fn send_delete_forum_topic_query(
        &mut self,
        _dialog_id: DialogId,
        _forum_topic_id: ForumTopicId,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send channels.deleteTopicHistory — [blocked: net]");
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

    fn get_topic_mut(
        &mut self,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
    ) -> Option<&mut Topic> {
        self.topics
            .get_mut(&dialog_id)
            .and_then(|topics| topics.get_mut(&forum_topic_id))
    }

    fn add_topic(&mut self, dialog_id: DialogId, forum_topic_id: ForumTopicId) -> &mut Topic {
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
            })
    }

    /// `create_forum_topic` (ForumTopicManager.cpp:515-547).
    pub fn create_forum_topic(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        title: &str,
        title_missing: bool,
        icon: Option<td_api::ForumTopicIcon>,
        mut promise: Promise<Option<td_api::ForumTopicInfo>>,
    ) {
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
        env.send_create_forum_topic_query(
            dialog_id,
            &new_title,
            title_missing,
            icon_color,
            icon_custom_emoji_id,
            as_dialog_id,
            promise,
        );
    }

    /// `on_forum_topic_created` (ForumTopicManager.cpp:548-564).
    pub fn on_forum_topic_created(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        forum_topic_info: ForumTopicInfo,
        mut promise: Promise<Option<td_api::ForumTopicInfo>>,
    ) {
        let status = env.close_status();
        if status.is_error() {
            promise.set_error(status);
            return;
        }
        let forum_topic_id = forum_topic_info.get_forum_topic_id();
        let topic = self.add_topic(dialog_id, forum_topic_id);
        if topic.info.is_none() {
            topic.info = Some(forum_topic_info);
        }
        topic.need_save_to_database = true;
        let info = topic.info.as_ref().expect("topic info").clone();
        let object = info.get_forum_topic_info_object();
        self.send_update_forum_topic_info(env, &info);
        promise.set_value(Some(object));
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
        let new_title = crate::misc::clean_name(title, MAX_FORUM_TOPIC_TITLE_LENGTH);
        if new_title.is_empty() {
            promise.set_error(Status::error(400, "Name must be non-empty"));
            return;
        }
        let edited_data = ForumTopicEditedData::get_edited_data(
            false,
            false,
            icon_custom_emoji_id,
            edit_icon_custom_emoji,
            Some(&new_title),
        );
        env.send_edit_forum_topic_query(dialog_id, forum_topic_id, &edited_data, promise);
    }

    /// `get_forum_topic` (ForumTopicManager.cpp:829-847) — the bot cache
    /// answers when the topic was received within the last 60 seconds;
    /// otherwise the [blocked: net] query.
    pub fn get_forum_topic(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
        mut promise: Promise<Option<td_api::ForumTopic>>,
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
        env.send_get_forum_topic_query(dialog_id, forum_topic_id, promise);
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
        env.send_get_forum_topics_query(
            dialog_id,
            &query,
            offset_date,
            offset_message_id,
            offset_forum_topic_id,
            limit,
            promise,
        );
    }

    /// `toggle_forum_topic_is_closed` (ForumTopicManager.cpp:957-971).
    pub fn toggle_forum_topic_is_closed(
        &mut self,
        env: &mut impl ForumTopicEnv,
        dialog_id: DialogId,
        forum_topic_id: ForumTopicId,
        is_closed: bool,
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
        let edited_data = ForumTopicEditedData::get_edited_data(
            is_closed,
            false,
            CustomEmojiId::default(),
            false,
            None,
        );
        env.send_edit_forum_topic_query(dialog_id, forum_topic_id, &edited_data, promise);
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
        env.send_toggle_general_topic_hidden_query(dialog_id, is_hidden, promise);
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
        env.send_reorder_pinned_topics_query(dialog_id, promise);
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
        self.pinned_topic_ids.insert(dialog_id, forum_topic_ids);
        env.send_reorder_pinned_topics_query(dialog_id, promise);
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
        self.deleted_topic_ids
            .entry(dialog_id)
            .or_default()
            .push(forum_topic_id);
        env.send_delete_forum_topic_query(dialog_id, forum_topic_id, promise);
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
            self.send_update_forum_topic_info(env, &info);
        }
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

    /// `send_update_forum_topic_info` (ForumTopicManager.cpp:1369-1380) —
    /// the local scheme's updateForumTopicInfo carries only the info.
    fn send_update_forum_topic_info(
        &mut self,
        env: &mut impl ForumTopicEnv,
        topic_info: &ForumTopicInfo,
    ) {
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

    /// The minimal test env.
    struct TestEnv {
        bot: bool,
        unix_time: i32,
        updates: Vec<td_api::Update>,
        queries: Vec<String>,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                bot: false,
                unix_time: 1000,
                updates: Vec::new(),
                queries: Vec::new(),
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

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn send_create_forum_topic_query(
            &mut self,
            dialog_id: DialogId,
            _title: &str,
            _title_missing: bool,
            _icon_color: i32,
            _icon_custom_emoji_id: CustomEmojiId,
            _as_dialog_id: DialogId,
            mut promise: Promise<Option<td_api::ForumTopicInfo>>,
        ) {
            self.queries.push(format!("create {}", dialog_id.get()));
            promise.set_value(None);
        }

        fn send_get_forum_topic_query(
            &mut self,
            _dialog_id: DialogId,
            _forum_topic_id: ForumTopicId,
            mut promise: Promise<Option<td_api::ForumTopic>>,
        ) {
            self.queries.push("get".to_owned());
            promise.set_value(None);
        }
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
    fn create_topic_validates_the_name() {
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv::default();
        let promise = Promise::from_interface(Absorbing);
        manager.create_forum_topic(&mut env, DialogId::new(10), "", false, None, promise);
        assert!(env.queries.is_empty(), "the empty name must be rejected");
    }

    #[test]
    fn topic_created_fills_the_registry_and_sends_updates() {
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv::default();
        let info = ForumTopicInfo::new(
            DialogId::new(10),
            ForumTopicId::new(3),
            "First".to_owned(),
            crate::forum_topic::ForumTopicIcon::default_icon(),
            100,
            DialogId::new(11),
            true,
            false,
            false,
            false,
        );
        let promise = Promise::from_interface(Absorbing);
        manager.on_forum_topic_created(&mut env, DialogId::new(10), info, promise);
        assert_eq!(env.updates.len(), 1);
        assert!(
            manager
                .get_topic(DialogId::new(10), ForumTopicId::new(3))
                .is_some()
        );
    }

    #[test]
    fn topic_edited_applies_and_reports() {
        let mut manager = ForumTopicManager::new();
        let mut env = TestEnv::default();
        let info = ForumTopicInfo::new(
            DialogId::new(10),
            ForumTopicId::new(3),
            "Old".to_owned(),
            crate::forum_topic::ForumTopicIcon::default_icon(),
            100,
            DialogId::new(11),
            false,
            false,
            false,
            false,
        );
        let promise = Promise::from_interface(Absorbing);
        manager.on_forum_topic_created(&mut env, DialogId::new(10), info, promise);
        let edited = ForumTopicEditedData::get_edited_data(
            true,
            false,
            CustomEmojiId::default(),
            false,
            Some("New"),
        );
        manager.on_forum_topic_edited(&mut env, DialogId::new(10), ForumTopicId::new(3), &edited);
        assert_eq!(env.updates.len(), 2);
    }
}
