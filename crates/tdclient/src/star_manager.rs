//! `StarManager` — the M.star slice.
//!
//! Port of `td/telegram/StarManager.{h,cpp}`: the owned Telegram Stars /
//! TON balance plane and the star payments/transactions/subscriptions
//! request plane (the 16 td_api request arms the C++ `Requests::on_request`
//! routes into `td_->star_manager_`).
//!
//! The manager value plane is real: the `get_star_count` /
//! `get_nanostar_count` / `get_months_by_star_count` statics, the owned
//! star/ton count ledger with its `owned_star_count`/`owned_ton_count`
//! pmc persistence and the `updateOwnedStarCount` /
//! `updateOwnedGramCount` outbox, `can_manage_stars`, and the 16 request
//! entry points with their exact C++ validation order.
//!
//! Seams ([`StarManagerEnv`], the unmerged-manager surfaces):
//! - StickersManager (`load_premium_gift_sticker_set`,
//!   `load_ton_gift_sticker_set`) — the C++ chains
//!   `get_star_transactions`/`get_ton_transactions` through the sticker
//!   set loads before the query; the default is the no-op load and the
//!   query seam fires immediately.
//! - PasswordManager (`get_input_check_password_srp`) — the M.25
//!   password plane; the default parks the SRP promise like the
//!   CallbackQueriesManager seam, and the withdrawal-url query rides
//!   the placeholder empty SRP (the collapsed shape of the C++
//!   send-closure chain).
//! - DialogManager (`my_dialog_id`) — the M.6.15a free-function answer
//!   (`getTonWithdrawalUrl` withdraws from the own dialog).
//! - UserManager (`get_bot_data`, `get_input_user`, `get_my_id`) and
//!   ChatManager (`get_channel_status`) — the read halves delegate to
//!   the real read surface through the td wiring.
//! - Every `td_->create_handler<...>->send(...)` query rides the
//!   client-side dispatcher under its `StarNetKind` (the answers drain
//!   through `Td::on_star_net_answer` — the port of the C++ handler
//!   halves of StarManager.cpp:49-1400): the topup/gift/giveaway option
//!   families, subscriptions (the `StarSubscription` conversion + the
//!   owned-balance update), the subscription change/cancel/fulfill and
//!   refund families, the revenue stats/withdrawal-url/ads-url set, the
//!   paid-message revenue and the transactions pair (the
//!   `GetStarsTransactionsQuery` list/by-id forms + the
//!   `GetTonTransactionsQuery` TON leg — the transaction partner ladder
//!   with its users/chats intakes, the owned-balance updates and the
//!   file-source registration live in the `Td::on_star_net_answer`
//!   arms).
//!
//! Recorded divergences:
//! - `on_update_stars_revenue_status` (StarManager.cpp:1761-1782) and the
//!   `convert_stars_revenue_status`/`convert_gram_revenue_status`
//!   converters ride the updates integration (the update arrives through
//!   the net step; the StatisticsManager chat-revenue twin is the landed
//!   shape to follow).
//! - The C++ per-manager `star_transaction_file_source_ids_` dedup maps
//!   (StarManager.cpp:1784-1797) collapse onto the
//!   `FileReferenceManager` source interning (an equal source keeps its
//!   id — the same dedup the C++ cache provides); the answer arms intern
//!   `FileSource::StarTransaction` directly.
//! - `reload_star_transaction`/`reload_owned_star_count` /
//!   `reload_owned_ton_count` (StarManager.cpp:1739-1758) are landed
//!   here over the same seams; their C++ callers (UpdatesManager,
//!   StarGiftManager, FileReferenceManager) ride their own planes.
//! - The `G()->close_status()` guards of the query tails run here
//!   against the env while the promise parks; the real send sites run
//!   past them on the Td side.

use crate::dialog_id::{DialogId, DialogType};
use crate::message_content::{StarAmount, TonAmount};
use crate::star_gift_manager::StarGiftResalePrice;
use crate::td_api;
use crate::user_manager::{BotData, UserId};
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::{Result, Status};

/// `StarManager::get_star_count` (StarManager.cpp:1799-1816) — the
/// ±2^51 clamp with the error logs; `allow_negative` keeps negative
/// amounts inside the clamp instead of zeroing them.
pub fn get_star_count(amount: i64, allow_negative: bool) -> i64 {
    let max_amount = 1i64 << 51;
    if amount < 0 {
        if !allow_negative {
            tdutils::log_error!("Receive Telegram Star amount = {amount}");
            return 0;
        }
        if amount < -max_amount {
            tdutils::log_error!("Receive Telegram Star amount = {amount}");
            return -max_amount;
        }
    }
    if amount > max_amount {
        tdutils::log_error!("Receive Telegram Star amount = {amount}");
        return max_amount;
    }
    amount
}

/// `StarManager::get_nanostar_count` (StarManager.cpp:1818-1838) — the
/// sign normalization; the star count is corrected in place so the pair
/// carries one sign.
pub fn get_nanostar_count(star_count: &mut i64, mut nanostar_count: i32) -> i32 {
    if !(-1_000_000_000 < nanostar_count && nanostar_count < 1_000_000_000) {
        tdutils::log_error!("Receive {star_count} + {nanostar_count} Telegram Stars");
        return nanostar_count;
    }
    let mut corrected = false;
    if *star_count < 0 && nanostar_count > 0 {
        tdutils::log_error!("Receive {star_count} + {nanostar_count} Telegram Stars");
        *star_count += 1;
        nanostar_count -= 1_000_000_000;
        corrected = true;
    }
    if *star_count > 0 && nanostar_count < 0 {
        tdutils::log_error!("Receive {star_count} + {nanostar_count} Telegram Stars");
        *star_count -= 1;
        nanostar_count += 1_000_000_000;
        corrected = true;
    }
    if corrected
        && ((*star_count < 0 && nanostar_count > 0) || (*star_count > 0 && nanostar_count < 0))
    {
        tdutils::log_error!("Receive {star_count} + {nanostar_count} Telegram Stars");
        return 0;
    }
    nanostar_count
}

/// `StarManager::get_months_by_star_count` (StarManager.cpp:1840-1842).
pub fn get_months_by_star_count(star_count: i64) -> i32 {
    if star_count <= 1000 {
        3
    } else if star_count < 2500 {
        6
    } else {
        12
    }
}

/// `to_integer<int32>` (tdutils/td/utils/misc.h) — the leading-digits
/// parse with the two's-complement clamping, never an error; the local
/// copy of the private helper the other managers keep.
fn to_integer_i32(value: &str) -> i32 {
    let value = value.trim_start_matches(['+']);
    let mut result: i64 = 0;
    for c in value.chars() {
        if !c.is_ascii_digit() {
            break;
        }
        result = (result * 10 + (c as u8 - b'0') as i64).clamp(i32::MIN as i64, i32::MAX as i64);
    }
    result as i32
}

/// The `Td *td` + `G()` slice of the collapsed StarManager.
pub trait StarManagerEnv {
    /// `td_->auth_manager_->is_authorized()`.
    fn is_authorized(&self) -> bool;
    /// `td_->auth_manager_->is_bot()`.
    fn is_bot(&self) -> bool;
    /// `G()->close_status()`.
    fn close_status(&self) -> Status;
    /// `td_->user_manager_->get_my_id()`.
    fn my_user_id(&self) -> UserId;
    /// `td_->dialog_manager_->get_my_dialog_id()` (the M.6.15a
    /// free-function answer).
    fn my_dialog_id(&self) -> DialogId;

    /// `td_->user_manager_->get_bot_data(user_id)`.
    fn get_bot_data(&mut self, user_id: UserId) -> Result<BotData>;
    /// `td_->chat_manager_->get_channel_status(channel_id)`.
    fn get_channel_status(
        &mut self,
        channel_id: crate::chat_manager::ChannelId,
    ) -> crate::chat_manager::DialogParticipantStatus;
    /// `td_->user_manager_->get_input_user(user_id)`.
    fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser>;

    /// `G()->td_db()->get_binlog_pmc()->get`.
    fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;
    /// `G()->td_db()->get_binlog_pmc()->set`.
    fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]);
    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    /// `td_->stickers_manager_->load_premium_gift_sticker_set(...)`
    /// (the getStarTransactions chain) — the default is the no-op load;
    /// the query seam fires immediately.
    fn load_premium_gift_sticker_set(&mut self) {}

    /// `td_->stickers_manager_->load_ton_gift_sticker_set(...)` (the
    /// getTonTransactions chain) — the default is the no-op load.
    fn load_ton_gift_sticker_set(&mut self) {}

    /// `td_->password_manager_->get_input_check_password_srp(password,
    /// promise)` — [blocked: PasswordManager]; the SRP promise parks
    /// with the seam.
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

    // ----------------------------------------------------------------
    // The net query senders — the C++ ResultHandler send legs
    // (StarManager.cpp:49-1400). The default arms are the net-inert
    // logs (the net-inert tests and the pre-init windows); the answers
    // drain through `Td::on_star_net_answer`.
    // ----------------------------------------------------------------

    /// `GetStarsTopupOptionsQuery::send` (payments.getStarsTopupOptions).
    fn send_get_star_payment_options_query(&mut self) {
        tdutils::log_info!("Send payments.getStarsTopupOptions — [net-inert]");
    }

    /// `GetStarsGiftOptionsQuery::send` (payments.getStarsGiftOptions).
    fn send_get_star_gift_payment_options_query(
        &mut self,
        _input_user: Option<telegram_api::InputUser>,
    ) {
        tdutils::log_info!("Send payments.getStarsGiftOptions — [net-inert]");
    }

    /// `GetStarsGiveawayOptionsQuery::send`
    /// (payments.getStarsGiveawayOptions).
    fn send_get_star_giveaway_payment_options_query(&mut self) {
        tdutils::log_info!("Send payments.getStarsGiveawayOptions — [net-inert]");
    }

    /// `GetStarsTransactionsQuery::send` (payments.getStarsTransactions).
    fn send_get_star_transactions_query(
        &mut self,
        _dialog_id: DialogId,
        _subscription_id: String,
        _offset: String,
        _limit: i32,
        _direction: Option<td_api::TransactionDirection>,
    ) {
        tdutils::log_info!("Send payments.getStarsTransactions — [net-inert]");
    }

    /// `GetStarsTransactionsQuery::send` (the by-id reload form —
    /// payments.getStarsTransactionsByID, StarManager.cpp:200-211).
    fn send_get_star_transactions_by_id_query(
        &mut self,
        _dialog_id: DialogId,
        _transaction_id: String,
        _is_refund: bool,
    ) {
        tdutils::log_info!("Send payments.getStarsTransactionsByID — [net-inert]");
    }

    /// `GetTonTransactionsQuery::send`
    /// (payments.getStarsTransactions, the TON leg).
    fn send_get_ton_transactions_query(
        &mut self,
        _offset: String,
        _limit: i32,
        _direction: Option<td_api::TransactionDirection>,
    ) {
        tdutils::log_info!("Send payments.getStarsTransactions (TON) — [net-inert]");
    }

    /// `GetStarsSubscriptionsQuery::send`
    /// (payments.getStarsSubscriptions).
    fn send_get_star_subscriptions_query(&mut self, _only_expiring: bool, _offset: String) {
        tdutils::log_info!("Send payments.getStarsSubscriptions — [net-inert]");
    }

    /// `ChangeStarsSubscriptionQuery::send`
    /// (payments.changeStarsSubscription).
    fn send_change_star_subscription_query(
        &mut self,
        _subscription_id: String,
        _is_canceled: bool,
    ) {
        tdutils::log_info!("Send payments.changeStarsSubscription — [net-inert]");
    }

    /// `BotCancelStarsSubscriptionQuery::send`
    /// (payments.botCancelStarsSubscription).
    fn send_bot_cancel_star_subscription_query(
        &mut self,
        _input_user: telegram_api::InputUser,
        _telegram_payment_charge_id: String,
        _is_canceled: bool,
    ) {
        tdutils::log_info!("Send payments.botCancelStarsSubscription — [net-inert]");
    }

    /// `FulfillStarsSubscriptionQuery::send`
    /// (payments.fulfillStarsSubscription).
    fn send_fulfill_star_subscription_query(&mut self, _subscription_id: String) {
        tdutils::log_info!("Send payments.fulfillStarsSubscription — [net-inert]");
    }

    /// `RefundStarsChargeQuery::send` (payments.refundStarsCharge).
    fn send_refund_star_charge_query(
        &mut self,
        _input_user: telegram_api::InputUser,
        _telegram_payment_charge_id: String,
    ) {
        tdutils::log_info!("Send payments.refundStarsCharge — [net-inert]");
    }

    /// `GetStarsRevenueStatsQuery::send` (payments.getStarsRevenueStats).
    fn send_get_star_revenue_stats_query(&mut self, _dialog_id: DialogId, _is_dark: bool) {
        tdutils::log_info!("Send payments.getStarsRevenueStats — [net-inert]");
    }

    /// `GetStarsRevenueWithdrawalUrlQuery::send`
    /// (payments.getStarsRevenueWithdrawalUrl).
    fn send_get_stars_revenue_withdrawal_url_query(
        &mut self,
        _dialog_id: DialogId,
        _is_ton: bool,
        _amount: i64,
        _input_check_password: telegram_api::InputCheckPasswordSRP,
    ) {
        tdutils::log_info!("Send payments.getStarsRevenueWithdrawalUrl — [net-inert]");
    }

    /// `GetStarsRevenueAdsAccountUrlQuery::send`
    /// (payments.getStarsRevenueAdsAccountUrl).
    fn send_get_stars_revenue_ads_account_url_query(&mut self, _dialog_id: DialogId) {
        tdutils::log_info!("Send payments.getStarsRevenueAdsAccountUrl — [net-inert]");
    }

    /// `GetGramRevenueStatsQuery::send`
    /// (payments.getStarsRevenueStats, the TON leg).
    fn send_get_gram_revenue_stats_query(&mut self, _is_dark: bool) {
        tdutils::log_info!("Send payments.getStarsRevenueStats (TON) — [net-inert]");
    }

    /// `GetPaidMessageRevenueQuery::send` (the paid-message revenue leg).
    fn send_get_paid_message_revenue_query(&mut self, _input_user: telegram_api::InputUser) {
        tdutils::log_info!("Send payments.getPaidMessageRevenue — [net-inert]");
    }
}

/// The parked query promises — the answer halves land with the net step
/// and fill these.
#[derive(Default)]
pub struct PendingStarQueries {
    pub star_payment_options: Vec<Promise<td_api::StarPaymentOptions>>,
    pub star_giveaway_payment_options: Vec<Promise<td_api::StarGiveawayPaymentOptions>>,
    pub star_transactions: Vec<Promise<td_api::StarTransactions>>,
    pub ton_transactions: Vec<Promise<td_api::TonTransactions>>,
    pub star_subscriptions: Vec<Promise<td_api::StarSubscriptions>>,
    pub units: Vec<Promise<()>>,
    pub star_revenue_statistics: Vec<Promise<td_api::StarRevenueStatistics>>,
    pub gram_revenue_statistics: Vec<Promise<td_api::GramRevenueStatistics>>,
    pub urls: Vec<Promise<String>>,
    pub paid_message_revenue: Vec<Promise<td_api::StarCount>>,
}

/// `StarSubscription` (StarSubscription.{h,cpp}) — the wire value class
/// of the subscriptions answer; the photo leg stays the
/// `[blocked: files]` seam (`get_web_document_photo` — the td_api object
/// answers `None`).
pub(crate) struct StarSubscription {
    pub(crate) id: String,
    pub(crate) dialog_id: DialogId,
    pub(crate) until_date: i32,
    pub(crate) can_reuse: bool,
    pub(crate) is_canceled: bool,
    pub(crate) is_bot_canceled: bool,
    pub(crate) missing_balance: bool,
    pub(crate) invite_hash: String,
    pub(crate) title: String,
    pub(crate) invoice_slug: String,
    pub(crate) pricing: crate::chat_manager::StarSubscriptionPricing,
}

impl StarSubscription {
    /// The wire constructor (StarSubscription.cpp:14-27).
    pub(crate) fn new(subscription: telegram_api::StarsSubscription) -> StarSubscription {
        StarSubscription {
            id: subscription.id,
            dialog_id: subscription
                .peer
                .as_ref()
                .map_or(DialogId::default(), DialogId::from_peer),
            until_date: subscription.until_date,
            can_reuse: subscription.can_refulfill,
            is_canceled: subscription.canceled,
            is_bot_canceled: subscription.bot_canceled,
            missing_balance: subscription.missing_balance,
            invite_hash: subscription.chat_invite_hash,
            title: subscription.title,
            invoice_slug: subscription.invoice_slug,
            pricing: crate::chat_manager::StarSubscriptionPricing::from_telegram(
                subscription.pricing,
            ),
        }
    }

    /// `is_valid` (StarSubscription.h:35-37).
    pub(crate) fn is_valid(&self) -> bool {
        !self.id.is_empty()
            && self.dialog_id.is_valid()
            && self.until_date >= 0
            && !self.pricing.is_empty()
    }
}

/// `convert_stars_revenue_status` (StarManager.cpp:1157-1184) — the
/// Stars-band status conversion; the invalid-amount band logs and keeps
/// the zero amounts.
pub(crate) fn convert_stars_revenue_status(
    obj: &telegram_api::StarsRevenueStatus,
    unix_time: i32,
) -> td_api::StarRevenueStatus {
    let mut next_withdrawal_in = 0;
    if obj.withdrawal_enabled && obj.next_withdrawal_at > 0 {
        next_withdrawal_in = (obj.next_withdrawal_at - unix_time).max(1);
    }
    let is_stars = |amount: &Option<telegram_api::StarsAmount>| {
        matches!(amount, Some(telegram_api::StarsAmount::starsAmount(_)))
    };
    let (overall_revenue, current_balance, available_balance) = if is_stars(&obj.overall_revenue)
        && is_stars(&obj.current_balance)
        && is_stars(&obj.available_balance)
    {
        (
            StarAmount::from_wire(obj.overall_revenue.as_ref(), true),
            StarAmount::from_wire(obj.current_balance.as_ref(), true),
            StarAmount::from_wire(obj.available_balance.as_ref(), true),
        )
    } else {
        tdutils::log_error!("Receive invalid stars revenue status");
        (
            StarAmount::default(),
            StarAmount::default(),
            StarAmount::default(),
        )
    };
    td_api::StarRevenueStatus {
        total_amount: Some(overall_revenue.get_star_amount_object()),
        current_amount: Some(current_balance.get_star_amount_object()),
        available_amount: Some(available_balance.get_star_amount_object()),
        withdrawal_enabled: obj.withdrawal_enabled,
        next_withdrawal_in,
    }
}

/// `convert_gram_revenue_status` (StarManager.cpp:1316-1341) — the TON
/// twin over the `starsTonAmount` band.
pub(crate) fn convert_gram_revenue_status(
    obj: &telegram_api::StarsRevenueStatus,
) -> td_api::GramRevenueStatus {
    if obj.next_withdrawal_at != 0 {
        tdutils::log_error!("Receive stars revenue status with a withdrawal date");
    }
    let is_ton = |amount: &Option<telegram_api::StarsAmount>| {
        matches!(amount, Some(telegram_api::StarsAmount::starsTonAmount(_)))
    };
    let (overall_revenue, current_balance, available_balance) = if is_ton(&obj.overall_revenue)
        && is_ton(&obj.current_balance)
        && is_ton(&obj.available_balance)
    {
        (
            TonAmount::from_wire(obj.overall_revenue.as_ref(), true),
            TonAmount::from_wire(obj.current_balance.as_ref(), true),
            TonAmount::from_wire(obj.available_balance.as_ref(), true),
        )
    } else {
        tdutils::log_error!("Receive invalid gram revenue status");
        (
            TonAmount::default(),
            TonAmount::default(),
            TonAmount::default(),
        )
    };
    td_api::GramRevenueStatus {
        total_amount: overall_revenue.ton_amount,
        balance_amount: current_balance.ton_amount,
        available_amount: available_balance.ton_amount,
        withdrawal_enabled: obj.withdrawal_enabled,
    }
}

/// The collapsed `StarManager` (StarManager.h:22-92): the owned
/// Telegram Stars / TON balance ledger plus the request plane.
#[derive(Default)]
pub struct StarManager {
    pub(crate) pending: PendingStarQueries,

    is_owned_star_count_inited: bool,
    owned_star_count: i64,
    owned_nanostar_count: i32,
    pending_owned_star_count: i64,
    sent_star_count: i64,
    sent_nanostar_count: i32,

    is_owned_ton_count_inited: bool,
    owned_ton_count: i64,
    pending_owned_ton_count: i64,
    sent_ton_count: i64,
}

impl StarManager {
    pub fn new() -> StarManager {
        StarManager::default()
    }

    /// Pop the head promise of a pending lot — the answer-routing half
    /// of a query family (the C++ handler's own promise).
    pub(crate) fn take_pending<T: std::marker::Send>(
        &mut self,
        slot: for<'a> fn(&'a mut PendingStarQueries) -> &'a mut Vec<Promise<T>>,
    ) -> Option<Promise<T>> {
        let lot = slot(&mut self.pending);
        if lot.is_empty() {
            None
        } else {
            Some(lot.remove(0))
        }
    }

    /// `StarManager::start_up` (StarManager.cpp:1402-1423) — the
    /// `owned_star_count`/`owned_ton_count` pmc load; the initial
    /// updates go out when the pmc has the values.
    pub fn start_up(&mut self, env: &mut impl StarManagerEnv) {
        if env.is_bot() || !env.is_authorized() {
            return;
        }
        let owned_star_count = env.binlog_pmc_get_raw(b"owned_star_count");
        if !owned_star_count.is_empty() {
            let owned_star_count = String::from_utf8_lossy(&owned_star_count).into_owned();
            let mut parts = owned_star_count.split(' ');
            let star_count = parts.next().unwrap_or("");
            let nanostar_count = parts.next().unwrap_or("");
            self.is_owned_star_count_inited = true;
            self.owned_star_count = crate::misc::to_integer_i64(star_count);
            self.owned_nanostar_count = to_integer_i32(nanostar_count);
            self.sent_star_count = self.owned_star_count;
            self.sent_nanostar_count = self.owned_nanostar_count;
            env.send_update(self.get_update_owned_star_count_object());
        }
        let owned_ton_count = env.binlog_pmc_get_raw(b"owned_ton_count");
        if !owned_ton_count.is_empty() {
            let owned_ton_count = String::from_utf8_lossy(&owned_ton_count).into_owned();
            self.is_owned_ton_count_inited = true;
            self.owned_ton_count = crate::misc::to_integer_i64(&owned_ton_count);
            self.sent_ton_count = self.owned_ton_count;
            env.send_update(self.get_update_owned_gram_count_object());
        }
    }

    /// `get_update_owned_star_count_object` (StarManager.cpp:1429-1434).
    pub fn get_update_owned_star_count_object(&self) -> td_api::Update {
        debug_assert!(self.is_owned_star_count_inited);
        // sent_star_count can be negative as well as owned_star_count
        td_api::Update::updateOwnedStarCount(td_api::updateOwnedStarCount {
            star_amount: Some(td_api::StarAmount {
                star_count: self.sent_star_count,
                nanostar_count: self.sent_nanostar_count,
            }),
        })
    }

    /// `get_update_owned_gram_count_object` (StarManager.cpp:1436-1440).
    pub fn get_update_owned_gram_count_object(&self) -> td_api::Update {
        debug_assert!(self.is_owned_ton_count_inited);
        // sent_ton_count can be negative as well as owned_ton_count
        td_api::Update::updateOwnedGramCount(td_api::updateOwnedGramCount {
            gram_amount: self.sent_ton_count,
        })
    }

    /// `on_update_owned_star_amount` (StarManager.cpp:1442-1462).
    pub fn on_update_owned_star_amount(
        &mut self,
        env: &mut impl StarManagerEnv,
        star_amount: StarAmount,
    ) {
        if env.is_bot() {
            return;
        }
        let star_count = star_amount.star_count;
        let nanostar_count = star_amount.nanostar_count;
        if self.is_owned_star_count_inited
            && star_count == self.owned_star_count
            && nanostar_count == self.owned_nanostar_count
        {
            return;
        }
        self.is_owned_star_count_inited = true;
        self.owned_star_count = star_count;
        self.owned_nanostar_count = nanostar_count;
        if self.owned_star_count + self.pending_owned_star_count != self.sent_star_count
            || self.owned_nanostar_count != self.sent_nanostar_count
        {
            self.sent_star_count = self.owned_star_count + self.pending_owned_star_count;
            self.sent_nanostar_count = self.owned_nanostar_count;
            env.send_update(self.get_update_owned_star_count_object());
        }
        env.binlog_pmc_set_raw(
            b"owned_star_count",
            format!("{} {}", self.owned_star_count, self.owned_nanostar_count).as_bytes(),
        );
    }

    /// `on_update_owned_ton_amount` (StarManager.cpp:1464-1479).
    pub fn on_update_owned_ton_amount(
        &mut self,
        env: &mut impl StarManagerEnv,
        ton_amount: TonAmount,
    ) {
        if env.is_bot() {
            return;
        }
        let ton_count = ton_amount.ton_amount;
        if self.is_owned_ton_count_inited && ton_count == self.owned_ton_count {
            return;
        }
        self.is_owned_ton_count_inited = true;
        self.owned_ton_count = ton_count;
        if self.owned_ton_count + self.pending_owned_ton_count != self.sent_ton_count {
            self.sent_ton_count = self.owned_ton_count + self.pending_owned_ton_count;
            env.send_update(self.get_update_owned_gram_count_object());
        }
        env.binlog_pmc_set_raw(
            b"owned_ton_count",
            self.owned_ton_count.to_string().as_bytes(),
        );
    }

    /// `add_pending_owned_star_count` (StarManager.cpp:1481-1496).
    pub fn add_pending_owned_star_count(
        &mut self,
        env: &mut impl StarManagerEnv,
        star_count: i64,
        move_to_owned: bool,
    ) {
        if star_count == 0 {
            return;
        }
        self.pending_owned_star_count += star_count;
        if self.is_owned_star_count_inited {
            if move_to_owned {
                self.owned_star_count -= star_count;
                env.binlog_pmc_set_raw(
                    b"owned_star_count",
                    format!("{} {}", self.owned_star_count, self.owned_nanostar_count).as_bytes(),
                );
            } else {
                self.sent_star_count += star_count;
                env.send_update(self.get_update_owned_star_count_object());
            }
        }
    }

    /// `add_pending_owned_ton_count` (StarManager.cpp:1498-1512).
    pub fn add_pending_owned_ton_count(
        &mut self,
        env: &mut impl StarManagerEnv,
        ton_count: i64,
        move_to_owned: bool,
    ) {
        if ton_count == 0 {
            return;
        }
        self.pending_owned_ton_count += ton_count;
        if self.is_owned_ton_count_inited {
            if move_to_owned {
                self.owned_ton_count -= ton_count;
                env.binlog_pmc_set_raw(
                    b"owned_ton_count",
                    self.owned_ton_count.to_string().as_bytes(),
                );
            } else {
                self.sent_ton_count += ton_count;
                env.send_update(self.get_update_owned_gram_count_object());
            }
        }
    }

    /// `add_pending_owned_amount` (StarManager.cpp:1514-1522).
    pub fn add_pending_owned_amount(
        &mut self,
        env: &mut impl StarManagerEnv,
        amount: StarGiftResalePrice,
        multiplier: i32,
        move_to_owned: bool,
    ) {
        if amount.is_star() {
            return self.add_pending_owned_star_count(
                env,
                multiplier as i64 * amount.get_star_count(),
                move_to_owned,
            );
        }
        if amount.is_ton() {
            return self.add_pending_owned_ton_count(
                env,
                multiplier as i64 * amount.get_ton_count(),
                move_to_owned,
            );
        }
        unreachable!("unknown StarGiftResalePrice type");
    }

    /// `has_owned_star_count` (StarManager.cpp:1524-1529).
    pub fn has_owned_star_count(&self, star_count: i64) -> bool {
        if star_count <= 0 || !self.is_owned_star_count_inited {
            return true;
        }
        self.sent_star_count >= star_count
    }

    /// `has_owned_ton_count` (StarManager.cpp:1531-1536).
    pub fn has_owned_ton_count(&self, ton_count: i64) -> bool {
        if ton_count <= 0 || !self.is_owned_ton_count_inited {
            return true;
        }
        self.sent_ton_count >= ton_count
    }

    /// `has_owned_amount` (StarManager.cpp:1538-1546).
    pub fn has_owned_amount(&self, amount: StarGiftResalePrice) -> bool {
        if amount.is_star() {
            return self.has_owned_star_count(amount.get_star_count());
        }
        if amount.is_ton() {
            return self.has_owned_ton_count(amount.get_ton_count());
        }
        false
    }

    /// `can_manage_stars` (StarManager.cpp:1548-1572).
    pub fn can_manage_stars(
        &self,
        env: &mut impl StarManagerEnv,
        dialog_id: DialogId,
        allow_self: bool,
    ) -> Result<()> {
        match dialog_id.get_type() {
            DialogType::User => {
                let user_id = dialog_id.get_user_id();
                if allow_self && user_id == env.my_user_id() {
                    return Result::Ok(());
                }
                let bot_data = match env.get_bot_data(user_id) {
                    Result::Ok(bot_data) => bot_data,
                    Result::Err(status) => return Result::Err(status),
                };
                if !bot_data.can_be_edited {
                    return Result::from_status(Status::error(400, "The bot isn't owned"));
                }
            }
            DialogType::Channel => {
                let channel_id = dialog_id.get_channel_id();
                if !env.get_channel_status(channel_id).is_creator() && !allow_self {
                    return Result::from_status(Status::error(400, "Not enough rights"));
                }
            }
            _ => {
                return Result::from_status(Status::error(400, "Unallowed chat specified"));
            }
        }
        Result::Ok(())
    }

    /// `get_star_payment_options` (StarManager.cpp:1574-1576).
    pub fn get_star_payment_options(
        &mut self,
        env: &mut impl StarManagerEnv,
        promise: Promise<td_api::StarPaymentOptions>,
    ) {
        self.pending.star_payment_options.push(promise);
        env.send_get_star_payment_options_query();
    }

    /// `get_star_gift_payment_options` (StarManager.cpp:1578-1586).
    pub fn get_star_gift_payment_options(
        &mut self,
        env: &mut impl StarManagerEnv,
        user_id: UserId,
        mut promise: Promise<td_api::StarPaymentOptions>,
    ) {
        if user_id == UserId::default() {
            self.pending.star_payment_options.push(promise);
            env.send_get_star_gift_payment_options_query(None);
            return;
        }
        match env.get_input_user(user_id) {
            Result::Ok(input_user) => {
                self.pending.star_payment_options.push(promise);
                env.send_get_star_gift_payment_options_query(Some(input_user));
            }
            Result::Err(status) => {
                // the C++ TRY_RESULT_PROMISE routes the error to the
                // query promise
                promise.set_error(status);
            }
        }
    }

    /// `get_star_giveaway_payment_options` (StarManager.cpp:1588-1591).
    pub fn get_star_giveaway_payment_options(
        &mut self,
        env: &mut impl StarManagerEnv,
        promise: Promise<td_api::StarGiveawayPaymentOptions>,
    ) {
        self.pending.star_giveaway_payment_options.push(promise);
        env.send_get_star_giveaway_payment_options_query();
    }

    /// `get_star_transactions` (StarManager.cpp:1593-1612) — the
    /// validation order is the sender probe (done by the caller),
    /// `can_manage_stars`, the limit check, then the sticker-set load
    /// chain.
    #[allow(clippy::too_many_arguments)]
    pub fn get_star_transactions(
        &mut self,
        env: &mut impl StarManagerEnv,
        dialog_id: DialogId,
        subscription_id: String,
        offset: String,
        limit: i32,
        direction: Option<td_api::TransactionDirection>,
        mut promise: Promise<td_api::StarTransactions>,
    ) {
        if let Result::Err(status) = self.can_manage_stars(env, dialog_id, true) {
            promise.set_error(status);
            return;
        }
        if limit < 0 {
            promise.set_error(Status::error(400, "Limit must be non-negative"));
            return;
        }
        env.load_premium_gift_sticker_set();
        self.do_get_star_transactions(
            env,
            dialog_id,
            subscription_id,
            offset,
            limit,
            direction,
            promise,
        );
    }

    /// `do_get_star_transactions` (StarManager.cpp:1614-1622).
    #[allow(clippy::too_many_arguments)]
    fn do_get_star_transactions(
        &mut self,
        env: &mut impl StarManagerEnv,
        dialog_id: DialogId,
        subscription_id: String,
        offset: String,
        limit: i32,
        direction: Option<td_api::TransactionDirection>,
        mut promise: Promise<td_api::StarTransactions>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            promise.set_error(close_status);
            return;
        }
        if let Result::Err(status) = self.can_manage_stars(env, dialog_id, true) {
            promise.set_error(status);
            return;
        }
        self.pending.star_transactions.push(promise);
        env.send_get_star_transactions_query(dialog_id, subscription_id, offset, limit, direction);
    }

    /// `get_ton_transactions` (StarManager.cpp:1624-1640).
    pub fn get_ton_transactions(
        &mut self,
        env: &mut impl StarManagerEnv,
        offset: String,
        limit: i32,
        direction: Option<td_api::TransactionDirection>,
        mut promise: Promise<td_api::TonTransactions>,
    ) {
        if limit < 0 {
            promise.set_error(Status::error(400, "Limit must be non-negative"));
            return;
        }
        env.load_ton_gift_sticker_set();
        self.do_get_ton_transactions(env, offset, limit, direction, promise);
    }

    /// `do_get_ton_transactions` (StarManager.cpp:1642-1648).
    #[allow(clippy::too_many_arguments)]
    fn do_get_ton_transactions(
        &mut self,
        env: &mut impl StarManagerEnv,
        offset: String,
        limit: i32,
        direction: Option<td_api::TransactionDirection>,
        mut promise: Promise<td_api::TonTransactions>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            promise.set_error(close_status);
            return;
        }
        self.pending.ton_transactions.push(promise);
        env.send_get_ton_transactions_query(offset, limit, direction);
    }

    /// `get_star_subscriptions` (StarManager.cpp:1650-1653).
    pub fn get_star_subscriptions(
        &mut self,
        env: &mut impl StarManagerEnv,
        only_expiring: bool,
        offset: String,
        promise: Promise<td_api::StarSubscriptions>,
    ) {
        self.pending.star_subscriptions.push(promise);
        env.send_get_star_subscriptions_query(only_expiring, offset);
    }

    /// `edit_star_subscription` (StarManager.cpp:1655-1657).
    pub fn edit_star_subscription(
        &mut self,
        env: &mut impl StarManagerEnv,
        subscription_id: String,
        is_canceled: bool,
        promise: Promise<()>,
    ) {
        self.pending.units.push(promise);
        env.send_change_star_subscription_query(subscription_id, is_canceled);
    }

    /// `edit_user_star_subscription` (StarManager.cpp:1659-1664).
    pub fn edit_user_star_subscription(
        &mut self,
        env: &mut impl StarManagerEnv,
        user_id: UserId,
        telegram_payment_charge_id: String,
        is_canceled: bool,
        mut promise: Promise<()>,
    ) {
        match env.get_input_user(user_id) {
            Result::Ok(input_user) => {
                self.pending.units.push(promise);
                env.send_bot_cancel_star_subscription_query(
                    input_user,
                    telegram_payment_charge_id,
                    is_canceled,
                );
            }
            Result::Err(status) => {
                promise.set_error(status);
            }
        }
    }

    /// `reuse_star_subscription` (StarManager.cpp:1666-1668).
    pub fn reuse_star_subscription(
        &mut self,
        env: &mut impl StarManagerEnv,
        subscription_id: String,
        promise: Promise<()>,
    ) {
        self.pending.units.push(promise);
        env.send_fulfill_star_subscription_query(subscription_id);
    }

    /// `refund_star_payment` (StarManager.cpp:1670-1675).
    pub fn refund_star_payment(
        &mut self,
        env: &mut impl StarManagerEnv,
        user_id: UserId,
        telegram_payment_charge_id: String,
        mut promise: Promise<()>,
    ) {
        match env.get_input_user(user_id) {
            Result::Ok(input_user) => {
                self.pending.units.push(promise);
                env.send_refund_star_charge_query(input_user, telegram_payment_charge_id);
            }
            Result::Err(status) => {
                promise.set_error(status);
            }
        }
    }

    /// `get_star_revenue_statistics` (StarManager.cpp:1677-1682).
    pub fn get_star_revenue_statistics(
        &mut self,
        env: &mut impl StarManagerEnv,
        dialog_id: DialogId,
        is_dark: bool,
        mut promise: Promise<td_api::StarRevenueStatistics>,
    ) {
        if let Result::Err(status) = self.can_manage_stars(env, dialog_id, true) {
            promise.set_error(status);
            return;
        }
        self.pending.star_revenue_statistics.push(promise);
        env.send_get_star_revenue_stats_query(dialog_id, is_dark);
    }

    /// `get_withdrawal_url` (StarManager.cpp:1684-1699) — the empty
    /// password rejects before the SRP plane; the collapsed port sends
    /// the query right after the SRP seam call instead of chaining
    /// through the answer (see the module header).
    fn get_withdrawal_url(
        &mut self,
        env: &mut impl StarManagerEnv,
        dialog_id: DialogId,
        is_ton: bool,
        amount: i64,
        password: String,
        mut promise: Promise<String>,
    ) {
        if password.is_empty() {
            promise.set_error(Status::error(400, "PASSWORD_HASH_INVALID"));
            return;
        }
        env.get_input_check_password_srp(password, Promise::default());
        self.send_get_withdrawal_url_query(env, dialog_id, is_ton, amount, promise);
    }

    /// `send_get_withdrawal_url_query` (StarManager.cpp:1701-1708).
    fn send_get_withdrawal_url_query(
        &mut self,
        env: &mut impl StarManagerEnv,
        dialog_id: DialogId,
        is_ton: bool,
        amount: i64,
        mut promise: Promise<String>,
    ) {
        let close_status = env.close_status();
        if close_status.is_error() {
            promise.set_error(close_status);
            return;
        }
        self.pending.urls.push(promise);
        env.send_get_stars_revenue_withdrawal_url_query(
            dialog_id,
            is_ton,
            amount,
            telegram_api::InputCheckPasswordSRP::inputCheckPasswordEmpty(
                telegram_api::inputCheckPasswordEmpty {},
            ),
        );
    }

    /// `get_star_withdrawal_url` (StarManager.cpp:1710-1716).
    pub fn get_star_withdrawal_url(
        &mut self,
        env: &mut impl StarManagerEnv,
        dialog_id: DialogId,
        star_count: i64,
        password: String,
        mut promise: Promise<String>,
    ) {
        if let Result::Err(status) = self.can_manage_stars(env, dialog_id, true) {
            promise.set_error(status);
            return;
        }
        self.get_withdrawal_url(env, dialog_id, false, star_count, password, promise);
    }

    /// `get_ton_revenue_statistics` (StarManager.cpp:1718-1721).
    pub fn get_ton_revenue_statistics(
        &mut self,
        env: &mut impl StarManagerEnv,
        is_dark: bool,
        promise: Promise<td_api::GramRevenueStatistics>,
    ) {
        self.pending.gram_revenue_statistics.push(promise);
        env.send_get_gram_revenue_stats_query(is_dark);
    }

    /// `get_ton_withdrawal_url` (StarManager.cpp:1723-1725).
    pub fn get_ton_withdrawal_url(
        &mut self,
        env: &mut impl StarManagerEnv,
        password: String,
        promise: Promise<String>,
    ) {
        let my_dialog_id = env.my_dialog_id();
        self.get_withdrawal_url(env, my_dialog_id, true, 0, password, promise);
    }

    /// `get_star_ad_account_url` (StarManager.cpp:1727-1732).
    pub fn get_star_ad_account_url(
        &mut self,
        env: &mut impl StarManagerEnv,
        dialog_id: DialogId,
        mut promise: Promise<String>,
    ) {
        if let Result::Err(status) = self.can_manage_stars(env, dialog_id, false) {
            promise.set_error(status);
            return;
        }
        self.pending.urls.push(promise);
        env.send_get_stars_revenue_ads_account_url_query(dialog_id);
    }

    /// `get_paid_message_revenue` (StarManager.cpp:1734-1737).
    pub fn get_paid_message_revenue(
        &mut self,
        env: &mut impl StarManagerEnv,
        user_id: UserId,
        mut promise: Promise<td_api::StarCount>,
    ) {
        match env.get_input_user(user_id) {
            Result::Ok(input_user) => {
                self.pending.paid_message_revenue.push(promise);
                env.send_get_paid_message_revenue_query(input_user);
            }
            Result::Err(status) => {
                promise.set_error(status);
            }
        }
    }

    /// `reload_star_transaction` (StarManager.cpp:1739-1751) — the
    /// by-id form of the transactions query (the file-reference
    /// reload entry point); the caller's `Unit` promise rides the
    /// `StarTransactions` answer through the wrapper.
    pub fn reload_star_transaction(
        &mut self,
        env: &mut impl StarManagerEnv,
        dialog_id: DialogId,
        transaction_id: String,
        is_refund: bool,
        mut promise: Promise<()>,
    ) {
        if let Result::Err(status) = self.can_manage_stars(env, dialog_id, true) {
            promise.set_error(status);
            return;
        }
        self.pending.star_transactions.push(Promise::from_interface(
            ReloadStarTransactionPromise {
                promise: Some(promise),
            },
        ));
        env.send_get_star_transactions_by_id_query(dialog_id, transaction_id, is_refund);
    }

    /// `reload_owned_star_count` (StarManager.cpp:1753-1755) — the
    /// one-entry own-dialog transactions query with the empty promise
    /// (the C++ `Auto()`; the balance update rides the answer intake).
    pub fn reload_owned_star_count(&mut self, env: &mut impl StarManagerEnv) {
        let my_dialog_id = env.my_dialog_id();
        self.do_get_star_transactions(
            env,
            my_dialog_id,
            String::new(),
            String::new(),
            1,
            None,
            Promise::default(),
        );
    }

    /// `reload_owned_ton_count` (StarManager.cpp:1757-1759).
    pub fn reload_owned_ton_count(&mut self, env: &mut impl StarManagerEnv) {
        self.do_get_ton_transactions(env, String::new(), 1, None, Promise::default());
    }

    /// `StarManager::get_current_state` (StarManager.cpp:1948-1955).
    pub fn get_current_state(&self, updates: &mut Vec<td_api::Update>) {
        if self.is_owned_star_count_inited {
            updates.push(self.get_update_owned_star_count_object());
        }
        if self.is_owned_ton_count_inited {
            updates.push(self.get_update_owned_gram_count_object());
        }
    }
}

/// The `reload_star_transaction` promise wrapper
/// (StarManager.cpp:1742-1749) — the `Unit` promise behind the
/// `StarTransactions` answer (an error forwards, an ok drops the value).
struct ReloadStarTransactionPromise {
    promise: Option<Promise<()>>,
}

impl tdutils::promise::PromiseInterface<td_api::StarTransactions> for ReloadStarTransactionPromise {
    fn set_value(&mut self, _value: td_api::StarTransactions) {
        if let Some(mut promise) = self.promise.take() {
            promise.set_value(());
        }
    }

    fn set_error(&mut self, error: Status) {
        if let Some(mut promise) = self.promise.take() {
            promise.set_error(error);
        }
    }
}

/// `StarManager::get_unused_star_transaction_field`
/// (StarManager.cpp:1844-1945) — the leftover-field audit over a
/// converted transaction; the caller passes its consumed copy (the
/// conversion clears each field it consumes, like the C++ in-place
/// mutation, so a leftover names itself).
pub(crate) fn get_unused_star_transaction_field(
    transaction: &telegram_api::StarsTransaction,
    product_info: Option<&td_api::ProductInfo>,
    bot_payload: &str,
    affiliate: Option<&td_api::AffiliateInfo>,
    commission_per_mille: i32,
) -> &'static str {
    if product_info.is_some() {
        return "product info";
    }
    if !bot_payload.is_empty() {
        return "bot payload";
    }
    if affiliate.is_some() {
        return "affiliate";
    }
    if commission_per_mille != 0 {
        return "commission";
    }
    if !transaction.title.is_empty() {
        return "title";
    }
    if !transaction.description.is_empty() {
        return "description";
    }
    if transaction.photo.is_some() {
        return "photo";
    }
    if !transaction.bot_payload.is_empty() {
        return "bot payload";
    }
    if transaction.transaction_date != 0
        || !transaction.transaction_url.is_empty()
        || transaction.pending
        || transaction.failed
    {
        return "withdrawal state";
    }
    if transaction.msg_id != 0 {
        return "message identifier";
    }
    if transaction.gift {
        return "gift";
    }
    if transaction.subscription_period != 0 {
        return "subscription period";
    }
    if transaction.reaction {
        return "reaction";
    }
    if !transaction.extended_media.is_empty() {
        return "paid media";
    }
    if transaction.giveaway_post_id != 0 {
        return "giveaway message";
    }
    if transaction.stargift.is_some() {
        return "gift";
    }
    if transaction.floodskip_number != 0 {
        return "API payment";
    }
    if transaction.starref_peer.is_some() {
        return "affiliate info";
    }
    if transaction.starref_amount.is_some() {
        return "affiliate payment info";
    }
    if transaction.starref_commission_permille != 0 {
        return "affiliate commission info";
    }
    if transaction.stargift_upgrade {
        return "gift upgrade";
    }
    if transaction.paid_messages != 0 {
        return "paid messages";
    }
    if transaction.premium_gift_months != 0 {
        return "Telegram Premium purchase";
    }
    if transaction.business_transfer {
        return "business bot transfer";
    }
    if transaction.stargift_resale {
        return "gift resale";
    }
    if transaction.ads_proceeds_from_date != 0 || transaction.ads_proceeds_to_date != 0 {
        return "ads proceeds";
    }
    if transaction.posts_search {
        return "post search";
    }
    if transaction.stargift_prepaid_upgrade {
        return "prepaid gift upgrade";
    }
    if transaction.stargift_drop_original_details {
        return "gift original details drop";
    }
    if transaction.phonegroup_message {
        return "live story message";
    }
    if transaction.stargift_auction_bid {
        return "gift auction bid";
    }
    if transaction.offer {
        return "gift purchase offer";
    }
    ""
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tdutils::promise::PromiseInterface;

    struct Recorded<T> {
        inner: Arc<Mutex<Option<Result<T>>>>,
    }

    struct RecordingPromise<T: Send + 'static> {
        recorded: Recorded<T>,
    }

    impl<T: Send + 'static> PromiseInterface<T> for RecordingPromise<T> {
        fn set_value(&mut self, value: T) {
            *self.recorded.inner.lock().unwrap() = Some(Result::Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            *self.recorded.inner.lock().unwrap() = Some(Result::Err(error));
        }
    }

    #[allow(clippy::type_complexity)]
    fn recording_promise<T: Send + 'static>() -> (Promise<T>, Arc<Mutex<Option<Result<T>>>>) {
        let recorded = Arc::new(Mutex::new(None));
        (
            Promise::from_interface(RecordingPromise {
                recorded: Recorded {
                    inner: recorded.clone(),
                },
            }),
            recorded,
        )
    }

    #[derive(Clone)]
    struct TestBot {
        can_be_edited: bool,
    }

    #[derive(Default)]
    struct TestEnv {
        is_authorized: bool,
        is_bot: bool,
        my_id: i64,
        bots: HashMap<i64, TestBot>,
        channel_creator: bool,
        pmc: HashMap<String, String>,
        updates: Vec<td_api::Update>,
        star_payment_options_seam: usize,
        star_gift_payment_options_seam: usize,
        star_giveaway_payment_options_seam: usize,
        star_transactions_seam: usize,
        ton_transactions_seam: usize,
        star_transactions_by_id_seam: usize,
        star_subscriptions_seam: usize,
        change_star_subscription_seam: usize,
        bot_cancel_star_subscription_seam: usize,
        fulfill_star_subscription_seam: usize,
        refund_star_charge_seam: usize,
        star_revenue_stats_seam: usize,
        withdrawal_url_seam: usize,
        ads_account_url_seam: usize,
        gram_revenue_stats_seam: usize,
        paid_message_revenue_seam: usize,
        srp_seam: usize,
        premium_gift_sticker_set_loads: usize,
        ton_gift_sticker_set_loads: usize,
    }

    impl TestEnv {
        fn authorized() -> Self {
            TestEnv {
                is_authorized: true,
                my_id: 7,
                ..TestEnv::default()
            }
        }
    }

    impl StarManagerEnv for TestEnv {
        fn is_authorized(&self) -> bool {
            self.is_authorized
        }

        fn is_bot(&self) -> bool {
            self.is_bot
        }

        fn close_status(&self) -> Status {
            tdutils::status::Status::ok()
        }

        fn my_user_id(&self) -> UserId {
            UserId::new(self.my_id)
        }

        fn my_dialog_id(&self) -> DialogId {
            DialogId::new(self.my_id)
        }

        fn get_bot_data(&mut self, user_id: UserId) -> Result<BotData> {
            match self.bots.get(&user_id.get()) {
                Some(bot) => Result::Ok(BotData {
                    username: "bot".into(),
                    can_be_edited: bot.can_be_edited,
                    can_join_groups: false,
                    can_read_all_group_messages: false,
                    has_main_app: false,
                    has_bot_forum_view: false,
                    can_bot_create_topics: false,
                    can_manage_bots: false,
                    is_inline: false,
                    is_guestchat: false,
                    is_business: false,
                    is_guard: false,
                    need_location: false,
                    can_be_added_to_attach_menu: false,
                }),
                None => Result::from_status(Status::error(400, "Bot not found")),
            }
        }

        fn get_channel_status(
            &mut self,
            _channel_id: crate::chat_manager::ChannelId,
        ) -> crate::chat_manager::DialogParticipantStatus {
            if self.channel_creator {
                crate::chat_manager::DialogParticipantStatus::creator(false, false, "Owner".into())
            } else {
                crate::chat_manager::DialogParticipantStatus::member(0, "Member".into())
            }
        }

        fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser> {
            if self.bots.contains_key(&user_id.get()) || user_id.get() == self.my_id {
                return Result::Ok(telegram_api::InputUser::inputUser(
                    telegram_api::inputUser {
                        user_id: user_id.get(),
                        access_hash: 0,
                    },
                ));
            }
            Result::from_status(Status::error(400, "Invalid user_id specified"))
        }

        fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.pmc
                .get(&String::from_utf8_lossy(key).into_owned())
                .cloned()
                .unwrap_or_default()
                .into_bytes()
        }

        fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.pmc.insert(
                String::from_utf8_lossy(key).into_owned(),
                String::from_utf8_lossy(value).into_owned(),
            );
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn load_premium_gift_sticker_set(&mut self) {
            self.premium_gift_sticker_set_loads += 1;
        }

        fn load_ton_gift_sticker_set(&mut self) {
            self.ton_gift_sticker_set_loads += 1;
        }

        fn get_input_check_password_srp(
            &mut self,
            _password: String,
            _promise: Promise<telegram_api::InputCheckPasswordSRP>,
        ) {
            self.srp_seam += 1;
        }

        fn send_get_star_payment_options_query(&mut self) {
            self.star_payment_options_seam += 1;
        }

        fn send_get_star_gift_payment_options_query(
            &mut self,
            _input_user: Option<telegram_api::InputUser>,
        ) {
            self.star_gift_payment_options_seam += 1;
        }

        fn send_get_star_giveaway_payment_options_query(&mut self) {
            self.star_giveaway_payment_options_seam += 1;
        }

        fn send_get_star_transactions_query(
            &mut self,
            _dialog_id: DialogId,
            _subscription_id: String,
            _offset: String,
            _limit: i32,
            _direction: Option<td_api::TransactionDirection>,
        ) {
            self.star_transactions_seam += 1;
        }

        fn send_get_star_transactions_by_id_query(
            &mut self,
            _dialog_id: DialogId,
            _transaction_id: String,
            _is_refund: bool,
        ) {
            self.star_transactions_by_id_seam += 1;
        }

        fn send_get_ton_transactions_query(
            &mut self,
            _offset: String,
            _limit: i32,
            _direction: Option<td_api::TransactionDirection>,
        ) {
            self.ton_transactions_seam += 1;
        }

        fn send_get_star_subscriptions_query(&mut self, _only_expiring: bool, _offset: String) {
            self.star_subscriptions_seam += 1;
        }

        fn send_change_star_subscription_query(
            &mut self,
            _subscription_id: String,
            _is_canceled: bool,
        ) {
            self.change_star_subscription_seam += 1;
        }

        fn send_bot_cancel_star_subscription_query(
            &mut self,
            _input_user: telegram_api::InputUser,
            _telegram_payment_charge_id: String,
            _is_canceled: bool,
        ) {
            self.bot_cancel_star_subscription_seam += 1;
        }

        fn send_fulfill_star_subscription_query(&mut self, _subscription_id: String) {
            self.fulfill_star_subscription_seam += 1;
        }

        fn send_refund_star_charge_query(
            &mut self,
            _input_user: telegram_api::InputUser,
            _telegram_payment_charge_id: String,
        ) {
            self.refund_star_charge_seam += 1;
        }

        fn send_get_star_revenue_stats_query(&mut self, _dialog_id: DialogId, _is_dark: bool) {
            self.star_revenue_stats_seam += 1;
        }

        fn send_get_stars_revenue_withdrawal_url_query(
            &mut self,
            _dialog_id: DialogId,
            _is_ton: bool,
            _amount: i64,
            _input_check_password: telegram_api::InputCheckPasswordSRP,
        ) {
            self.withdrawal_url_seam += 1;
        }

        fn send_get_stars_revenue_ads_account_url_query(&mut self, _dialog_id: DialogId) {
            self.ads_account_url_seam += 1;
        }

        fn send_get_gram_revenue_stats_query(&mut self, _is_dark: bool) {
            self.gram_revenue_stats_seam += 1;
        }

        fn send_get_paid_message_revenue_query(&mut self, _input_user: telegram_api::InputUser) {
            self.paid_message_revenue_seam += 1;
        }
    }

    #[test]
    fn get_star_count_clamps_to_51_bits() {
        let max = 1i64 << 51;
        assert_eq!(get_star_count(100, false), 100);
        assert_eq!(get_star_count(-5, false), 0);
        assert_eq!(get_star_count(-5, true), -5);
        assert_eq!(get_star_count(-max - 1, true), -max);
        assert_eq!(get_star_count(max + 1, false), max);
    }

    #[test]
    fn get_nanostar_count_normalizes_signs() {
        let mut star_count = -1i64;
        assert_eq!(
            get_nanostar_count(&mut star_count, 500_000_000),
            -500_000_000
        );
        assert_eq!(star_count, 0);

        let mut star_count = 1i64;
        assert_eq!(
            get_nanostar_count(&mut star_count, -500_000_000),
            500_000_000
        );
        assert_eq!(star_count, 0);

        let mut star_count = 5i64;
        assert_eq!(
            get_nanostar_count(&mut star_count, 250_000_000),
            250_000_000
        );
        assert_eq!(star_count, 5);

        let mut star_count = 1i64;
        assert_eq!(
            get_nanostar_count(&mut star_count, 2_000_000_000),
            2_000_000_000
        );
    }

    #[test]
    fn get_months_by_star_count_boundaries() {
        assert_eq!(get_months_by_star_count(0), 3);
        assert_eq!(get_months_by_star_count(1000), 3);
        assert_eq!(get_months_by_star_count(1001), 6);
        assert_eq!(get_months_by_star_count(2499), 6);
        assert_eq!(get_months_by_star_count(2500), 12);
    }

    #[test]
    fn start_up_loads_the_pmc_counts_and_emits_the_updates() {
        let mut env = TestEnv::authorized();
        env.pmc.insert("owned_star_count".into(), "123 456".into());
        env.pmc.insert("owned_ton_count".into(), "789".into());
        let mut manager = StarManager::new();
        manager.start_up(&mut env);
        assert_eq!(env.updates.len(), 2);
        assert!(matches!(
            env.updates[0],
            td_api::Update::updateOwnedStarCount(_)
        ));
        assert!(matches!(
            env.updates[1],
            td_api::Update::updateOwnedGramCount(_)
        ));
        assert!(manager.has_owned_star_count(100));
        assert!(!manager.has_owned_star_count(124));
        assert!(manager.has_owned_ton_count(789));
        assert!(!manager.has_owned_ton_count(790));
    }

    #[test]
    fn start_up_skips_bots_and_unauthorized() {
        let mut manager = StarManager::new();

        let mut env = TestEnv::authorized();
        env.is_authorized = false;
        manager.start_up(&mut env);
        assert_eq!(env.updates.len(), 0);

        let mut env = TestEnv::authorized();
        env.is_bot = true;
        manager.start_up(&mut env);
        assert_eq!(env.updates.len(), 0);
    }

    #[test]
    fn on_update_owned_star_amount_persists_and_gates_bots() {
        let mut env = TestEnv::authorized();
        let mut manager = StarManager::new();

        manager.on_update_owned_star_amount(
            &mut env,
            StarAmount {
                star_count: 50,
                nanostar_count: 25,
            },
        );
        assert_eq!(env.updates.len(), 1);
        assert_eq!(
            env.pmc.get("owned_star_count").map(String::as_str),
            Some("50 25")
        );

        // the same amount again — no update, no pmc write change
        manager.on_update_owned_star_amount(
            &mut env,
            StarAmount {
                star_count: 50,
                nanostar_count: 25,
            },
        );
        assert_eq!(env.updates.len(), 1);

        // a bot never records the balance
        let mut bot_env = TestEnv::authorized();
        bot_env.is_bot = true;
        manager.on_update_owned_ton_amount(&mut bot_env, TonAmount { ton_amount: 5 });
        assert_eq!(bot_env.updates.len(), 0);
        assert!(!bot_env.pmc.contains_key("owned_ton_count"));
        // the count stays uninited, so the check stays permissive
        assert!(manager.has_owned_ton_count(5));
    }

    #[test]
    fn add_pending_owned_star_count_moves_or_updates_sent() {
        let mut env = TestEnv::authorized();
        let mut manager = StarManager::new();
        manager.on_update_owned_star_amount(
            &mut env,
            StarAmount {
                star_count: 100,
                nanostar_count: 0,
            },
        );
        assert_eq!(env.updates.len(), 1);

        // pending without move — the sent count grows, an update goes out
        manager.add_pending_owned_star_count(&mut env, -30, false);
        assert_eq!(env.updates.len(), 2);
        assert_eq!(manager.owned_star_count, 100);
        assert_eq!(manager.sent_star_count, 70);
        assert!(manager.has_owned_star_count(70));
        assert!(!manager.has_owned_star_count(71));

        // pending with move — the owned count moves, no update
        manager.add_pending_owned_star_count(&mut env, -20, true);
        assert_eq!(env.updates.len(), 2);
        assert_eq!(manager.owned_star_count, 120);
        assert_eq!(
            env.pmc.get("owned_star_count").map(String::as_str),
            Some("120 0")
        );
    }

    #[test]
    fn can_manage_stars_decision_table() {
        let mut env = TestEnv::authorized();
        env.bots.insert(
            42,
            TestBot {
                can_be_edited: true,
            },
        );
        env.bots.insert(
            43,
            TestBot {
                can_be_edited: false,
            },
        );
        let manager = StarManager::new();

        // the own user dialog is allowed with allow_self
        assert!(matches!(
            manager.can_manage_stars(&mut env, DialogId::new(7), true),
            Result::Ok(())
        ));
        // the own user dialog without allow_self — not a bot
        assert!(matches!(
            manager.can_manage_stars(&mut env, DialogId::new(7), false),
            Result::Err(_)
        ));
        // an owned bot
        assert!(matches!(
            manager.can_manage_stars(&mut env, DialogId::new(42), false),
            Result::Ok(())
        ));
        // a foreign bot
        assert!(matches!(
            manager.can_manage_stars(&mut env, DialogId::new(43), false),
            Result::Err(_)
        ));
        // an unknown user
        assert!(matches!(
            manager.can_manage_stars(&mut env, DialogId::new(1000), true),
            Result::Err(_)
        ));
        // a creator channel
        env.channel_creator = true;
        assert!(matches!(
            manager.can_manage_stars(&mut env, DialogId::new(-1_000_000_000_001), true),
            Result::Ok(())
        ));
        // a non-creator channel rejects without allow_self
        env.channel_creator = false;
        assert!(matches!(
            manager.can_manage_stars(&mut env, DialogId::new(-1_000_000_000_001), false),
            Result::Err(_)
        ));
        // but allow_self passes
        assert!(matches!(
            manager.can_manage_stars(&mut env, DialogId::new(-1_000_000_000_001), true),
            Result::Ok(())
        ));
        // a chat is never allowed
        assert!(matches!(
            manager.can_manage_stars(&mut env, DialogId::new(-42), true),
            Result::Err(_)
        ));
    }

    #[test]
    fn payment_option_arms_park_and_flag_the_seams() {
        let mut env = TestEnv::authorized();
        let mut manager = StarManager::new();

        let (promise, _) = recording_promise();
        manager.get_star_payment_options(&mut env, promise);
        assert_eq!(env.star_payment_options_seam, 1);
        assert_eq!(manager.pending.star_payment_options.len(), 1);

        let (promise, _) = recording_promise();
        manager.get_star_gift_payment_options(&mut env, UserId::default(), promise);
        assert_eq!(env.star_gift_payment_options_seam, 1);
        assert_eq!(manager.pending.star_payment_options.len(), 2);

        let (promise, recorded) = recording_promise();
        manager.get_star_gift_payment_options(&mut env, UserId::new(999), promise);
        assert_eq!(env.star_gift_payment_options_seam, 1);
        assert_eq!(manager.pending.star_payment_options.len(), 2);
        assert!(matches!(&*recorded.lock().unwrap(), Some(Result::Err(_))));

        let (promise, _) = recording_promise();
        manager.get_star_giveaway_payment_options(&mut env, promise);
        assert_eq!(env.star_giveaway_payment_options_seam, 1);
        assert_eq!(manager.pending.star_giveaway_payment_options.len(), 1);
    }

    #[test]
    fn transaction_arms_validate_then_park() {
        let mut env = TestEnv::authorized();
        let mut manager = StarManager::new();

        // a negative limit rejects before the seam
        let (promise, recorded) = recording_promise();
        manager.get_star_transactions(
            &mut env,
            DialogId::new(7),
            String::new(),
            String::new(),
            -1,
            None,
            promise,
        );
        assert_eq!(env.star_transactions_seam, 0);
        assert!(matches!(&*recorded.lock().unwrap(), Some(Result::Err(_))));

        // the own dialog passes and parks through the sticker-set load
        let (promise, _) = recording_promise();
        manager.get_star_transactions(
            &mut env,
            DialogId::new(7),
            "sub".into(),
            "off".into(),
            10,
            None,
            promise,
        );
        assert_eq!(env.premium_gift_sticker_set_loads, 1);
        assert_eq!(env.star_transactions_seam, 1);
        assert_eq!(manager.pending.star_transactions.len(), 1);

        // the ton leg
        let (promise, _) = recording_promise();
        manager.get_ton_transactions(&mut env, String::new(), 5, None, promise);
        assert_eq!(env.ton_gift_sticker_set_loads, 1);
        assert_eq!(env.ton_transactions_seam, 1);
        assert_eq!(manager.pending.ton_transactions.len(), 1);

        let (promise, _) = recording_promise();
        manager.get_star_subscriptions(&mut env, true, "off".into(), promise);
        assert_eq!(env.star_subscriptions_seam, 1);
        assert_eq!(manager.pending.star_subscriptions.len(), 1);
    }

    #[test]
    fn subscription_mutation_arms_park_units_or_error() {
        let mut env = TestEnv::authorized();
        let mut manager = StarManager::new();

        let (promise, _) = recording_promise();
        manager.edit_star_subscription(&mut env, "sub".into(), true, promise);
        assert_eq!(env.change_star_subscription_seam, 1);
        assert_eq!(manager.pending.units.len(), 1);

        let (promise, _) = recording_promise();
        manager.reuse_star_subscription(&mut env, "sub".into(), promise);
        assert_eq!(env.fulfill_star_subscription_seam, 1);
        assert_eq!(manager.pending.units.len(), 2);

        // an invalid user errors the promise without the seam
        let (promise, recorded) = recording_promise();
        manager.edit_user_star_subscription(
            &mut env,
            UserId::new(999),
            "charge".into(),
            true,
            promise,
        );
        assert_eq!(env.bot_cancel_star_subscription_seam, 0);
        assert!(matches!(&*recorded.lock().unwrap(), Some(Result::Err(_))));

        let (promise, _) = recording_promise();
        manager.edit_user_star_subscription(
            &mut env,
            UserId::new(7),
            "charge".into(),
            true,
            promise,
        );
        assert_eq!(env.bot_cancel_star_subscription_seam, 1);
        assert_eq!(manager.pending.units.len(), 3);

        let (promise, _) = recording_promise();
        manager.refund_star_payment(&mut env, UserId::new(7), "charge".into(), promise);
        assert_eq!(env.refund_star_charge_seam, 1);
        assert_eq!(manager.pending.units.len(), 4);
    }

    #[test]
    fn revenue_and_url_arms_honor_can_manage_and_password() {
        let mut env = TestEnv::authorized();
        env.channel_creator = true;
        let mut manager = StarManager::new();
        let channel_dialog = DialogId::new(-1_000_000_000_001);

        let (promise, _) = recording_promise();
        manager.get_star_revenue_statistics(&mut env, channel_dialog, true, promise);
        assert_eq!(env.star_revenue_stats_seam, 1);
        assert_eq!(manager.pending.star_revenue_statistics.len(), 1);

        // an empty password rejects with PASSWORD_HASH_INVALID
        let (promise, recorded) = recording_promise();
        manager.get_star_withdrawal_url(&mut env, channel_dialog, 100, String::new(), promise);
        assert_eq!(env.srp_seam, 0);
        assert_eq!(env.withdrawal_url_seam, 0);
        match &*recorded.lock().unwrap() {
            Some(Result::Err(status)) => assert_eq!(status.code(), 400),
            _ => panic!("expected the password error"),
        }

        // a real password rides the SRP seam then the query seam
        let (promise, _) = recording_promise();
        manager.get_star_withdrawal_url(&mut env, channel_dialog, 100, "pw".into(), promise);
        assert_eq!(env.srp_seam, 1);
        assert_eq!(env.withdrawal_url_seam, 1);
        assert_eq!(manager.pending.urls.len(), 1);

        // the TON withdrawal targets the own dialog
        let (promise, _) = recording_promise();
        manager.get_ton_withdrawal_url(&mut env, "pw".into(), promise);
        assert_eq!(env.srp_seam, 2);
        assert_eq!(env.withdrawal_url_seam, 2);
        assert_eq!(manager.pending.urls.len(), 2);

        // the ads account url needs creator rights (allow_self is false)
        let (promise, _) = recording_promise();
        manager.get_star_ad_account_url(&mut env, channel_dialog, promise);
        assert_eq!(env.ads_account_url_seam, 1);
        assert_eq!(manager.pending.urls.len(), 3);

        env.channel_creator = false;
        let (promise, recorded) = recording_promise();
        manager.get_star_ad_account_url(&mut env, channel_dialog, promise);
        assert_eq!(env.ads_account_url_seam, 1);
        assert!(matches!(&*recorded.lock().unwrap(), Some(Result::Err(_))));

        let (promise, _) = recording_promise();
        manager.get_ton_revenue_statistics(&mut env, true, promise);
        assert_eq!(env.gram_revenue_stats_seam, 1);
        assert_eq!(manager.pending.gram_revenue_statistics.len(), 1);

        let (promise, _) = recording_promise();
        manager.get_paid_message_revenue(&mut env, UserId::new(7), promise);
        assert_eq!(env.paid_message_revenue_seam, 1);
        assert_eq!(manager.pending.paid_message_revenue.len(), 1);
    }

    #[test]
    fn get_current_state_streams_only_inited_counts() {
        let mut env = TestEnv::authorized();
        let mut manager = StarManager::new();

        let mut updates = Vec::new();
        manager.get_current_state(&mut updates);
        assert!(updates.is_empty());

        manager.on_update_owned_star_amount(
            &mut env,
            StarAmount {
                star_count: 1,
                nanostar_count: 2,
            },
        );
        manager.get_current_state(&mut updates);
        assert_eq!(updates.len(), 1);

        manager.on_update_owned_ton_amount(&mut env, TonAmount { ton_amount: 3 });
        manager.get_current_state(&mut updates);
        assert_eq!(updates.len(), 3);
    }

    #[test]
    fn reload_star_transaction_validates_then_rides_the_by_id_seam() {
        let mut env = TestEnv::authorized();
        let mut manager = StarManager::new();

        // a chat dialog rejects before the seam (the allow_self flag of
        // the reload covers users and channels, never chats — cpp:1548)
        let (promise, recorded) = recording_promise();
        manager.reload_star_transaction(&mut env, DialogId::new(-42), "tx".into(), false, promise);
        assert_eq!(env.star_transactions_by_id_seam, 0);
        assert!(matches!(&*recorded.lock().unwrap(), Some(Result::Err(_))));

        // the own dialog rides the by-id seam; the Unit promise parks
        // behind the StarTransactions wrapper
        let (promise, recorded) = recording_promise();
        manager.reload_star_transaction(&mut env, DialogId::new(7), "tx".into(), true, promise);
        assert_eq!(env.star_transactions_by_id_seam, 1);
        assert_eq!(manager.pending.star_transactions.len(), 1);

        // the wrapper forwards the error half; the ok half drops the
        // value (cpp:1742-1749)
        let mut parked = manager.pending.star_transactions.remove(0);
        parked.set_error(Status::error(400, "STARS_BALANCE_INVALID"));
        drop(parked);
        match &*recorded.lock().unwrap() {
            Some(Result::Err(status)) => assert_eq!(status.code(), 400),
            _ => panic!("expected the forwarded query error"),
        }

        let (promise, recorded) = recording_promise();
        manager.reload_star_transaction(&mut env, DialogId::new(7), "tx".into(), false, promise);
        let mut parked = manager.pending.star_transactions.remove(0);
        parked.set_value(td_api::StarTransactions::default());
        drop(parked);
        assert!(matches!(&*recorded.lock().unwrap(), Some(Result::Ok(()))));
    }

    #[test]
    fn reload_owned_counts_ride_the_list_seams_with_empty_promises() {
        let mut env = TestEnv::authorized();
        let mut manager = StarManager::new();

        manager.reload_owned_star_count(&mut env);
        assert_eq!(env.premium_gift_sticker_set_loads, 0); // the reload skips the load chain
        assert_eq!(env.star_transactions_seam, 1);
        assert_eq!(manager.pending.star_transactions.len(), 1);

        manager.reload_owned_ton_count(&mut env);
        assert_eq!(env.ton_transactions_seam, 1);
        assert_eq!(manager.pending.ton_transactions.len(), 1);

        // the empty promises swallow an answer without a panic
        let mut parked = manager.pending.star_transactions.remove(0);
        parked.set_value(td_api::StarTransactions::default());
        let mut parked = manager.pending.ton_transactions.remove(0);
        parked.set_value(td_api::TonTransactions::default());
    }

    #[test]
    fn get_unused_star_transaction_field_audits_leftovers() {
        let transaction = telegram_api::StarsTransaction::default();
        assert_eq!(
            get_unused_star_transaction_field(&transaction, None, "", None, 0),
            ""
        );

        let mut transaction = telegram_api::StarsTransaction {
            title: "Suggested Post".into(),
            ..telegram_api::StarsTransaction::default()
        };
        assert_eq!(
            get_unused_star_transaction_field(&transaction, None, "", None, 0),
            "title"
        );
        transaction.title.clear();

        // the converted halves shadow the wire ones (cpp:1848-1859)
        assert_eq!(
            get_unused_star_transaction_field(
                &transaction,
                Some(&td_api::ProductInfo::default()),
                "",
                None,
                0
            ),
            "product info"
        );
        assert_eq!(
            get_unused_star_transaction_field(&transaction, None, "payload", None, 0),
            "bot payload"
        );
        let affiliate = td_api::AffiliateInfo::default();
        assert_eq!(
            get_unused_star_transaction_field(&transaction, None, "", Some(&affiliate), 0),
            "affiliate"
        );
        assert_eq!(
            get_unused_star_transaction_field(&transaction, None, "", None, 77),
            "commission"
        );

        // the deep wire bands (cpp:1872-1944)
        transaction.pending = true;
        assert_eq!(
            get_unused_star_transaction_field(&transaction, None, "", None, 0),
            "withdrawal state"
        );
    }
}
