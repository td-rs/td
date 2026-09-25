//! `StatisticsManager` (td/telegram/StatisticsManager.{h,cpp}) — the M.35
//! Wave-3 statistics manager. The value plane (the StatsGraph conversion
//! plus the percentage/absolute-value helpers) is the real port; the eleven
//! query handlers are `[blocked: net]` env seams with parked promises.
//!
//! # Recorded collapses the megagroup/broadcast/message/story/poll result
//! converters ride the MessagesManager (M.10, unmerged) / StoryManager
//! (M.47, not started) surfaces — the answer handlers join with those
//! tracks; the SRP withdrawal password is the AuthManager seam.

use crate::dialog_id::DialogId;
use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The `G()`/`td_` surface of the manager.
pub trait StatisticsManagerEnv {
    fn close_flag(&self) -> bool;

    fn send_update(&mut self, update: td_api::Update);

    /// `ChatManager::get_channel_statistics_dc_id` — the collapsed sync
    /// form (the dc-id answer is 0/unknown until the net step).
    fn get_channel_statistics_dc_id(
        &mut self,
        dialog_id: DialogId,
    ) -> std::result::Result<(), Status>;

    // ---- the query seams ([blocked: net]; the promises park) ----
    fn send_get_channel_stats_query(&mut self, dialog_id: DialogId, is_dark: bool) {
        tdutils::log_info!("Send stats.getBroadcastStats for {dialog_id:?} — [blocked: net]");
        let _ = is_dark;
    }
    fn send_get_channel_message_stats_query(&mut self, dialog_id: DialogId, is_dark: bool) {
        tdutils::log_info!("Send stats.getMessageStats for {dialog_id:?} — [blocked: net]");
        let _ = is_dark;
    }
    fn send_get_channel_story_stats_query(&mut self, dialog_id: DialogId, is_dark: bool) {
        tdutils::log_info!("Send stats.getStoryStats for {dialog_id:?} — [blocked: net]");
        let _ = is_dark;
    }
    fn send_get_dialog_revenue_stats_query(&mut self, dialog_id: DialogId, is_dark: bool) {
        tdutils::log_info!(
            "Send paystars.getBroadcastRevenueStats for {dialog_id:?} — [blocked: net]"
        );
        let _ = is_dark;
    }
    fn send_get_poll_stats_query(
        &mut self,
        message_full_id: crate::message_id::MessageFullId,
        is_dark: bool,
    ) {
        tdutils::log_info!("Send messages.getPollVotes for {message_full_id:?} — [blocked: net]");
        let _ = is_dark;
    }
    fn send_get_dialog_revenue_withdrawal_url_query(
        &mut self,
        dialog_id: DialogId,
        password: &str,
    ) {
        tdutils::log_info!(
            "Send paystars.getBroadcastRevenueWithdrawalUrl for {dialog_id:?} — [blocked: net]"
        );
        let _ = password;
    }
    fn send_get_dialog_revenue_transactions_query(
        &mut self,
        dialog_id: DialogId,
        offset: &str,
        limit: i32,
    ) {
        tdutils::log_info!(
            "Send paystars.getBroadcastRevenueTransactions for {dialog_id:?} — [blocked: net]"
        );
        let _ = (offset, limit);
    }
    fn send_get_message_public_forwards_query(
        &mut self,
        dialog_id: DialogId,
        offset: &str,
        limit: i32,
    ) {
        tdutils::log_info!(
            "Send stats.getMessagePublicForwards for {dialog_id:?} — [blocked: net]"
        );
        let _ = (offset, limit);
    }
    fn send_get_story_public_forwards_query(
        &mut self,
        dialog_id: DialogId,
        offset: &str,
        limit: i32,
    ) {
        tdutils::log_info!("Send stats.getStoryPublicForwards for {dialog_id:?} — [blocked: net]");
        let _ = (offset, limit);
    }
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

/// The collapsed `StatisticsManager` — a router over the query seams.
#[derive(Default)]
pub struct StatisticsManager {
    /// The `ChatStatistics`-typed promises parked at the stats seam.
    pub parked_chat_statistics: Vec<Promise<td_api::ChatStatistics>>,
    /// The `MessageStatistics`-typed promises parked at the message-stats
    /// seam.
    pub parked_message_statistics: Vec<Promise<td_api::MessageStatistics>>,
    /// The `StoryStatistics`-typed promises parked at the story-stats
    /// seam.
    pub parked_story_statistics: Vec<Promise<td_api::StoryStatistics>>,
    /// The `ChatRevenueStatistics`-typed promises parked at the revenue
    /// seam.
    pub parked_revenue_statistics: Vec<Promise<td_api::ChatRevenueStatistics>>,
    /// The `ChatRevenueTransactions`-typed promises parked at the
    /// transactions seam.
    pub parked_revenue_transactions: Vec<Promise<td_api::ChatRevenueTransactions>>,
    /// The `String`-typed promises parked at the withdrawal-url seam.
    pub parked_withdrawal_urls: Vec<Promise<String>>,
    /// The `PublicForwards`-typed promises parked at the forwards seams.
    pub parked_public_forwards: Vec<Promise<td_api::PublicForwards>>,
    /// The `PollVoteStatistics`-typed promises parked at the poll-stats
    /// seam.
    pub parked_poll_vote_statistics: Vec<Promise<td_api::PollVoteStatistics>>,
}

impl StatisticsManager {
    pub fn new() -> StatisticsManager {
        StatisticsManager::default()
    }

    /// `get_channel_statistics` (cpp:746-758).
    pub fn get_channel_statistics(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        is_dark: bool,
        promise: Promise<td_api::ChatStatistics>,
    ) {
        if !dialog_id.is_valid() {
            return;
        }
        env.send_get_channel_stats_query(dialog_id, is_dark);
        self.parked_chat_statistics.push(promise);
    }

    /// `get_channel_message_statistics` (cpp:828-840).
    pub fn get_channel_message_statistics(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        is_dark: bool,
        promise: Promise<td_api::MessageStatistics>,
    ) {
        if !dialog_id.is_valid() {
            return;
        }
        env.send_get_channel_message_stats_query(dialog_id, is_dark);
        self.parked_message_statistics.push(promise);
    }

    /// `get_channel_story_statistics` (cpp:858-870).
    pub fn get_channel_story_statistics(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        is_dark: bool,
        promise: Promise<td_api::StoryStatistics>,
    ) {
        if !dialog_id.is_valid() {
            return;
        }
        env.send_get_channel_story_stats_query(dialog_id, is_dark);
        self.parked_story_statistics.push(promise);
    }

    /// `get_dialog_revenue_statistics` (cpp:770-776).
    pub fn get_dialog_revenue_statistics(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        is_dark: bool,
        promise: Promise<td_api::ChatRevenueStatistics>,
    ) {
        env.send_get_dialog_revenue_stats_query(dialog_id, is_dark);
        self.parked_revenue_statistics.push(promise);
    }

    /// `get_poll_statistics` (cpp:888-898) — the messages-manager gate
    /// (`have_message_force` → "Message not found",
    /// `can_get_message_poll_vote_statistics` → "Poll statistics are
    /// inaccessible") runs in the dispatcher arm over the MessagesManager;
    /// the GetPollStatsQuery send is the seam.
    pub fn get_poll_statistics(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        message_full_id: crate::message_id::MessageFullId,
        is_dark: bool,
        promise: Promise<td_api::PollVoteStatistics>,
    ) {
        env.send_get_poll_stats_query(message_full_id, is_dark);
        self.parked_poll_vote_statistics.push(promise);
    }

    /// `get_dialog_revenue_withdrawal_url` (cpp:793-811) — the SRP
    /// password leg is the AuthManager seam.
    pub fn get_dialog_revenue_withdrawal_url(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        password: &str,
        promise: Promise<String>,
    ) {
        env.send_get_dialog_revenue_withdrawal_url_query(dialog_id, password);
        self.parked_withdrawal_urls.push(promise);
    }

    /// `get_dialog_revenue_transactions` (cpp:820-827).
    pub fn get_dialog_revenue_transactions(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        offset: &str,
        limit: i32,
        promise: Promise<td_api::ChatRevenueTransactions>,
    ) {
        env.send_get_dialog_revenue_transactions_query(dialog_id, offset, limit);
        self.parked_revenue_transactions.push(promise);
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

    /// `get_message_public_forwards` (cpp:920-936).
    pub fn get_message_public_forwards(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        offset: &str,
        limit: i32,
        promise: Promise<td_api::PublicForwards>,
    ) {
        env.send_get_message_public_forwards_query(dialog_id, offset, limit);
        self.parked_public_forwards.push(promise);
    }

    /// `get_story_public_forwards` (cpp:955-979).
    pub fn get_story_public_forwards(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        offset: &str,
        limit: i32,
        promise: Promise<td_api::PublicForwards>,
    ) {
        env.send_get_story_public_forwards_query(dialog_id, offset, limit);
        self.parked_public_forwards.push(promise);
    }

    /// `load_statistics_graph` (cpp:900-912) — the token + x form (the
    /// td_api arm does not exist in this revision; recorded).
    pub fn load_statistics_graph(
        &mut self,
        env: &mut impl StatisticsManagerEnv,
        dialog_id: DialogId,
        token: &str,
        x: i64,
        promise: Promise<td_api::StatisticalGraph>,
    ) {
        let _ = (env, dialog_id, token, x, promise);
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

        fn get_channel_statistics_dc_id(
            &mut self,
            _dialog_id: DialogId,
        ) -> std::result::Result<(), Status> {
            Ok(())
        }

        fn send_get_channel_stats_query(&mut self, dialog_id: DialogId, is_dark: bool) {
            self.queries
                .push(format!("channelStats {} {is_dark}", dialog_id.get()));
        }

        fn send_get_channel_message_stats_query(&mut self, dialog_id: DialogId, is_dark: bool) {
            self.queries
                .push(format!("messageStats {} {is_dark}", dialog_id.get()));
        }

        fn send_get_message_public_forwards_query(
            &mut self,
            dialog_id: DialogId,
            offset: &str,
            limit: i32,
        ) {
            self.queries.push(format!(
                "messageForwards {} {offset} {limit}",
                dialog_id.get()
            ));
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
    fn the_router_dispatches_through_the_seams() {
        let mut env = TestEnv {
            updates: Vec::new(),
            queries: Vec::new(),
        };
        let mut manager = StatisticsManager::new();
        manager.get_channel_statistics(&mut env, DialogId::new(3), true, Promise::default());
        manager.get_channel_message_statistics(
            &mut env,
            DialogId::new(3),
            false,
            Promise::default(),
        );
        manager.get_message_public_forwards(
            &mut env,
            DialogId::new(3),
            "off",
            10,
            Promise::default(),
        );
        assert!(env.queries.iter().any(|q| q == "channelStats 3 true"));
        assert!(env.queries.iter().any(|q| q == "messageStats 3 false"));
        assert!(env.queries.iter().any(|q| q == "messageForwards 3 off 10"));
        assert_eq!(manager.parked_chat_statistics.len(), 1);
        assert_eq!(manager.parked_message_statistics.len(), 1);
        assert_eq!(manager.parked_public_forwards.len(), 1);
    }

    #[test]
    fn revenue_status_emits_the_amount_update() {
        let mut env = TestEnv {
            updates: Vec::new(),
            queries: Vec::new(),
        };
        let mut manager = StatisticsManager::new();
        manager.on_update_dialog_revenue_transactions(
            &mut env,
            DialogId::new(4),
            &telegram_api::StarsRevenueStatus {
                flags: 0,
                withdrawal_enabled: true,
                current_balance: Some(telegram_api::StarsAmount::starsAmount(
                    telegram_api::starsAmount {
                        amount: 100,
                        nanos: 0,
                    },
                )),
                available_balance: Some(telegram_api::StarsAmount::starsAmount(
                    telegram_api::starsAmount {
                        amount: 50,
                        nanos: 0,
                    },
                )),
                overall_revenue: Some(telegram_api::StarsAmount::starsAmount(
                    telegram_api::starsAmount {
                        amount: 200,
                        nanos: 0,
                    },
                )),
                next_withdrawal_at: 0,
            },
        );
        assert_eq!(env.updates.len(), 1);
    }
}
