//! Port of `td/telegram/MessageQueryManager.{h,cpp}` (M.18) — the planes
//! that need no network and no message plane, sliced per the Wave-2 plan:
//! the value/record planes (`MessageThreadInfo.h`, `MessageViewer.{h,cpp}`,
//! `AffectedHistory.h`, `EmojiGameInfo.{h,cpp}`, `SearchPostsFlood.{h,cpp}`,
//! `MessageSearchOffset.{h,cpp}`), the byte-exact binlog logevent
//! store/parse family (MessageQueryManager.cpp:4079-5142) and, in the
//! follow-up slice, the manager's local machinery (the pending-read
//! counters, the reactions-reload queue, the view-metrics buffer, the
//! affected-history loop and the emoji-game cache).
//!
//! Heavily M.10-coupled surfaces (threads/forwards/statistics over
//! MessagesManager) carry the `[blocked: M.10 integration]` seam tag; the
//! network query tails carry `[blocked: net]` — the collapsed-actor
//! convention: `Td` owns the struct, the [`MessageQueryManagerEnv`] trait
//! carries the `G()`/option/cross-manager surfaces and the promise lands
//! unanswered offline.

use crate::chat_manager::{ChannelId, MessageId};
use crate::dialog_id::{DialogId, MessageFullId, ServerMessageId};
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::UserId;
use std::collections::{HashMap, HashSet};
use tdmtproto::telegram_api;
use tdutils::promise::Promise;

/// `MessageQueryManager::MAX_SEARCH_MESSAGES` (MessageQueryManager.h:351) —
/// the server-side search limit the request arms clamp to; its consumers
/// (the search arms) land with the M.18.4 wiring.
#[allow(dead_code)]
pub(crate) const MAX_SEARCH_MESSAGES: i32 = 100;

/// The `G()`/manager slice the collapsed MessageQueryManager reads through.
pub trait MessageQueryManagerEnv {
    // -- the always-required surfaces
    /// `td_->auth_manager_->is_bot()`.
    fn auth_is_bot(&self) -> bool;
    /// `G()->close_flag()`.
    fn close_flag(&self) -> bool;
    /// `G()->unix_time()`.
    fn unix_time(&self) -> i32;
    /// `Time::now()` — the process-monotonic clock.
    fn now(&self) -> f64;
    /// `td_->option_manager_->get_option_string(name, default)`.
    fn get_option_string(&self, name: &str, default_value: &str) -> String;
    /// `user_manager_->get_user_id_object(user_id, source)` — the i53
    /// surface the viewer builders read.
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64;
    /// `user_manager_->have_user_force(user_id, source)` — the
    /// participant-list gate of `on_get_message_viewers`.
    fn have_user_force(&mut self, user_id: UserId, source: &str) -> bool;
    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    // -- the view-metrics / read-metrics plane
    /// `Random::secure_int64` — the wire `view_id` draw.
    fn secure_random_i64(&self) -> i64;
    /// `dialog_manager_->check_dialog_access` — the M.6 dialog-plane gate;
    /// the collapsed line answers the not-found band (the full access
    /// check is the M.6/M.10 integration).
    fn check_dialog_access(
        &self,
        dialog_id: DialogId,
        _require_valid: bool,
        _access_rights: crate::user_manager::AccessRights,
        _source: &str,
    ) -> Result<(), tdutils::status::Status> {
        let _ = dialog_id;
        Err(tdutils::status::Status::error(400, "Chat not found"))
    }
    /// `dialog_manager_->have_input_peer` — the M.6 stub answers false
    /// (the full InputPeer family is the dialog-plane integration).
    fn have_input_peer(
        &self,
        _dialog_id: DialogId,
        _from_group_chat: bool,
        _access_rights: crate::user_manager::AccessRights,
    ) -> bool {
        false
    }
    /// `messages_manager_->have_message_force` — the
    /// [blocked: M.10 integration] probe.
    fn have_message_force(&mut self, _message_full_id: MessageFullId, _source: &str) -> bool {
        false
    }
    /// `messages_manager_->can_get_message_viewers` — the
    /// [blocked: M.10 integration] gate; answers the not-found band.
    fn can_get_message_viewers(
        &self,
        _message_full_id: MessageFullId,
    ) -> Result<(), tdutils::status::Status> {
        Err(tdutils::status::Status::error(400, "Message not found"))
    }
    /// `messages_manager_->get_messages_from_server` — the
    /// [blocked: M.10 integration] refetch.
    fn get_messages_from_server(&mut self, _message_full_ids: Vec<MessageFullId>) {}

    // -- the pts bookkeeping of the affected-history loop
    /// `updates_manager_->add_pending_pts_update` with the `dummyUpdate`.
    fn add_pending_pts_update(
        &mut self,
        _pts: i32,
        _pts_count: i32,
        _receive_time: f64,
        _promise: Promise<()>,
        _source: &str,
    ) {
    }
    /// `messages_manager_->add_pending_channel_update` — the
    /// [blocked: M.10 integration] channel-pts path.
    fn add_pending_channel_update(
        &mut self,
        _dialog_id: DialogId,
        _pts: i32,
        _pts_count: i32,
        _promise: Promise<()>,
        _source: &str,
    ) {
    }

    // -- the file-source plane
    /// `file_reference_manager_->create_rich_message_file_source` — the
    /// M.12 file-source registry stand-in.
    fn create_rich_message_file_source(
        &mut self,
        _message_full_id: MessageFullId,
    ) -> crate::files::FileSourceId {
        crate::files::FileSourceId::default()
    }

    // -- the paid-message-reaction sender load (the M.16 audit arm)
    /// `chat_manager_->load_created_public_broadcasts` + `get_me` — the
    /// [blocked: net] load half; the promise lands unanswered offline.
    fn load_paid_message_reaction_sender_state(
        &mut self,
        promise: Promise<td_api::MessageSenders>,
    ) {
        let _ = promise;
    }

    // -- the `[blocked: net]` query tails; each one owns the promise,
    // -- which keeps the C++ request unanswered offline
    /// The affected-history refill (the Delete*/Read*/Unpin* queries).
    fn send_affected_history_query(
        &mut self,
        _dialog_id: DialogId,
        _query: AffectedHistoryQueryKind,
        _get_affected_messages: bool,
        _promise: Promise<()>,
    ) {
    }
    /// `GetExtendedMediaQuery` (MessageQueryManager.cpp:360-394).
    fn send_get_extended_media_query(
        &mut self,
        _dialog_id: DialogId,
        _message_ids: Vec<MessageId>,
    ) {
    }
    /// `GetFactCheckQuery` (MessageQueryManager.cpp:396-430).
    fn send_get_fact_check_query(&mut self, _dialog_id: DialogId, _message_ids: Vec<MessageId>) {}
    /// `ReportReadMetricsQuery` (MessageQueryManager.cpp:1028-1049).
    fn send_report_read_metrics_query(
        &mut self,
        _dialog_id: DialogId,
        _metrics: Vec<MessageReadMetric>,
    ) {
    }
    /// `GetMessageReadParticipantsQuery` (MessageQueryManager.cpp:1096-1127).
    fn send_get_message_read_participants_query(
        &mut self,
        _message_full_id: MessageFullId,
        _promise: Promise<td_api::MessageViewers>,
    ) {
    }
    /// `GetMessagesViewsQuery` (MessageQueryManager.cpp:1129-1175).
    fn send_get_messages_views_query(
        &mut self,
        _dialog_id: DialogId,
        _message_ids: Vec<MessageId>,
        _increment_view_counter: bool,
    ) {
    }
    /// `GetMessagesReactionsQuery` (MessageQueryManager.cpp:1177-1229).
    fn send_get_messages_reactions_query(
        &mut self,
        _dialog_id: DialogId,
        _message_ids: Vec<MessageId>,
    ) {
    }
    /// `ReadMessagesContentsQuery`/`ReadChannelMessagesContentsQuery`
    /// (MessageQueryManager.cpp:2210-2289).
    fn send_read_message_contents_query(
        &mut self,
        _dialog_id: DialogId,
        _message_ids: Vec<MessageId>,
    ) {
    }
    /// `GetEmojiGameInfoQuery` (MessageQueryManager.cpp:1388-1414).
    fn send_get_emoji_game_info_query(&mut self, _promise: Promise<td_api::StakeDiceState>) {}
}

/// `MessageThreadInfo` (MessageThreadInfo.h) — the discussion-thread answer
/// `get_discussion_message` resolves to; the wire intake itself
/// (`process_discussion_message`) is the `[blocked: M.10 integration]`
/// half, the record is what survives the seam.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageThreadInfo {
    pub dialog_id: DialogId,
    pub message_ids: Vec<MessageId>,
    pub unread_message_count: i32,
}

/// `MessageViewer` (MessageViewer.h:20-41).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageViewer {
    user_id: UserId,
    date: i32,
}

impl MessageViewer {
    /// `MessageViewer(UserId, int32)` (MessageViewer.h:29-31) — the date is
    /// clamped to non-negative.
    pub fn new(user_id: UserId, date: i32) -> MessageViewer {
        MessageViewer {
            user_id,
            date: date.max(0),
        }
    }

    /// `MessageViewer(telegram_api::readParticipantDate)`
    /// (MessageViewer.cpp:16-18).
    pub fn from_read_participant_date(
        read_date: &telegram_api::ReadParticipantDate,
    ) -> MessageViewer {
        MessageViewer::new(UserId::new(read_date.user_id), read_date.date)
    }

    /// `get_user_id` (MessageViewer.h:32-34).
    pub fn get_user_id(&self) -> UserId {
        self.user_id
    }

    /// `is_empty` (MessageViewer.h:36-38).
    pub fn is_empty(&self) -> bool {
        self.user_id == UserId::default() && self.date == 0
    }

    /// `get_message_viewer_object` (MessageViewer.cpp:20-23).
    pub fn get_message_viewer_object(
        &self,
        env: &mut impl MessageQueryManagerEnv,
    ) -> td_api::MessageViewer {
        td_api::MessageViewer {
            user_id: env.get_user_id_object(self.user_id, "get_message_viewer_object"),
            view_date: self.date,
        }
    }
}

impl std::fmt::Display for MessageViewer {
    /// `operator<<(StringBuilder, MessageViewer)` (MessageViewer.cpp:25-27).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{} at {}]", self.user_id.get(), self.date)
    }
}

/// `MessageViewers` (MessageViewer.h:45-58).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageViewers {
    message_viewers: Vec<MessageViewer>,
}

impl MessageViewers {
    /// `MessageViewers(vector<readParticipantDate>)` (MessageViewer.cpp:29-38)
    /// — invalid user identifiers are dropped with the C++ `LOG(ERROR)`.
    pub fn from_read_participant_dates(
        read_dates: Vec<telegram_api::ReadParticipantDate>,
    ) -> MessageViewers {
        let mut message_viewers = Vec::with_capacity(read_dates.len());
        for read_date in read_dates {
            let viewer = MessageViewer::from_read_participant_date(&read_date);
            if !viewer.get_user_id().is_valid() {
                tracing::error!(
                    "Receive invalid {} as a viewer of a message",
                    viewer.get_user_id().get()
                );
                continue;
            }
            message_viewers.push(viewer);
        }
        MessageViewers { message_viewers }
    }

    /// `get_user_ids` (MessageViewer.cpp:40-42).
    pub fn get_user_ids(&self) -> Vec<UserId> {
        self.message_viewers
            .iter()
            .map(|viewer| viewer.get_user_id())
            .collect()
    }

    /// `get_message_viewers_object` (MessageViewer.cpp:44-49).
    pub fn get_message_viewers_object(
        &self,
        env: &mut impl MessageQueryManagerEnv,
    ) -> td_api::MessageViewers {
        td_api::MessageViewers {
            viewers: self
                .message_viewers
                .iter()
                .map(|viewer| viewer.get_message_viewer_object(env))
                .collect(),
        }
    }
}

impl std::fmt::Display for MessageViewers {
    /// `operator<<(StringBuilder, MessageViewers)` (MessageViewer.cpp:51-53).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let joined: Vec<String> = self
            .message_viewers
            .iter()
            .map(|viewer| viewer.to_string())
            .collect();
        write!(f, "[{}]", joined.join(", "))
    }
}

/// `AffectedHistory` (AffectedHistory.h) — the pts bookkeeping answer of
/// every history-changing query; `is_final_` is the `offset_ <= 0` fold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AffectedHistory {
    pub pts: i32,
    pub pts_count: i32,
    pub is_final: bool,
}

impl AffectedHistory {
    /// `AffectedHistory(messages_affectedHistory)` (AffectedHistory.h:20-24).
    pub fn from_affected_history(
        affected_history: &telegram_api::messages_AffectedHistory,
    ) -> AffectedHistory {
        AffectedHistory {
            pts: affected_history.pts,
            pts_count: affected_history.pts_count,
            is_final: affected_history.offset <= 0,
        }
    }

    /// `AffectedHistory(messages_affectedFoundMessages)`
    /// (AffectedHistory.h:26-30).
    pub fn from_affected_found_messages(
        affected_history: &telegram_api::messages_AffectedFoundMessages,
    ) -> AffectedHistory {
        AffectedHistory {
            pts: affected_history.pts,
            pts_count: affected_history.pts_count,
            is_final: affected_history.offset <= 0,
        }
    }
}

/// `TonAmount::get_ton_count` (TonAmount.cpp:23-41) — the ±2⁵¹ nanogram
/// clamp; the StarManager twin (`StarManager::get_star_count`,
/// StarManager.cpp) is byte-identical, so both share this helper until the
/// owner managers land.
pub(crate) fn get_amount_count(amount: i64, allow_negative: bool) -> i64 {
    let max_amount = 1i64 << 51;
    if amount < 0 {
        if !allow_negative {
            tracing::error!("Receive amount = {amount}");
            return 0;
        }
        if amount < -max_amount {
            tracing::error!("Receive amount = {amount}");
            return -max_amount;
        }
    } else if amount > max_amount {
        tracing::error!("Receive amount = {amount}");
        return max_amount;
    }
    amount
}

/// `EmojiGameInfo` (EmojiGameInfo.h:18-34) — the dice-game cache entry; the
/// wire intake normalizes `messages_emojiGameDiceInfo` and rejects bad
/// shapes by falling back to the unavailable default.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EmojiGameInfo {
    game_hash: String,
    prev_stake: i64,
    current_streak: i32,
    params: Vec<i32>,
}

impl EmojiGameInfo {
    /// `EmojiGameInfo(messages_EmojiGameInfo)` (EmojiGameInfo.cpp:15-45).
    pub fn from_wire(game_info: &telegram_api::messages_EmojiGameInfo) -> EmojiGameInfo {
        let telegram_api::messages_EmojiGameInfo::messages_emojiGameDiceInfo(info) = game_info
        else {
            // `messages_emojiGameUnavailable` — the default record
            return EmojiGameInfo::default();
        };
        let mut is_bad = info.game_hash.is_empty()
            || info.prev_stake < 0
            || info.current_streak < 0
            || info.current_streak >= 3
            || info.params.len() != 7;
        for param in &info.params {
            if *param < 0 {
                is_bad = true;
            }
        }
        if is_bad {
            tracing::error!("Receive bad emojiGameDiceInfo");
            return EmojiGameInfo::default();
        }
        EmojiGameInfo {
            game_hash: info.game_hash.clone(),
            prev_stake: get_amount_count(info.prev_stake, false),
            current_streak: info.current_streak,
            params: info.params.clone(),
        }
    }

    /// `get_stake_dice_state_object` (EmojiGameInfo.cpp:47-61) — the
    /// suggested stake amounts come from the
    /// `ton_stakedice_stake_suggested_amounts` option; the C++
    /// `to_integer<int64>` CHECKs the parse, so does the port.
    pub fn get_stake_dice_state_object(
        &self,
        env: &impl MessageQueryManagerEnv,
    ) -> td_api::StakeDiceState {
        if self.game_hash.is_empty() {
            return td_api::StakeDiceState::default();
        }
        assert_eq!(self.params.len(), 7);
        let suggested_amounts: Vec<i64> = env
            .get_option_string("ton_stakedice_stake_suggested_amounts", "")
            .split(',')
            .map(|str| {
                str.parse::<i64>()
                    .expect("Failed to parse the suggested stake amount")
            })
            .collect();
        td_api::StakeDiceState {
            state_hash: self.game_hash.clone(),
            stake_gram_amount: self.prev_stake,
            suggested_stake_gram_amounts: suggested_amounts,
            current_streak: self.current_streak,
            prize_per_mille: self.params[..6].to_vec(),
            streak_prize_per_mille: self.params[6],
        }
    }

    /// `get_update_stake_dice_state_object` (EmojiGameInfo.cpp:63-65).
    pub fn get_update_stake_dice_state_object(
        &self,
        env: &impl MessageQueryManagerEnv,
    ) -> td_api::Update {
        td_api::Update::updateStakeDiceState(td_api::updateStakeDiceState {
            state: Some(self.get_stake_dice_state_object(env)),
        })
    }
}

/// `SearchPostsFlood` (SearchPostsFlood.h:17-36) — the public-post search
/// flood counters the search arms answer alongside the results.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchPostsFlood {
    total_daily: i32,
    remains: i32,
    star_count: i64,
    wait_till: i32,
    is_free: bool,
}

impl SearchPostsFlood {
    /// `SearchPostsFlood(searchPostsFlood)` (SearchPostsFlood.cpp:14-20) —
    /// the star count rides the ±2⁵¹ clamp.
    pub fn from_wire(flood: &telegram_api::SearchPostsFlood) -> SearchPostsFlood {
        SearchPostsFlood {
            total_daily: flood.total_daily,
            remains: flood.remains,
            star_count: get_amount_count(flood.stars_amount, false),
            wait_till: flood.wait_till,
            is_free: flood.query_is_free,
        }
    }

    /// `is_free` (SearchPostsFlood.h:33-35).
    pub fn is_free(&self) -> bool {
        self.is_free
    }

    /// `get_public_post_search_limits_object` (SearchPostsFlood.cpp:22-25).
    pub fn get_public_post_search_limits_object(
        &self,
        env: &impl MessageQueryManagerEnv,
    ) -> td_api::PublicPostSearchLimits {
        td_api::PublicPostSearchLimits {
            daily_free_query_count: self.total_daily,
            remaining_free_query_count: self.remains,
            next_free_query_in: (self.wait_till - env.unix_time()).max(0),
            star_count: self.star_count,
            is_current_query_free: self.is_free,
        }
    }
}

/// `MessagesManager::get_message_date` (MessagesManager.cpp) — the pure
/// wire date accessor the search offset needs; module-local until the
/// message plane lands.
fn get_message_date(message: &telegram_api::Message) -> i32 {
    match message {
        telegram_api::Message::messageEmpty(_) => 0,
        telegram_api::Message::message(message) => message.date.max(1),
        telegram_api::Message::messageService(message) => message.date.max(1),
    }
}

/// `MessageSearchOffset` (MessageSearchOffset.h:18-28) — the
/// `date,dialog_id,server_message_id` pagination token of the public-post
/// search arms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageSearchOffset {
    date: i32,
    message_id: MessageId,
    dialog_id: DialogId,
}

impl MessageSearchOffset {
    /// `update_from_message` (MessageSearchOffset.cpp:21-30) — keeps the
    /// first usable message position.
    pub fn update_from_message(&mut self, message: &telegram_api::Message) {
        let message_date = get_message_date(message);
        let message_id = MessageId::from_telegram_message(message, false);
        let dialog_id = DialogId::get_message_dialog_id(message);
        if message_date > 0 && message_id.is_valid() && dialog_id.is_valid() {
            self.date = message_date;
            self.message_id = message_id;
            self.dialog_id = dialog_id;
        }
    }

    /// `to_string` (MessageSearchOffset.cpp:32-34) — the wire form.
    pub fn to_offset_string(&self) -> String {
        format!(
            "{},{},{}",
            self.date,
            self.dialog_id.get(),
            self.message_id.get_server_message_id().get()
        )
    }

    /// `from_string` (MessageSearchOffset.cpp:36-67) — the empty offset is
    /// the "no bound" form (`date = i32::MAX`, the ids left default); the
    /// invalid forms answer 400 "Invalid offset specified".
    pub fn from_offset_string(
        offset: &str,
    ) -> Result<MessageSearchOffset, tdutils::status::Status> {
        let mut result = MessageSearchOffset {
            date: i32::MAX,
            message_id: MessageId::default(),
            dialog_id: DialogId::default(),
        };
        let is_offset_valid = 'offset: {
            if offset.is_empty() {
                break 'offset true;
            }
            let parts: Vec<&str> = offset.split(',').collect();
            if parts.len() != 3 {
                break 'offset false;
            }
            let (Some(offset_date), Some(offset_dialog_id), Some(offset_message_id)) = (
                parts.first().and_then(|part| part.parse::<i32>().ok()),
                parts.get(1).and_then(|part| part.parse::<i64>().ok()),
                parts.get(2).and_then(|part| part.parse::<i32>().ok()),
            ) else {
                break 'offset false;
            };
            result.date = offset_date;
            result.message_id =
                MessageId::from_server_message_id(ServerMessageId::new(offset_message_id));
            result.dialog_id = DialogId::new(offset_dialog_id);
            if result.date <= 0
                || !result.message_id.is_valid()
                || !result.dialog_id.is_valid()
                || matches!(
                    crate::dialog_manager::get_input_peer_force(result.dialog_id),
                    telegram_api::InputPeer::inputPeerEmpty(_)
                )
            {
                break 'offset false;
            }
            true
        };
        if !is_offset_valid {
            return Err(tdutils::status::Status::error(
                400,
                "Invalid offset specified",
            ));
        }
        Ok(result)
    }
}

/// `LogEvent::HandlerType` — the M.18-owned binlog handler entries
/// (LogEvent.h). The Rust binlog plane keys logevents by name; the numeric
/// C++ identities are kept in the docs for the eventual database step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageQueryLogEventType {
    /// `BlockMessageSenderFromRepliesOnServer`.
    BlockMessageSenderFromRepliesOnServer,
    /// `DeleteAllCallMessagesOnServer`.
    DeleteAllCallMessagesOnServer,
    /// `DeleteAllChannelMessagesFromSenderOnServer`.
    DeleteAllChannelMessagesFromSenderOnServer,
    /// `DeleteDialogHistoryOnServer`.
    DeleteDialogHistoryOnServer,
    /// `DeleteDialogMessagesByDateOnServer`.
    DeleteDialogMessagesByDateOnServer,
    /// `DeleteMessagesOnServer`.
    DeleteMessagesOnServer,
    /// `DeleteScheduledMessagesOnServer`.
    DeleteScheduledMessagesOnServer,
    /// `DeleteEphemeralMessageOnServer`.
    DeleteEphemeralMessageOnServer,
    /// `DeleteTopicHistoryOnServer`.
    DeleteTopicHistoryOnServer,
    /// `ReadAllDialogMentionsOnServer`.
    ReadAllDialogMentionsOnServer,
    /// `ReadAllDialogReactionsOnServer`.
    ReadAllDialogReactionsOnServer,
    /// `ReadAllPollVotesOnServer`.
    ReadAllPollVotesOnServer,
    /// `ReadMessageContentsOnServer`.
    ReadMessageContentsOnServer,
    /// `UnpinAllDialogMessagesOnServer`.
    UnpinAllDialogMessagesOnServer,
}

impl std::fmt::Display for MessageQueryLogEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            MessageQueryLogEventType::BlockMessageSenderFromRepliesOnServer => {
                "BlockMessageSenderFromRepliesOnServer"
            }
            MessageQueryLogEventType::DeleteAllCallMessagesOnServer => {
                "DeleteAllCallMessagesOnServer"
            }
            MessageQueryLogEventType::DeleteAllChannelMessagesFromSenderOnServer => {
                "DeleteAllChannelMessagesFromSenderOnServer"
            }
            MessageQueryLogEventType::DeleteDialogHistoryOnServer => "DeleteDialogHistoryOnServer",
            MessageQueryLogEventType::DeleteDialogMessagesByDateOnServer => {
                "DeleteDialogMessagesByDateOnServer"
            }
            MessageQueryLogEventType::DeleteMessagesOnServer => "DeleteMessagesOnServer",
            MessageQueryLogEventType::DeleteScheduledMessagesOnServer => {
                "DeleteScheduledMessagesOnServer"
            }
            MessageQueryLogEventType::DeleteEphemeralMessageOnServer => {
                "DeleteEphemeralMessageOnServer"
            }
            MessageQueryLogEventType::DeleteTopicHistoryOnServer => "DeleteTopicHistoryOnServer",
            MessageQueryLogEventType::ReadAllDialogMentionsOnServer => {
                "ReadAllDialogMentionsOnServer"
            }
            MessageQueryLogEventType::ReadAllDialogReactionsOnServer => {
                "ReadAllDialogReactionsOnServer"
            }
            MessageQueryLogEventType::ReadAllPollVotesOnServer => "ReadAllPollVotesOnServer",
            MessageQueryLogEventType::ReadMessageContentsOnServer => "ReadMessageContentsOnServer",
            MessageQueryLogEventType::UnpinAllDialogMessagesOnServer => {
                "UnpinAllDialogMessagesOnServer"
            }
        };
        write!(f, "{name}")
    }
}

/// The per-type structs below are the byte-exact `store`/`parse` ports of
/// the `MessageQueryManager::*OnServerLogEvent` family
/// (MessageQueryManager.cpp:4079-5142). Every block pins the C++ field
/// order and the flag-block layout, including the deliberately empty
/// `DeleteTopicHistory` flags word.
///
/// `BlockMessageSenderFromRepliesOnServerLogEvent` (cpp:4079-4107).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockMessageSenderFromRepliesOnServerLogEvent {
    pub message_id: MessageId,
    pub delete_message: bool,
    pub delete_all_messages: bool,
    pub report_spam: bool,
}

impl BlockMessageSenderFromRepliesOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(
            out,
            [
                self.delete_message,
                self.delete_all_messages,
                self.report_spam,
            ],
        );
        self.message_id.store(out);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self::default();
        let [delete_message, delete_all_messages, report_spam] = parser.parse_flags()?;
        result.delete_message = delete_message;
        result.delete_all_messages = delete_all_messages;
        result.report_spam = report_spam;
        result.message_id = MessageId::parse_from(parser)?;
        Ok(result)
    }
}

/// `DeleteAllCallMessagesOnServerLogEvent` (cpp:4303-4320).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeleteAllCallMessagesOnServerLogEvent {
    pub revoke: bool,
}

impl DeleteAllCallMessagesOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, [self.revoke]);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self::default();
        let [revoke] = parser.parse_flags()?;
        result.revoke = revoke;
        Ok(result)
    }
}

/// `DeleteAllChannelMessagesFromSenderOnServerLogEvent` (cpp:4341-4363) —
/// the sender was a bare user id before `Version::AddKeyboardButtonFlags`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeleteAllChannelMessagesFromSenderOnServerLogEvent {
    pub channel_id: ChannelId,
    pub sender_dialog_id: DialogId,
}

impl DeleteAllChannelMessagesFromSenderOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.channel_id.get());
        self.sender_dialog_id.store(out);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self {
            channel_id: ChannelId::new(parser.i64_()?),
            ..Default::default()
        };
        if parser.version() >= store_parse::version::ADD_KEYBOARD_BUTTON_FLAGS {
            result.sender_dialog_id = DialogId::parse_from(parser)?;
        } else {
            // `UserId user_id; td::parse(user_id, parser)` — the raw i64
            // form (UserId.h store/parse)
            let user_id = UserId::new(parser.i64_()?);
            result.sender_dialog_id = DialogId::from_user_id(user_id);
        }
        Ok(result)
    }
}

/// `DeleteDialogHistoryOnServerLogEvent` (cpp:4390-4418).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeleteDialogHistoryOnServerLogEvent {
    pub dialog_id: DialogId,
    pub max_message_id: MessageId,
    pub remove_from_dialog_list: bool,
    pub revoke: bool,
}

impl DeleteDialogHistoryOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, [self.remove_from_dialog_list, self.revoke]);
        self.dialog_id.store(out);
        self.max_message_id.store(out);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self::default();
        let [remove_from_dialog_list, revoke] = parser.parse_flags()?;
        result.remove_from_dialog_list = remove_from_dialog_list;
        result.revoke = revoke;
        result.dialog_id = DialogId::parse_from(parser)?;
        result.max_message_id = MessageId::parse_from(parser)?;
        Ok(result)
    }
}

/// `DeleteDialogMessagesByDateOnServerLogEvent` (cpp:4466-4492).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeleteDialogMessagesByDateOnServerLogEvent {
    pub dialog_id: DialogId,
    pub min_date: i32,
    pub max_date: i32,
    pub revoke: bool,
}

impl DeleteDialogMessagesByDateOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, [self.revoke]);
        self.dialog_id.store(out);
        store_parse::store_i32(out, self.min_date);
        store_parse::store_i32(out, self.max_date);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self::default();
        let [revoke] = parser.parse_flags()?;
        result.revoke = revoke;
        result.dialog_id = DialogId::parse_from(parser)?;
        result.min_date = parser.i32_()?;
        result.max_date = parser.i32_()?;
        Ok(result)
    }
}

/// `DeleteMessagesOnServerLogEvent` (cpp:4545-4570).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeleteMessagesOnServerLogEvent {
    pub dialog_id: DialogId,
    pub message_ids: Vec<MessageId>,
    pub revoke: bool,
}

impl DeleteMessagesOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, [self.revoke]);
        self.dialog_id.store(out);
        store_parse::store_i32(out, self.message_ids.len() as i32);
        for message_id in &self.message_ids {
            message_id.store(out);
        }
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self::default();
        let [revoke] = parser.parse_flags()?;
        result.revoke = revoke;
        result.dialog_id = DialogId::parse_from(parser)?;
        let len = parser.i32_()?;
        if len < 0 {
            return Err("Invalid vector size");
        }
        result.message_ids.reserve(len as usize);
        for _ in 0..len {
            result.message_ids.push(MessageId::parse_from(parser)?);
        }
        Ok(result)
    }
}

/// `DeleteScheduledMessagesOnServerLogEvent` (cpp:4644-4660) — no flag
/// block, unlike the non-scheduled twin.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeleteScheduledMessagesOnServerLogEvent {
    pub dialog_id: DialogId,
    pub message_ids: Vec<MessageId>,
}

impl DeleteScheduledMessagesOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        store_parse::store_i32(out, self.message_ids.len() as i32);
        for message_id in &self.message_ids {
            message_id.store(out);
        }
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self {
            dialog_id: DialogId::parse_from(parser)?,
            ..Default::default()
        };
        let len = parser.i32_()?;
        if len < 0 {
            return Err("Invalid vector size");
        }
        result.message_ids.reserve(len as usize);
        for _ in 0..len {
            result.message_ids.push(MessageId::parse_from(parser)?);
        }
        Ok(result)
    }
}

/// `DeleteEphemeralMessageOnServerLogEvent` (cpp:4686-4705).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeleteEphemeralMessageOnServerLogEvent {
    pub dialog_id: DialogId,
    pub receiver_dialog_id: DialogId,
    pub ephemeral_message_id: i32,
}

impl DeleteEphemeralMessageOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        self.receiver_dialog_id.store(out);
        store_parse::store_i32(out, self.ephemeral_message_id);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self {
            dialog_id: DialogId::parse_from(parser)?,
            ..Default::default()
        };
        result.receiver_dialog_id = DialogId::parse_from(parser)?;
        result.ephemeral_message_id = parser.i32_()?;
        Ok(result)
    }
}

/// `DeleteTopicHistoryOnServerLogEvent` (cpp:4745-4765) — the empty
/// `BEGIN_STORE_FLAGS`/`END_STORE_FLAGS` block still writes the zero
/// `uint32`; kept byte-exact.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeleteTopicHistoryOnServerLogEvent {
    pub dialog_id: DialogId,
    pub forum_topic_id: crate::files::ForumTopicId,
}

impl DeleteTopicHistoryOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, []);
        self.dialog_id.store(out);
        self.forum_topic_id.store(out);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let [] = parser.parse_flags()?;
        Ok(Self {
            dialog_id: DialogId::parse_from(parser)?,
            forum_topic_id: crate::files::ForumTopicId::parse_from(parser)?,
        })
    }
}

/// `ReadAllDialogMentionsOnServerLogEvent` (cpp:4839-4852).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadAllDialogMentionsOnServerLogEvent {
    pub dialog_id: DialogId,
}

impl ReadAllDialogMentionsOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        Ok(Self {
            dialog_id: DialogId::parse_from(parser)?,
        })
    }
}

/// `ReadAllDialogReactionsOnServerLogEvent` (cpp:4884-4897).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadAllDialogReactionsOnServerLogEvent {
    pub dialog_id: DialogId,
}

impl ReadAllDialogReactionsOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        Ok(Self {
            dialog_id: DialogId::parse_from(parser)?,
        })
    }
}

/// `ReadAllPollVotesOnServerLogEvent` (cpp:4919-4947) — the topic id is
/// stored only when valid, with the presence flag in the flag block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadAllPollVotesOnServerLogEvent {
    pub dialog_id: DialogId,
    pub forum_topic_id: crate::files::ForumTopicId,
}

impl ReadAllPollVotesOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_forum_topic_id = self.forum_topic_id.is_valid();
        store_parse::store_flags(out, [has_forum_topic_id]);
        self.dialog_id.store(out);
        if has_forum_topic_id {
            self.forum_topic_id.store(out);
        }
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let [has_forum_topic_id] = parser.parse_flags()?;
        let mut result = Self {
            dialog_id: DialogId::parse_from(parser)?,
            ..Default::default()
        };
        if has_forum_topic_id {
            result.forum_topic_id = crate::files::ForumTopicId::parse_from(parser)?;
        }
        Ok(result)
    }
}

/// `ReadMessageContentsOnServerLogEvent` (cpp:4981-4997).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadMessageContentsOnServerLogEvent {
    pub dialog_id: DialogId,
    pub message_ids: Vec<MessageId>,
}

impl ReadMessageContentsOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        store_parse::store_i32(out, self.message_ids.len() as i32);
        for message_id in &self.message_ids {
            message_id.store(out);
        }
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let mut result = Self {
            dialog_id: DialogId::parse_from(parser)?,
            ..Default::default()
        };
        let len = parser.i32_()?;
        if len < 0 {
            return Err("Invalid vector size");
        }
        result.message_ids.reserve(len as usize);
        for _ in 0..len {
            result.message_ids.push(MessageId::parse_from(parser)?);
        }
        Ok(result)
    }
}

/// `UnpinAllDialogMessagesOnServerLogEvent` (cpp:5123-5136).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnpinAllDialogMessagesOnServerLogEvent {
    pub dialog_id: DialogId,
}

impl UnpinAllDialogMessagesOnServerLogEvent {
    pub fn store(&self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        Ok(Self {
            dialog_id: DialogId::parse_from(parser)?,
        })
    }
}

/// `MessageQueryManager` (MessageQueryManager.h:58-585) — the collapsed
/// actor state. The upload-content/cover machinery and the rich-message
/// query family stay behind their seams (Files/net/M.17); the pending
/// read counters, the reactions-reload queue, the view-metrics buffer,
/// the view dedup sets, the affected-history loop and the emoji-game
/// cache are the local machine the manager exists for.
#[derive(Default)]
pub struct MessageQueryManager {
    /// `being_reloaded_extended_media_message_full_ids_` — dedup only; the
    /// C++ `reload_message_extended_media` inserts into the fact-check
    /// set (MessageQueryManager.cpp:3264-3265) and the finish erases from
    /// this one (cpp:3273-3277) — mirrored as-is.
    being_reloaded_extended_media_message_full_ids: HashSet<MessageFullId>,
    /// `being_reloaded_fact_checks_`.
    being_reloaded_fact_checks: HashSet<MessageFullId>,

    /// `need_view_counter_increment_message_full_ids_`.
    need_view_counter_increment_message_full_ids: HashSet<MessageFullId>,
    /// `being_reloaded_views_message_full_ids_`.
    being_reloaded_views_message_full_ids: HashSet<MessageFullId>,

    /// `ReactionsToReload` + `being_reloaded_reactions_`
    /// (MessageQueryManager.h:503-507).
    being_reloaded_reactions: HashMap<DialogId, ReactionsToReload>,
    /// The `SleepActor(0.2)` retry stand-in
    /// (MessageQueryManager.cpp:3878-3884): the dialog fires
    /// `try_reload_message_reactions(dialog, true)` at the deadline —
    /// the collapsed-line deadline replaces the one-shot actor.
    reactions_reload_retry_deadlines: HashMap<DialogId, f64>,

    /// `pending_read_reactions_` (MessageQueryManager.h:509).
    pending_read_reactions: HashMap<MessageFullId, i32>,
    /// `pending_read_poll_votes_` (MessageQueryManager.h:511).
    pending_read_poll_votes: HashMap<MessageFullId, i32>,

    /// `MessageViewMetrics` + `pending_message_view_metrics_`
    /// (MessageQueryManager.h:513-520).
    pending_message_view_metrics: HashMap<DialogId, Vec<MessageViewMetrics>>,
    /// The `send_message_view_metrics_timeout_` MultiTimeout
    /// (MessageQueryManager.h:562): dialog → 5.0s deadline.
    view_metrics_deadlines: HashMap<DialogId, f64>,

    /// `rich_message_full_id_to_file_source_id_`
    /// (MessageQueryManager.h:560) — the WaitFreeHashMap collapses to a
    /// plain map.
    rich_message_full_id_to_file_source_id: HashMap<MessageFullId, crate::files::FileSourceId>,
    /// `is_emoji_game_info_inited_` (MessageQueryManager.h:552-554).
    is_emoji_game_info_inited: bool,
    emoji_game_info_receive_time: f64,
    emoji_game_info: EmojiGameInfo,
}

/// `MessageQueryManager::ReactionsToReload` (MessageQueryManager.h:503-506).
#[derive(Default)]
struct ReactionsToReload {
    message_ids: HashSet<MessageId>,
    is_request_sent: bool,
}

/// `MessageQueryManager::MessageViewMetrics` (MessageQueryManager.h:513-519).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MessageViewMetrics {
    message_id: MessageId,
    time_in_view_ms: i32,
    active_time_in_view_ms: i32,
    height_to_viewport_ratio_per_mille: i32,
    seen_range_ratio_per_mille: i32,
}

/// The `telegram_api::inputMessageReadMetric` payload the timeout flush
/// sends — the Rust stand-in for the wire struct (the id draw rides the
/// env's secure random).
pub struct MessageReadMetric {
    pub message_id: MessageId,
    pub view_id: i64,
    pub time_in_view_ms: i32,
    pub active_time_in_view_ms: i32,
    pub height_to_viewport_ratio_per_mille: i32,
    pub seen_range_ratio_per_mille: i32,
}

/// The affected-history query identity — which net request refills the
/// loop ([blocked: net]; the C++ closures at
/// MessageQueryManager.cpp:4334/4380/4443/4536/4783/4832/4866/4876/4911/4962/4973/5150).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AffectedHistoryQueryKind {
    DeleteAllCallMessages {
        revoke: bool,
    },
    DeleteAllChannelMessagesFromSender {
        sender_dialog_id: DialogId,
    },
    DeleteDialogHistory {
        max_message_id: MessageId,
        remove_from_dialog_list: bool,
        revoke: bool,
    },
    DeleteDialogMessagesByDate {
        min_date: i32,
        max_date: i32,
        revoke: bool,
    },
    DeleteTopicHistory {
        forum_topic_id: crate::files::ForumTopicId,
    },
    ReadAllDialogMentions,
    ReadAllTopicMentions {
        forum_topic_id: crate::files::ForumTopicId,
    },
    ReadAllDialogReactions,
    ReadAllTopicReactions {
        forum_topic_id: crate::files::ForumTopicId,
    },
    ReadAllDialogPollVotes {
        forum_topic_id: crate::files::ForumTopicId,
    },
    UnpinAllDialogMessages,
    UnpinAllTopicMessages {
        forum_topic_id: crate::files::ForumTopicId,
    },
}

impl MessageQueryManager {
    pub fn new() -> MessageQueryManager {
        MessageQueryManager::default()
    }

    /// `fix_delete_message_min_max_dates` (MessageQueryManager.cpp:4501-4527)
    /// — the static date-window normalizer; returns the fixed
    /// `(min_date, max_date)` pair.
    pub fn fix_delete_message_min_max_dates(
        env: &impl MessageQueryManagerEnv,
        mut min_date: i32,
        mut max_date: i32,
    ) -> Result<(i32, i32), tdutils::status::Status> {
        if min_date > max_date {
            return Err(tdutils::status::Status::error(
                400,
                "Wrong date interval specified",
            ));
        }

        const TELEGRAM_LAUNCH_DATE: i32 = 1376438400;
        if max_date < TELEGRAM_LAUNCH_DATE {
            return Ok((0, 0));
        }
        if min_date < TELEGRAM_LAUNCH_DATE {
            min_date = TELEGRAM_LAUNCH_DATE;
        }

        let current_date = env.unix_time().max(1635000000);
        if min_date >= current_date - 30 {
            return Ok((0, 0));
        }
        if max_date >= current_date - 30 {
            max_date = current_date - 31;
        }
        assert!(min_date <= max_date);
        Ok((min_date, max_date))
    }

    /// `run_affected_history_query_until_complete`
    /// (MessageQueryManager.cpp:2447-2461). The query send is the
    /// `[blocked: net]` tail — the promise lands in the query and stays
    /// unanswered offline; the net step routes each
    /// `messages_affectedHistory` back into [`Self::on_get_affected_history`].
    pub fn run_affected_history_query_until_complete(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        query: AffectedHistoryQueryKind,
        get_affected_messages: bool,
        promise: Promise<()>,
    ) {
        assert!(!env.close_flag());
        env.send_affected_history_query(dialog_id, query, get_affected_messages, promise);
    }

    /// `on_get_affected_history` (MessageQueryManager.cpp:2463-2492) — the
    /// pts bookkeeping and the loop continuation.
    pub fn on_get_affected_history(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        query: AffectedHistoryQueryKind,
        get_affected_messages: bool,
        mut affected_history: AffectedHistory,
        mut promise: Promise<()>,
    ) {
        if env.close_flag() {
            return promise.set_error(tdutils::status::Status::error(500, "Request aborted"));
        }
        tracing::info!(
            "Receive {} affected history with PTS = {} and pts_count = {}",
            if affected_history.is_final {
                "final"
            } else {
                "partial"
            },
            affected_history.pts,
            affected_history.pts_count
        );

        if affected_history.pts_count > 0 {
            if get_affected_messages {
                affected_history.pts_count = 0;
            }
            let update_promise = if affected_history.is_final {
                std::mem::take(&mut promise)
            } else {
                Promise::default()
            };
            if dialog_id.get_type() == crate::dialog_id::DialogType::Channel {
                env.add_pending_channel_update(
                    dialog_id,
                    affected_history.pts,
                    affected_history.pts_count,
                    update_promise,
                    "on_get_affected_history",
                );
            } else {
                let receive_time = env.now() - if get_affected_messages { 10.0 } else { 0.0 };
                env.add_pending_pts_update(
                    affected_history.pts,
                    affected_history.pts_count,
                    receive_time,
                    update_promise,
                    "on_get_affected_history",
                );
            }
        } else if affected_history.is_final {
            promise.set_value(());
        }

        if !affected_history.is_final {
            self.run_affected_history_query_until_complete(
                env,
                dialog_id,
                query,
                get_affected_messages,
                promise,
            );
        }
    }

    /// `get_rich_message_file_source_id` (MessageQueryManager.cpp:2551-2567)
    /// — the FileSource cache half; the source creation rides the
    /// FileReferenceManager seam.
    pub fn get_rich_message_file_source_id(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        message_full_id: MessageFullId,
    ) -> crate::files::FileSourceId {
        if env.auth_is_bot() {
            return crate::files::FileSourceId::default();
        }

        let dialog_id = message_full_id.get_dialog_id();
        let message_id = message_full_id.get_message_id();
        if !dialog_id.is_valid()
            || dialog_id.get_type() == crate::dialog_id::DialogType::SecretChat
            || !message_id.is_server()
        {
            return crate::files::FileSourceId::default();
        }

        let entry = self
            .rich_message_full_id_to_file_source_id
            .entry(message_full_id)
            .or_default();
        if !entry.is_valid() {
            *entry = env.create_rich_message_file_source(message_full_id);
        }
        *entry
    }

    /// `reload_message_extended_media` (MessageQueryManager.cpp:3262-3271)
    /// — the dedup half; the query send is `[blocked: net]`.
    pub fn reload_message_extended_media(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        mut message_ids: Vec<MessageId>,
    ) {
        assert!(dialog_id.get_type() != crate::dialog_id::DialogType::SecretChat);
        // the C++ inserts into being_reloaded_fact_checks_ here
        // (cpp:3264-3265) — mirrored as-is
        message_ids.retain(|message_id| {
            self.being_reloaded_fact_checks
                .insert(MessageFullId::new(dialog_id, *message_id))
        });
        if message_ids.is_empty() {
            return;
        }
        env.send_get_extended_media_query(dialog_id, message_ids);
    }

    /// `finish_get_message_extended_media`
    /// (MessageQueryManager.cpp:3273-3277).
    pub fn finish_get_message_extended_media(
        &mut self,
        dialog_id: DialogId,
        message_ids: &[MessageId],
    ) {
        for message_id in message_ids {
            self.being_reloaded_extended_media_message_full_ids
                .remove(&MessageFullId::new(dialog_id, *message_id));
        }
    }

    /// `reload_message_fact_checks` (MessageQueryManager.cpp:3279-3295) —
    /// the dedup half; the query send is `[blocked: net]`.
    pub fn reload_message_fact_checks(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        mut message_ids: Vec<MessageId>,
    ) {
        assert!(dialog_id.get_type() != crate::dialog_id::DialogType::SecretChat);
        message_ids.retain(|message_id| {
            self.being_reloaded_fact_checks
                .insert(MessageFullId::new(dialog_id, *message_id))
        });
        if message_ids.is_empty() {
            return;
        }
        env.send_get_fact_check_query(dialog_id, message_ids);
    }

    /// `on_reload_message_fact_checks` (MessageQueryManager.cpp:3297-3318)
    /// — the local bookkeeping; the fact-check intake lands with the
    /// message plane.
    pub fn on_reload_message_fact_checks(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_ids: &[MessageId],
        is_error: bool,
    ) {
        for message_id in message_ids {
            let erased = self
                .being_reloaded_fact_checks
                .remove(&MessageFullId::new(dialog_id, *message_id));
            assert!(erased);
        }
        let _ = (is_error, env);
        // `on_update_message_fact_check` — the [blocked: M.10 integration]
        // consumer
    }

    /// `send_message_view_metrics` (MessageQueryManager.cpp:3652-3678) —
    /// validation, the local accumulate and the 5.0s re-arm; the flush is
    /// the `[blocked: net]` tail.
    #[allow(clippy::too_many_arguments)]
    pub fn send_message_view_metrics(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_id: MessageId,
        time_in_view_ms: i32,
        active_time_in_view_ms: i32,
        height_to_viewport_ratio_per_mille: i32,
        seen_range_ratio_per_mille: i32,
        promise: Promise<()>,
    ) {
        let mut promise = promise;
        if let Err(error) = env.check_dialog_access(
            dialog_id,
            true,
            crate::user_manager::AccessRights::Read,
            "send_message_view_metrics",
        ) {
            return promise.set_error(error);
        }
        if !(0..=1000000000).contains(&time_in_view_ms)
            || !(0..=1000000000).contains(&active_time_in_view_ms)
            || height_to_viewport_ratio_per_mille < 0
            || !(0..=1000).contains(&seen_range_ratio_per_mille)
        {
            return promise.set_error(tdutils::status::Status::error(
                400,
                "Invalid metrics specified",
            ));
        }
        if dialog_id.get_type() == crate::dialog_id::DialogType::SecretChat
            || !message_id.is_server()
            || time_in_view_ms < 300
            || !env.have_message_force(
                MessageFullId::new(dialog_id, message_id),
                "send_message_view_metrics",
            )
        {
            return promise.set_value(());
        }
        self.pending_message_view_metrics
            .entry(dialog_id)
            .or_default()
            .push(MessageViewMetrics {
                message_id,
                time_in_view_ms,
                active_time_in_view_ms,
                height_to_viewport_ratio_per_mille,
                seen_range_ratio_per_mille,
            });
        self.view_metrics_deadlines
            .insert(dialog_id, env.now() + 5.0);

        promise.set_value(());
    }

    /// `send_message_view_metrics_timeout`
    /// (MessageQueryManager.cpp:3691-3723) — the fold: summed view times,
    /// max ratios, one metric per message; the report send is
    /// `[blocked: net]`.
    fn send_message_view_metrics_timeout(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
    ) {
        let Some(input_metrics_raw) = self.pending_message_view_metrics.remove(&dialog_id) else {
            return;
        };
        let mut metrics: HashMap<MessageId, MessageViewMetrics> = HashMap::new();
        for input_metric in &input_metrics_raw {
            assert!(input_metric.message_id.is_server());
            let metric = metrics.entry(input_metric.message_id).or_default();
            metric.message_id = input_metric.message_id;
            metric.time_in_view_ms += input_metric.time_in_view_ms;
            metric.active_time_in_view_ms += input_metric.active_time_in_view_ms;
            metric.height_to_viewport_ratio_per_mille = metric
                .height_to_viewport_ratio_per_mille
                .max(input_metric.height_to_viewport_ratio_per_mille);
            metric.seen_range_ratio_per_mille = metric
                .seen_range_ratio_per_mille
                .max(input_metric.seen_range_ratio_per_mille);
        }

        let input_metrics = metrics
            .into_values()
            .map(|metric| MessageReadMetric {
                message_id: metric.message_id,
                view_id: env.secure_random_i64(),
                time_in_view_ms: metric.time_in_view_ms,
                active_time_in_view_ms: metric.active_time_in_view_ms,
                height_to_viewport_ratio_per_mille: metric.height_to_viewport_ratio_per_mille,
                seen_range_ratio_per_mille: metric.seen_range_ratio_per_mille,
            })
            .collect();
        env.send_report_read_metrics_query(dialog_id, input_metrics);
    }

    /// `get_message_viewers` (MessageQueryManager.cpp:3731-3746) — the
    /// access check rides the message plane (`can_get_message_viewers`,
    /// [blocked: M.10 integration]) and the query send is
    /// `[blocked: net]`; the port carries the promise shape.
    pub fn get_message_viewers(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        message_full_id: MessageFullId,
        mut promise: Promise<td_api::MessageViewers>,
    ) {
        if let Err(error) = env.can_get_message_viewers(message_full_id) {
            return promise.set_error(error);
        }
        env.send_get_message_read_participants_query(message_full_id, promise);
    }

    /// `on_get_message_viewers` (MessageQueryManager.cpp:3748-3782) — the
    /// participant-list gate and the final object build; the full reload
    /// is the chat-manager plane ([blocked: M.6 integration]).
    pub fn on_get_message_viewers(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        message_viewers: MessageViewers,
        is_recursive: bool,
        mut promise: Promise<td_api::MessageViewers>,
    ) {
        if !is_recursive {
            let need_participant_list = message_viewers
                .get_user_ids()
                .iter()
                .any(|user_id| !env.have_user_force(*user_id, "on_get_message_viewers"));
            if need_participant_list {
                // `reload_chat_full`/`get_channel_participants` then the
                // recursive call — the [blocked: M.6 integration] seam; the
                // collapsed line answers without the participant load
                // (the C++ recursive pass only fills the user cache)
                let mut object = message_viewers.get_message_viewers_object(env);
                let _ = &mut object;
                return promise.set_value(object);
            }
        }
        promise.set_value(message_viewers.get_message_viewers_object(env));
    }

    /// `view_messages` (MessageQueryManager.cpp:3784-3809) — the dedup
    /// sets and the 100-id chunking; each chunk send is `[blocked: net]`.
    pub fn view_messages(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_ids: &[MessageId],
        increment_view_counter: bool,
    ) {
        const MAX_MESSAGE_VIEWS: usize = 100; // server-side limit
        let mut viewed_message_ids = Vec::with_capacity(message_ids.len().min(MAX_MESSAGE_VIEWS));
        for message_id in message_ids {
            let message_full_id = MessageFullId::new(dialog_id, *message_id);
            if !self
                .being_reloaded_views_message_full_ids
                .insert(message_full_id)
            {
                if !increment_view_counter
                    || !self
                        .need_view_counter_increment_message_full_ids
                        .insert(message_full_id)
                {
                    continue;
                }
            } else if increment_view_counter {
                self.need_view_counter_increment_message_full_ids
                    .insert(message_full_id);
            }
            viewed_message_ids.push(*message_id);
            if viewed_message_ids.len() >= MAX_MESSAGE_VIEWS {
                env.send_get_messages_views_query(
                    dialog_id,
                    std::mem::take(&mut viewed_message_ids),
                    increment_view_counter,
                );
            }
        }
        if !viewed_message_ids.is_empty() {
            env.send_get_messages_views_query(
                dialog_id,
                viewed_message_ids,
                increment_view_counter,
            );
        }
    }

    /// `finish_get_message_views` (MessageQueryManager.cpp:3811-3817).
    pub fn finish_get_message_views(&mut self, dialog_id: DialogId, message_ids: &[MessageId]) {
        for message_id in message_ids {
            let message_full_id = MessageFullId::new(dialog_id, *message_id);
            self.being_reloaded_views_message_full_ids
                .remove(&message_full_id);
            self.need_view_counter_increment_message_full_ids
                .remove(&message_full_id);
        }
    }

    /// `queue_message_reactions_reload(MessageFullId)`
    /// (MessageQueryManager.cpp:3819-3826).
    pub fn queue_message_reactions_reload(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        message_full_id: MessageFullId,
    ) {
        let dialog_id = message_full_id.get_dialog_id();
        assert!(dialog_id.is_valid());
        let message_id = message_full_id.get_message_id();
        assert!(message_id.is_valid());
        self.being_reloaded_reactions
            .entry(dialog_id)
            .or_default()
            .message_ids
            .insert(message_id);
        self.try_reload_message_reactions(env, dialog_id, false);
    }

    /// `queue_message_reactions_reload(DialogId, vector<MessageId>)`
    /// (MessageQueryManager.cpp:3828-3836) — the overload set splits on
    /// the argument shape.
    pub fn queue_message_reactions_reload_for_dialog(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_ids: &[MessageId],
    ) {
        let message_ids_to_reload = &mut self
            .being_reloaded_reactions
            .entry(dialog_id)
            .or_default()
            .message_ids;
        for message_id in message_ids {
            assert!(message_id.is_valid());
            message_ids_to_reload.insert(*message_id);
        }
        self.try_reload_message_reactions(env, dialog_id, false);
    }

    /// `try_reload_message_reactions` (MessageQueryManager.cpp:3838-3892)
    /// — the queue drain and the request flag; the query send is
    /// `[blocked: net]`, the no-input-peer retry rides the 0.2s deadline
    /// (the SleepActor stand-in).
    pub fn try_reload_message_reactions(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        is_finished: bool,
    ) {
        if env.close_flag() {
            return;
        }

        let Some(state) = self.being_reloaded_reactions.get_mut(&dialog_id) else {
            return;
        };
        if is_finished {
            assert!(state.is_request_sent);
            state.is_request_sent = false;

            if state.message_ids.is_empty() {
                self.being_reloaded_reactions.remove(&dialog_id);
                self.reactions_reload_retry_deadlines.remove(&dialog_id);
                return;
            }
        } else if state.is_request_sent {
            return;
        }

        assert!(!state.message_ids.is_empty());
        assert!(!state.is_request_sent);

        state.is_request_sent = true;

        const MAX_MESSAGE_IDS: usize = 100; // server-side limit
        let mut message_ids = Vec::new();
        for message_id in state.message_ids.iter() {
            if message_ids.len() >= MAX_MESSAGE_IDS {
                break;
            }
            // the disjoint-field read of the pending map (the C++ reads
            // has_message_pending_read_reactions mid-iteration)
            if !self
                .pending_read_reactions
                .contains_key(&MessageFullId::new(dialog_id, *message_id))
            {
                message_ids.push(*message_id);
            }
        }
        for message_id in &message_ids {
            state.message_ids.remove(message_id);
        }
        // the C++ iterates its FlatHashSet (bucket order); the port sorts —
        // the id order is not part of the query contract, and the sort
        // pins the tests
        message_ids.sort_unstable();

        if !env.have_input_peer(dialog_id, false, crate::user_manager::AccessRights::Read)
            || message_ids.is_empty()
        {
            self.reactions_reload_retry_deadlines
                .insert(dialog_id, env.now() + 0.2);
            return;
        }

        for message_id in &message_ids {
            assert!(message_id.is_server());
        }

        env.send_get_messages_reactions_query(dialog_id, message_ids);
    }

    /// `get_paid_message_reaction_senders` +
    /// `do_get_paid_message_reaction_senders`
    /// (MessageQueryManager.cpp:3894-3938) — the M.16 routing-audit
    /// obligation. The load half (`load_created_public_broadcasts` +
    /// `get_me`) is `[blocked: net]`; the answer half needs the
    /// created-broadcasts/participant-count planes
    /// ([blocked: M.5 integration]) — the arm stays interim.
    pub fn get_paid_message_reaction_senders(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        _dialog_id: DialogId,
        promise: Promise<td_api::MessageSenders>,
    ) {
        // the C++ dialog_id rides the load; the seam drops it with the
        // promise ([blocked: net])
        env.load_paid_message_reaction_sender_state(promise);
    }

    /// `has_message_pending_read_reactions`
    /// (MessageQueryManager.cpp:5046-5048).
    pub fn has_message_pending_read_reactions(&self, message_full_id: MessageFullId) -> bool {
        self.pending_read_reactions.contains_key(&message_full_id)
    }

    /// `has_message_pending_read_poll_votes`
    /// (MessageQueryManager.cpp:5082-5084).
    pub fn has_message_pending_read_poll_votes(&self, message_full_id: MessageFullId) -> bool {
        self.pending_read_poll_votes.contains_key(&message_full_id)
    }

    /// `read_message_reactions_on_server`
    /// (MessageQueryManager.cpp:5050-5060) — the pending counter bump;
    /// the contents read is the `[blocked: net]` tail.
    pub fn read_message_reactions_on_server(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_ids: Vec<MessageId>,
    ) {
        for message_id in &message_ids {
            *self
                .pending_read_reactions
                .entry(MessageFullId::new(dialog_id, *message_id))
                .or_default() += 1;
        }
        env.send_read_message_contents_query(dialog_id, message_ids);
    }

    /// `on_read_message_reactions` (MessageQueryManager.cpp:5062-5080).
    pub fn on_read_message_reactions(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_ids: Vec<MessageId>,
        is_error: bool,
    ) {
        for message_id in message_ids {
            let message_full_id = MessageFullId::new(dialog_id, message_id);
            let counter = self
                .pending_read_reactions
                .get_mut(&message_full_id)
                .expect("the pending read-reactions counter underflowed");
            *counter -= 1;
            if *counter == 0 {
                self.pending_read_reactions.remove(&message_full_id);
            }

            if !env.have_message_force(message_full_id, "on_read_message_reactions") {
                continue;
            }

            if is_error {
                self.queue_message_reactions_reload(env, message_full_id);
            }
        }
    }

    /// `read_message_poll_votes_on_server`
    /// (MessageQueryManager.cpp:5086-5097).
    pub fn read_message_poll_votes_on_server(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_ids: Vec<MessageId>,
    ) {
        assert!(dialog_id.is_valid());
        for message_id in &message_ids {
            *self
                .pending_read_poll_votes
                .entry(MessageFullId::new(dialog_id, *message_id))
                .or_default() += 1;
        }
        env.send_read_message_contents_query(dialog_id, message_ids);
    }

    /// `on_read_message_poll_votes` (MessageQueryManager.cpp:5099-5121).
    pub fn on_read_message_poll_votes(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        dialog_id: DialogId,
        message_ids: Vec<MessageId>,
        is_error: bool,
    ) {
        let mut message_full_ids = Vec::new();
        for message_id in message_ids {
            let message_full_id = MessageFullId::new(dialog_id, message_id);
            let counter = self
                .pending_read_poll_votes
                .get_mut(&message_full_id)
                .expect("the pending read-poll-votes counter underflowed");
            *counter -= 1;
            if *counter == 0 {
                self.pending_read_poll_votes.remove(&message_full_id);
            }

            if !env.have_message_force(message_full_id, "on_read_message_poll_votes") {
                continue;
            }
            if is_error {
                message_full_ids.push(message_full_id);
            }
        }
        if !message_full_ids.is_empty() {
            // `get_messages_from_server` — the [blocked: M.10 integration]
            // refetch
            env.get_messages_from_server(message_full_ids);
        }
    }

    /// `get_emoji_game_info` (MessageQueryManager.cpp:4059-4069) — the
    /// fetch is `[blocked: net]`; the promise lands in the query.
    pub fn get_emoji_game_info(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        promise: Promise<td_api::StakeDiceState>,
    ) {
        env.send_get_emoji_game_info_query(promise);
    }

    /// `on_get_emoji_game_info` (MessageQueryManager.cpp:4071-4077) —
    /// fully local once the wire answer is in.
    pub fn on_get_emoji_game_info(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        game_info: &telegram_api::messages_EmojiGameInfo,
        mut promise: Promise<td_api::StakeDiceState>,
    ) {
        self.on_update_emoji_game_info(env, game_info);
        assert!(self.is_emoji_game_info_inited);
        promise.set_value(self.emoji_game_info.get_stake_dice_state_object(env));
    }

    /// `on_update_emoji_game_info` (MessageQueryManager.cpp:5158-5171) —
    /// the bot gate, the equality no-op and the update send.
    pub fn on_update_emoji_game_info(
        &mut self,
        env: &mut impl MessageQueryManagerEnv,
        game_info: &telegram_api::messages_EmojiGameInfo,
    ) {
        let emoji_game_info = EmojiGameInfo::from_wire(game_info);
        if env.auth_is_bot() {
            return;
        }
        if self.is_emoji_game_info_inited && emoji_game_info == self.emoji_game_info {
            return;
        }
        self.emoji_game_info = emoji_game_info;
        self.emoji_game_info_receive_time = env.now();
        self.is_emoji_game_info_inited = true;
        let update = self.emoji_game_info.get_update_stake_dice_state_object(env);
        env.send_update(update);
    }

    /// `get_current_state` (MessageQueryManager.cpp:5173-5181) — the
    /// 60-second freshness window.
    pub fn get_current_state(
        &self,
        env: &impl MessageQueryManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if env.auth_is_bot() {
            return;
        }

        if self.is_emoji_game_info_inited && self.emoji_game_info_receive_time > env.now() - 60.0 {
            updates.push(self.emoji_game_info.get_update_stake_dice_state_object(env));
        }
    }

    /// `send_message_view_metrics_timeout_` + the SleepActor retry, folded
    /// into the collapsed-line deadline tick.
    pub fn tick_timeouts(&mut self, env: &mut impl MessageQueryManagerEnv) {
        let now = env.now();
        let fired_metrics: Vec<DialogId> = self
            .view_metrics_deadlines
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(dialog_id, _)| *dialog_id)
            .collect();
        for dialog_id in fired_metrics {
            self.view_metrics_deadlines.remove(&dialog_id);
            self.send_message_view_metrics_timeout(env, dialog_id);
        }

        let fired_retries: Vec<DialogId> = self
            .reactions_reload_retry_deadlines
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(dialog_id, _)| *dialog_id)
            .collect();
        for dialog_id in fired_retries {
            self.reactions_reload_retry_deadlines.remove(&dialog_id);
            self.try_reload_message_reactions(env, dialog_id, true);
        }
    }

    /// The merged next deadline of the two timeout families.
    pub fn next_deadline(&self) -> Option<f64> {
        self.view_metrics_deadlines
            .values()
            .chain(self.reactions_reload_retry_deadlines.values())
            .fold(None, |acc: Option<f64>, deadline| {
                Some(acc.map_or(*deadline, |current| current.min(*deadline)))
            })
    }
}

impl std::fmt::Debug for MessageQueryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageQueryManager")
            .field("pending_read_reactions", &self.pending_read_reactions.len())
            .field(
                "pending_read_poll_votes",
                &self.pending_read_poll_votes.len(),
            )
            .field(
                "being_reloaded_reactions",
                &self.being_reloaded_reactions.len(),
            )
            .field(
                "pending_message_view_metrics",
                &self.pending_message_view_metrics.len(),
            )
            .field("is_emoji_game_info_inited", &self.is_emoji_game_info_inited)
            .finish()
    }
}

#[cfg(test)]
#[path = "message_query_manager_tests.rs"]
mod message_query_manager_tests;
