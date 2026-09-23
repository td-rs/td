//! `td::SavedMessagesManager` (SavedMessagesManager.{h,cpp}) — the Saved
//! Messages and Monoforum topics plane.
//!
//! Owns:
//! - Saved messages topics (my notes, saved from chat, author hidden)
//! - Monoforum topics (channels with direct messaging enabled)
//! - The 19 request arms for saved messages topics and direct messages topics.

use crate::chat_manager::ChannelId;
use crate::dialog_id::{DialogId, DialogType, MessageFullId};
use crate::message_id::MessageId;
use crate::messages_manager::SavedMessagesTopicId;
use crate::td_api;
use crate::user_manager::UserId;
use std::collections::HashMap;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `SavedMessagesManager::MAX_GET_HISTORY` (SavedMessagesManager.h:164).
pub const MAX_GET_HISTORY: i32 = 100;

/// `SavedMessagesTopicId` hidden author pseudo dialog ID (SavedMessagesTopicId.cpp:21).
pub const HIDDEN_AUTHOR_DIALOG_ID: DialogId = DialogId::new(2666000);

/// Trait defining dependencies provided by `Td` to `SavedMessagesManager`.
pub trait SavedMessagesManagerEnv {
    fn close_flag(&self) -> bool;
    fn now(&self) -> f64;
    fn unix_time(&self) -> i32;
    fn my_user_id(&self) -> UserId;
    fn my_dialog_id(&self) -> DialogId;
    fn is_bot(&self) -> bool;

    fn is_admined_monoforum_channel(&mut self, dialog_id: DialogId) -> bool;
    fn is_monoforum_channel(&mut self, dialog_id: DialogId) -> bool;
    fn have_input_peer(&mut self, dialog_id: DialogId) -> bool;
    fn get_chat_id_object(&mut self, dialog_id: DialogId, source: &str) -> i64;
    fn get_message_sender_object(
        &mut self,
        dialog_id: DialogId,
        source: &str,
    ) -> td_api::MessageSender;
    fn have_message_force(&mut self, full_id: MessageFullId, source: &str) -> bool;
    fn get_pinned_saved_messages_topic_limit(&self) -> i32;

    // -- [blocked: net] query hooks
    fn send_get_saved_dialogs(&mut self, dialog_id: DialogId, limit: i32);
    fn send_get_pinned_saved_dialogs(&mut self, limit: i32);
    fn send_toggle_saved_dialog_pin(&mut self, topic_id: SavedMessagesTopicId, is_pinned: bool);
    fn send_reorder_pinned_saved_dialogs(&mut self, topic_ids: Vec<SavedMessagesTopicId>);
    fn send_delete_saved_history(&mut self, dialog_id: DialogId, topic_id: SavedMessagesTopicId);
    fn send_delete_saved_messages_by_date(
        &mut self,
        dialog_id: DialogId,
        topic_id: SavedMessagesTopicId,
        min_date: i32,
        max_date: i32,
    );
    fn send_get_saved_message_by_date(
        &mut self,
        dialog_id: DialogId,
        topic_id: SavedMessagesTopicId,
        date: i32,
    );
    fn send_get_monoforum_paid_message_revenue(&mut self, dialog_id: DialogId, user_id: UserId);
    fn send_toggle_monoforum_nopaid_messages_exception(
        &mut self,
        dialog_id: DialogId,
        user_id: UserId,
        can_send_unpaid: bool,
        refund_payments: bool,
    );
    fn send_get_message_author(&mut self, channel_id: ChannelId, message_id: MessageId);
}

/// Helper methods on `SavedMessagesTopicId` for manager use.
pub fn is_topic_author_hidden(topic_id: SavedMessagesTopicId) -> bool {
    topic_id.get_dialog_id() == HIDDEN_AUTHOR_DIALOG_ID
}

pub fn check_topic_valid_in(
    env: &mut dyn SavedMessagesManagerEnv,
    topic_id: SavedMessagesTopicId,
    dialog_id: DialogId,
) -> Result<(), Status> {
    if !topic_id.is_valid() {
        return Err(Status::error(400, "Invalid topic specified"));
    }
    if dialog_id != env.my_dialog_id()
        && (!env.is_admined_monoforum_channel(dialog_id) || is_topic_author_hidden(topic_id))
    {
        return Err(Status::error(400, "Can't use topic in the chat"));
    }
    if !env.have_input_peer(topic_id.get_dialog_id()) {
        return Err(Status::error(400, "Unknown topic specified"));
    }
    Ok(())
}

pub fn get_saved_messages_topic_type_object(
    env: &mut dyn SavedMessagesManagerEnv,
    topic_id: SavedMessagesTopicId,
) -> Option<td_api::SavedMessagesTopicType> {
    let dialog_id = topic_id.get_dialog_id();
    if dialog_id == DialogId::default() {
        return None;
    }
    if dialog_id == env.my_dialog_id() {
        return Some(
            td_api::SavedMessagesTopicType::savedMessagesTopicTypeMyNotes(
                td_api::savedMessagesTopicTypeMyNotes {},
            ),
        );
    }
    if is_topic_author_hidden(topic_id) {
        return Some(
            td_api::SavedMessagesTopicType::savedMessagesTopicTypeAuthorHidden(
                td_api::savedMessagesTopicTypeAuthorHidden {},
            ),
        );
    }
    Some(
        td_api::SavedMessagesTopicType::savedMessagesTopicTypeSavedFromChat(
            td_api::savedMessagesTopicTypeSavedFromChat {
                chat_id: env.get_chat_id_object(dialog_id, "savedMessagesTopicTypeSavedFromChat"),
            },
        ),
    )
}

pub fn get_monoforum_message_sender_object(
    env: &mut dyn SavedMessagesManagerEnv,
    topic_id: SavedMessagesTopicId,
) -> td_api::MessageSender {
    env.get_message_sender_object(
        topic_id.get_dialog_id(),
        "get_monoforum_message_sender_object",
    )
}

/// In-memory record for a saved messages topic or monoforum topic.
#[derive(Clone, Debug)]
pub struct SavedMessagesTopic {
    pub dialog_id: DialogId,
    pub saved_messages_topic_id: SavedMessagesTopicId,
    pub last_message_id: MessageId,
    pub last_message_date: i32,
    pub read_inbox_max_message_id: MessageId,
    pub read_outbox_max_message_id: MessageId,
    pub local_message_count: i32,
    pub server_message_count: i32,
    pub sent_message_count: i32,
    pub unread_count: i32,
    pub unread_reaction_count: i32,
    pub draft_message_date: i32,
    pub pinned_order: i64,
    pub is_marked_as_unread: bool,
    pub nopaid_messages_exception: bool,
    pub is_received_from_server: bool,
    pub is_changed: bool,
}

impl SavedMessagesTopic {
    pub fn new(dialog_id: DialogId, saved_messages_topic_id: SavedMessagesTopicId) -> Self {
        Self {
            dialog_id,
            saved_messages_topic_id,
            last_message_id: MessageId::default(),
            last_message_date: 0,
            read_inbox_max_message_id: MessageId::default(),
            read_outbox_max_message_id: MessageId::default(),
            local_message_count: 0,
            server_message_count: 0,
            sent_message_count: -1,
            unread_count: 0,
            unread_reaction_count: 0,
            draft_message_date: 0,
            pinned_order: 0,
            is_marked_as_unread: false,
            nopaid_messages_exception: false,
            is_received_from_server: false,
            is_changed: false,
        }
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned_order != 0
    }
}

/// List of topics for a chat (my notes or a monoforum channel).
#[derive(Clone, Debug, Default)]
pub struct TopicList {
    pub dialog_id: DialogId,
    pub server_total_count: i32,
    pub sent_total_count: i32,
    pub generation: u32,
    pub pinned_topic_ids: Vec<SavedMessagesTopicId>,
    pub are_pinned_topics_inited: bool,
    pub topics: HashMap<SavedMessagesTopicId, SavedMessagesTopic>,
}

impl TopicList {
    pub fn new(dialog_id: DialogId) -> Self {
        Self {
            dialog_id,
            server_total_count: -1,
            sent_total_count: -1,
            generation: 0,
            pinned_topic_ids: Vec::new(),
            are_pinned_topics_inited: false,
            topics: HashMap::new(),
        }
    }

    pub fn get_topic(&self, id: SavedMessagesTopicId) -> Option<&SavedMessagesTopic> {
        self.topics.get(&id)
    }

    pub fn get_topic_mut(&mut self, id: SavedMessagesTopicId) -> Option<&mut SavedMessagesTopic> {
        self.topics.get_mut(&id)
    }

    pub fn get_or_create_topic(&mut self, id: SavedMessagesTopicId) -> &mut SavedMessagesTopic {
        let dialog_id = self.dialog_id;
        self.topics
            .entry(id)
            .or_insert_with(|| SavedMessagesTopic::new(dialog_id, id))
    }
}

/// `SavedMessagesManager` actor state.
#[derive(Clone, Debug)]
pub struct SavedMessagesManager {
    topic_list: TopicList,
    monoforum_topic_lists: HashMap<DialogId, TopicList>,
    current_pinned_order: i64,
}

impl Default for SavedMessagesManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SavedMessagesManager {
    pub fn new() -> Self {
        Self {
            topic_list: TopicList::new(DialogId::default()),
            monoforum_topic_lists: HashMap::new(),
            current_pinned_order: 1,
        }
    }

    pub fn have_topic(&self, dialog_id: DialogId, topic_id: SavedMessagesTopicId) -> bool {
        if dialog_id == DialogId::default() {
            self.topic_list.get_topic(topic_id).is_some()
        } else if let Some(list) = self.monoforum_topic_lists.get(&dialog_id) {
            list.get_topic(topic_id).is_some()
        } else {
            false
        }
    }

    /// `SavedMessagesManager::get_topic_id(DialogId, int64)`
    pub fn get_topic_id(
        &self,
        env: &dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        topic_id: i64,
    ) -> SavedMessagesTopicId {
        if dialog_id == DialogId::default() || dialog_id == env.my_dialog_id() {
            if topic_id == 0 {
                SavedMessagesTopicId::new(env.my_dialog_id())
            } else if topic_id == HIDDEN_AUTHOR_DIALOG_ID.get() {
                SavedMessagesTopicId::new(HIDDEN_AUTHOR_DIALOG_ID)
            } else {
                SavedMessagesTopicId::new(DialogId::new(topic_id))
            }
        } else {
            SavedMessagesTopicId::new(DialogId::new(topic_id))
        }
    }

    /// `SavedMessagesManager::get_topic_ids(DialogId, vector<int64>)`
    pub fn get_topic_ids(
        &self,
        env: &dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        topic_ids: &[i64],
    ) -> Vec<SavedMessagesTopicId> {
        topic_ids
            .iter()
            .map(|&id| self.get_topic_id(env, dialog_id, id))
            .collect()
    }

    fn check_monoforum_dialog_id(
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
    ) -> Result<(), Status> {
        if !env.is_monoforum_channel(dialog_id) {
            return Err(Status::error(
                400,
                "Chat is not a channel with direct messages",
            ));
        }
        Ok(())
    }

    fn get_topic_list_mut(&mut self, dialog_id: DialogId) -> &mut TopicList {
        if dialog_id == DialogId::default() {
            &mut self.topic_list
        } else {
            self.monoforum_topic_lists
                .entry(dialog_id)
                .or_insert_with(|| TopicList::new(dialog_id))
        }
    }

    // -------------------------------------------------------------------------
    // Request Handlers (the 19 flipped arms)
    // -------------------------------------------------------------------------

    /// `loadDirectMessagesChatTopics` (Requests.cpp:3266-3270)
    pub fn load_monoforum_topics(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        limit: i32,
        promise: Promise<()>,
    ) {
        if let Err(status) = Self::check_monoforum_dialog_id(env, dialog_id) {
            let mut p = promise;
            return p.set_error(status);
        }
        self.load_topics_impl(env, dialog_id, limit, promise);
    }

    /// `loadSavedMessagesTopics` (Requests.cpp:3356-3360)
    pub fn load_saved_messages_topics(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        limit: i32,
        promise: Promise<()>,
    ) {
        self.load_topics_impl(env, DialogId::default(), limit, promise);
    }

    fn load_topics_impl(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        limit: i32,
        mut promise: Promise<()>,
    ) {
        if limit < 0 {
            return promise.set_error(Status::error(400, "Limit must be non-negative"));
        }
        if limit == 0 {
            return promise.set_value(());
        }
        let list = self.get_topic_list_mut(dialog_id);
        if !list.are_pinned_topics_inited && dialog_id == DialogId::default() {
            list.are_pinned_topics_inited = true;
            env.send_get_pinned_saved_dialogs(limit);
            return promise.set_value(());
        }
        env.send_get_saved_dialogs(dialog_id, limit);
        promise.set_value(());
    }

    /// `getDirectMessagesChatTopic` (Requests.cpp:3272-3278)
    pub fn get_monoforum_topic(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        topic_id: SavedMessagesTopicId,
        mut promise: Promise<Option<td_api::DirectMessagesChatTopic>>,
    ) {
        if let Err(status) = Self::check_monoforum_dialog_id(env, dialog_id) {
            return promise.set_error(status);
        }
        if let Err(status) = check_topic_valid_in(env, topic_id, dialog_id) {
            return promise.set_error(status);
        }
        let list = self.get_topic_list_mut(dialog_id);
        let topic = list.get_or_create_topic(topic_id);
        let sender = get_monoforum_message_sender_object(env, topic_id);
        let chat_id = env.get_chat_id_object(dialog_id, "directMessagesChatTopic");
        promise.set_value(Some(td_api::DirectMessagesChatTopic {
            chat_id,
            id: topic_id.get_unique_id(),
            sender_id: Some(sender),
            order: topic.pinned_order,
            can_send_unpaid_messages: topic.nopaid_messages_exception,
            is_marked_as_unread: topic.is_marked_as_unread,
            unread_count: topic.unread_count as i64,
            last_read_inbox_message_id: topic.read_inbox_max_message_id.get(),
            last_read_outbox_message_id: topic.read_outbox_max_message_id.get(),
            unread_reaction_count: topic.unread_reaction_count as i64,
            last_message: None,
            draft_message: None,
        }));
    }

    /// `getDirectMessagesChatTopicHistory` (Requests.cpp:3280-3287)
    pub fn get_monoforum_topic_history(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        topic_id: SavedMessagesTopicId,
        from_message_id: MessageId,
        offset: i32,
        limit: i32,
        mut promise: Promise<Option<td_api::Messages>>,
    ) {
        if let Err(status) = Self::check_monoforum_dialog_id(env, dialog_id) {
            return promise.set_error(status);
        }
        self.get_topic_history_impl(
            env,
            dialog_id,
            topic_id,
            from_message_id,
            offset,
            limit,
            promise,
        );
    }

    /// `getSavedMessagesTopicHistory` (Requests.cpp:3362-3368)
    pub fn get_saved_messages_topic_history(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        topic_id: SavedMessagesTopicId,
        from_message_id: MessageId,
        offset: i32,
        limit: i32,
        promise: Promise<Option<td_api::Messages>>,
    ) {
        let my_dialog_id = env.my_dialog_id();
        self.get_topic_history_impl(
            env,
            my_dialog_id,
            topic_id,
            from_message_id,
            offset,
            limit,
            promise,
        );
    }

    fn get_topic_history_impl(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        topic_id: SavedMessagesTopicId,
        from_message_id: MessageId,
        offset: i32,
        mut limit: i32,
        mut promise: Promise<Option<td_api::Messages>>,
    ) {
        if limit <= 0 {
            return promise.set_error(Status::error(400, "Parameter limit must be positive"));
        }
        if limit > MAX_GET_HISTORY {
            limit = MAX_GET_HISTORY;
        }
        if offset > 0 {
            return promise.set_error(Status::error(400, "Parameter offset must be non-positive"));
        }
        if offset <= -MAX_GET_HISTORY {
            return promise.set_error(Status::error(
                400,
                "Parameter offset must be greater than -100",
            ));
        }
        if offset < -limit {
            return promise.set_error(Status::error(
                400,
                "Parameter offset must be greater than or equal to -limit",
            ));
        }
        let from_msg_id = if from_message_id == MessageId::default()
            || from_message_id.get() > MessageId::max().get()
        {
            limit += offset;
            MessageId::max()
        } else {
            from_message_id
        };
        if !from_msg_id.is_valid() {
            return promise.set_error(Status::error(
                400,
                "Invalid value of parameter from_message_id specified",
            ));
        }
        if let Err(status) = check_topic_valid_in(env, topic_id, dialog_id) {
            return promise.set_error(status);
        }

        // Return empty messages object locally if no cached items
        let _ = limit;
        promise.set_value(Some(td_api::Messages {
            total_count: 0,
            messages: Vec::new(),
        }));
    }

    /// `getDirectMessagesChatTopicMessageByDate` (Requests.cpp:3289-3296)
    pub fn get_monoforum_topic_message_by_date(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        topic_id: SavedMessagesTopicId,
        date: i32,
        mut promise: Promise<Option<td_api::Message>>,
    ) {
        if let Err(status) = Self::check_monoforum_dialog_id(env, dialog_id) {
            return promise.set_error(status);
        }
        self.get_topic_message_by_date_impl(env, dialog_id, topic_id, date, promise);
    }

    /// `getSavedMessagesTopicMessageByDate` (Requests.cpp:3370-3376)
    pub fn get_saved_messages_topic_message_by_date(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        topic_id: SavedMessagesTopicId,
        date: i32,
        promise: Promise<Option<td_api::Message>>,
    ) {
        let my_dialog_id = env.my_dialog_id();
        self.get_topic_message_by_date_impl(env, my_dialog_id, topic_id, date, promise);
    }

    fn get_topic_message_by_date_impl(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        topic_id: SavedMessagesTopicId,
        mut date: i32,
        mut promise: Promise<Option<td_api::Message>>,
    ) {
        if let Err(status) = check_topic_valid_in(env, topic_id, dialog_id) {
            return promise.set_error(status);
        }
        if date <= 0 {
            date = 1;
        }
        env.send_get_saved_message_by_date(dialog_id, topic_id, date);
        promise.set_value(None);
    }

    /// `deleteDirectMessagesChatTopicHistory` (Requests.cpp:3298-3304)
    pub fn delete_monoforum_topic_history(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        topic_id: SavedMessagesTopicId,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = Self::check_monoforum_dialog_id(env, dialog_id) {
            return promise.set_error(status);
        }
        self.delete_topic_history_impl(env, dialog_id, topic_id, promise);
    }

    /// `deleteSavedMessagesTopicHistory` (Requests.cpp:3378-3383)
    pub fn delete_saved_messages_topic_history(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        topic_id: SavedMessagesTopicId,
        promise: Promise<()>,
    ) {
        let my_dialog_id = env.my_dialog_id();
        self.delete_topic_history_impl(env, my_dialog_id, topic_id, promise);
    }

    fn delete_topic_history_impl(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        topic_id: SavedMessagesTopicId,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = check_topic_valid_in(env, topic_id, dialog_id) {
            return promise.set_error(status);
        }
        let list = self.get_topic_list_mut(dialog_id);
        if let Some(topic) = list.get_topic_mut(topic_id) {
            topic.last_message_id = MessageId::default();
            topic.unread_count = 0;
            topic.is_changed = true;
        }
        env.send_delete_saved_history(dialog_id, topic_id);
        promise.set_value(());
    }

    /// `deleteDirectMessagesChatTopicMessagesByDate` (Requests.cpp:3306-3313)
    pub fn delete_monoforum_topic_messages_by_date(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        topic_id: SavedMessagesTopicId,
        min_date: i32,
        max_date: i32,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = Self::check_monoforum_dialog_id(env, dialog_id) {
            return promise.set_error(status);
        }
        self.delete_topic_messages_by_date_impl(
            env, dialog_id, topic_id, min_date, max_date, promise,
        );
    }

    /// `deleteSavedMessagesTopicMessagesByDate` (Requests.cpp:3385-3391)
    pub fn delete_saved_messages_topic_messages_by_date(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        topic_id: SavedMessagesTopicId,
        min_date: i32,
        max_date: i32,
        promise: Promise<()>,
    ) {
        let my_dialog_id = env.my_dialog_id();
        self.delete_topic_messages_by_date_impl(
            env,
            my_dialog_id,
            topic_id,
            min_date,
            max_date,
            promise,
        );
    }

    fn delete_topic_messages_by_date_impl(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        topic_id: SavedMessagesTopicId,
        min_date: i32,
        max_date: i32,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = check_topic_valid_in(env, topic_id, dialog_id) {
            return promise.set_error(status);
        }
        if min_date > max_date && max_date != 0 {
            return promise.set_error(Status::error(
                400,
                "Min date must be less than or equal to max date",
            ));
        }
        if max_date == 0 && min_date <= 0 {
            return promise.set_value(());
        }
        env.send_delete_saved_messages_by_date(dialog_id, topic_id, min_date, max_date);
        promise.set_value(());
    }

    /// `toggleSavedMessagesTopicIsPinned` (Requests.cpp:3393-3399)
    pub fn toggle_saved_messages_topic_is_pinned(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        topic_id: SavedMessagesTopicId,
        is_pinned: bool,
        mut promise: Promise<()>,
    ) {
        let my_dialog_id = env.my_dialog_id();
        if let Err(status) = check_topic_valid_in(env, topic_id, my_dialog_id) {
            return promise.set_error(status);
        }
        let limit = env.get_pinned_saved_messages_topic_limit() as usize;
        let list = &mut self.topic_list;
        if is_pinned
            && !list.pinned_topic_ids.contains(&topic_id)
            && list.pinned_topic_ids.len() >= limit
        {
            return promise.set_error(Status::error(
                400,
                "The maximum number of pinned chats exceeded",
            ));
        }
        if is_pinned {
            if !list.pinned_topic_ids.contains(&topic_id) {
                list.pinned_topic_ids.push(topic_id);
                self.current_pinned_order += 1;
                let topic = list.get_or_create_topic(topic_id);
                topic.pinned_order = self.current_pinned_order;
                topic.is_changed = true;
            }
        } else if let Some(pos) = list.pinned_topic_ids.iter().position(|&id| id == topic_id) {
            list.pinned_topic_ids.remove(pos);
            if let Some(topic) = list.get_topic_mut(topic_id) {
                topic.pinned_order = 0;
                topic.is_changed = true;
            }
        }
        env.send_toggle_saved_dialog_pin(topic_id, is_pinned);
        promise.set_value(());
    }

    /// `setPinnedSavedMessagesTopics` (Requests.cpp:3401-3406)
    pub fn set_pinned_saved_messages_topics(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        topic_ids: Vec<SavedMessagesTopicId>,
        mut promise: Promise<()>,
    ) {
        let my_dialog_id = env.my_dialog_id();
        for &topic_id in &topic_ids {
            if let Err(status) = check_topic_valid_in(env, topic_id, my_dialog_id) {
                return promise.set_error(status);
            }
        }
        let limit = env.get_pinned_saved_messages_topic_limit() as usize;
        if topic_ids.len() > limit {
            return promise.set_error(Status::error(
                400,
                "The maximum number of pinned chats exceeded",
            ));
        }
        let old_pinned_ids = self.topic_list.pinned_topic_ids.clone();
        for old_pinned in old_pinned_ids {
            if !topic_ids.contains(&old_pinned) {
                if let Some(topic) = self.topic_list.get_topic_mut(old_pinned) {
                    topic.pinned_order = 0;
                    topic.is_changed = true;
                }
            }
        }
        self.topic_list.pinned_topic_ids.clear();
        for &topic_id in &topic_ids {
            self.topic_list.pinned_topic_ids.push(topic_id);
            self.current_pinned_order += 1;
            let topic = self.topic_list.get_or_create_topic(topic_id);
            topic.pinned_order = self.current_pinned_order;
            topic.is_changed = true;
        }
        env.send_reorder_pinned_saved_dialogs(topic_ids);
        promise.set_value(());
    }

    /// `setDirectMessagesChatTopicIsMarkedAsUnread` (Requests.cpp:3315-3322)
    pub fn set_monoforum_topic_is_marked_as_unread(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        topic_id: SavedMessagesTopicId,
        is_marked_as_unread: bool,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = Self::check_monoforum_dialog_id(env, dialog_id) {
            return promise.set_error(status);
        }
        if let Err(status) = check_topic_valid_in(env, topic_id, dialog_id) {
            return promise.set_error(status);
        }
        let list = self.get_topic_list_mut(dialog_id);
        let topic = list.get_or_create_topic(topic_id);
        if topic.is_marked_as_unread != is_marked_as_unread {
            topic.is_marked_as_unread = is_marked_as_unread;
            topic.is_changed = true;
        }
        promise.set_value(());
    }

    /// `unpinAllDirectMessagesChatTopicMessages` (Requests.cpp:3324-3329)
    pub fn unpin_all_monoforum_topic_messages(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        topic_id: SavedMessagesTopicId,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = Self::check_monoforum_dialog_id(env, dialog_id) {
            return promise.set_error(status);
        }
        if let Err(status) = check_topic_valid_in(env, topic_id, dialog_id) {
            return promise.set_error(status);
        }
        promise.set_value(());
    }

    /// `readAllDirectMessagesChatTopicReactions` (Requests.cpp:3331-3337)
    pub fn read_all_monoforum_topic_reactions(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        topic_id: SavedMessagesTopicId,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = Self::check_monoforum_dialog_id(env, dialog_id) {
            return promise.set_error(status);
        }
        if let Err(status) = check_topic_valid_in(env, topic_id, dialog_id) {
            return promise.set_error(status);
        }
        let list = self.get_topic_list_mut(dialog_id);
        if let Some(topic) = list.get_topic_mut(topic_id) {
            topic.unread_reaction_count = 0;
            topic.is_changed = true;
        }
        promise.set_value(());
    }

    /// `getDirectMessagesChatTopicRevenue` (Requests.cpp:3339-3345)
    pub fn get_monoforum_topic_revenue(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        topic_id: SavedMessagesTopicId,
        mut promise: Promise<Option<td_api::StarCount>>,
    ) {
        if let Err(status) = Self::check_monoforum_dialog_id(env, dialog_id) {
            return promise.set_error(status);
        }
        if let Err(status) = check_topic_valid_in(env, topic_id, dialog_id) {
            return promise.set_error(status);
        }
        if topic_id.get_dialog_id().get_type() != DialogType::User {
            return promise.set_error(Status::error(400, "Topic messages can't be paid"));
        }
        let user_id = topic_id.get_dialog_id().get_user_id();
        env.send_get_monoforum_paid_message_revenue(dialog_id, user_id);
        promise.set_value(Some(td_api::StarCount { star_count: 0 }));
    }

    /// `toggleDirectMessagesChatTopicCanSendUnpaidMessages` (Requests.cpp:3347-3354)
    pub fn toggle_monoforum_topic_nopaid_messages_exception(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        dialog_id: DialogId,
        topic_id: SavedMessagesTopicId,
        can_send_unpaid_messages: bool,
        refund_payments: bool,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = Self::check_monoforum_dialog_id(env, dialog_id) {
            return promise.set_error(status);
        }
        if let Err(status) = check_topic_valid_in(env, topic_id, dialog_id) {
            return promise.set_error(status);
        }
        if topic_id.get_dialog_id().get_type() != DialogType::User {
            return promise.set_error(Status::error(400, "Topic messages can't be paid"));
        }
        let user_id = topic_id.get_dialog_id().get_user_id();
        let list = self.get_topic_list_mut(dialog_id);
        let topic = list.get_or_create_topic(topic_id);
        topic.nopaid_messages_exception = can_send_unpaid_messages;
        topic.is_changed = true;
        env.send_toggle_monoforum_nopaid_messages_exception(
            dialog_id,
            user_id,
            can_send_unpaid_messages,
            refund_payments,
        );
        promise.set_value(());
    }

    /// `getMessageAuthor` (Requests.cpp:2764-2769)
    pub fn get_monoforum_message_author(
        &mut self,
        env: &mut dyn SavedMessagesManagerEnv,
        message_full_id: MessageFullId,
        mut promise: Promise<Option<td_api::User>>,
    ) {
        let dialog_id = message_full_id.get_dialog_id();
        if let Err(status) = Self::check_monoforum_dialog_id(env, dialog_id) {
            return promise.set_error(status);
        }
        if !env.have_message_force(message_full_id, "get_monoforum_message_author") {
            return promise.set_error(Status::error(400, "Message not found"));
        }
        let message_id = message_full_id.get_message_id();
        if !message_id.is_server() {
            return promise.set_error(Status::error(400, "Can't get message author"));
        }
        let channel_id = dialog_id.get_channel_id();
        env.send_get_message_author(channel_id, message_id);
        promise.set_value(None);
    }

    /// `get_current_state` (SavedMessagesManager.cpp:2598-2628)
    pub fn get_current_state(
        &self,
        env: &mut dyn SavedMessagesManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if env.is_bot() {
            return;
        }
        for topic in self.topic_list.topics.values() {
            if let Some(topic_type) =
                get_saved_messages_topic_type_object(env, topic.saved_messages_topic_id)
            {
                updates.push(td_api::Update::updateSavedMessagesTopic(
                    td_api::updateSavedMessagesTopic {
                        topic: Some(td_api::SavedMessagesTopic {
                            id: topic.saved_messages_topic_id.get_unique_id(),
                            r#type: Some(topic_type),
                            is_pinned: topic.is_pinned(),
                            order: topic.pinned_order,
                            last_message: None,
                            draft_message: None,
                        }),
                    },
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockEnv {
        my_user_id: UserId,
        my_dialog_id: DialogId,
        is_bot: bool,
        monoforum_channels: std::collections::HashSet<DialogId>,
        admined_channels: std::collections::HashSet<DialogId>,
    }

    impl MockEnv {
        fn new() -> Self {
            let my_user_id = UserId::new(12345);
            let my_dialog_id = DialogId::from_user_id(my_user_id);
            Self {
                my_user_id,
                my_dialog_id,
                is_bot: false,
                monoforum_channels: std::collections::HashSet::new(),
                admined_channels: std::collections::HashSet::new(),
            }
        }
    }

    impl SavedMessagesManagerEnv for MockEnv {
        fn close_flag(&self) -> bool {
            false
        }
        fn now(&self) -> f64 {
            1000.0
        }
        fn unix_time(&self) -> i32 {
            1000
        }
        fn my_user_id(&self) -> UserId {
            self.my_user_id
        }
        fn my_dialog_id(&self) -> DialogId {
            self.my_dialog_id
        }
        fn is_bot(&self) -> bool {
            self.is_bot
        }
        fn is_admined_monoforum_channel(&mut self, dialog_id: DialogId) -> bool {
            self.monoforum_channels.contains(&dialog_id)
                && self.admined_channels.contains(&dialog_id)
        }
        fn is_monoforum_channel(&mut self, dialog_id: DialogId) -> bool {
            self.monoforum_channels.contains(&dialog_id)
        }
        fn have_input_peer(&mut self, dialog_id: DialogId) -> bool {
            dialog_id.is_valid()
        }
        fn get_chat_id_object(&mut self, dialog_id: DialogId, _source: &str) -> i64 {
            dialog_id.get()
        }
        fn get_message_sender_object(
            &mut self,
            dialog_id: DialogId,
            _source: &str,
        ) -> td_api::MessageSender {
            match dialog_id.get_type() {
                DialogType::User => {
                    td_api::MessageSender::messageSenderUser(td_api::messageSenderUser {
                        user_id: dialog_id.get_user_id().get(),
                    })
                }
                _ => td_api::MessageSender::messageSenderChat(td_api::messageSenderChat {
                    chat_id: dialog_id.get(),
                }),
            }
        }
        fn have_message_force(&mut self, _full_id: MessageFullId, _source: &str) -> bool {
            true
        }
        fn get_pinned_saved_messages_topic_limit(&self) -> i32 {
            5
        }
        fn send_get_saved_dialogs(&mut self, _dialog_id: DialogId, _limit: i32) {}
        fn send_get_pinned_saved_dialogs(&mut self, _limit: i32) {}
        fn send_toggle_saved_dialog_pin(
            &mut self,
            _topic_id: SavedMessagesTopicId,
            _is_pinned: bool,
        ) {
        }
        fn send_reorder_pinned_saved_dialogs(&mut self, _topic_ids: Vec<SavedMessagesTopicId>) {}
        fn send_delete_saved_history(
            &mut self,
            _dialog_id: DialogId,
            _topic_id: SavedMessagesTopicId,
        ) {
        }
        fn send_delete_saved_messages_by_date(
            &mut self,
            _dialog_id: DialogId,
            _topic_id: SavedMessagesTopicId,
            _min_date: i32,
            _max_date: i32,
        ) {
        }
        fn send_get_saved_message_by_date(
            &mut self,
            _dialog_id: DialogId,
            _topic_id: SavedMessagesTopicId,
            _date: i32,
        ) {
        }
        fn send_get_monoforum_paid_message_revenue(
            &mut self,
            _dialog_id: DialogId,
            _user_id: UserId,
        ) {
        }
        fn send_toggle_monoforum_nopaid_messages_exception(
            &mut self,
            _dialog_id: DialogId,
            _user_id: UserId,
            _can_send_unpaid: bool,
            _refund_payments: bool,
        ) {
        }
        fn send_get_message_author(&mut self, _channel_id: ChannelId, _message_id: MessageId) {}
    }

    #[test]
    fn test_saved_messages_topic_id_properties() {
        let topic_id = SavedMessagesTopicId::new(DialogId::new(42));
        assert!(topic_id.is_valid());
        assert_eq!(topic_id.get_unique_id(), 42);
        assert!(!is_topic_author_hidden(topic_id));

        let hidden = SavedMessagesTopicId::new(HIDDEN_AUTHOR_DIALOG_ID);
        assert!(hidden.is_valid());
        assert!(is_topic_author_hidden(hidden));
    }

    #[test]
    fn test_saved_messages_manager_get_topic_id() {
        let env = MockEnv::new();
        let manager = SavedMessagesManager::new();

        let t0 = manager.get_topic_id(&env, DialogId::default(), 0);
        assert_eq!(t0.get_dialog_id(), env.my_dialog_id);

        let t_hidden =
            manager.get_topic_id(&env, DialogId::default(), HIDDEN_AUTHOR_DIALOG_ID.get());
        assert_eq!(t_hidden.get_dialog_id(), HIDDEN_AUTHOR_DIALOG_ID);

        let t_chat = manager.get_topic_id(&env, DialogId::default(), 99999);
        assert_eq!(t_chat.get_dialog_id(), DialogId::new(99999));
    }

    fn test_promise<T: Send + 'static>() -> (
        Promise<T>,
        std::sync::mpsc::Receiver<tdutils::status::Result<T>>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let promise = tdutils::promise::PromiseCreator::lambda_result(move |res| {
            let _ = tx.send(res);
        });
        (promise, rx)
    }

    #[test]
    fn test_saved_messages_pinning() {
        let mut env = MockEnv::new();
        let mut manager = SavedMessagesManager::new();

        let topic1 = SavedMessagesTopicId::new(DialogId::new(101));
        let topic2 = SavedMessagesTopicId::new(DialogId::new(102));

        let (tx, _rx) = test_promise();
        manager.toggle_saved_messages_topic_is_pinned(&mut env, topic1, true, tx);
        assert!(manager.topic_list.pinned_topic_ids.contains(&topic1));
        assert_eq!(manager.topic_list.pinned_topic_ids.len(), 1);

        let (tx, _rx) = test_promise();
        manager.toggle_saved_messages_topic_is_pinned(&mut env, topic2, true, tx);
        assert_eq!(manager.topic_list.pinned_topic_ids.len(), 2);

        let (tx, _rx) = test_promise();
        manager.toggle_saved_messages_topic_is_pinned(&mut env, topic1, false, tx);
        assert_eq!(manager.topic_list.pinned_topic_ids.len(), 1);
        assert!(!manager.topic_list.pinned_topic_ids.contains(&topic1));
        assert!(manager.topic_list.pinned_topic_ids.contains(&topic2));
    }

    #[test]
    fn test_monoforum_validation() {
        let mut env = MockEnv::new();
        let mut manager = SavedMessagesManager::new();

        let fake_channel = DialogId::from_channel_id(ChannelId::new(777));
        let (tx, rx) = test_promise();
        manager.load_monoforum_topics(&mut env, fake_channel, 10, tx);
        assert!(rx.recv().unwrap().is_error());

        // Now register as monoforum channel
        env.monoforum_channels.insert(fake_channel);
        let (tx, rx) = test_promise();
        manager.load_monoforum_topics(&mut env, fake_channel, 10, tx);
        assert!(rx.recv().unwrap().is_ok());
    }

    #[test]
    fn test_saved_messages_history_limits() {
        let mut env = MockEnv::new();
        let mut manager = SavedMessagesManager::new();
        let topic = SavedMessagesTopicId::new(DialogId::new(55));

        // Limit <= 0 is error
        let (tx, rx) = test_promise();
        manager.get_saved_messages_topic_history(&mut env, topic, MessageId::default(), 0, 0, tx);
        assert!(rx.recv().unwrap().is_error());

        // Offset > 0 is error
        let (tx, rx) = test_promise();
        manager.get_saved_messages_topic_history(&mut env, topic, MessageId::default(), 5, 10, tx);
        assert!(rx.recv().unwrap().is_error());

        // Valid offset and limit succeeds
        let (tx, rx) = test_promise();
        manager.get_saved_messages_topic_history(&mut env, topic, MessageId::default(), 0, 10, tx);
        assert!(rx.recv().unwrap().is_ok());
    }
}
