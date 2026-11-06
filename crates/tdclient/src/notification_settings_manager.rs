//! `NotificationSettingsManager` (NotificationSettingsManager.cpp) — the
//! scope/reaction notification settings state, its pmc persistence and the
//! on-server sync log events (M.13S.2).
//!
//! The C++ actor collapses into a plain struct owned by [`Td`](crate::td::Td)
//! (the M.13 convention): timeouts become deadline maps merged into the Td
//! timeout slot, `send_closure(G()->td(), &Td::send_update, ...)`
//! becomes `env.send_update`, and the pmc/binlog access runs through the
//! env.
//!
//! The net families split at the net-drain 40 open (verified against the
//! C++ ResultHandler table):
//!
//! * **Ungated — landed on the net plane** (the
//!   `NotificationSettingsNetKind` dispatch): the five scope/reaction
//!   settings queries — Get/UpdateScopeNotifySettings
//!   (`account.{get,update}NotifySettings` over the scope InputNotifyPeer,
//!   cpp:341-371/453-493), Get/SetReactionsNotifySettings
//!   (`account.{get,set}ReactionsNotifySettings`, cpp:373-399/495-528) and
//!   ResetNotifySettings (`account.resetNotifySettings`, cpp:530-560).
//! * **Files-coupled — parked**: the ringtones family
//!   (Upload/Save/GetSavedRingtones, cpp:54-186) — the upload leg builds
//!   an `InputFile` through the file-manager upload machinery and cleans
//!   partial remote locations, the save leg resolves the ringtone
//!   `FileId`/file-reference through the documents plane, and the saved
//!   list answer is `vector<Document>` parsed into notification sounds.
//! * **Dialog-plane-coupled — parked** (the M.6/M.10 external plane, not
//!   Files): Get/UpdateDialogNotifySettings and the two
//!   NotifyExceptions queries (cpp:188-339/401-451) — the send legs build
//!   their InputNotifyPeer from the dialog access checks
//!   (`get_input_notify_peer(dialog_id, forum_topic_id)`) and the answers
//!   route into the messages/forum-topic/updates intakes.

use std::collections::BTreeMap;

use crate::notification_manager::NotificationManagerEnv;
use crate::notification_settings::{
    NotificationSettingsScope, ReactionNotificationSettings, ScopeNotificationSettings,
    are_different_equivalent_notification_sounds, are_equivalent_notification_sounds,
    dup_notification_sound, get_input_notify_peer_for_scope,
    get_notification_settings_scope_object, get_scope_notification_settings_from_td_api,
    get_scope_notification_settings_from_telegram, get_scope_notification_settings_object,
    is_notification_sound_default,
};
use crate::store_parse::{Parser, store_i32, store_log_event_version};
use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `LogEvent::HandlerType::UpdateScopeNotificationSettingsOnServer`
/// (LogEvent.h:89).
pub const UPDATE_SCOPE_NOTIFICATION_SETTINGS_ON_SERVER: i32 = 0x110;
/// `LogEvent::HandlerType::ResetAllNotificationSettingsOnServer`
/// (LogEvent.h:90).
pub const RESET_ALL_NOTIFICATION_SETTINGS_ON_SERVER: i32 = 0x111;
/// `LogEvent::HandlerType::UpdateReactionNotificationSettingsOnServer`
/// (LogEvent.h:108).
pub const UPDATE_REACTION_NOTIFICATION_SETTINGS_ON_SERVER: i32 = 0x129;

/// `get_notification_settings_scope_database_key`
/// (NotificationSettingsManager.cpp:811-823).
fn get_notification_settings_scope_database_key(scope: NotificationSettingsScope) -> &'static [u8] {
    match scope {
        NotificationSettingsScope::Private => b"nsfpc",
        NotificationSettingsScope::Group => b"nsfgc",
        NotificationSettingsScope::Channel => b"nsfcc",
    }
}

/// `get_reaction_notification_settings_database_key`
/// (NotificationSettingsManager.cpp:924-926).
fn get_reaction_notification_settings_database_key() -> &'static [u8] {
    b"rns"
}

/// The `G()`/`td_` surface of the manager. The pmc/clock/update surfaces
/// come from [`NotificationManagerEnv`]; everything the C++ reaches
/// through the network or the file managers is a seam default.
pub trait NotificationSettingsManagerEnv: NotificationManagerEnv {
    /// `td_->auth_manager_->is_authorized()`.
    fn auth_is_authorized(&self) -> bool;

    /// `td_->auth_manager_->was_authorized()` — the init gate
    /// (NotificationSettingsManager.cpp:631).
    fn auth_was_authorized(&self) -> bool;

    /// `td_->auth_manager_->is_bot()`.
    fn auth_is_bot(&self) -> bool;

    /// `G()->td_db()->get_binlog_pmc()->get` over the raw (binary) values
    /// the settings log-event blobs need.
    fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;

    /// `G()->td_db()->get_binlog_pmc()->set` for binary values.
    fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]);

    /// `binlog_add(G()->td_db()->get_binlog(), type, data)`.
    fn binlog_add(&mut self, handler_type: i32, data: &[u8]) -> u64;

    /// `td_->messages_manager_->remove_scope_pinned_message_notifications`
    /// — seam until M.10.
    fn remove_scope_pinned_message_notifications(&mut self, _scope: NotificationSettingsScope) {}

    /// `td_->messages_manager_->on_update_scope_mention_notifications` —
    /// seam until M.10.
    fn on_update_scope_mention_notifications(
        &mut self,
        _scope: NotificationSettingsScope,
        _disable: bool,
    ) {
    }

    /// `td_->messages_manager_->on_update_notification_scope_is_muted` —
    /// seam until M.10.
    fn on_update_notification_scope_is_muted(
        &mut self,
        _scope: NotificationSettingsScope,
        _is_muted: bool,
    ) {
    }

    /// `td_->messages_manager_->reset_all_dialog_notification_settings` —
    /// seam until M.10.
    fn reset_all_dialog_notification_settings(&mut self) {}
}

/// The notification-settings-plane in-flight query kinds — the C++
/// `Td::ResultHandler` classes of the ungated net-drain 40 families
/// (NotificationSettingsManager.cpp:341-560). The completions park by
/// kind behind the manager-held net handle
/// (`crate::td::NotificationSettingsNetHandle`);
/// `Td::on_notification_settings_net_answer` routes them. The
/// Files-coupled ringtones family and the dialog-coupled families stay
/// parked on their planes (the module doc map).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotificationSettingsNetKind {
    /// `GetScopeNotifySettingsQuery` (cpp:341-371) —
    /// `account.getNotifySettings(get_input_notify_peer(scope))`; the
    /// query promise parks in the manager's scope-get lot (the empty
    /// default for the init/repair sends, the request promise for the
    /// `getScopeNotificationSettings` request).
    GetScope { scope: NotificationSettingsScope },
    /// `GetReactionsNotifySettingsQuery` (cpp:373-399) —
    /// `account.getReactionsNotifySettings()`; the promise is always the
    /// empty `Promise<Unit>()` on this line (the init fetch and the
    /// set-error repair tail).
    GetReactions,
    /// `UpdateScopeNotifySettingsQuery` (cpp:453-493) —
    /// `account.updateNotifySettings(...)` over the current scope
    /// settings; the query promise is the erase-log-event wrapper over
    /// the saved log event (the parked id lot carries the erase half).
    UpdateScope { scope: NotificationSettingsScope },
    /// `SetReactionsNotifySettingsQuery` (cpp:495-528) —
    /// `account.setReactionsNotifySettings(...)`; the erase-log-event
    /// wrapper rides the parked id lot.
    SetReactions,
    /// `ResetNotifySettingsQuery` (cpp:530-560) —
    /// `account.resetNotifySettings()`; the erase-log-event wrapper rides
    /// the parked id lot.
    Reset,
}

/// The manager state — the C++ member fields of
/// `NotificationSettingsManager` that survive on this line.
#[derive(Default)]
pub struct NotificationSettingsManager {
    users_notification_settings: ScopeNotificationSettings,
    chats_notification_settings: ScopeNotificationSettings,
    channels_notification_settings: ScopeNotificationSettings,
    reaction_notification_settings: ReactionNotificationSettings,
    have_reaction_notification_settings: bool,
    is_inited: bool,
    /// The `scope_unmute_timeout_` MultiTimeout slots; the C++ slot id is
    /// `scope + 1`, the map is keyed by the plain scope value. Deadlines
    /// are monotonic (`now()`) seconds, the M.13 collapse convention.
    scope_unmute_timeout: BTreeMap<i32, f64>,
    /// The Get-scope family's parked query promises — the C++ promise
    /// rides its own query (the request promise from
    /// `get_scope_notification_settings`, the empty `Promise<Unit>()`
    /// from the init/repair sends); the answers pop the scope's FIFO
    /// head (§5 — the net completes same-family queries in dispatch
    /// order).
    pending_scope_get: Vec<(
        NotificationSettingsScope,
        Promise<td_api::ScopeNotificationSettings>,
    )>,
    /// The Update-scope family's parked log-event ids — the
    /// `get_erase_log_event_promise` halves (LogEventHelper.cpp:42-52):
    /// the answers erase the binlog mirror on settlement, success or
    /// error, unless the td is closing.
    pending_update_scope: Vec<u64>,
    /// The Set-reactions family's parked log-event ids.
    pending_set_reaction: Vec<u64>,
    /// The Reset family's parked log-event ids.
    pending_reset: Vec<u64>,
    /// The net dispatch surface — the `td_` back-pointer the C++ manager
    /// uses for its query sends, narrowed to the net plane (`None` in the
    /// net-inert tests; set at the manager install, §0's shared-env
    /// shape).
    pub(crate) net: Option<crate::td::NotificationSettingsNetHandle>,
}

impl NotificationSettingsManager {
    pub fn new() -> Self {
        NotificationSettingsManager::default()
    }

    /// The `td_->create_handler<…>()->send(...)` collapse — the wire
    /// query construction lives at the manager send sites; `None` keeps
    /// the net-inert posture (the tests drive the answer halves).
    fn send_notification_settings_net_query(
        &self,
        kind: NotificationSettingsNetKind,
        function: telegram_api::Function,
        what: &str,
    ) {
        match &self.net {
            Some(net) => net.dispatch(kind, function),
            None => tdutils::log_info!("Send {what} — [net-inert]"),
        }
    }

    /// The Get-family promise pop for the
    /// `Td::on_notification_settings_net_answer` drain — the FIFO head
    /// of the scope's lot (the C++ promise rides its own query; the net
    /// completes the same-family queries in dispatch order).
    pub(crate) fn take_scope_get_promise(
        &mut self,
        scope: NotificationSettingsScope,
    ) -> Option<Promise<td_api::ScopeNotificationSettings>> {
        let index = self
            .pending_scope_get
            .iter()
            .position(|&(parked_scope, _)| parked_scope == scope)?;
        Some(self.pending_scope_get.remove(index).1)
    }

    /// The Update-scope family's parked log-event id pop — the
    /// erase-log-event promise half.
    pub(crate) fn take_update_scope_log_event(&mut self) -> Option<u64> {
        pop_head(&mut self.pending_update_scope)
    }

    /// The Set-reactions family's parked log-event id pop.
    pub(crate) fn take_set_reaction_log_event(&mut self) -> Option<u64> {
        pop_head(&mut self.pending_set_reaction)
    }

    /// The Reset family's parked log-event id pop.
    pub(crate) fn take_reset_log_event(&mut self) -> Option<u64> {
        pop_head(&mut self.pending_reset)
    }

    fn get_scope(&self, scope: NotificationSettingsScope) -> &ScopeNotificationSettings {
        match scope {
            NotificationSettingsScope::Private => &self.users_notification_settings,
            NotificationSettingsScope::Group => &self.chats_notification_settings,
            NotificationSettingsScope::Channel => &self.channels_notification_settings,
        }
    }

    fn get_scope_mut(
        &mut self,
        scope: NotificationSettingsScope,
    ) -> &mut ScopeNotificationSettings {
        match scope {
            NotificationSettingsScope::Private => &mut self.users_notification_settings,
            NotificationSettingsScope::Group => &mut self.chats_notification_settings,
            NotificationSettingsScope::Channel => &mut self.channels_notification_settings,
        }
    }

    /// `is_active` (NotificationSettingsManager.cpp:980-982).
    pub fn is_active(&self, env: &impl NotificationSettingsManagerEnv) -> bool {
        !env.close_flag() && env.auth_is_authorized() && !env.auth_is_bot()
    }

    /// `init` (NotificationSettingsManager.cpp:627-670): the pmc loads,
    /// the unmute schedules, the update pushes and the scope fetch
    /// queries (the net plane; `None` keeps the net-inert posture).
    pub fn init(&mut self, env: &mut impl NotificationSettingsManagerEnv) {
        if self.is_inited {
            return;
        }
        let was_authorized_user = env.auth_was_authorized() && !env.auth_is_bot();
        if !was_authorized_user {
            return;
        }
        self.is_inited = true;

        let unix_time = env.unix_time();
        for scope in [
            NotificationSettingsScope::Private,
            NotificationSettingsScope::Group,
            NotificationSettingsScope::Channel,
        ] {
            let notification_settings_string =
                env.pmc_get_raw(get_notification_settings_scope_database_key(scope));
            if !notification_settings_string.is_empty() {
                // `log_event_parse(*current_settings, ...).ensure()` — a
                // corrupt blob aborts the C++; here it logs and keeps the
                // defaults (the collapsed-process divergence).
                let parse = || -> Result<ScopeNotificationSettings, &'static str> {
                    let mut parser = Parser::new_log_event(&notification_settings_string)?;
                    ScopeNotificationSettings::parse(&mut parser)
                };
                match parse() {
                    Ok(current_settings) => {
                        let mute_until = current_settings.mute_until;
                        *self.get_scope_mut(scope) = current_settings;
                        self.schedule_scope_unmute(env, scope, mute_until, unix_time);
                        let update =
                            self.get_update_scope_notification_settings_object_at(scope, unix_time);
                        env.send_update(update);
                    }
                    Err(error) => {
                        tdutils::log_error!(
                            "Failed to load notification settings in {scope:?}: {error}"
                        );
                    }
                }
            }
            if !self.get_scope(scope).is_synchronized {
                self.send_get_scope_notification_settings_query(env, scope, Promise::default());
            }
        }

        let reaction_notification_settings_string =
            env.pmc_get_raw(get_reaction_notification_settings_database_key());
        if !reaction_notification_settings_string.is_empty() {
            let parse = || -> Result<ReactionNotificationSettings, &'static str> {
                let mut parser = Parser::new_log_event(&reaction_notification_settings_string)?;
                ReactionNotificationSettings::parse(&mut parser)
            };
            match parse() {
                Ok(reaction_notification_settings) => {
                    self.reaction_notification_settings = reaction_notification_settings;
                    self.have_reaction_notification_settings = true;
                }
                Err(error) => {
                    tdutils::log_error!("Failed to load reaction notification settings: {error}");
                }
            }
        } else {
            self.send_get_reaction_notification_settings_query(env);
        }
        env.send_update(self.get_update_reaction_notification_settings_object());

        // the story-notification settings announcement key, dead since the
        // story release (cpp:669)
        env.pmc_erase("nsfac");
    }

    /// `send_get_scope_notification_settings_query`
    /// (NotificationSettingsManager.cpp:1546-1554): the bot gate errors
    /// the promise (500, "Wrong getScopeNotificationSettings query"); the
    /// query rides the net plane and the promise parks in the family lot —
    /// the request promise from the request path, the empty default from
    /// the init/repair sends (the C++ `Promise<Unit>()`).
    pub(crate) fn send_get_scope_notification_settings_query(
        &mut self,
        env: &impl NotificationSettingsManagerEnv,
        scope: NotificationSettingsScope,
        mut promise: Promise<td_api::ScopeNotificationSettings>,
    ) {
        if env.auth_is_bot() {
            tdutils::log_error!("Can't get notification settings for {scope:?}");
            promise.set_error(Status::error(
                500,
                "Wrong getScopeNotificationSettings query",
            ));
            return;
        }
        self.pending_scope_get.push((scope, promise));
        self.send_notification_settings_net_query(
            NotificationSettingsNetKind::GetScope { scope },
            telegram_api::Function::account_getNotifySettings(
                telegram_api::account_getNotifySettings {
                    peer: Some(get_input_notify_peer_for_scope(scope)),
                },
            ),
            "account.getNotifySettings",
        );
    }

    /// `send_get_reaction_notification_settings_query`
    /// (NotificationSettingsManager.cpp:893-900): the bot gate errors the
    /// C++ promise — every live caller on this line passes the empty
    /// `Promise<Unit>()` (the init fetch and the set-error repair tail),
    /// so nothing parks; the answer runs the landed
    /// [`NotificationSettingsManager::on_update_reaction_notification_settings`]
    /// intake.
    pub(crate) fn send_get_reaction_notification_settings_query(
        &mut self,
        env: &impl NotificationSettingsManagerEnv,
    ) {
        if env.auth_is_bot() {
            tdutils::log_error!("Can't get reaction notification settings");
            return;
        }
        self.send_notification_settings_net_query(
            NotificationSettingsNetKind::GetReactions,
            telegram_api::Function::account_getReactionsNotifySettings(
                telegram_api::account_getReactionsNotifySettings,
            ),
            "account.getReactionsNotifySettings",
        );
    }

    /// `get_scope_mute_until` (NotificationSettingsManager.cpp:689-691).
    pub fn get_scope_mute_until(&self, scope: NotificationSettingsScope) -> i32 {
        self.get_scope(scope).mute_until
    }

    /// `get_scope_mute_stories` (cpp:693-696).
    pub fn get_scope_mute_stories(&self, scope: NotificationSettingsScope) -> (bool, bool) {
        let settings = self.get_scope(scope);
        (settings.use_default_mute_stories, settings.mute_stories)
    }

    /// `get_scope_notification_sound` (cpp:698-701).
    pub fn get_scope_notification_sound(
        &self,
        scope: NotificationSettingsScope,
    ) -> &Option<crate::notification_settings::NotificationSound> {
        &self.get_scope(scope).sound
    }

    /// `get_scope_story_notification_sound` (cpp:703-706).
    pub fn get_scope_story_notification_sound(
        &self,
        scope: NotificationSettingsScope,
    ) -> &Option<crate::notification_settings::NotificationSound> {
        &self.get_scope(scope).story_sound
    }

    /// `get_scope_show_preview` (cpp:708-710).
    pub fn get_scope_show_preview(&self, scope: NotificationSettingsScope) -> bool {
        self.get_scope(scope).show_preview
    }

    /// `get_scope_hide_story_sender` (cpp:712-714).
    pub fn get_scope_hide_story_sender(&self, scope: NotificationSettingsScope) -> bool {
        self.get_scope(scope).hide_story_sender
    }

    /// `get_scope_disable_pinned_message_notifications` (cpp:716-719).
    pub fn get_scope_disable_pinned_message_notifications(
        &self,
        scope: NotificationSettingsScope,
    ) -> bool {
        self.get_scope(scope).disable_pinned_message_notifications
    }

    /// `get_scope_disable_mention_notifications` (cpp:721-723).
    pub fn get_scope_disable_mention_notifications(
        &self,
        scope: NotificationSettingsScope,
    ) -> bool {
        self.get_scope(scope).disable_mention_notifications
    }

    /// `get_scope_notification_settings(scope, promise)` — the request
    /// path (cpp:1533-1544): an unsynchronized scope defers into the
    /// fetch — the promise parks on the query (the C++ RequestActor
    /// defer, Requests.cpp:1340-1364) and the answer drain resolves it
    /// with the now-synchronized state (the do_run re-run); otherwise the
    /// promise settles inline with the synchronized object (the C++
    /// `promise.set_value(Unit())` + `do_send_result` collapse — the
    /// promise is the answer path, both bands).
    pub fn get_scope_notification_settings_for_request(
        &mut self,
        env: &impl NotificationSettingsManagerEnv,
        scope: NotificationSettingsScope,
        mut promise: Promise<td_api::ScopeNotificationSettings>,
    ) {
        if !self.get_scope(scope).is_synchronized && !env.auth_is_bot() {
            self.send_get_scope_notification_settings_query(env, scope, promise);
            return;
        }
        let unix_time = env.unix_time();
        let object = get_scope_notification_settings_object(self.get_scope(scope), unix_time);
        promise.set_value(object);
    }

    /// The synchronized-state object builder for the Get-family answer
    /// drain — the `GetScopeNotificationSettingsRequest::do_send_result`
    /// half (Requests.cpp:1352-1361): after the query answer the scope is
    /// synchronized and the object builds from the state.
    pub(crate) fn scope_settings_object(
        &self,
        scope: NotificationSettingsScope,
        unix_time: i32,
    ) -> td_api::ScopeNotificationSettings {
        get_scope_notification_settings_object(self.get_scope(scope), unix_time)
    }

    /// `get_update_scope_notification_settings_object` (cpp:770-776); the
    /// C++ reads `G()->unix_time()` inside the object builder
    /// (ScopeNotificationSettings.cpp:28-43), so the builder carries it.
    pub fn get_update_scope_notification_settings_object_at(
        &self,
        scope: NotificationSettingsScope,
        unix_time: i32,
    ) -> td_api::Update {
        td_api::Update::updateScopeNotificationSettings(td_api::updateScopeNotificationSettings {
            scope: Some(get_notification_settings_scope_object(scope)),
            notification_settings: Some(get_scope_notification_settings_object(
                self.get_scope(scope),
                unix_time,
            )),
        })
    }

    /// `get_update_reaction_notification_settings_object` (cpp:778-782).
    pub fn get_update_reaction_notification_settings_object(&self) -> td_api::Update {
        td_api::Update::updateReactionNotificationSettings(
            td_api::updateReactionNotificationSettings {
                notification_settings: Some(
                    self.reaction_notification_settings
                        .get_reaction_notification_settings_object(),
                ),
            },
        )
    }

    /// `on_scope_unmute` (cpp:784-809).
    pub fn on_scope_unmute(
        &mut self,
        env: &mut impl NotificationSettingsManagerEnv,
        scope: NotificationSettingsScope,
    ) {
        if env.auth_is_bot() {
            // just in case
            return;
        }

        let unix_time = env.unix_time();
        let old_mute_until = self.get_scope(scope).mute_until;
        if old_mute_until == 0 {
            return;
        }
        if old_mute_until > unix_time {
            tdutils::log_info!(
                "Failed to unmute {scope:?} in {unix_time}, will be unmuted in {old_mute_until}"
            );
            self.schedule_scope_unmute(env, scope, old_mute_until, unix_time);
            return;
        }

        tdutils::log_info!("Unmute {scope:?}");
        // `update_scope_unmute_timeout(scope, mute_until, 0)` — the write
        // and the muted-changed fan-out (cpp:942-965)
        self.scope_unmute_timeout.remove(&(scope.as_i32()));
        let was_muted = old_mute_until != 0;
        self.get_scope_mut(scope).mute_until = 0;
        if was_muted {
            env.on_update_notification_scope_is_muted(scope, false);
        }
        let update = self.get_update_scope_notification_settings_object_at(scope, unix_time);
        env.send_update(update);
        let settings = self.get_scope(scope).clone();
        self.save_scope_notification_settings(env, scope, &settings);
    }

    /// `save_scope_notification_settings` (cpp:825-829).
    fn save_scope_notification_settings(
        &mut self,
        env: &mut impl NotificationSettingsManagerEnv,
        scope: NotificationSettingsScope,
        new_settings: &ScopeNotificationSettings,
    ) {
        let mut data = Vec::new();
        store_log_event_version(&mut data);
        new_settings.store(&mut data, env.unix_time());
        env.pmc_set_raw(get_notification_settings_scope_database_key(scope), &data);
    }

    /// `on_update_scope_notify_settings` (cpp:831-848) — the telegram
    /// update intake (the UpdatesManager fan-out flips at the M.7
    /// integration).
    pub fn on_update_scope_notify_settings(
        &mut self,
        env: &mut impl NotificationSettingsManagerEnv,
        scope: NotificationSettingsScope,
        peer_notify_settings: Option<telegram_api::PeerNotifySettings>,
    ) {
        if env.auth_is_bot() {
            return;
        }

        let old = self.get_scope(scope);
        let notification_settings = get_scope_notification_settings_from_telegram(
            peer_notify_settings,
            old.disable_pinned_message_notifications,
            old.disable_mention_notifications,
            env.unix_time(),
        );
        if !notification_settings.is_synchronized {
            return;
        }

        self.update_scope_notification_settings(env, scope, notification_settings);
    }

    /// `update_scope_notification_settings` (cpp:850-891) — returns
    /// `need_update_server`.
    pub fn update_scope_notification_settings(
        &mut self,
        env: &mut impl NotificationSettingsManagerEnv,
        scope: NotificationSettingsScope,
        new_settings: ScopeNotificationSettings,
    ) -> bool {
        if env.auth_is_bot() {
            // just in case
            return false;
        }

        let unix_time = env.unix_time();
        let (
            old_mute_until,
            old_sound,
            old_show_preview,
            old_is_synchronized,
            old_disable_pinned,
            old_disable_mention,
        ) = {
            let current = self.get_scope(scope);
            (
                current.mute_until,
                current.sound.clone(),
                current.show_preview,
                current.is_synchronized,
                current.disable_pinned_message_notifications,
                current.disable_mention_notifications,
            )
        };

        let need_update_server = old_mute_until != new_settings.mute_until
            || !are_equivalent_notification_sounds(&old_sound, &new_settings.sound)
            || old_show_preview != new_settings.show_preview;
        let need_update_local = old_disable_pinned
            != new_settings.disable_pinned_message_notifications
            || old_disable_mention != new_settings.disable_mention_notifications;
        let is_inited = new_settings.is_synchronized;
        if old_is_synchronized && !is_inited {
            return false; // just in case
        }
        let is_changed = need_update_server
            || need_update_local
            || old_is_synchronized != is_inited
            || are_different_equivalent_notification_sounds(&old_sound, &new_settings.sound);
        if is_changed {
            self.save_scope_notification_settings(env, scope, &new_settings);

            // `update_scope_unmute_timeout` (cpp:942-965) inline: schedule
            // for the new value, write it, fan out the muted flip
            if old_mute_until != new_settings.mute_until {
                self.schedule_scope_unmute(env, scope, new_settings.mute_until, unix_time);
                let was_muted = old_mute_until != 0;
                let is_muted = new_settings.mute_until != 0;
                self.get_scope_mut(scope).mute_until = new_settings.mute_until;
                if was_muted != is_muted {
                    env.on_update_notification_scope_is_muted(scope, is_muted);
                }
            }

            if !old_disable_pinned && new_settings.disable_pinned_message_notifications {
                env.remove_scope_pinned_message_notifications(scope);
            }
            if old_disable_mention != new_settings.disable_mention_notifications {
                env.on_update_scope_mention_notifications(
                    scope,
                    new_settings.disable_mention_notifications,
                );
            }

            *self.get_scope_mut(scope) = new_settings;

            let update = self.get_update_scope_notification_settings_object_at(scope, unix_time);
            env.send_update(update);
        }
        need_update_server
    }

    /// `on_update_reaction_notification_settings` (cpp:902-922).
    pub fn on_update_reaction_notification_settings(
        &mut self,
        env: &mut impl NotificationSettingsManagerEnv,
        reaction_notification_settings: ReactionNotificationSettings,
    ) {
        if env.auth_is_bot() {
            // the C++ CHECKs !is_bot here; the update intake is
            // unreachable for bots, so the collapsed form ignores it
            return;
        }
        if reaction_notification_settings == self.reaction_notification_settings {
            if !self.have_reaction_notification_settings {
                self.have_reaction_notification_settings = true;
                self.save_reaction_notification_settings(env);
            }
            return;
        }

        self.reaction_notification_settings = reaction_notification_settings;
        self.have_reaction_notification_settings = true;

        self.save_reaction_notification_settings(env);

        let update = self.get_update_reaction_notification_settings_object();
        env.send_update(update);
    }

    /// `save_reaction_notification_settings` (cpp:928-931).
    fn save_reaction_notification_settings(
        &mut self,
        env: &mut impl NotificationSettingsManagerEnv,
    ) {
        let mut data = Vec::new();
        store_log_event_version(&mut data);
        self.reaction_notification_settings.store(&mut data);
        env.pmc_set_raw(get_reaction_notification_settings_database_key(), &data);
    }

    /// `schedule_scope_unmute` (cpp:933-940).
    fn schedule_scope_unmute(
        &mut self,
        env: &impl NotificationSettingsManagerEnv,
        scope: NotificationSettingsScope,
        mute_until: i32,
        unix_time: i32,
    ) {
        let slot = scope.as_i32();
        if mute_until >= unix_time && mute_until < unix_time + 366 * 86400 {
            let timeout_in = f64::from(mute_until - unix_time + 1);
            self.scope_unmute_timeout
                .insert(slot, env.now() + timeout_in);
        } else {
            self.scope_unmute_timeout.remove(&slot);
        }
    }

    /// `reset_scope_notification_settings` (cpp:967-978).
    fn reset_scope_notification_settings(&mut self, env: &mut impl NotificationSettingsManagerEnv) {
        for scope in [
            NotificationSettingsScope::Private,
            NotificationSettingsScope::Group,
            NotificationSettingsScope::Channel,
        ] {
            let new_scope_settings = ScopeNotificationSettings {
                is_synchronized: true,
                ..ScopeNotificationSettings::default()
            };
            self.update_scope_notification_settings(env, scope, new_scope_settings);
        }
    }

    /// `set_scope_notification_settings` (cpp:1580-1593) — the
    /// setScopeNotificationSettings arm; the bot CHECK is the arm's
    /// `CHECK_IS_USER`.
    pub fn set_scope_notification_settings(
        &mut self,
        env: &mut impl NotificationSettingsManagerEnv,
        scope: NotificationSettingsScope,
        notification_settings: Option<td_api::ScopeNotificationSettings>,
    ) -> Result<(), Status> {
        let new_settings =
            get_scope_notification_settings_from_td_api(notification_settings, env.unix_time())?;
        let current_sound = self.get_scope(scope).sound.clone();
        let mut new_settings = new_settings;
        if is_notification_sound_default(&current_sound)
            && is_notification_sound_default(&new_settings.sound)
        {
            new_settings.sound = dup_notification_sound(&current_sound);
        }
        if self.update_scope_notification_settings(env, scope, new_settings) {
            self.update_scope_notification_settings_on_server(env, scope, 0);
        }
        Ok(())
    }

    /// `update_scope_notification_settings_on_server` (cpp:1617-1627) —
    /// the log event is real and the query rides the net plane: the wire
    /// body is the current scope settings
    /// (`ScopeNotificationSettings::get_input_peer_notify_settings`), the
    /// query promise is `get_erase_log_event_promise(log_event_id)` — the
    /// parked id lot carries the erase half and the kind's scope the
    /// on_error repair tail.
    fn update_scope_notification_settings_on_server(
        &mut self,
        env: &mut impl NotificationSettingsManagerEnv,
        scope: NotificationSettingsScope,
        mut log_event_id: u64,
    ) {
        if log_event_id == 0 {
            let mut data = Vec::new();
            store_log_event_version(&mut data);
            store_i32(&mut data, scope.as_i32());
            log_event_id = env.binlog_add(UPDATE_SCOPE_NOTIFICATION_SETTINGS_ON_SERVER, &data);
        }

        tdutils::log_info!(
            "Update {scope:?} notification settings on server with log_event {log_event_id}"
        );
        self.pending_update_scope.push(log_event_id);
        let settings = self.get_scope(scope).clone();
        self.send_notification_settings_net_query(
            NotificationSettingsNetKind::UpdateScope { scope },
            telegram_api::Function::account_updateNotifySettings(
                telegram_api::account_updateNotifySettings {
                    peer: Some(get_input_notify_peer_for_scope(scope)),
                    settings: Some(settings.get_input_peer_notify_settings()),
                },
            ),
            "account.updateNotifySettings",
        );
    }

    /// `set_reaction_notification_settings` (cpp:1629-1650).
    pub fn set_reaction_notification_settings(
        &mut self,
        env: &mut impl NotificationSettingsManagerEnv,
        notification_settings: ReactionNotificationSettings,
    ) -> Result<(), Status> {
        let mut notification_settings = notification_settings;
        notification_settings
            .update_default_notification_sound(&self.reaction_notification_settings);
        if notification_settings == self.reaction_notification_settings {
            self.have_reaction_notification_settings = true;
            return Ok(());
        }

        self.reaction_notification_settings = notification_settings;
        self.have_reaction_notification_settings = true;

        self.save_reaction_notification_settings(env);

        let update = self.get_update_reaction_notification_settings_object();
        env.send_update(update);

        self.update_reaction_notification_settings_on_server(env, 0);
        Ok(())
    }

    /// `update_reaction_notification_settings_on_server` (cpp:1673-1682)
    /// — the log event is real and the query rides the net plane; the
    /// erase-log-event id parks in the family lot.
    fn update_reaction_notification_settings_on_server(
        &mut self,
        env: &mut impl NotificationSettingsManagerEnv,
        mut log_event_id: u64,
    ) {
        if log_event_id == 0 {
            let mut data = Vec::new();
            store_log_event_version(&mut data);
            // BEGIN/END_STORE_FLAGS with no flags — the zero flag word
            data.extend_from_slice(&0u32.to_le_bytes());
            log_event_id = env.binlog_add(UPDATE_REACTION_NOTIFICATION_SETTINGS_ON_SERVER, &data);
        }

        tdutils::log_info!(
            "Update reaction notification settings on server with log_event {log_event_id}"
        );
        self.pending_set_reaction.push(log_event_id);
        self.send_notification_settings_net_query(
            NotificationSettingsNetKind::SetReactions,
            telegram_api::Function::account_setReactionsNotifySettings(
                telegram_api::account_setReactionsNotifySettings {
                    settings: Some(
                        self.reaction_notification_settings
                            .get_input_reactions_notify_settings(),
                    ),
                },
            ),
            "account.setReactionsNotifySettings",
        );
    }

    /// `reset_all_notification_settings` (cpp:1694-1702).
    pub fn reset_all_notification_settings(
        &mut self,
        env: &mut impl NotificationSettingsManagerEnv,
    ) {
        env.reset_all_dialog_notification_settings();

        self.reset_scope_notification_settings(env);

        self.reset_all_notification_settings_on_server(env, 0);
    }

    /// `reset_all_notification_settings_on_server` (cpp:1721-1729) — the
    /// log event is real and the query rides the net plane; the
    /// erase-log-event id parks in the family lot.
    fn reset_all_notification_settings_on_server(
        &mut self,
        env: &mut impl NotificationSettingsManagerEnv,
        mut log_event_id: u64,
    ) {
        if log_event_id == 0 {
            let mut data = Vec::new();
            store_log_event_version(&mut data);
            data.extend_from_slice(&0u32.to_le_bytes());
            log_event_id = env.binlog_add(RESET_ALL_NOTIFICATION_SETTINGS_ON_SERVER, &data);
        }

        tdutils::log_info!("Reset all notification settings");
        self.pending_reset.push(log_event_id);
        self.send_notification_settings_net_query(
            NotificationSettingsNetKind::Reset,
            telegram_api::Function::account_resetNotifySettings(
                telegram_api::account_resetNotifySettings,
            ),
            "account.resetNotifySettings",
        );
    }

    /// `on_binlog_events` (cpp:1731-1763) — the replay dispatch from the
    /// binlog init.
    pub fn on_binlog_events(
        &mut self,
        env: &mut impl NotificationSettingsManagerEnv,
        events: Vec<tddb::binlog::event::BinlogEvent>,
    ) {
        for event in events {
            if event.id == 0 {
                continue;
            }
            match event.type_ {
                RESET_ALL_NOTIFICATION_SETTINGS_ON_SERVER => {
                    self.reset_all_notification_settings_on_server(env, event.id);
                }
                UPDATE_SCOPE_NOTIFICATION_SETTINGS_ON_SERVER => {
                    // the payload is the scope as int32 behind the version
                    let scope = match Parser::new_log_event(event.get_data()) {
                        Ok(mut parser) => {
                            let scope_i32 = parser.i32_().unwrap_or(0);
                            NotificationSettingsScope::from_i32(scope_i32)
                        }
                        Err(_) => {
                            tdutils::log_error!(
                                "Failed to parse the scope notification settings log event"
                            );
                            continue;
                        }
                    };
                    self.update_scope_notification_settings_on_server(env, scope, event.id);
                }
                UPDATE_REACTION_NOTIFICATION_SETTINGS_ON_SERVER => {
                    self.update_reaction_notification_settings_on_server(env, event.id);
                }
                _ => {
                    tdutils::log_error!("Unsupported log event type {}", event.type_);
                }
            }
        }
    }

    /// `get_current_state` (cpp:1765-1784). The saved-ringtones update is
    /// skipped — the ringtones family is `[blocked: net/Files]` and the
    /// list can never load on this line.
    pub fn get_current_state(
        &self,
        env: &impl NotificationSettingsManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if env.auth_is_bot() {
            return;
        }

        let unix_time = env.unix_time();
        for scope in [
            NotificationSettingsScope::Private,
            NotificationSettingsScope::Group,
            NotificationSettingsScope::Channel,
        ] {
            if self.get_scope(scope).is_synchronized {
                updates
                    .push(self.get_update_scope_notification_settings_object_at(scope, unix_time));
            }
        }

        updates.push(self.get_update_reaction_notification_settings_object());

        // `are_saved_ringtones_loaded_` — the saved-ringtones family is
        // [blocked: net/Files], so the updateSavedNotificationSounds
        // branch (cpp:1781-1783) can never fire on this line.
    }

    /// The earliest unmute deadline — the Td timeout slot merge point.
    pub fn next_deadline(&self) -> Option<f64> {
        self.scope_unmute_timeout.values().next().copied()
    }

    /// The `scope_unmute_timeout_` callback pump: fire every due scope
    /// once (cpp:672-683 routes the fired slot into `on_scope_unmute`).
    pub fn tick_timeouts(&mut self, env: &mut impl NotificationSettingsManagerEnv) {
        let now = env.now();
        let due: Vec<i32> = self
            .scope_unmute_timeout
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(&key, _)| key)
            .collect();
        for key in due {
            self.scope_unmute_timeout.remove(&key);
            if env.close_flag() {
                return;
            }
            self.on_scope_unmute(env, NotificationSettingsScope::from_i32(key));
        }
    }
}

/// Pop the oldest parked entry — the §5 FIFO-per-family convention (the
/// C++ promise rides its own ResultHandler; the net completes the
/// same-family queries in dispatch order, so the head pairs).
fn pop_head<T>(queue: &mut Vec<T>) -> Option<T> {
    if queue.is_empty() {
        None
    } else {
        Some(queue.remove(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notification::NotificationTypeEnv;
    use crate::notification_settings::{NotificationSound, ReactionNotificationsFrom};
    use crate::store_parse::store_flags;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    struct TestSettingsEnv {
        pmc: HashMap<Vec<u8>, Vec<u8>>,
        time: f64,
        unix_time: i32,
        updates: Vec<td_api::Update>,
        binlog: Vec<(u64, i32, Vec<u8>)>,
        next_log_event_id: u64,
        is_bot: bool,
        is_authorized: bool,
    }

    impl TestSettingsEnv {
        fn new() -> Self {
            TestSettingsEnv {
                pmc: HashMap::new(),
                time: 0.0,
                unix_time: 1_000_000,
                updates: Vec::new(),
                binlog: Vec::new(),
                next_log_event_id: 1,
                is_bot: false,
                is_authorized: true,
            }
        }

        fn seed_scope_settings(
            &mut self,
            scope: NotificationSettingsScope,
            settings: &ScopeNotificationSettings,
        ) {
            let mut data = Vec::new();
            store_log_event_version(&mut data);
            settings.store(&mut data, self.unix_time);
            self.pmc.insert(
                get_notification_settings_scope_database_key(scope).to_vec(),
                data,
            );
        }
    }

    impl NotificationTypeEnv for TestSettingsEnv {}

    impl NotificationManagerEnv for TestSettingsEnv {
        fn is_disabled(&self) -> bool {
            false
        }
        fn close_flag(&self) -> bool {
            false
        }
        fn get_option_integer(&self, _name: &str, default_value: i64) -> i64 {
            default_value
        }
        fn get_option_boolean(&self, _name: &str, default_value: bool) -> bool {
            default_value
        }
        fn unix_time(&self) -> i32 {
            self.unix_time
        }
        fn now(&self) -> f64 {
            self.time
        }
        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }
        fn pmc_get(&mut self, key: &str) -> String {
            String::from_utf8_lossy(&self.pmc.get(key.as_bytes()).cloned().unwrap_or_default())
                .to_string()
        }
        fn pmc_set(&mut self, key: &str, value: &str) {
            self.pmc
                .insert(key.as_bytes().to_vec(), value.as_bytes().to_vec());
        }
        fn pmc_erase(&mut self, key: &str) {
            self.pmc.remove(key.as_bytes());
        }
    }

    impl NotificationSettingsManagerEnv for TestSettingsEnv {
        fn auth_is_authorized(&self) -> bool {
            self.is_authorized
        }
        fn auth_was_authorized(&self) -> bool {
            self.is_authorized
        }
        fn auth_is_bot(&self) -> bool {
            self.is_bot
        }
        fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.pmc.get(key).cloned().unwrap_or_default()
        }
        fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.pmc.insert(key.to_vec(), value.to_vec());
        }
        fn binlog_add(&mut self, handler_type: i32, data: &[u8]) -> u64 {
            let id = self.next_log_event_id;
            self.next_log_event_id += 1;
            self.binlog.push((id, handler_type, data.to_vec()));
            id
        }
    }

    fn muted_settings(mute_until: i32) -> ScopeNotificationSettings {
        ScopeNotificationSettings {
            mute_until,
            sound: Some(NotificationSound::Ringtone { ringtone_id: 9 }),
            is_synchronized: true,
            ..ScopeNotificationSettings::default()
        }
    }

    fn sync_settings_request(
        mute_for: i32,
        sound_id: i64,
        show_preview: bool,
    ) -> td_api::ScopeNotificationSettings {
        td_api::ScopeNotificationSettings {
            mute_for,
            sound_id,
            show_preview,
            use_default_mute_stories: true,
            mute_stories: false,
            story_sound_id: -1,
            show_story_poster: true,
            disable_pinned_message_notifications: false,
            disable_mention_notifications: false,
        }
    }

    #[test]
    fn init_without_pmc_pushes_default_reaction_update_only() {
        let mut env = TestSettingsEnv::new();
        let mut manager = NotificationSettingsManager::new();
        manager.init(&mut env);

        assert!(!env.pmc.contains_key(&b"nsfac"[..]), "nsfac must be erased");
        // no scope settings: all three unsynchronized -> no scope updates;
        // the three fetch queries park their empty promises (the net-inert
        // posture logs the sends — the td.rs round-trip drives the wire)
        assert!(env.updates.iter().all(|update| matches!(
            update,
            td_api::Update::updateReactionNotificationSettings(_)
        )));
        for scope in [
            NotificationSettingsScope::Private,
            NotificationSettingsScope::Group,
            NotificationSettingsScope::Channel,
        ] {
            assert!(
                manager.take_scope_get_promise(scope).is_some(),
                "the {scope:?} fetch parked its promise"
            );
        }
        // the reaction update carries the defaults
        let td_api::Update::updateReactionNotificationSettings(payload) = &env.updates[0] else {
            panic!("expected the reaction settings update");
        };
        let settings = payload
            .notification_settings
            .as_ref()
            .expect("settings set");
        assert_eq!(
            settings.message_reaction_source,
            Some(
                td_api::ReactionNotificationSource::reactionNotificationSourceContacts(
                    td_api::reactionNotificationSourceContacts
                )
            )
        );
        assert_eq!(settings.sound_id, -1);
        assert!(settings.show_preview);
    }

    #[test]
    fn init_is_idempotent_and_gated() {
        let mut env = TestSettingsEnv::new();
        env.is_authorized = false;
        let mut manager = NotificationSettingsManager::new();
        manager.init(&mut env);
        assert!(env.updates.is_empty(), "unauthorized init must not push");

        env.is_authorized = true;
        manager.init(&mut env);
        let count = env.updates.len();
        assert!(count > 0);
        manager.init(&mut env);
        assert_eq!(env.updates.len(), count, "the second init must be a no-op");
    }

    #[test]
    fn init_loads_saved_scope_settings_and_schedules_unmute() {
        let mut env = TestSettingsEnv::new();
        env.seed_scope_settings(
            NotificationSettingsScope::Private,
            &muted_settings(1_000_100),
        );
        let mut manager = NotificationSettingsManager::new();
        manager.init(&mut env);

        // the loaded scope pushes its update with the saved values
        let update = env
            .updates
            .iter()
            .find_map(|update| match update {
                td_api::Update::updateScopeNotificationSettings(payload) => Some(payload),
                _ => None,
            })
            .expect("the loaded scope pushes its update");
        assert_eq!(
            update.notification_settings.as_ref().expect("set").mute_for,
            100
        );
        assert_eq!(
            update.notification_settings.as_ref().expect("set").sound_id,
            9
        );
        // the unmute deadline is now() + (mute_until - now) + 1
        assert_eq!(
            manager.next_deadline(),
            Some(101.0),
            "the unmute is scheduled 101 monotonic seconds out"
        );
        assert_eq!(
            manager.get_scope_mute_until(NotificationSettingsScope::Private),
            1_000_100
        );
    }

    #[test]
    fn set_scope_settings_updates_state_pmc_and_binlog() {
        let mut env = TestSettingsEnv::new();
        let mut manager = NotificationSettingsManager::new();
        manager.init(&mut env);
        let updates_before = env.updates.len();
        let binlog_before = env.binlog.len();

        manager
            .set_scope_notification_settings(
                &mut env,
                NotificationSettingsScope::Group,
                Some(sync_settings_request(50, 0, true)),
            )
            .expect("ok");

        // the scope became synchronized and muted-for-50
        assert_eq!(
            manager.get_scope_mute_until(NotificationSettingsScope::Group),
            1_000_050
        );
        assert!(manager.next_deadline().is_some());
        // the update push and the pmc write happened
        assert!(env.updates.len() > updates_before);
        let stored = env
            .pmc
            .get(&b"nsfgc"[..])
            .expect("scope settings persisted");
        let restored =
            ScopeNotificationSettings::parse(&mut Parser::new_log_event(stored).unwrap())
                .expect("parse");
        assert!(restored.is_synchronized);
        assert_eq!(restored.mute_until, 1_000_050);
        assert_eq!(restored.sound, Some(NotificationSound::NoSound));
        // the on-server log event was appended with the scope payload
        assert_eq!(env.binlog.len(), binlog_before + 1);
        assert_eq!(
            env.binlog.last().expect("set").1,
            UPDATE_SCOPE_NOTIFICATION_SETTINGS_ON_SERVER
        );
        let payload = &env.binlog.last().expect("set").2;
        let mut parser = Parser::new_log_event(payload).expect("framed");
        assert_eq!(
            parser.i32_().expect("scope"),
            NotificationSettingsScope::Group.as_i32()
        );
    }

    #[test]
    fn set_scope_settings_sound_carry_over_rules() {
        let mut env = TestSettingsEnv::new();
        let mut manager = NotificationSettingsManager::new();
        manager.init(&mut env);
        manager
            .set_scope_notification_settings(
                &mut env,
                NotificationSettingsScope::Private,
                Some(sync_settings_request(10, 5, true)),
            )
            .expect("ok");
        // current Ringtone(5) is not default and the new default (-1) wins
        manager
            .set_scope_notification_settings(
                &mut env,
                NotificationSettingsScope::Private,
                Some(sync_settings_request(10, -1, true)),
            )
            .expect("ok");
        assert_eq!(
            manager.get_scope_notification_sound(NotificationSettingsScope::Private),
            &None
        );

        // a Local sound is "default", so it carries over a default request
        let mut local = muted_settings(1_000_010);
        local.sound = Some(NotificationSound::Local {
            title: "t".to_owned(),
            data: "t".to_owned(),
        });
        manager.update_scope_notification_settings(
            &mut env,
            NotificationSettingsScope::Channel,
            local,
        );
        manager
            .set_scope_notification_settings(
                &mut env,
                NotificationSettingsScope::Channel,
                Some(sync_settings_request(20, -1, true)),
            )
            .expect("ok");
        assert_eq!(
            manager.get_scope_notification_sound(NotificationSettingsScope::Channel),
            &Some(NotificationSound::Local {
                title: "t".to_owned(),
                data: "t".to_owned()
            }),
            "the Local sound is default and must carry over"
        );
    }

    #[test]
    fn unmute_fires_updates_and_persists() {
        let mut env = TestSettingsEnv::new();
        env.seed_scope_settings(
            NotificationSettingsScope::Private,
            &muted_settings(1_000_050),
        );
        let mut manager = NotificationSettingsManager::new();
        manager.init(&mut env);

        // not yet due
        env.time = 30.0;
        env.unix_time = 1_000_030;
        manager.tick_timeouts(&mut env);
        assert_eq!(
            manager.get_scope_mute_until(NotificationSettingsScope::Private),
            1_000_050
        );

        // due at 51.0; the wall clock advanced past the mute too
        env.time = 52.0;
        env.unix_time = 1_000_052;
        manager.tick_timeouts(&mut env);
        assert_eq!(
            manager.get_scope_mute_until(NotificationSettingsScope::Private),
            0
        );
        assert!(manager.next_deadline().is_none());
        let restored = ScopeNotificationSettings::parse(
            &mut Parser::new_log_event(env.pmc.get(&b"nsfpc"[..]).expect("persisted")).unwrap(),
        )
        .expect("parse");
        assert_eq!(restored.mute_until, 0, "the unmute persists");
    }

    #[test]
    fn reaction_settings_set_and_short_circuit() {
        let mut env = TestSettingsEnv::new();
        let mut manager = NotificationSettingsManager::new();
        manager.init(&mut env);
        let binlog_before = env.binlog.len();

        let new_settings = ReactionNotificationSettings {
            message_reactions: ReactionNotificationsFrom::All,
            ..ReactionNotificationSettings::default()
        };
        manager
            .set_reaction_notification_settings(&mut env, new_settings.clone())
            .expect("ok");
        assert_eq!(manager.reaction_notification_settings, new_settings);
        assert_eq!(env.binlog.len(), binlog_before + 1);
        assert_eq!(
            env.binlog.last().expect("set").1,
            UPDATE_REACTION_NOTIFICATION_SETTINGS_ON_SERVER
        );
        // the pmc blob round-trips
        let restored = ReactionNotificationSettings::parse(
            &mut Parser::new_log_event(env.pmc.get(&b"rns"[..]).expect("persisted")).unwrap(),
        )
        .expect("parse");
        assert_eq!(restored, new_settings);

        // an equal set short-circuits: no second log event
        manager
            .set_reaction_notification_settings(&mut env, new_settings)
            .expect("ok");
        assert_eq!(env.binlog.len(), binlog_before + 1);
    }

    #[test]
    fn reset_all_resets_scopes_and_writes_reset_log_event() {
        let mut env = TestSettingsEnv::new();
        let mut manager = NotificationSettingsManager::new();
        manager.init(&mut env);
        manager
            .set_scope_notification_settings(
                &mut env,
                NotificationSettingsScope::Private,
                Some(sync_settings_request(100, -1, false)),
            )
            .expect("ok");
        let binlog_before = env.binlog.len();

        manager.reset_all_notification_settings(&mut env);

        assert_eq!(
            manager.get_scope_mute_until(NotificationSettingsScope::Private),
            0
        );
        assert!(manager.get_scope_show_preview(NotificationSettingsScope::Private));
        assert_eq!(
            env.binlog.last().expect("set").1,
            RESET_ALL_NOTIFICATION_SETTINGS_ON_SERVER
        );
        assert_eq!(env.binlog.len(), binlog_before + 1);
    }

    #[test]
    fn binlog_replay_reuses_event_ids() {
        let mut env = TestSettingsEnv::new();
        let mut manager = NotificationSettingsManager::new();
        manager.init(&mut env);
        manager
            .set_scope_notification_settings(
                &mut env,
                NotificationSettingsScope::Channel,
                Some(sync_settings_request(100, -1, true)),
            )
            .expect("ok");
        let recorded = env.binlog.clone();
        assert_eq!(recorded.len(), 1);

        // replay: the same events must not append new ones
        let events = recorded
            .iter()
            .map(|(id, handler_type, data)| {
                tddb::binlog::event::BinlogEvent::init(
                    tddb::binlog::event::BinlogEvent::create_raw(*id, *handler_type, 0, data),
                )
                .unwrap()
            })
            .collect();
        manager.on_binlog_events(&mut env, events);
        assert_eq!(
            env.binlog.len(),
            1,
            "the replay re-sends with the recorded ids"
        );
    }

    #[test]
    fn get_current_state_emits_synchronized_scopes_and_reaction() {
        let mut env = TestSettingsEnv::new();
        let mut manager = NotificationSettingsManager::new();
        manager.init(&mut env);
        manager
            .set_scope_notification_settings(
                &mut env,
                NotificationSettingsScope::Private,
                Some(sync_settings_request(100, -1, true)),
            )
            .expect("ok");

        let mut updates = Vec::new();
        manager.get_current_state(&env, &mut updates);
        let scope_updates = updates
            .iter()
            .filter(|update| matches!(update, td_api::Update::updateScopeNotificationSettings(_)))
            .count();
        assert_eq!(scope_updates, 1, "only the synchronized scope emits");
        assert!(updates.iter().any(|update| matches!(
            update,
            td_api::Update::updateReactionNotificationSettings(_)
        )));

        // bots get nothing
        env.is_bot = true;
        let mut updates = Vec::new();
        manager.get_current_state(&env, &mut updates);
        assert!(updates.is_empty());
    }

    #[test]
    fn scope_settings_flag_block_layout_matches_cpp() {
        // the stored flag order (ScopeNotificationSettings.hpp):
        // is_muted, has_sound, show_preview, false, is_synchronized,
        // disable_pinned, disable_mention, has_ringtone_support,
        // mute_stories, has_story_sound, hide_story_sender, use_mute_stories
        let settings = muted_settings(0); // mute_until 0 -> is_muted false
        let mut out = Vec::new();
        settings.store(&mut out, 1_000_000);
        let flags = u32::from_le_bytes(out[..4].try_into().expect("4 bytes"));
        assert_eq!(flags & 1, 0, "is_muted");
        assert_eq!((flags >> 1) & 1, 1, "has_sound");
        assert_eq!((flags >> 2) & 1, 1, "show_preview default");
        assert_eq!((flags >> 3) & 1, 0, "the reserved false flag");
        assert_eq!((flags >> 4) & 1, 1, "is_synchronized");
        assert_eq!((flags >> 7) & 1, 1, "has_ringtone_support is written true");
        assert_eq!((flags >> 11) & 1, 0, "use_mute_stories false");
        assert_eq!(
            out.len(),
            4 + (4 + 8),
            "flags + sound only: not muted, no story sound"
        );

        // the empty flag log events are the version + a zero u32
        let mut data = Vec::new();
        store_log_event_version(&mut data);
        data.extend_from_slice(&store_flags_zero());
        let mut parser = Parser::new_log_event(&data).expect("framed");
        assert!(parser.parse_flags::<0>().is_ok());
    }

    fn store_flags_zero() -> [u8; 4] {
        let mut out = Vec::new();
        store_flags(&mut out, []);
        out.try_into().expect("4 bytes")
    }

    /// The scope-object promise slot — captures the settled result (the
    /// deferred request promise shape).
    type ScopeSlot = Arc<Mutex<Option<tdutils::status::Result<td_api::ScopeNotificationSettings>>>>;

    fn scope_slot() -> (Promise<td_api::ScopeNotificationSettings>, ScopeSlot) {
        let slot: ScopeSlot = Arc::new(Mutex::new(None));
        let capture = Arc::clone(&slot);
        let promise = tdutils::promise::PromiseCreator::lambda_result(move |result| {
            *capture.lock().unwrap() = Some(result);
        });
        (promise, slot)
    }

    #[test]
    fn scope_get_request_defers_and_parks_the_promise() {
        let mut env = TestSettingsEnv::new();
        let mut manager = NotificationSettingsManager::new();
        // unsynchronized scope — the request defers (cpp:1533-1544)
        let (promise, slot) = scope_slot();
        manager.get_scope_notification_settings_for_request(
            &env,
            NotificationSettingsScope::Private,
            promise,
        );
        assert!(slot.lock().unwrap().is_none(), "the promise stays parked");
        // the fetch parks the promise — the drain pops it FIFO per scope;
        // the answer drain shape: the intake runs first, then the do_run
        // re-run answers from the synchronized state
        let mut popped = manager
            .take_scope_get_promise(NotificationSettingsScope::Private)
            .expect("the request promise parked");
        let wire_unix_time = env.unix_time;
        manager.on_update_scope_notify_settings(
            &mut env,
            NotificationSettingsScope::Private,
            Some(telegram_api::PeerNotifySettings {
                flags: (1 << 0) | (1 << 2),
                show_previews: true,
                silent: false,
                mute_until: wire_unix_time + 30,
                ios_sound: None,
                android_sound: None,
                other_sound: None,
                stories_muted: false,
                stories_hide_sender: false,
                stories_ios_sound: None,
                stories_android_sound: None,
                stories_other_sound: None,
            }),
        );
        let object =
            manager.scope_settings_object(NotificationSettingsScope::Private, env.unix_time);
        popped.set_value(object);
        match slot.lock().unwrap().take() {
            Some(tdutils::status::Result::Ok(settings)) => {
                assert_eq!(settings.mute_for, 30);
            }
            other => panic!("the settled scope object: {other:?}"),
        }
        // a second pop finds nothing — one query, one promise
        assert!(
            manager
                .take_scope_get_promise(NotificationSettingsScope::Private)
                .is_none()
        );
        // the now-synchronized scope settles the promise inline with the
        // object (the C++ set_value(Unit) + do_send_result collapse)
        let (promise, slot) = scope_slot();
        manager.get_scope_notification_settings_for_request(
            &env,
            NotificationSettingsScope::Private,
            promise,
        );
        match slot.lock().unwrap().take() {
            Some(tdutils::status::Result::Ok(settings)) => {
                assert_eq!(settings.mute_for, 30);
            }
            other => panic!("the inline settle: {other:?}"),
        }
    }

    #[test]
    fn scope_get_bot_gate_errors_the_promise() {
        let mut env = TestSettingsEnv::new();
        env.is_bot = true;
        let mut manager = NotificationSettingsManager::new();
        let (promise, slot) = scope_slot();
        // cpp:1546-1554 — the bot gate errors the request promise directly
        manager.send_get_scope_notification_settings_query(
            &env,
            NotificationSettingsScope::Group,
            promise,
        );
        match slot.lock().unwrap().take() {
            Some(tdutils::status::Result::Err(status)) => {
                assert_eq!(status.code(), 500);
                assert_eq!(status.message(), "Wrong getScopeNotificationSettings query");
            }
            other => panic!("the bot gate: {other:?}"),
        }
        assert!(
            manager
                .take_scope_get_promise(NotificationSettingsScope::Group)
                .is_none(),
            "the gated send parks nothing"
        );
    }

    #[test]
    fn on_server_families_park_their_log_event_ids() {
        let mut env = TestSettingsEnv::new();
        let mut manager = NotificationSettingsManager::new();
        manager.init(&mut env);
        let binlog_before = env.binlog.len();

        // UpdateScope — the wire send is net-inert here; the log event id
        // parks FIFO (the erase-log-event promise half)
        manager.update_scope_notification_settings_on_server(
            &mut env,
            NotificationSettingsScope::Channel,
            0,
        );
        assert_eq!(env.binlog.len(), binlog_before + 1);
        assert_eq!(
            manager.take_update_scope_log_event(),
            Some(env.binlog[binlog_before].0)
        );
        assert!(manager.take_update_scope_log_event().is_none());

        // SetReactions
        manager.update_reaction_notification_settings_on_server(&mut env, 0);
        assert_eq!(env.binlog.len(), binlog_before + 2);
        assert_eq!(
            manager.take_set_reaction_log_event(),
            Some(env.binlog[binlog_before + 1].0)
        );
        assert!(manager.take_set_reaction_log_event().is_none());

        // Reset
        manager.reset_all_notification_settings_on_server(&mut env, 0);
        assert_eq!(env.binlog.len(), binlog_before + 3);
        assert_eq!(
            manager.take_reset_log_event(),
            Some(env.binlog[binlog_before + 2].0)
        );
        assert!(manager.take_reset_log_event().is_none());

        // the replay form reuses the recorded id — no new binlog event
        let replayed_id = env.binlog[binlog_before].0;
        manager.update_scope_notification_settings_on_server(
            &mut env,
            NotificationSettingsScope::Channel,
            replayed_id,
        );
        assert_eq!(
            env.binlog.len(),
            binlog_before + 3,
            "the replayed id parks without a new log event"
        );
    }
}
