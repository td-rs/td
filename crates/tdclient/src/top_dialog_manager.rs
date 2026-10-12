//! Port of `td/telegram/TopDialogManager.{h,cpp}` (M.27) plus the model
//! header that only serves it, `TopDialogCategory.{h,cpp}`.
//!
//! The manager is the collapsed actor: [`Td`](crate::td::Td) owns the
//! struct, the [`TopDialogManagerEnv`] trait carries the
//! `G()`/option/pmc/cross-manager surfaces, and every `create_handler<...>`
//! network tail is an env seam marked `[blocked: net]` — the C++ promise
//! lands in the query and stays unanswered offline, exactly like the
//! collapsed-line convention. The `StateManager::wait_first_sync` hop and
//! the `MessagesManager::load_dialogs` hop are the same kind of seam (the
//! first waits for the net-backed state machine, the second is the M.10
//! dialog plane), so `TopDialogManager::on_first_sync` and
//! `TopDialogManager::on_load_dialogs`, like the query intakes, stay
//! directly callable — and unit-tested — as the net-independent halves.
//!
//! The daily rating decay keeps the C++ recorded-time shape: every clock
//! read (`Timestamp::now_cached`, `G()->server_time`, `Clocks::system`)
//! arrives through the env, never the wall clock, so tests drive the sync
//! machine and the decay arithmetic with recorded times.
//!
//! Local-tree quirk kept byte-faithful: the pinned td_api scheme has no
//! `topChatCategoryForwardUsers`, and the C++ maps
//! `topChatCategoryForwardChats` onto [`TopDialogCategory::ForwardUsers`]
//! — the bucket that then merges with ForwardChats in
//! `TopDialogManager::do_get_top_dialogs`.

use crate::dialog_id::DialogId;
use crate::store_parse::{Parser, store_f64, store_i32};
use crate::td_api;
use crate::user_manager::{BotData, UserId};
use std::cmp::Ordering;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;
use tdutils::time::Timestamp;

/// `TopDialogCategory` (TopDialogCategory.h) — the top-peer category
/// table. `Size` is the C++ sentinel (`static_cast<size_t>` position
/// bound, never a real category).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TopDialogCategory {
    Correspondent,
    BotPM,
    BotInline,
    Group,
    Channel,
    Call,
    ForwardUsers,
    ForwardChats,
    BotApp,
    BotGuest,
    Size,
}

/// `static_cast<size_t>(TopDialogCategory::Size)` — the `by_category_`
/// array bound.
pub const TOP_DIALOG_CATEGORY_SIZE: usize = 10;

impl TopDialogCategory {
    /// `static_cast<size_t>(category)` — the `by_category_` index. `Size`
    /// maps to the array bound, like the C++ cast; callers CHECK before
    /// indexing.
    pub fn position(self) -> usize {
        self as usize
    }
}

/// `get_top_dialog_category_name` (TopDialogCategory.cpp:15-42) — the
/// binlog-pmc key suffixes (`top_dialogs#<name>`).
pub fn get_top_dialog_category_name(category: TopDialogCategory) -> &'static str {
    match category {
        TopDialogCategory::Correspondent => "correspondent",
        TopDialogCategory::BotPM => "bot_pm",
        TopDialogCategory::BotInline => "bot_inline",
        TopDialogCategory::Group => "group",
        TopDialogCategory::Channel => "channel",
        TopDialogCategory::Call => "call",
        TopDialogCategory::ForwardUsers => "forward_users",
        TopDialogCategory::ForwardChats => "forward_chats",
        TopDialogCategory::BotApp => "bot_app",
        TopDialogCategory::BotGuest => "bot_guest",
        TopDialogCategory::Size => unreachable!("get_top_dialog_category_name(Size)"),
    }
}

/// `get_top_dialog_category(const td_api::TopChatCategory &)` — the null
/// pointer maps to `Size` ("must be non-empty" for the request arms); the
/// local tree maps `topChatCategoryForwardChats` onto `ForwardUsers`.
pub fn get_top_dialog_category_from_td_api(
    category: Option<&td_api::TopChatCategory>,
) -> TopDialogCategory {
    let Some(category) = category else {
        return TopDialogCategory::Size;
    };
    match category {
        td_api::TopChatCategory::topChatCategoryUsers(_) => TopDialogCategory::Correspondent,
        td_api::TopChatCategory::topChatCategoryBots(_) => TopDialogCategory::BotPM,
        td_api::TopChatCategory::topChatCategoryInlineBots(_) => TopDialogCategory::BotInline,
        td_api::TopChatCategory::topChatCategoryGroups(_) => TopDialogCategory::Group,
        td_api::TopChatCategory::topChatCategoryChannels(_) => TopDialogCategory::Channel,
        td_api::TopChatCategory::topChatCategoryCalls(_) => TopDialogCategory::Call,
        td_api::TopChatCategory::topChatCategoryForwardChats(_) => TopDialogCategory::ForwardUsers,
        td_api::TopChatCategory::topChatCategoryWebAppBots(_) => TopDialogCategory::BotApp,
        td_api::TopChatCategory::topChatCategoryGuestBots(_) => TopDialogCategory::BotGuest,
    }
}

/// `get_top_dialog_category(const telegram_api::TopPeerCategory &)` — the
/// wire category of the `contacts_topPeers` intake; the C++ CHECKs the
/// pointer non-null, so `None` is unreachable by construction.
pub fn get_top_dialog_category_from_telegram(
    category: Option<&telegram_api::TopPeerCategory>,
) -> TopDialogCategory {
    let Some(category) = category else {
        tdutils::log_error!("Lose null top peer category");
        return TopDialogCategory::Size;
    };
    match category {
        telegram_api::TopPeerCategory::topPeerCategoryCorrespondents(_) => {
            TopDialogCategory::Correspondent
        }
        telegram_api::TopPeerCategory::topPeerCategoryBotsPM(_) => TopDialogCategory::BotPM,
        telegram_api::TopPeerCategory::topPeerCategoryBotsInline(_) => TopDialogCategory::BotInline,
        telegram_api::TopPeerCategory::topPeerCategoryGroups(_) => TopDialogCategory::Group,
        telegram_api::TopPeerCategory::topPeerCategoryChannels(_) => TopDialogCategory::Channel,
        telegram_api::TopPeerCategory::topPeerCategoryPhoneCalls(_) => TopDialogCategory::Call,
        telegram_api::TopPeerCategory::topPeerCategoryForwardUsers(_) => {
            TopDialogCategory::ForwardUsers
        }
        telegram_api::TopPeerCategory::topPeerCategoryForwardChats(_) => {
            TopDialogCategory::ForwardChats
        }
        telegram_api::TopPeerCategory::topPeerCategoryBotsApp(_) => TopDialogCategory::BotApp,
        telegram_api::TopPeerCategory::topPeerCategoryBotsGuestChat(_) => {
            TopDialogCategory::BotGuest
        }
    }
}

/// `get_input_top_peer_category` — the `contacts_resetTopPeerRating`
/// request category.
pub fn get_input_top_peer_category(category: TopDialogCategory) -> telegram_api::TopPeerCategory {
    use telegram_api::TopPeerCategory as Wire;
    use telegram_api::{topPeerCategoryBotsApp, topPeerCategoryBotsGuestChat};
    use telegram_api::{topPeerCategoryBotsInline, topPeerCategoryBotsPM};
    use telegram_api::{topPeerCategoryChannels, topPeerCategoryPhoneCalls};
    use telegram_api::{topPeerCategoryCorrespondents, topPeerCategoryForwardChats};
    use telegram_api::{topPeerCategoryForwardUsers, topPeerCategoryGroups};
    match category {
        TopDialogCategory::Correspondent => {
            Wire::topPeerCategoryCorrespondents(topPeerCategoryCorrespondents)
        }
        TopDialogCategory::BotPM => Wire::topPeerCategoryBotsPM(topPeerCategoryBotsPM),
        TopDialogCategory::BotInline => Wire::topPeerCategoryBotsInline(topPeerCategoryBotsInline),
        TopDialogCategory::Group => Wire::topPeerCategoryGroups(topPeerCategoryGroups),
        TopDialogCategory::Channel => Wire::topPeerCategoryChannels(topPeerCategoryChannels),
        TopDialogCategory::Call => Wire::topPeerCategoryPhoneCalls(topPeerCategoryPhoneCalls),
        TopDialogCategory::ForwardUsers => {
            Wire::topPeerCategoryForwardUsers(topPeerCategoryForwardUsers)
        }
        TopDialogCategory::ForwardChats => {
            Wire::topPeerCategoryForwardChats(topPeerCategoryForwardChats)
        }
        TopDialogCategory::BotApp => Wire::topPeerCategoryBotsApp(topPeerCategoryBotsApp),
        TopDialogCategory::BotGuest => {
            Wire::topPeerCategoryBotsGuestChat(topPeerCategoryBotsGuestChat)
        }
        TopDialogCategory::Size => unreachable!("get_input_top_peer_category(Size)"),
    }
}

/// `TopDialogManager::MAX_TOP_DIALOGS_LIMIT` (TopDialogManager.h:49).
pub const MAX_TOP_DIALOGS_LIMIT: usize = 30;
/// `TopDialogManager::SERVER_SYNC_DELAY` — seconds.
pub const SERVER_SYNC_DELAY: i32 = 86400;
/// `TopDialogManager::SERVER_SYNC_RESEND_DELAY` — seconds.
pub const SERVER_SYNC_RESEND_DELAY: i32 = 60;
/// `TopDialogManager::DB_SYNC_DELAY` — seconds.
pub const DB_SYNC_DELAY: i32 = 5;
/// `TopDialogManager::MIN_STORY_RATING` (TopDialogManager.h:54).
pub const MIN_STORY_RATING: f64 = 10.0;

/// `TopDialogManager::TopDialog` (TopDialogManager.h:80-86) — ordered by
/// `(-rating, dialog_id)`, the C++ `operator<`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TopDialog {
    pub(crate) dialog_id: DialogId,
    pub(crate) rating: f64,
}

// `Ord` requires `Eq`; the f64 rating is the C++ double rating, which the
// recorded path never sets to NaN (`cmp` folds NaN onto `Ordering::Equal`).
impl Eq for TopDialog {}

impl TopDialog {
    pub fn new(dialog_id: DialogId, rating: f64) -> TopDialog {
        TopDialog { dialog_id, rating }
    }

    /// The raw dialog id (the local accessor of the private C++ field).
    pub fn get_dialog_id(&self) -> DialogId {
        self.dialog_id
    }

    /// The raw rating (the local accessor of the private C++ field).
    pub fn get_rating(&self) -> f64 {
        self.rating
    }
}

impl PartialOrd for TopDialog {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TopDialog {
    fn cmp(&self, other: &Self) -> Ordering {
        // std::make_pair(-rating, dialog_id.get()) ordering
        (-self.rating)
            .partial_cmp(&(-other.rating))
            .unwrap_or(Ordering::Equal)
            .then_with(|| self.dialog_id.get().cmp(&other.dialog_id.get()))
    }
}

/// `TopDialogManager::TopDialogs` (TopDialogManager.h:88-92). `is_dirty`
/// is runtime-only — the C++ store writes `rating_timestamp` + the vector.
#[derive(Clone, Debug, Default)]
pub struct TopDialogs {
    pub(crate) is_dirty: bool,
    pub(crate) rating_timestamp: f64,
    pub(crate) dialogs: Vec<TopDialog>,
}

impl TopDialogs {
    /// `store(const TopDialogs &, StorerT &)` (TopDialogManager.cpp:357-362).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_f64(out, self.rating_timestamp);
        store_i32(out, self.dialogs.len() as i32);
        for dialog in &self.dialogs {
            dialog.store(out);
        }
    }

    /// `parse(TopDialogs &, ParserT &)` (TopDialogManager.cpp:364-369).
    pub fn parse(parser: &mut Parser<'_>) -> Result<TopDialogs, &'static str> {
        let mut result = TopDialogs {
            rating_timestamp: parser.f64_()?,
            ..Default::default()
        };
        let len = parser.i32_()?;
        if len < 0 {
            return Err("Invalid vector size");
        }
        result.dialogs.reserve(len as usize);
        for _ in 0..len {
            result.dialogs.push(TopDialog::parse(parser)?);
        }
        Ok(result)
    }
}

impl TopDialog {
    /// `store(const TopDialog &, StorerT &)` (TopDialogManager.cpp:343-348).
    pub fn store(&self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        store_f64(out, self.rating);
    }

    /// `parse(TopDialog &, ParserT &)` (TopDialogManager.cpp:350-355).
    pub fn parse(parser: &mut Parser<'_>) -> Result<TopDialog, &'static str> {
        let dialog_id = DialogId::parse_from(parser)?;
        let rating = parser.f64_()?;
        Ok(TopDialog { dialog_id, rating })
    }
}

/// The `G()`/auth/options/pmc slice and the cross-manager surfaces the
/// collapsed TopDialogManager reads through. Every `create_handler<...>`
/// network tail is a seam that keeps the C++ request unanswered offline.
pub trait TopDialogManagerEnv {
    // -- the clock slice; the recorded-time pattern: no wall-clock reads
    /// `Timestamp::now_cached()`.
    fn now_cached(&self) -> f64;
    /// `G()->server_time()`.
    fn server_time(&self) -> f64;
    /// `Clocks::system()` — the wall clock since the epoch, the
    /// `top_dialogs_ts` pmc format.
    fn clocks_system(&self) -> f64;

    // -- the auth/close slice
    fn auth_is_authorized(&self) -> bool;
    fn auth_is_bot(&self) -> bool;
    fn close_flag(&self) -> bool;

    // -- options
    fn get_option_boolean(&self, name: &str, default_value: bool) -> bool;
    fn get_option_integer(&self, name: &str, default_value: i64) -> i64;
    fn set_option_boolean(&mut self, name: &str, value: bool);
    fn set_option_empty(&mut self, name: &str);

    // -- the pmc slice (raw values; the top-dialogs records are binary)
    fn binlog_pmc_get_raw(&mut self, key: &str) -> Vec<u8>;
    fn binlog_pmc_set_raw(&mut self, key: &str, value: &[u8]);
    fn binlog_pmc_erase(&mut self, key: &str);
    fn binlog_pmc_erase_by_prefix(&mut self, prefix: &str);

    /// `G()->use_chat_info_database()`.
    fn use_chat_info_database(&self) -> bool;

    // -- the cross-manager surfaces
    /// `dialog_manager_->check_dialog_access(dialog_id, false, Read,
    /// source)` — the M.6/M.10 dialog plane.
    fn check_dialog_access(&mut self, dialog_id: DialogId, source: &str) -> Result<(), Status>;
    /// `dialog_manager_->get_chats_object(-1, dialog_ids, source)`.
    fn get_chats_object(
        &mut self,
        total_count: i32,
        dialog_ids: &[DialogId],
        source: &str,
    ) -> td_api::Chats;
    /// `user_manager_->is_user_deleted(user_id)`.
    fn is_user_deleted(&self, user_id: UserId) -> bool;
    /// `user_manager_->get_my_id()`.
    fn get_my_id(&self) -> UserId;
    /// `user_manager_->get_bot_data(user_id)`.
    fn get_bot_data(&self, user_id: UserId) -> Result<BotData, Status>;
    /// `send_closure_later(td_->story_manager_actor_,
    /// &StoryManager::on_dialog_active_stories_order_updated, ...)` — the
    /// StoryManager seam (Wave 3 M.47).
    fn on_need_dialog_stories_changed(&mut self, dialog_id: DialogId);
    /// `user_manager_->on_get_users(users, source)` — the
    /// `contacts_topPeers` users intake.
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str);
    /// `chat_manager_->on_get_chats(chats, source)` — the
    /// `contacts_topPeers` chats intake.
    fn on_get_chats(&mut self, chats: Vec<telegram_api::Chat>, source: &str);

    // -- the hop seams
    /// `send_closure(td_->messages_manager_actor_,
    /// &MessagesManager::load_dialogs, dialog_ids, promise)` — the M.10
    /// dialog-load hop; the promise routes back to
    /// `TopDialogManager::on_load_dialogs` and stays parked offline.
    fn load_dialogs(&mut self, dialog_ids: Vec<DialogId>);
    /// `StateManager::wait_first_sync(create_event_promise(...))` — the
    /// parked promise fires `TopDialogManager::on_first_sync`
    /// (`[blocked: StateManager]` — the net-backed state machine).
    fn wait_first_sync(&mut self);

    // -- the `[blocked: net]` query tails; each one drops its promise,
    // -- which keeps the C++ request unanswered offline
    /// `ToggleTopPeersQuery` (TopDialogManager.cpp:70-93); the promise
    /// routes back to `TopDialogManager::on_toggle_top_peers`.
    fn send_toggle_top_peers_query(&mut self, is_enabled: bool);
    /// `GetTopPeersQuery` (TopDialogManager.cpp:43-68) with the
    /// `get_vector_hash` of the top-peer ids; the promise routes back to
    /// `TopDialogManager::on_get_top_peers`.
    fn send_get_top_peers_query(&mut self, hash: i64);
    /// `ResetTopPeerRatingQuery` (TopDialogManager.cpp:95-122) —
    /// fire-and-forget in the C++ (the result is ignored, errors are only
    /// logged).
    fn send_reset_top_peer_rating_query(
        &mut self,
        category: TopDialogCategory,
        dialog_id: DialogId,
    );
}

/// `TopDialogManager::SyncState` (TopDialogManager.h:67).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncState {
    None,
    Pending,
    Ok,
}

/// `TopDialogManager::GetTopDialogsQuery` (TopDialogManager.h:73-77).
/// `limit`/`promise` are consumed by `on_load_dialogs` — the net-independent
/// half of the `getTopChats` tail (its net half parks the promise).
#[allow(dead_code)]
pub struct GetTopDialogsQuery {
    pub(crate) category: TopDialogCategory,
    pub(crate) limit: usize,
    pub(crate) promise: Promise<td_api::Chats>,
}

/// `TopDialogManager` (TopDialogManager.h:28-139) — the collapsed actor.
pub struct TopDialogManager {
    is_enabled: bool,
    is_synchronized: bool,
    rating_e_decay: i32,

    have_toggle_top_peers_query: bool,
    have_pending_toggle_top_peers_query: bool,
    pending_toggle_top_peers_query: bool,
    was_first_sync: bool,
    db_sync_state: SyncState,
    first_unsync_change: Timestamp,
    server_sync_state: SyncState,
    last_server_sync: Timestamp,

    pending_get_top_dialogs: Vec<GetTopDialogsQuery>,

    by_category: [TopDialogs; TOP_DIALOG_CATEGORY_SIZE],

    /// The actor's own timer slot (`set_timeout_at`/`cancel_timeout`),
    /// collapsed into the shared Td timeout slot.
    deadline: Timestamp,
}

impl Default for TopDialogManager {
    fn default() -> Self {
        TopDialogManager {
            is_enabled: true,
            is_synchronized: false,
            rating_e_decay: 241920,
            have_toggle_top_peers_query: false,
            have_pending_toggle_top_peers_query: false,
            pending_toggle_top_peers_query: false,
            was_first_sync: false,
            db_sync_state: SyncState::None,
            first_unsync_change: Timestamp::never(),
            server_sync_state: SyncState::None,
            last_server_sync: Timestamp::never(),
            pending_get_top_dialogs: Vec::new(),
            by_category: std::array::from_fn(|_| TopDialogs::default()),
            deadline: Timestamp::never(),
        }
    }
}

impl std::fmt::Debug for TopDialogManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TopDialogManager")
            .field("is_enabled", &self.is_enabled)
            .field("is_synchronized", &self.is_synchronized)
            .field("rating_e_decay", &self.rating_e_decay)
            .field(
                "have_toggle_top_peers_query",
                &self.have_toggle_top_peers_query,
            )
            .field(
                "have_pending_toggle_top_peers_query",
                &self.have_pending_toggle_top_peers_query,
            )
            .field(
                "pending_toggle_top_peers_query",
                &self.pending_toggle_top_peers_query,
            )
            .field("was_first_sync", &self.was_first_sync)
            .field("db_sync_state", &self.db_sync_state)
            .field("first_unsync_change", &self.first_unsync_change)
            .field("server_sync_state", &self.server_sync_state)
            .field("last_server_sync", &self.last_server_sync)
            .field(
                "pending_get_top_dialogs",
                &self.pending_get_top_dialogs.len(),
            )
            .finish()
    }
}

impl TopDialogManager {
    pub fn new() -> TopDialogManager {
        TopDialogManager::default()
    }

    /// `TopDialogManager::update_is_enabled` (TopDialogManager.cpp:127-138).
    pub fn update_is_enabled(&mut self, env: &mut impl TopDialogManagerEnv, is_enabled: bool) {
        if !env.auth_is_authorized() || env.auth_is_bot() {
            return;
        }

        if self.set_is_enabled(env, is_enabled) {
            env.binlog_pmc_set_raw("top_peers_enabled", if is_enabled { b"1" } else { b"0" });
            self.send_toggle_top_peers(env, is_enabled);

            self.loop_(env);
        }
    }

    /// `TopDialogManager::set_is_enabled` (TopDialogManager.cpp:140-149).
    fn set_is_enabled(&mut self, env: &mut impl TopDialogManagerEnv, is_enabled: bool) -> bool {
        if self.is_enabled == is_enabled {
            return false;
        }

        tdutils::log_debug!("Change top chats is_enabled to {is_enabled}");
        self.is_enabled = is_enabled;
        self.try_start(env);
        true
    }

    /// `TopDialogManager::send_toggle_top_peers` (TopDialogManager.cpp:151-169).
    fn send_toggle_top_peers(&mut self, env: &mut impl TopDialogManagerEnv, is_enabled: bool) {
        if env.close_flag() {
            return;
        }

        if self.have_toggle_top_peers_query {
            self.have_pending_toggle_top_peers_query = true;
            self.pending_toggle_top_peers_query = is_enabled;
            return;
        }

        tdutils::log_debug!("Send toggle top peers query to {is_enabled}");
        self.have_toggle_top_peers_query = true;

        env.send_toggle_top_peers_query(is_enabled);
    }

    /// `TopDialogManager::on_toggle_top_peers` (TopDialogManager.cpp:171-191)
    /// — the ToggleTopPeersQuery callback; net-coupled, directly callable
    /// (and tested) as the local decision machine.
    #[allow(dead_code)] // the ToggleTopPeersQuery callback — the net tail parks offline
    pub(crate) fn on_toggle_top_peers(
        &mut self,
        env: &mut impl TopDialogManagerEnv,
        is_enabled: bool,
        result: Result<(), Status>,
    ) {
        assert!(self.have_toggle_top_peers_query);
        self.have_toggle_top_peers_query = false;

        if self.have_pending_toggle_top_peers_query {
            self.have_pending_toggle_top_peers_query = false;
            if self.pending_toggle_top_peers_query != is_enabled {
                self.send_toggle_top_peers(env, self.pending_toggle_top_peers_query);
                return;
            }
        }

        if result.is_ok() {
            // everything is synchronized
            env.binlog_pmc_erase("top_peers_enabled");
        } else {
            // let's resend the query forever
            self.send_toggle_top_peers(env, is_enabled);
        }
        self.loop_(env);
    }

    /// `TopDialogManager::on_dialog_used` (TopDialogManager.cpp:193-235).
    #[allow(dead_code)] // the M.10 senders (MessagesManager::on_dialog_used free function) land at the M.10 integration
    pub(crate) fn on_dialog_used(
        &mut self,
        env: &mut impl TopDialogManagerEnv,
        category: TopDialogCategory,
        dialog_id: DialogId,
        date: i32,
    ) {
        assert!(!env.auth_is_bot());
        if !self.is_enabled {
            return;
        }
        let top_dialogs = &mut self.by_category[category.position()];

        top_dialogs.is_dirty = true;
        let pos = top_dialogs
            .dialogs
            .iter()
            .position(|top_dialog| top_dialog.dialog_id == dialog_id);
        let pos = match pos {
            Some(pos) => pos,
            None => {
                top_dialogs.dialogs.push(TopDialog {
                    dialog_id,
                    rating: 0.0,
                });
                top_dialogs.dialogs.len() - 1
            }
        };

        let delta = TopDialogManager::rating_add_static(
            date as f64,
            top_dialogs.rating_timestamp,
            self.rating_e_decay,
        );
        let old_need_dialog_stories =
            need_dialog_stories(category, dialog_id, top_dialogs.dialogs[pos].rating);
        top_dialogs.dialogs[pos].rating += delta;
        let mut pos = pos;
        while pos > 0 {
            let next = pos - 1;
            if top_dialogs.dialogs[next] < top_dialogs.dialogs[pos] {
                break;
            }
            top_dialogs.dialogs.swap(next, pos);
            pos = next;
        }

        tdutils::log_info!(
            "Update {} rating of {} by {delta} to {}",
            get_top_dialog_category_name(category),
            dialog_id.get(),
            top_dialogs.dialogs[pos].rating
        );

        if old_need_dialog_stories
            != need_dialog_stories(category, dialog_id, top_dialogs.dialogs[pos].rating)
        {
            self.on_need_dialog_stories_changed(env, dialog_id);
        }

        if !bool::from(self.first_unsync_change) {
            self.first_unsync_change = Timestamp::at_time(env.now_cached());
        }
        self.loop_(env);
    }

    /// `TopDialogManager::remove_dialog` (TopDialogManager.cpp:237-276).
    pub fn remove_dialog(
        &mut self,
        env: &mut impl TopDialogManagerEnv,
        category: TopDialogCategory,
        dialog_id: DialogId,
        mut promise: Promise<()>,
    ) {
        if category == TopDialogCategory::Size {
            promise.set_error(Status::error(400, "Top chat category must be non-empty"));
            return;
        }
        if let Err(error) = env.check_dialog_access(dialog_id, "remove_dialog") {
            promise.set_error(error);
            return;
        }
        assert!(!env.auth_is_bot());
        if !self.is_enabled {
            promise.set_value(());
            return;
        }

        let mut category = category;
        if category == TopDialogCategory::ForwardUsers
            && dialog_id.get_type() != crate::dialog_id::DialogType::User
        {
            category = TopDialogCategory::ForwardChats;
        }

        env.send_reset_top_peer_rating_query(category, dialog_id);

        let top_dialogs = &mut self.by_category[category.position()];
        let Some(pos) = top_dialogs
            .dialogs
            .iter()
            .position(|top_dialog| top_dialog.dialog_id == dialog_id)
        else {
            promise.set_value(());
            return;
        };

        let old_need_dialog_stories =
            need_dialog_stories(category, dialog_id, top_dialogs.dialogs[pos].rating);
        top_dialogs.is_dirty = true;
        top_dialogs.dialogs.remove(pos);
        if !bool::from(self.first_unsync_change) {
            self.first_unsync_change = Timestamp::at_time(env.now_cached());
        }
        self.loop_(env);
        promise.set_value(());

        if old_need_dialog_stories {
            self.on_need_dialog_stories_changed(env, dialog_id);
        }
    }

    /// `TopDialogManager::get_top_dialogs` (TopDialogManager.cpp:278-296).
    pub fn get_top_dialogs(
        &mut self,
        env: &mut impl TopDialogManagerEnv,
        category: TopDialogCategory,
        limit: i32,
        mut promise: Promise<td_api::Chats>,
    ) {
        if category == TopDialogCategory::Size {
            promise.set_error(Status::error(400, "Top chat category must be non-empty"));
            return;
        }
        if limit <= 0 {
            promise.set_error(Status::error(400, "Limit must be positive"));
            return;
        }
        if !self.is_enabled {
            promise.set_error(Status::error(400, "Top chat computation is disabled"));
            return;
        }

        self.pending_get_top_dialogs.push(GetTopDialogsQuery {
            category,
            limit: limit as usize,
            promise,
        });
        self.loop_(env);
    }

    /// `TopDialogManager::is_top_dialog` (TopDialogManager.cpp:298-317).
    #[allow(dead_code)] // the MessagesManager::on_get_dialogs consumer lands at the M.10 integration
    pub(crate) fn is_top_dialog(
        &self,
        env: &impl TopDialogManagerEnv,
        category: TopDialogCategory,
        limit: usize,
        dialog_id: DialogId,
    ) -> i32 {
        assert!(category != TopDialogCategory::Size);
        assert!(category != TopDialogCategory::ForwardUsers);
        assert!(limit > 0);
        assert!(!env.auth_is_bot());
        if !self.is_enabled {
            return 0;
        }

        let dialogs = &self.by_category[category.position()].dialogs;
        for dialog in dialogs.iter().take(limit) {
            if dialog.dialog_id == dialog_id {
                return 1;
            }
        }
        if self.is_synchronized { 0 } else { -1 }
    }

    /// `TopDialogManager::get_story_dialog_ids` (TopDialogManager.cpp:319-334).
    #[allow(dead_code)] // the StoryManager consumer lands with M.47
    pub(crate) fn get_story_dialog_ids(&self, env: &impl TopDialogManagerEnv) -> Vec<DialogId> {
        assert!(!env.auth_is_bot());
        if !self.is_enabled {
            return Vec::new();
        }

        let mut dialog_ids = Vec::new();
        let dialogs = &self.by_category[TopDialogCategory::Correspondent.position()].dialogs;
        for dialog in dialogs {
            if dialog_ids.len() >= 30 {
                break;
            }
            if need_dialog_stories(
                TopDialogCategory::Correspondent,
                dialog.dialog_id,
                dialog.rating,
            ) {
                dialog_ids.push(dialog.dialog_id);
            }
        }
        dialog_ids
    }

    /// `TopDialogManager::update_rating_e_decay` (TopDialogManager.cpp:336-341).
    #[allow(dead_code)] // fires from the OptionManager::on_option_changed table ("rating_e_decay") — the option-hook seam
    pub(crate) fn update_rating_e_decay(&mut self, env: &impl TopDialogManagerEnv) {
        if env.auth_is_bot() {
            return;
        }
        self.rating_e_decay =
            env.get_option_integer("rating_e_decay", self.rating_e_decay as i64) as i32;
    }

    /// `TopDialogManager::rating_add` (TopDialogManager.cpp:371-373).
    fn rating_add(&self, now: f64, rating_timestamp: f64) -> f64 {
        Self::rating_add_static(now, rating_timestamp, self.rating_e_decay)
    }

    fn rating_add_static(now: f64, rating_timestamp: f64, rating_e_decay: i32) -> f64 {
        ((now - rating_timestamp) / rating_e_decay as f64).exp()
    }

    /// `TopDialogManager::current_rating_add` (TopDialogManager.cpp:375-377).
    fn current_rating_add(&self, server_time: f64, rating_timestamp: f64) -> f64 {
        self.rating_add(server_time, rating_timestamp)
    }

    /// `TopDialogManager::normalize_rating` (TopDialogManager.cpp:379-397)
    /// — the daily decay.
    fn normalize_rating(&mut self, env: &mut impl TopDialogManagerEnv) {
        let server_time = env.server_time();
        for i in 0..TOP_DIALOG_CATEGORY_SIZE {
            let div_by = self.current_rating_add(server_time, self.by_category[i].rating_timestamp);
            let category = to_top_dialog_category(i);
            let mut stories_changed: Vec<DialogId> = Vec::new();
            {
                let top_dialogs = &mut self.by_category[i];
                top_dialogs.rating_timestamp = server_time;
                for dialog in &mut top_dialogs.dialogs {
                    let old_need_dialog_stories =
                        need_dialog_stories(category, dialog.dialog_id, dialog.rating);
                    dialog.rating /= div_by;
                    if old_need_dialog_stories
                        != need_dialog_stories(category, dialog.dialog_id, dialog.rating)
                    {
                        stories_changed.push(dialog.dialog_id);
                    }
                }
                top_dialogs.is_dirty = true;
            }
            for dialog_id in stories_changed {
                self.on_need_dialog_stories_changed(env, dialog_id);
            }
        }
        self.db_sync_state = SyncState::None;
    }

    /// `TopDialogManager::on_need_dialog_stories_changed`
    /// (TopDialogManager.cpp:408-411) — the StoryManager hop (Wave 3
    /// M.47).
    fn on_need_dialog_stories_changed(
        &mut self,
        env: &mut impl TopDialogManagerEnv,
        dialog_id: DialogId,
    ) {
        env.on_need_dialog_stories_changed(dialog_id);
    }

    /// `TopDialogManager::do_get_top_dialogs` (TopDialogManager.cpp:413-443).
    fn do_get_top_dialogs(
        &mut self,
        env: &mut impl TopDialogManagerEnv,
        query: GetTopDialogsQuery,
    ) {
        let mut dialog_ids = Vec::new();
        if query.category != TopDialogCategory::ForwardUsers {
            let top_dialogs = &self.by_category[query.category.position()];
            dialog_ids.extend(top_dialogs.dialogs.iter().map(|x| x.dialog_id));
        } else {
            // merge ForwardUsers and ForwardChats
            let users = &self.by_category[TopDialogCategory::ForwardUsers.position()].dialogs;
            let chats = &self.by_category[TopDialogCategory::ForwardChats.position()].dialogs;
            let mut users_pos = 0;
            let mut chats_pos = 0;
            while users_pos < users.len() || chats_pos < chats.len() {
                if chats_pos == chats.len()
                    || (users_pos < users.len() && users[users_pos] < chats[chats_pos])
                {
                    dialog_ids.push(users[users_pos].dialog_id);
                    users_pos += 1;
                } else {
                    dialog_ids.push(chats[chats_pos].dialog_id);
                    chats_pos += 1;
                }
            }
        }

        // `MessagesManager::load_dialogs` — the M.10 hop; the parked
        // promise routes back to on_load_dialogs and stays unanswered
        // offline.
        env.load_dialogs(dialog_ids);
        drop(query);
    }

    /// `TopDialogManager::on_load_dialogs` (TopDialogManager.cpp:445-488)
    /// — the load_dialogs callback; net/M.10-coupled, directly callable
    /// (and tested) as the local filter + builder half.
    /// The M.6/M.10 `load_dialogs` hop — only the blocked seam skips it in
    /// the lib build (the inline tests drive it).
    #[allow(dead_code)]
    pub(crate) fn on_load_dialogs(
        &self,
        env: &mut impl TopDialogManagerEnv,
        mut query: GetTopDialogsQuery,
        dialog_ids: Vec<DialogId>,
    ) {
        let limit = query.limit.min(MAX_TOP_DIALOGS_LIMIT).min(dialog_ids.len());
        let mut result = Vec::with_capacity(limit);
        for dialog_id in dialog_ids {
            if dialog_id.get_type() == crate::dialog_id::DialogType::User {
                let user_id = dialog_id.get_user_id();
                if env.is_user_deleted(user_id) {
                    tdutils::log_info!("Skip deleted {user_id:?}");
                    continue;
                }
                if env.get_my_id() == user_id {
                    tdutils::log_info!("Skip self {user_id:?}");
                    continue;
                }
                if query.category == TopDialogCategory::BotInline
                    || query.category == TopDialogCategory::BotGuest
                    || query.category == TopDialogCategory::BotPM
                {
                    let bot_info = match env.get_bot_data(user_id) {
                        Ok(bot_info) => bot_info,
                        Err(_) => {
                            tdutils::log_info!("Skip not a bot {user_id:?}");
                            continue;
                        }
                    };
                    if query.category == TopDialogCategory::BotInline
                        && (bot_info.username.is_empty() || !bot_info.is_inline)
                    {
                        tdutils::log_info!("Skip non-inline bot {user_id:?}");
                        continue;
                    }
                    if query.category == TopDialogCategory::BotGuest
                        && (bot_info.username.is_empty() || !bot_info.is_guestchat)
                    {
                        tdutils::log_info!("Skip non-guest bot {user_id:?}");
                        continue;
                    }
                }
            }

            result.push(dialog_id);
            if result.len() == limit {
                break;
            }
        }

        query.promise.set_value(env.get_chats_object(
            -1,
            &result,
            "TopDialogManager::on_load_dialogs",
        ));
    }

    /// `TopDialogManager::do_get_top_peers` (TopDialogManager.cpp:490-515).
    fn do_get_top_peers(&mut self, env: &mut impl TopDialogManagerEnv) {
        let mut peer_ids = Vec::new();
        for category in &self.by_category {
            for top_dialog in &category.dialogs {
                let dialog_id = top_dialog.dialog_id;
                match dialog_id.get_type() {
                    crate::dialog_id::DialogType::User => {
                        peer_ids.push(dialog_id.get_user_id().get() as u64)
                    }
                    crate::dialog_id::DialogType::Chat => {
                        peer_ids.push(dialog_id.get_chat_id().get() as u64)
                    }
                    crate::dialog_id::DialogType::Channel => {
                        peer_ids.push(dialog_id.get_channel_id().get() as u64)
                    }
                    _ => {}
                }
            }
        }
        env.send_get_top_peers_query(crate::misc::get_vector_hash(&peer_ids));
    }

    /// `TopDialogManager::on_get_top_peers` (TopDialogManager.cpp:517-584)
    /// — the GetTopPeersQuery callback; net-coupled, directly callable
    /// (and tested) as the intake half.
    /// The `contacts.getTopPeers` intake — its query is a [blocked: net]
    /// seam (the inline tests drive it).
    #[allow(dead_code)]
    pub(crate) fn on_get_top_peers(
        &mut self,
        env: &mut impl TopDialogManagerEnv,
        result: Result<telegram_api::contacts_TopPeers, Status>,
    ) {
        self.normalize_rating(env); // once a day too

        let top_peers_parent = match result {
            Ok(top_peers_parent) => top_peers_parent,
            Err(_) => {
                self.last_server_sync = Timestamp::in_(
                    (SERVER_SYNC_RESEND_DELAY - SERVER_SYNC_DELAY) as f64,
                    Timestamp::at_time(env.now_cached()),
                );
                self.loop_(env);
                return;
            }
        };

        self.last_server_sync = Timestamp::at_time(env.now_cached());
        self.server_sync_state = SyncState::Ok;
        self.is_synchronized = true;

        match top_peers_parent {
            telegram_api::contacts_TopPeers::contacts_topPeersNotModified(_) => {
                // nothing to do
            }
            telegram_api::contacts_TopPeers::contacts_topPeersDisabled(_) => {
                env.set_option_boolean("disable_top_chats", true);
                self.set_is_enabled(env, false); // apply immediately
            }
            telegram_api::contacts_TopPeers::contacts_topPeers(top_peers) => {
                env.set_option_empty("disable_top_chats");
                self.set_is_enabled(env, true); // apply immediately

                env.on_get_users(top_peers.users, "on get top chats");
                env.on_get_chats(top_peers.chats, "on get top chats");

                let old_dialog_ids = self.get_story_dialog_ids(env);
                for category in top_peers.categories {
                    let dialog_category =
                        get_top_dialog_category_from_telegram(category.category.as_ref());
                    let top_dialogs = &mut self.by_category[dialog_category.position()];

                    top_dialogs.is_dirty = true;
                    top_dialogs.dialogs.clear();
                    for top_peer in category.peers {
                        top_dialogs.dialogs.push(TopDialog {
                            dialog_id: top_peer
                                .peer
                                .as_ref()
                                .map(DialogId::from_peer)
                                .unwrap_or_default(),
                            rating: top_peer.rating,
                        });
                    }
                }
                let new_dialog_ids = self.get_story_dialog_ids(env);
                for dialog_id in &old_dialog_ids {
                    if !new_dialog_ids.contains(dialog_id) {
                        self.on_need_dialog_stories_changed(env, *dialog_id);
                    }
                }
                for dialog_id in &new_dialog_ids {
                    if !old_dialog_ids.contains(dialog_id) {
                        self.on_need_dialog_stories_changed(env, *dialog_id);
                    }
                }
                self.db_sync_state = SyncState::None;
            }
        }

        let system = env.clocks_system();
        env.binlog_pmc_set_raw("top_dialogs_ts", (system as u32).to_string().as_bytes());
        self.loop_(env);
    }

    /// `TopDialogManager::do_save_top_dialogs` (TopDialogManager.cpp:586-603).
    fn do_save_top_dialogs(&mut self, env: &mut impl TopDialogManagerEnv) {
        tdutils::log_info!("Save top chats");
        for top_dialog_category_i in 0..TOP_DIALOG_CATEGORY_SIZE {
            let top_dialogs = &mut self.by_category[top_dialog_category_i];
            if !top_dialogs.is_dirty {
                continue;
            }
            top_dialogs.is_dirty = false;

            if env.use_chat_info_database() {
                let top_dialog_category = to_top_dialog_category(top_dialog_category_i);
                let key = format!(
                    "top_dialogs#{}",
                    get_top_dialog_category_name(top_dialog_category)
                );
                // `log_event_store` framing — the version prefix leads the
                // record (TopDialogManager.cpp:598).
                let mut value = Vec::new();
                crate::store_parse::store_log_event_version(&mut value);
                top_dialogs.store(&mut value);
                env.binlog_pmc_set_raw(&key, &value);
            }
        }
        self.db_sync_state = SyncState::Ok;
        self.first_unsync_change = Timestamp::never();
    }

    /// `TopDialogManager::init` (TopDialogManager.cpp:613-628) — fired
    /// from the AuthData block (AuthManager.cpp:1553).
    pub fn init(&mut self, env: &mut impl TopDialogManagerEnv) {
        if !env.auth_is_authorized() {
            return;
        }

        self.is_enabled = !env.get_option_boolean("disable_top_chats", false);
        self.update_rating_e_decay(env);

        let need_update_top_peers = env.binlog_pmc_get_raw("top_peers_enabled");
        if !need_update_top_peers.is_empty() {
            self.send_toggle_top_peers(env, need_update_top_peers[0] == b'1');
        }

        self.try_start(env);
        self.loop_(env);
    }

    /// `TopDialogManager::try_start` (TopDialogManager.cpp:630-679).
    fn try_start(&mut self, env: &mut impl TopDialogManagerEnv) {
        self.was_first_sync = false;
        self.first_unsync_change = Timestamp::never();
        self.server_sync_state = SyncState::None;
        self.last_server_sync = Timestamp::never();

        if env.auth_is_bot() {
            return;
        }

        tdutils::log_debug!("Init is enabled: {}", self.is_enabled);

        let di_top_dialogs_ts = env.binlog_pmc_get_raw("top_dialogs_ts");
        if !di_top_dialogs_ts.is_empty() {
            let stored_system: u32 = std::str::from_utf8(&di_top_dialogs_ts)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            self.last_server_sync = Timestamp::in_(
                stored_system as f64 - env.clocks_system(),
                Timestamp::at_time(env.now_cached()),
            );
            if self
                .last_server_sync
                .is_in_past_at(Timestamp::at_time(env.now_cached()))
            {
                self.server_sync_state = SyncState::Ok;
            }
            self.is_synchronized = env.use_chat_info_database();
        }

        if self.is_enabled && env.use_chat_info_database() {
            for top_dialog_category_i in 0..TOP_DIALOG_CATEGORY_SIZE {
                let top_dialog_category = to_top_dialog_category(top_dialog_category_i);
                let key = format!(
                    "top_dialogs#{}",
                    get_top_dialog_category_name(top_dialog_category)
                );
                let value = env.binlog_pmc_get_raw(&key);

                let top_dialogs = &mut self.by_category[top_dialog_category_i];
                top_dialogs.is_dirty = false;
                if value.is_empty() {
                    continue;
                }
                // the log-event version prefix leads the record
                let mut parser =
                    Parser::new_log_event(&value).expect("Failed to parse top dialogs"); // log_event_parse(...).ensure()
                *top_dialogs = TopDialogs::parse(&mut parser).expect("Failed to parse top dialogs");
            }
            self.normalize_rating(env);
        } else {
            env.binlog_pmc_erase_by_prefix("top_dialogs#");
            for top_dialogs in &mut self.by_category {
                *top_dialogs = TopDialogs::default();
            }
        }
        self.db_sync_state = SyncState::Ok;

        for dialog_id in self.get_story_dialog_ids(env) {
            self.on_need_dialog_stories_changed(env, dialog_id);
        }

        // `StateManager::wait_first_sync` — the parked promise fires
        // on_first_sync (`[blocked: StateManager]`).
        env.wait_first_sync();
    }

    /// `TopDialogManager::on_first_sync` (TopDialogManager.cpp:681-684) —
    /// the wait_first_sync callback; net-coupled, directly callable (and
    /// tested) as the local machine half.
    /// The StateManager first-sync hop — [blocked: StateManager] (the
    /// inline tests drive it).
    #[allow(dead_code)]
    pub(crate) fn on_first_sync(&mut self, env: &mut impl TopDialogManagerEnv) {
        self.was_first_sync = true;
        self.loop_(env);
    }

    /// `TopDialogManager::loop` (TopDialogManager.cpp:686-744) — the sync
    /// machine; `loop` is a Rust keyword.
    pub(crate) fn loop_(&mut self, env: &mut impl TopDialogManagerEnv) {
        if env.close_flag() || env.auth_is_bot() {
            return;
        }

        if !self.pending_get_top_dialogs.is_empty() && (self.is_synchronized || !self.is_enabled) {
            let pending = std::mem::take(&mut self.pending_get_top_dialogs);
            for query in pending {
                self.do_get_top_dialogs(env, query);
            }
        }

        // server sync
        let mut server_sync_timeout = Timestamp::never();
        if self.server_sync_state == SyncState::Ok {
            server_sync_timeout = if self.pending_get_top_dialogs.is_empty() {
                Timestamp::at_time(self.last_server_sync.at() + SERVER_SYNC_DELAY as f64)
            } else {
                Timestamp::at_time(env.now_cached())
            };
            if server_sync_timeout.is_in_past_at(Timestamp::at_time(env.now_cached())) {
                self.server_sync_state = SyncState::None;
            }
        }

        let mut wakeup_timeout = Timestamp::never();
        if self.server_sync_state == SyncState::Ok {
            wakeup_timeout.relax(&server_sync_timeout);
        } else if self.server_sync_state == SyncState::None
            && (self.was_first_sync || !self.pending_get_top_dialogs.is_empty())
        {
            self.server_sync_state = SyncState::Pending;
            self.do_get_top_peers(env);
        }

        if self.is_enabled {
            // database sync
            let mut db_sync_timeout = Timestamp::never();
            if self.db_sync_state == SyncState::Ok && bool::from(self.first_unsync_change) {
                db_sync_timeout =
                    Timestamp::at_time(self.first_unsync_change.at() + DB_SYNC_DELAY as f64);
                if db_sync_timeout.is_in_past_at(Timestamp::at_time(env.now_cached())) {
                    self.db_sync_state = SyncState::None;
                }
            }

            if self.db_sync_state == SyncState::Ok {
                wakeup_timeout.relax(&db_sync_timeout);
            } else if self.db_sync_state == SyncState::None
                && self.server_sync_state == SyncState::Ok
            {
                self.do_save_top_dialogs(env);
            }
        }

        // set_timeout_at / cancel_timeout — the shared Td slot re-arms at
        // this deadline.
        self.deadline = wakeup_timeout;
    }

    /// The collapsed actor timer slot — `next_deadline` for the shared Td
    /// timeout (`wakeup_timeout`, TopDialogManager.cpp:737-743).
    /// Read by the Td timeout sweep wiring (and the tests) — the lib
    /// build only reaches it through the [blocked: net] reschedule path.
    #[allow(dead_code)]
    pub(crate) fn next_deadline(&self) -> Option<f64> {
        if bool::from(self.deadline) {
            Some(self.deadline.at())
        } else {
            None
        }
    }

    /// The fired shared timeout slot — the C++ actor's `timeout_expired`
    /// re-runs the loop.
    pub(crate) fn tick_timeouts(&mut self, env: &mut impl TopDialogManagerEnv) {
        self.loop_(env);
    }
}

/// `static_cast<TopDialogCategory>(i)` for the `by_category_` index.
fn to_top_dialog_category(i: usize) -> TopDialogCategory {
    assert!(i < TOP_DIALOG_CATEGORY_SIZE);
    match i {
        0 => TopDialogCategory::Correspondent,
        1 => TopDialogCategory::BotPM,
        2 => TopDialogCategory::BotInline,
        3 => TopDialogCategory::Group,
        4 => TopDialogCategory::Channel,
        5 => TopDialogCategory::Call,
        6 => TopDialogCategory::ForwardUsers,
        7 => TopDialogCategory::ForwardChats,
        8 => TopDialogCategory::BotApp,
        _ => TopDialogCategory::BotGuest,
    }
}

/// `TopDialogManager::need_dialog_stories` (TopDialogManager.cpp:399-406).
fn need_dialog_stories(category: TopDialogCategory, dialog_id: DialogId, rating: f64) -> bool {
    if category == TopDialogCategory::Correspondent
        && dialog_id.get_type() == crate::dialog_id::DialogType::User
        && rating >= MIN_STORY_RATING
    {
        tdutils::log_debug!(
            "Show stories of {}, because it has rating {rating}",
            dialog_id.get()
        );
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialog_id::DialogType;
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use tdutils::promise::PromiseInterface;

    /// A [`PromiseInterface`] that parks the answer in a shared slot — the
    /// offline stand-in for the deferred outbox.
    #[derive(Clone)]
    struct Slot<T: Send + 'static> {
        inner: Arc<parking_lot::Mutex<Option<Result<T, Status>>>>,
    }

    impl<T: Send + 'static> Slot<T> {
        fn new() -> Slot<T> {
            Slot {
                inner: Arc::new(parking_lot::Mutex::new(None)),
            }
        }

        fn take(&self) -> Option<Result<T, Status>> {
            self.inner.lock().take()
        }
    }

    impl<T: Send + 'static> PromiseInterface<T> for Slot<T> {
        fn set_value(&mut self, value: T) {
            *self.inner.lock() = Some(Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            *self.inner.lock() = Some(Err(error));
        }
    }

    /// The offline env: every seam records its call.
    struct FakeEnv {
        now: f64,
        server_time: f64,
        system: f64,
        pmc: HashMap<String, Vec<u8>>,
        options_boolean: HashMap<String, bool>,
        options_integer: HashMap<String, i64>,
        auth_is_authorized: bool,
        auth_is_bot: bool,
        close_flag: bool,
        use_chat_info_database: bool,
        access_denied: Option<Status>,
        deleted_users: HashSet<i64>,
        my_id: UserId,
        bots: HashMap<i64, BotData>,

        // the recorded seams
        toggle_queries: Vec<bool>,
        get_top_peers_hashes: Vec<i64>,
        reset_queries: Vec<(TopDialogCategory, i64)>,
        load_dialogs_calls: Vec<Vec<i64>>,
        wait_first_sync_count: usize,
        story_updates: Vec<i64>,
        option_writes: Vec<(String, Option<bool>)>,
        users_intakes: Vec<&'static str>,
        chats_intakes: Vec<&'static str>,
    }

    impl FakeEnv {
        fn new() -> FakeEnv {
            FakeEnv {
                now: 1000.0,
                server_time: 2000.0,
                system: 1_700_000_000.0,
                pmc: HashMap::new(),
                options_boolean: HashMap::new(),
                options_integer: HashMap::new(),
                auth_is_authorized: true,
                auth_is_bot: false,
                close_flag: false,
                use_chat_info_database: true,
                access_denied: None,
                deleted_users: HashSet::new(),
                my_id: UserId::new(1),
                bots: HashMap::new(),
                toggle_queries: Vec::new(),
                get_top_peers_hashes: Vec::new(),
                reset_queries: Vec::new(),
                load_dialogs_calls: Vec::new(),
                wait_first_sync_count: 0,
                story_updates: Vec::new(),
                option_writes: Vec::new(),
                users_intakes: Vec::new(),
                chats_intakes: Vec::new(),
            }
        }

        fn make_bot(username: &str, is_inline: bool, is_guestchat: bool) -> BotData {
            BotData {
                username: username.to_owned(),
                can_be_edited: false,
                can_join_groups: false,
                can_read_all_group_messages: false,
                has_main_app: false,
                has_bot_forum_view: false,
                can_bot_create_topics: false,
                can_manage_bots: false,
                is_inline,
                is_guestchat,
                is_business: false,
                is_guard: false,
                need_location: false,
                can_be_added_to_attach_menu: false,
            }
        }
    }

    impl TopDialogManagerEnv for FakeEnv {
        fn now_cached(&self) -> f64 {
            self.now
        }
        fn server_time(&self) -> f64 {
            self.server_time
        }
        fn clocks_system(&self) -> f64 {
            self.system
        }

        fn auth_is_authorized(&self) -> bool {
            self.auth_is_authorized
        }
        fn auth_is_bot(&self) -> bool {
            self.auth_is_bot
        }
        fn close_flag(&self) -> bool {
            self.close_flag
        }

        fn get_option_boolean(&self, name: &str, default_value: bool) -> bool {
            self.options_boolean
                .get(name)
                .copied()
                .unwrap_or(default_value)
        }
        fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
            self.options_integer
                .get(name)
                .copied()
                .unwrap_or(default_value)
        }
        fn set_option_boolean(&mut self, name: &str, value: bool) {
            self.option_writes.push((name.to_owned(), Some(value)));
        }
        fn set_option_empty(&mut self, name: &str) {
            self.option_writes.push((name.to_owned(), None));
        }

        fn binlog_pmc_get_raw(&mut self, key: &str) -> Vec<u8> {
            self.pmc.get(key).cloned().unwrap_or_default()
        }
        fn binlog_pmc_set_raw(&mut self, key: &str, value: &[u8]) {
            self.pmc.insert(key.to_owned(), value.to_vec());
        }
        fn binlog_pmc_erase(&mut self, key: &str) {
            self.pmc.remove(key);
        }
        fn binlog_pmc_erase_by_prefix(&mut self, prefix: &str) {
            let keys: Vec<String> = self
                .pmc
                .keys()
                .filter(|key| key.as_bytes().starts_with(prefix.as_bytes()))
                .cloned()
                .collect();
            for key in keys {
                self.pmc.remove(&key);
            }
        }

        fn use_chat_info_database(&self) -> bool {
            self.use_chat_info_database
        }

        fn check_dialog_access(
            &mut self,
            dialog_id: DialogId,
            _source: &str,
        ) -> Result<(), Status> {
            if let Some(error) = &self.access_denied {
                return Err(error.clone());
            }
            if dialog_id.get_type() == DialogType::None {
                return Err(Status::error(400, "Chat not found"));
            }
            Ok(())
        }
        fn get_chats_object(
            &mut self,
            total_count: i32,
            dialog_ids: &[DialogId],
            source: &str,
        ) -> td_api::Chats {
            crate::dialog_manager::get_chats_object(total_count, dialog_ids, source)
        }
        fn is_user_deleted(&self, user_id: UserId) -> bool {
            self.deleted_users.contains(&user_id.get())
        }
        fn get_my_id(&self) -> UserId {
            self.my_id
        }
        fn get_bot_data(&self, user_id: UserId) -> Result<BotData, Status> {
            self.bots
                .get(&user_id.get())
                .cloned()
                .ok_or_else(|| Status::error(400, "Bot not found"))
        }
        fn on_need_dialog_stories_changed(&mut self, dialog_id: DialogId) {
            self.story_updates.push(dialog_id.get());
        }
        fn on_get_users(&mut self, _users: Vec<telegram_api::User>, source: &str) {
            self.users_intakes.push(source_name(source));
        }
        fn on_get_chats(&mut self, _chats: Vec<telegram_api::Chat>, source: &str) {
            self.chats_intakes.push(source_name(source));
        }

        fn load_dialogs(&mut self, dialog_ids: Vec<DialogId>) {
            self.load_dialogs_calls
                .push(dialog_ids.iter().map(|id| id.get()).collect());
        }
        fn wait_first_sync(&mut self) {
            self.wait_first_sync_count += 1;
        }

        fn send_toggle_top_peers_query(&mut self, is_enabled: bool) {
            self.toggle_queries.push(is_enabled);
        }
        fn send_get_top_peers_query(&mut self, hash: i64) {
            self.get_top_peers_hashes.push(hash);
        }
        fn send_reset_top_peer_rating_query(
            &mut self,
            category: TopDialogCategory,
            dialog_id: DialogId,
        ) {
            self.reset_queries.push((category, dialog_id.get()));
        }
    }

    // The env trait passes `&str` sources with 'static call sites; the fake
    // records them through a simple normalization.
    fn source_name(source: &str) -> &'static str {
        if source == "on get top chats" {
            "on get top chats"
        } else {
            "other"
        }
    }

    #[test]
    fn category_name_table_matches_the_cpp() {
        assert_eq!(
            get_top_dialog_category_name(TopDialogCategory::Correspondent),
            "correspondent"
        );
        assert_eq!(
            get_top_dialog_category_name(TopDialogCategory::BotPM),
            "bot_pm"
        );
        assert_eq!(
            get_top_dialog_category_name(TopDialogCategory::BotInline),
            "bot_inline"
        );
        assert_eq!(
            get_top_dialog_category_name(TopDialogCategory::Group),
            "group"
        );
        assert_eq!(
            get_top_dialog_category_name(TopDialogCategory::Channel),
            "channel"
        );
        assert_eq!(
            get_top_dialog_category_name(TopDialogCategory::Call),
            "call"
        );
        assert_eq!(
            get_top_dialog_category_name(TopDialogCategory::ForwardUsers),
            "forward_users"
        );
        assert_eq!(
            get_top_dialog_category_name(TopDialogCategory::ForwardChats),
            "forward_chats"
        );
        assert_eq!(
            get_top_dialog_category_name(TopDialogCategory::BotApp),
            "bot_app"
        );
        assert_eq!(
            get_top_dialog_category_name(TopDialogCategory::BotGuest),
            "bot_guest"
        );
    }

    #[test]
    fn td_api_category_mapping_follows_the_local_tree() {
        let map = |ctor: td_api::TopChatCategory| get_top_dialog_category_from_td_api(Some(&ctor));
        assert_eq!(
            map(td_api::TopChatCategory::topChatCategoryUsers(
                td_api::topChatCategoryUsers
            )),
            TopDialogCategory::Correspondent
        );
        assert_eq!(
            map(td_api::TopChatCategory::topChatCategoryBots(
                td_api::topChatCategoryBots
            )),
            TopDialogCategory::BotPM
        );
        assert_eq!(
            map(td_api::TopChatCategory::topChatCategoryInlineBots(
                td_api::topChatCategoryInlineBots
            )),
            TopDialogCategory::BotInline
        );
        assert_eq!(
            map(td_api::TopChatCategory::topChatCategoryGroups(
                td_api::topChatCategoryGroups
            )),
            TopDialogCategory::Group
        );
        assert_eq!(
            map(td_api::TopChatCategory::topChatCategoryChannels(
                td_api::topChatCategoryChannels
            )),
            TopDialogCategory::Channel
        );
        assert_eq!(
            map(td_api::TopChatCategory::topChatCategoryCalls(
                td_api::topChatCategoryCalls
            )),
            TopDialogCategory::Call
        );
        // the local-tree quirk: forwardChats lands in the ForwardUsers bucket
        // (the merge-both-tables one)
        assert_eq!(
            map(td_api::TopChatCategory::topChatCategoryForwardChats(
                td_api::topChatCategoryForwardChats
            )),
            TopDialogCategory::ForwardUsers
        );
        assert_eq!(
            map(td_api::TopChatCategory::topChatCategoryWebAppBots(
                td_api::topChatCategoryWebAppBots
            )),
            TopDialogCategory::BotApp
        );
        assert_eq!(
            map(td_api::TopChatCategory::topChatCategoryGuestBots(
                td_api::topChatCategoryGuestBots
            )),
            TopDialogCategory::BotGuest
        );
        // the C++ null-object arm
        assert_eq!(
            get_top_dialog_category_from_td_api(None),
            TopDialogCategory::Size
        );
    }

    #[test]
    fn wire_category_round_trips_through_the_input_table() {
        let all = [
            TopDialogCategory::Correspondent,
            TopDialogCategory::BotPM,
            TopDialogCategory::BotInline,
            TopDialogCategory::Group,
            TopDialogCategory::Channel,
            TopDialogCategory::Call,
            TopDialogCategory::ForwardUsers,
            TopDialogCategory::ForwardChats,
            TopDialogCategory::BotApp,
            TopDialogCategory::BotGuest,
        ];
        for category in all {
            let wire = get_input_top_peer_category(category);
            assert_eq!(get_top_dialog_category_from_telegram(Some(&wire)), category);
        }
    }

    #[test]
    fn top_dialogs_store_parse_round_trips_byte_exact() {
        let mut out = Vec::new();
        let dialogs = TopDialogs {
            is_dirty: true, // runtime-only, never stored
            rating_timestamp: 1000.5,
            dialogs: vec![
                TopDialog::new(DialogId::new(123), 12.5),
                TopDialog::new(DialogId::new(-4), 0.25),
            ],
        };
        dialogs.store(&mut out);

        // the pinned bytes: f64 timestamp + i32 length + (i64 id + f64 rating)*
        let mut expected = Vec::new();
        expected.extend_from_slice(&1000.5f64.to_le_bytes());
        expected.extend_from_slice(&2i32.to_le_bytes());
        expected.extend_from_slice(&123i64.to_le_bytes());
        expected.extend_from_slice(&12.5f64.to_le_bytes());
        expected.extend_from_slice(&(-4i64).to_le_bytes());
        expected.extend_from_slice(&0.25f64.to_le_bytes());
        assert_eq!(out, expected);

        let mut parser = Parser::new(&out);
        let parsed = TopDialogs::parse(&mut parser).unwrap();
        assert_eq!(parsed.rating_timestamp, 1000.5);
        assert_eq!(parsed.dialogs.len(), 2);
        assert_eq!(parsed.dialogs[0], TopDialog::new(DialogId::new(123), 12.5));
        assert_eq!(parsed.dialogs[1], TopDialog::new(DialogId::new(-4), 0.25));
        assert!(!parsed.is_dirty);
    }

    #[test]
    fn top_dialogs_parse_rejects_negative_size() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0.0f64.to_le_bytes());
        bytes.extend_from_slice(&(-1i32).to_le_bytes());
        let mut parser = Parser::new(&bytes);
        assert!(TopDialogs::parse(&mut parser).is_err());
    }

    #[test]
    fn rating_add_follows_the_decay_formula() {
        let manager = TopDialogManager::new();
        // exp((now - rating_timestamp) / 241920)
        assert!((manager.rating_add(241920.0, 0.0) - std::f64::consts::E).abs() < 1e-9);
        assert_eq!(manager.rating_add(100.0, 100.0), 1.0);
        assert!((manager.rating_add(120960.0, 0.0) - std::f64::consts::E.sqrt()).abs() < 1e-9);
    }

    #[test]
    fn on_dialog_used_inserts_orders_and_marks_unsync() {
        let mut env = FakeEnv::new();
        let mut manager = TopDialogManager::new();

        let chat_a = DialogId::new(1111);
        let chat_b = DialogId::new(2222);
        manager.on_dialog_used(&mut env, TopDialogCategory::Group, chat_a, 10);
        manager.on_dialog_used(&mut env, TopDialogCategory::Group, chat_b, 10);
        manager.on_dialog_used(&mut env, TopDialogCategory::Group, chat_b, 10);

        let group = &manager.by_category[TopDialogCategory::Group.position()];
        assert!(group.is_dirty);
        assert_eq!(group.dialogs.len(), 2);
        // two uses beat one: b first, and both carry e^(10-0/241920) * uses...
        // the rating timestamps start at 0.0, so delta = exp(date/decay)
        let one = manager.rating_add(10.0, 0.0);
        assert!((group.dialogs[1].rating - one).abs() < 1e-9);
        assert!((group.dialogs[0].rating - 2.0 * one).abs() < 1e-9);
        assert!(bool::from(manager.first_unsync_change));
    }

    #[test]
    fn remove_dialog_bands_and_local_removal() {
        let mut env = FakeEnv::new();
        let mut manager = TopDialogManager::new();
        let chat_id = DialogId::new(3333);

        // Size category
        let slot: Slot<()> = Slot::new();
        manager.remove_dialog(
            &mut env,
            TopDialogCategory::Size,
            chat_id,
            Promise::from_interface(slot.clone()),
        );
        let answer = slot.take().unwrap();
        assert_eq!(
            answer.unwrap_err().message(),
            "Top chat category must be non-empty"
        );

        // access denied rides the env band
        env.access_denied = Some(Status::error(400, "Chat not found"));
        let slot: Slot<()> = Slot::new();
        manager.remove_dialog(
            &mut env,
            TopDialogCategory::Group,
            chat_id,
            Promise::from_interface(slot.clone()),
        );
        assert_eq!(
            slot.take().unwrap().unwrap_err().message(),
            "Chat not found"
        );
        env.access_denied = None;

        // disabled: ok without touching the table (after the access check)
        manager.is_enabled = false;
        let slot: Slot<()> = Slot::new();
        manager.remove_dialog(
            &mut env,
            TopDialogCategory::Group,
            chat_id,
            Promise::from_interface(slot.clone()),
        );
        assert!(slot.take().unwrap().is_ok());
        assert!(env.reset_queries.is_empty());
        manager.is_enabled = true;

        // not found: ok, but the fire-and-forget reset query still goes out
        // (the C++ sends it before the membership check,
        // TopDialogManager.cpp:256-262)
        let slot: Slot<()> = Slot::new();
        manager.remove_dialog(
            &mut env,
            TopDialogCategory::Group,
            chat_id,
            Promise::from_interface(slot.clone()),
        );
        assert!(slot.take().unwrap().is_ok());
        assert_eq!(env.reset_queries, vec![(TopDialogCategory::Group, 3333)]);

        // found: another reset query, the row is removed, the answer lands
        // before the story callback
        manager.on_dialog_used(&mut env, TopDialogCategory::Group, chat_id, 10);
        let slot: Slot<()> = Slot::new();
        manager.remove_dialog(
            &mut env,
            TopDialogCategory::Group,
            chat_id,
            Promise::from_interface(slot.clone()),
        );
        assert!(slot.take().unwrap().is_ok());
        assert_eq!(
            env.reset_queries,
            vec![
                (TopDialogCategory::Group, 3333),
                (TopDialogCategory::Group, 3333)
            ]
        );
        assert!(
            manager.by_category[TopDialogCategory::Group.position()]
                .dialogs
                .is_empty()
        );
        assert!(manager.by_category[TopDialogCategory::Group.position()].is_dirty);
    }

    #[test]
    fn get_top_dialogs_bands_and_pending_dispatch() {
        let mut env = FakeEnv::new();
        let mut manager = TopDialogManager::new();
        // the manager is born enabled (TopDialogManager.h:59); the disabled
        // band below rides it explicitly
        manager.is_enabled = false;

        let band = |manager: &mut TopDialogManager, env: &mut FakeEnv, category, limit| {
            let slot: Slot<td_api::Chats> = Slot::new();
            manager.get_top_dialogs(env, category, limit, Promise::from_interface(slot.clone()));
            slot.take().unwrap()
        };

        let err = band(&mut manager, &mut env, TopDialogCategory::Size, 10).unwrap_err();
        assert_eq!(err.message(), "Top chat category must be non-empty");
        let err = band(&mut manager, &mut env, TopDialogCategory::Group, 0).unwrap_err();
        assert_eq!(err.message(), "Limit must be positive");
        let err = band(&mut manager, &mut env, TopDialogCategory::Group, 10).unwrap_err();
        assert_eq!(err.message(), "Top chat computation is disabled");

        // enabled again: the query parks until the machine synchronizes
        manager.is_enabled = true;
        let slot: Slot<td_api::Chats> = Slot::new();
        manager.get_top_dialogs(
            &mut env,
            TopDialogCategory::Group,
            10,
            Promise::from_interface(slot.clone()),
        );
        assert!(slot.take().is_none());
        assert_eq!(manager.pending_get_top_dialogs.len(), 1);
        assert!(env.load_dialogs_calls.is_empty());

        // the first sync starts the server sync (the [blocked: net] query
        // seam), but the parked answer waits: is_synchronized is still false
        // (C++ loop: the dispatch needs is_synchronized_ || !is_enabled_)
        manager.on_first_sync(&mut env);
        assert_eq!(env.get_top_peers_hashes.len(), 1);
        assert!(env.load_dialogs_calls.is_empty());
        assert_eq!(manager.pending_get_top_dialogs.len(), 1);

        // the server answer synchronizes the machine: loop dispatches
        // through the load_dialogs seam (M.10) and clears the queue
        manager.on_get_top_peers(
            &mut env,
            Ok(
                telegram_api::contacts_TopPeers::contacts_topPeersNotModified(
                    telegram_api::contacts_topPeersNotModified,
                ),
            ),
        );
        assert_eq!(env.load_dialogs_calls.len(), 1);
        assert!(manager.pending_get_top_dialogs.is_empty());
    }

    #[test]
    fn is_top_dialog_answers_the_one_zero_minus_one_table() {
        let mut env = FakeEnv::new();
        let mut manager = TopDialogManager::new();
        let chat_id = DialogId::new(4444);
        manager.on_dialog_used(&mut env, TopDialogCategory::Group, chat_id, 10);

        assert_eq!(
            manager.is_top_dialog(&env, TopDialogCategory::Group, 5, chat_id),
            1
        );
        // not synchronized: an absent dialog answers -1
        assert_eq!(
            manager.is_top_dialog(&env, TopDialogCategory::Group, 5, DialogId::new(5555)),
            -1
        );
        // synchronized: 0
        manager.is_synchronized = true;
        assert_eq!(
            manager.is_top_dialog(&env, TopDialogCategory::Group, 5, DialogId::new(5555)),
            0
        );
        // disabled: 0 even for a present dialog
        manager.is_enabled = false;
        assert_eq!(
            manager.is_top_dialog(&env, TopDialogCategory::Group, 5, chat_id),
            0
        );
    }

    #[test]
    fn story_dialog_ids_filter_by_rating_and_cap_at_thirty() {
        let mut env = FakeEnv::new();
        let mut manager = TopDialogManager::new();

        // 30 users: 31 of them get a rating that crosses MIN_STORY_RATING...
        // only the first 30 are collected
        for i in 1..=31 {
            let dialog_id = DialogId::new(10_000 + i);
            for _ in 0..5 {
                manager.on_dialog_used(&mut env, TopDialogCategory::Correspondent, dialog_id, 10);
            }
        }
        // 5 * exp(10/241920) ≈ 5.0002 < 10 — no stories yet
        assert!(manager.get_story_dialog_ids(&env).is_empty());

        // push the first dialog over MIN_STORY_RATING
        for _ in 0..5 {
            manager.on_dialog_used(
                &mut env,
                TopDialogCategory::Correspondent,
                DialogId::new(10_001),
                10,
            );
        }
        // 10 * exp(10/241920) ≈ 10.0004 >= 10
        let ids = manager.get_story_dialog_ids(&env);
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].get(), 10_001);

        // disabled: empty
        manager.is_enabled = false;
        assert!(manager.get_story_dialog_ids(&env).is_empty());
    }

    #[test]
    fn on_get_top_peers_applies_the_wire_result() {
        let mut env = FakeEnv::new();
        let mut manager = TopDialogManager::new();
        let pre_rating = DialogId::new(7001);
        manager.on_dialog_used(&mut env, TopDialogCategory::Correspondent, pre_rating, 10);

        let make_peer = |user_id: i64, rating: f64| telegram_api::TopPeer {
            peer: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                user_id,
            })),
            rating,
        };
        let result = Ok(telegram_api::contacts_TopPeers::contacts_topPeers(
            telegram_api::contacts_topPeers {
                categories: vec![
                    telegram_api::TopPeerCategoryPeers {
                        category: Some(
                            telegram_api::TopPeerCategory::topPeerCategoryCorrespondents(
                                telegram_api::topPeerCategoryCorrespondents,
                            ),
                        ),
                        count: 2,
                        peers: vec![make_peer(100, 42.0), make_peer(101, 41.0)],
                    },
                    telegram_api::TopPeerCategoryPeers {
                        category: Some(telegram_api::TopPeerCategory::topPeerCategoryChannels(
                            telegram_api::topPeerCategoryChannels,
                        )),
                        count: 1,
                        peers: vec![make_peer(-1_000_000_100, 7.0)],
                    },
                ],
                chats: vec![],
                users: vec![],
            },
        ));

        manager.on_get_top_peers(&mut env, result);

        assert!(manager.is_synchronized);
        assert_eq!(manager.server_sync_state, SyncState::Ok);
        assert_eq!(env.users_intakes, vec!["on get top chats"]);
        assert_eq!(env.chats_intakes, vec!["on get top chats"]);
        // the pre-existing rating row was replaced by the server table
        let correspondent = &manager.by_category[TopDialogCategory::Correspondent.position()];
        assert_eq!(correspondent.dialogs.len(), 2);
        assert_eq!(correspondent.dialogs[0].dialog_id.get(), 100);
        assert_eq!(correspondent.dialogs[0].rating, 42.0);
        // the channel table landed too
        assert_eq!(
            manager.by_category[TopDialogCategory::Channel.position()]
                .dialogs
                .len(),
            1
        );
        // the pmc timestamp stores the system clock
        assert_eq!(
            env.pmc.get("top_dialogs_ts").map(|v| v.as_slice()),
            Some(b"1700000000".as_slice())
        );
        // the story set flipped [] -> [100, 101]: both new correspondents
        // crossed MIN_STORY_RATING on the wire replacement
        // (TopDialogManager.cpp:548-574 fires the change per boundary)
        assert_eq!(env.story_updates, vec![100, 101]);
    }

    #[test]
    fn on_get_top_peers_disabled_turns_the_machine_off() {
        let mut env = FakeEnv::new();
        let mut manager = TopDialogManager::new();
        manager.on_get_top_peers(
            &mut env,
            Ok(telegram_api::contacts_TopPeers::contacts_topPeersDisabled(
                telegram_api::contacts_topPeersDisabled,
            )),
        );
        assert!(!manager.is_enabled);
        assert_eq!(
            env.option_writes,
            vec![("disable_top_chats".to_string(), Some(true))]
        );
        assert!(manager.is_synchronized);
    }

    #[test]
    fn on_get_top_peers_error_reschedules_the_daily_sync() {
        let mut env = FakeEnv::new();
        let mut manager = TopDialogManager::new();
        manager.is_synchronized = true;
        manager.server_sync_state = SyncState::Ok;
        manager.last_server_sync = Timestamp::at_time(env.now);

        manager.on_get_top_peers(&mut env, Err(Status::error(429, "Too Many Requests")));

        // last_server_sync = now + (SERVER_SYNC_RESEND_DELAY -
        // SERVER_SYNC_DELAY) = now + (60 - 86400)
        assert!((manager.last_server_sync.at() - (1000.0 - 86340.0)).abs() < 1e-9);
        // still Ok, not in the past: the wakeup lands at ts + 86400
        assert_eq!(manager.server_sync_state, SyncState::Ok);
        let deadline = manager.next_deadline().unwrap();
        assert!((deadline - (1000.0 - 86340.0 + 86400.0)).abs() < 1e-9);
    }

    #[test]
    fn normalize_rating_decays_by_server_time_delta() {
        let mut env = FakeEnv::new();
        let mut manager = TopDialogManager::new();
        manager.init(&mut env); // start_up: pins rating_timestamp at the server time
        let dialog_id = DialogId::new(8_001);
        // cross the story threshold before the decay
        for _ in 0..3 {
            manager.on_dialog_used(&mut env, TopDialogCategory::Correspondent, dialog_id, 10);
        }
        // 3 * e^(10/241920) ≈ 3.0 < 10
        assert!(manager.get_story_dialog_ids(&env).is_empty());
        for _ in 0..20 {
            manager.on_dialog_used(&mut env, TopDialogCategory::Correspondent, dialog_id, 10);
        }
        assert!(manager.get_story_dialog_ids(&env).len() == 1);

        // advance the server clock by one decay period: every rating is
        // divided by e
        let before =
            manager.by_category[TopDialogCategory::Correspondent.position()].dialogs[0].rating;
        env.server_time += 241920.0;
        manager.on_get_top_peers(
            &mut env,
            Ok(
                telegram_api::contacts_TopPeers::contacts_topPeersNotModified(
                    telegram_api::contacts_topPeersNotModified,
                ),
            ),
        );
        let after =
            manager.by_category[TopDialogCategory::Correspondent.position()].dialogs[0].rating;
        assert!((after - before / std::f64::consts::E).abs() < 1e-9);
        // and the story flag flipped on during the uses, then off in the
        // decay — one change callback per boundary
        assert_eq!(env.story_updates, vec![8_001, 8_001]);
    }

    #[test]
    fn do_save_top_dialogs_writes_the_pmc_keys() {
        let mut env = FakeEnv::new();
        let mut manager = TopDialogManager::new();
        manager.on_dialog_used(&mut env, TopDialogCategory::Group, DialogId::new(9_001), 10);

        // reach the saving state: server sync lands, then the loop saves the
        // dirty categories after the db delay
        manager.on_get_top_peers(
            &mut env,
            Ok(
                telegram_api::contacts_TopPeers::contacts_topPeersNotModified(
                    telegram_api::contacts_topPeersNotModified,
                ),
            ),
        );
        env.now += DB_SYNC_DELAY as f64 + 1.0;
        manager.loop_(&mut env);

        assert!(env.pmc.contains_key("top_dialogs#correspondent"));
        let saved = env.pmc.get("top_dialogs#group").unwrap();
        let mut parser = Parser::new_log_event(saved.as_slice()).unwrap();
        let parsed = TopDialogs::parse(&mut parser).unwrap();
        assert_eq!(parsed.dialogs.len(), 1);
        assert_eq!(parsed.dialogs[0].dialog_id.get(), 9_001);
        assert_eq!(manager.db_sync_state, SyncState::Ok);
        assert!(!bool::from(manager.first_unsync_change));
    }

    #[test]
    fn init_restores_from_the_pmc_and_pins_the_sync_state() {
        let mut env = FakeEnv::new();
        // a stale (in-the-past) ts: the server sync counts as done, the
        // chat-info database keeps the machine synchronized
        env.pmc.insert(
            "top_dialogs_ts".to_owned(),
            (env.system as u32 - 100_000).to_string().into_bytes(),
        );
        let mut dialogs = TopDialogs {
            is_dirty: false,
            rating_timestamp: env.server_time - 241920.0,
            dialogs: vec![TopDialog::new(DialogId::new(9_500), 20.0)],
        };
        let mut stored = Vec::new();
        dialogs.rating_timestamp -= 0.0;
        crate::store_parse::store_log_event_version(&mut stored);
        dialogs.store(&mut stored);
        env.pmc.insert("top_dialogs#group".to_owned(), stored);

        let mut manager = TopDialogManager::new();
        manager.init(&mut env);

        assert!(manager.is_synchronized);
        // the stale (in-the-past) ts pins Ok in try_start, but init's tail
        // loop() demotes it back to None — the same C++ init -> try_start ->
        // lambda() path — so the machine re-syncs after the first sync
        assert_eq!(manager.server_sync_state, SyncState::None);
        // restored and normalized: the timestamp moved to the server time and
        // the rating decayed by one period
        let group = &manager.by_category[TopDialogCategory::Group.position()];
        assert_eq!(group.dialogs[0].dialog_id.get(), 9_500);
        assert!((group.rating_timestamp - env.server_time).abs() < 1e-9);
        assert!((group.dialogs[0].rating - 20.0 / std::f64::consts::E).abs() < 1e-6);
        // the state machine waits for the first sync
        assert_eq!(env.wait_first_sync_count, 1);
        // no toggle pmc record: no toggle query
        assert!(env.toggle_queries.is_empty());
    }

    #[test]
    fn init_sends_the_stored_toggle_query() {
        let mut env = FakeEnv::new();
        env.pmc
            .insert("top_peers_enabled".to_owned(), b"1".to_vec());
        let mut manager = TopDialogManager::new();
        manager.init(&mut env);
        assert_eq!(env.toggle_queries, vec![true]);
        assert!(manager.have_toggle_top_peers_query);

        // the callback erases the pmc record once the server confirms
        manager.on_toggle_top_peers(&mut env, true, Ok(()));
        assert!(!env.pmc.contains_key("top_peers_enabled"));
        assert!(!manager.have_toggle_top_peers_query);
    }

    #[test]
    fn disabled_init_erases_the_stored_tables() {
        let mut env = FakeEnv::new();
        env.options_boolean
            .insert("disable_top_chats".to_owned(), true);
        env.pmc.insert("top_dialogs#group".to_owned(), {
            let mut out = Vec::new();
            TopDialogs {
                is_dirty: false,
                rating_timestamp: 1.0,
                dialogs: vec![TopDialog::new(DialogId::new(5), 1.0)],
            }
            .store(&mut out);
            out
        });
        let mut manager = TopDialogManager::new();
        manager.init(&mut env);
        assert!(!manager.is_enabled);
        assert!(!env.pmc.contains_key("top_dialogs#group"));
        // disabled machine: get_top_dialogs answers the disabled band
        let slot: Slot<td_api::Chats> = Slot::new();
        manager.get_top_dialogs(
            &mut env,
            TopDialogCategory::Group,
            10,
            Promise::from_interface(slot.clone()),
        );
        assert_eq!(
            slot.take().unwrap().unwrap_err().message(),
            "Top chat computation is disabled"
        );
        // the stored ts still pins the server sync state
        // (is_synchronized stays false without the chat-info database)
    }

    #[test]
    fn on_load_dialogs_filters_and_answers_the_chats_object() {
        let mut env = FakeEnv::new();
        env.deleted_users.insert(102);
        env.bots
            .insert(103, FakeEnv::make_bot("bot", /* is_inline */ false, false));
        env.bots.insert(104, FakeEnv::make_bot("inl", true, false));
        let manager = TopDialogManager::new();

        let ids = vec![
            DialogId::new(101), // not a bot: skipped in the BotInline category
            DialogId::new(102), // deleted
            DialogId::new(1),   // self (my_id)
            DialogId::new(103), // non-inline bot
            DialogId::new(104), // inline bot, kept
        ];
        let slot: Slot<td_api::Chats> = Slot::new();
        let query = GetTopDialogsQuery {
            category: TopDialogCategory::BotInline,
            limit: 10,
            promise: Promise::from_interface(slot.clone()),
        };
        manager.on_load_dialogs(&mut env, query, ids);

        let chats: td_api::Chats = slot.take().unwrap().unwrap();
        // -1 total_count: the answer's total is the list size
        assert_eq!(chats.total_count, 1);
        assert_eq!(chats.chat_ids, vec![104i64]);
    }
}
