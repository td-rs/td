//! Port of `td/telegram/UpdatesManager.{h,cpp}` (M.7.1–M.7.5, master
//! 85af8aa): the update state (pts/qts/date/seq), its binlog-pmc
//! persistence, the `set_pts`/`set_date` decision trees, the
//! `on_get_updates` routing (`Updates` umbrella dispatch), the pts/qts/seq
//! pending + postponed queues with their multiset ordering, and the gap
//! timers that funnel every gap into `get_difference`.
//!
//! Engine seams (each flips to a real manager intake when that slice
//! lands; the C++ line references name the flip target):
//! - [`UpdatesSink`] — the manager fan-out (`td_->messages_manager_->…`
//!   & friends). The fan-out census and the per-arm flips are M.7.12–M.7.15;
//!   the arms this engine needs today are the message-family stand-ins.
//! - [`UpdatesManagerEnv`] — the `G()->` / `td_->` environment surface
//!   (clock, options, binlog pmc, authorization) plus the GetDifference /
//!   GetUpdatesState / ConfirmPtsQts / GetPtsUpdate senders, which stay
//!   `[blocked: net]` until the net step (the deferred S76
//!   bridge↔dispatcher reconciliation); the S74 harness drives the
//!   response intakes (`on_get_difference`, `on_get_updates_state`)
//!   directly.
//!
//! Documented divergences (all synchronous-collapse, the M.3/M.4 pmc
//! convention):
//! - C++ defers pmc writes through event promises (`create_event_promise`);
//!   our `BinlogKeyValue::set` is synchronous, so `add_pts`/`add_qts` fire
//!   their acks immediately (the `PtsManager::finish` structure is kept).
//! - C++ completes update promises asynchronously through actors
//!   (`MultiPromiseActorSafe`, `send_closure_later`); our promises complete
//!   at the same decision points, synchronously. `being_processed_updates_`
//!   bookkeeping is therefore not observable.
//! - The acceptability filter (M.7.10/M.7.11) asks the sink whether a
//!   user/chat/channel is known; the filter's `nullptr`-peer semantics
//!   collapse onto the wire's zero-id default peer (see
//!   `peer_id_is_zero`).
//! - The pmc state reads use a strict canonical-form int32 parse; the C++
//!   `to_integer<int32>` is a lenient prefix parse. Values are self-written
//!   (`to_string(int32)`), so intact databases parse identically; a corrupt
//!   value logs an error and reads as 0 instead of C++'s prefix value.

use std::collections::BTreeMap;
use std::collections::HashSet;

use crate::chat_manager::{ChannelId, ChatId, DialogId, DialogType};
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

// UpdatesManager.h:161-168 (DROP_PTS_UPDATES gates nothing today — kept
// because the C++ keeps it; the dead branch is the debug kill switch).
/// `FORCED_GET_DIFFERENCE_PTS_DIFF`.
pub const FORCED_GET_DIFFERENCE_PTS_DIFF: i32 = 100_000;
/// `GAP_TIMEOUT_UPDATE_COUNT`.
pub const GAP_TIMEOUT_UPDATE_COUNT: usize = 20;
/// `MIN_UNFILLED_GAP_TIME`.
pub const MIN_UNFILLED_GAP_TIME: f64 = 0.05;
/// `MAX_UNFILLED_GAP_TIME`.
pub const MAX_UNFILLED_GAP_TIME: f64 = 0.7;
/// `MAX_PTS_SAVE_DELAY`.
pub const MAX_PTS_SAVE_DELAY: f64 = 0.05;
/// `UPDATE_APPLY_WARNING_TIME`.
pub const UPDATE_APPLY_WARNING_TIME: f64 = 0.1;
/// `DROP_PTS_UPDATES` (dead in C++ 1.8.67).
pub const DROP_PTS_UPDATES: bool = false;
/// `AFTER_GET_DIFFERENCE_SOURCE`.
pub const AFTER_GET_DIFFERENCE_SOURCE: &str = "after get difference";
/// `skipped_postponed_updates_after_start_` initial value (h:308).
const SKIPPED_POSTPONED_UPDATES_AFTER_START: i32 = 50_000;

/// The constructor id and the scheme name of any `Update` variant — the
/// M.7.12 exhaustive table (generated from the pinned scheme; a new codegen
/// variant fails to compile until this arm is added).
fn update_id_and_name(update: &telegram_api::Update) -> (i32, &'static str) {
    use telegram_api::*;
    match update {
        Update::updateNewMessage(_t) => (updateNewMessage::ID, "updateNewMessage"),
        Update::updateMessageID(_t) => (updateMessageID::ID, "updateMessageID"),
        Update::updateDeleteMessages(_t) => (updateDeleteMessages::ID, "updateDeleteMessages"),
        Update::updateUserTyping(_t) => (updateUserTyping::ID, "updateUserTyping"),
        Update::updateChatUserTyping(_t) => (updateChatUserTyping::ID, "updateChatUserTyping"),
        Update::updateChatParticipants(_t) => {
            (updateChatParticipants::ID, "updateChatParticipants")
        }
        Update::updateUserStatus(_t) => (updateUserStatus::ID, "updateUserStatus"),
        Update::updateUserName(_t) => (updateUserName::ID, "updateUserName"),
        Update::updateNewAuthorization(_t) => {
            (updateNewAuthorization::ID, "updateNewAuthorization")
        }
        Update::updateNewEncryptedMessage(_t) => {
            (updateNewEncryptedMessage::ID, "updateNewEncryptedMessage")
        }
        Update::updateEncryptedChatTyping(_t) => {
            (updateEncryptedChatTyping::ID, "updateEncryptedChatTyping")
        }
        Update::updateEncryption(_t) => (updateEncryption::ID, "updateEncryption"),
        Update::updateEncryptedMessagesRead(_t) => (
            updateEncryptedMessagesRead::ID,
            "updateEncryptedMessagesRead",
        ),
        Update::updateChatParticipantAdd(_t) => {
            (updateChatParticipantAdd::ID, "updateChatParticipantAdd")
        }
        Update::updateChatParticipantDelete(_t) => (
            updateChatParticipantDelete::ID,
            "updateChatParticipantDelete",
        ),
        Update::updateDcOptions(_t) => (updateDcOptions::ID, "updateDcOptions"),
        Update::updateNotifySettings(_t) => (updateNotifySettings::ID, "updateNotifySettings"),
        Update::updateServiceNotification(_t) => {
            (updateServiceNotification::ID, "updateServiceNotification")
        }
        Update::updatePrivacy(_t) => (updatePrivacy::ID, "updatePrivacy"),
        Update::updateUserPhone(_t) => (updateUserPhone::ID, "updateUserPhone"),
        Update::updateReadHistoryInbox(_t) => {
            (updateReadHistoryInbox::ID, "updateReadHistoryInbox")
        }
        Update::updateReadHistoryOutbox(_t) => {
            (updateReadHistoryOutbox::ID, "updateReadHistoryOutbox")
        }
        Update::updateWebPage(_t) => (updateWebPage::ID, "updateWebPage"),
        Update::updateReadMessagesContents(_t) => {
            (updateReadMessagesContents::ID, "updateReadMessagesContents")
        }
        Update::updateChannelTooLong(_t) => (updateChannelTooLong::ID, "updateChannelTooLong"),
        Update::updateChannel(_t) => (updateChannel::ID, "updateChannel"),
        Update::updateNewChannelMessage(_t) => {
            (updateNewChannelMessage::ID, "updateNewChannelMessage")
        }
        Update::updateReadChannelInbox(_t) => {
            (updateReadChannelInbox::ID, "updateReadChannelInbox")
        }
        Update::updateDeleteChannelMessages(_t) => (
            updateDeleteChannelMessages::ID,
            "updateDeleteChannelMessages",
        ),
        Update::updateChannelMessageViews(_t) => {
            (updateChannelMessageViews::ID, "updateChannelMessageViews")
        }
        Update::updateChatParticipantAdmin(_t) => {
            (updateChatParticipantAdmin::ID, "updateChatParticipantAdmin")
        }
        Update::updateNewStickerSet(_t) => (updateNewStickerSet::ID, "updateNewStickerSet"),
        Update::updateStickerSetsOrder(_t) => {
            (updateStickerSetsOrder::ID, "updateStickerSetsOrder")
        }
        Update::updateStickerSets(_t) => (updateStickerSets::ID, "updateStickerSets"),
        Update::updateSavedGifs(_t) => (updateSavedGifs::ID, "updateSavedGifs"),
        Update::updateBotInlineQuery(_t) => (updateBotInlineQuery::ID, "updateBotInlineQuery"),
        Update::updateBotInlineSend(_t) => (updateBotInlineSend::ID, "updateBotInlineSend"),
        Update::updateEditChannelMessage(_t) => {
            (updateEditChannelMessage::ID, "updateEditChannelMessage")
        }
        Update::updateBotCallbackQuery(_t) => {
            (updateBotCallbackQuery::ID, "updateBotCallbackQuery")
        }
        Update::updateEditMessage(_t) => (updateEditMessage::ID, "updateEditMessage"),
        Update::updateInlineBotCallbackQuery(_t) => (
            updateInlineBotCallbackQuery::ID,
            "updateInlineBotCallbackQuery",
        ),
        Update::updateReadChannelOutbox(_t) => {
            (updateReadChannelOutbox::ID, "updateReadChannelOutbox")
        }
        Update::updateDraftMessage(_t) => (updateDraftMessage::ID, "updateDraftMessage"),
        Update::updateReadFeaturedStickers(_t) => {
            (updateReadFeaturedStickers::ID, "updateReadFeaturedStickers")
        }
        Update::updateRecentStickers(_t) => (updateRecentStickers::ID, "updateRecentStickers"),
        Update::updateConfig(_t) => (updateConfig::ID, "updateConfig"),
        Update::updatePtsChanged(_t) => (updatePtsChanged::ID, "updatePtsChanged"),
        Update::updateChannelWebPage(_t) => (updateChannelWebPage::ID, "updateChannelWebPage"),
        Update::updateDialogPinned(_t) => (updateDialogPinned::ID, "updateDialogPinned"),
        Update::updatePinnedDialogs(_t) => (updatePinnedDialogs::ID, "updatePinnedDialogs"),
        Update::updateBotWebhookJSON(_t) => (updateBotWebhookJSON::ID, "updateBotWebhookJSON"),
        Update::updateBotWebhookJSONQuery(_t) => {
            (updateBotWebhookJSONQuery::ID, "updateBotWebhookJSONQuery")
        }
        Update::updateBotShippingQuery(_t) => {
            (updateBotShippingQuery::ID, "updateBotShippingQuery")
        }
        Update::updateBotPrecheckoutQuery(_t) => {
            (updateBotPrecheckoutQuery::ID, "updateBotPrecheckoutQuery")
        }
        Update::updatePhoneCall(_t) => (updatePhoneCall::ID, "updatePhoneCall"),
        Update::updateLangPackTooLong(_t) => (updateLangPackTooLong::ID, "updateLangPackTooLong"),
        Update::updateLangPack(_t) => (updateLangPack::ID, "updateLangPack"),
        Update::updateFavedStickers(_t) => (updateFavedStickers::ID, "updateFavedStickers"),
        Update::updateChannelReadMessagesContents(_t) => (
            updateChannelReadMessagesContents::ID,
            "updateChannelReadMessagesContents",
        ),
        Update::updateContactsReset(_t) => (updateContactsReset::ID, "updateContactsReset"),
        Update::updateChannelAvailableMessages(_t) => (
            updateChannelAvailableMessages::ID,
            "updateChannelAvailableMessages",
        ),
        Update::updateDialogUnreadMark(_t) => {
            (updateDialogUnreadMark::ID, "updateDialogUnreadMark")
        }
        Update::updateMessagePoll(_t) => (updateMessagePoll::ID, "updateMessagePoll"),
        Update::updateChatDefaultBannedRights(_t) => (
            updateChatDefaultBannedRights::ID,
            "updateChatDefaultBannedRights",
        ),
        Update::updateFolderPeers(_t) => (updateFolderPeers::ID, "updateFolderPeers"),
        Update::updatePeerSettings(_t) => (updatePeerSettings::ID, "updatePeerSettings"),
        Update::updatePeerLocated(_t) => (updatePeerLocated::ID, "updatePeerLocated"),
        Update::updateNewScheduledMessage(_t) => {
            (updateNewScheduledMessage::ID, "updateNewScheduledMessage")
        }
        Update::updateDeleteScheduledMessages(_t) => (
            updateDeleteScheduledMessages::ID,
            "updateDeleteScheduledMessages",
        ),
        Update::updateTheme(_t) => (updateTheme::ID, "updateTheme"),
        Update::updateGeoLiveViewed(_t) => (updateGeoLiveViewed::ID, "updateGeoLiveViewed"),
        Update::updateLoginToken(_t) => (updateLoginToken::ID, "updateLoginToken"),
        Update::updateMessagePollVote(_t) => (updateMessagePollVote::ID, "updateMessagePollVote"),
        Update::updateDialogFilter(_t) => (updateDialogFilter::ID, "updateDialogFilter"),
        Update::updateDialogFilterOrder(_t) => {
            (updateDialogFilterOrder::ID, "updateDialogFilterOrder")
        }
        Update::updateDialogFilters(_t) => (updateDialogFilters::ID, "updateDialogFilters"),
        Update::updatePhoneCallSignalingData(_t) => (
            updatePhoneCallSignalingData::ID,
            "updatePhoneCallSignalingData",
        ),
        Update::updateChannelMessageForwards(_t) => (
            updateChannelMessageForwards::ID,
            "updateChannelMessageForwards",
        ),
        Update::updateReadChannelDiscussionInbox(_t) => (
            updateReadChannelDiscussionInbox::ID,
            "updateReadChannelDiscussionInbox",
        ),
        Update::updateReadChannelDiscussionOutbox(_t) => (
            updateReadChannelDiscussionOutbox::ID,
            "updateReadChannelDiscussionOutbox",
        ),
        Update::updatePeerBlocked(_t) => (updatePeerBlocked::ID, "updatePeerBlocked"),
        Update::updateChannelUserTyping(_t) => {
            (updateChannelUserTyping::ID, "updateChannelUserTyping")
        }
        Update::updatePinnedMessages(_t) => (updatePinnedMessages::ID, "updatePinnedMessages"),
        Update::updatePinnedChannelMessages(_t) => (
            updatePinnedChannelMessages::ID,
            "updatePinnedChannelMessages",
        ),
        Update::updateChat(_t) => (updateChat::ID, "updateChat"),
        Update::updateGroupCallParticipants(_t) => (
            updateGroupCallParticipants::ID,
            "updateGroupCallParticipants",
        ),
        Update::updateGroupCall(_t) => (updateGroupCall::ID, "updateGroupCall"),
        Update::updatePeerHistoryTTL(_t) => (updatePeerHistoryTTL::ID, "updatePeerHistoryTTL"),
        Update::updateChatParticipant(_t) => (updateChatParticipant::ID, "updateChatParticipant"),
        Update::updateChannelParticipant(_t) => {
            (updateChannelParticipant::ID, "updateChannelParticipant")
        }
        Update::updateBotStopped(_t) => (updateBotStopped::ID, "updateBotStopped"),
        Update::updateGroupCallConnection(_t) => {
            (updateGroupCallConnection::ID, "updateGroupCallConnection")
        }
        Update::updateBotCommands(_t) => (updateBotCommands::ID, "updateBotCommands"),
        Update::updatePendingJoinRequests(_t) => {
            (updatePendingJoinRequests::ID, "updatePendingJoinRequests")
        }
        Update::updateBotChatInviteRequester(_t) => (
            updateBotChatInviteRequester::ID,
            "updateBotChatInviteRequester",
        ),
        Update::updateMessageReactions(_t) => {
            (updateMessageReactions::ID, "updateMessageReactions")
        }
        Update::updateAttachMenuBots(_t) => (updateAttachMenuBots::ID, "updateAttachMenuBots"),
        Update::updateWebViewResultSent(_t) => {
            (updateWebViewResultSent::ID, "updateWebViewResultSent")
        }
        Update::updateBotMenuButton(_t) => (updateBotMenuButton::ID, "updateBotMenuButton"),
        Update::updateSavedRingtones(_t) => (updateSavedRingtones::ID, "updateSavedRingtones"),
        Update::updateTranscribedAudio(_t) => {
            (updateTranscribedAudio::ID, "updateTranscribedAudio")
        }
        Update::updateReadFeaturedEmojiStickers(_t) => (
            updateReadFeaturedEmojiStickers::ID,
            "updateReadFeaturedEmojiStickers",
        ),
        Update::updateUserEmojiStatus(_t) => (updateUserEmojiStatus::ID, "updateUserEmojiStatus"),
        Update::updateRecentEmojiStatuses(_t) => {
            (updateRecentEmojiStatuses::ID, "updateRecentEmojiStatuses")
        }
        Update::updateRecentReactions(_t) => (updateRecentReactions::ID, "updateRecentReactions"),
        Update::updateMoveStickerSetToTop(_t) => {
            (updateMoveStickerSetToTop::ID, "updateMoveStickerSetToTop")
        }
        Update::updateMessageExtendedMedia(_t) => {
            (updateMessageExtendedMedia::ID, "updateMessageExtendedMedia")
        }
        Update::updateUser(_t) => (updateUser::ID, "updateUser"),
        Update::updateAutoSaveSettings(_t) => {
            (updateAutoSaveSettings::ID, "updateAutoSaveSettings")
        }
        Update::updateStory(_t) => (updateStory::ID, "updateStory"),
        Update::updateReadStories(_t) => (updateReadStories::ID, "updateReadStories"),
        Update::updateStoryID(_t) => (updateStoryID::ID, "updateStoryID"),
        Update::updateStoriesStealthMode(_t) => {
            (updateStoriesStealthMode::ID, "updateStoriesStealthMode")
        }
        Update::updateSentStoryReaction(_t) => {
            (updateSentStoryReaction::ID, "updateSentStoryReaction")
        }
        Update::updateBotChatBoost(_t) => (updateBotChatBoost::ID, "updateBotChatBoost"),
        Update::updateChannelViewForumAsMessages(_t) => (
            updateChannelViewForumAsMessages::ID,
            "updateChannelViewForumAsMessages",
        ),
        Update::updatePeerWallpaper(_t) => (updatePeerWallpaper::ID, "updatePeerWallpaper"),
        Update::updateBotMessageReaction(_t) => {
            (updateBotMessageReaction::ID, "updateBotMessageReaction")
        }
        Update::updateBotMessageReactions(_t) => {
            (updateBotMessageReactions::ID, "updateBotMessageReactions")
        }
        Update::updateSavedDialogPinned(_t) => {
            (updateSavedDialogPinned::ID, "updateSavedDialogPinned")
        }
        Update::updatePinnedSavedDialogs(_t) => {
            (updatePinnedSavedDialogs::ID, "updatePinnedSavedDialogs")
        }
        Update::updateSavedReactionTags(_t) => {
            (updateSavedReactionTags::ID, "updateSavedReactionTags")
        }
        Update::updateSmsJob(_t) => (updateSmsJob::ID, "updateSmsJob"),
        Update::updateQuickReplies(_t) => (updateQuickReplies::ID, "updateQuickReplies"),
        Update::updateNewQuickReply(_t) => (updateNewQuickReply::ID, "updateNewQuickReply"),
        Update::updateDeleteQuickReply(_t) => {
            (updateDeleteQuickReply::ID, "updateDeleteQuickReply")
        }
        Update::updateQuickReplyMessage(_t) => {
            (updateQuickReplyMessage::ID, "updateQuickReplyMessage")
        }
        Update::updateDeleteQuickReplyMessages(_t) => (
            updateDeleteQuickReplyMessages::ID,
            "updateDeleteQuickReplyMessages",
        ),
        Update::updateBotBusinessConnect(_t) => {
            (updateBotBusinessConnect::ID, "updateBotBusinessConnect")
        }
        Update::updateBotNewBusinessMessage(_t) => (
            updateBotNewBusinessMessage::ID,
            "updateBotNewBusinessMessage",
        ),
        Update::updateBotEditBusinessMessage(_t) => (
            updateBotEditBusinessMessage::ID,
            "updateBotEditBusinessMessage",
        ),
        Update::updateBotDeleteBusinessMessage(_t) => (
            updateBotDeleteBusinessMessage::ID,
            "updateBotDeleteBusinessMessage",
        ),
        Update::updateNewStoryReaction(_t) => {
            (updateNewStoryReaction::ID, "updateNewStoryReaction")
        }
        Update::updateStarsBalance(_t) => (updateStarsBalance::ID, "updateStarsBalance"),
        Update::updateBusinessBotCallbackQuery(_t) => (
            updateBusinessBotCallbackQuery::ID,
            "updateBusinessBotCallbackQuery",
        ),
        Update::updateStarsRevenueStatus(_t) => {
            (updateStarsRevenueStatus::ID, "updateStarsRevenueStatus")
        }
        Update::updateBotPurchasedPaidMedia(_t) => (
            updateBotPurchasedPaidMedia::ID,
            "updateBotPurchasedPaidMedia",
        ),
        Update::updatePaidReactionPrivacy(_t) => {
            (updatePaidReactionPrivacy::ID, "updatePaidReactionPrivacy")
        }
        Update::updateSentPhoneCode(_t) => (updateSentPhoneCode::ID, "updateSentPhoneCode"),
        Update::updateGroupCallChainBlocks(_t) => {
            (updateGroupCallChainBlocks::ID, "updateGroupCallChainBlocks")
        }
        Update::updateReadMonoForumInbox(_t) => {
            (updateReadMonoForumInbox::ID, "updateReadMonoForumInbox")
        }
        Update::updateReadMonoForumOutbox(_t) => {
            (updateReadMonoForumOutbox::ID, "updateReadMonoForumOutbox")
        }
        Update::updateMonoForumNoPaidException(_t) => (
            updateMonoForumNoPaidException::ID,
            "updateMonoForumNoPaidException",
        ),
        Update::updateGroupCallMessage(_t) => {
            (updateGroupCallMessage::ID, "updateGroupCallMessage")
        }
        Update::updateGroupCallEncryptedMessage(_t) => (
            updateGroupCallEncryptedMessage::ID,
            "updateGroupCallEncryptedMessage",
        ),
        Update::updatePinnedForumTopic(_t) => {
            (updatePinnedForumTopic::ID, "updatePinnedForumTopic")
        }
        Update::updatePinnedForumTopics(_t) => {
            (updatePinnedForumTopics::ID, "updatePinnedForumTopics")
        }
        Update::updateDeleteGroupCallMessages(_t) => (
            updateDeleteGroupCallMessages::ID,
            "updateDeleteGroupCallMessages",
        ),
        Update::updateStarGiftAuctionState(_t) => {
            (updateStarGiftAuctionState::ID, "updateStarGiftAuctionState")
        }
        Update::updateStarGiftAuctionUserState(_t) => (
            updateStarGiftAuctionUserState::ID,
            "updateStarGiftAuctionUserState",
        ),
        Update::updateEmojiGameInfo(_t) => (updateEmojiGameInfo::ID, "updateEmojiGameInfo"),
        Update::updateStarGiftCraftFail(_t) => {
            (updateStarGiftCraftFail::ID, "updateStarGiftCraftFail")
        }
        Update::updateChatParticipantRank(_t) => {
            (updateChatParticipantRank::ID, "updateChatParticipantRank")
        }
        Update::updateManagedBot(_t) => (updateManagedBot::ID, "updateManagedBot"),
        Update::updateBotGuestChatQuery(_t) => {
            (updateBotGuestChatQuery::ID, "updateBotGuestChatQuery")
        }
        Update::updateAiComposeTones(_t) => (updateAiComposeTones::ID, "updateAiComposeTones"),
        Update::updateJoinChatWebViewDecision(_t) => (
            updateJoinChatWebViewDecision::ID,
            "updateJoinChatWebViewDecision",
        ),
        Update::updateNewBotConnection(_t) => {
            (updateNewBotConnection::ID, "updateNewBotConnection")
        }
        Update::updateWebBrowserSettings(_t) => {
            (updateWebBrowserSettings::ID, "updateWebBrowserSettings")
        }
        Update::updateWebBrowserException(_t) => {
            (updateWebBrowserException::ID, "updateWebBrowserException")
        }
        Update::updateNewEphemeralMessage(_t) => {
            (updateNewEphemeralMessage::ID, "updateNewEphemeralMessage")
        }
        Update::updateDeleteEphemeralMessages(_t) => (
            updateDeleteEphemeralMessages::ID,
            "updateDeleteEphemeralMessages",
        ),
        Update::updateEditEphemeralMessage(_t) => {
            (updateEditEphemeralMessage::ID, "updateEditEphemeralMessage")
        }
        Update::updateEphemeralBotCallbackQuery(_t) => (
            updateEphemeralBotCallbackQuery::ID,
            "updateEphemeralBotCallbackQuery",
        ),
        Update::updateBotStarsSubscription(_t) => {
            (updateBotStarsSubscription::ID, "updateBotStarsSubscription")
        }
        Update::updateWalletState(_t) => (updateWalletState::ID, "updateWalletState"),
        Update::updateWalletTonConnectSession(_t) => (
            updateWalletTonConnectSession::ID,
            "updateWalletTonConnectSession",
        ),
        Update::updateWalletTonConnectPendingDisconnect(_t) => (
            updateWalletTonConnectPendingDisconnect::ID,
            "updateWalletTonConnectPendingDisconnect",
        ),
        Update::updateSentWalletTransaction(_t) => (
            updateSentWalletTransaction::ID,
            "updateSentWalletTransaction",
        ),
        Update::updateWalletGaslessInfo(_t) => {
            (updateWalletGaslessInfo::ID, "updateWalletGaslessInfo")
        }
    }
}

/// The queue element: a wire update or one of the C++ synthetic updates.
/// The synthetics (h:42 `dummyUpdate` ID 1234567891, h:63
/// `updateSentMessage` ID 1234567890) subclass `telegram_api::Update` in
/// C++; the Rust wrapper keeps their ids out of any serialized form.
// TODO(M.7.6): Box the Wire variant (the C++ queues hold pointer-sized
// updates); it restructures the dispatcher/table patterns, so it lands
// with the on_get_difference rewrite.
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum PendingUpdate {
    Wire(telegram_api::Update),
    /// `dummyUpdate` — a pts-only bookkeeping update (empty
    /// updateDeleteMessages, the updateWebPage/updateFolderPeers pts
    /// tails).
    Dummy,
    /// `updateSentMessage` — synthesized by the send path (the
    /// MessagesManager slice).
    SentMessage {
        random_id: i64,
        message_id: i32,
        date: i32,
        ttl_period: i32,
    },
}

impl PendingUpdate {
    /// The C++ `get_id()` — synthetic ids included (comparison-only; the
    /// ids are never serialized).
    pub fn get_id(&self) -> i32 {
        match self {
            PendingUpdate::Wire(update) => update_id_and_name(update).0,
            PendingUpdate::Dummy => 1_234_567_891,
            PendingUpdate::SentMessage { .. } => 1_234_567_890,
        }
    }

    /// The scheme name for logs and the unsupported-update counter.
    pub fn name(&self) -> &'static str {
        match self {
            PendingUpdate::Wire(update) => update_id_and_name(update).1,
            PendingUpdate::Dummy => "dummyUpdate",
            PendingUpdate::SentMessage { .. } => "updateSentMessage",
        }
    }

    /// `to_string(update)` for the `Receive from … pending …` logs
    /// (UpdatesManager.cpp:2705/:3038) — the TL-text dump for wire updates;
    /// the synthetic arms never reach the C++ log sites.
    pub fn dump_string(&self) -> String {
        match self {
            PendingUpdate::Wire(update) => update.to_debug_string(),
            PendingUpdate::Dummy => "dummyUpdate".to_string(),
            PendingUpdate::SentMessage { .. } => "updateSentMessage".to_string(),
        }
    }
}

impl From<telegram_api::Update> for PendingUpdate {
    fn from(update: telegram_api::Update) -> Self {
        PendingUpdate::Wire(update)
    }
}

impl PendingUpdate {
    /// The wire update, if not synthetic.
    pub fn wire(&self) -> Option<&telegram_api::Update> {
        match self {
            PendingUpdate::Wire(update) => Some(update),
            _ => None,
        }
    }
}

/// The manager fan-out — the C++ `td_->…` calls, as a trait until each
/// manager slice lands (the M.7.12–M.7.15 flips enumerate every arm).
pub trait UpdatesSink {
    fn on_user(&mut self, user: telegram_api::User);
    fn on_chat(&mut self, chat: telegram_api::Chat);
    /// `on_update(updateWalletState)` — the TonWalletManager state intake
    /// (cpp:5124-5127); the default no-op keeps the test sinks inert.
    fn on_update_wallet_state(&mut self, _state: telegram_api::WalletState) {}
    /// `on_update(updateWalletGaslessInfo)` (cpp:5129-5131).
    fn on_update_wallet_gasless_info(&mut self, _info: telegram_api::updateWalletGaslessInfo) {}
    /// `on_update(updateWalletTonConnectSession)` (cpp:5138-5140).
    fn on_update_wallet_ton_connect_session(&mut self, _session: telegram_api::TonConnectSession) {}
    /// `on_update(updateWalletTonConnectPendingDisconnect)` (cpp:5142-5147).
    fn on_update_wallet_ton_connect_disconnect(&mut self, _session_ids: Vec<i64>) {}
    /// `MessagesManager::process_pts_update` — the applied-pts-update
    /// switch (the M.7.13 flip; the impl fans out per update kind).
    fn process_pts_update(&mut self, update: PendingUpdate);
    /// `MessagesManager::skip_old_pending_pts_update`.
    fn skip_old_pending_pts_update(
        &mut self,
        update: PendingUpdate,
        new_pts: i32,
        old_pts: i32,
        pts_count: i32,
        source: &str,
    ) {
        let _ = (update, new_pts, old_pts, pts_count, source);
    }
    /// `MessagesManager::add_pending_channel_update` — the channel-pts
    /// machine (M.5/MessagesManager flip); `None` is the invalid dialog
    /// (`messageEmpty`).
    fn add_pending_channel_update(
        &mut self,
        peer: Option<telegram_api::Peer>,
        update: PendingUpdate,
        pts: i32,
        pts_count: i32,
        promise: Promise<()>,
        source: &'static str,
    );
    /// `MessagesManager::on_update_message_id`; returns whether the
    /// random_id was known (C++ logs the container again on `false`).
    fn on_update_message_id(
        &mut self,
        random_id: i64,
        id: i32,
        is_scheduled: bool,
        source: &str,
    ) -> bool {
        let _ = (random_id, id, is_scheduled, source);
        true
    }
    /// `MessagesManager::on_update_channel_too_long`.
    fn on_update_channel_too_long(
        &mut self,
        update: telegram_api::updateChannelTooLong,
        force: bool,
    ) {
        let _ = (update, force);
    }
    /// `UserManager::on_update_user_local_was_online` (the single
    /// updateReadHistoryOutbox fast path).
    fn on_update_user_local_was_online(&mut self, user_id: i64, date: i32) {
        let _ = (user_id, date);
    }
    /// `MessagesManager::on_get_message` for updateNewScheduledMessage.
    fn on_get_message_scheduled(&mut self, message: telegram_api::Message) {
        let _ = message;
    }
    /// `SecretChatsManager::on_new_message` — the updateNewEncryptedMessage
    /// qts intake and the getDifference encrypted-messages intake (Wave 3).
    /// The qts ack is synchronous in the engine (the module docs), so the
    /// C++ ack-promise argument collapses away.
    fn on_new_encrypted_message(&mut self, message: telegram_api::EncryptedMessage) {
        let _ = message;
    }
    /// `MessagesManager::on_get_message` for the getDifference
    /// new-messages vector (`via get difference`; the M.7 MessagesManager
    /// flip).
    fn on_get_difference_message(&mut self, message: telegram_api::Message) {
        let _ = message;
    }
    /// `MessagesManager::is_invalid_poll_message` — the
    /// reget-difference trigger for unparseable polls (the M.7
    /// MessagesManager flip).
    fn is_invalid_poll_message(&mut self, message: &telegram_api::Message) -> bool {
        let _ = message;
        false
    }
    /// `ConfigManager::request_config(false)` + the promo-data reload.
    fn on_update_config(&mut self) {}
    /// `ConfigManager::on_dc_options_update`.
    fn on_dc_options_update(&mut self, _dc_options: Vec<telegram_api::DcOption>) {}
    /// `UpdatesManager::on_update(updateTranscribedAudio)` — the
    /// TranscriptionManager intake landed with M.25
    /// (`TranscriptionManager::on_update_transcribed_audio`); the sink
    /// default parks the intake until the sink can reach the manager
    /// (the live update flow arrives with the net step).
    fn on_update_transcribed_audio(&mut self, _update: telegram_api::updateTranscribedAudio) {}
    /// `AuthManager::on_update_login_token` (the pre-auth short update).
    fn on_update_login_token(&mut self) {}
    /// `AuthManager::on_update_sent_code` (the pre-auth short update).
    fn on_update_sent_code(&mut self, _sent_code: telegram_api::auth_SentCode) {}
    /// The `before_get_difference` manager fan-out
    /// (`StateManager::on_synchronized(false)` +
    /// `MessagesManager::before_get_difference` + the notification-manager
    /// phase; M.7.18 wiring).
    fn on_before_get_difference(&mut self) {}
    /// The `after_get_difference` manager fan-out (download/inline/
    /// messages/notification managers + state sync; M.7.18 wiring).
    fn on_after_get_difference(&mut self) {}
    /// An update whose manager slice has not landed: the enumerated
    /// trace-drop arm (M.7.12/M.7.15 flip the real intakes).
    fn on_unsupported_update(&mut self, id: i32, name: &str, cpp_line: u32) {
        tdutils::log_error!(
            "Unsupported update {} (id {}) dropped (cpp:{})",
            name,
            id,
            cpp_line
        );
    }
    /// `Td::send_update(updateSpeedLimitNotification)` — the
    /// speed-limit notice out to the client (the M.7.16 aux seam).
    fn on_speed_limit_notification(&mut self, is_upload: bool) {
        let _ = is_upload;
    }
    /// `UpdatesManager::try_reload_data`'s fan-out — the C++ makes ~45
    /// saved-config reload calls (cpp:2418-2462): saved animations,
    /// autosave settings, the created public dialogs trio, the default
    /// emoji statuses quartet, paid-reaction privacy, saved ringtones +
    /// the scope notification settings trio, quick-reply shortcuts,
    /// reactions + message effects + the reaction-list family, owned
    /// star/ton counts, the installed/featured/recent/favorite sticker
    /// families + the special sticker sets, the default custom-emoji
    /// stickers + the sticker-list emoji statuses, accent colors, chat
    /// themes, profile accent colors, time zones, AI compose tones,
    /// contact birthdates, the saved-music list and web browser settings.
    /// The calls land here as their manager slices land.
    fn reload_data(&mut self) {}
    /// `UpdatesManager::try_reload_often_data`'s fan-out — the stories
    /// reload pair (cpp:2520-2522).
    fn reload_often_data(&mut self) {}

    // ---- M.7.10 — the acceptability filter's manager asks (cpp:715-731).
    // The defaults mirror an empty cache (unknown ⇒ unacceptable); the
    // td-level impl asks the landed managers.

    /// `UserManager::have_user_force && have_accessible_user`.
    fn is_acceptable_user(&mut self, user_id: i64) -> bool {
        let _ = user_id;
        false
    }
    /// `ChatManager::have_chat_force` — M.5 flip.
    fn is_acceptable_chat(&mut self, chat_id: i64) -> bool {
        let _ = chat_id;
        false
    }
    /// `ChatManager::have_channel_force` — M.5 flip.
    fn is_acceptable_channel(&mut self, channel_id: i64) -> bool {
        let _ = channel_id;
        false
    }
    /// `CommunityManager::have_community_force &&
    /// have_accessible_community` — Wave 2.
    fn is_acceptable_community(&mut self, community_id: i64) -> bool {
        let _ = community_id;
        false
    }
    /// the negated `UserManager::get_input_user(...).is_error()` of the
    /// mention-name/button checks (cpp:771/788).
    fn can_get_input_user(&mut self, user_id: i64) -> bool {
        let _ = user_id;
        false
    }
    /// `MessagesManager::is_old_channel_update` — the unacceptable-update
    /// replacement ladder (cpp:2607); M.7 MessagesManager flip.
    fn is_old_channel_update(&mut self, channel_id: i64, pts: i32) -> bool {
        let _ = (channel_id, pts);
        false
    }

    // ---- M.7.12 — the real manager groups (UserManager is landed; the
    // td-level sink impl calls into user_manager with its env, the M.3.9
    // manager convention).

    /// `UserManager::on_update_user_online` (cpp:4364).
    fn on_update_user_status(&mut self, user_id: i64, status: telegram_api::UserStatus) {
        let _ = (user_id, status);
    }
    /// `WebBrowserManager::on_update_web_browser_settings` (M.31) — the
    /// td-level sink routes to the web browser manager; the ZST stand-in
    /// no-ops until the sink-state refactor.
    fn on_update_web_browser_settings(&mut self, update: telegram_api::updateWebBrowserSettings) {
        let _ = update;
    }
    /// `WebBrowserManager::on_update_web_browser_exception` (M.31).
    fn on_update_web_browser_exception(&mut self, update: telegram_api::updateWebBrowserException) {
        let _ = update;
    }
    /// `CallManager::update_call` (M.32) — the td-level sink routes the
    /// updatePhoneCall intake to the call manager; the ZST stand-in
    /// no-ops until the sink-state refactor.
    fn on_update_phone_call(&mut self, update: telegram_api::updatePhoneCall) {
        let _ = update;
    }
    /// `CallManager::update_call_signaling_data` (M.32).
    fn on_update_phone_call_signaling_data(&mut self, call_id: i64, data: Vec<u8>) {
        let _ = (call_id, data);
    }
    /// `UserManager::on_update_user_name` (cpp:4369).
    fn on_update_user_name(&mut self, user_id: i64, first_name: String, last_name: String) {
        let _ = (user_id, first_name, last_name);
    }
    /// `UserManager::on_update_user_usernames` — the empty-editable
    /// constructor of cpp:4372.
    fn on_update_user_usernames(&mut self, user_id: i64, usernames: crate::usernames::Usernames) {
        let _ = (user_id, usernames);
    }
    /// `UserManager::on_update_user_phone_number` (cpp:4377).
    fn on_update_user_phone(&mut self, user_id: i64, phone: String) {
        let _ = (user_id, phone);
    }
    /// `UserManager::invalidate_user_full` (cpp:4382).
    fn invalidate_user_full(&mut self, user_id: i64) {
        let _ = user_id;
    }
    /// `UserManager::on_update_user_emoji_status` (cpp:4387); the wire
    /// `emojiStatusEmpty` is the None of the manager intake.
    fn on_update_user_emoji_status(
        &mut self,
        user_id: i64,
        emoji_status: telegram_api::EmojiStatus,
    ) {
        let _ = (user_id, emoji_status);
    }
    /// `UserManager::on_update_contacts_reset` (cpp:4813).
    fn on_update_contacts_reset(&mut self) {}

    // ---- M.7.13 — the message-family seams (messages_manager; the
    // td-level sink impl fans each out when that slice lands). The wire
    // struct travels whole, exactly like `on_update_channel_too_long`.

    /// `MessagesManager::on_update_service_notification` (cpp:3834).
    /// `LanguagePackManager::on_language_pack_too_long`
    /// (UpdatesManager.cpp:4818-4823, M.24). The manager-local halves are
    /// landed and tested on [`crate::language_pack_manager`]; this sink
    /// impl routes into Td's `language_pack_manager` with the net step
    /// (the same collapsed-sink convention as the MessagesManager
    /// intakes).
    fn on_update_lang_pack_too_long(&mut self, _lang_code: String) {}

    /// `LanguagePackManager::on_update_language_pack`
    /// (UpdatesManager.cpp:4824-4829, M.24).
    fn on_update_lang_pack(&mut self, _difference: telegram_api::LangPackDifference) {}

    fn on_update_service_notification(&mut self, _update: telegram_api::updateServiceNotification) {
    }
    /// `MessagesManager::on_update_read_channel_inbox` (cpp:3844).
    fn on_update_read_channel_inbox(&mut self, _update: telegram_api::updateReadChannelInbox) {}
    /// `MessagesManager::on_update_read_channel_outbox` (cpp:3849).
    fn on_update_read_channel_outbox(&mut self, _update: telegram_api::updateReadChannelOutbox) {}
    /// `MessagesManager::on_update_read_channel_messages_contents`
    /// (cpp:3854).
    fn on_update_read_channel_messages_contents(
        &mut self,
        _update: telegram_api::updateChannelReadMessagesContents,
    ) {
    }
    /// `MessagesManager::on_update_channel_max_unavailable_message_id`
    /// (cpp:3912); the engine validated the channel id.
    fn on_update_channel_max_unavailable_message_id(
        &mut self,
        _update: telegram_api::updateChannelAvailableMessages,
    ) {
    }
    /// `MessagesManager::on_update_dialog_view_as_messages` (cpp:3920).
    fn on_update_dialog_view_as_messages(
        &mut self,
        _update: telegram_api::updateChannelViewForumAsMessages,
    ) {
    }
    /// `MessagesManager::on_update_read_message_comments` — the inbox
    /// half, incl. the broadcast-post echo (cpp:3926).
    fn on_update_read_channel_discussion_inbox(
        &mut self,
        _update: telegram_api::updateReadChannelDiscussionInbox,
    ) {
    }
    /// `MessagesManager::on_update_read_message_comments` — the outbox
    /// half (cpp:3944).
    fn on_update_read_channel_discussion_outbox(
        &mut self,
        _update: telegram_api::updateReadChannelDiscussionOutbox,
    ) {
    }
    /// `MessagesManager::on_get_peer_settings` (cpp:4070).
    fn on_get_peer_settings(&mut self, _update: telegram_api::updatePeerSettings) {}
    /// `MessagesManager::on_update_dialog_message_ttl` (cpp:4075).
    fn on_update_dialog_message_ttl(&mut self, _update: telegram_api::updatePeerHistoryTTL) {}
    /// `MessagesManager::on_update_message_view_count` (cpp:3887); the
    /// engine validated the channel id.
    fn on_update_message_view_count(&mut self, _update: telegram_api::updateChannelMessageViews) {}
    /// `MessagesManager::on_update_message_forward_count` (cpp:3899).
    fn on_update_message_forward_count(
        &mut self,
        _update: telegram_api::updateChannelMessageForwards,
    ) {
    }
    /// `MessagesManager::on_update_live_location_viewed` (cpp:4830).
    fn on_update_live_location_viewed(&mut self, _update: telegram_api::updateGeoLiveViewed) {}
    /// `MessagesManager::on_update_message_extended_media` (cpp:4836).
    fn on_update_message_extended_media(
        &mut self,
        _update: telegram_api::updateMessageExtendedMedia,
    ) {
    }
    /// `MessagesManager::on_update_message_reactions` (cpp:4101); the
    /// C++ passes the apply promise through — the synchronous seam
    /// completes it engine-side.
    fn on_update_message_reactions(&mut self, _update: telegram_api::updateMessageReactions) {}
    /// `MessagesManager::on_update_dialog_pending_join_requests`
    /// (cpp:4962).
    fn on_update_dialog_pending_join_requests(
        &mut self,
        _update: telegram_api::updatePendingJoinRequests,
    ) {
    }
    /// `MessagesManager::on_update_dialog_is_blocked` (cpp:4397).
    fn on_update_dialog_is_blocked(&mut self, _update: telegram_api::updatePeerBlocked) {}
    /// `MessagesManager::on_new_ephemeral_message` /
    /// `WelcomeMessageManager::on_new_welcome_message` — the
    /// `welcome_template` flag picks the target (cpp:3792); the flag is
    /// preserved for the flip.
    fn on_new_ephemeral_message(
        &mut self,
        _message: telegram_api::EphemeralMessage,
        is_welcome: bool,
    ) {
        let _ = is_welcome;
    }
    /// `MessagesManager::on_edited_ephemeral_message` /
    /// `WelcomeMessageManager::on_edited_welcome_message` (cpp:3801).
    fn on_edited_ephemeral_message(
        &mut self,
        _message: telegram_api::EphemeralMessage,
        is_welcome: bool,
    ) {
        let _ = is_welcome;
    }
    /// `MessagesManager::on_delete_ephemeral_messages` +
    /// `WelcomeMessageManager::on_delete_welcome_messages` (cpp:3811);
    /// the ids are the raw wire values.
    fn on_delete_ephemeral_messages(&mut self, _peer: telegram_api::Peer, _ids: Vec<i32>) {}

    // ---- M.7.14 — the dialog-family seams (messages_manager pair for the
    // drafts; the pinned/marked trio; flips when M.6 lands).

    /// `MessagesManager::on_update_dialog_draft_message` /
    /// `SavedMessagesManager::on_update_topic_draft_message` — the
    /// `saved_peer_id` branch is recorded in the payload (cpp:4468).
    fn on_update_dialog_draft_message(&mut self, _update: telegram_api::updateDraftMessage) {}
    /// `MessagesManager::on_update_dialog_is_pinned` (cpp:4480).
    fn on_update_dialog_is_pinned(&mut self, _update: telegram_api::updateDialogPinned) {}
    /// `MessagesManager::on_update_pinned_dialogs` (cpp:4486); the C++
    /// ignores `order_`.
    fn on_update_pinned_dialogs(&mut self, _update: telegram_api::updatePinnedDialogs) {}
    /// `MessagesManager::on_update_dialog_is_marked_as_unread` /
    /// `SavedMessagesManager::on_update_topic_is_marked_as_unread`
    /// (cpp:4501).
    fn on_update_dialog_is_marked_as_unread(
        &mut self,
        _update: telegram_api::updateDialogUnreadMark,
    ) {
    }
}

/// The `G()->`/`td_->` environment the engine runs against: the clock,
/// options, the binlog pmc, the authorization state, and the
/// `[blocked: net]` query senders (the S74 harness drives the response
/// intakes directly).
pub trait UpdatesManagerEnv {
    /// `Time::now()` — the process-monotonic clock.
    fn now(&mut self) -> f64;
    /// `G()->unix_time()`.
    fn unix_time(&mut self) -> i32;
    /// `G()->close_flag()`.
    fn close_flag(&mut self) -> bool;
    /// `td_->auth_manager_->is_authorized()`.
    fn is_authorized(&mut self) -> bool;
    /// `td_->auth_manager_->is_bot()`.
    fn is_bot(&mut self) -> bool;
    /// `td_->ignore_background_updates()`.
    fn ignore_background_updates(&mut self) -> bool;
    /// `td_->can_ignore_background_updates()`.
    fn can_ignore_background_updates(&mut self) -> bool;
    /// `td_->option_manager_->get_option_integer(name)`.
    fn get_option_integer(&mut self, name: &str) -> i64;
    /// `td_->option_manager_->set_option_empty(name)`.
    fn set_option_empty(&mut self, name: &str);
    /// `G()->td_db()->get_binlog_pmc()->get(key)`.
    fn pmc_get(&mut self, key: &str) -> String;
    /// `G()->td_db()->get_binlog_pmc()->set(key, value)`.
    fn pmc_set(&mut self, key: &str, value: &str);
    /// `G()->td_db()->get_binlog_pmc()->erase(key)`.
    fn pmc_erase(&mut self, key: &str);
    /// `td_->user_manager_->get_my_id()` (0 while unknown).
    fn get_my_id(&mut self) -> i64;
    /// The `GetDifferenceQuery` — `[blocked: net]`; records the request.
    fn send_get_difference(&mut self, pts: i32, date: i32, qts: i32);
    /// The `GetUpdatesStateQuery` — `[blocked: net]`.
    fn send_get_updates_state(&mut self);
    /// The `ConfirmPtsQtsQuery` — `[blocked: net]`.
    fn send_confirm_pts_qts(&mut self, pts: i32, qts: i32);
    /// The `GetPtsUpdateQuery` (the pts-short-gap repair) —
    /// `[blocked: net]`.
    fn send_get_pts_update(&mut self, pts: i32);
    /// The `PingServerQuery` — `[blocked: net]`; the response intake is
    /// [`UpdatesManager::on_server_pong`].
    fn send_ping_server(&mut self);
    /// `td_->online_manager_->is_online()` (the data-reload precondition).
    fn is_online(&mut self) -> bool;
}

/// `td::PtsManager` — the db/mem pts pair plus the ordered in-flight
/// changes (`ChangesProcessor<int32>`: a `BTreeMap` keyed by the add id;
/// `finish` folds every finished prefix into `db_pts`).
#[derive(Debug)]
pub struct PtsManager {
    db_pts: i32,
    mem_pts: i32,
    changes: BTreeMap<u64, i32>,
    next_id: u64,
}

impl PtsManager {
    /// C++ `db_pts_ = -1; mem_pts_ = -1;` (PtsManager.h:53-54) — -1 is the
    /// "unknown" sentinel that routes `get_difference` through the
    /// getState-first init; a 0 default sent `getDifference(pts=0)` on
    /// fresh sessions (PERSISTENT_TIMESTAMP_EMPTY, the fill-gap loop).
    fn new_unknown() -> Self {
        Self {
            db_pts: -1,
            mem_pts: -1,
            changes: BTreeMap::new(),
            next_id: 0,
        }
    }
}

impl PtsManager {
    pub fn init(&mut self, pts: i32) {
        self.db_pts = pts;
        self.mem_pts = pts;
        self.changes.clear();
        self.next_id = 0;
    }

    /// `PtsManager::add_pts` — 0 when not a checkpoint.
    pub fn add_pts(&mut self, pts: i32) -> u64 {
        if pts > 0 {
            self.mem_pts = pts;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.changes.insert(id, pts);
        id
    }

    /// `PtsManager::finish` — the finished prefix folds into `db_pts`
    /// (the C++ `state_helper_.finish(pts_id, cb)`).
    pub fn finish(&mut self, pts_id: u64) -> i32 {
        let ids: Vec<u64> = self
            .changes
            .keys()
            .take_while(|id| **id <= pts_id)
            .copied()
            .collect();
        for id in ids {
            let pts = self.changes.remove(&id).expect("the id exists");
            if pts != 0 {
                self.db_pts = pts;
            }
        }
        self.db_pts
    }

    pub fn db_pts(&self) -> i32 {
        self.db_pts
    }

    pub fn mem_pts(&self) -> i32 {
        self.mem_pts
    }
}

/// A `td::Timeout` deadline (`td/actor/Timeout.h`) for the non-actor
/// engine: `tick_timeouts` fires the deadlines; `get_timeout` returns the
/// delay as set, matching `td::Timeout::get_timeout()`.
#[derive(Debug, Default, Clone, Copy)]
struct Timeout {
    deadline: Option<f64>,
    set_delay: f64,
}

impl Timeout {
    fn has_timeout(&self) -> bool {
        self.deadline.is_some()
    }

    fn get_timeout(&self) -> f64 {
        self.set_delay
    }

    fn set_timeout_in(&mut self, now: f64, delay: f64) {
        self.set_delay = delay;
        self.deadline = Some(now + delay);
    }

    fn cancel_timeout(&mut self) {
        self.deadline = None;
        self.set_delay = 0.0;
    }
}

/// `UpdatesManager::PendingPtsUpdate` (h:188) — the C++ `multiset` order
/// is pts ascending, then pts_count descending.
struct PendingPtsUpdate {
    update: PendingUpdate,
    pts: i32,
    pts_count: i32,
    receive_time: f64,
    promise: Promise<()>,
}

fn pending_pts_order(a: &PendingPtsUpdate, b: &PendingPtsUpdate) -> std::cmp::Ordering {
    a.pts.cmp(&b.pts).then(b.pts_count.cmp(&a.pts_count))
}

/// The multiset insert (duplicates allowed) at the C++ ordering position.
fn insert_pending_pts(queue: &mut Vec<PendingPtsUpdate>, update: PendingPtsUpdate) {
    let pos = queue.partition_point(|existing| {
        pending_pts_order(existing, &update) != std::cmp::Ordering::Greater
    });
    queue.insert(pos, update);
}

/// `UpdatesManager::PendingSeqUpdates` (h:213) — the C++ `multiset` order
/// is seq_begin ascending, then seq_end descending.
struct PendingSeqUpdates {
    seq_begin: i32,
    seq_end: i32,
    date: i32,
    receive_time: f64,
    updates: Vec<Option<PendingUpdate>>,
    promise: Option<Promise<()>>,
}

fn pending_seq_order(a: &PendingSeqUpdates, b: &PendingSeqUpdates) -> std::cmp::Ordering {
    a.seq_begin
        .cmp(&b.seq_begin)
        .then(b.seq_end.cmp(&a.seq_end))
}

fn insert_pending_seq(queue: &mut Vec<PendingSeqUpdates>, entry: PendingSeqUpdates) {
    let pos = queue.partition_point(|existing| {
        pending_seq_order(existing, &entry) != std::cmp::Ordering::Greater
    });
    queue.insert(pos, entry);
}

/// `UpdatesManager::PendingQtsUpdate` (h:240) — a `std::map` value keyed
/// by qts.
#[derive(Default)]
struct PendingQtsUpdate {
    receive_time: f64,
    update: Option<PendingUpdate>,
    promises: Vec<Promise<()>>,
}

/// `UpdatesManager::SessionInfo` (h:316) — the per-auth-key update
/// statistics the auth-key reset path maintains.
#[derive(Debug, Default, Clone, Copy)]
struct SessionInfo {
    first_update_time: f64,
    last_update_time: f64,
    update_count: i32,
}

/// `td::UpdatesManager` — the engine core. The sink and environment are
/// injected per call site (the `&mut env` manager convention), so the
/// struct carries only the engine state.
pub struct UpdatesManager<S: UpdatesSink> {
    pts_manager: PtsManager,
    qts_manager: PtsManager,
    date: i32,
    seq: i32,
    date_source: String,

    last_pts_save_time: f64,
    last_qts_save_time: f64,
    pending_pts: i32,
    pending_qts: i32,

    pts_short_gap: i32,
    pts_fixed_short_gap: i32,
    pts_gap: i32,
    pts_diff: i32,
    last_fetched_pts: i32,

    qts_gap: i32,
    qts_diff: i32,

    short_update_date: i32,

    accumulated_pts_count: i32,
    accumulated_pts: i32,
    last_pts_jump_warning_time: f64,
    last_pts_gap_time: f64,

    pending_pts_updates: Vec<PendingPtsUpdate>,
    postponed_pts_updates: Vec<PendingPtsUpdate>,

    postponed_updates: Vec<PendingSeqUpdates>,
    pending_seq_updates: Vec<PendingSeqUpdates>,

    pending_qts_updates: BTreeMap<i32, PendingQtsUpdate>,

    min_pts_gap_timeout: Timeout,
    pts_gap_timeout: Timeout,
    seq_gap_timeout: Timeout,
    qts_gap_timeout: Timeout,

    retry_time: i32,
    retry_timeout: Timeout,

    // The actor-level save-flush timeout (`set_timeout_in` for the
    // throttled pts/qts pmc writes); `timeout_expired` flushes both.
    save_timeout: Timeout,

    expect_pts_gap: bool,
    running_get_difference: bool,
    skipped_postponed_updates_after_start: i32,
    last_confirmed_pts: i32,
    last_confirmed_qts: i32,
    min_postponed_update_pts: i32,
    min_postponed_update_qts: i32,
    get_difference_start_time: f64,
    // Read by on_get_difference's retry bookkeeping (M.7.6).
    #[allow(dead_code)]
    get_difference_retry_count: i32,

    // M.7.16 — the aux machinery state (cpp:1908-1957).
    is_ping_sent: bool,
    session_infos: BTreeMap<u64, SessionInfo>,
    next_notify_speed_limited: [f64; 2],

    // M.7.17 — the data reloaders (cpp:2373-2534).
    next_data_reload_time: f64,
    data_reload_timeout: Timeout,
    next_often_data_reload_time: f64,
    often_data_reload_timeout: Timeout,

    sink: S,
}

impl<S: UpdatesSink> UpdatesManager<S> {
    pub fn new(sink: S) -> Self {
        Self {
            pts_manager: PtsManager::new_unknown(),
            qts_manager: PtsManager::new_unknown(),
            date: 0,
            seq: 0,
            date_source: "nowhere".to_string(),
            last_pts_save_time: 0.0,
            last_qts_save_time: 0.0,
            pending_pts: 0,
            pending_qts: 0,
            pts_short_gap: 0,
            pts_fixed_short_gap: 0,
            pts_gap: 0,
            pts_diff: 0,
            last_fetched_pts: 0,
            qts_gap: 0,
            qts_diff: 0,
            short_update_date: 0,
            accumulated_pts_count: 0,
            accumulated_pts: -1,
            last_pts_jump_warning_time: 0.0,
            last_pts_gap_time: 0.0,
            pending_pts_updates: Vec::new(),
            postponed_pts_updates: Vec::new(),
            postponed_updates: Vec::new(),
            pending_seq_updates: Vec::new(),
            pending_qts_updates: BTreeMap::new(),
            min_pts_gap_timeout: Timeout::default(),
            pts_gap_timeout: Timeout::default(),
            seq_gap_timeout: Timeout::default(),
            qts_gap_timeout: Timeout::default(),
            retry_time: 1,
            retry_timeout: Timeout::default(),
            save_timeout: Timeout::default(),
            expect_pts_gap: false,
            running_get_difference: false,
            skipped_postponed_updates_after_start: SKIPPED_POSTPONED_UPDATES_AFTER_START,
            last_confirmed_pts: 0,
            last_confirmed_qts: 0,
            min_postponed_update_pts: 0,
            min_postponed_update_qts: 0,
            get_difference_start_time: 0.0,
            get_difference_retry_count: 0,
            next_data_reload_time: 0.0,
            data_reload_timeout: Timeout::default(),
            next_often_data_reload_time: 0.0,
            often_data_reload_timeout: Timeout::default(),
            is_ping_sent: false,
            session_infos: BTreeMap::new(),
            next_notify_speed_limited: [0.0; 2],
            sink,
        }
    }

    pub fn sink(&mut self) -> &mut S {
        &mut self.sink
    }

    fn get_pts(&self) -> i32 {
        self.pts_manager.mem_pts()
    }

    fn get_qts(&self) -> i32 {
        self.qts_manager.mem_pts()
    }

    pub fn get_date(&self) -> i32 {
        self.date
    }

    pub fn get_seq(&self) -> i32 {
        self.seq
    }

    pub fn running_get_difference(&self) -> bool {
        self.running_get_difference
    }

    pub fn get_pending_pts_update_count(&self) -> usize {
        self.postponed_pts_updates.len() + self.pending_pts_updates.len()
    }

    // ------------------------------------------------------------------
    // M.7.1 — state, persistence, set_pts/set_date
    // ------------------------------------------------------------------

    /// `UpdatesManager::add_pts` (cpp:560) — the pmc write is synchronous
    /// here, so the ack event fires immediately (see the module docs).
    fn add_pts(&mut self, env: &mut impl UpdatesManagerEnv, pts: i32) {
        let id = self.pts_manager.add_pts(pts);
        self.on_pts_ack(env, id);
    }

    /// `UpdatesManager::add_qts` (cpp:565).
    fn add_qts(&mut self, env: &mut impl UpdatesManagerEnv, qts: i32) {
        let id = self.qts_manager.add_pts(qts);
        self.on_qts_ack(env, id);
    }

    /// `UpdatesManager::on_pts_ack` (cpp:570).
    fn on_pts_ack(&mut self, env: &mut impl UpdatesManagerEnv, ack_token: u64) {
        let old_pts = self.pts_manager.db_pts();
        let new_pts = self.pts_manager.finish(ack_token);
        if old_pts != new_pts {
            self.save_pts(env, new_pts);
        }
    }

    /// `UpdatesManager::on_qts_ack` (cpp:578).
    fn on_qts_ack(&mut self, env: &mut impl UpdatesManagerEnv, ack_token: u64) {
        let old_qts = self.qts_manager.db_pts();
        let new_qts = self.qts_manager.finish(ack_token);
        if old_qts != new_qts {
            self.save_qts(env, new_qts);
        }
    }

    /// `UpdatesManager::save_pts` (cpp:586) — the 0.05 s bot throttle; an
    /// `i32::MAX` pts erases the stored value.
    fn save_pts(&mut self, env: &mut impl UpdatesManagerEnv, pts: i32) {
        if pts == i32::MAX {
            env.pmc_erase("updates.pts");
            self.last_pts_save_time -= 2.0 * MAX_PTS_SAVE_DELAY;
            self.pending_pts = 0;
        } else if !env.ignore_background_updates() {
            let now = env.now();
            let delay = self.last_pts_save_time + MAX_PTS_SAVE_DELAY - now;
            if delay <= 0.0 || !env.is_bot() {
                self.last_pts_save_time = now;
                self.pending_pts = 0;
                env.pmc_set("updates.pts", &pts.to_string());
            } else {
                self.pending_pts = pts;
                if !self.save_timeout.has_timeout() {
                    self.save_timeout.set_timeout_in(now, delay);
                }
            }
        }
    }

    /// `UpdatesManager::save_qts` (cpp:607).
    fn save_qts(&mut self, env: &mut impl UpdatesManagerEnv, qts: i32) {
        if !env.ignore_background_updates() {
            let now = env.now();
            let delay = self.last_qts_save_time + MAX_PTS_SAVE_DELAY - now;
            if delay <= 0.0 || !env.is_bot() {
                self.last_qts_save_time = now;
                self.pending_qts = 0;
                env.pmc_set("updates.qts", &qts.to_string());
            } else {
                self.pending_qts = qts;
                if !self.save_timeout.has_timeout() {
                    self.save_timeout.set_timeout_in(now, delay);
                }
            }
        }
    }

    /// `UpdatesManager::timeout_expired` (cpp:624) — the throttled
    /// pmc save flush; fired from [`Self::tick_timeouts`] and the
    /// close-path flush (the `Td::close_impl` wiring).
    pub(crate) fn timeout_expired(&mut self, env: &mut impl UpdatesManagerEnv) {
        if self.pending_pts != 0 {
            self.last_pts_save_time -= 2.0 * MAX_PTS_SAVE_DELAY;
            let pts = self.pending_pts;
            self.save_pts(env, pts);
            debug_assert_eq!(self.pending_pts, 0, "the throttled pts save must flush");
        }
        if self.pending_qts != 0 {
            self.last_qts_save_time -= 2.0 * MAX_PTS_SAVE_DELAY;
            let qts = self.pending_qts;
            self.save_qts(env, qts);
            debug_assert_eq!(self.pending_qts, 0, "the throttled qts save must flush");
        }
    }

    /// `UpdatesManager::set_pts` (cpp:637) — the full jump/backwards/gap
    /// decision tree. The returned ack promise is synchronous here (the
    /// module docs).
    fn set_pts(&mut self, env: &mut impl UpdatesManagerEnv, pts: i32, source: &str) {
        if pts == i32::MAX {
            tdutils::log_warning!(
                at "UpdatesManager.cpp",
                639,
                "Update PTS from {} to -1 from {}",
                self.get_pts(),
                source
            );
            self.save_pts(env, pts); // drop saved PTS value
            self.add_pts(env, pts);
            self.init_state(env);
            return;
        }
        let old_pts = self.get_pts();
        if pts > old_pts || (0 < pts && pts < old_pts.wrapping_sub(1_000_009)) {
            // PTS can only go up or drop cardinally
            if pts < old_pts.wrapping_sub(1_000_009) {
                tdutils::log_warning!(
                    at "UpdatesManager.cpp",
                    649,
                    "PTS decreases from {} to {} from {}",
                    old_pts,
                    pts,
                    source
                );
            } else {
                tdutils::log_info!(
                    at "UpdatesManager.cpp",
                    651,
                    "Update PTS from {} to {} from {}",
                    old_pts,
                    pts,
                    source
                );
                if old_pts > 0 {
                    self.pts_diff += pts - old_pts;
                    if self.pts_diff >= 1_000_000 {
                        if self.pts_gap > 0 || self.pts_short_gap > 0 {
                            tdutils::log_warning!(
                                "Fixed {} PTS gaps and {} short gaps up to PTS {} by sending {} requests",
                                self.pts_gap,
                                self.pts_fixed_short_gap,
                                pts,
                                self.pts_short_gap
                            );
                            self.pts_short_gap = 0;
                            self.pts_fixed_short_gap = 0;
                            self.pts_gap = 0;
                        }
                        self.pts_diff = 0;
                    }
                }
            }

            self.add_pts(env, pts);
            if self.last_confirmed_pts < self.get_pts() - FORCED_GET_DIFFERENCE_PTS_DIFF
                && self.last_confirmed_pts != 0
            {
                let qts = self.get_qts();
                self.confirm_pts_qts(env, qts);
            }
        } else if pts < self.get_pts() && (pts > 1 || env.get_option_integer("session_count") <= 1)
        {
            tdutils::log_error!(
                "Receive wrong PTS = {} from {}. Current PTS = {}",
                pts,
                source,
                self.get_pts()
            );
        }
    }

    /// `UpdatesManager::set_date` (cpp:677).
    fn set_date(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        date: i32,
        from_update: bool,
        date_source: String,
    ) {
        if date > self.date {
            tdutils::log_info!(
                at "UpdatesManager.cpp",
                679,
                "Update date to {}",
                date
            );
            // C++ keeps a dead `if (from_update && false)` branch here —
            // the date in updates is never decreased by the server.
            let now = env.unix_time();
            if self.date > now + 1 {
                tdutils::log_error!(
                    "Receive wrong by {} date = {}, now = {}, from {}",
                    self.date - now,
                    self.date,
                    now,
                    date_source
                );
                self.date = now;
                if self.date <= date {
                    return;
                }
            }

            self.date = date;
            self.date_source = date_source;
            if !env.ignore_background_updates() {
                env.pmc_set("updates.date", &date.to_string());
            }
        } else if date < self.date {
            let mut date = date;
            if from_update {
                date += 1;

                if date == self.date {
                    return;
                }
            }
            tdutils::log_error!(
                "Receive wrong by {} date = {} from {}. Current date = {} from {}",
                date - self.date,
                date,
                date_source,
                self.date,
                self.date_source
            );
        }
    }

    /// `UpdatesManager::get_short_update_date` (cpp:4146) — the date the
    /// typing-style short updates carry (the C++ arms of the
    /// messages-manager typing fan-out use it).
    pub fn get_short_update_date(&self, env: &mut impl UpdatesManagerEnv) -> i32 {
        let now = env.unix_time();
        if self.short_update_date > 0 {
            return self.short_update_date.min(now);
        }
        now
    }

    /// `UpdatesManager::confirm_pts_qts` (cpp:2270) — the query is the
    /// `[blocked: net]` seam.
    fn confirm_pts_qts(&mut self, env: &mut impl UpdatesManagerEnv, qts: i32) {
        let mut pts = self.get_pts();
        if pts < 0 {
            pts = 0;
        }

        env.send_confirm_pts_qts(pts, qts);

        self.last_confirmed_pts = pts;
        self.last_confirmed_qts = qts;
    }

    /// `UpdatesManager::can_postpone_updates` (h:354).
    fn can_postpone_updates(&mut self) -> bool {
        if self.skipped_postponed_updates_after_start == 0 {
            return true;
        }
        self.skipped_postponed_updates_after_start -= 1;
        false
    }

    /// `UpdatesManager::init_state` (cpp:1851).
    pub fn init_state(&mut self, env: &mut impl UpdatesManagerEnv) {
        if env.close_flag() || !env.is_authorized() {
            return;
        }

        let drop_state = self.get_pts() == -1
            && env.can_ignore_background_updates()
            && env.is_bot()
            && env.get_option_integer("since_last_open") >= 2 * 86400;

        if env.ignore_background_updates() || drop_state {
            // just in case
            env.pmc_erase("updates.pts");
            env.pmc_erase("updates.qts");
            env.pmc_erase("updates.date");
        }
        let pts_str = env.pmc_get("updates.pts");
        if pts_str.is_empty() {
            if !self.running_get_difference {
                self.running_get_difference = true;

                self.before_get_difference(env, true);

                env.send_get_updates_state();
            }
            return;
        }
        let pts = parse_pmc_i32(&pts_str, "updates.pts");
        self.pts_manager.init(pts);
        self.last_confirmed_pts = self.get_pts();
        let qts_str = env.pmc_get("updates.qts");
        let qts = parse_pmc_i32(&qts_str, "updates.qts");
        self.qts_manager.init(qts);
        self.last_confirmed_qts = self.get_qts();
        let date_str = env.pmc_get("updates.date");
        self.date = parse_pmc_i32(&date_str, "updates.date");
        self.date_source = "database".to_string();
        tdutils::log_debug!("Init: {} {} {}", self.get_pts(), self.get_qts(), self.date);

        self.get_difference(env, "init_state");
    }

    // ------------------------------------------------------------------
    // M.7.2 — on_get_updates routing + updates_state + failure backoff
    // ------------------------------------------------------------------

    /// `UpdatesManager::on_get_updates` (cpp:1176) — the C++
    /// `send_closure_later` deferral is the actor queue; the synchronous
    /// engine applies directly (the caller pumps from the event loop).
    pub fn on_get_updates(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        updates: telegram_api::Updates,
        mut promise: Promise<()>,
    ) {
        self.on_get_updates_impl(env, updates, &mut promise);
        promise.set_value(());
    }

    /// `UpdatesManager::on_get_updates_impl` (cpp:1180). The C++ wraps the
    /// promise with a failure logger keyed by `get_update_ids`; our
    /// synchronous promises carry no error here (the module docs).
    fn on_get_updates_impl(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        updates: telegram_api::Updates,
        promise: &mut Promise<()>,
    ) {
        // UpdatesManager.cpp:1192-1194: the whole container, TL-text dump
        // (skipped for updateShort — the single-update kind).
        if !matches!(updates, telegram_api::Updates::updateShort(_)) {
            tdutils::log_info!(
                at "UpdatesManager.cpp",
                1193,
                "Receive {}",
                updates.to_debug_string()
            );
        }
        if !env.is_authorized() {
            if let telegram_api::Updates::updateShort(short) = &updates
                && !env.close_flag()
            {
                let update = short.update.clone().unwrap_or_default();
                let update_id = update_id_and_name(&update).0;
                if update_id == telegram_api::updateLoginToken::ID {
                    self.sink.on_update_login_token();
                    return;
                }
                if update_id == telegram_api::updateSentPhoneCode::ID {
                    if let telegram_api::Update::updateSentPhoneCode(update) = update {
                        self.sink
                            .on_update_sent_code(update.sent_code.unwrap_or_default());
                    }
                    return;
                }

                // updateServiceNotification / updateDcOptions /
                // updateConfig / updateLangPackTooLong /
                // updateLangPack apply without authorization
                if matches!(
                    update_id,
                    telegram_api::updateServiceNotification::ID
                        | telegram_api::updateDcOptions::ID
                        | telegram_api::updateConfig::ID
                        | telegram_api::updateLangPackTooLong::ID
                        | telegram_api::updateLangPack::ID
                ) {
                    tdutils::log_info!("Apply without authorization");
                    self.on_update(env, update.into(), std::mem::take(promise));
                    return;
                }
            }
            tdutils::log_info!(
                at "UpdatesManager.cpp",
                1222,
                "Ignore received before authorization or after logout {}",
                updates.to_debug_string()
            );
            return;
        }

        match updates {
            telegram_api::Updates::updatesTooLong(_) => {
                self.get_difference(env, "updatesTooLong");
            }
            telegram_api::Updates::updateShortMessage(update) => {
                let from_id = if update.out {
                    env.get_my_id()
                } else {
                    update.user_id
                };
                let message = telegram_api::Message::message(telegram_api::message {
                    out: update.out,
                    mentioned: update.mentioned,
                    media_unread: update.media_unread,
                    silent: update.silent,
                    id: update.id,
                    from_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                        user_id: from_id,
                    })),
                    peer_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                        user_id: update.user_id,
                    })),
                    fwd_from: update.fwd_from,
                    via_bot_id: update.via_bot_id,
                    reply_to: update.reply_to,
                    date: update.date,
                    message: update.message,
                    entities: update.entities,
                    ttl_period: update.ttl_period,
                    ..Default::default()
                });
                self.on_pending_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateNewMessage(
                        telegram_api::updateNewMessage {
                            message: Some(message),
                            pts: update.pts,
                            pts_count: update.pts_count,
                        },
                    )),
                    0,
                    Some(std::mem::take(promise)),
                    "telegram_api::updateShortMessage",
                );
            }
            telegram_api::Updates::updateShortChatMessage(update) => {
                let message = telegram_api::Message::message(telegram_api::message {
                    out: update.out,
                    mentioned: update.mentioned,
                    media_unread: update.media_unread,
                    silent: update.silent,
                    id: update.id,
                    from_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                        user_id: update.from_id,
                    })),
                    peer_id: Some(telegram_api::Peer::peerChat(telegram_api::peerChat {
                        chat_id: update.chat_id,
                    })),
                    fwd_from: update.fwd_from,
                    via_bot_id: update.via_bot_id,
                    reply_to: update.reply_to,
                    date: update.date,
                    message: update.message,
                    entities: update.entities,
                    ttl_period: update.ttl_period,
                    ..Default::default()
                });
                self.on_pending_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateNewMessage(
                        telegram_api::updateNewMessage {
                            message: Some(message),
                            pts: update.pts,
                            pts_count: update.pts_count,
                        },
                    )),
                    0,
                    Some(std::mem::take(promise)),
                    "telegram_api::updateShortChatMessage",
                );
            }
            telegram_api::Updates::updateShort(short) => {
                tdutils::log_debug!("Receive updateShort");
                let short_update = short.update.unwrap_or_default();
                if !self.is_acceptable_update(env, &short_update) {
                    tdutils::log_error!(
                        "Receive unacceptable short update: {}",
                        update_id_and_name(&short_update).1
                    );
                    self.get_difference(env, "unacceptable short update");
                    return;
                }
                self.short_update_date = short.date;
                self.on_update(env, short_update.into(), std::mem::take(promise));
                self.short_update_date = 0;
            }
            telegram_api::Updates::updatesCombined(combined) => {
                for user in combined.users {
                    self.sink.on_user(user);
                }
                for chat in combined.chats {
                    self.sink.on_chat(chat);
                }
                let now = env.now();
                self.on_pending_updates(
                    env,
                    combined
                        .updates
                        .into_iter()
                        .map(PendingUpdate::Wire)
                        .map(Some)
                        .collect(),
                    combined.seq_start,
                    combined.seq,
                    combined.date,
                    now,
                    Some(std::mem::take(promise)),
                    "telegram_api::updatesCombined",
                );
            }
            telegram_api::Updates::updates(updates) => {
                // UpdatesManager.cpp:1288-1295: TWO sources here — the
                // user/chat intake gets "update <ctor_id>" (single-update
                // containers) or "updates"; the pending feed always gets
                // the container ctor name. (The intake source rides the
                // UserManager-intake log family.)
                for user in updates.users {
                    self.sink.on_user(user);
                }
                for chat in updates.chats {
                    self.sink.on_chat(chat);
                }
                let now = env.now();
                self.on_pending_updates(
                    env,
                    updates
                        .updates
                        .into_iter()
                        .map(PendingUpdate::Wire)
                        .map(Some)
                        .collect(),
                    updates.seq,
                    updates.seq,
                    updates.date,
                    now,
                    Some(std::mem::take(promise)),
                    "telegram_api::updates",
                );
            }
            telegram_api::Updates::updateShortSentMessage(_) => {
                tdutils::log_error!("Receive updateShortSentMessage");
                self.get_difference(env, "updateShortSentMessage");
            }
        }
    }

    /// `UpdatesManager::on_failed_get_updates_state` (cpp:1307).
    pub fn on_failed_get_updates_state(&mut self, env: &mut impl UpdatesManagerEnv, error: Status) {
        if env.close_flag() || !env.is_authorized() {
            return;
        }
        if error.code() != 401 {
            tdutils::log_error!("Receive updates.getState error: {}", error.message());
        }

        self.running_get_difference = false;
        self.schedule_get_difference(env, "on_failed_get_updates_state");
    }

    /// `UpdatesManager::on_failed_get_difference` (cpp:1319).
    pub fn on_failed_get_difference(&mut self, env: &mut impl UpdatesManagerEnv, error: Status) {
        if env.close_flag() || !env.is_authorized() {
            return;
        }
        if error.code() != 401 {
            tdutils::log_error!("Receive updates.getDifference error: {}", error.message());
        }

        self.running_get_difference = false;
        self.schedule_get_difference(env, "on_failed_get_difference");

        if error.message() == "PERSISTENT_TIMESTAMP_INVALID" {
            self.set_pts(env, i32::MAX, "PERSISTENT_TIMESTAMP_INVALID");
        }
    }

    /// `UpdatesManager::schedule_get_difference` (cpp:1335) — the doubled
    /// backoff with the `fill_get_difference_gap` fire action.
    pub fn schedule_get_difference(&mut self, env: &mut impl UpdatesManagerEnv, source: &str) {
        if env.close_flag() || !env.is_authorized() {
            return;
        }
        if !self.retry_timeout.has_timeout() {
            tdutils::log_warning!(
                "Schedule getDifference in {} seconds with PTS = {}, QTS = {}, date = {} from {}",
                self.retry_time,
                self.get_pts(),
                self.get_qts(),
                self.get_date(),
                source
            );
            self.retry_timeout
                .set_timeout_in(env.now(), f64::from(self.retry_time));
            self.retry_time *= 2;
            if self.retry_time > 60 {
                self.retry_time = tdutils::random::fast(60, 80);
            }
        } else {
            tdutils::log_debug!("Schedule getDifference from {}", source);
        }
    }

    /// `UpdatesManager::on_get_updates_state` (cpp:1354).
    pub fn on_get_updates_state(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        state: telegram_api::updates_State,
        source: &str,
    ) {
        // UpdatesManager.cpp:1357 — the intake dump rides the
        // get_difference verbosity family with the oneline TL text.
        tdutils::vlog!(
            tdutils::logging::verbosity::GET_DIFFERENCE,
            "get_difference",
            at "UpdatesManager.cpp",
            1357,
            "Receive {} from {}",
            crate::td::oneline(&state.to_debug_string()),
            source
        );

        if self.get_pts() == i32::MAX {
            tdutils::log_warning!("Restore PTS to {}", state.pts);
            // restoring right PTS
            debug_assert!(self.pending_pts_updates.is_empty());
            let real_running_get_difference = self.running_get_difference;
            self.running_get_difference = false;
            self.process_postponed_pts_updates(env); // drop all updates with old PTS
            self.running_get_difference = real_running_get_difference;
            self.pts_manager.init(state.pts);
            self.last_confirmed_pts = self.get_pts();
            self.last_pts_save_time = env.now() - 2.0 * MAX_PTS_SAVE_DELAY;
            self.save_pts(env, state.pts);
        } else {
            // UpdatesManager.cpp:1374: the source carries the oneline
            // state dump into both the PTS and date logs.
            let full_source = format!(
                "on_get_updates_state {} from {}",
                crate::td::oneline(&state.to_debug_string()),
                source
            );
            self.set_pts(env, state.pts, &full_source);
            self.set_date(env, state.date, false, full_source.clone());
            self.add_qts(env, state.qts);

            self.seq = state.seq;
        }

        if self.running_get_difference {
            // called from getUpdatesState
            self.running_get_difference = false;
            self.after_get_difference(env);
        }
    }

    // ------------------------------------------------------------------
    // M.7.3 — seq queue + gap
    // ------------------------------------------------------------------

    /// `UpdatesManager::on_pending_update` (cpp:3739).
    fn on_pending_update(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        update: PendingUpdate,
        seq: i32,
        promise: Option<Promise<()>>,
        source: &str,
    ) {
        let now = env.now();
        self.on_pending_updates(env, vec![Some(update)], seq, seq, 0, now, promise, source);
    }

    /// `UpdatesManager::on_pending_updates` (cpp:2534). The C++
    /// MultiPromise fan-in collapses to the synchronous promise completion
    /// points below (the module docs): `promise` completes when every
    /// dispatched sub-work has completed or the batch has been parked in a
    /// queue (the queue entry carries it).
    #[allow(clippy::too_many_arguments)]
    fn on_pending_updates(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        mut updates: Vec<Option<PendingUpdate>>,
        seq_begin: i32,
        seq_end: i32,
        date: i32,
        receive_time: f64,
        mut promise: Option<Promise<()>>,
        source: &str,
    ) {
        if self.get_pts() == -1 {
            self.init_state(env);
        }

        if !env.is_authorized() {
            tdutils::log_info!("Ignore updates received before authorization or after logout");
            complete(promise);
            return;
        }

        if seq_begin < 0 || seq_end < 0 || date < 0 || seq_end < seq_begin {
            tdutils::log_error!(
                "Wrong updates parameters seq_begin = {}, seq_end = {}, date = {} from {}",
                seq_begin,
                seq_end,
                date,
                source
            );
            self.get_difference(env, "on wrong updates in on_pending_updates");
            complete(promise);
            return;
        }

        // The 11 immediately-applied short updates (they carry no pts
        // state and need no promise).
        for update in updates.iter_mut() {
            // UpdatesManager.cpp:2705 — the per-update Receive dump.
            if let Some(update) = update.as_ref() {
                tdutils::log_info!(
                    at "UpdatesManager.cpp",
                    2705,
                    "Receive from {} pending {}",
                    source,
                    update.dump_string()
                );
            }
            let is_short = update
                .as_ref()
                .is_some_and(|update| is_immediately_applied_update(update.get_id()));
            if is_short {
                let update = update.take().expect("checked above");
                self.short_update_date = if date == 0 { env.unix_time() } else { date };
                tdutils::log_info!("Process short {}", update.name());
                self.on_update(env, update, Promise::default());
                self.short_update_date = 0;
            }
        }

        let need_postpone = self.running_get_difference;
        let can_postpone = self.can_postpone_updates();
        if !need_postpone {
            for update in updates.iter_mut() {
                let Some(update_ref) = update.as_mut() else {
                    continue;
                };
                let wire = update_ref.wire().expect("wire update");
                if self.is_acceptable_update(env, wire) {
                    continue;
                }

                // for channels we can try to replace an unacceptable
                // update with updateChannelTooLong (cpp:2592-2620)
                let mut pts = 0;
                let channel_message = match wire {
                    telegram_api::Update::updateNewChannelMessage(update) => {
                        pts = update.pts;
                        Some(&update.message)
                    }
                    telegram_api::Update::updateEditChannelMessage(update) => {
                        pts = update.pts;
                        Some(&update.message)
                    }
                    _ => None,
                };
                if let Some(message) = channel_message {
                    let dialog_id = message_dialog_id(message);
                    if dialog_id.get_type() == DialogType::Channel {
                        let channel_id = dialog_id.get_channel_id();
                        if self.sink.is_acceptable_channel(channel_id.get()) {
                            if self.sink.is_old_channel_update(channel_id.get(), pts) {
                                // the update will be ignored anyway, so
                                // there is no reason to replace it or
                                // force get_difference
                                tdutils::log_info!(
                                    "Allow an outdated unacceptable update from {}",
                                    source
                                );
                                continue;
                            }
                            if !matches!(
                                message.as_ref(),
                                Some(telegram_api::Message::messageService(_))
                            ) {
                                // don't replace service messages, because
                                // they can be about bot's kicking
                                tdutils::log_info!(
                                    "Replace update about new message with updateChannelTooLong in dialog {}",
                                    dialog_id.get()
                                );
                                *update_ref = PendingUpdate::Wire(
                                    telegram_api::Update::updateChannelTooLong(
                                        telegram_api::updateChannelTooLong {
                                            flags: 1,
                                            channel_id: channel_id.get(),
                                            pts,
                                        },
                                    ),
                                );
                                continue;
                            }
                        }
                    } else {
                        tdutils::log_error!("Update is not from a channel: {}", update_ref.name());
                    }
                }

                self.get_difference(env, "on unacceptable updates in on_pending_updates");
                complete(promise);
                return;
            }
        }

        // The single updateReadHistoryOutbox-from-a-user fast path.
        if date > 0
            && updates.len() == 1
            && let Some(PendingUpdate::Wire(telegram_api::Update::updateReadHistoryOutbox(update))) =
                &updates[0]
            && let Some(telegram_api::Peer::peerUser(peer)) = &update.peer
            && is_valid_user_id(peer.user_id)
        {
            self.sink
                .on_update_user_local_was_online(peer.user_id, date);
        }

        let mut ordinary_new_message_count = 0usize;
        let mut scheduled_new_message_count = 0usize;
        let mut update_message_id_count = 0usize;
        let mut update_count = 0usize;
        for update in updates.iter().flatten() {
            let constructor_id = update.get_id();
            if constructor_id == telegram_api::updateMessageID::ID {
                update_message_id_count += 1;
            } else {
                update_count += 1;
                if constructor_id == telegram_api::updateNewMessage::ID
                    || constructor_id == telegram_api::updateNewChannelMessage::ID
                {
                    ordinary_new_message_count += 1;
                } else if constructor_id == telegram_api::updateNewScheduledMessage::ID {
                    scheduled_new_message_count += 1;
                }
            }
        }

        if update_message_id_count != 0
            && ordinary_new_message_count != 0
            && scheduled_new_message_count != 0
        {
            tdutils::log_error!("Receive mixed message types in updates");
            if !self.running_get_difference {
                self.schedule_get_difference(env, "on_get_wrong_updates");
            }
            complete(promise);
            return;
        }

        // The C++ get_promise(): with exactly one non-postponed update,
        // the only promise goes to the first dispatched update; otherwise
        // the MultiPromise fan-in collapses (see the module docs).
        let use_single = !need_postpone && update_count == 1;
        let mut single_promise = if use_single { promise.take() } else { None };

        // The pre-pass: updateMessageID / updateStoryID /
        // updateFolderPeers / updateEncryption are processed before the
        // classification.
        for update in updates.iter_mut() {
            if update.is_none() {
                continue;
            }
            let id = update.as_ref().expect("checked above").get_id();
            if id == telegram_api::updateMessageID::ID {
                let update = update.take().expect("checked above");
                if let PendingUpdate::Wire(telegram_api::Update::updateMessageID(
                    sent_message_update,
                )) = update
                {
                    let is_scheduled = if ordinary_new_message_count != 0 {
                        false
                    } else if scheduled_new_message_count != 0 {
                        true
                    } else {
                        // C++ leaves the MessageId default (invalid)
                        false
                    };
                    if !self.sink.on_update_message_id(
                        sent_message_update.random_id,
                        sent_message_update.id,
                        is_scheduled,
                        source,
                    ) {
                        tdutils::log_error!(
                            "updateMessageID with unknown random_id from {}",
                            source
                        );
                    }
                }
            }
            if id == telegram_api::updateStoryID::ID {
                let update = update.take().expect("checked above");
                // story_manager_->on_update_story_id — the M.7.15 flip.
                self.sink
                    .on_unsupported_update(update.get_id(), update.name(), 2741);
            }
            if id == telegram_api::updateFolderPeers::ID {
                let update = update.take().expect("checked above");
                let dispatch_promise = single_promise.take().unwrap_or_default();
                self.on_update(env, update, dispatch_promise);
            }
            if id == telegram_api::updateEncryption::ID {
                let update = update.take().expect("checked above");
                let dispatch_promise = single_promise.take().unwrap_or_default();
                self.on_update(env, update, dispatch_promise);
            }
            debug_assert!(need_postpone || !self.running_get_difference);
        }

        // The pts/qts/channel classification with the postpone bookkeeping.
        for update in updates.iter_mut() {
            if update.is_none() {
                continue;
            }
            let is_pts = update.as_ref().is_some_and(is_pts_update);
            let is_qts = !is_pts && update.as_ref().is_some_and(is_qts_update);
            let is_channel_pts =
                !is_pts && !is_qts && update.as_ref().is_some_and(is_channel_pts_update);
            if is_pts {
                let pts = update.as_ref().map_or(0, get_update_pts);
                if self.running_get_difference
                    && can_postpone
                    && pts != 0
                    && (self.min_postponed_update_pts == 0 || pts < self.min_postponed_update_pts)
                {
                    self.min_postponed_update_pts = pts;
                }
                let update = update.take().expect("checked above");
                let dispatch_promise = single_promise.take().unwrap_or_default();
                self.on_update(env, update, dispatch_promise);
            } else if is_qts {
                let qts = update.as_ref().map_or(0, get_update_qts);
                if self.running_get_difference
                    && can_postpone
                    && qts != 0
                    && (self.min_postponed_update_qts == 0 || qts < self.min_postponed_update_qts)
                {
                    self.min_postponed_update_qts = qts;
                }
                let update = update.take().expect("checked above");
                let dispatch_promise = single_promise.take().unwrap_or_default();
                self.on_update(env, update, dispatch_promise);
            } else if is_channel_pts {
                let update = update.take().expect("checked above");
                let dispatch_promise = single_promise.take().unwrap_or_default();
                self.on_update(env, update, dispatch_promise);
            }
        }

        if seq_begin == 0 && seq_end == 0 {
            let have_updates = updates.iter().any(|update| update.is_some());
            if !have_updates {
                tdutils::log_info!("All updates were processed");
                single_promise = single_promise.or(promise.take());
                complete(single_promise);
                return;
            }
        }

        // still need to process the only update: the surviving promise
        // becomes the seq-processing "lock".
        let mut lock = if use_single {
            single_promise.take()
        } else {
            promise.take()
        };

        if need_postpone || self.running_get_difference {
            tdutils::log_info!(
                "Postpone {} updates [{}, {}] with date = {} from {}",
                updates.len(),
                seq_begin,
                seq_end,
                date,
                source
            );
            if !need_postpone {
                tdutils::log_error!("Run get difference while applying updates from {}", source);
            }
            if can_postpone {
                let entry = PendingSeqUpdates {
                    seq_begin,
                    seq_end,
                    date,
                    receive_time,
                    updates,
                    promise: lock.take(),
                };
                insert_pending_seq(&mut self.postponed_updates, entry);
            } else {
                complete(lock);
            }
            return;
        }

        if seq_begin == 0 || seq_begin == self.seq + 1 {
            tdutils::log_info!(
                "Process {} updates [{}, {}] with date = {} from {}",
                updates.len(),
                seq_begin,
                seq_end,
                date,
                source
            );
            let updates = updates.into_iter().flatten().collect();
            self.process_seq_updates(env, seq_end, date, updates, lock);
            self.process_pending_seq_updates(env);
            return;
        }

        if seq_begin <= self.seq {
            if self.seq >= (1 << 30) && seq_begin < self.seq - (1 << 30) {
                let now = env.now();
                self.set_seq_gap_timeout(now, 0.001);
            }
            if seq_end > self.seq {
                tdutils::log_error!(
                    "Receive updates with seq_begin = {}, seq_end = {}, but seq = {} from {}",
                    seq_begin,
                    seq_end,
                    self.seq,
                    source
                );
            } else {
                tdutils::log_info!(
                    "Receive old updates with seq_begin = {}, seq_end = {}, but seq = {} from {}",
                    seq_begin,
                    seq_end,
                    self.seq,
                    source
                );
            }
            complete(lock);
            return;
        }

        tdutils::log_info!(
            "Gap in seq has found. Receive {} updates [{}, {}] from {}, but seq = {}",
            updates.len(),
            seq_begin,
            seq_end,
            source,
            self.seq
        );

        if self.can_postpone_updates() {
            let entry = PendingSeqUpdates {
                seq_begin,
                seq_end,
                date,
                receive_time,
                updates,
                promise: lock.take(),
            };
            insert_pending_seq(&mut self.pending_seq_updates, entry);
        } else {
            complete(lock);
        }

        let now = env.now();
        self.set_seq_gap_timeout(now, receive_time + MAX_UNFILLED_GAP_TIME - now);
    }

    /// `UpdatesManager::process_seq_updates` (cpp:3177).
    fn process_seq_updates(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        seq_end: i32,
        date: i32,
        updates: Vec<PendingUpdate>,
        promise: Option<Promise<()>>,
    ) {
        let serialized_updates = if date != 0 && seq_end != 0 {
            format!(
                "process_seq_updates [seq_ = {}, seq_end = {}]",
                self.seq, seq_end
            )
        } else {
            String::new()
        };
        let mut promise = promise.unwrap_or_default();
        let updates = updates.into_iter().map(Some).collect();
        self.process_updates(env, updates, false, &mut promise);
        if seq_end != 0 {
            self.seq = seq_end;
        }
        if date != 0 && seq_end != 0 {
            self.set_date(env, date, true, serialized_updates);
        }
    }

    /// `UpdatesManager::process_updates` (cpp:2889) — the private apply
    /// path. The C++ MultiPromise collapse is documented in the module
    /// docs.
    fn process_updates(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        mut updates: Vec<Option<PendingUpdate>>,
        force_apply: bool,
        promise: &mut Promise<()>,
    ) {
        let update_count = updates.iter().filter(|update| update.is_some()).count();
        if update_count == 0 {
            promise.set_value(());
            return;
        }

        let use_single = update_count == 1;
        let mut remaining_promise: Option<Promise<()>> = if use_single {
            Some(std::mem::take(promise))
        } else {
            None
        };

        let mut update_pts_changed: Option<PendingUpdate> = None;
        for update in updates.iter_mut() {
            if update.is_none() {
                continue;
            }
            // process updateNewChannelMessage first
            if update.as_ref().expect("checked above").get_id()
                == telegram_api::updateNewChannelMessage::ID
            {
                let update = update.take().expect("checked above");
                let dispatch_promise = take_dispatch_promise(&mut remaining_promise);
                self.on_update(env, update, dispatch_promise);
                continue;
            }

            // process updateNewScheduledMessage first
            if update.as_ref().expect("checked above").get_id()
                == telegram_api::updateNewScheduledMessage::ID
            {
                let update = update.take().expect("checked above");
                let dispatch_promise = take_dispatch_promise(&mut remaining_promise);
                self.on_update(env, update, dispatch_promise);
                continue;
            }

            // updateGroupCallConnection must be processed before updateGroupCall
            if update.as_ref().expect("checked above").get_id()
                == telegram_api::updateGroupCallConnection::ID
            {
                let update = update.take().expect("checked above");
                let dispatch_promise = take_dispatch_promise(&mut remaining_promise);
                self.on_update(env, update, dispatch_promise);
                continue;
            }

            // updateGroupCallChainBlocks must be processed before updateGroupCall
            if update.as_ref().expect("checked above").get_id()
                == telegram_api::updateGroupCallChainBlocks::ID
            {
                let update = update.take().expect("checked above");
                let dispatch_promise = take_dispatch_promise(&mut remaining_promise);
                self.on_update(env, update, dispatch_promise);
                continue;
            }

            // updatePtsChanged forces get difference, so process it last
            if update.as_ref().expect("checked above").get_id()
                == telegram_api::updatePtsChanged::ID
            {
                let update = update.take().expect("checked above");
                update_pts_changed = Some(update);
                continue;
            }
        }
        if force_apply {
            for update in updates.iter_mut() {
                let Some(pending) = update else { continue };
                if is_pts_update(pending) {
                    if pending.get_id() == telegram_api::updateWebPage::ID {
                        let update = update.take().expect("checked above");
                        // web_pages_manager_->on_get_web_page — M.7.15 flip.
                        self.sink
                            .on_unsupported_update(update.get_id(), update.name(), 2978);
                        continue;
                    }

                    debug_assert!(pending.get_id() != telegram_api::updateFolderPeers::ID);

                    if pending.get_id() == telegram_api::updateReadHistoryInbox::ID
                        && let PendingUpdate::Wire(telegram_api::Update::updateReadHistoryInbox(
                            inbox,
                        )) = pending
                    {
                        inbox.still_unread_count = -1;
                    }

                    let update = update.take().expect("checked above");
                    self.process_pts_update(env, update);
                } else if is_qts_update(pending) {
                    let update = update.take().expect("checked above");
                    let dispatch_promise = take_dispatch_promise(&mut remaining_promise);
                    self.process_qts_update(env, update, 0, dispatch_promise);
                } else if pending.get_id() == telegram_api::updateChannelTooLong::ID {
                    let update = update.take().expect("checked above");
                    if let PendingUpdate::Wire(telegram_api::Update::updateChannelTooLong(
                        too_long,
                    )) = update
                    {
                        self.sink.on_update_channel_too_long(too_long, true);
                    }
                }
            }
        }
        for update in updates.iter_mut() {
            if update.is_none() {
                continue;
            }
            tdutils::log_debug!(
                "Process update {}",
                update.as_ref().expect("checked above").name()
            );
            let update = update.take().expect("checked above");
            let dispatch_promise = take_dispatch_promise(&mut remaining_promise);
            self.on_update(env, update, dispatch_promise);
            debug_assert!(!self.running_get_difference);
        }
        if let Some(update_pts_changed) = update_pts_changed {
            let dispatch_promise = take_dispatch_promise(&mut remaining_promise);
            self.on_update(env, update_pts_changed, dispatch_promise);
        }
        // The MultiPromise collapse: the outer promise completes after all
        // dispatched work (each dispatch promise completed synchronously).
        if let Some(promise) = remaining_promise.as_mut() {
            promise.set_value(());
        }
    }

    /// `UpdatesManager::process_pending_seq_updates` (cpp:3620).
    fn process_pending_seq_updates(&mut self, env: &mut impl UpdatesManagerEnv) {
        if !self.pending_seq_updates.is_empty() {
            tdutils::log_debug!(
                "Trying to process {} pending seq updates",
                self.pending_seq_updates.len()
            );
            // must not return, because in case of seq overflow there are
            // no pending seq updates
        }

        let begin_time = env.now();
        let initial_seq = self.seq;
        let mut applied_update_count = 0;
        while !self.pending_seq_updates.is_empty() && !self.running_get_difference {
            let seq_begin = self.pending_seq_updates[0].seq_begin;
            if seq_begin.wrapping_sub(1) > self.seq && seq_begin.wrapping_sub(1 << 30) <= self.seq {
                // the updates will be applied later
                break;
            }

            applied_update_count += 1;
            let mut entry = self.pending_seq_updates.remove(0);
            let seq_end = entry.seq_end;
            if seq_begin - 1 == self.seq {
                let updates = entry.updates.into_iter().flatten().collect();
                self.process_seq_updates(env, seq_end, entry.date, updates, entry.promise.take());
            } else {
                // old update
                debug_assert!(seq_begin != 0);
                if seq_begin <= self.seq && self.seq < seq_end {
                    tdutils::log_error!(
                        "Receive updates with seq_begin = {}, seq_end = {}, but seq = {}",
                        seq_begin,
                        seq_end,
                        self.seq
                    );
                }
                complete(entry.promise.take());
            }
        }
        if self.pending_seq_updates.is_empty() || applied_update_count > 0 {
            self.seq_gap_timeout.cancel_timeout();
        }
        if !self.pending_seq_updates.is_empty() {
            // if still have a gap, reset timeout
            let receive_time = self
                .pending_seq_updates
                .iter()
                .take(GAP_TIMEOUT_UPDATE_COUNT)
                .map(|update| update.receive_time)
                .min_by(f64::total_cmp)
                .expect("non-empty");
            let now = env.now();
            self.set_seq_gap_timeout(now, (receive_time + MAX_UNFILLED_GAP_TIME - now).max(0.001));
        }

        let passed_time = env.now() - begin_time;
        if passed_time >= UPDATE_APPLY_WARNING_TIME {
            tdutils::log_warning!(
                "Seq has changed from {} to {} after applying {} and keeping {} pending updates in {} seconds",
                initial_seq,
                self.seq,
                applied_update_count,
                self.pending_seq_updates.len(),
                passed_time
            );
        }
    }

    // ------------------------------------------------------------------
    // M.7.4 — qts queue
    // ------------------------------------------------------------------

    /// `UpdatesManager::add_pending_qts_update` (cpp:2743).
    fn add_pending_qts_update(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        update: PendingUpdate,
        qts: i32,
        mut promise: Promise<()>,
    ) {
        if qts <= 1 {
            tdutils::log_error!("Receive wrong QTS {} in {}", qts, update.name());
            self.schedule_get_difference(env, "wrong QTS");
            promise.set_value(());
            return;
        }

        let mut old_qts = self.get_qts();
        tdutils::log_info!(
            "Process update with QTS = {}, current QTS = {}",
            qts,
            old_qts
        );
        if qts < old_qts.wrapping_sub(1_000_009) {
            tdutils::log_warning!(
                "Restore QTS after QTS overflow from {} to {} by {}",
                old_qts,
                qts,
                update.name()
            );
            self.add_qts(env, qts.wrapping_sub(1));
            debug_assert_eq!(self.get_qts(), qts.wrapping_sub(1));
            old_qts = qts.wrapping_sub(1);
            self.last_confirmed_qts = self.get_qts();
        }

        if qts <= old_qts {
            tdutils::log_info!("Skip already applied update with QTS = {}", qts);
            promise.set_value(());
            return;
        }

        if self.running_get_difference || (qts.wrapping_sub(1) > old_qts && old_qts > 0) {
            tdutils::log_info!("Postpone update with QTS = {}", qts);
            if !self.running_get_difference && self.pending_qts_updates.is_empty() {
                let now = env.now();
                self.set_qts_gap_timeout(now, MAX_UNFILLED_GAP_TIME);
            }
            if !self.can_postpone_updates() {
                promise.set_value(());
                return;
            }
            let pending_update = self.pending_qts_updates.entry(qts).or_default();
            if pending_update.update.is_some() {
                tdutils::log_warning!("Receive duplicate update with QTS = {}", qts);
            } else {
                pending_update.receive_time = env.now();
            }
            pending_update.update = Some(update);
            pending_update.promises.push(promise);
            return;
        }

        self.process_qts_update(env, update, qts, promise);
        self.process_pending_qts_updates(env);
    }

    /// `UpdatesManager::process_qts_update` (cpp:3204). The bot-only qts
    /// arms are the enumerated trace drops (the M.7.15 flips).
    fn process_qts_update(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        update: PendingUpdate,
        qts: i32,
        mut promise: Promise<()>,
    ) {
        tdutils::log_debug!("Process {}", update.name());
        if self.last_confirmed_qts < qts - FORCED_GET_DIFFERENCE_PTS_DIFF
            && self.last_confirmed_qts != 0
        {
            self.confirm_pts_qts(env, qts);
        }
        self.qts_diff += 1;
        if self.qts_diff >= 1_000_000 {
            tdutils::log_warning!("Fixed {} QTS gaps up to QTS {}", self.qts_gap, qts);
            self.qts_gap = 0;
            self.qts_diff = 0;
        }
        let constructor_id = update.get_id();
        if constructor_id == telegram_api::updateNewEncryptedMessage::ID {
            if let PendingUpdate::Wire(telegram_api::Update::updateNewEncryptedMessage(encrypted)) =
                update
            {
                self.add_qts(env, qts);
                self.sink
                    .on_new_encrypted_message(encrypted.message.unwrap_or_default());
            }
        } else if env.is_bot() {
            let (id, name) = match &update {
                PendingUpdate::Wire(wire) => update_id_and_name(wire),
                other => (other.get_id(), other.name()),
            };
            // The 16 bot qts intakes (poll votes, managed bots,
            // participant updates, reactions, business messages, ...) —
            // the M.7.15 flips.
            self.sink.on_unsupported_update(id, name, 3217);
            self.add_qts(env, qts);
        } else {
            self.add_qts(env, qts);
            tdutils::log_error!("Receive {}", update.name());
        }
        promise.set_value(());
    }

    /// `UpdatesManager::process_pending_qts_updates` (cpp:3544).
    fn process_pending_qts_updates(&mut self, env: &mut impl UpdatesManagerEnv) {
        if self.pending_qts_updates.is_empty() {
            return;
        }

        tdutils::log_debug!(
            "Process {} pending QTS updates",
            self.pending_qts_updates.len()
        );
        let begin_time = env.now();
        let initial_qts = self.get_qts();
        let mut applied_update_count = 0;
        while !self.pending_qts_updates.is_empty() {
            debug_assert!(!self.running_get_difference);
            let qts = *self.pending_qts_updates.keys().next().expect("non-empty");
            let old_qts = self.get_qts();
            if qts.wrapping_sub(1) > old_qts && qts.wrapping_sub(1 << 30) <= old_qts {
                // the update will be applied later
                break;
            }
            let mut entry = self
                .pending_qts_updates
                .remove(&qts)
                .expect("checked above");
            applied_update_count += 1;
            if qts == old_qts + 1
                && let Some(update) = entry.update.take()
            {
                // the C++ completes the stored promises together after
                // the update applies; the dispatch promise is empty
                // here (the fan-in collapse)
                self.process_qts_update(env, update, qts, Promise::default());
            }
            for promise in entry.promises.iter_mut() {
                promise.set_value(());
            }
        }

        if applied_update_count > 0 {
            self.qts_gap_timeout.cancel_timeout();
        }
        if !self.pending_qts_updates.is_empty() {
            // if still have a gap, reset timeout
            let receive_time = self
                .pending_qts_updates
                .values()
                .take(GAP_TIMEOUT_UPDATE_COUNT)
                .map(|update| update.receive_time)
                .min_by(f64::total_cmp)
                .expect("non-empty");
            let now = env.now();
            self.set_qts_gap_timeout(now, (receive_time + MAX_UNFILLED_GAP_TIME - now).max(0.001));
        }
        debug_assert!(!self.running_get_difference);

        let passed_time = env.now() - begin_time;
        if passed_time >= UPDATE_APPLY_WARNING_TIME {
            tdutils::log_warning!(
                "QTS has changed from {} to {} after applying {} and keeping {} pending updates in {} seconds",
                initial_qts,
                self.get_qts(),
                applied_update_count,
                self.pending_qts_updates.len(),
                passed_time
            );
        }
    }

    // ------------------------------------------------------------------
    // M.7.5 — PTS pending/postponed machinery
    // ------------------------------------------------------------------

    /// `UpdatesManager::process_pts_update` (cpp:3013) — must be called
    /// only during getDifference (the C++ CHECKs are debug asserts).
    fn process_pts_update(&mut self, _env: &mut impl UpdatesManagerEnv, update: PendingUpdate) {
        if !check_pts_update(&update) {
            tdutils::log_error!("Receive wrong PTS update: {}", update.name());
            return;
        }

        // must be called only during getDifference
        debug_assert!(self.pending_pts_updates.is_empty());
        debug_assert_eq!(self.accumulated_pts, -1);

        self.sink.process_pts_update(update);
    }

    /// `UpdatesManager::add_pending_pts_update` (cpp:3032) — the
    /// validation, the jump recovery, the postponement and the
    /// accumulated-pts bookkeeping with the fast path.
    // `pub(crate)` — the M.18 affected-history loop routes the
    // `on_get_affected_history` pts bookkeeping here (the C++ calls
    // `updates_manager_->add_pending_pts_update` cross-manager).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_pending_pts_update(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        update: PendingUpdate,
        new_pts: i32,
        pts_count: i32,
        receive_time: f64,
        mut promise: Promise<()>,
        source: &str,
    ) {
        // do not try to run getDifference from this function
        tdutils::log_info!(
            at "UpdatesManager.cpp",
            3038,
            "Receive from {} with pts_count = {} pending {}",
            source,
            pts_count,
            update.dump_string()
        );
        if pts_count < 0 || new_pts <= pts_count {
            tdutils::log_error!(
                "Receive update with wrong PTS = {} or pts_count = {} from {}: {}",
                new_pts,
                pts_count,
                source,
                update.name()
            );
            promise.set_value(());
            return;
        }

        if !check_pts_update(&update) {
            tdutils::log_error!(
                "Receive wrong PTS update from {}: {}",
                source,
                update.name()
            );
            promise.set_value(());
            return;
        }

        if DROP_PTS_UPDATES && source != "on_get_pts_update" {
            let now = env.now();
            self.set_pts_gap_timeout(now, 1.0);
            promise.set_value(());
            return;
        }

        let old_pts = self.get_pts();
        if new_pts < old_pts.wrapping_sub(99) && source != AFTER_GET_DIFFERENCE_SOURCE {
            let need_restore_pts = new_pts < old_pts.wrapping_sub(19_999);
            let now = env.now();
            if old_pts == 2_100_000_000
                && new_pts < 1_100_000_000
                && pts_count <= 10_000
                && env.get_option_integer("session_count") > 1
            {
                self.set_pts(env, 1, "restore PTS");
                self.set_pts_gap_timeout(now, 0.001);
                promise.set_value(());
                return;
            } else if now > self.last_pts_jump_warning_time + 1.0
                && need_restore_pts
                && !(old_pts == i32::MAX && self.running_get_difference)
            {
                tdutils::log_error!(
                    "Restore PTS after delete_first_messages from {} to {} is disabled, pts_count = {}, update is from {}: {}",
                    old_pts,
                    new_pts,
                    pts_count,
                    source,
                    update.name()
                );
                self.last_pts_jump_warning_time = now;
            }
            if need_restore_pts {
                self.set_pts_gap_timeout(now, 0.001);
            }
        }

        if new_pts <= old_pts || (old_pts >= 1 && new_pts.wrapping_sub(1 << 30) > old_pts) {
            self.sink
                .skip_old_pending_pts_update(update, new_pts, old_pts, pts_count, source);
            promise.set_value(());
            return;
        }
        if source == AFTER_GET_DIFFERENCE_SOURCE
            && old_pts >= 1
            && new_pts.wrapping_sub(1 << 21) > old_pts
        {
            tdutils::log_error!(
                "Ignore update {}, because current PTS is {}",
                update.name(),
                old_pts
            );
            promise.set_value(());
            return;
        }

        if self.running_get_difference || !self.postponed_pts_updates.is_empty() {
            tdutils::log_info!(
                "Save pending update received while running getDifference from {}",
                source
            );
            self.postpone_pts_update(env, update, new_pts, pts_count, receive_time, promise);
            return;
        }

        // is_acceptable_update check was skipped for postponed PTS updates
        if source == AFTER_GET_DIFFERENCE_SOURCE
            && !self.is_acceptable_update(env, update.wire().expect("wire update"))
        {
            tdutils::log_info!("Postpone again unacceptable pending update");
            self.postpone_pts_update(env, update, new_pts, pts_count, receive_time, promise);
            let now = env.now();
            self.set_pts_gap_timeout(now, 0.001);
            return;
        }

        if old_pts > new_pts.wrapping_sub(pts_count) {
            tdutils::log_warning!(
                "Have old_pts (= {}) + pts_count (= {}) > new_pts (= {}). Update from {} = {}",
                old_pts,
                pts_count,
                new_pts,
                source,
                update.name()
            );
            self.postpone_pts_update(env, update, new_pts, pts_count, receive_time, promise);
            let now = env.now();
            self.set_pts_gap_timeout(now, 0.001);
            return;
        }

        self.accumulated_pts_count = self.accumulated_pts_count.wrapping_add(pts_count);
        if new_pts > self.accumulated_pts {
            self.accumulated_pts = new_pts;
        }

        if old_pts
            > self
                .accumulated_pts
                .wrapping_sub(self.accumulated_pts_count)
        {
            tdutils::log_warning!(
                "Have old_pts (= {}) + accumulated_pts_count (= {}) > accumulated_pts (= {}). new_pts = {}, pts_count = {}. Update from {} = {}",
                old_pts,
                self.accumulated_pts_count,
                self.accumulated_pts,
                new_pts,
                pts_count,
                source,
                update.name()
            );
            self.postpone_pts_update(env, update, new_pts, pts_count, receive_time, promise);
            let now = env.now();
            self.set_pts_gap_timeout(now, 0.001);
            return;
        }

        if pts_count == 0 && update.get_id() != PendingUpdate::Dummy.get_id() {
            tdutils::log_info!("Skip useless update {}", update.name());
        }

        if self.pending_pts_updates.is_empty()
            && old_pts
                == self
                    .accumulated_pts
                    .wrapping_sub(self.accumulated_pts_count)
            && !self.pts_gap_timeout.has_timeout()
        {
            if pts_count > 0 {
                self.sink.process_pts_update(update);
                self.set_pts(
                    env,
                    self.accumulated_pts,
                    "process pending updates fast path",
                );
                self.accumulated_pts_count = 0;
                self.accumulated_pts = -1;
            } else {
                tdutils::log_debug!("There is no need to process the update");
            }
            promise.set_value(());
            return;
        }

        if pts_count == 0
            && receive_time < env.now() - MAX_UNFILLED_GAP_TIME
            && update.get_id() == PendingUpdate::Dummy.get_id()
        {
            // don't warn about fetching of the affected history
            self.expect_pts_gap = true;
        }

        let entry = PendingPtsUpdate {
            update,
            pts: new_pts,
            pts_count,
            receive_time,
            promise,
        };
        insert_pending_pts(&mut self.pending_pts_updates, entry);

        if old_pts < self.accumulated_pts - self.accumulated_pts_count {
            if old_pts == new_pts - pts_count {
                // can't apply all updates, but can apply this and probably
                // some other updates
                self.process_pending_pts_updates(env);
            } else {
                tdutils::log_debug!(
                    "Can't process PTS updates, because current PTS = {}, new PTS = {}, pts_count = {}, maximum known PTS = {}, accumulated_pts_count = {}",
                    old_pts,
                    new_pts,
                    pts_count,
                    self.accumulated_pts,
                    self.accumulated_pts_count
                );
                let now = env.now();
                self.set_pts_gap_timeout(
                    now,
                    (receive_time + MAX_UNFILLED_GAP_TIME - now).max(0.001),
                );
            }
            return;
        }

        debug_assert_eq!(old_pts, self.accumulated_pts - self.accumulated_pts_count);
        self.process_all_pending_pts_updates(env);
    }

    /// `UpdatesManager::postpone_pts_update` (cpp:3167).
    #[allow(clippy::too_many_arguments)]
    fn postpone_pts_update(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        update: PendingUpdate,
        pts: i32,
        pts_count: i32,
        receive_time: f64,
        mut promise: Promise<()>,
    ) {
        if !self.can_postpone_updates()
            || (pts_count > 1 && env.get_option_integer("session_count") <= 1)
        {
            promise.set_value(());
            return;
        }
        let entry = PendingPtsUpdate {
            update,
            pts,
            pts_count,
            receive_time,
            promise,
        };
        insert_pending_pts(&mut self.postponed_pts_updates, entry);
    }

    /// `UpdatesManager::process_all_pending_pts_updates` (cpp:3418).
    fn process_all_pending_pts_updates(&mut self, env: &mut impl UpdatesManagerEnv) {
        let begin_time = env.now();
        for mut entry in std::mem::take(&mut self.pending_pts_updates) {
            self.sink.process_pts_update(entry.update);
            entry.promise.set_value(());
        }

        if self.last_pts_gap_time != 0.0 {
            let begin_diff = begin_time - self.last_pts_gap_time;
            let diff = env.now() - self.last_pts_gap_time;
            self.last_pts_gap_time = 0.0;
            if diff > 0.1 {
                tdutils::log_debug!(
                    "Gap in PTS from {} to {} has been filled in {}-{} seconds",
                    self.accumulated_pts - self.accumulated_pts_count,
                    self.accumulated_pts,
                    begin_diff,
                    diff
                );
            }
        }

        let pts = self.accumulated_pts;
        self.set_pts(env, pts, "process_all_pending_pts_updates");
        self.drop_all_pending_pts_updates();
    }

    /// `UpdatesManager::drop_all_pending_pts_updates` (cpp:3443).
    fn drop_all_pending_pts_updates(&mut self) {
        self.accumulated_pts_count = 0;
        self.accumulated_pts = -1;
        self.min_pts_gap_timeout.cancel_timeout();
        self.pts_gap_timeout.cancel_timeout();
        self.pending_pts_updates.clear();
    }

    /// `UpdatesManager::process_postponed_pts_updates` (cpp:3452).
    fn process_postponed_pts_updates(&mut self, env: &mut impl UpdatesManagerEnv) {
        if self.postponed_pts_updates.is_empty() {
            return;
        }

        let begin_time = env.now();
        let initial_pts = self.get_pts();
        let mut old_pts = initial_pts;
        let mut skipped_update_count = 0;
        let mut applied_update_count = 0;
        let update_idx = 0usize;
        while update_idx < self.postponed_pts_updates.len() {
            let mut new_pts = self.postponed_pts_updates[update_idx].pts;
            let mut pts_count = self.postponed_pts_updates[update_idx].pts_count;
            if new_pts <= old_pts || (old_pts >= 1 && new_pts.wrapping_sub(1 << 30) > old_pts) {
                skipped_update_count += 1;
                let entry = self.postponed_pts_updates.remove(update_idx);
                self.sink.skip_old_pending_pts_update(
                    entry.update,
                    entry.pts,
                    old_pts,
                    entry.pts_count,
                    "process_postponed_pts_updates",
                );
                complete(Some(entry.promise));
                continue;
            }

            if env.now() - begin_time >= UPDATE_APPLY_WARNING_TIME {
                // the updates will be applied or skipped later; reget the
                // remaining updates through getDifference
                break;
            }

            // Find the longest applicable run [update_idx, last_idx): the
            // C++ inner scan, at most GAP_TIMEOUT_UPDATE_COUNT entries.
            let len = self.postponed_pts_updates.len();
            let mut last_idx = update_idx;
            let mut i = 1i32;
            loop {
                last_idx += 1;
                if old_pts == new_pts.wrapping_sub(pts_count) {
                    // the updates can be applied
                    break;
                }
                if old_pts > new_pts.wrapping_sub(pts_count)
                    || last_idx >= len
                    || i == GAP_TIMEOUT_UPDATE_COUNT as i32
                {
                    // the updates can't be applied
                    last_idx = update_idx;
                    break;
                }
                new_pts = self.postponed_pts_updates[last_idx].pts;
                pts_count += self.postponed_pts_updates[last_idx].pts_count;
                i += 1;
            }

            if last_idx == update_idx {
                // the updates will be applied or skipped later
                break;
            }
            debug_assert_eq!(old_pts, new_pts.wrapping_sub(pts_count));

            while update_idx < last_idx && update_idx < self.postponed_pts_updates.len() {
                let entry = self.postponed_pts_updates.remove(update_idx);
                last_idx -= 1;
                if entry.pts_count > 0 {
                    applied_update_count += 1;
                    self.sink.process_pts_update(entry.update);
                }
                complete(Some(entry.promise));
            }
            old_pts = new_pts;
        }
        if old_pts != initial_pts {
            self.set_pts(env, old_pts, "process_postponed_pts_updates");
        }
        debug_assert!(!self.running_get_difference);
        if skipped_update_count + applied_update_count > 0 {
            tdutils::log_debug!(
                "PTS has changed from {} to {} after skipping {}, applying {} and keeping {} postponed updates",
                initial_pts,
                old_pts,
                skipped_update_count,
                applied_update_count,
                self.postponed_pts_updates.len()
            );
        }

        let passed_time = env.now() - begin_time;
        if passed_time >= UPDATE_APPLY_WARNING_TIME {
            tdutils::log_warning!(
                "PTS has changed from {} to {} after skipping {}, applying {} and keeping {} postponed for {} seconds updates in {} seconds",
                initial_pts,
                old_pts,
                skipped_update_count,
                applied_update_count,
                self.postponed_pts_updates.len(),
                env.now() - self.get_difference_start_time,
                passed_time
            );
        }
    }

    /// `UpdatesManager::process_pending_pts_updates` (cpp:3570).
    fn process_pending_pts_updates(&mut self, env: &mut impl UpdatesManagerEnv) {
        if self.pending_pts_updates.is_empty() {
            return;
        }

        let begin_time = env.now();
        let initial_pts = self.get_pts();
        let mut applied_update_count = 0;
        while !self.pending_pts_updates.is_empty() {
            let entry_pts = self.pending_pts_updates[0].pts;
            let entry_pts_count = self.pending_pts_updates[0].pts_count;
            if self.get_pts() != entry_pts.wrapping_sub(entry_pts_count) {
                // the updates will be applied or skipped later
                break;
            }

            applied_update_count += 1;
            let mut entry = self.pending_pts_updates.remove(0);
            if entry.pts_count > 0 {
                self.sink.process_pts_update(entry.update);
                self.set_pts(env, entry.pts, "process_pending_pts_updates");

                if self.accumulated_pts != -1 {
                    debug_assert!(entry.pts <= self.accumulated_pts);
                    debug_assert!(self.accumulated_pts_count >= entry.pts_count);
                    self.accumulated_pts_count -= entry.pts_count;
                }
            } else {
                tdutils::log_info!("Skip because of pts_count == 0 {}", entry.update.name());
            }
            entry.promise.set_value(());
        }
        if applied_update_count > 0 {
            self.min_pts_gap_timeout.cancel_timeout();
            self.pts_gap_timeout.cancel_timeout();
        }
        if !self.pending_pts_updates.is_empty() {
            // if still have a gap, reset timeout
            let receive_time = self
                .pending_pts_updates
                .iter()
                .take(GAP_TIMEOUT_UPDATE_COUNT)
                .map(|update| update.receive_time)
                .min_by(f64::total_cmp)
                .expect("non-empty");
            let now = env.now();
            self.set_pts_gap_timeout(now, (receive_time + MAX_UNFILLED_GAP_TIME - now).max(0.001));
        }

        let passed_time = env.now() - begin_time;
        if passed_time >= UPDATE_APPLY_WARNING_TIME {
            tdutils::log_warning!(
                "PTS has changed from {} to {} after applying {} and keeping {} pending updates in {} seconds",
                initial_pts,
                self.get_pts(),
                applied_update_count,
                self.pending_pts_updates.len(),
                passed_time
            );
        }
    }

    // ------------------------------------------------------------------
    // M.7.6 skeleton — get_difference / before / after
    // ------------------------------------------------------------------

    /// `UpdatesManager::get_difference` (cpp:479).
    pub fn get_difference(&mut self, env: &mut impl UpdatesManagerEnv, source: &str) {
        if env.close_flag() || !env.is_authorized() {
            return;
        }
        if self.get_pts() == -1 {
            self.init_state(env);
            return;
        }

        if self.running_get_difference {
            tdutils::log_debug!(
                "Skip running getDifference from {} because it is already running",
                source
            );
            return;
        }

        self.run_get_difference(env, false, source);
    }

    /// `UpdatesManager::run_get_difference` (cpp:496).
    fn run_get_difference(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        is_recursive: bool,
        source: &str,
    ) {
        debug_assert!(self.get_pts() != -1);
        debug_assert!(env.is_authorized());
        debug_assert!(!self.running_get_difference);

        self.running_get_difference = true;

        let mut pts = self.get_pts();
        let date = self.get_date();
        let qts = self.get_qts();
        if pts < 0 {
            pts = 0;
        }

        tdutils::log_debug!(
            "-----BEGIN GET DIFFERENCE----- from {} with PTS = {}, QTS = {}, date = {}",
            source,
            pts,
            qts,
            date
        );

        self.before_get_difference(env, false);

        if !is_recursive {
            self.min_postponed_update_pts = 0;
            self.min_postponed_update_qts = 0;
        }

        env.send_get_difference(pts, date, qts);
        self.last_confirmed_pts = pts;
        self.last_confirmed_qts = qts;
    }

    /// `UpdatesManager::before_get_difference` (cpp:532).
    fn before_get_difference(&mut self, env: &mut impl UpdatesManagerEnv, _is_initial: bool) {
        // may be called many times before after_get_difference is called
        self.sink.on_before_get_difference();

        if self.can_postpone_updates() {
            let pending = std::mem::take(&mut self.pending_pts_updates);
            for entry in pending {
                insert_pending_pts(&mut self.postponed_pts_updates, entry);
            }
        } else {
            for mut entry in std::mem::take(&mut self.pending_pts_updates) {
                entry.promise.set_value(());
            }
        }

        self.drop_all_pending_pts_updates();

        if self.get_difference_start_time <= 0.0 {
            self.get_difference_start_time = env.now();
        }
    }

    /// `UpdatesManager::after_get_difference` (cpp:2282).
    pub fn after_get_difference(&mut self, env: &mut impl UpdatesManagerEnv) {
        debug_assert!(!self.running_get_difference);

        self.retry_timeout.cancel_timeout();
        self.retry_time = 1;

        self.skipped_postponed_updates_after_start = 0;
        env.set_option_empty("since_last_open");

        // cancels qts_gap_timeout_ if needed, can apply some updates
        // received during getDifference, but missed in getDifference
        self.process_pending_qts_updates(env);

        // cancels seq_gap_timeout_ if needed, can apply some updates
        // received during getDifference, but missed in getDifference
        self.process_pending_seq_updates(env);

        if self.running_get_difference {
            return;
        }

        if !self.postponed_updates.is_empty() {
            let begin_time = env.now();
            let chunk_count = self.postponed_updates.len();
            tdutils::log_debug!("Begin to apply {} postponed update chunks", chunk_count);
            let mut total_update_count = 0usize;
            while !self.postponed_updates.is_empty() {
                let mut entry = self.postponed_updates.remove(0);
                let updates_seq_begin = entry.seq_begin;
                let updates_seq_end = entry.seq_end;
                let receive_time = entry.receive_time;
                // ignore entry.date, because it may be too old
                let update_count = entry.updates.len();
                self.on_pending_updates(
                    env,
                    entry.updates,
                    updates_seq_begin,
                    updates_seq_end,
                    0,
                    receive_time,
                    entry.promise.take(),
                    "postponed updates",
                );
                if self.running_get_difference {
                    tdutils::log_debug!(
                        "Finish to apply postponed updates with {} updates left after applied {} updates, because forced to run getDifference",
                        self.postponed_updates.len(),
                        total_update_count
                    );
                    return;
                }
                total_update_count += update_count;
            }
            tdutils::log_debug!("Finished to apply {} postponed updates", total_update_count);
            let passed_time = env.now() - begin_time;
            if passed_time >= UPDATE_APPLY_WARNING_TIME {
                tdutils::log_warning!(
                    "Applied {} updates in {} chunks in {} seconds after postponing them for {} seconds",
                    total_update_count,
                    chunk_count,
                    passed_time,
                    env.now() - self.get_difference_start_time
                );
            }
        }

        if !self.postponed_pts_updates.is_empty() {
            // must be before the messages-manager after_get_difference
            let postponed_updates = std::mem::take(&mut self.postponed_pts_updates);

            let begin_time = env.now();
            let update_count = postponed_updates.len();
            let old_pts = self.get_pts();
            tdutils::log_debug!(
                "Begin to apply {} postponed PTS updates with PTS = {}",
                postponed_updates.len(),
                self.get_pts()
            );
            for entry in postponed_updates {
                self.add_pending_pts_update(
                    env,
                    entry.update,
                    entry.pts,
                    entry.pts_count,
                    entry.receive_time,
                    entry.promise,
                    AFTER_GET_DIFFERENCE_SOURCE,
                );
                debug_assert!(!self.running_get_difference);
            }
            tdutils::log_debug!(
                "After applying postponed PTS updates have PTS = {}, max_pts = {} and {} + {} pending PTS updates",
                self.get_pts(),
                self.accumulated_pts,
                self.pending_pts_updates.len(),
                self.postponed_pts_updates.len()
            );
            let passed_time = env.now() - begin_time;
            if passed_time >= UPDATE_APPLY_WARNING_TIME {
                tdutils::log_warning!(
                    "Updated PTS from {} to {} and applied {} PTS updates in {} seconds after postponing them for {} seconds",
                    old_pts,
                    self.get_pts(),
                    update_count,
                    passed_time,
                    env.now() - self.get_difference_start_time
                );
            }
        }

        self.sink.on_after_get_difference();
        self.get_difference_start_time = 0.0;

        // cpp:2369-2370 — the reloaders re-run after every getDifference.
        self.try_reload_data(env);
        self.try_reload_often_data(env);
    }

    /// `UpdatesManager::on_get_difference` (cpp:2040) — all four variants.
    /// The response intake is driven directly by the harness while the
    /// GetDifference sender is `[blocked: net]`.
    pub fn on_get_difference(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        difference: telegram_api::updates_Difference,
    ) {
        if env.close_flag() || !env.is_authorized() {
            return;
        }

        tdutils::log_debug!("----- END  GET DIFFERENCE-----");
        self.running_get_difference = false;

        match difference {
            telegram_api::updates_Difference::updates_differenceEmpty(difference) => {
                self.set_date(
                    env,
                    difference.date,
                    false,
                    "on_get_difference_empty".to_string(),
                );
                self.seq = difference.seq;

                self.process_pending_qts_updates(env);
                if !self.pending_qts_updates.is_empty() {
                    tdutils::log_warning!(
                        "Drop {} pending QTS updates after receive empty difference",
                        self.pending_qts_updates.len()
                    );
                    let pending_qts_updates = std::mem::take(&mut self.pending_qts_updates);
                    for (_, mut pending_update) in pending_qts_updates {
                        for promise in pending_update.promises.iter_mut() {
                            promise.set_value(());
                        }
                    }
                }

                self.process_pending_seq_updates(env);
                if !self.pending_seq_updates.is_empty() {
                    tdutils::log_warning!(
                        "Drop {} pending seq updates after receive empty difference",
                        self.pending_seq_updates.len()
                    );
                    let pending_seq_updates = std::mem::take(&mut self.pending_seq_updates);
                    for mut pending_update in pending_seq_updates {
                        complete(pending_update.promise.take());
                    }
                }
            }
            telegram_api::updates_Difference::updates_difference(difference) => {
                tdutils::log_debug!(
                    "In get difference receive {} users and {} chats",
                    difference.users.len(),
                    difference.chats.len()
                );
                for user in difference.users {
                    self.sink.on_user(user);
                }
                for chat in difference.chats {
                    self.sink.on_chat(chat);
                }

                if self.get_difference_retry_count <= 5
                    && difference
                        .new_messages
                        .iter()
                        .any(|message| self.sink.is_invalid_poll_message(message))
                {
                    self.get_difference_retry_count += 1;
                    tdutils::log_error!(
                        "Receive invalid poll message in updates.difference after {} tries",
                        self.get_difference_retry_count
                    );
                    self.run_get_difference(env, true, "reget difference");
                    return;
                }

                self.process_get_difference_updates(
                    env,
                    difference.new_messages,
                    difference.new_encrypted_messages,
                    difference
                        .other_updates
                        .into_iter()
                        .map(PendingUpdate::Wire)
                        .map(Some)
                        .collect(),
                );
                if self.running_get_difference {
                    tdutils::log_error!(
                        "Get difference has run while processing get difference updates"
                    );
                } else {
                    self.on_get_updates_state(
                        env,
                        difference.state.unwrap_or_default(),
                        "get difference",
                    );
                }
            }
            telegram_api::updates_Difference::updates_differenceSlice(difference) => {
                let is_pts_changed = have_update_pts_changed(&difference.other_updates);
                // The C++ carries a TODO here: it compares the
                // intermediate state against the current one without
                // acting on it.

                tdutils::log_debug!(
                    "In get difference receive {} users and {} chats",
                    difference.users.len(),
                    difference.chats.len()
                );
                for user in difference.users {
                    self.sink.on_user(user);
                }
                for chat in difference.chats {
                    self.sink.on_chat(chat);
                }

                if self.get_difference_retry_count <= 5
                    && difference
                        .new_messages
                        .iter()
                        .any(|message| self.sink.is_invalid_poll_message(message))
                {
                    self.get_difference_retry_count += 1;
                    tdutils::log_error!(
                        "Receive invalid poll message in updates.differenceSlice after {} tries",
                        self.get_difference_retry_count
                    );
                    self.run_get_difference(env, true, "reget difference");
                    return;
                }

                self.process_get_difference_updates(
                    env,
                    difference.new_messages,
                    difference.new_encrypted_messages,
                    difference
                        .other_updates
                        .into_iter()
                        .map(PendingUpdate::Wire)
                        .map(Some)
                        .collect(),
                );
                if self.running_get_difference {
                    if !is_pts_changed {
                        tdutils::log_error!(
                            "Get difference has run while processing get difference updates"
                        );
                    }
                } else {
                    debug_assert!(
                        !is_pts_changed,
                        "updatePtsChanged must have restarted getDifference"
                    );

                    let old_pts = self.get_pts();
                    let old_date = self.get_date();
                    let old_qts = self.get_qts();
                    self.on_get_updates_state(
                        env,
                        difference.intermediate_state.unwrap_or_default(),
                        "get difference slice",
                    );

                    self.process_postponed_pts_updates(env);
                    self.process_pending_qts_updates(env);

                    let new_pts = self.get_pts();
                    let new_date = self.get_date();
                    let new_qts = self.get_qts();
                    if old_pts != i32::MAX
                        && new_date == old_date
                        && (new_pts == old_pts
                            || (self.min_postponed_update_pts != 0
                                && new_pts >= self.min_postponed_update_pts))
                        && (new_qts == old_qts
                            || (self.min_postponed_update_qts != 0
                                && new_qts >= self.min_postponed_update_qts))
                    {
                        tdutils::log_debug!("Switch back from getDifference to update processing");
                    } else if new_pts != -1 {
                        // just in case
                        self.run_get_difference(env, true, "on updates_differenceSlice");
                    }
                }
            }
            telegram_api::updates_Difference::updates_differenceTooLong(difference) => {
                if env.get_option_integer("session_count") <= 1 {
                    tdutils::log_error!("Receive differenceTooLong");
                }
                self.set_pts(env, difference.pts, "differenceTooLong");
                self.get_difference(env, "on updates_differenceTooLong");
            }
        }

        self.get_difference_retry_count = 0;

        if !self.running_get_difference {
            self.after_get_difference(env);
        }
    }

    /// `UpdatesManager::process_get_difference_updates` (cpp:1957).
    fn process_get_difference_updates(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        new_messages: Vec<telegram_api::Message>,
        new_encrypted_messages: Vec<telegram_api::EncryptedMessage>,
        mut other_updates: Vec<Option<PendingUpdate>>,
    ) {
        tdutils::log_debug!(
            "In get difference receive {} messages, {} encrypted messages and {} other updates",
            new_messages.len(),
            new_encrypted_messages.len(),
            other_updates.len()
        );
        for update in other_updates.iter_mut() {
            if update.is_none() {
                continue;
            }
            let constructor_id = update.as_ref().expect("checked above").get_id();
            if constructor_id == telegram_api::updateMessageID::ID {
                // in getDifference updateMessageID can't be received for
                // scheduled messages
                let update = update.take().expect("checked above");
                if let PendingUpdate::Wire(telegram_api::Update::updateMessageID(
                    update_message_id,
                )) = update
                {
                    self.sink.on_update_message_id(
                        update_message_id.random_id,
                        update_message_id.id,
                        false,
                        "getDifference",
                    );
                }
                debug_assert!(!self.running_get_difference);
            } else if constructor_id == telegram_api::updateStoryID::ID {
                let update = update.take().expect("checked above");
                // story_manager_->on_update_story_id — the M.7.15 flip.
                self.sink
                    .on_unsupported_update(update.get_id(), update.name(), 1987);
                debug_assert!(!self.running_get_difference);
            } else if constructor_id == telegram_api::updateEncryption::ID {
                let update = update.take().expect("checked above");
                self.on_update(env, update, Promise::default());
                debug_assert!(!self.running_get_difference);
            } else if constructor_id == telegram_api::updateFolderPeers::ID {
                let mut update = update.take().expect("checked above");
                if let PendingUpdate::Wire(telegram_api::Update::updateFolderPeers(
                    update_folder_peers,
                )) = &mut update
                {
                    if update_folder_peers.pts_count != 0 {
                        tdutils::log_error!(
                            "Receive updateFolderPeers with pts_count = {}",
                            update_folder_peers.pts_count
                        );
                        update_folder_peers.pts_count = 0;
                    }
                    update_folder_peers.pts = 0;
                }
                self.on_update(env, update, Promise::default());
                debug_assert!(!self.running_get_difference);
            } else if constructor_id == telegram_api::updateUser::ID
                || constructor_id == telegram_api::updateChat::ID
                || constructor_id == telegram_api::updateChannel::ID
            {
                *update = None;
            }
        }

        // The session_count <= 1 message-id order validation is a
        // MessagesManager diagnostic (no state effect); it lands with that
        // slice.

        for message in new_messages {
            // channel messages must not be received in this vector
            self.sink.on_get_difference_message(message);
            debug_assert!(!self.running_get_difference);
        }

        for encrypted_message in new_encrypted_messages {
            self.sink.on_new_encrypted_message(encrypted_message);
        }

        let mut promise = Promise::default();
        self.process_updates(env, other_updates, true, &mut promise);
    }

    /// `UpdatesManager::on_get_pts_update` (cpp:2189) — the
    /// GetPtsUpdateQuery response (the pts-short-gap repair intake).
    pub fn on_get_pts_update(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        pts: i32,
        difference: telegram_api::updates_Difference,
    ) {
        if env.close_flag() || !env.is_authorized() {
            return;
        }
        tdutils::log_debug!(
            "Receive update with PTS {}: {}",
            pts,
            difference_name(&difference)
        );
        if self.get_pts() != pts - 1
            || self.running_get_difference
            || !self.postponed_pts_updates.is_empty()
            || self.pending_pts_updates.is_empty()
            || self.pending_pts_updates[0].pts > pts + 1
            || self.pending_pts_updates[0].pts != pts + self.pending_pts_updates[0].pts_count
        {
            return;
        }

        // The C++ updates_difference → updates_differenceSlice conversion
        // with the fallthrough: both share the slice body.
        let slice = match difference {
            telegram_api::updates_Difference::updates_difference(difference) => {
                telegram_api::updates_differenceSlice {
                    new_messages: difference.new_messages,
                    new_encrypted_messages: difference.new_encrypted_messages,
                    other_updates: difference.other_updates,
                    chats: difference.chats,
                    users: difference.users,
                    intermediate_state: difference.state,
                }
            }
            telegram_api::updates_Difference::updates_differenceSlice(difference) => difference,
            telegram_api::updates_Difference::updates_differenceEmpty(_)
            | telegram_api::updates_Difference::updates_differenceTooLong(_) => {
                tdutils::log_error!(
                    "Receive wrong difference for request with PTS = {}, but have pending with PTS = {}",
                    pts - 1,
                    self.pending_pts_updates[0].pts
                );
                return;
            }
        };
        self.on_get_pts_update_slice(env, pts, slice);
    }

    /// The `updates_differenceSlice` body of `on_get_pts_update`
    /// (cpp:2215-2264, the C++ fallthrough target).
    fn on_get_pts_update_slice(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        pts: i32,
        difference: telegram_api::updates_differenceSlice,
    ) {
        if have_update_pts_changed(&difference.other_updates) {
            return;
        }

        for user in difference.users {
            self.sink.on_user(user);
        }
        for chat in difference.chats {
            self.sink.on_chat(chat);
        }

        let mut updates: Vec<Option<PendingUpdate>> = difference
            .other_updates
            .into_iter()
            .map(PendingUpdate::Wire)
            .map(Some)
            .collect();
        for message in difference.new_messages {
            updates.push(Some(PendingUpdate::Wire(
                telegram_api::Update::updateNewMessage(telegram_api::updateNewMessage {
                    message: Some(message),
                    pts,
                    pts_count: 1,
                }),
            )));
        }
        let mut update_ptr: Option<PendingUpdate> = None;
        let mut update_count = 0usize;
        for update in updates.iter_mut() {
            if update.is_none() {
                continue;
            }
            let constructor_id = update.as_ref().expect("checked above").get_id();
            if constructor_id == telegram_api::updateMessageID::ID {
                // in getDifference updateMessageID can't be received for
                // scheduled messages
                let update = update.take().expect("checked above");
                if let PendingUpdate::Wire(telegram_api::Update::updateMessageID(
                    update_message_id,
                )) = update
                {
                    self.sink.on_update_message_id(
                        update_message_id.random_id,
                        update_message_id.id,
                        false,
                        "on_get_pts_update",
                    );
                }
                continue;
            }
            if constructor_id == telegram_api::updateDeleteMessages::ID
                && let Some(PendingUpdate::Wire(telegram_api::Update::updateDeleteMessages(
                    update_delete_messages,
                ))) = update.as_ref()
                && (update_delete_messages.pts_count != 0
                    || update_delete_messages.messages.len() != 1)
            {
                tdutils::log_error!("Receive in getDifference updateDeleteMessages");
            }

            update_ptr = update.take();
            update_count += 1;
        }

        if !difference.new_encrypted_messages.is_empty() || update_count != 1 {
            tdutils::log_error!("Receive unexpected updates with PTS {}", pts);
            return;
        }

        debug_assert!(update_ptr.is_some());
        tdutils::log_debug!("Repair update with PTS {}", pts);
        self.pts_fixed_short_gap += 1;
        let now = env.now();
        self.add_pending_pts_update(
            env,
            update_ptr.expect("checked above"),
            pts,
            1,
            now,
            Promise::default(),
            "on_get_pts_update",
        );
    }

    // ------------------------------------------------------------------
    // M.7.7 skeleton — gap timers
    // ------------------------------------------------------------------

    /// `UpdatesManager::set_pts_gap_timeout` (cpp:3705).
    fn set_pts_gap_timeout(&mut self, now: f64, timeout: f64) {
        if !self.pts_gap_timeout.has_timeout() || timeout < self.pts_gap_timeout.get_timeout() {
            tdutils::log_debug!("Set PTS gap timeout in {}", timeout);
            if timeout > 2.0 * MIN_UNFILLED_GAP_TIME {
                self.min_pts_gap_timeout
                    .set_timeout_in(now, MIN_UNFILLED_GAP_TIME);
            }

            self.pts_gap_timeout.set_timeout_in(now, timeout);
            self.last_pts_gap_time = now;
        }
    }

    /// `UpdatesManager::set_seq_gap_timeout` (cpp:3724).
    fn set_seq_gap_timeout(&mut self, now: f64, timeout: f64) {
        if !self.seq_gap_timeout.has_timeout() || timeout < self.seq_gap_timeout.get_timeout() {
            tdutils::log_debug!("Set seq gap timeout in {}", timeout);
            self.seq_gap_timeout.set_timeout_in(now, timeout);
        }
    }

    /// `UpdatesManager::set_qts_gap_timeout` (cpp:3731).
    fn set_qts_gap_timeout(&mut self, now: f64, timeout: f64) {
        if !self.qts_gap_timeout.has_timeout() || timeout < self.qts_gap_timeout.get_timeout() {
            tdutils::log_debug!("Set QTS gap timeout in {}", timeout);
            self.qts_gap_timeout.set_timeout_in(now, timeout);
        }
    }

    /// `UpdatesManager::check_pts_gap`/`repair_pts_gap` (cpp:341-377) —
    /// the pts-short-gap single-update fetch; the GetPtsUpdateQuery is the
    /// `[blocked: net]` seam and the response intake
    /// (`on_get_pts_update(pts, difference)`) lands with M.7.6.
    fn check_pts_gap(&mut self, env: &mut impl UpdatesManagerEnv) {
        if env.close_flag() {
            return;
        }
        if !env.is_authorized()
            || self.running_get_difference
            || !self.postponed_pts_updates.is_empty()
        {
            return;
        }
        let pts = self.get_pts() + 1;
        if self.pending_pts_updates.is_empty() {
            return;
        }
        let first_pts = self.pending_pts_updates[0].pts;
        let first_pts_count = self.pending_pts_updates[0].pts_count;
        if first_pts != pts + first_pts_count {
            return;
        }
        if self.last_fetched_pts == pts {
            tdutils::log_debug!("Don't fetch again update with PTS = {}", pts);
            return;
        }
        self.last_fetched_pts = pts;

        tdutils::log_debug!("Fetch update with PTS = {}", pts);
        self.pts_short_gap += 1;
        env.send_get_pts_update(pts - 1);
    }

    /// `UpdatesManager::fill_pts_gap` (cpp:379).
    fn fill_pts_gap(&mut self, env: &mut impl UpdatesManagerEnv) {
        if self.expect_pts_gap {
            self.expect_pts_gap = false;
            self.fill_gap(env, String::new());
            return;
        }

        let mut min_pts = i32::MAX;
        let mut min_pts_count = 0;
        let mut first_update_name = "";
        let mut max_pts = 0;
        if let Some(min_update) = self.pending_pts_updates.first()
            && min_update.pts < min_pts
        {
            min_pts = min_update.pts;
            min_pts_count = min_update.pts_count;
            first_update_name = min_update.update.name();
        }
        if let Some(max_update) = self.pending_pts_updates.last() {
            max_pts = max_pts.max(max_update.pts);
        }
        if let Some(min_update) = self.postponed_pts_updates.first()
            && min_update.pts < min_pts
        {
            min_pts = min_update.pts;
            min_pts_count = min_update.pts_count;
            first_update_name = min_update.update.name();
        }
        if let Some(max_update) = self.postponed_pts_updates.last() {
            max_pts = max_pts.max(max_update.pts);
        }
        self.pts_gap += 1;
        self.fill_gap(
            env,
            format!(
                "PTS from {} to {}(-{})-{} {}",
                self.get_pts(),
                min_pts,
                min_pts_count,
                max_pts,
                first_update_name
            ),
        );
    }

    /// `UpdatesManager::fill_seq_gap` (cpp:416).
    fn fill_seq_gap(&mut self, env: &mut impl UpdatesManagerEnv) {
        let mut min_seq = i32::MAX;
        let mut max_seq = 0;
        if !self.pending_seq_updates.is_empty() {
            min_seq = self
                .pending_seq_updates
                .first()
                .expect("non-empty")
                .seq_begin;
            max_seq = self.pending_seq_updates.last().expect("non-empty").seq_end;
        }
        self.fill_gap(
            env,
            format!("seq from {} to {}-{}", self.seq, min_seq, max_seq),
        );
    }

    /// `UpdatesManager::fill_qts_gap` (cpp:427).
    fn fill_qts_gap(&mut self, env: &mut impl UpdatesManagerEnv) {
        let mut min_qts = i32::MAX;
        let mut max_qts = 0;
        if !self.pending_qts_updates.is_empty() {
            min_qts = *self.pending_qts_updates.keys().next().expect("non-empty");
            max_qts = *self
                .pending_qts_updates
                .keys()
                .next_back()
                .expect("non-empty");
        }
        self.qts_gap += 1;
        self.fill_gap(
            env,
            format!("QTS from {} to {}-{}", self.get_qts(), min_qts, max_qts),
        );
    }

    /// `UpdatesManager::fill_gap` (cpp:437). The C++ warning carries the
    /// least-recently-active auth-key session info (`session_infos_`,
    /// M.7.16); the engine logs the gap itself until then.
    fn fill_gap(&mut self, env: &mut impl UpdatesManagerEnv, source: String) {
        if env.close_flag() {
            return;
        }
        if !env.is_authorized() {
            return;
        }

        if !source.is_empty() && !self.running_get_difference {
            tdutils::log_warning!("Filling gap in {} by running getDifference", source);
        }

        self.get_difference(env, "fill_gap");
    }

    /// The timeout pump — the tdactor timeouts folded into the manager
    /// convention (`next_deadline`/`tick_timeouts`, the UserManager
    /// pattern). Fires, in order: the throttled-save flush
    /// (`timeout_expired`), `check_pts_gap`, `fill_pts_gap`,
    /// `fill_seq_gap`, `fill_qts_gap`, the getDifference retry
    /// (`fill_get_difference_gap` → `fill_gap("")`) and the data/often
    /// data reloaders.
    pub fn next_deadline(&self) -> Option<f64> {
        [
            self.save_timeout.deadline,
            self.min_pts_gap_timeout.deadline,
            self.pts_gap_timeout.deadline,
            self.seq_gap_timeout.deadline,
            self.qts_gap_timeout.deadline,
            self.retry_timeout.deadline,
            self.data_reload_timeout.deadline,
            self.often_data_reload_timeout.deadline,
        ]
        .into_iter()
        .flatten()
        .reduce(f64::min)
    }

    pub fn tick_timeouts(&mut self, env: &mut impl UpdatesManagerEnv) {
        // A fired timer may arm others (a gap fill starts getDifference,
        // the flush arms saves); iterate to fixpoint with a hard cap.
        for _ in 0..100 {
            let now = env.now();
            let Some(deadline) = self.next_deadline() else {
                break;
            };
            if deadline > now {
                break;
            }
            if self
                .save_timeout
                .deadline
                .is_some_and(|deadline| deadline <= now)
            {
                self.save_timeout.cancel_timeout();
                self.timeout_expired(env);
                continue;
            }
            if self
                .min_pts_gap_timeout
                .deadline
                .is_some_and(|deadline| deadline <= now)
            {
                self.min_pts_gap_timeout.cancel_timeout();
                self.check_pts_gap(env);
                continue;
            }
            if self
                .pts_gap_timeout
                .deadline
                .is_some_and(|deadline| deadline <= now)
            {
                self.pts_gap_timeout.cancel_timeout();
                self.fill_pts_gap(env);
                continue;
            }
            if self
                .seq_gap_timeout
                .deadline
                .is_some_and(|deadline| deadline <= now)
            {
                self.seq_gap_timeout.cancel_timeout();
                self.fill_seq_gap(env);
                continue;
            }
            if self
                .qts_gap_timeout
                .deadline
                .is_some_and(|deadline| deadline <= now)
            {
                self.qts_gap_timeout.cancel_timeout();
                self.fill_qts_gap(env);
                continue;
            }
            if self
                .retry_timeout
                .deadline
                .is_some_and(|deadline| deadline <= now)
            {
                self.retry_timeout.cancel_timeout();
                self.fill_gap(env, String::new());
                continue;
            }
            if self
                .data_reload_timeout
                .deadline
                .is_some_and(|deadline| deadline <= now)
            {
                self.data_reload_timeout.cancel_timeout();
                self.try_reload_data(env);
                continue;
            }
            if self
                .often_data_reload_timeout
                .deadline
                .is_some_and(|deadline| deadline <= now)
            {
                self.often_data_reload_timeout.cancel_timeout();
                self.try_reload_often_data(env);
                continue;
            }
            break;
        }
    }

    // ------------------------------------------------------------------
    // M.7.12 skeleton — the on_update dispatcher
    // ------------------------------------------------------------------

    /// `UpdatesManager::on_update` — the dispatcher entry. The real
    /// intakes are here for the engine-internal kinds; the message-family
    /// seams route to the sink; the rest are enumerated trace drops
    /// (M.7.12–M.7.15 flip them).
    /// The Wave-2/3 enumerated trace drop: record via the sink and settle
    /// the apply promise (the C++ overload for the kind only completes the
    /// promise into the dropped manager).
    fn drop_unsupported(
        &mut self,
        id: i32,
        name: &'static str,
        cpp_line: u32,
        mut promise: Promise<()>,
    ) {
        self.sink.on_unsupported_update(id, name, cpp_line);
        promise.set_value(());
    }

    fn on_update(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        update: PendingUpdate,
        mut promise: Promise<()>,
    ) {
        match update {
            PendingUpdate::Wire(telegram_api::Update::updateNewMessage(update)) => {
                let new_pts = update.pts;
                let pts_count = update.pts_count;
                let now = env.now();
                self.add_pending_pts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateNewMessage(update)),
                    new_pts,
                    pts_count,
                    now,
                    promise,
                    "updateNewMessage",
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateReadMessagesContents(update)) => {
                let new_pts = update.pts;
                let pts_count = update.pts_count;
                let now = env.now();
                self.add_pending_pts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateReadMessagesContents(update)),
                    new_pts,
                    pts_count,
                    now,
                    promise,
                    "updateReadMessagesContents",
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateEditMessage(update)) => {
                let new_pts = update.pts;
                let pts_count = update.pts_count;
                let now = env.now();
                self.add_pending_pts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateEditMessage(update)),
                    new_pts,
                    pts_count,
                    now,
                    promise,
                    "updateEditMessage",
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateReadHistoryInbox(update)) => {
                let new_pts = update.pts;
                let pts_count = update.pts_count;
                let now = env.now();
                self.add_pending_pts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateReadHistoryInbox(update)),
                    new_pts,
                    pts_count,
                    now,
                    promise,
                    "updateReadHistoryInbox",
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateReadHistoryOutbox(update)) => {
                let new_pts = update.pts;
                let pts_count = update.pts_count;
                let now = env.now();
                self.add_pending_pts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateReadHistoryOutbox(update)),
                    new_pts,
                    pts_count,
                    now,
                    promise,
                    "updateReadHistoryOutbox",
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updatePinnedMessages(update)) => {
                let new_pts = update.pts;
                let pts_count = update.pts_count;
                let now = env.now();
                self.add_pending_pts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updatePinnedMessages(update)),
                    new_pts,
                    pts_count,
                    now,
                    promise,
                    "updatePinnedMessages",
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateDeleteMessages(update)) => {
                let new_pts = update.pts;
                let pts_count = update.pts_count;
                let now = env.now();
                if update.messages.is_empty() {
                    self.add_pending_pts_update(
                        env,
                        PendingUpdate::Dummy,
                        new_pts,
                        pts_count,
                        now,
                        Promise::default(),
                        "updateDeleteMessages",
                    );
                    promise.set_value(());
                } else {
                    self.add_pending_pts_update(
                        env,
                        PendingUpdate::Wire(telegram_api::Update::updateDeleteMessages(update)),
                        new_pts,
                        pts_count,
                        now,
                        promise,
                        "updateDeleteMessages",
                    );
                }
            }
            PendingUpdate::Wire(telegram_api::Update::updateNewChannelMessage(update)) => {
                let dialog_peer = message_peer_id(&update.message).cloned();
                let new_pts = update.pts;
                let pts_count = update.pts_count;
                self.sink.add_pending_channel_update(
                    dialog_peer,
                    PendingUpdate::Wire(telegram_api::Update::updateNewChannelMessage(update)),
                    new_pts,
                    pts_count,
                    promise,
                    "updateNewChannelMessage",
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateDeleteChannelMessages(update)) => {
                let dialog_peer = telegram_api::Peer::peerChannel(telegram_api::peerChannel {
                    channel_id: update.channel_id,
                });
                let new_pts = update.pts;
                let pts_count = update.pts_count;
                self.sink.add_pending_channel_update(
                    Some(dialog_peer),
                    PendingUpdate::Wire(telegram_api::Update::updateDeleteChannelMessages(update)),
                    new_pts,
                    pts_count,
                    promise,
                    "updateDeleteChannelMessages",
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateEditChannelMessage(update)) => {
                let dialog_peer = message_peer_id(&update.message).cloned();
                let new_pts = update.pts;
                let pts_count = update.pts_count;
                self.sink.add_pending_channel_update(
                    dialog_peer,
                    PendingUpdate::Wire(telegram_api::Update::updateEditChannelMessage(update)),
                    new_pts,
                    pts_count,
                    promise,
                    "updateEditChannelMessage",
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updatePinnedChannelMessages(update)) => {
                let dialog_peer = telegram_api::Peer::peerChannel(telegram_api::peerChannel {
                    channel_id: update.channel_id,
                });
                let new_pts = update.pts;
                let pts_count = update.pts_count;
                self.sink.add_pending_channel_update(
                    Some(dialog_peer),
                    PendingUpdate::Wire(telegram_api::Update::updatePinnedChannelMessages(update)),
                    new_pts,
                    pts_count,
                    promise,
                    "updatePinnedChannelMessages",
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updatePtsChanged(_)) => {
                if env.get_option_integer("session_count") > 1 {
                    let old_pts = self.get_pts();
                    let new_pts = 1;
                    if old_pts != new_pts {
                        tdutils::log_warning!("PTS changes from {} from updatePtsChanged", old_pts);
                        self.save_pts(env, new_pts);
                        self.add_pts(env, new_pts);
                        self.get_difference(env, "updatePtsChanged");
                    }
                } else {
                    self.set_pts(env, i32::MAX, "updatePtsChanged");
                }
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateConfig(_)) => {
                self.sink.on_update_config();
                // promo_data_manager_->reload_promo_data() — M.7.15 flip.
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateDcOptions(update)) => {
                self.sink.on_dc_options_update(update.dc_options);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateMessageID(update)) => {
                tdutils::log_error!(
                    "Receive not in getDifference and not in on_pending_updates updateMessageID {}",
                    update.id
                );
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateNewScheduledMessage(update)) => {
                self.sink
                    .on_get_message_scheduled(update.message.unwrap_or_default());
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateChannelTooLong(update)) => {
                self.sink.on_update_channel_too_long(update, false);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateFolderPeers(update)) => {
                // messages_manager_->on_update_dialog_folder_id per peer —
                // the dialog-action seam (M.7.14 flip).
                self.sink.on_unsupported_update(
                    telegram_api::updateFolderPeers::ID,
                    "updateFolderPeers",
                    4130,
                );
                if update.pts > 0 {
                    let now = env.now();
                    self.add_pending_pts_update(
                        env,
                        PendingUpdate::Dummy,
                        update.pts,
                        update.pts_count,
                        now,
                        Promise::default(),
                        "updateFolderPeers",
                    );
                }
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateWebPage(update)) => {
                // web_pages_manager_->on_get_web_page — M.7.15 flip.
                self.sink.on_unsupported_update(
                    telegram_api::updateWebPage::ID,
                    "updateWebPage",
                    4140,
                );
                let pts = update.pts;
                let now = env.now();
                self.add_pending_pts_update(
                    env,
                    PendingUpdate::Dummy,
                    pts,
                    update.pts_count,
                    now,
                    Promise::default(),
                    "updateWebPage",
                );
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateChannelWebPage(update)) => {
                // web_pages_manager_->on_get_web_page — M.7.15 flip; the
                // channel pts still advances through the dummy update
                // (cpp:4093).
                self.sink.on_unsupported_update(
                    telegram_api::updateChannelWebPage::ID,
                    "updateChannelWebPage",
                    4095,
                );
                let dialog_peer = telegram_api::Peer::peerChannel(telegram_api::peerChannel {
                    channel_id: update.channel_id,
                });
                let pts = update.pts;
                let pts_count = update.pts_count;
                self.sink.add_pending_channel_update(
                    Some(dialog_peer),
                    PendingUpdate::Dummy,
                    pts,
                    pts_count,
                    Promise::default(),
                    "updateChannelWebPage",
                );
                promise.set_value(());
            }

            // ---- M.7.12 — the UserManager group (real) ---------------
            PendingUpdate::Wire(telegram_api::Update::updateUserStatus(update)) => {
                self.sink
                    .on_update_user_status(update.user_id, update.status.unwrap_or_default());
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateUserName(update)) => {
                self.sink
                    .on_update_user_name(update.user_id, update.first_name, update.last_name);
                self.sink.on_update_user_usernames(
                    update.user_id,
                    crate::usernames::Usernames::from_telegram(String::new(), update.usernames),
                );
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateUserPhone(update)) => {
                self.sink.on_update_user_phone(update.user_id, update.phone);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateUser(update)) => {
                self.sink.invalidate_user_full(update.user_id);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateUserEmojiStatus(update)) => {
                self.sink.on_update_user_emoji_status(
                    update.user_id,
                    update.emoji_status.unwrap_or_default(),
                );
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateContactsReset(_)) => {
                self.sink.on_update_contacts_reset();
                promise.set_value(());
            }

            // ---- the post-authorization auth ignores (cpp:4887/4892) --
            PendingUpdate::Wire(telegram_api::Update::updateLoginToken(_)) => {
                tdutils::log_info!("Ignore updateLoginToken after authorization");
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateSentPhoneCode(_)) => {
                tdutils::log_info!("Ignore updateSentPhoneCode after authorization");
                promise.set_value(());
            }

            // ---- M.7.13 — the ephemeral-message family ---------------
            PendingUpdate::Wire(telegram_api::Update::updateNewEphemeralMessage(update)) => {
                let message = update.message.unwrap_or_default();
                let is_welcome = message.welcome_template;
                self.sink.on_new_ephemeral_message(message, is_welcome);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateEditEphemeralMessage(update)) => {
                let message = update.message.unwrap_or_default();
                let is_welcome = message.welcome_template;
                self.sink.on_edited_ephemeral_message(message, is_welcome);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateDeleteEphemeralMessages(update)) => {
                self.sink
                    .on_delete_ephemeral_messages(update.peer.unwrap_or_default(), update.ids);
                promise.set_value(());
            }

            // ---- M.7.13 — the message-family seams -------------------
            PendingUpdate::Wire(telegram_api::Update::updateServiceNotification(update)) => {
                self.sink.on_update_service_notification(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateReadChannelInbox(update)) => {
                self.sink.on_update_read_channel_inbox(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateReadChannelOutbox(update)) => {
                self.sink.on_update_read_channel_outbox(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateChannelReadMessagesContents(
                update,
            )) => {
                self.sink.on_update_read_channel_messages_contents(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateChannelAvailableMessages(update)) => {
                if !ChannelId::new(update.channel_id).is_valid() {
                    tdutils::log_error!("Receive invalid channel id {}", update.channel_id);
                } else {
                    self.sink
                        .on_update_channel_max_unavailable_message_id(update);
                }
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateChannelViewForumAsMessages(update)) => {
                self.sink.on_update_dialog_view_as_messages(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateReadChannelDiscussionInbox(update)) => {
                self.sink.on_update_read_channel_discussion_inbox(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateReadChannelDiscussionOutbox(
                update,
            )) => {
                self.sink.on_update_read_channel_discussion_outbox(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updatePeerSettings(update)) => {
                self.sink.on_get_peer_settings(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updatePeerHistoryTTL(update)) => {
                self.sink.on_update_dialog_message_ttl(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateChannelMessageViews(update)) => {
                if !ChannelId::new(update.channel_id).is_valid() {
                    tdutils::log_error!("Receive invalid channel id {}", update.channel_id);
                } else {
                    self.sink.on_update_message_view_count(update);
                }
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateChannelMessageForwards(update)) => {
                if !ChannelId::new(update.channel_id).is_valid() {
                    tdutils::log_error!("Receive invalid channel id {}", update.channel_id);
                } else {
                    self.sink.on_update_message_forward_count(update);
                }
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateGeoLiveViewed(update)) => {
                self.sink.on_update_live_location_viewed(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateMessageExtendedMedia(update)) => {
                self.sink.on_update_message_extended_media(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateMessageReactions(update)) => {
                self.sink.on_update_message_reactions(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updatePendingJoinRequests(update)) => {
                self.sink.on_update_dialog_pending_join_requests(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updatePeerBlocked(update)) => {
                self.sink.on_update_dialog_is_blocked(update);
                promise.set_value(());
            }

            // ---- M.7.14 — the dialog-family seams --------------------
            PendingUpdate::Wire(telegram_api::Update::updateDraftMessage(update)) => {
                self.sink.on_update_dialog_draft_message(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateDialogPinned(update)) => {
                self.sink.on_update_dialog_is_pinned(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updatePinnedDialogs(update)) => {
                self.sink.on_update_pinned_dialogs(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateDialogUnreadMark(update)) => {
                self.sink.on_update_dialog_is_marked_as_unread(update);
                promise.set_value(());
            }

            // ---- the qts family: every C++ overload is exactly
            // add_pending_qts_update (cpp:4629-5087) -------------------
            PendingUpdate::Wire(telegram_api::Update::updateNewEncryptedMessage(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateNewEncryptedMessage(update)),
                    qts,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateMessagePollVote(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateMessagePollVote(update)),
                    qts,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateManagedBot(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateManagedBot(update)),
                    qts,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotStopped(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateBotStopped(update)),
                    qts,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateChatParticipant(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateChatParticipant(update)),
                    qts,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateChannelParticipant(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateChannelParticipant(update)),
                    qts,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotChatInviteRequester(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateBotChatInviteRequester(update)),
                    qts,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotChatBoost(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateBotChatBoost(update)),
                    qts,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotMessageReaction(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateBotMessageReaction(update)),
                    qts,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotMessageReactions(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateBotMessageReactions(update)),
                    qts,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotPurchasedPaidMedia(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateBotPurchasedPaidMedia(update)),
                    qts,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotGuestChatQuery(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateBotGuestChatQuery(update)),
                    qts,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotBusinessConnect(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateBotBusinessConnect(update)),
                    qts,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotNewBusinessMessage(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateBotNewBusinessMessage(update)),
                    qts,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotEditBusinessMessage(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateBotEditBusinessMessage(update)),
                    qts,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotDeleteBusinessMessage(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateBotDeleteBusinessMessage(
                        update,
                    )),
                    qts,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotStarsSubscription(update)) => {
                let qts = update.qts;
                self.add_pending_qts_update(
                    env,
                    PendingUpdate::Wire(telegram_api::Update::updateBotStarsSubscription(update)),
                    qts,
                    promise,
                );
            }

            // ---- the no-op overloads ---------------------------------
            PendingUpdate::Wire(telegram_api::Update::updatePeerLocated(_)) => {
                // shouldn't be sent by the server (cpp:4081)
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateNewStoryReaction(_)) => {
                // the unsupported-updates tail (cpp:5146)
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateWalletState(update)) => {
                // cpp:5124-5127 — the TonWalletManager state intake
                // through the sink (the drain holds the env).
                self.sink
                    .on_update_wallet_state(update.state.unwrap_or_default());
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateWalletGaslessInfo(update)) => {
                // cpp:5129-5131 — the gasless-info intake.
                self.sink.on_update_wallet_gasless_info(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateSentWalletTransaction(_)) => {
                // cpp:5133-5136 — "Ignore unexpected" (the extract form
                // lives on the SendWalletTransferQuery tail).
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateWalletTonConnectSession(update)) => {
                // cpp:5138-5140 — the TonConnect-session intake.
                self.sink
                    .on_update_wallet_ton_connect_session(update.session.unwrap_or_default());
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateWalletTonConnectPendingDisconnect(
                update,
            )) => {
                // cpp:5142-5147 — the disconnect-required update.
                self.sink
                    .on_update_wallet_ton_connect_disconnect(update.session_ids);
                promise.set_value(());
            }

            // ---- Wave-2/3 enumerated trace drops ----------------------
            PendingUpdate::Wire(telegram_api::Update::updateAttachMenuBots(_)) => {
                // attach_menu_manager reload — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateAttachMenuBots::ID,
                    "updateAttachMenuBots",
                    4123,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updatePaidReactionPrivacy(_)) => {
                // reaction_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updatePaidReactionPrivacy::ID,
                    "updatePaidReactionPrivacy",
                    4118,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateRecentReactions(_)) => {
                // reaction_manager reload — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateRecentReactions::ID,
                    "updateRecentReactions",
                    4108,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateSavedReactionTags(_)) => {
                // reaction_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateSavedReactionTags::ID,
                    "updateSavedReactionTags",
                    4113,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateWebViewResultSent(_)) => {
                // web_app_manager + td_api update — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateWebViewResultSent::ID,
                    "updateWebViewResultSent",
                    4127,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateRecentEmojiStatuses(_)) => {
                // get_recent_emoji_statuses reload — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateRecentEmojiStatuses::ID,
                    "updateRecentEmojiStatuses",
                    4392,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateSavedDialogPinned(_)) => {
                // saved_messages_manager reload — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateSavedDialogPinned::ID,
                    "updateSavedDialogPinned",
                    4491,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updatePinnedSavedDialogs(_)) => {
                // saved_messages_manager reload — Wave 2.
                self.drop_unsupported(
                    telegram_api::updatePinnedSavedDialogs::ID,
                    "updatePinnedSavedDialogs",
                    4496,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateDialogFilter(_)) => {
                // dialog_filter_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateDialogFilter::ID,
                    "updateDialogFilter",
                    4511,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateDialogFilters(_)) => {
                // dialog_filter_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateDialogFilters::ID,
                    "updateDialogFilters",
                    4515,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateDialogFilterOrder(_)) => {
                // dialog_filter_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateDialogFilterOrder::ID,
                    "updateDialogFilterOrder",
                    4519,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateUserTyping(_)) => {
                // dialog_action_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateUserTyping::ID,
                    "updateUserTyping",
                    4332,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateChatUserTyping(_)) => {
                // dialog_action_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateChatUserTyping::ID,
                    "updateChatUserTyping",
                    4340,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateChannelUserTyping(_)) => {
                // dialog_action_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateChannelUserTyping::ID,
                    "updateChannelUserTyping",
                    4348,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateEncryptedChatTyping(_)) => {
                // dialog_action_manager + secret chats — Wave 3.
                self.drop_unsupported(
                    telegram_api::updateEncryptedChatTyping::ID,
                    "updateEncryptedChatTyping",
                    4356,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateChat(_)) => {
                // dialog_manager on_dialog_info_full_invalidated — M.6 flip.
                self.drop_unsupported(telegram_api::updateChat::ID, "updateChat", 3839, promise);
            }
            PendingUpdate::Wire(telegram_api::Update::updateChannel(_)) => {
                // chat_manager invalidate_channel_full — M.5 flip.
                self.drop_unsupported(
                    telegram_api::updateChannel::ID,
                    "updateChannel",
                    3865,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateChatParticipants(_)) => {
                // chat_manager on_get_chat_participants — M.5 flip.
                self.drop_unsupported(
                    telegram_api::updateChatParticipants::ID,
                    "updateChatParticipants",
                    4414,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateChatParticipantAdd(_)) => {
                // chat_manager on_update_chat_add_user — M.5 flip.
                self.drop_unsupported(
                    telegram_api::updateChatParticipantAdd::ID,
                    "updateChatParticipantAdd",
                    4419,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateChatParticipantAdmin(_)) => {
                // chat_manager on_update_chat_edit_administrator — M.5 flip.
                self.drop_unsupported(
                    telegram_api::updateChatParticipantAdmin::ID,
                    "updateChatParticipantAdmin",
                    4425,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateChatParticipantRank(_)) => {
                // chat_manager on_update_chat_participant_rank — M.5 flip.
                self.drop_unsupported(
                    telegram_api::updateChatParticipantRank::ID,
                    "updateChatParticipantRank",
                    4432,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateChatParticipantDelete(_)) => {
                // chat_manager on_update_chat_delete_user — M.5 flip.
                self.drop_unsupported(
                    telegram_api::updateChatParticipantDelete::ID,
                    "updateChatParticipantDelete",
                    4438,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateChatDefaultBannedRights(_)) => {
                // chat_manager default-permissions pair — M.5 flip.
                self.drop_unsupported(
                    telegram_api::updateChatDefaultBannedRights::ID,
                    "updateChatDefaultBannedRights",
                    4444,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updatePinnedForumTopic(_)) => {
                // forum_topic_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updatePinnedForumTopic::ID,
                    "updatePinnedForumTopic",
                    3989,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updatePinnedForumTopics(_)) => {
                // forum_topic_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updatePinnedForumTopics::ID,
                    "updatePinnedForumTopics",
                    3995,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateReadMonoForumInbox(_)) => {
                // saved_messages_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateReadMonoForumInbox::ID,
                    "updateReadMonoForumInbox",
                    3957,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateReadMonoForumOutbox(_)) => {
                // saved_messages_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateReadMonoForumOutbox::ID,
                    "updateReadMonoForumOutbox",
                    3969,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateMonoForumNoPaidException(_)) => {
                // saved_messages_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateMonoForumNoPaidException::ID,
                    "updateMonoForumNoPaidException",
                    3981,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateNotifySettings(_)) => {
                // messages/notification/forum scope split — M.6/Wave 2.
                self.drop_unsupported(
                    telegram_api::updateNotifySettings::ID,
                    "updateNotifySettings",
                    4025,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateSavedRingtones(_)) => {
                // notification_settings_manager reload — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateSavedRingtones::ID,
                    "updateSavedRingtones",
                    4968,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotCommands(_)) => {
                // dialog_manager on_update_dialog_bot_commands — M.6 flip.
                self.drop_unsupported(
                    telegram_api::updateBotCommands::ID,
                    "updateBotCommands",
                    4403,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotMenuButton(_)) => {
                // user_manager on_update_bot_menu_button — flip with the
                // bot-menu-button step.
                self.drop_unsupported(
                    telegram_api::updateBotMenuButton::ID,
                    "updateBotMenuButton",
                    4409,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateJoinChatWebViewDecision(_)) => {
                // dialog_manager + td_api update — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateJoinChatWebViewDecision::ID,
                    "updateJoinChatWebViewDecision",
                    4523,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotInlineQuery(_)) => {
                // inline_queries_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateBotInlineQuery::ID,
                    "updateBotInlineQuery",
                    4543,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotInlineSend(_)) => {
                // inline_queries_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateBotInlineSend::ID,
                    "updateBotInlineSend",
                    4550,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotCallbackQuery(_)) => {
                // callback_queries_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateBotCallbackQuery::ID,
                    "updateBotCallbackQuery",
                    4556,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateEphemeralBotCallbackQuery(_)) => {
                // callback_queries_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateEphemeralBotCallbackQuery::ID,
                    "updateEphemeralBotCallbackQuery",
                    4563,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateInlineBotCallbackQuery(_)) => {
                // callback_queries_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateInlineBotCallbackQuery::ID,
                    "updateInlineBotCallbackQuery",
                    4571,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBusinessBotCallbackQuery(_)) => {
                // callback_queries_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateBusinessBotCallbackQuery::ID,
                    "updateBusinessBotCallbackQuery",
                    4579,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateFavedStickers(_)) => {
                // stickers_manager reload — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateFavedStickers::ID,
                    "updateFavedStickers",
                    4587,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateSavedGifs(_)) => {
                // animations_manager reload — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateSavedGifs::ID,
                    "updateSavedGifs",
                    4592,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateNewStickerSet(_)) => {
                // stickers_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateNewStickerSet::ID,
                    "updateNewStickerSet",
                    4645,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateStickerSets(_)) => {
                // stickers_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateStickerSets::ID,
                    "updateStickerSets",
                    4651,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateStickerSetsOrder(_)) => {
                // stickers_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateStickerSetsOrder::ID,
                    "updateStickerSetsOrder",
                    4657,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateMoveStickerSetToTop(_)) => {
                // stickers_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateMoveStickerSetToTop::ID,
                    "updateMoveStickerSetToTop",
                    4663,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateReadFeaturedStickers(_)) => {
                // stickers_manager reload — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateReadFeaturedStickers::ID,
                    "updateReadFeaturedStickers",
                    4669,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateReadFeaturedEmojiStickers(_)) => {
                // stickers_manager reload — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateReadFeaturedEmojiStickers::ID,
                    "updateReadFeaturedEmojiStickers",
                    4675,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateRecentStickers(_)) => {
                // stickers_manager reload — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateRecentStickers::ID,
                    "updateRecentStickers",
                    4681,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateEmojiGameInfo(_)) => {
                // message_query_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateEmojiGameInfo::ID,
                    "updateEmojiGameInfo",
                    4603,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateEncryption(_)) => {
                // secret_chats_manager — Wave 3.
                self.drop_unsupported(
                    telegram_api::updateEncryption::ID,
                    "updateEncryption",
                    4624,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateEncryptedMessagesRead(_)) => {
                // messages_manager read_secret_chat_outbox — Wave 3.
                self.drop_unsupported(
                    telegram_api::updateEncryptedMessagesRead::ID,
                    "updateEncryptedMessagesRead",
                    4634,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updatePrivacy(_)) => {
                // privacy_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updatePrivacy::ID,
                    "updatePrivacy",
                    4640,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotShippingQuery(_)) => {
                // td_api bot-query surface — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateBotShippingQuery::ID,
                    "updateBotShippingQuery",
                    4687,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotPrecheckoutQuery(_)) => {
                // td_api bot-query surface — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateBotPrecheckoutQuery::ID,
                    "updateBotPrecheckoutQuery",
                    4704,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotWebhookJSON(_)) => {
                // td_api custom event — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateBotWebhookJSON::ID,
                    "updateBotWebhookJSON",
                    4720,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateBotWebhookJSONQuery(_)) => {
                // td_api custom query — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateBotWebhookJSONQuery::ID,
                    "updateBotWebhookJSONQuery",
                    4725,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updatePhoneCall(update)) => {
                // CallManager::update_call — M.32; the td-level sink
                // routes to the manager (the ZST stand-in no-ops until
                // the sink-state refactor).
                self.sink.on_update_phone_call(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updatePhoneCallSignalingData(update)) => {
                // CallManager::update_call_signaling_data — M.32.
                self.sink
                    .on_update_phone_call_signaling_data(update.phone_call_id, update.data);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateGroupCallConnection(_)) => {
                // group_call_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateGroupCallConnection::ID,
                    "updateGroupCallConnection",
                    4743,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateGroupCallChainBlocks(_)) => {
                // group_call_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateGroupCallChainBlocks::ID,
                    "updateGroupCallChainBlocks",
                    4753,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateGroupCall(_)) => {
                // group_call_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateGroupCall::ID,
                    "updateGroupCall",
                    4762,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateGroupCallParticipants(_)) => {
                // group_call_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateGroupCallParticipants::ID,
                    "updateGroupCallParticipants",
                    4768,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateGroupCallMessage(_)) => {
                // group_call_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateGroupCallMessage::ID,
                    "updateGroupCallMessage",
                    4775,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateGroupCallEncryptedMessage(_)) => {
                // group_call_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateGroupCallEncryptedMessage::ID,
                    "updateGroupCallEncryptedMessage",
                    4781,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateDeleteGroupCallMessages(_)) => {
                // group_call_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateDeleteGroupCallMessages::ID,
                    "updateDeleteGroupCallMessages",
                    4789,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateStarGiftAuctionState(_)) => {
                // star_gift_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateStarGiftAuctionState::ID,
                    "updateStarGiftAuctionState",
                    4801,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateStarGiftAuctionUserState(_)) => {
                // star_gift_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateStarGiftAuctionUserState::ID,
                    "updateStarGiftAuctionUserState",
                    4807,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateLangPackTooLong(update)) => {
                // UpdatesManager::on_update (cpp:4818-4823, M.24); the
                // sink impl lands with the net step
                self.sink.on_update_lang_pack_too_long(update.lang_code);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateLangPack(update)) => {
                // UpdatesManager::on_update (cpp:4824-4829, M.24); the
                // sink impl lands with the net step
                if let Some(difference) = update.difference {
                    self.sink.on_update_lang_pack(difference);
                }
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateMessagePoll(_)) => {
                // poll_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateMessagePoll::ID,
                    "updateMessagePoll",
                    4843,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateDeleteScheduledMessages(_)) => {
                // messages_manager scheduled-delete + wait_message_add —
                // the MessagesManager slice flip.
                self.drop_unsupported(
                    telegram_api::updateDeleteScheduledMessages::ID,
                    "updateDeleteScheduledMessages",
                    4864,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateTheme(_)) => {
                // theme_manager — Wave 2.
                self.drop_unsupported(telegram_api::updateTheme::ID, "updateTheme", 4949, promise);
            }
            PendingUpdate::Wire(telegram_api::Update::updatePeerWallpaper(_)) => {
                // user_manager wallpaper + messages_manager background —
                // M.6/Wave 2.
                self.drop_unsupported(
                    telegram_api::updatePeerWallpaper::ID,
                    "updatePeerWallpaper",
                    4953,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateTranscribedAudio(update)) => {
                // `UpdatesManager::on_update(updateTranscribedAudio)`
                // (UpdatesManager.cpp:4972-4975) — the intake landed with
                // M.25 (`TranscriptionManager::on_update_transcribed_audio`)
                // and rides the sink (the ZST cannot reach the manager
                // yet; the live flow arrives with the net step).
                self.sink.on_update_transcribed_audio(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateAutoSaveSettings(_)) => {
                // autosave_manager reload — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateAutoSaveSettings::ID,
                    "updateAutoSaveSettings",
                    4977,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateStory(_)) => {
                // story_manager — Wave 2.
                self.drop_unsupported(telegram_api::updateStory::ID, "updateStory", 4982, promise);
            }
            PendingUpdate::Wire(telegram_api::Update::updateReadStories(_)) => {
                // story_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateReadStories::ID,
                    "updateReadStories",
                    4987,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateStoriesStealthMode(_)) => {
                // story_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateStoriesStealthMode::ID,
                    "updateStoriesStealthMode",
                    4992,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateSentStoryReaction(_)) => {
                // story_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateSentStoryReaction::ID,
                    "updateSentStoryReaction",
                    4997,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateNewAuthorization(_)) => {
                // account_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateNewAuthorization::ID,
                    "updateNewAuthorization",
                    5008,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateNewBotConnection(_)) => {
                // account_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateNewBotConnection::ID,
                    "updateNewBotConnection",
                    5018,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateQuickReplies(_)) => {
                // quick_reply_manager reload — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateQuickReplies::ID,
                    "updateQuickReplies",
                    5033,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateNewQuickReply(_)) => {
                // quick_reply_manager reload — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateNewQuickReply::ID,
                    "updateNewQuickReply",
                    5038,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateDeleteQuickReply(_)) => {
                // quick_reply_manager reload — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateDeleteQuickReply::ID,
                    "updateDeleteQuickReply",
                    5043,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateQuickReplyMessage(_)) => {
                // quick_reply_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateQuickReplyMessage::ID,
                    "updateQuickReplyMessage",
                    5048,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateDeleteQuickReplyMessages(_)) => {
                // quick_reply_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateDeleteQuickReplyMessages::ID,
                    "updateDeleteQuickReplyMessages",
                    5053,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateStarsBalance(_)) => {
                // star_manager — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateStarsBalance::ID,
                    "updateStarsBalance",
                    5093,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateStarsRevenueStatus(_)) => {
                // star/statistics managers — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateStarsRevenueStatus::ID,
                    "updateStarsRevenueStatus",
                    5109,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateAiComposeTones(_)) => {
                // translation_manager reload — Wave 2.
                self.drop_unsupported(
                    telegram_api::updateAiComposeTones::ID,
                    "updateAiComposeTones",
                    5129,
                    promise,
                );
            }
            PendingUpdate::Wire(telegram_api::Update::updateWebBrowserSettings(update)) => {
                // WebBrowserManager::on_update_web_browser_settings — M.31;
                // the td-level sink routes to the manager (the ZST
                // stand-in no-ops until the sink-state refactor).
                self.sink.on_update_web_browser_settings(update);
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateWebBrowserException(update)) => {
                // WebBrowserManager::on_update_web_browser_exception — M.31.
                self.sink.on_update_web_browser_exception(update);
                promise.set_value(());
            }

            // ---- the C++ log-only overloads --------------------------
            PendingUpdate::Wire(telegram_api::Update::updateStarGiftCraftFail(_)) => {
                tdutils::log_error!("Receive unexpected updateStarGiftCraftFail");
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateStoryID(_)) => {
                tdutils::log_error!(
                    "Receive not in getDifference and not in on_pending_updates updateStoryID"
                );
                promise.set_value(());
            }
            PendingUpdate::Wire(telegram_api::Update::updateSmsJob(_)) => {
                tdutils::log_error!("Receive updateSmsJob");
                promise.set_value(());
            }
            PendingUpdate::Dummy | PendingUpdate::SentMessage { .. } => {
                // The synthetics enter the engine only through
                // add_pending_pts_update/process_pts_update, never through
                // the dispatcher.
                tdutils::log_error!("Synthetic update {} in on_update", update.name());
                promise.set_value(());
            }
        }
    }

    /// `UpdatesManager::is_acceptable_peer` (cpp:733) — an absent peer
    /// (the C++ nullptr; the wire default encodes the zero id) is
    /// acceptable.
    fn is_acceptable_peer(&mut self, peer: &telegram_api::Peer) -> bool {
        if peer_id_is_zero(peer) {
            return true;
        }
        let dialog_id = DialogId::from_peer(peer);
        match dialog_id.get_type() {
            DialogType::User => self.sink.is_acceptable_user(dialog_id.get_user_id().get()),
            DialogType::Chat => self.sink.is_acceptable_chat(dialog_id.get_chat_id().get()),
            DialogType::Channel => self
                .sink
                .is_acceptable_channel(dialog_id.get_channel_id().get()),
            // The C++ UNREACHABLE arms: wire peers carry no secret-chat
            // dialogs and the zero dialog was excluded above; reject (the
            // LOG_CHECK policy).
            DialogType::None | DialogType::SecretChat => false,
        }
    }

    /// `UpdatesManager::is_acceptable_message_entities` (cpp:766) — the
    /// mention-name entities need a constructible InputUser.
    fn is_acceptable_message_entities(&mut self, entities: &[telegram_api::MessageEntity]) -> bool {
        for entity in entities {
            if let telegram_api::MessageEntity::messageEntityMentionName(entity) = entity
                && (!self.sink.is_acceptable_user(entity.user_id)
                    || !self.sink.can_get_input_user(entity.user_id))
            {
                return false;
            }
        }
        true
    }

    /// `UpdatesManager::is_acceptable_reply_markup` (cpp:780) — only the
    /// inline markup carries user-profile buttons.
    fn is_acceptable_reply_markup(&mut self, reply_markup: &telegram_api::ReplyMarkup) -> bool {
        let telegram_api::ReplyMarkup::replyInlineMarkup(markup) = reply_markup else {
            return true;
        };
        for row in &markup.rows {
            for button in &row.buttons {
                if let Some(telegram_api::InlineButtonType::inlineButtonTypeUserProfile(button)) =
                    &button.r#type
                    && (!self.sink.is_acceptable_user(button.user_id)
                        || !self.sink.can_get_input_user(button.user_id))
                {
                    return false;
                }
            }
        }
        true
    }

    /// `UpdatesManager::is_acceptable_message_reply_header` (cpp:798).
    fn is_acceptable_message_reply_header(
        &mut self,
        header: &telegram_api::MessageReplyHeader,
    ) -> bool {
        match header {
            telegram_api::MessageReplyHeader::messageReplyHeader(header) => {
                self.is_acceptable_peer(
                    header
                        .reply_to_peer_id
                        .as_ref()
                        .unwrap_or(&telegram_api::Peer::default()),
                ) && self.is_acceptable_message_forward_header(
                    header
                        .reply_from
                        .as_ref()
                        .unwrap_or(&telegram_api::MessageFwdHeader::default()),
                ) && self.is_acceptable_message_entities(&header.quote_entities)
                    && self.is_acceptable_message_media(
                        header
                            .reply_media
                            .as_deref()
                            .unwrap_or(&telegram_api::MessageMedia::default()),
                    )
            }
            telegram_api::MessageReplyHeader::messageReplyStoryHeader(header) => self
                .is_acceptable_peer(
                    header
                        .peer
                        .as_ref()
                        .unwrap_or(&telegram_api::Peer::default()),
                ),
        }
    }

    /// `UpdatesManager::is_acceptable_message_forward_header` (cpp:822).
    fn is_acceptable_message_forward_header(
        &mut self,
        header: &telegram_api::MessageFwdHeader,
    ) -> bool {
        self.is_acceptable_peer(
            header
                .from_id
                .as_ref()
                .unwrap_or(&telegram_api::Peer::default()),
        ) && self.is_acceptable_peer(
            header
                .saved_from_peer
                .as_ref()
                .unwrap_or(&telegram_api::Peer::default()),
        )
    }

    /// `UpdatesManager::is_acceptable_message_media` (cpp:839) — the
    /// poll/web-page/todo payloads always carry min users and chats (the
    /// C++ comment), everything unrecognized is acceptable.
    fn is_acceptable_message_media(&mut self, media: &telegram_api::MessageMedia) -> bool {
        match media {
            telegram_api::MessageMedia::messageMediaContact(media) => {
                media.user_id == 0 || self.sink.is_acceptable_user(media.user_id)
            }
            telegram_api::MessageMedia::messageMediaStory(media) => self.is_acceptable_peer(
                media
                    .peer
                    .as_ref()
                    .unwrap_or(&telegram_api::Peer::default()),
            ),
            telegram_api::MessageMedia::messageMediaGiveaway(media) => media
                .channels
                .iter()
                .all(|channel_id| self.sink.is_acceptable_channel(*channel_id)),
            telegram_api::MessageMedia::messageMediaGiveawayResults(media) => {
                self.sink.is_acceptable_channel(media.channel_id)
                    && media
                        .winners
                        .iter()
                        .all(|user_id| self.sink.is_acceptable_user(*user_id))
            }
            _ => true,
        }
    }

    /// `UpdatesManager::is_acceptable_message` (cpp:887).
    fn is_acceptable_message(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        message: &telegram_api::Message,
    ) -> bool {
        match message {
            telegram_api::Message::messageEmpty(_) => true,
            telegram_api::Message::message(message) => {
                self.is_acceptable_peer(
                    message
                        .peer_id
                        .as_ref()
                        .unwrap_or(&telegram_api::Peer::default()),
                ) && self.is_acceptable_peer(
                    message
                        .from_id
                        .as_ref()
                        .unwrap_or(&telegram_api::Peer::default()),
                ) && self.is_acceptable_message_reply_header(
                    message
                        .reply_to
                        .as_ref()
                        .unwrap_or(&telegram_api::MessageReplyHeader::default()),
                ) && self.is_acceptable_message_forward_header(
                    message
                        .fwd_from
                        .as_ref()
                        .unwrap_or(&telegram_api::MessageFwdHeader::default()),
                ) && (message.via_bot_id == 0 || self.sink.is_acceptable_user(message.via_bot_id))
                    && self.is_acceptable_message_entities(&message.entities)
                    && self.is_acceptable_message_media(
                        message
                            .media
                            .as_deref()
                            .unwrap_or(&telegram_api::MessageMedia::default()),
                    )
                    && self.is_acceptable_reply_markup(
                        message
                            .reply_markup
                            .as_ref()
                            .unwrap_or(&telegram_api::ReplyMarkup::default()),
                    )
            }
            telegram_api::Message::messageService(message) => {
                if !self.is_acceptable_peer(
                    message
                        .peer_id
                        .as_ref()
                        .unwrap_or(&telegram_api::Peer::default()),
                ) || !self.is_acceptable_peer(
                    message
                        .from_id
                        .as_ref()
                        .unwrap_or(&telegram_api::Peer::default()),
                ) {
                    return false;
                }
                self.is_acceptable_message_action(
                    env,
                    message
                        .action
                        .as_ref()
                        .unwrap_or(&telegram_api::MessageAction::default()),
                )
            }
        }
    }

    /// The messageService action switch of cpp:897-1136. The C++
    /// allowlist passes; the checked actions consult the sink. The match
    /// is exhaustive over the pinned scheme's MessageAction — a new
    /// codegen variant fails to compile until it is classified here (the
    /// C++ `UNREACHABLE` default, reject-at-build-time).
    fn is_acceptable_message_action(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        action: &telegram_api::MessageAction,
    ) -> bool {
        use telegram_api::MessageAction as MA;
        match action {
            MA::messageActionEmpty(_)
            | MA::messageActionChatEditTitle(_)
            | MA::messageActionChatEditPhoto(_)
            | MA::messageActionChatDeletePhoto(_)
            | MA::messageActionCustomAction(_)
            | MA::messageActionBotAllowed(_)
            | MA::messageActionHistoryClear(_)
            | MA::messageActionChannelCreate(_)
            | MA::messageActionPinMessage(_)
            | MA::messageActionGameScore(_)
            | MA::messageActionPhoneCall(_)
            | MA::messageActionPaymentSent(_)
            | MA::messageActionPaymentSentMe(_)
            | MA::messageActionScreenshotTaken(_)
            | MA::messageActionSecureValuesSent(_)
            | MA::messageActionSecureValuesSentMe(_)
            | MA::messageActionContactSignUp(_)
            | MA::messageActionGroupCall(_)
            | MA::messageActionGroupCallScheduled(_)
            | MA::messageActionSetChatTheme(_)
            | MA::messageActionChatJoinedByRequest(_)
            | MA::messageActionWebViewDataSentMe(_)
            | MA::messageActionWebViewDataSent(_)
            | MA::messageActionGiftPremium(_)
            | MA::messageActionTopicCreate(_)
            | MA::messageActionTopicEdit(_)
            | MA::messageActionSuggestProfilePhoto(_)
            | MA::messageActionSetChatWallPaper(_)
            | MA::messageActionGiveawayLaunch(_)
            | MA::messageActionGiveawayResults(_)
            | MA::messageActionBoostApply(_)
            | MA::messageActionRequestedPeerSentMe(_)
            | MA::messageActionGiftStars(_)
            | MA::messageActionPrizeStars(_)
            | MA::messageActionStarGift(_)
            | MA::messageActionStarGiftUnique(_)
            | MA::messageActionPaidMessagesRefunded(_)
            | MA::messageActionPaidMessagesPrice(_)
            | MA::messageActionTodoCompletions(_)
            | MA::messageActionTodoAppendTasks(_)
            | MA::messageActionSuggestedPostApproval(_)
            | MA::messageActionSuggestedPostSuccess(_)
            | MA::messageActionSuggestedPostRefund(_)
            | MA::messageActionGiftTon(_)
            | MA::messageActionSuggestBirthday(_)
            | MA::messageActionStarGiftPurchaseOffer(_)
            | MA::messageActionStarGiftPurchaseOfferDeclined(_)
            | MA::messageActionNoForwardsToggle(_)
            | MA::messageActionNoForwardsRequest(_)
            | MA::messageActionPollAppendAnswer(_)
            | MA::messageActionPollDeleteAnswer(_)
            | MA::messageActionManagedBotCreated(_)
            | MA::messageActionGramTransfer(_)
            | MA::messageActionWalletTonConnectRequest(_) => true,
            MA::messageActionChatCreate(action) => action
                .users
                .iter()
                .all(|user_id| self.sink.is_acceptable_user(*user_id)),
            MA::messageActionChatAddUser(action) => action
                .users
                .iter()
                .all(|user_id| self.sink.is_acceptable_user(*user_id)),
            // inviter_id_ isn't used (the C++ comment).
            MA::messageActionChatJoinedByLink(_) => true,
            MA::messageActionChatDeleteUser(action) => self.sink.is_acceptable_user(action.user_id),
            MA::messageActionChatMigrateTo(action) => {
                self.sink.is_acceptable_channel(action.channel_id)
            }
            MA::messageActionChannelMigrateFrom(action) => {
                self.sink.is_acceptable_chat(action.chat_id)
            }
            MA::messageActionGeoProximityReached(action) => {
                self.is_acceptable_peer(
                    action
                        .from_id
                        .as_ref()
                        .unwrap_or(&telegram_api::Peer::default()),
                ) && self.is_acceptable_peer(
                    action
                        .to_id
                        .as_ref()
                        .unwrap_or(&telegram_api::Peer::default()),
                )
            }
            MA::messageActionInviteToGroupCall(action) => action
                .users
                .iter()
                .all(|user_id| self.sink.is_acceptable_user(*user_id)),
            MA::messageActionSetMessagesTTL(action) => {
                action.auto_setting_from == 0
                    || self.sink.is_acceptable_user(action.auto_setting_from)
            }
            MA::messageActionRequestedPeer(action) => {
                env.is_bot()
                    || action
                        .peers
                        .iter()
                        .all(|peer| self.is_acceptable_peer(peer))
            }
            MA::messageActionGiftCode(action) => self.is_acceptable_peer(
                action
                    .boost_peer
                    .as_ref()
                    .unwrap_or(&telegram_api::Peer::default()),
            ),
            MA::messageActionPaymentRefunded(action) => self.is_acceptable_peer(
                action
                    .peer
                    .as_ref()
                    .unwrap_or(&telegram_api::Peer::default()),
            ),
            MA::messageActionConferenceCall(action) => action
                .other_participants
                .iter()
                .all(|peer| self.is_acceptable_peer(peer)),
            MA::messageActionNewCreatorPending(action) => {
                self.sink.is_acceptable_user(action.new_creator_id)
            }
            MA::messageActionChangeCreator(action) => {
                self.sink.is_acceptable_user(action.new_creator_id)
            }
            MA::messageActionChangeCommunity(action) => {
                action.community_id == 0 || self.sink.is_acceptable_community(action.community_id)
            }
            MA::messageActionChatJoinedViaCommunity(action) => {
                action.community_id == 0 || self.sink.is_acceptable_community(action.community_id)
            }
        }
    }

    /// `UpdatesManager::is_acceptable_update` (cpp:1148) — only the five
    /// message carriers are inspected.
    fn is_acceptable_update(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        update: &telegram_api::Update,
    ) -> bool {
        let message = match update {
            telegram_api::Update::updateNewMessage(update) => &update.message,
            telegram_api::Update::updateNewChannelMessage(update) => &update.message,
            telegram_api::Update::updateNewScheduledMessage(update) => &update.message,
            telegram_api::Update::updateEditMessage(update) => &update.message,
            telegram_api::Update::updateEditChannelMessage(update) => &update.message,
            _ => return true,
        };
        self.is_acceptable_message(
            env,
            message
                .as_ref()
                .unwrap_or(&telegram_api::Message::default()),
        )
    }

    // ------------------------------------------------------------------
    // M.7.16 — the aux machinery (cpp:1896-1957)
    // ------------------------------------------------------------------
    /// `UpdatesManager::get_most_unused_auth_key_id` (cpp:1896) — the
    /// session with the oldest update; 0 when everything is current.
    fn get_most_unused_auth_key_id(&self, env: &mut impl UpdatesManagerEnv) -> u64 {
        let mut min_auth_key_id = 0u64;
        let mut min_time = env.now();
        for (auth_key_id, info) in &self.session_infos {
            if info.last_update_time < min_time {
                min_time = info.last_update_time;
                min_auth_key_id = *auth_key_id;
            }
        }
        min_auth_key_id
    }

    /// `UpdatesManager::on_update_from_auth_key_id` (cpp:1908) — the
    /// per-session update accounting; a fresh session evicts the oldest
    /// session beyond the `session_count` option.
    pub fn on_update_from_auth_key_id(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        auth_key_id: u64,
    ) {
        if auth_key_id == 0 {
            return;
        }
        let now = env.now();
        let is_new = {
            let info = self.session_infos.entry(auth_key_id).or_default();
            info.last_update_time = now;
            let is_new = info.update_count == 0;
            info.update_count += 1;
            if is_new {
                info.first_update_time = now;
            }
            is_new
        };
        if is_new {
            let max_sessions = std::cmp::max(env.get_option_integer("session_count"), 1) as usize;
            while self.session_infos.len() > max_sessions {
                let unused_auth_key_id = self.get_most_unused_auth_key_id(env);
                tdutils::log_info!("Delete statistics for auth key {unused_auth_key_id}");
                if self.session_infos.remove(&unused_auth_key_id).is_none() {
                    break;
                }
            }
        }
    }

    /// `UpdatesManager::ping_server` (cpp:1926) — the pong drives the
    /// state intake; the query send is the `[blocked: net]` env seam.
    pub fn ping_server(&mut self, env: &mut impl UpdatesManagerEnv) {
        if self.is_ping_sent {
            return;
        }
        self.is_ping_sent = true;
        env.send_ping_server();
    }

    /// `UpdatesManager::on_server_pong` (cpp:1936) — `None` is the failed
    /// query (the C++ null state); a stale pong triggers getDifference.
    pub fn on_server_pong(
        &mut self,
        env: &mut impl UpdatesManagerEnv,
        state: Option<telegram_api::updates_State>,
    ) {
        tdutils::log_info!("Receive server pong");
        self.is_ping_sent = false;
        let is_stale = match state {
            None => true,
            Some(state) => state.pts > self.get_pts() || state.seq > self.seq,
        };
        if is_stale {
            self.get_difference(env, "on server pong");
        }
    }

    /// `UpdatesManager::notify_speed_limited` (cpp:1946) — the
    /// user-visible speed-limit notice, throttled by the
    /// `upload_premium_speedup_notify_period` option.
    pub fn notify_speed_limited(&mut self, env: &mut impl UpdatesManagerEnv, is_upload: bool) {
        let now = env.now();
        if now < self.next_notify_speed_limited[usize::from(is_upload)] {
            return;
        }
        self.next_notify_speed_limited[usize::from(is_upload)] =
            now + env.get_option_integer("upload_premium_speedup_notify_period") as f64;
        self.sink.on_speed_limit_notification(is_upload);
    }

    // ------------------------------------------------------------------
    // M.7.17 — the data reloaders (cpp:2373-2534)
    // ------------------------------------------------------------------

    /// `UpdatesManager::start_up`'s tail (cpp:308-310): anchor both data
    /// reload timers one second into the past, so the first online tick
    /// fires them (the C++ constructor zero value never passes the
    /// `now < next_data_reload_time_` gate).
    pub(crate) fn init_reload_times(&mut self, env: &mut impl UpdatesManagerEnv) {
        let now = env.now();
        self.next_data_reload_time = now - 1.0;
        self.next_often_data_reload_time = now - 1.0;
    }

    /// `UpdatesManager::hangup`'s queue half (cpp:305-311): the close path
    /// drops every pending update batch before the actor stops. The pmc
    /// flush is `timeout_expired`'s business (the close path calls it
    /// separately); this only clears the in-memory backlog.
    pub(crate) fn clear_pending_queues(&mut self) {
        self.pending_pts_updates.clear();
        self.postponed_pts_updates.clear();
        self.postponed_updates.clear();
        self.pending_seq_updates.clear();
        self.pending_qts_updates.clear();
    }

    /// `UpdatesManager::schedule_data_reload` (cpp:2373).
    fn schedule_data_reload(&mut self, env: &mut impl UpdatesManagerEnv) {
        if self.data_reload_timeout.has_timeout() {
            return;
        }
        let timeout = self.next_data_reload_time - env.now();
        self.data_reload_timeout.set_timeout_in(env.now(), timeout);
    }

    /// `UpdatesManager::try_reload_data` (cpp:2391) — the four
    /// preconditions (authorized, not a bot, no running getDifference,
    /// online), the 365-day one-shot gate and the
    /// [`UpdatesSink::reload_data`] fan-out.
    pub fn try_reload_data(&mut self, env: &mut impl UpdatesManagerEnv) {
        if !env.is_authorized() || env.is_bot() || self.running_get_difference || !env.is_online() {
            return;
        }
        let now = env.now();
        if now < self.next_data_reload_time {
            self.schedule_data_reload(env);
            return;
        }
        self.next_data_reload_time = now + f64::from(365 * 86400);
        self.sink.reload_data();
        self.on_data_reloaded(env);
    }

    /// `UpdatesManager::on_data_reloaded` (cpp:2478) — the C++ re-arm
    /// arrives through the reload promise; the synchronous sink collapses
    /// it to an immediate re-arm (the module-docs convention).
    pub fn on_data_reloaded(&mut self, env: &mut impl UpdatesManagerEnv) {
        if env.close_flag() {
            return;
        }
        self.next_data_reload_time = env.now() + f64::from(tdutils::random::fast(3000, 4200));
        self.data_reload_timeout.cancel_timeout();
        self.schedule_data_reload(env);
    }

    /// `UpdatesManager::schedule_often_data_reload` (cpp:2491).
    fn schedule_often_data_reload(&mut self, env: &mut impl UpdatesManagerEnv) {
        if self.often_data_reload_timeout.has_timeout() {
            return;
        }
        let timeout = self.next_often_data_reload_time - env.now();
        self.often_data_reload_timeout
            .set_timeout_in(env.now(), timeout);
    }

    /// `UpdatesManager::try_reload_often_data` (cpp:2511) — the same gate
    /// as [`Self::try_reload_data`] with the stories fan-out.
    pub fn try_reload_often_data(&mut self, env: &mut impl UpdatesManagerEnv) {
        if !env.is_authorized() || env.is_bot() || self.running_get_difference || !env.is_online() {
            return;
        }
        let now = env.now();
        if now < self.next_often_data_reload_time {
            self.schedule_often_data_reload(env);
            return;
        }
        self.next_often_data_reload_time = now + f64::from(365 * 86400);
        self.sink.reload_often_data();
        self.on_often_data_reloaded(env);
    }

    /// `UpdatesManager::on_often_data_reloaded` (cpp:2529).
    pub fn on_often_data_reloaded(&mut self, env: &mut impl UpdatesManagerEnv) {
        if env.close_flag() {
            return;
        }
        self.next_often_data_reload_time = env.now() + f64::from(tdutils::random::fast(300, 420));
        self.often_data_reload_timeout.cancel_timeout();
        self.schedule_often_data_reload(env);
    }
}

// ------------------------------------------------------------------
// M.7.8 — the container statics (the free functions other managers call on
// an `Updates` umbrella they already hold; the C++ statics on
// UpdatesManager, cpp:1412-1851).
// ------------------------------------------------------------------

/// The `td::MessageId` bit layout (MessageId.h:17-23) — the statics below
/// need the server/scheduled comparisons before the full `MessageId` port
/// lands with the MessagesManager slice (`chat_manager::MessageId` is the
/// opaque stand-in today).
const MESSAGE_ID_SERVER_SHIFT: u32 = 20;
const MESSAGE_ID_TYPE_MASK: i64 = (1 << 3) - 1;
const MESSAGE_ID_FULL_TYPE_MASK: i64 = (1 << MESSAGE_ID_SERVER_SHIFT) - 1;
const MESSAGE_ID_SCHEDULED_MASK: i64 = 4;
const MESSAGE_ID_TYPE_YET_UNSENT: i64 = 1;
const MESSAGE_ID_TYPE_LOCAL: i64 = 2;

/// `MessageId::max()` — MessageId.h:66.
const MESSAGE_ID_MAX: i64 = (i32::MAX as i64) << MESSAGE_ID_SERVER_SHIFT;

/// `MessageId(ServerMessageId)` — MessageId.h:57-59.
fn server_message_id_to_message_id(server_message_id: i32) -> i64 {
    (server_message_id as i64) << MESSAGE_ID_SERVER_SHIFT
}

/// `MessageId::is_valid` — MessageId.cpp:112-122.
fn is_valid_message_id(id: i64) -> bool {
    if id <= 0 || id > MESSAGE_ID_MAX {
        return false;
    }
    if (id & MESSAGE_ID_FULL_TYPE_MASK) == 0 {
        return true;
    }
    let type_bits = id & MESSAGE_ID_TYPE_MASK;
    type_bits == MESSAGE_ID_TYPE_YET_UNSENT || type_bits == MESSAGE_ID_TYPE_LOCAL
}

/// `MessageId::is_valid_scheduled` — MessageId.cpp:126-132.
fn is_valid_scheduled_message_id(id: i64) -> bool {
    if id <= 0 || id > 1i64 << 51 {
        return false;
    }
    let type_bits = id & MESSAGE_ID_TYPE_MASK;
    type_bits == MESSAGE_ID_SCHEDULED_MASK
        || type_bits == (MESSAGE_ID_SCHEDULED_MASK | MESSAGE_ID_TYPE_YET_UNSENT)
        || type_bits == (MESSAGE_ID_SCHEDULED_MASK | MESSAGE_ID_TYPE_LOCAL)
}

/// `MessageId::is_scheduled_server` — MessageId.h:126-129, with the C++
/// `CHECK(is_valid_scheduled())` folded into the predicate.
fn is_scheduled_server_message_id(id: i64) -> bool {
    is_valid_scheduled_message_id(id) && (id & MESSAGE_ID_TYPE_MASK) == MESSAGE_ID_SCHEDULED_MASK
}

/// `MessageId::get_scheduled_server_message_id_force` — MessageId.h:51-56.
fn scheduled_server_message_id_of(id: i64) -> i32 {
    ((id >> 3) & ((1 << 18) - 1)) as i32
}

/// `MessageId::get_server_message_id_force` — MessageId.h:47-49.
fn server_message_id_of(id: i64) -> i32 {
    (id >> MESSAGE_ID_SERVER_SHIFT) as i32
}

/// `MessageId(ScheduledServerMessageId, send_date)` — MessageId.cpp:15-26;
/// the two C++ `LOG(ERROR)`-and-zero paths collapse to 0.
fn scheduled_message_id_to_message_id(scheduled_server_message_id: i32, send_date: i32) -> i64 {
    if send_date <= 1 << 30 {
        tdutils::log_error!("Scheduled message send date {send_date} is in the past");
        return 0;
    }
    if !(0 < scheduled_server_message_id && scheduled_server_message_id < 1 << 18) {
        tdutils::log_error!("Scheduled message ID {scheduled_server_message_id} is invalid");
        return 0;
    }
    (((send_date - (1 << 30)) as i64) << 21)
        | ((scheduled_server_message_id as i64) << 3)
        | MESSAGE_ID_SCHEDULED_MASK
}

/// `MessageId::get_message_id(message, is_scheduled)` — MessageId.cpp:28-49.
fn message_id_from_message(message: &telegram_api::Message, is_scheduled: bool) -> i64 {
    match message {
        telegram_api::Message::messageEmpty(message) => {
            if is_scheduled {
                0
            } else {
                server_message_id_to_message_id(message.id)
            }
        }
        telegram_api::Message::message(message) => {
            if is_scheduled {
                scheduled_message_id_to_message_id(message.id, message.date)
            } else {
                server_message_id_to_message_id(message.id)
            }
        }
        telegram_api::Message::messageService(message) => {
            if is_scheduled {
                scheduled_message_id_to_message_id(message.id, message.date)
            } else {
                server_message_id_to_message_id(message.id)
            }
        }
    }
}

/// `DialogId::get_message_dialog_id(message)` — the peer of any message
/// carrier; `messageEmpty` yields the invalid dialog.
fn message_dialog_id(message: &Option<telegram_api::Message>) -> DialogId {
    match message_peer_id(message) {
        Some(peer) => DialogId::from_peer(peer),
        None => DialogId::new(0),
    }
}

/// `MessageFullId::get_message_full_id(message, is_scheduled)` — the
/// (dialog, message-id) pair; `messageEmpty` carries the invalid dialog.
fn message_full_id(message: &Option<telegram_api::Message>, is_scheduled: bool) -> (DialogId, i64) {
    (
        message_dialog_id(message),
        message_id_from_message(
            message
                .as_ref()
                .unwrap_or(&telegram_api::Message::default()),
            is_scheduled,
        ),
    )
}

/// `UpdatesManager::get_updates` (the C++ private helper) — the updates
/// vector of the two container umbrella variants.
fn get_updates_vec(updates: &telegram_api::Updates) -> Option<&Vec<telegram_api::Update>> {
    match updates {
        telegram_api::Updates::updates(updates) => Some(&updates.updates),
        telegram_api::Updates::updatesCombined(updates) => Some(&updates.updates),
        _ => None,
    }
}

/// The mutable variant, for the erasing extractors.
fn get_updates_vec_mut(
    updates: &mut telegram_api::Updates,
) -> Option<&mut Vec<telegram_api::Update>> {
    match updates {
        telegram_api::Updates::updates(updates) => Some(&mut updates.updates),
        telegram_api::Updates::updatesCombined(updates) => Some(&mut updates.updates),
        _ => None,
    }
}

/// `UpdatesManager::extract_sent_wallet_transaction` (63c084905,
/// UpdatesManager.cpp:1667-1677) — the first `updateSentWalletTransaction`
/// of the container, erased out (the `SendWalletTransferQuery` tail reads
/// it before feeding the container to `on_get_updates`).
pub fn extract_sent_wallet_transaction(
    updates: &mut telegram_api::Updates,
) -> Option<telegram_api::updateSentWalletTransaction> {
    let container = get_updates_vec_mut(updates)?;
    let index = container.iter().position(|update| {
        matches!(update, telegram_api::Update::updateSentWalletTransaction(_))
    })?;
    match container.remove(index) {
        telegram_api::Update::updateSentWalletTransaction(update) => Some(update),
        _ => None,
    }
}

/// `UpdatesManager::are_empty_updates` (cpp:1412) — true for the two
/// carry-no-updates umbrellas, the container emptiness otherwise.
pub fn are_empty_updates(updates: &telegram_api::Updates) -> bool {
    match updates {
        telegram_api::Updates::updatesTooLong(_)
        | telegram_api::Updates::updateShortSentMessage(_) => true,
        telegram_api::Updates::updateShortMessage(_)
        | telegram_api::Updates::updateShortChatMessage(_)
        | telegram_api::Updates::updateShort(_) => false,
        telegram_api::Updates::updatesCombined(updates) => updates.updates.is_empty(),
        telegram_api::Updates::updates(updates) => updates.updates.is_empty(),
    }
}

/// `UpdatesManager::get_sent_messages_random_ids` (cpp:1434) — the
/// random_ids of the updateMessageID entries whose message the container
/// itself carries (the sent-message echo guard).
pub fn get_sent_messages_random_ids(updates: &telegram_api::Updates) -> HashSet<i64> {
    let mut random_ids = HashSet::new();
    let Some(updates_vec) = get_updates_vec(updates) else {
        return random_ids;
    };
    let new_messages = get_new_messages(updates);
    for update in updates_vec {
        let telegram_api::Update::updateMessageID(update_message_id) = update else {
            continue;
        };
        let random_id = update_message_id.random_id;
        if random_id == 0 {
            continue;
        }
        let mut found_message = false;
        for &(message, is_scheduled) in &new_messages {
            let message_id = message_id_from_message(message, is_scheduled);
            if is_scheduled {
                found_message |= is_scheduled_server_message_id(message_id)
                    && scheduled_server_message_id_of(message_id) == update_message_id.id;
            } else {
                found_message |= is_valid_message_id(message_id)
                    && server_message_id_of(message_id) == update_message_id.id;
            }
        }
        if found_message && !random_ids.insert(random_id) {
            tdutils::log_error!("Receive twice updateMessageID for {random_id}");
        }
    }
    random_ids
}

/// `UpdatesManager::get_message_by_random_id` (cpp:1464) — the sent
/// message behind an updateMessageID, when the container carries exactly
/// one candidate; `dialog_id` is the sending dialog (the invalid dialog
/// matches only empty-carried messages, the C++ default-DialogId path).
pub fn get_message_by_random_id(
    updates: &telegram_api::Updates,
    dialog_id: DialogId,
    random_id: i64,
) -> Option<&telegram_api::Message> {
    let updates_vec = get_updates_vec(updates)?;

    let mut message_id = 0;
    for update in updates_vec {
        let telegram_api::Update::updateMessageID(update_message_id) = update else {
            continue;
        };
        if update_message_id.random_id == random_id {
            if message_id != 0 || update_message_id.id == 0 {
                return None;
            }
            message_id = update_message_id.id;
        }
    }
    if message_id == 0 {
        return None;
    }

    let target = (dialog_id, server_message_id_to_message_id(message_id));
    let mut result: Option<&telegram_api::Message> = None;
    for update in updates_vec {
        let message = match update {
            telegram_api::Update::updateNewMessage(update) => &update.message,
            telegram_api::Update::updateNewChannelMessage(update) => &update.message,
            _ => continue,
        };
        if message_full_id(message, false) == target {
            if result.is_some() {
                return None;
            }
            result = message.as_ref();
        }
    }
    result
}

/// `UpdatesManager::is_additional_service_message` (cpp:1505) — the
/// messageService `SetMessagesTTL` marker that filters new-message
/// extraction.
pub fn is_additional_service_message(message: &telegram_api::Message) -> bool {
    match message {
        telegram_api::Message::messageService(message) => matches!(
            message.action,
            Some(telegram_api::MessageAction::messageActionSetMessagesTTL(_))
        ),
        _ => false,
    }
}

/// `UpdatesManager::get_new_messages` (cpp:1513) — the
/// `[Message, is_scheduled]` pairs of the container, service-TTL markers
/// excluded.
pub fn get_new_messages(updates: &telegram_api::Updates) -> Vec<(&telegram_api::Message, bool)> {
    let mut messages = Vec::new();
    let Some(updates_vec) = get_updates_vec(updates) else {
        return messages;
    };
    for update in updates_vec {
        let (message, is_scheduled) = match update {
            telegram_api::Update::updateNewMessage(update) => (update.message.as_ref(), false),
            telegram_api::Update::updateNewChannelMessage(update) => {
                (update.message.as_ref(), false)
            }
            telegram_api::Update::updateNewScheduledMessage(update) => {
                (update.message.as_ref(), true)
            }
            _ => continue,
        };
        let Some(message) = message else {
            continue;
        };
        if !is_additional_service_message(message) {
            messages.push((message, is_scheduled));
        }
    }
    messages
}

/// `UpdatesManager::get_new_ephemeral_messages` (cpp:1541).
pub fn get_new_ephemeral_messages(
    updates: &telegram_api::Updates,
) -> Vec<&telegram_api::EphemeralMessage> {
    let mut messages = Vec::new();
    let Some(updates_vec) = get_updates_vec(updates) else {
        return messages;
    };
    for update in updates_vec {
        if let telegram_api::Update::updateNewEphemeralMessage(update) = update
            && let Some(message) = update.message.as_ref()
        {
            messages.push(message);
        }
    }
    messages
}

/// `UpdatesManager::get_update_new_group_call_id` (cpp:1554) — the
/// `(id, access_hash)` of the single groupCall the container carries;
/// `None` is the C++ invalid InputGroupCallId (absent, multiple, or
/// divergent).
pub fn get_update_new_group_call_id(updates: &telegram_api::Updates) -> Option<(i64, i64)> {
    let mut result: Option<(i64, i64)> = None;
    if let Some(updates_vec) = get_updates_vec(updates) {
        for update in updates_vec {
            let mut input_group_call_id: Option<(i64, i64)> = None;
            if let telegram_api::Update::updateGroupCall(update) = update
                && let Some(telegram_api::GroupCall::groupCall(group_call)) = &update.call
            {
                input_group_call_id = Some((group_call.id, group_call.access_hash));
            }
            if let Some(input_group_call_id) = input_group_call_id {
                if result.is_none() {
                    result = Some(input_group_call_id);
                } else if result != Some(input_group_call_id) {
                    result = None;
                    break;
                }
            }
        }
    }
    if result.is_none() {
        tdutils::log_error!(
            "Receive wrong response {}",
            update_umbrella_string_name(updates)
        );
    }
    result
}

/// The umbrella name for the statics' error logs (the free helpers have no
/// `update_umbrella_name` access for non-umbrella reasons — this is the
/// same table).
fn update_umbrella_string_name(updates: &telegram_api::Updates) -> &'static str {
    update_umbrella_name(updates)
}

/// `UpdatesManager::process_updates_users_and_chats` (cpp:1600) — the
/// users/chats pre-application; the vectors are drained into the sink and
/// cleared in place, the C++ `std::move` + `clear()` pair.
pub fn process_updates_users_and_chats(
    sink: &mut impl UpdatesSink,
    updates: &mut telegram_api::Updates,
) {
    let (users, chats) = match updates {
        telegram_api::Updates::updatesCombined(updates) => (
            std::mem::take(&mut updates.users),
            std::mem::take(&mut updates.chats),
        ),
        telegram_api::Updates::updates(updates) => (
            std::mem::take(&mut updates.users),
            std::mem::take(&mut updates.chats),
        ),
        _ => return,
    };
    for user in users {
        sink.on_user(user);
    }
    for chat in chats {
        sink.on_chat(chat);
    }
}

/// `UpdatesManager::extract_join_group_call_presentation_params`
/// (cpp:1622) — removes and returns the presentation
/// updateGroupCallConnection params. The C++ iterates a container
/// unconditionally (undefined on the leaf umbrellas); here the leaf
/// umbrellas yield the empty string.
pub fn extract_join_group_call_presentation_params(updates: &mut telegram_api::Updates) -> String {
    let Some(updates_vec) = get_updates_vec_mut(updates) else {
        return String::new();
    };
    let position = updates_vec.iter().position(|update| {
        matches!(
            update,
            telegram_api::Update::updateGroupCallConnection(update) if update.presentation
        )
    });
    let Some(position) = position else {
        return String::new();
    };
    let telegram_api::Update::updateGroupCallConnection(update) = updates_vec.remove(position)
    else {
        unreachable!("the position was matched above")
    };
    update.params.unwrap_or_default().data
}

/// `UpdatesManager::extract_star_gift_craft_fail` (cpp:1640) — removes the
/// first updateStarGiftCraftFail and reports whether one was present.
pub fn extract_star_gift_craft_fail(updates: &mut telegram_api::Updates) -> bool {
    let Some(updates_vec) = get_updates_vec_mut(updates) else {
        return false;
    };
    let position = updates_vec
        .iter()
        .position(|update| matches!(update, telegram_api::Update::updateStarGiftCraftFail(_)));
    let Some(position) = position else {
        return false;
    };
    updates_vec.remove(position);
    true
}

/// `UpdatesManager::extract_group_call_messages` (cpp:1653) — removes and
/// returns every updateGroupCallMessage. The C++ moves the objects out and
/// leaves moved-from nulls in the container (any later iteration would
/// CHECK-crash); draining the entries is the same observable contract for
/// the callers, which never re-iterate the container.
pub fn extract_group_call_messages(
    updates: &mut telegram_api::Updates,
) -> Vec<telegram_api::updateGroupCallMessage> {
    let Some(updates_vec) = get_updates_vec_mut(updates) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    let mut position = 0;
    while position < updates_vec.len() {
        if matches!(
            updates_vec[position],
            telegram_api::Update::updateGroupCallMessage(_)
        ) {
            let telegram_api::Update::updateGroupCallMessage(update) = updates_vec.remove(position)
            else {
                unreachable!("the position was matched above")
            };
            result.push(update);
        } else {
            position += 1;
        }
    }
    result
}

/// `UpdatesManager::extract_story` (cpp:1668) — removes and returns the
/// owner's story; the business path accepts the skipped-story variant and
/// only runs on 1-2-update containers. The C++ moves the story out and
/// leaves the updateStory in place with a null story (never re-iterated by
/// the callers); draining the entry is the same observable contract.
pub fn extract_story(
    updates: &mut telegram_api::Updates,
    owner_dialog_id: DialogId,
    is_business: bool,
) -> Option<telegram_api::StoryItem> {
    let updates_vec = get_updates_vec_mut(updates)?;
    if is_business && updates_vec.len() != 1 && updates_vec.len() != 2 {
        return None;
    }
    let position = updates_vec.iter().position(|update| {
        matches!(update, telegram_api::Update::updateStory(update)
            if DialogId::from_peer(update.peer.as_ref().unwrap_or(&telegram_api::Peer::default()))
                == owner_dialog_id
                && (is_business
                    || matches!(update.story.as_deref(), Some(telegram_api::StoryItem::storyItem(_)))))
    });
    let position = position?;
    let telegram_api::Update::updateStory(update) = updates_vec.remove(position) else {
        unreachable!("the position was matched above")
    };
    update.story.map(|boxed| *boxed)
}

/// `UpdatesManager::get_update_notify_settings_dialog_ids` (cpp:1697) —
/// the peer-dialog of every updateNotifySettings; scope/community wrappers
/// and invalid peers log and skip, exactly as the C++.
pub fn get_update_notify_settings_dialog_ids(updates: &telegram_api::Updates) -> Vec<DialogId> {
    let mut dialog_ids = Vec::new();
    let Some(updates_vec) = get_updates_vec(updates) else {
        return dialog_ids;
    };
    dialog_ids.reserve(updates_vec.len());
    for update in updates_vec {
        let mut dialog_id = DialogId::new(0);
        if let telegram_api::Update::updateNotifySettings(update) = update
            && let Some(telegram_api::NotifyPeer::notifyPeer(notify_peer)) = &update.peer
        {
            dialog_id = DialogId::from_peer(
                notify_peer
                    .peer
                    .as_ref()
                    .unwrap_or(&telegram_api::Peer::default()),
            );
        }
        if dialog_id.is_valid() {
            dialog_ids.push(dialog_id);
        } else {
            tdutils::log_error!("Receive unexpected {}", update_id_and_name(update).1);
        }
    }
    dialog_ids
}

/// `UpdatesManager::get_update_ids` (cpp:1727) — the constructor ids the
/// umbrella carries; the leaf umbrellas collapse to themselves.
pub fn get_update_ids(updates: &telegram_api::Updates) -> Vec<i32> {
    let updates_vec = match updates {
        telegram_api::Updates::updatesTooLong(_) => return vec![telegram_api::updatesTooLong::ID],
        telegram_api::Updates::updateShortMessage(_) => {
            return vec![telegram_api::updateShortMessage::ID];
        }
        telegram_api::Updates::updateShortChatMessage(_) => {
            return vec![telegram_api::updateShortChatMessage::ID];
        }
        telegram_api::Updates::updateShortSentMessage(_) => {
            return vec![telegram_api::updateShortSentMessage::ID];
        }
        telegram_api::Updates::updateShort(update) => {
            return vec![
                update_id_and_name(
                    update
                        .update
                        .as_ref()
                        .unwrap_or(&telegram_api::Update::default()),
                )
                .0,
            ];
        }
        telegram_api::Updates::updatesCombined(updates) => &updates.updates,
        telegram_api::Updates::updates(updates) => &updates.updates,
    };
    updates_vec
        .iter()
        .map(|update| update_id_and_name(update).0)
        .collect()
}

/// `ChatManager::get_unsupported_channel_id` (ChatManager.cpp:3161) — the
/// test/main dummy channel the dialog-id lists filter out.
fn get_unsupported_channel_id(is_test_dc: bool) -> ChannelId {
    ChannelId::new(if is_test_dc {
        10_304_875
    } else {
        1_535_424_647
    })
}

/// `ChatManager::get_dialog_id(chat)` (ChatManager.cpp:4435) — the channel
/// dialog when the chat carries one, the basic-group dialog otherwise.
fn chat_dialog_id(chat: &telegram_api::Chat) -> DialogId {
    let channel_id = match chat {
        telegram_api::Chat::channel(channel) => Some(ChannelId::new(channel.id)),
        telegram_api::Chat::channelForbidden(channel) => Some(ChannelId::new(channel.id)),
        _ => None,
    };
    if let Some(channel_id) = channel_id
        && channel_id.is_valid()
    {
        return DialogId::from_channel_id(channel_id);
    }
    let chat_id = match chat {
        telegram_api::Chat::chatEmpty(chat) => ChatId::new(chat.id),
        telegram_api::Chat::chat(chat) => ChatId::new(chat.id),
        telegram_api::Chat::chatForbidden(chat) => ChatId::new(chat.id),
        _ => ChatId::default(),
    };
    DialogId::from_chat_id(chat_id)
}

/// `UpdatesManager::get_chat_dialog_ids` (cpp:1764) — the chat dialogs the
/// container carries; the unsupported test/main dummy channel drops out
/// once the list is plural. The leaf umbrellas log and yield empty, as the
/// C++.
pub fn get_chat_dialog_ids(updates: &telegram_api::Updates, is_test_dc: bool) -> Vec<DialogId> {
    let chats: &[telegram_api::Chat] = match updates {
        telegram_api::Updates::updatesTooLong(_)
        | telegram_api::Updates::updateShortMessage(_)
        | telegram_api::Updates::updateShortChatMessage(_)
        | telegram_api::Updates::updateShort(_)
        | telegram_api::Updates::updateShortSentMessage(_) => {
            tdutils::log_error!(
                "Receive {} instead of updates",
                update_umbrella_string_name(updates)
            );
            return Vec::new();
        }
        telegram_api::Updates::updatesCombined(updates) => &updates.chats,
        telegram_api::Updates::updates(updates) => &updates.chats,
    };

    let mut dialog_ids = Vec::with_capacity(chats.len());
    for chat in chats {
        let dialog_id = chat_dialog_id(chat);
        if dialog_id.is_valid() {
            dialog_ids.push(dialog_id);
        } else {
            tdutils::log_error!("Can't find identifier of {:?}", chat);
        }
    }
    if dialog_ids.len() > 1 {
        let unsupported = DialogId::from_channel_id(get_unsupported_channel_id(is_test_dc));
        dialog_ids.retain(|&dialog_id| dialog_id != unsupported);
    }
    dialog_ids
}

/// `UpdatesManager::get_community_id` (cpp:1807) — the raw id of the
/// single non-regular channel update the container carries; `None` is the
/// C++ invalid CommunityId (absent, multiple, or non-positive).
pub fn get_community_id(updates: &telegram_api::Updates) -> Option<i64> {
    let mut community_id: Option<i64> = None;
    if let Some(updates_vec) = get_updates_vec(updates) {
        for update in updates_vec {
            let telegram_api::Update::updateChannel(update) = update else {
                continue;
            };
            let channel_id = ChannelId::new(update.channel_id);
            if channel_id.is_regular_channel() {
                continue;
            }
            if community_id.is_some() {
                tdutils::log_error!("Receive multiple updateChannel");
                return None;
            }
            if update.channel_id <= 0 {
                tdutils::log_error!("Receive {}", update.channel_id);
                return None;
            }
            community_id = Some(update.channel_id);
        }
    }
    if community_id.is_none() {
        tdutils::log_error!("Receive no community");
    }
    community_id
}

/// `UpdatesManager::get_update_edit_message_pts` (cpp:1832) — the pts of
/// the container's edit-update for `message_full_id`; -1 collapses to 0
/// with the C++ plural error, absence is the C++ missing error.
pub fn get_update_edit_message_pts(
    updates: &telegram_api::Updates,
    full_id: (DialogId, i64),
) -> i32 {
    let mut pts: i32 = 0;
    let Some(updates_vec) = get_updates_vec(updates) else {
        return 0;
    };
    for update in updates_vec {
        let update_pts: i32 = match update {
            telegram_api::Update::updateEditMessage(update)
                if message_full_id(&update.message, false) == full_id =>
            {
                update.pts
            }
            telegram_api::Update::updateEditChannelMessage(update)
                if message_full_id(&update.message, false) == full_id =>
            {
                update.pts
            }
            telegram_api::Update::updateNewScheduledMessage(update) => {
                let new_message_full_id = message_full_id(&update.message, true);
                if new_message_full_id.0 == full_id.0
                    && is_scheduled_server_message_id(new_message_full_id.1)
                    && is_scheduled_server_message_id(full_id.1)
                    && scheduled_server_message_id_of(new_message_full_id.1)
                        == scheduled_server_message_id_of(full_id.1)
                {
                    -2
                } else {
                    0
                }
            }
            _ => 0,
        };
        if update_pts != 0 {
            pts = if pts == 0 { update_pts } else { -1 };
        }
    }
    if pts == -1 {
        tdutils::log_error!(
            "Receive multiple edit message updates in {}",
            update_umbrella_string_name(updates)
        );
        pts = 0;
    } else if pts == 0 {
        tdutils::log_error!(
            "Receive no edit message updates for {} in {}",
            full_id.0.get(),
            update_umbrella_string_name(updates)
        );
    }
    pts
}

/// Complete an optional promise (the C++ `promise.set_value(Unit())` on a
/// possibly-moved-from promise is a no-op).
fn complete(mut promise: Option<Promise<()>>) {
    if let Some(promise) = promise.as_mut() {
        promise.set_value(());
    }
}

/// The `get_promise()` of the process_updates/on_pending_updates fan-in:
/// the only promise goes to the first dispatched update, the rest get the
/// empty promise.
fn take_dispatch_promise(single: &mut Option<Promise<()>>) -> Promise<()> {
    single.take().unwrap_or_default()
}

fn update_umbrella_name(updates: &telegram_api::Updates) -> &'static str {
    match updates {
        telegram_api::Updates::updatesTooLong(_) => "updatesTooLong",
        telegram_api::Updates::updateShortMessage(_) => "updateShortMessage",
        telegram_api::Updates::updateShortChatMessage(_) => "updateShortChatMessage",
        telegram_api::Updates::updateShort(_) => "updateShort",
        telegram_api::Updates::updatesCombined(_) => "updatesCombined",
        telegram_api::Updates::updates(_) => "updates",
        telegram_api::Updates::updateShortSentMessage(_) => "updateShortSentMessage",
    }
}

fn difference_name(difference: &telegram_api::updates_Difference) -> &'static str {
    match difference {
        telegram_api::updates_Difference::updates_difference(_) => "updates.difference",
        telegram_api::updates_Difference::updates_differenceEmpty(_) => "updates.differenceEmpty",
        telegram_api::updates_Difference::updates_differenceSlice(_) => "updates.differenceSlice",
        telegram_api::updates_Difference::updates_differenceTooLong(_) => {
            "updates.differenceTooLong"
        }
    }
}

/// `UpdatesManager::have_update_pts_changed` (cpp:4153).
fn have_update_pts_changed(updates: &[telegram_api::Update]) -> bool {
    updates
        .iter()
        .any(|update| update_id_and_name(update).0 == telegram_api::updatePtsChanged::ID)
}

/// The 11 immediately-applied updates of `on_pending_updates` (cpp:2561).
fn is_immediately_applied_update(id: i32) -> bool {
    matches!(
        id,
        telegram_api::updateUserTyping::ID
            | telegram_api::updateChatUserTyping::ID
            | telegram_api::updateChannelUserTyping::ID
            | telegram_api::updateEncryptedChatTyping::ID
            | telegram_api::updateLoginToken::ID
            | telegram_api::updateSentPhoneCode::ID
            | telegram_api::updateDcOptions::ID
            | telegram_api::updateConfig::ID
            | telegram_api::updateServiceNotification::ID
            | telegram_api::updateLangPackTooLong::ID
            | telegram_api::updateLangPack::ID
    )
}

/// `DialogId::get_message_dialog_id` — the peer of any message carrier
/// (`messageEmpty` carries none).
fn message_peer_id(message: &Option<telegram_api::Message>) -> Option<&telegram_api::Peer> {
    match message.as_ref()? {
        telegram_api::Message::message(m) => m.peer_id.as_ref(),
        telegram_api::Message::messageService(m) => m.peer_id.as_ref(),
        telegram_api::Message::messageEmpty(_) => None,
    }
}

/// The C++ nullptr-peer of the acceptability filter: the wire default
/// encodes an absent peer as the zero id (the flag-gated object fields
/// parse to their default), and a zero id is unacceptable anyway.
fn peer_id_is_zero(peer: &telegram_api::Peer) -> bool {
    match peer {
        telegram_api::Peer::peerUser(peer) => peer.user_id == 0,
        telegram_api::Peer::peerChat(peer) => peer.chat_id == 0,
        telegram_api::Peer::peerChannel(peer) => peer.channel_id == 0,
    }
}

/// `UpdatesManager::check_pts_update` (cpp:4176) — the pts-bearing updates
/// must be for known dialog types and carry sane pts/pts_count.
fn check_pts_update(update: &PendingUpdate) -> bool {
    let (pts, pts_count) = match update {
        PendingUpdate::Wire(telegram_api::Update::updateNewMessage(u)) => {
            if !message_peer_id(&u.message).is_some_and(check_pts_update_peer) {
                return false;
            }
            (u.pts, u.pts_count)
        }
        PendingUpdate::Wire(telegram_api::Update::updateReadHistoryInbox(u)) => {
            if !check_pts_update_peer(u.peer.as_ref().unwrap_or(&telegram_api::Peer::default())) {
                return false;
            }
            (u.pts, u.pts_count)
        }
        PendingUpdate::Wire(telegram_api::Update::updateReadHistoryOutbox(u)) => {
            if !check_pts_update_peer(u.peer.as_ref().unwrap_or(&telegram_api::Peer::default())) {
                return false;
            }
            (u.pts, u.pts_count)
        }
        PendingUpdate::Wire(telegram_api::Update::updateEditMessage(u)) => {
            if !message_peer_id(&u.message).is_some_and(check_pts_update_peer) {
                return false;
            }
            (u.pts, u.pts_count)
        }
        PendingUpdate::Wire(telegram_api::Update::updatePinnedMessages(u)) => {
            if !check_pts_update_peer(u.peer.as_ref().unwrap_or(&telegram_api::Peer::default())) {
                return false;
            }
            (u.pts, u.pts_count)
        }
        PendingUpdate::Wire(telegram_api::Update::updateReadMessagesContents(u)) => {
            (u.pts, u.pts_count)
        }
        PendingUpdate::Wire(telegram_api::Update::updateDeleteMessages(u)) => (u.pts, u.pts_count),
        PendingUpdate::Dummy | PendingUpdate::SentMessage { .. } => return true,
        _ => return false,
    };
    pts_count >= 0 && pts > pts_count
}

/// `UpdatesManager::check_pts_update_dialog_id` (cpp:4166) — users and
/// basic chats are checkable; channels and secret chats are not.
fn check_pts_update_peer(peer: &telegram_api::Peer) -> bool {
    matches!(
        peer,
        telegram_api::Peer::peerUser(_) | telegram_api::Peer::peerChat(_)
    )
}

/// `UpdatesManager::is_pts_update` (cpp:4212).
fn is_pts_update(update: &PendingUpdate) -> bool {
    matches!(
        update,
        PendingUpdate::Wire(telegram_api::Update::updateNewMessage(_))
            | PendingUpdate::Wire(telegram_api::Update::updateReadMessagesContents(_))
            | PendingUpdate::Wire(telegram_api::Update::updateEditMessage(_))
            | PendingUpdate::Wire(telegram_api::Update::updateDeleteMessages(_))
            | PendingUpdate::Wire(telegram_api::Update::updateReadHistoryInbox(_))
            | PendingUpdate::Wire(telegram_api::Update::updateReadHistoryOutbox(_))
            | PendingUpdate::Wire(telegram_api::Update::updateWebPage(_))
            | PendingUpdate::Wire(telegram_api::Update::updatePinnedMessages(_))
            | PendingUpdate::Wire(telegram_api::Update::updateFolderPeers(_))
    )
}

/// `UpdatesManager::get_update_pts` (cpp:4231).
fn get_update_pts(update: &PendingUpdate) -> i32 {
    match update {
        PendingUpdate::Wire(telegram_api::Update::updateNewMessage(u)) => u.pts,
        PendingUpdate::Wire(telegram_api::Update::updateReadMessagesContents(u)) => u.pts,
        PendingUpdate::Wire(telegram_api::Update::updateEditMessage(u)) => u.pts,
        PendingUpdate::Wire(telegram_api::Update::updateDeleteMessages(u)) => u.pts,
        PendingUpdate::Wire(telegram_api::Update::updateReadHistoryInbox(u)) => u.pts,
        PendingUpdate::Wire(telegram_api::Update::updateReadHistoryOutbox(u)) => u.pts,
        PendingUpdate::Wire(telegram_api::Update::updateWebPage(u)) => u.pts,
        PendingUpdate::Wire(telegram_api::Update::updatePinnedMessages(u)) => u.pts,
        PendingUpdate::Wire(telegram_api::Update::updateFolderPeers(u)) => u.pts,
        _ => 0,
    }
}

/// `UpdatesManager::is_qts_update` (cpp:4254) — updateNewEncryptedMessage
/// plus the bot-qts table.
fn is_qts_update(update: &PendingUpdate) -> bool {
    matches!(
        update,
        PendingUpdate::Wire(telegram_api::Update::updateNewEncryptedMessage(_))
            | PendingUpdate::Wire(telegram_api::Update::updateMessagePollVote(_))
            | PendingUpdate::Wire(telegram_api::Update::updateManagedBot(_))
            | PendingUpdate::Wire(telegram_api::Update::updateBotStopped(_))
            | PendingUpdate::Wire(telegram_api::Update::updateChatParticipant(_))
            | PendingUpdate::Wire(telegram_api::Update::updateChannelParticipant(_))
            | PendingUpdate::Wire(telegram_api::Update::updateBotChatInviteRequester(_))
            | PendingUpdate::Wire(telegram_api::Update::updateBotChatBoost(_))
            | PendingUpdate::Wire(telegram_api::Update::updateBotMessageReaction(_))
            | PendingUpdate::Wire(telegram_api::Update::updateBotMessageReactions(_))
            | PendingUpdate::Wire(telegram_api::Update::updateBotBusinessConnect(_))
            | PendingUpdate::Wire(telegram_api::Update::updateBotNewBusinessMessage(_))
            | PendingUpdate::Wire(telegram_api::Update::updateBotEditBusinessMessage(_))
            | PendingUpdate::Wire(telegram_api::Update::updateBotDeleteBusinessMessage(_))
            | PendingUpdate::Wire(telegram_api::Update::updateBotPurchasedPaidMedia(_))
            | PendingUpdate::Wire(telegram_api::Update::updateBotGuestChatQuery(_))
            | PendingUpdate::Wire(telegram_api::Update::updateBotStarsSubscription(_))
    )
}

/// `UpdatesManager::get_update_qts` (cpp:4280).
fn get_update_qts(update: &PendingUpdate) -> i32 {
    match update {
        PendingUpdate::Wire(telegram_api::Update::updateNewEncryptedMessage(u)) => u.qts,
        PendingUpdate::Wire(telegram_api::Update::updateMessagePollVote(u)) => u.qts,
        PendingUpdate::Wire(telegram_api::Update::updateManagedBot(u)) => u.qts,
        PendingUpdate::Wire(telegram_api::Update::updateBotStopped(u)) => u.qts,
        PendingUpdate::Wire(telegram_api::Update::updateChatParticipant(u)) => u.qts,
        PendingUpdate::Wire(telegram_api::Update::updateChannelParticipant(u)) => u.qts,
        PendingUpdate::Wire(telegram_api::Update::updateBotChatInviteRequester(u)) => u.qts,
        PendingUpdate::Wire(telegram_api::Update::updateBotChatBoost(u)) => u.qts,
        PendingUpdate::Wire(telegram_api::Update::updateBotMessageReaction(u)) => u.qts,
        PendingUpdate::Wire(telegram_api::Update::updateBotMessageReactions(u)) => u.qts,
        PendingUpdate::Wire(telegram_api::Update::updateBotBusinessConnect(u)) => u.qts,
        PendingUpdate::Wire(telegram_api::Update::updateBotNewBusinessMessage(u)) => u.qts,
        PendingUpdate::Wire(telegram_api::Update::updateBotEditBusinessMessage(u)) => u.qts,
        PendingUpdate::Wire(telegram_api::Update::updateBotDeleteBusinessMessage(u)) => u.qts,
        PendingUpdate::Wire(telegram_api::Update::updateBotPurchasedPaidMedia(u)) => u.qts,
        PendingUpdate::Wire(telegram_api::Update::updateBotGuestChatQuery(u)) => u.qts,
        PendingUpdate::Wire(telegram_api::Update::updateBotStarsSubscription(u)) => u.qts,
        _ => 0,
    }
}

/// `UpdatesManager::is_channel_pts_update` (cpp:4320).
fn is_channel_pts_update(update: &PendingUpdate) -> bool {
    matches!(
        update,
        PendingUpdate::Wire(telegram_api::Update::updateNewChannelMessage(_))
            | PendingUpdate::Wire(telegram_api::Update::updateEditChannelMessage(_))
            | PendingUpdate::Wire(telegram_api::Update::updateDeleteChannelMessages(_))
            | PendingUpdate::Wire(telegram_api::Update::updatePinnedChannelMessages(_))
    )
}

/// `td::is_valid_user_id` — a positive user id.
fn is_valid_user_id(user_id: i64) -> bool {
    user_id > 0
}

/// The strict canonical-form int32 read of a self-written pmc value (the
/// module docs document the divergence from the lenient `td::to_integer`).
fn parse_pmc_i32(value: &str, key: &str) -> i32 {
    match value.parse::<i32>() {
        Ok(parsed) => parsed,
        Err(_) => {
            tdutils::log_error!("Failed to parse binlog pmc {} = {:?}", key, value);
            0
        }
    }
}

#[cfg(test)]
mod extract_sent_wallet_transaction_tests {
    use super::*;
    use tdmtproto::telegram_api;

    fn sent_update(gasless: bool) -> telegram_api::Update {
        telegram_api::Update::updateSentWalletTransaction(
            telegram_api::updateSentWalletTransaction {
                flags: 0,
                gasless,
                msg_hash: "ab".to_owned(),
                transaction: Some(telegram_api::WalletTransaction {
                    flags: 0,
                    incoming: true,
                    gasless,
                    failed: false,
                    key_change: false,
                    comment_encrypted: false,
                    id: "tx".to_owned(),
                    amount: 1,
                    fee: 0,
                    date: 2,
                    peer: None,
                    comment: String::new(),
                    tx_hash: String::new(),
                    nft: None,
                }),
            },
        )
    }

    #[test]
    fn extracts_and_erases_the_first_sent_transaction() {
        // 63c084905 — the SendWalletTransferQuery tail form: the update is
        // lifted out of the container before on_get_updates sees it.
        let mut updates = telegram_api::Updates::updates(telegram_api::updates {
            updates: vec![
                sent_update(false),
                telegram_api::Update::updateSentWalletTransaction(
                    telegram_api::updateSentWalletTransaction {
                        flags: 0,
                        gasless: true,
                        msg_hash: "cd".to_owned(),
                        transaction: None,
                    },
                ),
            ],
            users: Vec::new(),
            chats: Vec::new(),
            date: 0,
            seq: 0,
        });
        let extracted = extract_sent_wallet_transaction(&mut updates).unwrap();
        assert!(!extracted.gasless);
        assert_eq!(extracted.msg_hash, "ab");
        assert!(extracted.transaction.is_some());
        // The second extraction lifts the remaining update; then None.
        assert_eq!(
            extract_sent_wallet_transaction(&mut updates)
                .unwrap()
                .msg_hash,
            "cd"
        );
        assert!(extract_sent_wallet_transaction(&mut updates).is_none());
        match &updates {
            telegram_api::Updates::updates(u) => assert!(u.updates.is_empty()),
            _ => panic!("unexpected container"),
        }
        // The short forms answer None.
        assert!(
            extract_sent_wallet_transaction(&mut telegram_api::Updates::updatesTooLong(
                telegram_api::updatesTooLong
            ))
            .is_none()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingSink {
        users: Vec<i64>,
        chats: Vec<i64>,
        pts_applied: Vec<(i32, &'static str)>, // (message id marker, kind name)
        channel_updates: Vec<(i64, i32, i32)>, // (peer id, pts, pts_count)
        message_ids: Vec<(i64, i32, bool)>,
        local_was_online: Vec<(i64, i32)>,
        encrypted_messages: Vec<i64>,
        login_tokens: usize,
        sent_codes: usize,
        unsupported: Vec<(i32, String)>,
        before_difference: usize,
        after_difference: usize,
        config_updates: usize,
        skipped: Vec<(i32, i32, i32)>, // (new_pts, old_pts, pts_count)
        difference_messages: Vec<i32>,
        invalid_poll: bool,
        speed_limit_notifications: Vec<bool>,
        reload_data_calls: usize,
        often_reload_calls: usize,
        deny_users: std::collections::BTreeSet<i64>,
        channel_too_long: Vec<(i64, i32, bool)>,
    }

    impl UpdatesSink for RecordingSink {
        fn on_user(&mut self, user: telegram_api::User) {
            self.users.push(match user {
                telegram_api::User::user(u) => u.id,
                _ => 0,
            });
        }
        fn on_chat(&mut self, chat: telegram_api::Chat) {
            self.chats.push(match chat {
                telegram_api::Chat::chat(c) => c.id,
                _ => 0,
            });
        }
        fn process_pts_update(&mut self, update: PendingUpdate) {
            // the MessagesManager::process_pts_update stand-in: record the
            // applied kind
            let marker = match &update {
                PendingUpdate::Wire(telegram_api::Update::updateNewMessage(u)) => {
                    match u.message.as_ref() {
                        Some(telegram_api::Message::message(m)) => m.id,
                        Some(telegram_api::Message::messageService(m)) => m.id,
                        Some(telegram_api::Message::messageEmpty(_)) | None => 0,
                    }
                }
                PendingUpdate::Wire(telegram_api::Update::updateDeleteMessages(u)) => {
                    u.messages.first().copied().unwrap_or(-1)
                }
                _ => 0,
            };
            self.pts_applied.push((marker, update.name()));
        }
        fn skip_old_pending_pts_update(
            &mut self,
            _update: PendingUpdate,
            new_pts: i32,
            old_pts: i32,
            pts_count: i32,
            _source: &str,
        ) {
            self.skipped.push((new_pts, old_pts, pts_count));
        }
        fn add_pending_channel_update(
            &mut self,
            peer: Option<telegram_api::Peer>,
            _update: PendingUpdate,
            pts: i32,
            pts_count: i32,
            _promise: Promise<()>,
            _source: &'static str,
        ) {
            let peer_id = match peer {
                Some(telegram_api::Peer::peerChannel(channel)) => channel.channel_id,
                Some(telegram_api::Peer::peerUser(user)) => user.user_id,
                Some(telegram_api::Peer::peerChat(chat)) => chat.chat_id,
                None => 0,
            };
            self.channel_updates.push((peer_id, pts, pts_count));
        }
        fn on_update_message_id(
            &mut self,
            random_id: i64,
            id: i32,
            is_scheduled: bool,
            _source: &str,
        ) -> bool {
            self.message_ids.push((random_id, id, is_scheduled));
            true
        }
        fn on_update_user_local_was_online(&mut self, user_id: i64, date: i32) {
            self.local_was_online.push((user_id, date));
        }
        fn on_new_encrypted_message(&mut self, message: telegram_api::EncryptedMessage) {
            let random_id = match message {
                telegram_api::EncryptedMessage::encryptedMessage(m) => m.random_id,
                _ => 0,
            };
            self.encrypted_messages.push(random_id);
        }
        fn on_update_login_token(&mut self) {
            self.login_tokens += 1;
        }
        fn on_update_sent_code(&mut self, _sent_code: telegram_api::auth_SentCode) {
            self.sent_codes += 1;
        }
        fn on_update_config(&mut self) {
            self.config_updates += 1;
        }
        fn on_before_get_difference(&mut self) {
            self.before_difference += 1;
        }
        fn on_after_get_difference(&mut self) {
            self.after_difference += 1;
        }
        fn on_get_difference_message(&mut self, message: telegram_api::Message) {
            let id = match &message {
                telegram_api::Message::message(m) => m.id,
                _ => 0,
            };
            self.difference_messages.push(id);
        }
        fn is_invalid_poll_message(&mut self, _message: &telegram_api::Message) -> bool {
            self.invalid_poll
        }
        fn on_unsupported_update(&mut self, id: i32, name: &str, _cpp_line: u32) {
            self.unsupported.push((id, name.to_string()));
        }
        // The harness simulates a populated cache: every queried
        // user/chat/channel is known and InputUser-constructible, except
        // the explicitly denied users.
        fn is_acceptable_user(&mut self, user_id: i64) -> bool {
            !self.deny_users.contains(&user_id)
        }
        fn is_acceptable_chat(&mut self, _chat_id: i64) -> bool {
            true
        }
        fn is_acceptable_channel(&mut self, _channel_id: i64) -> bool {
            true
        }
        fn can_get_input_user(&mut self, _user_id: i64) -> bool {
            true
        }
        fn on_update_channel_too_long(
            &mut self,
            update: telegram_api::updateChannelTooLong,
            force: bool,
        ) {
            self.channel_too_long
                .push((update.channel_id, update.pts, force));
        }
        fn on_speed_limit_notification(&mut self, is_upload: bool) {
            self.speed_limit_notifications.push(is_upload);
        }
        fn reload_data(&mut self) {
            self.reload_data_calls += 1;
        }
        fn reload_often_data(&mut self) {
            self.often_reload_calls += 1;
        }
    }

    #[derive(Default)]
    struct TestEnv {
        clock: f64,
        unix_time_value: i32,
        authorized: bool,
        bot: bool,
        close: bool,
        ignore_background: bool,
        can_ignore_background: bool,
        options: BTreeMap<String, i64>,
        pmc: BTreeMap<String, String>,
        sent_difference: Vec<(i32, i32, i32)>,
        sent_updates_state: usize,
        sent_confirm: Vec<(i32, i32)>,
        sent_pts_updates: Vec<i32>,
        sent_pings: usize,
        online: bool,
    }

    impl UpdatesManagerEnv for TestEnv {
        fn now(&mut self) -> f64 {
            self.clock
        }
        fn unix_time(&mut self) -> i32 {
            self.unix_time_value
        }
        fn close_flag(&mut self) -> bool {
            self.close
        }
        fn is_authorized(&mut self) -> bool {
            self.authorized
        }
        fn is_bot(&mut self) -> bool {
            self.bot
        }
        fn ignore_background_updates(&mut self) -> bool {
            self.ignore_background
        }
        fn can_ignore_background_updates(&mut self) -> bool {
            self.can_ignore_background
        }
        fn get_option_integer(&mut self, name: &str) -> i64 {
            self.options.get(name).copied().unwrap_or(0)
        }
        fn set_option_empty(&mut self, name: &str) {
            self.options.remove(name);
        }
        fn pmc_get(&mut self, key: &str) -> String {
            self.pmc.get(key).cloned().unwrap_or_default()
        }
        fn pmc_set(&mut self, key: &str, value: &str) {
            self.pmc.insert(key.to_string(), value.to_string());
        }
        fn pmc_erase(&mut self, key: &str) {
            self.pmc.remove(key);
        }
        fn get_my_id(&mut self) -> i64 {
            7
        }
        fn send_get_difference(&mut self, pts: i32, date: i32, qts: i32) {
            self.sent_difference.push((pts, date, qts));
        }
        fn send_get_updates_state(&mut self) {
            self.sent_updates_state += 1;
        }
        fn send_confirm_pts_qts(&mut self, pts: i32, qts: i32) {
            self.sent_confirm.push((pts, qts));
        }
        fn send_get_pts_update(&mut self, pts: i32) {
            self.sent_pts_updates.push(pts);
        }
        fn send_ping_server(&mut self) {
            self.sent_pings += 1;
        }
        fn is_online(&mut self) -> bool {
            self.online
        }
    }

    fn authorized_env() -> TestEnv {
        TestEnv {
            authorized: true,
            unix_time_value: 1_000_000,
            ..TestEnv::default()
        }
    }

    /// Seed a known pts baseline the way the C++ constructor restores it
    /// from the pmc — the fresh manager now carries the -1 unknown
    /// sentinel (PtsManager.h:53-54), which routes through getState.
    fn seed_pts<S: UpdatesSink>(manager: &mut UpdatesManager<S>, pts: i32) {
        manager.pts_manager.init(pts);
        manager.qts_manager.init(0);
    }

    fn user(id: i64) -> telegram_api::User {
        telegram_api::User::user(telegram_api::user {
            id,
            ..telegram_api::user::default()
        })
    }

    fn chat(id: i64) -> telegram_api::Chat {
        telegram_api::Chat::chat(telegram_api::chat {
            id,
            ..telegram_api::chat::default()
        })
    }

    fn new_message_update(id: i32, pts: i32, pts_count: i32) -> telegram_api::Update {
        telegram_api::Update::updateNewMessage(telegram_api::updateNewMessage {
            message: Some(telegram_api::Message::message(telegram_api::message {
                id,
                peer_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                    user_id: 100,
                })),
                ..telegram_api::message::default()
            })),
            pts,
            pts_count,
        })
    }

    fn updates_container(
        updates: Vec<telegram_api::Update>,
        seq: i32,
        date: i32,
    ) -> telegram_api::Updates {
        telegram_api::Updates::updates(telegram_api::updates {
            updates,
            users: vec![],
            chats: vec![],
            date,
            seq,
        })
    }

    #[test]
    fn wire_update_dump_matches_the_cpp_tl_text() {
        // The oracle is the C++ 1024-log line from the h2h-pip run,
        // byte-for-byte (UpdatesManager.cpp:2705's `to_string(update)`).
        let update = telegram_api::updateNewMessage {
            message: Some(telegram_api::Message::message(telegram_api::message {
                flags: 0,
                id: 361,
                peer_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                    user_id: 2202815082,
                })),
                date: 1790672705,
                message: "h2h message one".to_owned(),
                ..telegram_api::message::default()
            })),
            pts: 362,
            pts_count: 1,
        };
        let pending = PendingUpdate::Wire(telegram_api::Update::updateNewMessage(update));
        assert_eq!(
            pending.dump_string(),
            "updateNewMessage {\n  message = message {\n    flags = 0\n    flags2 = 0\n    id = 361\n    peer_id = peerUser {\n      user_id = 2202815082\n    }\n    date = 1790672705\n    message = \"h2h message one\"\n  }\n  pts = 362\n  pts_count = 1\n}\n"
        );
    }

    #[test]
    fn umbrella_dump_matches_the_cpp_receive_updates_text() {
        // The oracle is the C++ 1024-log body at UpdatesManager.cpp:1193
        // (`Receive ` + `to_string(updates_ptr)`): the container fields in
        // scheme order — updates, users, chats, date, seq — with the same
        // vector/ctor text the h2h log shows.
        let update = telegram_api::updateNewMessage {
            message: Some(telegram_api::Message::message(telegram_api::message {
                flags: 0,
                id: 361,
                peer_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                    user_id: 2202815082,
                })),
                date: 1790672705,
                message: "h2h message one".to_owned(),
                ..telegram_api::message::default()
            })),
            pts: 362,
            pts_count: 1,
        };
        let umbrella = telegram_api::Updates::updates(telegram_api::updates {
            updates: vec![telegram_api::Update::updateNewMessage(update)],
            users: vec![],
            chats: vec![],
            date: 1790672705,
            seq: 0,
        });
        assert_eq!(
            umbrella.to_debug_string(),
            "updates {\n  updates = vector[1] {\n    updateNewMessage {\n      message = message {\n        flags = 0\n        flags2 = 0\n        id = 361\n        peer_id = peerUser {\n          user_id = 2202815082\n        }\n        date = 1790672705\n        message = \"h2h message one\"\n      }\n      pts = 362\n      pts_count = 1\n    }\n  }\n  users = vector[0] {\n  }\n  chats = vector[0] {\n  }\n  date = 1790672705\n  seq = 0\n}\n"
        );
    }

    #[test]
    fn pts_manager_tracks_mem_and_db() {
        let mut manager = PtsManager::new_unknown();
        // The unknown sentinel (PtsManager.h:53-54) until `init`.
        assert_eq!(manager.mem_pts(), -1);
        assert_eq!(manager.db_pts(), -1);
        manager.init(10);
        assert_eq!(manager.mem_pts(), 10);
        assert_eq!(manager.db_pts(), 10);
        let id1 = manager.add_pts(12);
        let id2 = manager.add_pts(15);
        assert_eq!(manager.mem_pts(), 15);
        assert_eq!(manager.db_pts(), 10);
        assert_eq!(manager.finish(id1), 12);
        assert_eq!(manager.finish(id2), 15);
        // A zero pts (not a checkpoint) leaves db_pts alone.
        let id3 = manager.add_pts(0);
        manager.add_pts(20);
        assert_eq!(manager.finish(id3), 15);
    }

    // -- M.7.1 --------------------------------------------------------

    #[test]
    fn save_pts_throttles_for_bots_and_flushes_on_tick() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        env.bot = true;
        manager.pts_manager.init(10);
        // For bots every save within the window throttles — including the
        // first one (the C++ bot bypass only skips the throttle for
        // non-bots).
        manager.add_pts(&mut env, 11);
        assert!(!env.pmc.contains_key("updates.pts"), "throttled");
        assert_eq!(manager.next_deadline(), Some(MAX_PTS_SAVE_DELAY));
        env.clock += MAX_PTS_SAVE_DELAY + 0.01;
        manager.tick_timeouts(&mut env);
        assert_eq!(
            env.pmc.get("updates.pts").map(String::as_str),
            Some("11"),
            "flushed"
        );

        // A second save within the new window throttles again.
        env.clock += MAX_PTS_SAVE_DELAY / 2.0;
        manager.add_pts(&mut env, 12);
        assert_eq!(
            env.pmc.get("updates.pts").map(String::as_str),
            Some("11"),
            "throttled"
        );
        env.clock += MAX_PTS_SAVE_DELAY;
        manager.tick_timeouts(&mut env);
        assert_eq!(
            env.pmc.get("updates.pts").map(String::as_str),
            Some("12"),
            "flushed"
        );
    }

    #[test]
    fn save_pts_erases_at_i32_max() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        manager.save_pts(&mut env, i32::MAX);
        assert!(!env.pmc.contains_key("updates.pts"));
    }

    #[test]
    fn set_pts_moves_up_and_persists() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        manager.set_pts(&mut env, 15, "test");
        assert_eq!(manager.pts_manager.db_pts(), 15);
        assert_eq!(env.pmc.get("updates.pts").map(String::as_str), Some("15"));
    }

    #[test]
    fn set_pts_rejects_small_backwards() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(1_000_000);
        manager.set_pts(&mut env, 999_000, "backwards");
        assert_eq!(
            manager.get_pts(),
            1_000_000,
            "small backwards is logged only"
        );
        // 5 < 1_000_000 - 1_000_009 (= -9) is false — also rejected.
        manager.set_pts(&mut env, 5, "below threshold");
        assert_eq!(manager.get_pts(), 1_000_000);
    }

    #[test]
    fn set_pts_applies_cardinal_drop_and_rejects_medium_backwards() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(2_000_000);
        // 1_200_000 > 2_000_000 - 1_000_009 (= 999_991) — not cardinal →
        // rejected.
        manager.set_pts(&mut env, 1_200_000, "backwards");
        assert_eq!(manager.get_pts(), 2_000_000);

        // A cardinal drop applies (0 < pts < old - 1000009).
        manager.set_pts(&mut env, 5, "cardinal");
        assert_eq!(manager.get_pts(), 5);
    }

    #[test]
    fn set_pts_max_drops_state_and_refetches() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        manager.set_pts(&mut env, i32::MAX, "PERSISTENT_TIMESTAMP_INVALID");
        assert!(!env.pmc.contains_key("updates.pts"), "stored PTS dropped");
        assert_eq!(env.sent_updates_state, 1, "init_state requested the state");
        assert!(manager.running_get_difference());
    }

    #[test]
    fn set_date_advances_and_persists_only_forward() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.set_date(&mut env, 100, false, "test".to_string());
        assert_eq!(manager.get_date(), 100);
        assert_eq!(env.pmc.get("updates.date").map(String::as_str), Some("100"));

        manager.set_date(&mut env, 90, false, "back".to_string());
        assert_eq!(manager.get_date(), 100, "backwards date is logged only");

        // from_update +1 tolerance: 99 + 1 == 100 → silent return.
        manager.set_date(&mut env, 99, true, "upd".to_string());
        assert_eq!(manager.get_date(), 100);
    }

    #[test]
    fn get_short_update_date_clamps_to_unix_time() {
        let mut env = authorized_env();
        env.unix_time_value = 5_000;
        let mut manager = UpdatesManager::new(RecordingSink::default());
        assert_eq!(manager.get_short_update_date(&mut env), 5_000);
        manager.short_update_date = 6_000;
        assert_eq!(
            manager.get_short_update_date(&mut env),
            5_000,
            "min(short, now)"
        );
        manager.short_update_date = 4_000;
        assert_eq!(manager.get_short_update_date(&mut env), 4_000);
    }

    #[test]
    fn init_state_restores_from_pmc() {
        let mut env = authorized_env();
        env.pmc.insert("updates.pts".into(), "42".into());
        env.pmc.insert("updates.qts".into(), "7".into());
        env.pmc.insert("updates.date".into(), "500".into());
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.init_state(&mut env);
        assert_eq!(manager.get_pts(), 42);
        assert_eq!(manager.get_qts(), 7);
        assert_eq!(manager.get_date(), 500);
        assert_eq!(manager.date_source, "database");
        assert_eq!(
            env.sent_difference,
            [(42, 500, 7)],
            "get_difference(init_state)"
        );
    }

    #[test]
    fn init_state_without_pmc_fetches_updates_state() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.init_state(&mut env);
        assert_eq!(env.sent_updates_state, 1);
        assert!(manager.running_get_difference());
        assert_eq!(manager.sink().before_difference, 1);
    }

    #[test]
    fn init_state_drops_stored_state_when_ignoring_background() {
        let mut env = authorized_env();
        env.ignore_background = true;
        env.pmc.insert("updates.pts".into(), "42".into());
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.init_state(&mut env);
        assert!(env.pmc.is_empty(), "the stored state was dropped");
        assert_eq!(env.sent_updates_state, 1);
    }

    #[test]
    fn on_get_updates_state_applies_and_finishes_difference() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.running_get_difference = true;
        manager.on_get_updates_state(
            &mut env,
            telegram_api::updates_State {
                pts: 20,
                qts: 5,
                date: 900,
                seq: 3,
                unread_count: 0,
            },
            "GetUpdatesStateQuery",
        );
        assert_eq!(manager.get_pts(), 20);
        assert_eq!(manager.get_qts(), 5);
        assert_eq!(manager.get_date(), 900);
        assert_eq!(manager.get_seq(), 3);
        assert!(!manager.running_get_difference());
        assert_eq!(manager.sink().after_difference, 1);
    }

    #[test]
    fn on_get_updates_state_restores_from_max_pts() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(i32::MAX);
        manager.on_get_updates_state(
            &mut env,
            telegram_api::updates_State {
                pts: 33,
                qts: 2,
                date: 0,
                seq: 0,
                unread_count: 0,
            },
            "GetUpdatesStateQuery",
        );
        assert_eq!(manager.get_pts(), 33);
        assert_eq!(env.pmc.get("updates.pts").map(String::as_str), Some("33"));
    }

    #[test]
    fn failed_get_difference_backs_off_and_retries() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        manager.on_failed_get_difference(&mut env, Status::error(429, "FLOOD_WAIT_3"));
        assert_eq!(
            manager.next_deadline(),
            Some(1.0),
            "first retry in retry_time_ = 1s"
        );
        assert!(env.sent_difference.is_empty());

        env.clock = 0.5;
        manager.tick_timeouts(&mut env);
        assert!(env.sent_difference.is_empty(), "too early");

        env.clock = 1.5;
        manager.tick_timeouts(&mut env);
        assert_eq!(
            env.sent_difference,
            // QTS rides its -1 unknown sentinel (PtsManager.h:53-54) —
            // only PTS is clamped (UpdatesManager.cpp:506-508).
            [(10, 0, -1)],
            "retried with the stored state"
        );
    }

    #[test]
    fn failed_get_difference_persistent_timestamp_drops_pts() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        manager
            .on_failed_get_difference(&mut env, Status::error(400, "PERSISTENT_TIMESTAMP_INVALID"));
        assert!(!env.pmc.contains_key("updates.pts"));
        assert_eq!(env.sent_updates_state, 1, "set_pts(i32::MAX) → init_state");
    }

    // -- M.7.2 --------------------------------------------------------

    #[test]
    fn updates_too_long_triggers_get_difference() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        manager.on_get_updates(
            &mut env,
            telegram_api::Updates::updatesTooLong(telegram_api::updatesTooLong),
            Promise::default(),
        );
        assert_eq!(env.sent_difference, [(10, 0, -1)]);
    }

    #[test]
    fn update_short_message_becomes_new_message() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        manager.on_get_updates(
            &mut env,
            telegram_api::Updates::updateShortMessage(telegram_api::updateShortMessage {
                out: false,
                mentioned: true,
                media_unread: false,
                silent: false,
                id: 55,
                user_id: 42,
                message: "hi".to_string(),
                pts: 11,
                pts_count: 1,
                date: 999,
                ..Default::default()
            }),
            Promise::default(),
        );
        assert_eq!(
            manager.sink().pts_applied,
            [(55, "updateNewMessage")],
            "the synthesized message applied via the pending fast path"
        );
        assert_eq!(manager.get_pts(), 11);
    }

    #[test]
    fn updates_container_applies_users_then_updates_and_seq_date() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(100);
        // seq 1 continues seq_ = 0 (seq_begin == seq_ + 1) — applies with
        // the seq/date advancement.
        manager.on_get_updates(
            &mut env,
            telegram_api::Updates::updates(telegram_api::updates {
                updates: vec![
                    telegram_api::Update::updateUser(telegram_api::updateUser { user_id: 5 }),
                    new_message_update(42, 101, 1),
                ],
                users: vec![user(5)],
                chats: vec![chat(7)],
                date: 800,
                seq: 1,
            }),
            Promise::default(),
        );
        assert_eq!(manager.sink().users, [5]);
        assert_eq!(manager.sink().chats, [7]);
        assert_eq!(manager.sink().pts_applied, [(42, "updateNewMessage")]);
        assert_eq!(manager.get_seq(), 1);
        assert_eq!(manager.get_date(), 800);
    }

    #[test]
    fn out_of_order_seq_container_waits_then_applies() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        // The 50,000-window rejects postponement until the first
        // after_get_difference (C++ can_postpone_updates semantics).
        manager.skipped_postponed_updates_after_start = 0;
        manager.on_get_updates(
            &mut env,
            updates_container(vec![], 5, 700),
            Promise::default(),
        );
        assert_eq!(manager.get_seq(), 0, "gap: the container waits");
        assert_eq!(manager.pending_seq_updates.len(), 1);

        // seq [1, 5] fills the gap; the user object rides the container's
        // users vector (pre-application), while the updateUser entry is a
        // cache-invalidation arm (the M.7.12 user-group flip).
        manager.on_get_updates(
            &mut env,
            telegram_api::Updates::updatesCombined(telegram_api::updatesCombined {
                updates: vec![telegram_api::Update::updateUser(telegram_api::updateUser {
                    user_id: 9,
                })],
                users: vec![user(9)],
                chats: vec![],
                date: 600,
                seq_start: 1,
                seq: 5,
            }),
            Promise::default(),
        );
        assert_eq!(manager.sink().users, [9]);
        assert_eq!(manager.get_seq(), 5, "the fill applied and advanced seq");
        assert!(
            manager.pending_seq_updates.is_empty(),
            "the [5, 5] container became an old update"
        );
    }

    #[test]
    fn old_seq_container_is_dropped() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.seq = 10;
        manager.on_get_updates(
            &mut env,
            updates_container(vec![], 2, 0),
            Promise::default(),
        );
        assert_eq!(manager.get_seq(), 10);
        assert!(manager.pending_seq_updates.is_empty());
    }

    #[test]
    fn wrong_seq_parameters_trigger_get_difference() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        manager.on_get_updates(
            &mut env,
            telegram_api::Updates::updatesCombined(telegram_api::updatesCombined {
                updates: vec![],
                users: vec![],
                chats: vec![],
                date: 0,
                seq_start: 7,
                seq: 3,
            }),
            Promise::default(),
        );
        assert_eq!(env.sent_difference.len(), 1);
    }

    #[test]
    fn short_updates_apply_before_authorization() {
        let mut env = authorized_env();
        env.authorized = false;
        let mut manager = UpdatesManager::new(RecordingSink::default());

        // updateShort(updateLoginToken) — the auth intake.
        manager.on_get_updates(
            &mut env,
            telegram_api::Updates::updateShort(telegram_api::updateShort {
                update: Some(telegram_api::Update::updateLoginToken(
                    telegram_api::updateLoginToken,
                )),
                date: 1,
            }),
            Promise::default(),
        );
        assert_eq!(manager.sink().login_tokens, 1);

        // updateShort(updateConfig) applies without authorization.
        manager.on_get_updates(
            &mut env,
            telegram_api::Updates::updateShort(telegram_api::updateShort {
                update: Some(telegram_api::Update::updateConfig(
                    telegram_api::updateConfig,
                )),
                date: 1,
            }),
            Promise::default(),
        );
        assert_eq!(manager.sink().config_updates, 1);

        // Anything else before authorization is dropped.
        manager.on_get_updates(
            &mut env,
            telegram_api::Updates::updateShort(telegram_api::updateShort {
                update: Some(telegram_api::Update::updateUser(telegram_api::updateUser {
                    user_id: 1,
                })),
                date: 1,
            }),
            Promise::default(),
        );
        assert!(manager.sink().users.is_empty());
    }

    // -- M.7.3 --------------------------------------------------------

    #[test]
    fn update_message_id_pre_pass_routes_to_the_sink() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.on_get_updates(
            &mut env,
            updates_container(
                vec![
                    telegram_api::Update::updateMessageID(telegram_api::updateMessageID {
                        id: 77,
                        random_id: 12345,
                    }),
                    telegram_api::Update::updateUser(telegram_api::updateUser { user_id: 1 }),
                ],
                1,
                500,
            ),
            Promise::default(),
        );
        assert_eq!(manager.sink().message_ids, [(12345, 77, false)]);
    }

    #[test]
    fn channel_message_routes_to_the_channel_seam() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        manager.on_get_updates(
            &mut env,
            updates_container(
                vec![telegram_api::Update::updateNewChannelMessage(
                    telegram_api::updateNewChannelMessage {
                        message: Some(telegram_api::Message::message(telegram_api::message {
                            id: 5,
                            peer_id: Some(telegram_api::Peer::peerChannel(
                                telegram_api::peerChannel { channel_id: 900 },
                            )),
                            ..Default::default()
                        })),
                        pts: 3,
                        pts_count: 1,
                    },
                )],
                1,
                500,
            ),
            Promise::default(),
        );
        assert_eq!(manager.sink().channel_updates, [(900, 3, 1)]);
    }

    #[test]
    fn seq_gap_timer_fires_fill_seq_gap_get_difference() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        let receive_time = env.clock;
        manager.on_get_updates(
            &mut env,
            updates_container(vec![], 5, 0),
            Promise::default(),
        );
        // MAX_UNFILLED_GAP_TIME after the receive time the gap fires.
        env.clock = receive_time + MAX_UNFILLED_GAP_TIME + 0.01;
        manager.tick_timeouts(&mut env);
        assert_eq!(
            env.sent_difference.len(),
            1,
            "fill_seq_gap → get_difference"
        );
    }

    // -- M.7.4 --------------------------------------------------------

    #[test]
    fn qts_out_of_order_pair_applies_in_order() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.qts_manager.init(3);
        // Postponement is enabled after the first after_get_difference.
        manager.skipped_postponed_updates_after_start = 0;

        let managed_bot = |qts: i32| {
            PendingUpdate::Wire(telegram_api::Update::updateManagedBot(
                telegram_api::updateManagedBot {
                    qts,
                    user_id: 1,
                    bot_id: 2,
                },
            ))
        };

        // qts 5 cannot apply yet (5-1 > 3).
        manager.add_pending_qts_update(&mut env, managed_bot(5), 5, Promise::default());
        assert_eq!(manager.get_qts(), 3);
        assert_eq!(manager.pending_qts_updates.len(), 1);

        // qts 4 applies and unblocks 5.
        manager.add_pending_qts_update(&mut env, managed_bot(4), 4, Promise::default());
        assert_eq!(manager.get_qts(), 5);
        assert!(manager.pending_qts_updates.is_empty());
    }

    #[test]
    fn qts_backwards_update_is_skipped() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.qts_manager.init(10);
        manager.add_pending_qts_update(
            &mut env,
            PendingUpdate::Wire(telegram_api::Update::updateManagedBot(
                telegram_api::updateManagedBot {
                    qts: 8,
                    user_id: 1,
                    bot_id: 2,
                },
            )),
            8,
            Promise::default(),
        );
        assert_eq!(manager.get_qts(), 10, "already-applied qts skipped");
    }

    #[test]
    fn encrypted_message_routes_to_the_secret_chat_seam() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.qts_manager.init(3);
        manager.process_qts_update(
            &mut env,
            PendingUpdate::Wire(telegram_api::Update::updateNewEncryptedMessage(
                telegram_api::updateNewEncryptedMessage {
                    message: Some(telegram_api::EncryptedMessage::encryptedMessage(
                        telegram_api::encryptedMessage {
                            random_id: 1,
                            chat_id: 5,
                            date: 10,
                            bytes: vec![1, 2, 3],
                            file: Some(telegram_api::EncryptedFile::encryptedFileEmpty(
                                telegram_api::encryptedFileEmpty {},
                            )),
                        },
                    )),
                    qts: 4,
                },
            )),
            4,
            Promise::default(),
        );
        assert_eq!(manager.get_qts(), 4);
        assert_eq!(manager.sink().encrypted_messages, [1]);
    }

    // -- M.7.5 --------------------------------------------------------

    #[test]
    fn pts_gap_queue_applies_in_pts_order() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(2);
        let now = env.clock;

        // pts 10 (count 2) cannot attach (needs old 8).
        manager.add_pending_pts_update(
            &mut env,
            new_message_update(1, 10, 2).into(),
            10,
            2,
            now,
            Promise::default(),
            "test",
        );
        assert_eq!(manager.get_pts(), 2);
        assert_eq!(manager.pending_pts_updates.len(), 1);

        // pts 8 (count 6) attaches and unblocks pts 10.
        manager.add_pending_pts_update(
            &mut env,
            new_message_update(2, 8, 6).into(),
            8,
            6,
            now,
            Promise::default(),
            "test",
        );
        assert_eq!(manager.get_pts(), 10);
        assert!(manager.pending_pts_updates.is_empty());
        assert_eq!(
            manager.sink().pts_applied,
            [(2, "updateNewMessage"), (1, "updateNewMessage")],
            "pts 8 applied before pts 10"
        );
    }

    #[test]
    fn pts_multiset_orders_equal_pts_by_pts_count_desc() {
        let entry = |pts_count: i32| PendingPtsUpdate {
            update: PendingUpdate::Dummy,
            pts: 5,
            pts_count,
            receive_time: 0.0,
            promise: Promise::default(),
        };
        assert_eq!(
            pending_pts_order(&entry(3), &entry(1)),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            pending_pts_order(&entry(1), &entry(3)),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn pts_updates_postpone_during_get_difference_and_flush_after() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(2);
        // Postponement is enabled after the first after_get_difference.
        manager.skipped_postponed_updates_after_start = 0;
        manager.running_get_difference = true;
        let now = env.clock;

        manager.add_pending_pts_update(
            &mut env,
            new_message_update(1, 3, 1).into(),
            3,
            1,
            now,
            Promise::default(),
            "test",
        );
        assert_eq!(manager.get_pts(), 2, "not applied during getDifference");
        assert_eq!(manager.postponed_pts_updates.len(), 1);

        // after_get_difference flushes the postponed updates.
        manager.running_get_difference = false;
        manager.after_get_difference(&mut env);
        assert_eq!(manager.get_pts(), 3);
        assert_eq!(manager.sink().pts_applied, [(1, "updateNewMessage")]);
        assert!(manager.postponed_pts_updates.is_empty());
    }

    #[test]
    fn pts_delete_messages_with_empty_list_uses_dummy() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(2);
        manager.on_update(
            &mut env,
            PendingUpdate::Wire(telegram_api::Update::updateDeleteMessages(
                telegram_api::updateDeleteMessages {
                    messages: vec![],
                    pts: 3,
                    pts_count: 1,
                },
            )),
            Promise::default(),
        );
        assert_eq!(manager.get_pts(), 3);
        assert_eq!(
            manager.sink().pts_applied,
            [(0, "dummyUpdate")],
            "the messages-manager switch saw the dummyUpdate"
        );
    }

    #[test]
    fn pts_jump_restore_on_2_1e9_with_multiple_sessions() {
        let mut env = authorized_env();
        env.options.insert("session_count".into(), 2);
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(2_100_000_000);
        let now = env.clock;

        manager.add_pending_pts_update(
            &mut env,
            new_message_update(1, 1_050_000_000, 1).into(),
            1_050_000_000,
            1,
            now,
            Promise::default(),
            "test",
        );
        assert_eq!(manager.get_pts(), 1, "restore PTS");
        // The restore arms a near-immediate pts gap fill → getDifference.
        env.clock += 0.01;
        manager.tick_timeouts(&mut env);
        assert_eq!(env.sent_difference.len(), 1);
    }

    #[test]
    fn update_pts_changed_single_session_drops_state() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        manager.on_update(
            &mut env,
            PendingUpdate::Wire(telegram_api::Update::updatePtsChanged(
                telegram_api::updatePtsChanged,
            )),
            Promise::default(),
        );
        assert!(!env.pmc.contains_key("updates.pts"));
        assert_eq!(env.sent_updates_state, 1);
    }

    #[test]
    fn update_pts_changed_multi_session_resets_to_one() {
        let mut env = authorized_env();
        env.options.insert("session_count".into(), 2);
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        manager.on_update(
            &mut env,
            PendingUpdate::Wire(telegram_api::Update::updatePtsChanged(
                telegram_api::updatePtsChanged,
            )),
            Promise::default(),
        );
        assert_eq!(manager.get_pts(), 1);
        assert_eq!(env.pmc.get("updates.pts").map(String::as_str), Some("1"));
        assert_eq!(
            env.sent_difference,
            [(1, 0, -1)],
            "get_difference(updatePtsChanged)"
        );
    }

    #[test]
    fn unsupported_updates_are_enumerated_drops() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        seed_pts(&mut manager, 0);
        manager.on_get_updates(
            &mut env,
            updates_container(
                vec![telegram_api::Update::updateFavedStickers(
                    telegram_api::updateFavedStickers,
                )],
                1,
                500,
            ),
            Promise::default(),
        );
        assert_eq!(
            manager.sink().unsupported,
            [(
                telegram_api::updateFavedStickers::ID,
                "updateFavedStickers".to_string()
            )],
            "trace-dropped with the scheme name"
        );
    }

    #[test]
    fn old_pts_updates_are_skipped_via_the_sink_seam() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        let now = env.clock;
        manager.add_pending_pts_update(
            &mut env,
            new_message_update(1, 5, 1).into(),
            5,
            1,
            now,
            Promise::default(),
            "test",
        );
        assert_eq!(
            manager.sink().skipped,
            [(5, 10, 1)],
            "skip_old_pending_pts_update"
        );
        assert!(manager.sink().pts_applied.is_empty());
    }

    // -- M.7.6 --------------------------------------------------------

    fn plain_message(id: i32) -> telegram_api::Message {
        telegram_api::Message::message(telegram_api::message {
            id,
            ..telegram_api::message::default()
        })
    }

    #[test]
    fn difference_empty_applies_date_seq_and_drops_pending() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.qts_manager.init(3);
        // Queue a pending qts update (postponement enabled).
        manager.skipped_postponed_updates_after_start = 0;
        manager.add_pending_qts_update(
            &mut env,
            PendingUpdate::Wire(telegram_api::Update::updateManagedBot(
                telegram_api::updateManagedBot {
                    qts: 5,
                    user_id: 1,
                    bot_id: 2,
                },
            )),
            5,
            Promise::default(),
        );
        assert_eq!(manager.pending_qts_updates.len(), 1);

        manager.on_get_difference(
            &mut env,
            telegram_api::updates_Difference::updates_differenceEmpty(
                telegram_api::updates_differenceEmpty { date: 777, seq: 4 },
            ),
        );
        assert_eq!(manager.get_date(), 777);
        assert_eq!(manager.get_seq(), 4);
        assert!(
            manager.pending_qts_updates.is_empty(),
            "pending QTS dropped"
        );
        assert_eq!(manager.sink().after_difference, 1);
    }

    #[test]
    fn difference_full_applies_users_messages_updates_and_state() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        manager.on_get_difference(
            &mut env,
            telegram_api::updates_Difference::updates_difference(
                telegram_api::updates_difference {
                    new_messages: vec![plain_message(77)],
                    new_encrypted_messages: vec![],
                    other_updates: vec![new_message_update(78, 11, 1)],
                    chats: vec![chat(3)],
                    users: vec![user(2)],
                    state: Some(telegram_api::updates_State {
                        pts: 11,
                        qts: 6,
                        date: 900,
                        seq: 5,
                        unread_count: 0,
                    }),
                },
            ),
        );
        assert_eq!(manager.sink().users, [2]);
        assert_eq!(manager.sink().chats, [3]);
        assert_eq!(manager.sink().difference_messages, [77]);
        assert_eq!(
            manager.sink().pts_applied,
            [(78, "updateNewMessage")],
            "force-apply path"
        );
        assert_eq!(manager.get_pts(), 11);
        assert_eq!(manager.get_qts(), 6);
        assert_eq!(manager.get_date(), 900);
        assert_eq!(manager.get_seq(), 5);
        assert!(!manager.running_get_difference());
        assert_eq!(manager.sink().after_difference, 1);
    }

    #[test]
    fn difference_slice_without_progress_switches_back() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        manager.qts_manager.init(2);
        manager.on_get_difference(
            &mut env,
            telegram_api::updates_Difference::updates_differenceSlice(
                telegram_api::updates_differenceSlice {
                    new_messages: vec![],
                    new_encrypted_messages: vec![],
                    other_updates: vec![],
                    chats: vec![],
                    users: vec![],
                    intermediate_state: Some(telegram_api::updates_State {
                        pts: 10,
                        qts: 2,
                        date: 0,
                        seq: 0,
                        unread_count: 0,
                    }),
                },
            ),
        );
        assert!(
            !manager.running_get_difference(),
            "switch back to update processing"
        );
        assert!(env.sent_difference.is_empty(), "no re-run of getDifference");
        assert_eq!(manager.sink().after_difference, 1);
    }

    #[test]
    fn difference_slice_with_progress_reruns_get_difference() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        manager.qts_manager.init(2);
        manager.on_get_difference(
            &mut env,
            telegram_api::updates_Difference::updates_differenceSlice(
                telegram_api::updates_differenceSlice {
                    new_messages: vec![],
                    new_encrypted_messages: vec![],
                    other_updates: vec![],
                    chats: vec![],
                    users: vec![],
                    intermediate_state: Some(telegram_api::updates_State {
                        pts: 11,
                        qts: 2,
                        date: 0,
                        seq: 0,
                        unread_count: 0,
                    }),
                },
            ),
        );
        assert!(
            manager.running_get_difference(),
            "the slice re-ran getDifference"
        );
        assert_eq!(env.sent_difference, [(11, 0, 2)]);
        assert_eq!(
            manager.sink().after_difference,
            0,
            "after_get_difference deferred"
        );
    }

    #[test]
    fn difference_too_long_resets_pts_and_reruns() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        manager.on_get_difference(
            &mut env,
            telegram_api::updates_Difference::updates_differenceTooLong(
                telegram_api::updates_differenceTooLong { pts: 50 },
            ),
        );
        assert_eq!(manager.get_pts(), 50);
        assert_eq!(env.sent_difference, [(50, 0, -1)]);
    }

    #[test]
    fn invalid_poll_message_regets_the_difference() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(10);
        manager.sink().invalid_poll = true;
        manager.on_get_difference(
            &mut env,
            telegram_api::updates_Difference::updates_difference(
                telegram_api::updates_difference {
                    new_messages: vec![plain_message(1)],
                    new_encrypted_messages: vec![],
                    other_updates: vec![],
                    chats: vec![],
                    users: vec![],
                    state: Some(telegram_api::updates_State {
                        pts: 11,
                        qts: 1,
                        date: 1,
                        seq: 1,
                        unread_count: 0,
                    }),
                },
            ),
        );
        assert_eq!(
            env.sent_difference,
            [(10, 0, -1)],
            "reget difference with the same state"
        );
        assert!(manager.running_get_difference());
        assert!(
            manager.sink().difference_messages.is_empty(),
            "the difference was not applied"
        );
    }

    #[test]
    fn get_pts_update_repairs_the_short_gap() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(2);
        let now = env.clock;
        // A pending pts-4 update that needs pts 3 first.
        manager.add_pending_pts_update(
            &mut env,
            new_message_update(5, 4, 1).into(),
            4,
            1,
            now,
            Promise::default(),
            "test",
        );
        assert_eq!(manager.pending_pts_updates.len(), 1);

        manager.on_get_pts_update(
            &mut env,
            3,
            telegram_api::updates_Difference::updates_differenceSlice(
                telegram_api::updates_differenceSlice {
                    new_messages: vec![],
                    new_encrypted_messages: vec![],
                    other_updates: vec![new_message_update(9, 3, 1)],
                    chats: vec![],
                    users: vec![],
                    intermediate_state: Some(telegram_api::updates_State {
                        pts: 0,
                        qts: 0,
                        date: 0,
                        seq: 0,
                        unread_count: 0,
                    }),
                },
            ),
        );
        assert_eq!(
            manager.sink().pts_applied,
            [(9, "updateNewMessage"), (5, "updateNewMessage")],
            "the repair applied and unblocked the queued pts-4 update"
        );
        assert_eq!(manager.get_pts(), 4);
        assert!(
            manager.pending_pts_updates.is_empty(),
            "the whole queue drained"
        );
    }

    // ==== M.7.8/M.7.9 — the container statics --------

    fn peer_user(user_id: i64) -> telegram_api::Peer {
        telegram_api::Peer::peerUser(telegram_api::peerUser { user_id })
    }

    fn dialog_of(peer: &telegram_api::Peer) -> DialogId {
        DialogId::from_peer(peer)
    }

    fn message_for(user_id: i64, id: i32) -> telegram_api::Message {
        telegram_api::Message::message(telegram_api::message {
            id,
            peer_id: Some(peer_user(user_id)),
            ..telegram_api::message::default()
        })
    }

    #[test]
    fn statics_classify_container_emptiness() {
        assert!(are_empty_updates(&telegram_api::Updates::updatesTooLong(
            telegram_api::updatesTooLong {},
        )));
        assert!(are_empty_updates(&updates_container(vec![], 0, 0)));
        assert!(!are_empty_updates(&updates_container(
            vec![new_message_update(5, 4, 1)],
            0,
            0
        )));
        assert!(!are_empty_updates(&telegram_api::Updates::updateShort(
            telegram_api::updateShort {
                update: Some(telegram_api::Update::updateConfig(
                    telegram_api::updateConfig {}
                )),
                date: 0,
            },
        )));
    }

    #[test]
    fn statics_extract_new_messages_and_filter_service_ttl() {
        let ttl_service = telegram_api::Update::updateNewMessage(telegram_api::updateNewMessage {
            message: Some(telegram_api::Message::messageService(
                telegram_api::messageService {
                    id: 9,
                    peer_id: Some(peer_user(100)),
                    action: Some(telegram_api::MessageAction::messageActionSetMessagesTTL(
                        telegram_api::messageActionSetMessagesTTL {
                            flags: 0,
                            period: 60,
                            auto_setting_from: 0,
                        },
                    )),
                    ..telegram_api::messageService::default()
                },
            )),
            pts: 2,
            pts_count: 1,
        });
        let updates = updates_container(
            vec![
                new_message_update(5, 4, 1),
                ttl_service,
                telegram_api::Update::updateNewScheduledMessage(
                    telegram_api::updateNewScheduledMessage {
                        message: Some(message_for(100, 7)),
                    },
                ),
                new_message_update(6, 5, 1),
            ],
            0,
            0,
        );
        let messages = get_new_messages(&updates);
        assert_eq!(messages.len(), 3, "the service-TTL marker is filtered out");
        assert_eq!(message_id_from_message(messages[0].0, false), 5 << 20);
        assert!(!messages[0].1);
        assert!(messages[1].1, "the scheduled pair is flagged");
    }

    #[test]
    fn statics_message_by_random_id() {
        let updates = updates_container(
            vec![
                telegram_api::Update::updateMessageID(telegram_api::updateMessageID {
                    id: 5,
                    random_id: 42,
                }),
                new_message_update(5, 4, 1),
            ],
            0,
            0,
        );
        let dialog = dialog_of(&peer_user(100));
        let found = get_message_by_random_id(&updates, dialog, 42);
        assert!(found.is_some(), "the container carries message 5");
        let other_dialog = dialog_of(&peer_user(101));
        assert!(get_message_by_random_id(&updates, other_dialog, 42).is_none());
        assert!(get_message_by_random_id(&updates, dialog, 43).is_none());
    }

    #[test]
    fn statics_sent_random_ids_match_container_messages() {
        let updates = updates_container(
            vec![
                telegram_api::Update::updateMessageID(telegram_api::updateMessageID {
                    id: 5,
                    random_id: 42,
                }),
                telegram_api::Update::updateMessageID(telegram_api::updateMessageID {
                    id: 99,
                    random_id: 43,
                }),
                new_message_update(5, 4, 1),
            ],
            0,
            0,
        );
        let random_ids = get_sent_messages_random_ids(&updates);
        assert_eq!(random_ids, HashSet::from([42]));
    }

    #[test]
    fn statics_update_ids_leaf_short_container() {
        assert_eq!(
            get_update_ids(&telegram_api::Updates::updatesTooLong(
                telegram_api::updatesTooLong {}
            )),
            vec![telegram_api::updatesTooLong::ID]
        );
        assert_eq!(
            get_update_ids(&telegram_api::Updates::updateShort(
                telegram_api::updateShort {
                    update: Some(telegram_api::Update::updateConfig(
                        telegram_api::updateConfig {}
                    )),
                    date: 0,
                }
            )),
            vec![telegram_api::updateConfig::ID]
        );
        assert_eq!(
            get_update_ids(&updates_container(
                vec![
                    new_message_update(5, 4, 1),
                    telegram_api::Update::updateChannel(telegram_api::updateChannel {
                        channel_id: 55
                    }),
                ],
                0,
                0
            )),
            vec![
                telegram_api::updateNewMessage::ID,
                telegram_api::updateChannel::ID
            ]
        );
    }

    #[test]
    fn statics_chat_dialog_ids_filter_invalid_and_unsupported() {
        let chats = vec![
            telegram_api::Chat::channel(telegram_api::channel {
                id: 55,
                ..telegram_api::channel::default()
            }),
            telegram_api::Chat::chat(telegram_api::chat {
                id: 3,
                ..telegram_api::chat::default()
            }),
            telegram_api::Chat::chatEmpty(telegram_api::chatEmpty { id: 0 }),
        ];
        let updates = telegram_api::Updates::updatesCombined(telegram_api::updatesCombined {
            updates: vec![],
            users: vec![],
            chats: chats.clone(),
            date: 0,
            seq_start: 0,
            seq: 0,
        });
        let dialog_ids = get_chat_dialog_ids(&updates, false);
        assert_eq!(
            dialog_ids,
            vec![
                DialogId::from_channel_id(ChannelId::new(55)),
                DialogId::from_chat_id(ChatId::new(3)),
            ],
            "the empty chat is invalid and skipped"
        );

        // The unsupported test/main dummy channel drops out once plural.
        let unsupported_channel = telegram_api::Chat::channel(telegram_api::channel {
            id: 1_535_424_647,
            ..telegram_api::channel::default()
        });
        let updates = telegram_api::Updates::updatesCombined(telegram_api::updatesCombined {
            updates: vec![],
            users: vec![],
            chats: vec![unsupported_channel, chats[0].clone(), chats[1].clone()],
            date: 0,
            seq_start: 0,
            seq: 0,
        });
        let dialog_ids = get_chat_dialog_ids(&updates, false);
        assert_eq!(
            dialog_ids,
            vec![
                DialogId::from_channel_id(ChannelId::new(55)),
                DialogId::from_chat_id(ChatId::new(3)),
            ],
            "the dummy channel is removed"
        );
    }

    #[test]
    fn statics_extract_presentation_params_removes_entry() {
        let mut updates = updates_container(
            vec![
                telegram_api::Update::updateGroupCallConnection(
                    telegram_api::updateGroupCallConnection {
                        flags: 0,
                        presentation: true,
                        params: Some(telegram_api::DataJSON {
                            data: "params".to_string(),
                        }),
                    },
                ),
                new_message_update(5, 4, 1),
            ],
            0,
            0,
        );
        assert_eq!(
            extract_join_group_call_presentation_params(&mut updates),
            "params"
        );
        let remaining = get_updates_vec(&updates).expect("container");
        assert_eq!(remaining.len(), 1, "the presentation update was removed");
        assert!(matches!(
            remaining[0],
            telegram_api::Update::updateNewMessage(_)
        ));
        // A non-presentation connection stays put.
        let mut updates = updates_container(
            vec![telegram_api::Update::updateGroupCallConnection(
                telegram_api::updateGroupCallConnection {
                    flags: 0,
                    presentation: false,
                    params: Some(telegram_api::DataJSON {
                        data: "main".to_string(),
                    }),
                },
            )],
            0,
            0,
        );
        assert_eq!(
            extract_join_group_call_presentation_params(&mut updates),
            ""
        );
        assert_eq!(get_updates_vec(&updates).expect("container").len(), 1);
    }

    #[test]
    fn statics_extract_star_gift_and_group_call_messages() {
        let mut updates = updates_container(
            vec![
                new_message_update(5, 4, 1),
                telegram_api::Update::updateGroupCallMessage(
                    telegram_api::updateGroupCallMessage {
                        call: Some(telegram_api::InputGroupCall::inputGroupCall(
                            telegram_api::inputGroupCall::default(),
                        )),
                        message: Some(telegram_api::GroupCallMessage::default()),
                    },
                ),
                telegram_api::Update::updateStarGiftCraftFail(
                    telegram_api::updateStarGiftCraftFail {},
                ),
            ],
            0,
            0,
        );
        assert!(extract_star_gift_craft_fail(&mut updates));
        assert!(!extract_star_gift_craft_fail(&mut updates));
        let group_call_messages = extract_group_call_messages(&mut updates);
        assert_eq!(group_call_messages.len(), 1);
        assert_eq!(get_updates_vec(&updates).expect("container").len(), 1);
    }

    #[test]
    fn statics_extract_story_owner_and_business_rules() {
        let make = |is_business_count: usize| {
            let mut entries = vec![telegram_api::Update::updateStory(
                telegram_api::updateStory {
                    peer: Some(peer_user(7)),
                    story: Some(Box::new(telegram_api::StoryItem::storyItemSkipped(
                        telegram_api::storyItemSkipped {
                            id: 3,
                            ..telegram_api::storyItemSkipped::default()
                        },
                    ))),
                },
            )];
            for extra in 0..is_business_count {
                entries.push(new_message_update(extra as i32 + 10, extra as i32 + 2, 1));
            }
            updates_container(entries, 0, 0)
        };
        let owner = dialog_of(&peer_user(7));
        let other = dialog_of(&peer_user(8));

        let mut updates = make(0);
        assert!(
            extract_story(&mut updates, owner, false).is_none(),
            "the skipped story is rejected without the business flag"
        );
        assert!(extract_story(&mut updates, other, false).is_none());
        let story = extract_story(&mut updates, owner, true);
        assert!(
            story.is_some(),
            "the business path accepts the skipped story"
        );
        assert!(
            get_updates_vec(&updates).expect("container").is_empty(),
            "the extracted entry is drained"
        );

        // The business size gate: 3-entry containers are refused.
        let mut updates = make(2);
        assert!(extract_story(&mut updates, owner, true).is_none());
    }

    #[test]
    fn statics_edit_message_pts_single_plural_scheduled() {
        let scheduled_date = (1 << 30) + 500;
        let updates = updates_container(
            vec![
                telegram_api::Update::updateEditMessage(telegram_api::updateEditMessage {
                    message: Some(message_for(100, 5)),
                    pts: 12,
                    pts_count: 1,
                }),
                telegram_api::Update::updateNewScheduledMessage(
                    telegram_api::updateNewScheduledMessage {
                        message: Some(telegram_api::Message::message(telegram_api::message {
                            id: 8,
                            date: scheduled_date,
                            peer_id: Some(peer_user(100)),
                            ..telegram_api::message::default()
                        })),
                    },
                ),
            ],
            0,
            0,
        );
        let edit_full = (
            dialog_of(&peer_user(100)),
            server_message_id_to_message_id(5),
        );
        assert_eq!(get_update_edit_message_pts(&updates, edit_full), 12);
        let other_edit = (
            dialog_of(&peer_user(100)),
            server_message_id_to_message_id(6),
        );
        assert_eq!(get_update_edit_message_pts(&updates, other_edit), 0);

        let scheduled_full = (
            dialog_of(&peer_user(100)),
            scheduled_message_id_to_message_id(8, scheduled_date),
        );
        assert_eq!(get_update_edit_message_pts(&updates, scheduled_full), -2);

        let plural = updates_container(
            vec![
                telegram_api::Update::updateEditMessage(telegram_api::updateEditMessage {
                    message: Some(message_for(100, 5)),
                    pts: 12,
                    pts_count: 1,
                }),
                telegram_api::Update::updateEditMessage(telegram_api::updateEditMessage {
                    message: Some(message_for(100, 5)),
                    pts: 13,
                    pts_count: 1,
                }),
            ],
            0,
            0,
        );
        assert_eq!(get_update_edit_message_pts(&plural, edit_full), 0);
    }

    #[test]
    fn message_id_layout_pins() {
        assert_eq!(server_message_id_to_message_id(5), 5 << 20);
        assert!(is_valid_message_id(server_message_id_to_message_id(5)));
        assert!(is_valid_message_id((5 << 20) | MESSAGE_ID_TYPE_LOCAL));
        assert!(
            !is_valid_message_id(3),
            "yet-unsent|local is not a valid type"
        );
        assert!(!is_valid_message_id(MESSAGE_ID_MAX + (5 << 20)));

        let scheduled = scheduled_message_id_to_message_id(8, (1 << 30) + 500);
        assert!(is_scheduled_server_message_id(scheduled));
        assert!(is_valid_scheduled_message_id(scheduled));
        assert!(!is_valid_message_id(scheduled));
        assert_eq!(scheduled_server_message_id_of(scheduled), 8);
        assert_eq!(scheduled_message_id_to_message_id(8, 100), 0, "past date");
        assert_eq!(scheduled_message_id_to_message_id(0, (1 << 30) + 500), 0);
    }

    // ==== M.7.16/M.7.17 — the aux machinery and the reloaders --------

    #[test]
    fn aux_auth_key_accounting_evicts_the_oldest_session() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        env.options.insert("session_count".to_string(), 2);

        manager.on_update_from_auth_key_id(&mut env, 11);
        env.clock += 10.0;
        manager.on_update_from_auth_key_id(&mut env, 22);
        env.clock += 10.0;
        manager.on_update_from_auth_key_id(&mut env, 33);
        assert!(
            !manager.session_infos.contains_key(&11),
            "three sessions with a cap of two evict the oldest"
        );
        assert!(manager.session_infos.contains_key(&22));
        assert!(manager.session_infos.contains_key(&33));

        // The zero auth key is ignored (cpp:1909).
        manager.on_update_from_auth_key_id(&mut env, 0);
        assert_eq!(manager.session_infos.len(), 2);
    }

    #[test]
    fn aux_pong_drives_the_state_intake() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        manager.pts_manager.init(5);
        manager.seq = 3;

        manager.ping_server(&mut env);
        assert_eq!(env.sent_pings, 1);
        manager.ping_server(&mut env);
        assert_eq!(env.sent_pings, 1, "the ping is single-flight");

        let state = |pts: i32, seq: i32| telegram_api::updates_State {
            pts,
            qts: 0,
            date: 0,
            seq,
            unread_count: 0,
        };
        // A current pong changes nothing.
        manager.on_server_pong(&mut env, Some(state(5, 3)));
        assert!(env.sent_difference.is_empty());
        // A pong from ahead of the local state triggers getDifference
        // (the C++ `state->pts_ > get_pts() || state->seq_ > seq_`).
        manager.on_server_pong(&mut env, Some(state(6, 3)));
        assert_eq!(env.sent_difference.len(), 1);
        // A null pong (the failed query) would trigger it, but the
        // difference is already running.
        manager.on_server_pong(&mut env, None);
        assert_eq!(env.sent_difference.len(), 1);
    }

    #[test]
    fn aux_speed_limit_notice_throttles_per_lane() {
        let mut env = authorized_env();
        env.options
            .insert("upload_premium_speedup_notify_period".to_string(), 60);
        let mut manager = UpdatesManager::new(RecordingSink::default());

        manager.notify_speed_limited(&mut env, true);
        assert_eq!(manager.sink().speed_limit_notifications, [true]);
        manager.notify_speed_limited(&mut env, true);
        assert_eq!(
            manager.sink().speed_limit_notifications.len(),
            1,
            "the notice throttles inside the option period"
        );
        env.clock += 61.0;
        manager.notify_speed_limited(&mut env, true);
        assert_eq!(manager.sink().speed_limit_notifications.len(), 2);
        // The upload/download lanes throttle independently.
        manager.notify_speed_limited(&mut env, false);
        assert_eq!(
            manager.sink().speed_limit_notifications,
            [true, true, false]
        );
    }

    #[test]
    fn reloaders_gate_and_rearm_like_the_cpp() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());

        // Bots never reload.
        env.bot = true;
        manager.try_reload_data(&mut env);
        assert_eq!(manager.sink().reload_data_calls, 0);
        env.bot = false;
        env.online = true;

        // The first attempt passes the gate (now >= next 0) and re-arms
        // into the Random::fast(3000, 4200) window.
        manager.try_reload_data(&mut env);
        assert_eq!(manager.sink().reload_data_calls, 1);
        let rearm = manager.next_data_reload_time - env.clock;
        assert!(
            (3000.0..=4200.0).contains(&rearm),
            "the re-arm window is 3000..4200, got {rearm}"
        );

        // Inside the window the attempt only reschedules.
        manager.try_reload_data(&mut env);
        assert_eq!(manager.sink().reload_data_calls, 1);
        assert!(manager.data_reload_timeout.has_timeout());

        // The often pair re-arms into its own 300..420 window.
        manager.try_reload_often_data(&mut env);
        assert_eq!(manager.sink().often_reload_calls, 1);
        let often_rearm = manager.next_often_data_reload_time - env.clock;
        assert!((300.0..=420.0).contains(&often_rearm));

        // Unauthorized bodies return without touching the timers.
        env.authorized = false;
        let calls = manager.sink().reload_data_calls;
        manager.try_reload_data(&mut env);
        assert_eq!(manager.sink().reload_data_calls, calls);
    }

    #[test]
    fn dispatcher_enumerates_drops_without_a_wildcard() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        let update = telegram_api::Update::updateUserTyping(telegram_api::updateUserTyping {
            ..telegram_api::updateUserTyping::default()
        });
        let name = update_id_and_name(&update).1;
        manager.on_update(&mut env, update.into(), Promise::default());
        assert_eq!(
            manager.sink().unsupported,
            [(telegram_api::updateUserTyping::ID, name.to_string())]
        );

        // The channel-pts family reaches the channel machine, not the
        // drop counter (the updateEditChannelMessage arm of cpp:3870).
        let edit = telegram_api::Update::updateEditChannelMessage(
            telegram_api::updateEditChannelMessage {
                message: Some(message_for(100, 5)),
                pts: 6,
                pts_count: 2,
            },
        );
        manager.on_update(&mut env, edit.into(), Promise::default());
        assert_eq!(manager.sink().channel_updates, [(100, 6, 2)]);
        assert_eq!(manager.sink().unsupported.len(), 1);
    }

    #[test]
    fn pins_match_the_cpp_header() {
        // UpdatesManager.h:161-168 + h:255/h:308 and the synthetic ids of
        // h:42/h:63.
        assert_eq!(FORCED_GET_DIFFERENCE_PTS_DIFF, 100_000);
        assert_eq!(GAP_TIMEOUT_UPDATE_COUNT, 20);
        assert_eq!(MIN_UNFILLED_GAP_TIME, 0.05);
        assert_eq!(MAX_UNFILLED_GAP_TIME, 0.7);
        assert_eq!(MAX_PTS_SAVE_DELAY, 0.05);
        assert_eq!(UPDATE_APPLY_WARNING_TIME, 0.1);
        // The dead debug kill switch pins to false at compile time.
        const { assert!(!DROP_PTS_UPDATES) };
        assert_eq!(AFTER_GET_DIFFERENCE_SOURCE, "after get difference");
        assert_eq!(SKIPPED_POSTPONED_UPDATES_AFTER_START, 50_000);
        assert_eq!(PendingUpdate::Dummy.get_id(), 1_234_567_891);
        assert_eq!(PendingUpdate::Dummy.name(), "dummyUpdate");
        assert_eq!(
            PendingUpdate::SentMessage {
                random_id: 0,
                message_id: 0,
                date: 0,
                ttl_period: 0
            }
            .get_id(),
            1_234_567_890
        );
        assert_eq!(
            PendingUpdate::SentMessage {
                random_id: 0,
                message_id: 0,
                date: 0,
                ttl_period: 0
            }
            .name(),
            "updateSentMessage"
        );
        // h:255 — `date_source_ = "nowhere"` is the fresh-ctor state.
        let manager: UpdatesManager<RecordingSink> = UpdatesManager::new(RecordingSink::default());
        assert_eq!(manager.date_source, "nowhere");
    }

    // ==== M.7.10/M.7.11 — the acceptability filter --------

    /// A sink whose cache is empty: every manager ask fails, so any
    /// update touching a non-zero peer is unacceptable.
    #[derive(Default)]
    struct EmptyCacheSink;

    impl UpdatesSink for EmptyCacheSink {
        fn on_user(&mut self, _user: telegram_api::User) {}
        fn on_chat(&mut self, _chat: telegram_api::Chat) {}
        fn process_pts_update(&mut self, _update: PendingUpdate) {}
        fn add_pending_channel_update(
            &mut self,
            _peer: Option<telegram_api::Peer>,
            _update: PendingUpdate,
            _pts: i32,
            _pts_count: i32,
            _promise: Promise<()>,
            _source: &'static str,
        ) {
        }
    }

    #[test]
    fn acceptability_asks_the_sink_for_known_peers() {
        let mut env = authorized_env();
        let mut manager = UpdatesManager::new(RecordingSink::default());
        // The harness cache is populated: a regular new message passes.
        let update = telegram_api::Update::updateNewMessage(telegram_api::updateNewMessage {
            message: Some(message_for(100, 5)),
            pts: 4,
            pts_count: 1,
        });
        assert!(manager.is_acceptable_update(&mut env, &update));

        // The empty cache: the message peer user 100 is unknown.
        let mut manager = UpdatesManager::new(EmptyCacheSink);
        let update = telegram_api::Update::updateNewMessage(telegram_api::updateNewMessage {
            message: Some(message_for(100, 5)),
            pts: 4,
            pts_count: 1,
        });
        assert!(!manager.is_acceptable_update(&mut env, &update));

        // A zero-id peer is the absent mark and passes even on the empty
        // cache.
        let update = telegram_api::Update::updateNewMessage(telegram_api::updateNewMessage {
            message: Some(telegram_api::Message::message(telegram_api::message {
                id: 5,
                peer_id: Some(peer_user(0)),
                from_id: Some(peer_user(0)),
                ..telegram_api::message::default()
            })),
            pts: 4,
            pts_count: 1,
        });
        assert!(
            manager.is_acceptable_update(&mut env, &update),
            "the zero-id peers are the C++ nullptr and pass"
        );
    }

    #[test]
    fn acceptability_ladder_replaces_channel_updates_with_too_long() {
        let mut env = authorized_env();
        let mut sink = RecordingSink::default();
        sink.deny_users.insert(100);
        let mut manager = UpdatesManager::new(sink);
        seed_pts(&mut manager, 0);

        let update =
            telegram_api::Update::updateNewChannelMessage(telegram_api::updateNewChannelMessage {
                message: Some(telegram_api::Message::message(telegram_api::message {
                    id: 5,
                    peer_id: Some(telegram_api::Peer::peerChannel(telegram_api::peerChannel {
                        channel_id: 55,
                    })),
                    from_id: Some(peer_user(100)),
                    ..telegram_api::message::default()
                })),
                pts: 6,
                pts_count: 2,
            });
        let now = env.clock;
        manager.on_pending_updates(
            &mut env,
            vec![Some(PendingUpdate::Wire(update))],
            0,
            0,
            0,
            now,
            Some(Promise::default()),
            "test",
        );
        assert_eq!(
            manager.sink().channel_too_long,
            [(55, 6, false)],
            "the unacceptable update was replaced with updateChannelTooLong"
        );
        assert!(
            manager.sink().channel_updates.is_empty(),
            "the channel pts machine was not fed the original update"
        );
    }
}
