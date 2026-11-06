//! `StatisticsManager` (td/telegram/StatisticsManager.{h,cpp}) — the M.35
//! Wave-3 statistics manager. The value plane (the StatsGraph conversion
//! plus the percentage/absolute-value helpers) is the real port; the nine
//! query families (net-drain 38) ride the `StatisticsNetKind` plane: the
//! dc chain resolves at the send leg (`StatisticsDc`), the promises park
//! in the FIFO [`PendingStatisticsQueries`] lots (the C++ ResultHandler
//! objects) and the answers drain through `Td::on_statistics_net_answer`.
//! The `getStatisticalGraph` rider (`load_statistics_graph` →
//! `stats.loadAsyncGraph`, cpp:900-918) rides the same plane — its
//! td_api arm (Requests.cpp:8279-8285) predates the row.
//!
//! # Recorded collapses
//! - the broadcast answer's recent-interactions walk
//!   (`on_update_message_interaction_info`, StatisticsManager.cpp:244) —
//!   the MessagesManager interaction-info update plane is the M.10
//!   residue; the env seam logs the walk.
//! - the public-forwards channel-difference walk
//!   (`get_channel_differences_if_needed`, StatisticsManager.cpp:1049-1075)
//!   — the difference request rides the updates engine (net-drain 37);
//!   the collapse runs the `on_get_public_forwards` conversion directly.
//! - the SRP withdrawal password (`get_input_check_password_srp`) — the
//!   PasswordManager plane; the promise parks with the seam and the wire
//!   query rides the empty-SRP placeholder (the
//!   `StarManager::get_withdrawal_url` collapse, StarManager.cpp:1701-1708).

use crate::chat_manager::ChannelId;
use crate::dialog_id::DialogId;
use crate::message_id::MessageFullId;
use crate::story_ids::StoryFullId;
use crate::td_api;
use crate::user_manager::AccessRights;
use tdmtproto::telegram_api;
use tdnet::net_query::DcId;
use tdutils::promise::Promise;
use tdutils::status::{Result, Status};

/// The dc chain outcome of [`StatisticsManagerEnv::statistics_dc_id`] —
/// the collapsed `get_channel_statistics_dc_id` promise (the sync-cached
/// dc answers immediately; a missing/unexact dc refires the full query
/// with the continuation parked, ChatManager.cpp:3846-3876).
#[derive(Clone, Copy, Debug)]
pub enum StatisticsDc {
    /// The synchronously-resolved exact dc.
    Resolved(DcId),
    /// The full query refired; the answer leg drains the parked request
    /// through `Td::finish_pending_channel_statistics_dc_id`.
    Refired,
}

/// The [`StatisticsManager`] in-flight query kinds — the C++
/// ResultHandler classes of the net-drain 38 families
/// (StatisticsManager.cpp:180-737). The handler state the answers read
/// (the channel/dialog ids, the megagroup branch, every `promise_`)
/// rides the [`PendingStatisticsQueries`] lots on the manager — the
/// kinds stay `Copy`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum StatisticsNetKind {
    /// `GetBroadcastStatsQuery`/`GetMegagroupStatsQuery` (cpp:180-262) —
    /// `stats.getBroadcastStats`/`stats.getMegagroupStats`; the branch
    /// flag rides the lot entry.
    ChannelStats,
    /// `GetMessageStatsQuery` (cpp:516-552) — `stats.getMessageStats`.
    MessageStats,
    /// `GetStoryStatsQuery` (cpp:560-594) — `stats.getStoryStats`.
    StoryStats,
    /// `GetPollStatsQuery` (cpp:596-630) — `stats.getPollStats`.
    PollStats,
    /// `GetDialogRevenueStatsQuery` (cpp:290-330) —
    /// `payments.getStarsRevenueStats` (ton).
    RevenueStats,
    /// `GetTonRevenueWithdrawalUrlQuery` (cpp:332-365) —
    /// `payments.getStarsRevenueWithdrawalUrl` (ton).
    RevenueWithdrawalUrl,
    /// `GetTonRevenueTransactionsQuery` (cpp:367-507) —
    /// `payments.getStarsTransactions` (ton).
    RevenueTransactions,
    /// `GetMessagePublicForwardsQuery` (cpp:663-698) —
    /// `stats.getMessagePublicForwards`.
    MessagePublicForwards,
    /// `GetStoryPublicForwardsQuery` (cpp:700-737) —
    /// `stats.getStoryPublicForwards`.
    StoryPublicForwards,
    /// `LoadAsyncGraphQuery` (cpp:632-661) — `stats.loadAsyncGraph` (the
    /// `getStatisticalGraph` rider).
    LoadAsyncGraph,
}

/// The parked ResultHandler members of the in-flight statistics net
/// queries (StatisticsManager.cpp:180-737) — FIFO-paired per family (§5).
#[derive(Default)]
pub struct PendingStatisticsQueries {
    /// `GetBroadcastStatsQuery`/`GetMegagroupStatsQuery`: the
    /// `channel_id_` handler member, the send-leg megagroup branch and
    /// the `ChatStatistics` promise.
    pub channel_stats: Vec<(ChannelId, bool, Promise<td_api::ChatStatistics>)>,
    /// `GetMessageStatsQuery`: the `channel_id_` member + the promise.
    pub message_stats: Vec<(ChannelId, Promise<td_api::MessageStatistics>)>,
    /// `GetStoryStatsQuery`: the `channel_id_` member + the promise.
    pub story_stats: Vec<(ChannelId, Promise<td_api::StoryStatistics>)>,
    /// `GetPollStatsQuery`: the `dialog_id_` member + the promise.
    pub poll_stats: Vec<(DialogId, Promise<td_api::PollVoteStatistics>)>,
    /// `GetDialogRevenueStatsQuery`: the `dialog_id_` member + the
    /// promise.
    pub revenue_stats: Vec<(DialogId, Promise<td_api::ChatRevenueStatistics>)>,
    /// `GetTonRevenueWithdrawalUrlQuery`: the `dialog_id_` member + the
    /// string promise.
    pub revenue_withdrawal_urls: Vec<(DialogId, Promise<String>)>,
    /// `GetTonRevenueTransactionsQuery`: the `dialog_id_` member + the
    /// promise.
    pub revenue_transactions: Vec<(DialogId, Promise<td_api::ChatRevenueTransactions>)>,
    /// `GetMessagePublicForwardsQuery`: the `dialog_id_` member + the
    /// promise.
    pub message_public_forwards: Vec<(DialogId, Promise<td_api::PublicForwards>)>,
    /// `GetStoryPublicForwardsQuery`: the `dialog_id_` member + the
    /// promise.
    pub story_public_forwards: Vec<(DialogId, Promise<td_api::PublicForwards>)>,
    /// `LoadAsyncGraphQuery::promise_`.
    pub load_async_graph: Vec<Promise<td_api::StatisticalGraph>>,
}

impl PendingStatisticsQueries {
    /// Pop the head of a pending lot — the FIFO §5 pairing (the
    /// `take_pending_head` shape; `Vec::pop` would pair answers with the
    /// newest lot).
    pub(crate) fn take_pending_head<T>(lot: &mut Vec<T>) -> Option<T> {
        if lot.is_empty() {
            None
        } else {
            Some(lot.remove(0))
        }
    }
}

/// The dc-parked statistics requests — the `get_channel_statistics_dc_id`
/// chain windows (the `dc_id_promise` lambda captures of
/// StatisticsManager.cpp:746-758, 828-840, 858-870, 920-936, 955-979).
/// The refire answer drains them through
/// `Td::finish_pending_channel_statistics_dc_id`.
pub enum PendingStatisticsDcRequest {
    /// `get_channel_statistics` (cpp:746-758) — for_full_statistics =
    /// true.
    ChannelStats {
        dialog_id: DialogId,
        is_dark: bool,
        promise: Promise<td_api::ChatStatistics>,
    },
    /// `get_channel_message_statistics` (cpp:828-840).
    MessageStats {
        message_full_id: MessageFullId,
        is_dark: bool,
        promise: Promise<td_api::MessageStatistics>,
    },
    /// `get_channel_story_statistics` (cpp:858-870).
    StoryStats {
        story_full_id: StoryFullId,
        is_dark: bool,
        promise: Promise<td_api::StoryStatistics>,
    },
    /// `get_message_public_forwards` (cpp:920-936).
    MessageForwards {
        message_full_id: MessageFullId,
        offset: String,
        limit: i32,
        promise: Promise<td_api::PublicForwards>,
    },
    /// `get_story_public_forwards` (cpp:955-978).
    StoryForwards {
        story_full_id: StoryFullId,
        offset: String,
        limit: i32,
        promise: Promise<td_api::PublicForwards>,
    },
    /// `load_statistics_graph` (cpp:900-912).
    Graph {
        dialog_id: DialogId,
        token: String,
        x: i64,
        promise: Promise<td_api::StatisticalGraph>,
    },
}

impl PendingStatisticsDcRequest {
    /// The dialog the request waits on (the drain pairs by it).
    fn dialog_id(&self) -> DialogId {
        match self {
            PendingStatisticsDcRequest::ChannelStats { dialog_id, .. } => *dialog_id,
            PendingStatisticsDcRequest::MessageStats {
                message_full_id, ..
            } => message_full_id.get_dialog_id(),
            PendingStatisticsDcRequest::StoryStats { story_full_id, .. } => {
                story_full_id.get_dialog_id()
            }
            PendingStatisticsDcRequest::MessageForwards {
                message_full_id, ..
            } => message_full_id.get_dialog_id(),
            PendingStatisticsDcRequest::StoryForwards { story_full_id, .. } => {
                story_full_id.get_dialog_id()
            }
            PendingStatisticsDcRequest::Graph { dialog_id, .. } => *dialog_id,
        }
    }

    /// The chain's `for_full_statistics` flag (the impl result the
    /// request waits on).
    fn for_full_statistics(&self) -> bool {
        matches!(self, PendingStatisticsDcRequest::ChannelStats { .. })
    }

    /// The dc_id_promise lambda tail (the error half) — the request
    /// fails with the chain status.
    fn fail(self, status: Status) {
        match self {
            PendingStatisticsDcRequest::ChannelStats { mut promise, .. } => {
                promise.set_error(status)
            }
            PendingStatisticsDcRequest::MessageStats { mut promise, .. } => {
                promise.set_error(status)
            }
            PendingStatisticsDcRequest::StoryStats { mut promise, .. } => promise.set_error(status),
            PendingStatisticsDcRequest::MessageForwards { mut promise, .. } => {
                promise.set_error(status)
            }
            PendingStatisticsDcRequest::StoryForwards { mut promise, .. } => {
                promise.set_error(status)
            }
            PendingStatisticsDcRequest::Graph { mut promise, .. } => promise.set_error(status),
        }
    }

    /// The dc_id_promise lambda tail (the ok half) — the send leg with
    /// the resolved dc.
    fn run_send_leg(
        self,
        manager: &mut StatisticsManager,
        env: &mut impl StatisticsManagerEnv,
        dc_id: DcId,
    ) {
        match self {
            PendingStatisticsDcRequest::ChannelStats {
                dialog_id,
                is_dark,
                promise,
            } => manager.send_get_channel_stats_query(
                env,
                dc_id,
                dialog_id.get_channel_id(),
                is_dark,
                promise,
            ),
            PendingStatisticsDcRequest::MessageStats {
                message_full_id,
                is_dark,
                promise,
            } => manager.send_get_channel_message_stats_query(
                env,
                dc_id,
                message_full_id,
                is_dark,
                promise,
            ),
            PendingStatisticsDcRequest::StoryStats {
                story_full_id,
                is_dark,
                promise,
            } => manager.send_get_channel_story_stats_query(
                env,
                dc_id,
                story_full_id,
                is_dark,
                promise,
            ),
            PendingStatisticsDcRequest::MessageForwards {
                message_full_id,
                offset,
                limit,
                promise,
            } => manager.send_get_message_public_forwards_query(
                env,
                dc_id,
                message_full_id,
                &offset,
                limit,
                promise,
            ),
            PendingStatisticsDcRequest::StoryForwards {
                story_full_id,
                offset,
                limit,
                promise,
            } => manager.send_get_story_public_forwards_query(
                env,
                dc_id,
                story_full_id,
                &offset,
                limit,
                promise,
            ),
            PendingStatisticsDcRequest::Graph {
                token, x, promise, ..
            } => manager.send_load_async_graph_query(env, dc_id, &token, x, promise),
        }
    }
}

/// The `G()`/`td_` surface of the manager. The query seams of the
/// interim shape are gone — the net-drain 38 flows run inside the
/// manager over these env seams (the whole-`Td` `StatisticsEnv`
/// implements them over the real managers; the defaults keep the
/// net-inert posture for the small test envs).
pub trait StatisticsManagerEnv {
    fn close_flag(&self) -> bool;

    fn send_update(&mut self, update: td_api::Update);

    /// `G()->close_status()`.
    fn close_status(&self) -> Status {
        if self.close_flag() {
            Status::error(500, "Request aborted")
        } else {
            Status::ok()
        }
    }

    // ---- the send-leg seams ----

    /// `ChatManager::get_channel_statistics_dc_id` (ChatManager.cpp:
    /// 3846-3876) collapsed to the sync/async split — `Resolved` is the
    /// exact cached dc; `Refired` parks the request (the refire rides
    /// the GetFullChannel family; the answer leg drains the parked
    /// request).
    fn statistics_dc_id(
        &mut self,
        _dialog_id: DialogId,
        _for_full_statistics: bool,
    ) -> Result<StatisticsDc> {
        let _ = (_dialog_id, _for_full_statistics);
        tdutils::log_info!("StatisticsManager dc chain — [net-inert]");
        Result::Ok(StatisticsDc::Refired)
    }

    /// `ChatManager::is_megagroup_channel` — the send-leg branch of the
    /// channel-stats family (cpp:763).
    fn is_megagroup_channel(&self, _channel_id: ChannelId) -> bool {
        false
    }

    /// `ChatManager::get_input_channel`.
    fn get_input_channel(&mut self, _channel_id: ChannelId) -> Option<telegram_api::InputChannel> {
        None
    }

    /// `DialogManager::get_input_peer`.
    fn get_input_peer(
        &mut self,
        _dialog_id: DialogId,
        _access_rights: AccessRights,
    ) -> Option<telegram_api::InputPeer> {
        None
    }

    /// `DialogManager::check_dialog_access`.
    fn check_dialog_access(
        &mut self,
        _dialog_id: DialogId,
        _allow_secret_chats: bool,
        _access_rights: AccessRights,
        _source: &'static str,
    ) -> Result<()> {
        Result::Ok(())
    }

    /// `MessagesManager::have_message_force`.
    fn have_message_force(
        &mut self,
        _message_full_id: MessageFullId,
        _source: &'static str,
    ) -> bool {
        false
    }

    /// `MessagesManager::can_get_message_statistics`.
    fn can_get_message_statistics(&mut self, _message_full_id: MessageFullId) -> bool {
        false
    }

    /// `StoryManager::have_story_force`.
    fn have_story_force(&mut self, _story_full_id: StoryFullId) -> bool {
        false
    }

    /// `StoryManager::can_get_story_statistics` (StoryManager.cpp:
    /// 2498-2508).
    fn can_get_story_statistics(&mut self, _story_full_id: StoryFullId) -> bool {
        false
    }

    /// `DialogManager::get_my_dialog_id`.
    fn get_my_dialog_id(&self) -> DialogId {
        DialogId::default()
    }

    /// `PasswordManager::get_input_check_password_srp` — [blocked:
    /// PasswordManager]; the SRP promise parks with the seam (the star
    /// manager shape).
    fn get_input_check_password_srp(
        &mut self,
        password: String,
        promise: Promise<telegram_api::InputCheckPasswordSRP>,
    ) {
        let _ = (password, promise);
        tdutils::log_info!(
            "PasswordManager::get_input_check_password_srp — [blocked: PasswordManager]"
        );
    }

    // ---- the answer-leg intakes ----

    /// `ChatManager::on_get_channel_error`.
    fn on_get_channel_error(
        &mut self,
        _channel_id: ChannelId,
        _status: &Status,
        _source: &'static str,
    ) {
    }

    /// `DialogManager::on_get_dialog_error`.
    fn on_get_dialog_error(
        &mut self,
        _dialog_id: DialogId,
        _status: &Status,
        _source: &'static str,
    ) {
    }

    /// `UserManager::on_get_users`.
    fn on_get_users(&mut self, _users: Vec<telegram_api::User>, _source: &'static str) {}

    /// `ChatManager::on_get_chats`.
    fn on_get_chats(&mut self, _chats: Vec<telegram_api::Chat>, _source: &'static str) {}

    /// `UserManager::get_user_id_object` — the raw id in, the td_api id
    /// out.
    fn get_user_id_object(&mut self, _user_id: i64, _source: &'static str) -> i64 {
        0
    }

    /// The public-forward message leg —
    /// `MessagesManager::on_get_message` + `get_message_object`
    /// (StatisticsManager.cpp:1012-1017); `None` models the C++ invalid
    /// `MessageFullId` (the total_count decrement).
    fn get_public_forward_message(
        &mut self,
        _message: telegram_api::Message,
    ) -> Option<td_api::Message> {
        None
    }

    /// The public-forward story leg —
    /// `StoryManager::on_get_story` + `have_story` + `get_story_object`
    /// (StatisticsManager.cpp:1026-1030); `None` models the C++ invalid
    /// `StoryId` (the total_count decrement).
    fn get_public_forward_story(
        &mut self,
        _dialog_id: DialogId,
        _story: Box<telegram_api::StoryItem>,
    ) -> Option<td_api::Story> {
        None
    }

    /// `MessagesManager::on_update_message_interaction_info`
    /// (MessagesManager.cpp:4803-4813) — the broadcast answer's
    /// recent-interactions walk; the update plane is the M.10 residue
    /// (the recorded leg).
    fn on_update_message_interaction_info(
        &mut self,
        dialog_id: DialogId,
        message_id: i64,
        view_count: i32,
        forward_count: i32,
    ) {
        let _ = (dialog_id, message_id, view_count, forward_count);
        tdutils::log_info!("MessagesManager::on_update_message_interaction_info — [blocked: M.10]");
    }
}

/// The collapsed `StatisticsManager` — the net-drain 38 router: the
/// flows run the dc chains/gates at the send legs, the promises park in
/// the FIFO [`PendingStatisticsQueries`] lots (the C++ ResultHandler
/// objects) and the answers drain through `Td::on_statistics_net_answer`.
#[derive(Default)]
pub struct StatisticsManager {
    /// The manager-held dispatch surface (the `GroupCallManager::net`
    /// shape) — `None` keeps the net-inert posture (the pre-init
    /// windows and the net-inert tests).
    pub(crate) net: Option<StatisticsNetHandle>,
    /// The wire lots (FIFO per family, §5).
    pub pending: PendingStatisticsQueries,
    /// The dc-parked requests (the chain refire windows).
    pub dc_parked: Vec<PendingStatisticsDcRequest>,
}

/// The cloned statistics dispatch surface the manager senders hold — the
/// answers drain through `Td::on_statistics_net_answer`.
pub(crate) type StatisticsNetHandle = crate::td::NetKindHandle<StatisticsNetKind>;

impl StatisticsManager {
    pub fn new() -> StatisticsManager {
        StatisticsManager::default()
    }

    /// The per-DC (or main-DC) wire dispatch — the `[net-inert]` log in
    /// the None arm (the group-call convention).
    fn send_net_query(
        &self,
        kind: StatisticsNetKind,
        function: telegram_api::Function,
        dc_id: Option<DcId>,
        what: &str,
    ) {
        match &self.net {
            Some(net) => match dc_id {
                Some(dc_id) => net.dispatch_dc(kind, function, dc_id),
                None => net.dispatch(kind, function),
            },
            None => tdutils::log_info!("Send {what} — [net-inert]"),
        }
    }

    /// The send-leg failure band that reenters the C++ `on_error` tail
    /// (the handler-promise bands with the dialog-error head) through
    /// the completed queue.
    fn complete_send_error(&self, kind: StatisticsNetKind, code: i32, message: &str, what: &str) {
        match &self.net {
            Some(net) => net.complete_error(kind, code, message),
            None => tdutils::log_info!("Send {what} — [net-inert]"),
        }
    }

    /// `get_channel_statistics` (cpp:746-758) — the dc chain over the
    /// real ChatManager flow; the lambda tail is the send leg.
    pub fn get_channel_statistics(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        is_dark: bool,
        mut promise: Promise<td_api::ChatStatistics>,
    ) {
        match env.statistics_dc_id(dialog_id, true) {
            Result::Ok(StatisticsDc::Resolved(dc_id)) => self.send_get_channel_stats_query(
                env,
                dc_id,
                dialog_id.get_channel_id(),
                is_dark,
                promise,
            ),
            Result::Ok(StatisticsDc::Refired) => {
                self.dc_parked
                    .push(PendingStatisticsDcRequest::ChannelStats {
                        dialog_id,
                        is_dark,
                        promise,
                    });
            }
            Result::Err(status) => promise.set_error(status),
        }
    }

    /// `send_get_channel_stats_query` (cpp:759-768) — the close gate and
    /// the megagroup branch; the C++ handler CHECKs the input channel
    /// (cpp:193/227), the collapse answers the sibling handler's 400
    /// band (cpp:530).
    pub(crate) fn send_get_channel_stats_query(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dc_id: DcId,
        channel_id: ChannelId,
        is_dark: bool,
        mut promise: Promise<td_api::ChatStatistics>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        let is_megagroup = env.is_megagroup_channel(channel_id);
        let Some(input_channel) = env.get_input_channel(channel_id) else {
            promise.set_error(Status::error(400, "Supergroup not found"));
            return;
        };
        self.pending
            .channel_stats
            .push((channel_id, is_megagroup, promise));
        let function = if is_megagroup {
            telegram_api::Function::stats_getMegagroupStats(telegram_api::stats_getMegagroupStats {
                flags: if is_dark { 1 } else { 0 },
                dark: is_dark,
                channel: Some(input_channel),
            })
        } else {
            telegram_api::Function::stats_getBroadcastStats(telegram_api::stats_getBroadcastStats {
                flags: if is_dark { 1 } else { 0 },
                dark: is_dark,
                channel: Some(input_channel),
            })
        };
        self.send_net_query(
            StatisticsNetKind::ChannelStats,
            function,
            Some(dc_id),
            "stats.getBroadcastStats",
        );
    }

    /// `get_channel_message_statistics` (cpp:828-840).
    pub fn get_channel_message_statistics(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        message_full_id: MessageFullId,
        is_dark: bool,
        mut promise: Promise<td_api::MessageStatistics>,
    ) {
        match env.statistics_dc_id(message_full_id.get_dialog_id(), false) {
            Result::Ok(StatisticsDc::Resolved(dc_id)) => self.send_get_channel_message_stats_query(
                env,
                dc_id,
                message_full_id,
                is_dark,
                promise,
            ),
            Result::Ok(StatisticsDc::Refired) => {
                self.dc_parked
                    .push(PendingStatisticsDcRequest::MessageStats {
                        message_full_id,
                        is_dark,
                        promise,
                    });
            }
            Result::Err(status) => promise.set_error(status),
        }
    }

    /// `send_get_channel_message_stats_query` (cpp:841-856).
    pub(crate) fn send_get_channel_message_stats_query(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dc_id: DcId,
        message_full_id: MessageFullId,
        is_dark: bool,
        mut promise: Promise<td_api::MessageStatistics>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        if !env.have_message_force(message_full_id, "send_get_channel_message_stats_query") {
            return promise.set_error(Status::error(400, "Message not found"));
        }
        if !env.can_get_message_statistics(message_full_id) {
            return promise.set_error(Status::error(400, "Message statistics are inaccessible"));
        }
        let channel_id = message_full_id.get_dialog_id().get_channel_id();
        // cpp:528-531 — the handler's explicit band, no on_error tail
        let Some(input_channel) = env.get_input_channel(channel_id) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        self.pending.message_stats.push((channel_id, promise));
        let function =
            telegram_api::Function::stats_getMessageStats(telegram_api::stats_getMessageStats {
                flags: if is_dark { 1 } else { 0 },
                dark: is_dark,
                channel: Some(input_channel),
                msg_id: message_full_id
                    .get_message_id()
                    .get_server_message_id()
                    .get(),
            });
        self.send_net_query(
            StatisticsNetKind::MessageStats,
            function,
            Some(dc_id),
            "stats.getMessageStats",
        );
    }

    /// `get_channel_story_statistics` (cpp:858-870).
    pub fn get_channel_story_statistics(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        story_full_id: StoryFullId,
        is_dark: bool,
        mut promise: Promise<td_api::StoryStatistics>,
    ) {
        match env.statistics_dc_id(story_full_id.get_dialog_id(), false) {
            Result::Ok(StatisticsDc::Resolved(dc_id)) => {
                self.send_get_channel_story_stats_query(env, dc_id, story_full_id, is_dark, promise)
            }
            Result::Ok(StatisticsDc::Refired) => {
                self.dc_parked.push(PendingStatisticsDcRequest::StoryStats {
                    story_full_id,
                    is_dark,
                    promise,
                });
            }
            Result::Err(status) => promise.set_error(status),
        }
    }

    /// `send_get_channel_story_stats_query` (cpp:871-886).
    pub(crate) fn send_get_channel_story_stats_query(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dc_id: DcId,
        story_full_id: StoryFullId,
        is_dark: bool,
        mut promise: Promise<td_api::StoryStatistics>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        if !env.have_story_force(story_full_id) {
            return promise.set_error(Status::error(400, "Story not found"));
        }
        if !env.can_get_story_statistics(story_full_id) {
            return promise.set_error(Status::error(400, "Story statistics are inaccessible"));
        }
        let dialog_id = story_full_id.get_dialog_id();
        // cpp:572-575 — the handler's explicit band, no on_error tail
        let Some(input_peer) = env.get_input_peer(dialog_id, AccessRights::Read) else {
            return promise.set_error(Status::error(400, "Chat not found"));
        };
        self.pending
            .story_stats
            .push((dialog_id.get_channel_id(), promise));
        let function =
            telegram_api::Function::stats_getStoryStats(telegram_api::stats_getStoryStats {
                flags: if is_dark { 1 } else { 0 },
                dark: is_dark,
                peer: Some(Box::new(input_peer)),
                id: story_full_id.get_story_id().get(),
            });
        self.send_net_query(
            StatisticsNetKind::StoryStats,
            function,
            Some(dc_id),
            "stats.getStoryStats",
        );
    }

    /// `get_poll_statistics` (cpp:888-898) — the messages-manager gate
    /// (`have_message_force` → "Message not found",
    /// `can_get_message_poll_vote_statistics` → "Poll statistics are
    /// inaccessible") runs in the dispatcher arm over the
    /// MessagesManager.
    pub fn get_poll_statistics(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        message_full_id: MessageFullId,
        is_dark: bool,
        mut promise: Promise<td_api::PollVoteStatistics>,
    ) {
        let dialog_id = message_full_id.get_dialog_id();
        // cpp:608-609 — the handler CHECK; the arm gates guaranteed the
        // dialog, the collapse answers the sibling peer band (cpp:574)
        let Some(input_peer) = env.get_input_peer(dialog_id, AccessRights::Read) else {
            return promise.set_error(Status::error(400, "Chat not found"));
        };
        self.pending.poll_stats.push((dialog_id, promise));
        let function =
            telegram_api::Function::stats_getPollStats(telegram_api::stats_getPollStats {
                flags: if is_dark { 1 } else { 0 },
                dark: is_dark,
                peer: Some(Box::new(input_peer)),
                msg_id: message_full_id
                    .get_message_id()
                    .get_server_message_id()
                    .get(),
            });
        self.send_net_query(
            StatisticsNetKind::PollStats,
            function,
            None,
            "messages.getPollVotes",
        );
    }

    /// `get_dialog_revenue_statistics` (cpp:770-775).
    pub fn get_dialog_revenue_statistics(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        is_dark: bool,
        mut promise: Promise<td_api::ChatRevenueStatistics>,
    ) {
        if let Result::Err(status) = env.check_dialog_access(
            dialog_id,
            false,
            AccessRights::Read,
            "get_dialog_revenue_statistics",
        ) {
            return promise.set_error(status);
        }
        // cpp:302-303 — the handler CHECK; the collapse answers the
        // sibling peer band (cpp:574)
        let Some(input_peer) = env.get_input_peer(dialog_id, AccessRights::Read) else {
            return promise.set_error(Status::error(400, "Chat not found"));
        };
        self.pending.revenue_stats.push((dialog_id, promise));
        let function = telegram_api::Function::payments_getStarsRevenueStats(
            telegram_api::payments_getStarsRevenueStats {
                flags: if is_dark { 1 } else { 0 },
                dark: is_dark,
                ton: true,
                peer: Some(Box::new(input_peer)),
            },
        );
        self.send_net_query(
            StatisticsNetKind::RevenueStats,
            function,
            None,
            "paystars.getBroadcastRevenueStats",
        );
    }

    /// `get_dialog_revenue_withdrawal_url` (cpp:793-810) — the SRP leg
    /// is the PasswordManager seam (the star-manager collapse: the wire
    /// query rides the empty-SRP placeholder).
    pub fn get_dialog_revenue_withdrawal_url(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        password: &str,
        mut promise: Promise<String>,
    ) {
        if let Result::Err(status) = env.check_dialog_access(
            dialog_id,
            false,
            AccessRights::Write,
            "get_dialog_revenue_withdrawal_url",
        ) {
            return promise.set_error(status);
        }
        if password.is_empty() {
            return promise.set_error(Status::error(400, "PASSWORD_HASH_INVALID"));
        }
        env.get_input_check_password_srp(password.to_owned(), Promise::default());
        self.send_get_dialog_revenue_withdrawal_url_query(env, dialog_id, promise);
    }

    /// `send_get_dialog_revenue_withdrawal_url_query` (cpp:812-818).
    pub(crate) fn send_get_dialog_revenue_withdrawal_url_query(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        mut promise: Promise<String>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        let input_peer = env.get_input_peer(dialog_id, AccessRights::Write);
        self.pending
            .revenue_withdrawal_urls
            .push((dialog_id, promise));
        let Some(input_peer) = input_peer else {
            // cpp:344-346 — the on_error band: the tail runs through the
            // completed queue
            self.complete_send_error(
                StatisticsNetKind::RevenueWithdrawalUrl,
                400,
                "Have no access to the chat",
                "paystars.getBroadcastRevenueWithdrawalUrl",
            );
            return;
        };
        let function = telegram_api::Function::payments_getStarsRevenueWithdrawalUrl(
            telegram_api::payments_getStarsRevenueWithdrawalUrl {
                flags: 1,
                ton: true,
                peer: Some(Box::new(input_peer)),
                amount: 0,
                password: Some(
                    telegram_api::InputCheckPasswordSRP::inputCheckPasswordEmpty(
                        telegram_api::inputCheckPasswordEmpty {},
                    ),
                ),
            },
        );
        self.send_net_query(
            StatisticsNetKind::RevenueWithdrawalUrl,
            function,
            None,
            "paystars.getBroadcastRevenueWithdrawalUrl",
        );
    }

    /// `get_dialog_revenue_transactions` (cpp:820-826).
    pub fn get_dialog_revenue_transactions(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        offset: &str,
        limit: i32,
        mut promise: Promise<td_api::ChatRevenueTransactions>,
    ) {
        if let Result::Err(status) = env.check_dialog_access(
            dialog_id,
            false,
            AccessRights::Read,
            "get_dialog_revenue_transactions",
        ) {
            return promise.set_error(status);
        }
        // cpp:379-380 — the handler CHECK; the collapse answers the
        // sibling peer band (cpp:574)
        let Some(input_peer) = env.get_input_peer(dialog_id, AccessRights::Read) else {
            return promise.set_error(Status::error(400, "Chat not found"));
        };
        self.pending.revenue_transactions.push((dialog_id, promise));
        let function = telegram_api::Function::payments_getStarsTransactions(
            telegram_api::payments_getStarsTransactions {
                flags: 1 << 4,
                inbound: false,
                outbound: false,
                ascending: false,
                ton: true,
                subscription_id: String::new(),
                peer: Some(Box::new(input_peer)),
                offset: offset.to_owned(),
                limit,
            },
        );
        self.send_net_query(
            StatisticsNetKind::RevenueTransactions,
            function,
            None,
            "paystars.getBroadcastRevenueTransactions",
        );
    }

    /// `get_message_public_forwards` (cpp:920-936).
    pub fn get_message_public_forwards(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        message_full_id: MessageFullId,
        offset: &str,
        limit: i32,
        mut promise: Promise<td_api::PublicForwards>,
    ) {
        if limit <= 0 {
            return promise.set_error(Status::error(400, "Parameter limit must be positive"));
        }
        match env.statistics_dc_id(message_full_id.get_dialog_id(), false) {
            Result::Ok(StatisticsDc::Resolved(dc_id)) => self
                .send_get_message_public_forwards_query(
                    env,
                    dc_id,
                    message_full_id,
                    offset,
                    limit,
                    promise,
                ),
            Result::Ok(StatisticsDc::Refired) => {
                self.dc_parked
                    .push(PendingStatisticsDcRequest::MessageForwards {
                        message_full_id,
                        offset: offset.to_owned(),
                        limit,
                        promise,
                    });
            }
            Result::Err(status) => promise.set_error(status),
        }
    }

    /// `send_get_message_public_forwards_query` (cpp:937-953).
    pub(crate) fn send_get_message_public_forwards_query(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dc_id: DcId,
        message_full_id: MessageFullId,
        offset: &str,
        mut limit: i32,
        mut promise: Promise<td_api::PublicForwards>,
    ) {
        if !env.have_message_force(message_full_id, "send_get_message_public_forwards_query") {
            return promise.set_error(Status::error(400, "Message not found"));
        }
        if !env.can_get_message_statistics(message_full_id) {
            return promise.set_error(Status::error(400, "Message forwards are inaccessible"));
        }
        // the server-side clamp (cpp:947-950)
        limit = limit.min(100);
        let dialog_id = message_full_id.get_dialog_id();
        // cpp:675-676 — the handler CHECK; the collapse answers the
        // sibling handler's 400 band (cpp:530)
        let Some(input_channel) = env.get_input_channel(dialog_id.get_channel_id()) else {
            return promise.set_error(Status::error(400, "Supergroup not found"));
        };
        self.pending
            .message_public_forwards
            .push((dialog_id, promise));
        let function = telegram_api::Function::stats_getMessagePublicForwards(
            telegram_api::stats_getMessagePublicForwards {
                channel: Some(input_channel),
                msg_id: message_full_id
                    .get_message_id()
                    .get_server_message_id()
                    .get(),
                offset: offset.to_owned(),
                limit,
            },
        );
        self.send_net_query(
            StatisticsNetKind::MessagePublicForwards,
            function,
            Some(dc_id),
            "stats.getMessagePublicForwards",
        );
    }

    /// `get_story_public_forwards` (cpp:955-978) — the User-dialog arm
    /// sends directly over the main dc (cpp:961-967).
    pub fn get_story_public_forwards(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        story_full_id: StoryFullId,
        offset: &str,
        limit: i32,
        mut promise: Promise<td_api::PublicForwards>,
    ) {
        if limit <= 0 {
            return promise.set_error(Status::error(400, "Parameter limit must be positive"));
        }
        let dialog_id = story_full_id.get_dialog_id();
        if dialog_id.get_type() == crate::dialog_id::DialogType::User {
            if dialog_id != env.get_my_dialog_id() {
                return promise.set_error(Status::error(400, "Have no access to story statistics"));
            }
            return self.send_get_story_public_forwards_query(
                env,
                DcId::main(),
                story_full_id,
                offset,
                limit,
                promise,
            );
        }
        match env.statistics_dc_id(dialog_id, false) {
            Result::Ok(StatisticsDc::Resolved(dc_id)) => self.send_get_story_public_forwards_query(
                env,
                dc_id,
                story_full_id,
                offset,
                limit,
                promise,
            ),
            Result::Ok(StatisticsDc::Refired) => {
                self.dc_parked
                    .push(PendingStatisticsDcRequest::StoryForwards {
                        story_full_id,
                        offset: offset.to_owned(),
                        limit,
                        promise,
                    });
            }
            Result::Err(status) => promise.set_error(status),
        }
    }

    /// `send_get_story_public_forwards_query` (cpp:980-996).
    pub(crate) fn send_get_story_public_forwards_query(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dc_id: DcId,
        story_full_id: StoryFullId,
        offset: &str,
        mut limit: i32,
        mut promise: Promise<td_api::PublicForwards>,
    ) {
        if !env.have_story_force(story_full_id) {
            return promise.set_error(Status::error(400, "Story not found"));
        }
        let dialog_id = story_full_id.get_dialog_id();
        if !env.can_get_story_statistics(story_full_id) && dialog_id != env.get_my_dialog_id() {
            return promise.set_error(Status::error(400, "Story forwards are inaccessible"));
        }
        // the server-side clamp (cpp:991-994)
        limit = limit.min(100);
        let input_peer = env.get_input_peer(dialog_id, AccessRights::Read);
        self.pending
            .story_public_forwards
            .push((dialog_id, promise));
        let Some(input_peer) = input_peer else {
            // cpp:713-715 — the on_error band: the tail runs through the
            // completed queue
            self.complete_send_error(
                StatisticsNetKind::StoryPublicForwards,
                400,
                "Can't get story statistics",
                "stats.getStoryPublicForwards",
            );
            return;
        };
        let function = telegram_api::Function::stats_getStoryPublicForwards(
            telegram_api::stats_getStoryPublicForwards {
                peer: Some(Box::new(input_peer)),
                id: story_full_id.get_story_id().get(),
                offset: offset.to_owned(),
                limit,
            },
        );
        self.send_net_query(
            StatisticsNetKind::StoryPublicForwards,
            function,
            Some(dc_id),
            "stats.getStoryPublicForwards",
        );
    }

    /// The dc-parked drain half — the refire answer runs each parked
    /// request's lambda tail with its own chain result (the
    /// per-continuation impls of ChatManager.cpp:3879-3886).
    pub(crate) fn run_parked_dc_requests(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        results: &[(bool, Result<DcId>)],
    ) {
        let mut index = 0;
        while index < self.dc_parked.len() {
            if self.dc_parked[index].dialog_id() == dialog_id {
                let request = self.dc_parked.remove(index);
                let result = results
                    .iter()
                    .find(|(flag, _)| *flag == request.for_full_statistics())
                    .map(|(_, result)| result.clone());
                match result {
                    Some(Result::Ok(dc_id)) => request.run_send_leg(self, env, dc_id),
                    Some(Result::Err(status)) => request.fail(status),
                    // the drain computed both flags; unreachable
                    None => {}
                }
            } else {
                index += 1;
            }
        }
    }

    /// `on_update_dialog_revenue_transactions` (cpp:777-792) — the
    /// revenue-status intake; the C++ emits
    /// `updateChatRevenueAmount` (the update builder joins the revenue
    /// answer converters).
    pub fn on_update_dialog_revenue_transactions(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        status: &telegram_api::StarsRevenueStatus,
    ) {
        let stars = |amount: &Option<telegram_api::StarsAmount>| {
            amount.as_ref().map_or(0, |amount| match amount {
                telegram_api::StarsAmount::starsAmount(amount) => amount.amount,
                telegram_api::StarsAmount::starsTonAmount(amount) => amount.amount,
            })
        };
        let update = td_api::Update::updateChatRevenueAmount(td_api::updateChatRevenueAmount {
            chat_id: dialog_id.get(),
            revenue_amount: Some(td_api::ChatRevenueAmount {
                cryptocurrency: String::new(),
                total_amount: stars(&status.overall_revenue),
                balance_amount: stars(&status.current_balance),
                available_amount: stars(&status.available_balance),
                withdrawal_enabled: status.withdrawal_enabled,
            }),
        });
        env.send_update(update);
    }

    /// `on_get_public_forwards` (cpp:999-1047) — the publicForwards
    /// conversion over the message/story intake seams; the dropped
    /// forwards decrement the total (cpp:1020/1033).
    pub(crate) fn on_get_public_forwards(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        public_forwards: telegram_api::stats_PublicForwards,
        mut promise: Promise<td_api::PublicForwards>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        let mut total_count = public_forwards.count;
        tdutils::log_info!(
            "Receive {} forwarded stories out of {}",
            public_forwards.forwards.len(),
            public_forwards.count
        );
        let mut result = Vec::new();
        for forward in public_forwards.forwards {
            match forward {
                telegram_api::PublicForward::publicForwardMessage(forward) => {
                    let Some(message) = forward.message else {
                        total_count -= 1;
                        continue;
                    };
                    match env.get_public_forward_message(message) {
                        Some(object) => {
                            result.push(td_api::PublicForward::publicForwardMessage(
                                td_api::publicForwardMessage {
                                    message: Some(object),
                                },
                            ));
                        }
                        None => total_count -= 1,
                    }
                }
                telegram_api::PublicForward::publicForwardStory(forward) => {
                    let Some(peer) = forward.peer.as_ref() else {
                        total_count -= 1;
                        continue;
                    };
                    let dialog_id =
                        DialogId::new(crate::messages_manager::DialogId::from_peer(peer).get());
                    let Some(story) = forward.story else {
                        total_count -= 1;
                        continue;
                    };
                    match env.get_public_forward_story(dialog_id, story) {
                        Some(object) => {
                            result.push(td_api::PublicForward::publicForwardStory(
                                td_api::publicForwardStory {
                                    story: Some(object),
                                },
                            ));
                        }
                        None => total_count -= 1,
                    }
                }
            }
        }
        if total_count < result.len() as i32 {
            tdutils::log_error!(
                "Receive {} valid story sorwards out of {total_count}",
                result.len()
            );
            total_count = result.len() as i32;
        }
        promise.set_value(td_api::PublicForwards {
            total_count,
            forwards: result,
            next_offset: public_forwards.next_offset,
        });
    }

    /// `load_statistics_graph` (cpp:900-912) — the dc chain, then the
    /// async-graph load (the `getStatisticalGraph` rider; the arm
    /// predates the row at Requests.cpp:8279-8285).
    pub fn load_statistics_graph(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        token: &str,
        x: i64,
        mut promise: Promise<td_api::StatisticalGraph>,
    ) {
        match env.statistics_dc_id(dialog_id, false) {
            Result::Ok(StatisticsDc::Resolved(dc_id)) => {
                self.send_load_async_graph_query(env, dc_id, token, x, promise)
            }
            Result::Ok(StatisticsDc::Refired) => {
                self.dc_parked.push(PendingStatisticsDcRequest::Graph {
                    dialog_id,
                    token: token.to_owned(),
                    x,
                    promise,
                });
            }
            Result::Err(status) => promise.set_error(status),
        }
    }

    /// `send_load_async_graph_query` (cpp:914-918).
    pub(crate) fn send_load_async_graph_query(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dc_id: DcId,
        token: &str,
        x: i64,
        mut promise: Promise<td_api::StatisticalGraph>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            return promise.set_error(close_status);
        }
        // the X_MASK flag (cpp:915-918)
        let mut flags = 0;
        if x != 0 {
            flags |= 1;
        }
        self.pending.load_async_graph.push(promise);
        let function =
            telegram_api::Function::stats_loadAsyncGraph(telegram_api::stats_loadAsyncGraph {
                flags,
                token: token.to_owned(),
                x,
            });
        self.send_net_query(
            StatisticsNetKind::LoadAsyncGraph,
            function,
            Some(dc_id),
            "stats.loadAsyncGraph",
        );
    }
}

/// `convert_date_range` (StatisticsManager.cpp:35-38).
pub fn convert_date_range(
    obj: &Option<telegram_api::StatsDateRangeDays>,
) -> Option<td_api::DateRange> {
    obj.as_ref().map(|obj| td_api::DateRange {
        start_date: obj.min_date,
        end_date: obj.max_date,
    })
}

/// `convert_stats_graph` (StatisticsManager.cpp:40-60) — the
/// `StatsGraph` union → the td_api form.
pub fn convert_stats_graph(graph: &telegram_api::StatsGraph) -> td_api::StatisticalGraph {
    match graph {
        telegram_api::StatsGraph::statsGraphAsync(graph) => {
            td_api::StatisticalGraph::statisticalGraphAsync(td_api::statisticalGraphAsync {
                token: graph.token.clone(),
            })
        }
        telegram_api::StatsGraph::statsGraphError(graph) => {
            td_api::StatisticalGraph::statisticalGraphError(td_api::statisticalGraphError {
                error_message: graph.error.clone(),
            })
        }
        telegram_api::StatsGraph::statsGraph(graph) => {
            let data = graph
                .json
                .as_ref()
                .map_or(String::new(), |json| json.data.clone());
            td_api::StatisticalGraph::statisticalGraphData(td_api::statisticalGraphData {
                json_data: data,
                zoom_token: graph.zoom_token.clone(),
            })
        }
    }
}

/// The `Option`-carrying form the generated answer fields arrive in.
pub fn convert_stats_graph_opt(
    graph: &Option<telegram_api::StatsGraph>,
) -> Option<td_api::StatisticalGraph> {
    graph.as_ref().map(convert_stats_graph)
}

/// `get_percentage_value` (StatisticsManager.cpp:62-77).
pub fn get_percentage_value(part: f64, total: f64, is_percentage: bool) -> f64 {
    if total < 1e-6 && total > -1e-6 {
        if part < 1e-6 && part > -1e-6 {
            return 0.0;
        }
        return 100.0;
    }
    if part > 1e20 {
        return 100.0;
    }
    let value = part / total * 100.0;
    if is_percentage {
        value.clamp(0.0, 100.0)
    } else {
        value.max(-100.0)
    }
}

/// `convert_stats_absolute_value` (StatisticsManager.cpp:79-84).
pub fn convert_stats_absolute_value(current: f64, previous: f64) -> td_api::StatisticalValue {
    td_api::StatisticalValue {
        value: current,
        previous_value: previous,
        growth_rate_percentage: get_percentage_value(current - previous, previous, false),
    }
}

/// The `Option`-carrying form over the generated `StatsAbsValueAndPrev`.
pub fn convert_stats_abs_value(
    obj: &Option<telegram_api::StatsAbsValueAndPrev>,
) -> Option<td_api::StatisticalValue> {
    obj.as_ref()
        .map(|obj| convert_stats_absolute_value(obj.current, obj.previous))
}

/// `convert_broadcast_stats` (StatisticsManager.cpp:135-178) — the
/// channel (broadcast) statistics conversion.
pub fn convert_broadcast_stats(obj: telegram_api::stats_BroadcastStats) -> td_api::ChatStatistics {
    let recent_interactions = obj
        .recent_posts_interactions
        .into_iter()
        .map(|interaction| match interaction {
            telegram_api::PostInteractionCounters::postInteractionCountersMessage(interaction) => {
                td_api::ChatStatisticsInteractionInfo {
                    object_type: Some(
                        td_api::ChatStatisticsObjectType::chatStatisticsObjectTypeMessage(
                            td_api::chatStatisticsObjectTypeMessage {
                                message_id: crate::message_id::MessageId::from_server_message_id(
                                    crate::messages_manager::ServerMessageId::new(
                                        interaction.msg_id,
                                    ),
                                )
                                .get(),
                            },
                        ),
                    ),
                    view_count: interaction.views,
                    forward_count: interaction.forwards,
                    reaction_count: interaction.reactions,
                }
            }
            telegram_api::PostInteractionCounters::postInteractionCountersStory(interaction) => {
                td_api::ChatStatisticsInteractionInfo {
                    object_type: Some(
                        td_api::ChatStatisticsObjectType::chatStatisticsObjectTypeStory(
                            td_api::chatStatisticsObjectTypeStory {
                                story_id: crate::story_ids::StoryId::new(interaction.story_id)
                                    .get(),
                            },
                        ),
                    ),
                    view_count: interaction.views,
                    forward_count: interaction.forwards,
                    reaction_count: interaction.reactions,
                }
            }
        })
        .collect();
    let enabled_notifications = obj.enabled_notifications.as_ref().map_or(0.0, |value| {
        get_percentage_value(value.part, value.total, true)
    });
    td_api::ChatStatistics::chatStatisticsChannel(td_api::chatStatisticsChannel {
        period: convert_date_range(&obj.period),
        member_count: convert_stats_abs_value(&obj.followers),
        mean_message_view_count: convert_stats_abs_value(&obj.views_per_post),
        mean_message_share_count: convert_stats_abs_value(&obj.shares_per_post),
        mean_message_reaction_count: convert_stats_abs_value(&obj.reactions_per_post),
        mean_story_view_count: convert_stats_abs_value(&obj.views_per_story),
        mean_story_share_count: convert_stats_abs_value(&obj.shares_per_story),
        mean_story_reaction_count: convert_stats_abs_value(&obj.reactions_per_story),
        enabled_notifications_percentage: enabled_notifications,
        member_count_graph: convert_stats_graph_opt(&obj.growth_graph),
        join_graph: convert_stats_graph_opt(&obj.followers_graph),
        mute_graph: convert_stats_graph_opt(&obj.mute_graph),
        view_count_by_hour_graph: convert_stats_graph_opt(&obj.top_hours_graph),
        view_count_by_source_graph: convert_stats_graph_opt(&obj.views_by_source_graph),
        join_by_source_graph: convert_stats_graph_opt(&obj.new_followers_by_source_graph),
        language_graph: convert_stats_graph_opt(&obj.languages_graph),
        message_interaction_graph: convert_stats_graph_opt(&obj.interactions_graph),
        message_reaction_graph: convert_stats_graph_opt(&obj.reactions_by_emotion_graph),
        story_interaction_graph: convert_stats_graph_opt(&obj.story_interactions_graph),
        story_reaction_graph: convert_stats_graph_opt(&obj.story_reactions_by_emotion_graph),
        instant_view_interaction_graph: convert_stats_graph_opt(&obj.iv_interactions_graph),
        recent_interactions,
    })
}

/// `convert_megagroup_stats` (StatisticsManager.cpp:88-133) — the
/// megagroup statistics conversion over the user intake seam.
pub fn convert_megagroup_stats(
    env: &mut impl StatisticsManagerEnv,
    obj: telegram_api::stats_MegagroupStats,
) -> td_api::ChatStatistics {
    env.on_get_users(obj.users, "convert_megagroup_stats");
    let top_senders = obj
        .top_posters
        .into_iter()
        // the remove_if bands (cpp:95-97)
        .filter(|poster| {
            crate::user_manager::UserId::new(poster.user_id).is_valid()
                && poster.messages >= 0
                && poster.avg_chars >= 0
        })
        .map(|top_poster| td_api::ChatStatisticsMessageSenderInfo {
            user_id: env.get_user_id_object(top_poster.user_id, "get_top_senders"),
            sent_message_count: top_poster.messages,
            average_character_count: top_poster.avg_chars,
        })
        .collect();
    let top_administrators = obj
        .top_admins
        .into_iter()
        // the remove_if bands (cpp:98-100)
        .filter(|admin| {
            crate::user_manager::UserId::new(admin.user_id).is_valid()
                && admin.deleted >= 0
                && admin.kicked >= 0
                && admin.banned >= 0
        })
        .map(|top_admin| td_api::ChatStatisticsAdministratorActionsInfo {
            user_id: env.get_user_id_object(top_admin.user_id, "get_top_administrators"),
            deleted_message_count: top_admin.deleted,
            banned_user_count: top_admin.kicked,
            restricted_user_count: top_admin.banned,
        })
        .collect();
    let top_inviters = obj
        .top_inviters
        .into_iter()
        // the remove_if band (cpp:101-102)
        .filter(|inviter| {
            crate::user_manager::UserId::new(inviter.user_id).is_valid() && inviter.invitations >= 0
        })
        .map(|top_inviter| td_api::ChatStatisticsInviterInfo {
            user_id: env.get_user_id_object(top_inviter.user_id, "get_top_inviters"),
            added_member_count: top_inviter.invitations,
        })
        .collect();
    td_api::ChatStatistics::chatStatisticsSupergroup(td_api::chatStatisticsSupergroup {
        period: convert_date_range(&obj.period),
        member_count: convert_stats_abs_value(&obj.members),
        message_count: convert_stats_abs_value(&obj.messages),
        viewer_count: convert_stats_abs_value(&obj.viewers),
        sender_count: convert_stats_abs_value(&obj.posters),
        member_count_graph: convert_stats_graph_opt(&obj.growth_graph),
        join_graph: convert_stats_graph_opt(&obj.members_graph),
        join_by_source_graph: convert_stats_graph_opt(&obj.new_members_by_source_graph),
        language_graph: convert_stats_graph_opt(&obj.languages_graph),
        message_content_graph: convert_stats_graph_opt(&obj.messages_graph),
        action_graph: convert_stats_graph_opt(&obj.actions_graph),
        day_graph: convert_stats_graph_opt(&obj.top_hours_graph),
        week_graph: convert_stats_graph_opt(&obj.weekdays_graph),
        top_senders,
        top_administrators,
        top_inviters,
    })
}

/// `convert_message_stats` (StatisticsManager.cpp:509-514).
pub fn convert_message_stats(obj: telegram_api::stats_MessageStats) -> td_api::MessageStatistics {
    td_api::MessageStatistics {
        message_interaction_graph: convert_stats_graph_opt(&obj.views_graph),
        message_reaction_graph: convert_stats_graph_opt(&obj.reactions_by_emotion_graph),
    }
}

/// `convert_story_stats` (StatisticsManager.cpp:554-558).
pub fn convert_story_stats(obj: telegram_api::stats_StoryStats) -> td_api::StoryStatistics {
    td_api::StoryStatistics {
        story_interaction_graph: convert_stats_graph_opt(&obj.views_graph),
        story_reaction_graph: convert_stats_graph_opt(&obj.reactions_by_emotion_graph),
    }
}

/// The `convert_stars_revenue_status` TON twin (StatisticsManager.cpp:
/// 264-279) — the `starsTonAmount` band with the log-and-zero fallback.
pub(crate) fn convert_ton_revenue_status(
    obj: telegram_api::StarsRevenueStatus,
) -> td_api::ChatRevenueAmount {
    let get_amount = |amount: &Option<telegram_api::StarsAmount>| -> i64 {
        match amount {
            Some(telegram_api::StarsAmount::starsTonAmount(amount)) => {
                crate::message_content::TonAmount::from_wire(
                    Some(&telegram_api::StarsAmount::starsTonAmount(
                        telegram_api::starsTonAmount {
                            amount: amount.amount,
                        },
                    )),
                    false,
                )
                .ton_amount
            }
            other => {
                tdutils::log_error!("Receive {other:?} instead of a TON amount");
                0
            }
        }
    };
    td_api::ChatRevenueAmount {
        cryptocurrency: "TON".to_owned(),
        total_amount: get_amount(&obj.overall_revenue),
        balance_amount: get_amount(&obj.current_balance),
        available_amount: get_amount(&obj.available_balance),
        withdrawal_enabled: obj.withdrawal_enabled,
    }
}

/// `convert_ton_revenue_stats` (StatisticsManager.cpp:281-288) — the
/// caller synthesizes the null top-hours graph (cpp:317-322).
pub(crate) fn convert_ton_revenue_stats(
    mut obj: telegram_api::payments_StarsRevenueStats,
) -> td_api::ChatRevenueStatistics {
    // cpp:317-322 — the null top-hours graph synthesizes the error form
    // from the revenue graph (or the default message)
    if obj.top_hours_graph.is_none() {
        let error = match obj.revenue_graph.as_ref() {
            Some(telegram_api::StatsGraph::statsGraphError(graph)) => graph.error.clone(),
            _ => "Not enough data to display.".to_owned(),
        };
        obj.top_hours_graph = Some(telegram_api::StatsGraph::statsGraphError(
            telegram_api::statsGraphError { error },
        ));
    }
    let status = obj.status.map_or_else(
        || {
            tdutils::log_error!("Receive no stars revenue status");
            td_api::ChatRevenueAmount {
                cryptocurrency: "TON".to_owned(),
                total_amount: 0,
                balance_amount: 0,
                available_amount: 0,
                withdrawal_enabled: false,
            }
        },
        convert_ton_revenue_status,
    );
    td_api::ChatRevenueStatistics {
        revenue_by_hour_graph: convert_stats_graph_opt(&obj.top_hours_graph),
        revenue_graph: convert_stats_graph_opt(&obj.revenue_graph),
        revenue_amount: Some(status),
        usd_rate: if obj.usd_rate > 0.0 {
            (obj.usd_rate * 1e-7).clamp(1e-18, 1e18)
        } else {
            1.0
        },
    }
}

/// The `GetTonRevenueTransactionsQuery::on_result` conversion body
/// (StatisticsManager.cpp:386-500) — the users/chats intakes and the
/// balance band run at the answer router; this walks the history.
pub(crate) fn convert_ton_revenue_transactions(
    env: &mut impl StatisticsManagerEnv,
    result: telegram_api::payments_StarsStatus,
) -> td_api::ChatRevenueTransactions {
    let mut transactions = Vec::new();
    for transaction in result.history {
        let mut transaction = transaction;
        if !matches!(
            transaction.amount,
            Some(telegram_api::StarsAmount::starsTonAmount(_))
        ) {
            tdutils::log_error!("Receive {:?}", transaction.amount);
            continue;
        }
        let transaction_amount =
            crate::message_content::TonAmount::from_wire(transaction.amount.as_ref(), true);
        let is_refund = transaction.refund;
        let is_purchase = transaction_amount.is_positive() == is_refund;
        let r#type = match &transaction.peer {
            Some(telegram_api::StarsTransactionPeer::starsTransactionPeerUnsupported(_))
            | Some(telegram_api::StarsTransactionPeer::starsTransactionPeerPremiumBot(_))
            | Some(telegram_api::StarsTransactionPeer::starsTransactionPeerAppStore(_))
            | Some(telegram_api::StarsTransactionPeer::starsTransactionPeerPlayMarket(_))
            | Some(telegram_api::StarsTransactionPeer::starsTransactionPeerAPI(_)) => Some(
                td_api::ChatRevenueTransactionType::chatRevenueTransactionTypeUnsupported(
                    td_api::chatRevenueTransactionTypeUnsupported {},
                ),
            ),
            Some(telegram_api::StarsTransactionPeer::starsTransactionPeerFragment(_)) => {
                if is_refund {
                    Some(td_api::ChatRevenueTransactionType::chatRevenueTransactionTypeFragmentRefund(
                        td_api::chatRevenueTransactionTypeFragmentRefund {
                            refund_date: transaction.date,
                        },
                    ))
                } else {
                    // the SCOPE_EXIT clears ride the move (cpp:427-443)
                    let state = if transaction.transaction_date > 0 {
                        Some(
                            td_api::RevenueWithdrawalState::revenueWithdrawalStateSucceeded(
                                td_api::revenueWithdrawalStateSucceeded {
                                    date: transaction.transaction_date,
                                    url: transaction.transaction_url.clone(),
                                },
                            ),
                        )
                    } else if transaction.pending {
                        Some(
                            td_api::RevenueWithdrawalState::revenueWithdrawalStatePending(
                                td_api::revenueWithdrawalStatePending {},
                            ),
                        )
                    } else if transaction.failed {
                        Some(
                            td_api::RevenueWithdrawalState::revenueWithdrawalStateFailed(
                                td_api::revenueWithdrawalStateFailed {},
                            ),
                        )
                    } else {
                        None
                    };
                    state.map(|state| {
                        td_api::ChatRevenueTransactionType::
                            chatRevenueTransactionTypeFragmentWithdrawal(
                                td_api::chatRevenueTransactionTypeFragmentWithdrawal {
                                    withdrawal_date: transaction.date,
                                    state: Some(state),
                                },
                            )
                    })
                }
            }
            Some(telegram_api::StarsTransactionPeer::starsTransactionPeerAds(_)) => {
                if transaction.ads_proceeds_from_date > 0
                    && transaction.ads_proceeds_from_date <= transaction.ads_proceeds_to_date
                {
                    Some(td_api::ChatRevenueTransactionType::
                        chatRevenueTransactionTypeSponsoredMessageEarnings(
                            td_api::chatRevenueTransactionTypeSponsoredMessageEarnings {
                                start_date: transaction.ads_proceeds_from_date,
                                end_date: transaction.ads_proceeds_to_date,
                            },
                        ))
                } else {
                    None
                }
            }
            Some(telegram_api::StarsTransactionPeer::starsTransactionPeer(peer)) => {
                let peer_dialog_id = peer.peer.as_ref().map(|peer| {
                    DialogId::new(crate::messages_manager::DialogId::from_peer(peer).get())
                });
                match peer_dialog_id {
                    Some(peer_dialog_id)
                        if peer_dialog_id.is_valid()
                            && transaction.paid_messages != 0
                            && !is_purchase
                            && peer_dialog_id.get_type() == crate::dialog_id::DialogType::User =>
                    {
                        Some(td_api::ChatRevenueTransactionType::
                            chatRevenueTransactionTypeSuggestedPostEarnings(
                                td_api::chatRevenueTransactionTypeSuggestedPostEarnings {
                                    user_id: env.get_user_id_object(
                                        peer_dialog_id.get_user_id().get(),
                                        "chatRevenueTransactionTypeSuggestedPostEarnings",
                                    ),
                                },
                            ))
                    }
                    _ => None,
                }
            }
            other => {
                // the C++ UNREACHABLE (cpp:479); the collapse answers the
                // unsupported form
                tdutils::log_error!("Receive unsupported transaction peer: {other:?}");
                Some(
                    td_api::ChatRevenueTransactionType::chatRevenueTransactionTypeUnsupported(
                        td_api::chatRevenueTransactionTypeUnsupported {},
                    ),
                )
            }
        };
        let r#type = match r#type {
            Some(r#type) => r#type,
            None => {
                tdutils::log_error!("Receive unsupported TON transaction: {:?}", transaction);
                td_api::ChatRevenueTransactionType::chatRevenueTransactionTypeUnsupported(
                    td_api::chatRevenueTransactionTypeUnsupported {},
                )
            }
        };
        // the SCOPE_EXIT clears (cpp:427-443, 459-461, 468-470) — the
        // consumed fields reset before the unused-field audit so the
        // built types don't flag their own source fields
        match &r#type {
            td_api::ChatRevenueTransactionType::chatRevenueTransactionTypeFragmentWithdrawal(
                withdrawal,
            ) => match withdrawal.state.as_ref() {
                Some(td_api::RevenueWithdrawalState::revenueWithdrawalStateSucceeded(_)) => {
                    transaction.transaction_date = 0;
                    transaction.transaction_url = String::new();
                }
                Some(td_api::RevenueWithdrawalState::revenueWithdrawalStatePending(_)) => {
                    transaction.pending = false;
                }
                Some(td_api::RevenueWithdrawalState::revenueWithdrawalStateFailed(_)) => {
                    transaction.failed = false;
                }
                None => {}
            },
            td_api::ChatRevenueTransactionType::chatRevenueTransactionTypeSponsoredMessageEarnings(
                _,
            ) => {
                transaction.ads_proceeds_from_date = 0;
                transaction.ads_proceeds_to_date = 0;
            }
            td_api::ChatRevenueTransactionType::chatRevenueTransactionTypeSuggestedPostEarnings(
                _,
            ) => {
                transaction.paid_messages = 0;
                transaction.title = String::new();
            }
            _ => {}
        }
        if !matches!(
            r#type,
            td_api::ChatRevenueTransactionType::chatRevenueTransactionTypeUnsupported(_)
        ) {
            let name = crate::star_manager::get_unused_star_transaction_field(
                &transaction,
                None,
                "",
                None,
                0,
            );
            if !name.is_empty() {
                tdutils::log_error!("Receive {name} with a TON transaction");
            }
        }
        transactions.push(td_api::ChatRevenueTransaction {
            cryptocurrency: "TON".to_owned(),
            cryptocurrency_amount: transaction_amount.ton_amount,
            r#type: Some(r#type),
        });
    }
    let ton_amount = crate::message_content::TonAmount::from_wire(result.balance.as_ref(), true);
    td_api::ChatRevenueTransactions {
        gram_amount: ton_amount.ton_amount,
        transactions,
        next_offset: result.next_offset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnv {
        updates: Vec<td_api::Update>,
        queries: Vec<String>,
    }

    impl StatisticsManagerEnv for TestEnv {
        fn close_flag(&self) -> bool {
            false
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn statistics_dc_id(
            &mut self,
            _dialog_id: DialogId,
            _for_full_statistics: bool,
        ) -> Result<StatisticsDc> {
            self.queries.push("dc".to_owned());
            Result::Ok(StatisticsDc::Resolved(DcId::internal(2)))
        }

        fn get_input_channel(
            &mut self,
            _channel_id: ChannelId,
        ) -> Option<telegram_api::InputChannel> {
            Some(telegram_api::InputChannel::inputChannel(
                telegram_api::inputChannel {
                    channel_id: 804,
                    access_hash: 0,
                },
            ))
        }

        fn get_input_peer(
            &mut self,
            _dialog_id: DialogId,
            _access_rights: AccessRights,
        ) -> Option<telegram_api::InputPeer> {
            Some(telegram_api::InputPeer::inputPeerEmpty(
                telegram_api::inputPeerEmpty {},
            ))
        }

        fn have_message_force(
            &mut self,
            _message_full_id: MessageFullId,
            _source: &'static str,
        ) -> bool {
            true
        }

        fn can_get_message_statistics(&mut self, _message_full_id: MessageFullId) -> bool {
            true
        }
    }

    fn stats_graph_data() -> telegram_api::StatsGraph {
        telegram_api::StatsGraph::statsGraph(telegram_api::statsGraph {
            flags: 0,
            json: Some(telegram_api::DataJSON {
                data: "{\"x\":1}".to_string(),
            }),
            zoom_token: "zoom".to_string(),
        })
    }

    #[test]
    fn graphs_convert_across_the_variants() {
        match convert_stats_graph(&stats_graph_data()) {
            td_api::StatisticalGraph::statisticalGraphData(graph) => {
                assert_eq!(graph.json_data, "{\"x\":1}");
                assert_eq!(graph.zoom_token, "zoom");
            }
            other => panic!("unexpected graph: {other:?}"),
        }

        match convert_stats_graph(&telegram_api::StatsGraph::statsGraphAsync(
            telegram_api::statsGraphAsync {
                token: "tok".to_string(),
            },
        )) {
            td_api::StatisticalGraph::statisticalGraphAsync(graph) => {
                assert_eq!(graph.token, "tok");
            }
            other => panic!("unexpected graph: {other:?}"),
        }

        match convert_stats_graph(&telegram_api::StatsGraph::statsGraphError(
            telegram_api::statsGraphError {
                error: "err".to_string(),
            },
        )) {
            td_api::StatisticalGraph::statisticalGraphError(graph) => {
                assert_eq!(graph.error_message, "err");
            }
            other => panic!("unexpected graph: {other:?}"),
        }
    }

    #[test]
    fn percentage_edges_match_the_cpp() {
        assert_eq!(get_percentage_value(0.0, 0.0, false), 0.0);
        assert_eq!(get_percentage_value(1.0, 0.0, false), 100.0);
        assert_eq!(get_percentage_value(50.0, 200.0, true), 25.0);
        assert_eq!(get_percentage_value(-50.0, 200.0, true), 0.0, "the clamp");
        assert_eq!(get_percentage_value(-50.0, 200.0, false), -25.0);
        assert_eq!(
            get_percentage_value(2e20, 1.0, false),
            100.0,
            "the part cap"
        );
    }

    #[test]
    fn absolute_values_carry_the_growth() {
        let value = convert_stats_absolute_value(60.0, 40.0);
        assert_eq!(value.value, 60.0);
        assert_eq!(value.previous_value, 40.0);
        assert_eq!(value.growth_rate_percentage, 50.0);
    }

    #[test]
    fn broadcast_stats_convert_field_wise() {
        let stats = convert_broadcast_stats(telegram_api::stats_BroadcastStats {
            period: Some(telegram_api::StatsDateRangeDays {
                min_date: 100,
                max_date: 200,
            }),
            followers: Some(telegram_api::StatsAbsValueAndPrev {
                current: 60.0,
                previous: 40.0,
            }),
            views_per_post: None,
            shares_per_post: None,
            reactions_per_post: None,
            views_per_story: None,
            shares_per_story: None,
            reactions_per_story: None,
            enabled_notifications: Some(telegram_api::StatsPercentValue {
                part: 25.0,
                total: 100.0,
            }),
            growth_graph: Some(stats_graph_data()),
            followers_graph: None,
            mute_graph: None,
            top_hours_graph: None,
            interactions_graph: None,
            iv_interactions_graph: None,
            views_by_source_graph: None,
            new_followers_by_source_graph: None,
            languages_graph: None,
            reactions_by_emotion_graph: None,
            story_interactions_graph: None,
            story_reactions_by_emotion_graph: None,
            recent_posts_interactions: vec![
                telegram_api::PostInteractionCounters::postInteractionCountersMessage(
                    telegram_api::postInteractionCountersMessage {
                        msg_id: 7,
                        views: 10,
                        forwards: 3,
                        reactions: 2,
                    },
                ),
                telegram_api::PostInteractionCounters::postInteractionCountersStory(
                    telegram_api::postInteractionCountersStory {
                        story_id: 9,
                        views: 11,
                        forwards: 4,
                        reactions: 5,
                    },
                ),
            ],
        });
        let td_api::ChatStatistics::chatStatisticsChannel(channel) = stats else {
            panic!("unexpected stats form");
        };
        let period = channel.period.expect("period");
        assert_eq!((period.start_date, period.end_date), (100, 200));
        let members = channel.member_count.expect("members");
        assert_eq!((members.value, members.previous_value), (60.0, 40.0));
        assert_eq!(
            channel.mean_message_view_count, None,
            "the None passthrough"
        );
        assert_eq!(channel.enabled_notifications_percentage, 25.0);
        assert!(channel.member_count_graph.is_some());
        assert_eq!(channel.join_graph, None);
        assert_eq!(channel.recent_interactions.len(), 2);
        match channel.recent_interactions[0]
            .object_type
            .as_ref()
            .expect("type")
        {
            td_api::ChatStatisticsObjectType::chatStatisticsObjectTypeMessage(message) => {
                // `MessageId(ServerMessageId(7))` — the server-id shift
                // rides the get (MessageId.h:59-61)
                assert_eq!(
                    message.message_id,
                    i64::from(crate::messages_manager::ServerMessageId::new(7).get()) << 20
                );
            }
            other => panic!("unexpected object type: {other:?}"),
        }
        match channel.recent_interactions[1]
            .object_type
            .as_ref()
            .expect("type")
        {
            td_api::ChatStatisticsObjectType::chatStatisticsObjectTypeStory(story) => {
                assert_eq!(story.story_id, 9);
            }
            other => panic!("unexpected object type: {other:?}"),
        }
    }

    #[test]
    fn megagroup_stats_filter_and_convert() {
        struct TopEnv {
            users: usize,
            ids: Vec<i64>,
        }
        impl StatisticsManagerEnv for TopEnv {
            fn close_flag(&self) -> bool {
                false
            }
            fn send_update(&mut self, _update: td_api::Update) {}
            fn on_get_users(&mut self, users: Vec<telegram_api::User>, _source: &'static str) {
                self.users += users.len();
            }
            fn get_user_id_object(&mut self, user_id: i64, _source: &'static str) -> i64 {
                self.ids.push(user_id);
                user_id
            }
        }
        let mut env = TopEnv {
            users: 0,
            ids: Vec::new(),
        };
        let stats = convert_megagroup_stats(
            &mut env,
            telegram_api::stats_MegagroupStats {
                period: Some(telegram_api::StatsDateRangeDays {
                    min_date: 1,
                    max_date: 2,
                }),
                members: Some(telegram_api::StatsAbsValueAndPrev {
                    current: 10.0,
                    previous: 5.0,
                }),
                messages: None,
                viewers: None,
                posters: None,
                growth_graph: None,
                members_graph: None,
                new_members_by_source_graph: None,
                languages_graph: None,
                messages_graph: None,
                actions_graph: None,
                top_hours_graph: None,
                weekdays_graph: None,
                top_posters: vec![
                    telegram_api::StatsGroupTopPoster {
                        user_id: 901,
                        messages: 5,
                        avg_chars: 10,
                    },
                    // the invalid row drops (cpp:95-97)
                    telegram_api::StatsGroupTopPoster {
                        user_id: -1,
                        messages: 5,
                        avg_chars: 10,
                    },
                ],
                top_admins: vec![
                    telegram_api::StatsGroupTopAdmin {
                        user_id: 902,
                        deleted: 1,
                        kicked: 2,
                        banned: 3,
                    },
                    // the negative kicked drops (cpp:98-100)
                    telegram_api::StatsGroupTopAdmin {
                        user_id: 903,
                        deleted: 1,
                        kicked: -2,
                        banned: 3,
                    },
                ],
                top_inviters: vec![telegram_api::StatsGroupTopInviter {
                    user_id: 904,
                    invitations: 7,
                }],
                users: vec![],
            },
        );
        assert_eq!(env.ids, vec![901, 902, 904], "the invalid rows drop");
        let td_api::ChatStatistics::chatStatisticsSupergroup(group) = stats else {
            panic!("unexpected stats form");
        };
        assert_eq!(group.top_senders.len(), 1);
        assert_eq!(group.top_administrators.len(), 1);
        assert_eq!(group.top_inviters.len(), 1);
        assert_eq!(group.top_senders[0].sent_message_count, 5);
    }

    #[test]
    fn ton_revenue_stats_synthesize_the_top_hours_and_usd_rate() {
        let stats = convert_ton_revenue_stats(telegram_api::payments_StarsRevenueStats {
            flags: 0,
            top_hours_graph: None,
            revenue_graph: Some(telegram_api::StatsGraph::statsGraphError(
                telegram_api::statsGraphError {
                    error: "no data".to_owned(),
                },
            )),
            status: Some(telegram_api::StarsRevenueStatus {
                flags: 0,
                withdrawal_enabled: true,
                current_balance: Some(telegram_api::StarsAmount::starsTonAmount(
                    telegram_api::starsTonAmount { amount: 100 },
                )),
                available_balance: Some(telegram_api::StarsAmount::starsAmount(
                    telegram_api::starsAmount {
                        amount: 50,
                        nanos: 0,
                    },
                )),
                overall_revenue: Some(telegram_api::StarsAmount::starsTonAmount(
                    telegram_api::starsTonAmount { amount: 200 },
                )),
                next_withdrawal_at: 0,
            }),
            usd_rate: 2.0,
        });
        match stats.revenue_by_hour_graph.as_ref().expect("synthesized") {
            td_api::StatisticalGraph::statisticalGraphError(graph) => {
                assert_eq!(graph.error_message, "no data");
            }
            other => panic!("unexpected graph: {other:?}"),
        }
        let amount = stats.revenue_amount.expect("status");
        assert_eq!(amount.cryptocurrency, "TON");
        assert_eq!(amount.total_amount, 200);
        assert_eq!(amount.balance_amount, 100);
        assert_eq!(amount.available_amount, 0, "the non-TON amount zeroes");
        assert!(amount.withdrawal_enabled);
        assert_eq!(stats.usd_rate, 2.0 * 1e-7);
    }

    #[test]
    fn ton_transactions_convert_the_peer_bands() {
        struct TxEnv {
            ids: Vec<i64>,
        }
        impl StatisticsManagerEnv for TxEnv {
            fn close_flag(&self) -> bool {
                false
            }
            fn send_update(&mut self, _update: td_api::Update) {}
            fn get_user_id_object(&mut self, user_id: i64, _source: &'static str) -> i64 {
                self.ids.push(user_id);
                user_id
            }
        }
        let mut env = TxEnv { ids: Vec::new() };
        let ton = |amount: i64| {
            Some(telegram_api::StarsAmount::starsTonAmount(
                telegram_api::starsTonAmount { amount },
            ))
        };
        let result = convert_ton_revenue_transactions(
            &mut env,
            telegram_api::payments_StarsStatus {
                flags: 0,
                balance: ton(500),
                subscriptions: Vec::new(),
                subscriptions_next_offset: String::new(),
                subscriptions_missing_balance: 0,
                history: vec![
                    // the fragment withdrawal band
                    telegram_api::StarsTransaction {
                        flags: 0,
                        refund: false,
                        pending: false,
                        failed: false,
                        gift: false,
                        reaction: false,
                        stargift_upgrade: false,
                        business_transfer: false,
                        stargift_resale: false,
                        posts_search: false,
                        stargift_prepaid_upgrade: false,
                        stargift_drop_original_details: false,
                        phonegroup_message: false,
                        stargift_auction_bid: false,
                        offer: false,
                        id: String::new(),
                        amount: ton(100),
                        date: 7,
                        peer: Some(
                            telegram_api::StarsTransactionPeer::starsTransactionPeerFragment(
                                telegram_api::starsTransactionPeerFragment {},
                            ),
                        ),
                        title: String::new(),
                        description: String::new(),
                        photo: None,
                        transaction_date: 11,
                        transaction_url: "https://t".to_owned(),
                        bot_payload: Vec::new(),
                        msg_id: 0,
                        extended_media: Vec::new(),
                        subscription_period: 0,
                        giveaway_post_id: 0,
                        stargift: None,
                        floodskip_number: 0,
                        starref_commission_permille: 0,
                        starref_peer: None,
                        starref_amount: None,
                        paid_messages: 0,
                        premium_gift_months: 0,
                        ads_proceeds_from_date: 0,
                        ads_proceeds_to_date: 0,
                    },
                    // the ads-proceeds band
                    telegram_api::StarsTransaction {
                        ads_proceeds_from_date: 20,
                        ads_proceeds_to_date: 30,
                        peer: Some(telegram_api::StarsTransactionPeer::starsTransactionPeerAds(
                            telegram_api::starsTransactionPeerAds {},
                        )),
                        amount: ton(60),
                        ..telegram_api::StarsTransaction::default()
                    },
                    // the non-TON amount drops
                    telegram_api::StarsTransaction {
                        amount: Some(telegram_api::StarsAmount::starsAmount(
                            telegram_api::starsAmount {
                                amount: 1,
                                nanos: 0,
                            },
                        )),
                        ..telegram_api::StarsTransaction::default()
                    },
                ],
                next_offset: "next".to_owned(),
                chats: vec![],
                users: vec![],
            },
        );
        assert!(env.ids.is_empty(), "no suggested-post band hit");
        assert_eq!(result.gram_amount, 500);
        assert_eq!(result.next_offset, "next");
        assert_eq!(result.transactions.len(), 2, "the non-TON row drops");
        match result.transactions[0].r#type.as_ref().expect("type") {
            td_api::ChatRevenueTransactionType::chatRevenueTransactionTypeFragmentWithdrawal(
                withdrawal,
            ) => {
                assert_eq!(withdrawal.withdrawal_date, 7);
                match withdrawal.state.as_ref().expect("state") {
                    td_api::RevenueWithdrawalState::revenueWithdrawalStateSucceeded(state) => {
                        assert_eq!((state.date, state.url.as_str()), (11, "https://t"));
                    }
                    other => panic!("unexpected withdrawal state: {other:?}"),
                }
            }
            other => panic!("unexpected transaction type: {other:?}"),
        }
        match result.transactions[1].r#type.as_ref().expect("type") {
            td_api::ChatRevenueTransactionType::chatRevenueTransactionTypeSponsoredMessageEarnings(
                earnings,
            ) => {
                assert_eq!((earnings.start_date, earnings.end_date), (20, 30));
            }
            other => panic!("unexpected transaction type: {other:?}"),
        }
        assert_eq!(result.transactions[0].cryptocurrency_amount, 100);
    }

    #[test]
    fn the_router_dispatches_through_the_send_legs() {
        let mut env = TestEnv {
            updates: Vec::new(),
            queries: Vec::new(),
        };
        let mut manager = StatisticsManager::new();
        // a channel dialog id — `get_channel_id` asserts the channel type
        let channel_dialog = DialogId::new(
            crate::chat_manager::DialogId::from_channel_id(crate::chat_manager::ChannelId::new(2))
                .get(),
        );
        // the TestEnv resolves the dc synchronously — the send leg runs
        // at once and parks the typed promise in the wire lot
        manager.get_channel_statistics(&mut env, channel_dialog, true, Promise::default());
        assert!(manager.dc_parked.is_empty(), "the resolved dc sends");
        assert_eq!(manager.pending.channel_stats.len(), 1, "the lot head");
        assert_eq!(
            manager.pending.channel_stats[0].0,
            crate::chat_manager::ChannelId::new(2)
        );
        assert!(!manager.pending.channel_stats[0].1, "the broadcast branch");

        // the refire window drains through the parked-request send leg
        // (the `finish_pending_channel_statistics_dc_id` shape)
        manager.get_channel_message_statistics(
            &mut env,
            MessageFullId::new(channel_dialog, crate::message_id::MessageId::new(7 << 20)),
            false,
            Promise::default(),
        );
        assert_eq!(manager.pending.message_stats.len(), 1);

        // the revenue families gate + dispatch over the main dc — no
        // dc chain call
        manager.get_dialog_revenue_statistics(
            &mut env,
            DialogId::new(4),
            false,
            Promise::default(),
        );
        assert_eq!(manager.pending.revenue_stats.len(), 1);
        manager.get_dialog_revenue_transactions(
            &mut env,
            DialogId::new(4),
            "off",
            10,
            Promise::default(),
        );
        assert_eq!(manager.pending.revenue_transactions.len(), 1);
        assert_eq!(
            env.queries.len(),
            2,
            "two dc chain calls (the revenue families need no chain)"
        );
    }
}
