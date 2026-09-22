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
use crate::dialog_id::{DialogId, ServerMessageId};
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;

/// `MessageQueryManager::MAX_SEARCH_MESSAGES` (MessageQueryManager.h:351) —
/// the server-side search limit the request arms clamp to; its consumers
/// (the search arms) land with the M.18.4 wiring.
#[allow(dead_code)]
pub(crate) const MAX_SEARCH_MESSAGES: i32 = 100;

/// The `G()`/manager slice the collapsed MessageQueryManager reads through.
pub trait MessageQueryManagerEnv {
    /// `td_->auth_manager_->is_bot()`.
    fn auth_is_bot(&self) -> bool;
    /// `G()->close_flag()`.
    fn close_flag(&self) -> bool;
    /// `G()->unix_time()`.
    fn unix_time(&self) -> i32;
    /// `td_->option_manager_->get_option_string(name, default)`.
    fn get_option_string(&self, name: &str, default_value: &str) -> String;
    /// `user_manager_->get_user_id_object(user_id, source)` — the i53
    /// surface the viewer builders read.
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64;
    /// `user_manager_->have_user_force(user_id, source)` — the
    /// participant-list gate of `on_get_message_viewers`.
    fn have_user_force(&mut self, user_id: UserId, source: &str) -> bool;
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

#[cfg(test)]
#[path = "message_query_manager_tests.rs"]
mod message_query_manager_tests;
