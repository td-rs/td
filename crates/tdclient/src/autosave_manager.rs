//! `AutosaveManager` (td/telegram/AutosaveManager.{h,cpp}) — the M.39
//! Wave-3 slice: the autosave (media preload) settings with the three
//! scopes, the per-dialog exceptions and their persistence. The settings
//! ride the sqlite pmc under the `autosave_settings` key (the C++
//! `G()->td_db()->get_sqlite_pmc()` surface; the async kv get/set collapse
//! to the sync [`AutosaveManagerEnv`] calls — the dialog_db convention),
//! with the C++ corrupt-load path's binlog-pmc erase kept as its own seam.
//! The three `Td::ResultHandler` query classes (`GetAutoSaveSettingsQuery`,
//! `SaveAutoSaveSettingsQuery`, `DeleteAutoSaveExceptionsQuery`) collapse
//! into the [`AutosaveManagerEnv`] query seams — `[blocked: net]` with the
//! promises parked until the net queries land (the M.31 convention); the
//! answer handler [`AutosaveManager::on_get_auto_save_settings`] is real
//! and unit-tested against constructed answers. The C++
//! `UpdatesManager::before_get_difference` (cpp:4978) /
//! `after_get_difference` (cpp:2413) reload hooks flip at the M.7
//! integration through [`AutosaveManager::reload_autosave_settings`].

use crate::dialog_id::DialogId;
use crate::dialog_manager;
use crate::store_parse::{self, Parser};
use crate::td_api;
use std::collections::HashMap;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `telegram_api::autoSaveSettings::VIDEO_MAX_SIZE_MASK` (flags.2?long).
const VIDEO_MAX_SIZE_MASK: i32 = 1 << 2;

/// The `G()`/`td_` surface of the manager. The net queries are the
/// `[blocked: net]` query seams; the `send_update` outbox is drained by
/// [`crate::td::Td::flush_user_updates`].
pub trait AutosaveManagerEnv {
    /// `G()->close_flag()`.
    fn close_flag(&self) -> bool;

    /// `G()->use_message_database()`.
    fn use_message_database(&self) -> bool;

    /// `G()->td_db()->get_sqlite_pmc()->get` — the sync sqlite pmc read
    /// (empty when the key is missing).
    fn sqlite_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;

    /// `G()->td_db()->get_sqlite_pmc()->set`.
    fn sqlite_pmc_set_raw(&mut self, key: &[u8], value: &[u8]);

    /// `G()->td_db()->get_binlog_pmc()->erase` — the C++ corrupt-load
    /// path's erase (AutosaveManager.cpp:314; the binlog store, not the
    /// sqlite one the settings live in).
    fn binlog_pmc_erase(&mut self, key: &str);

    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    /// `td_->dialog_manager_->check_dialog_access(dialog_id, false,
    /// AccessRights::Read, source)` (AutosaveManager.cpp:469) — the
    /// simplified valid-dialog seam until the M.5/M.7 dialog integration
    /// (the BoostEnv convention).
    fn check_dialog_access(&mut self, dialog_id: DialogId) -> Result<(), Status>;

    /// `td_->dialog_manager_->force_create_dialog(dialog_id, source)`
    /// (AutosaveManager.cpp:403).
    fn force_create_dialog(&mut self, dialog_id: DialogId);

    /// `GetAutoSaveSettingsQuery::send` (cpp:40-42) — `[blocked: net]`;
    /// the answer arrives through
    /// [`AutosaveManager::on_get_auto_save_settings`].
    fn send_get_auto_save_settings_query(&mut self) {
        tdutils::log_info!("Send GetAutoSaveSettings — [blocked: net]");
    }

    /// `SaveAutoSaveSettingsQuery::send` (cpp:67-80) — `[blocked: net]`;
    /// the C++ builds the input peer for the peer-only save inside the
    /// query (with the non-null CHECK the set-time `check_dialog_access`
    /// already guarantees) and the C++ `on_error` reloads the settings
    /// (cpp:91-94) — both obligations ride the net-step integration.
    fn send_save_auto_save_settings_query(
        &mut self,
        users: bool,
        chats: bool,
        broadcasts: bool,
        dialog_id: DialogId,
        settings: &telegram_api::AutoSaveSettings,
    ) {
        tdutils::log_info!(
            "Send SaveAutoSaveSettings(users = {users}, chats = {chats}, \
             broadcasts = {broadcasts}, dialog_id = {dialog_id:?}) — [blocked: net]"
        );
        let _ = settings;
    }

    /// `DeleteAutoSaveExceptionsQuery::send` (cpp:104-106) —
    /// `[blocked: net]`; the C++ `on_error` reloads the settings
    /// (cpp:117-120) — the same net-step obligation.
    fn send_delete_auto_save_exceptions_query(&mut self) {
        tdutils::log_info!("Send DeleteAutoSaveExceptions — [blocked: net]");
    }
}

/// `td::fail_promises` (Promise.h) — fail the queue with a cloned error.
fn fail_promises<T: Send>(promises: &mut Vec<Promise<T>>, error: &Status) {
    for mut promise in std::mem::take(promises) {
        promise.set_error(error.clone());
    }
}

/// `Global::request_aborted_error()` (Global.h) — the close-time answer.
fn request_aborted_error() -> Status {
    Status::error(500, "Request aborted")
}

/// `AutosaveManager::DialogAutosaveSettings` (AutosaveManager.h:40-72) —
/// the settings of one scope or exception dialog.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DialogAutosaveSettings {
    are_inited: bool,
    autosave_photos: bool,
    autosave_videos: bool,
    max_video_file_size: i64,
}

impl DialogAutosaveSettings {
    /// `MIN_MAX_VIDEO_FILE_SIZE` (AutosaveManager.h:46).
    pub(crate) const MIN_MAX_VIDEO_FILE_SIZE: i64 = 512 * 1024;
    /// `DEFAULT_MAX_VIDEO_FILE_SIZE` (AutosaveManager.h:47).
    pub(crate) const DEFAULT_MAX_VIDEO_FILE_SIZE: i64 = 100 * 1024 * 1024;
    /// `MAX_MAX_VIDEO_FILE_SIZE` (AutosaveManager.h:48) — the C++
    /// `static_cast<int64>(4000) * 1024 * 1024`.
    pub(crate) const MAX_MAX_VIDEO_FILE_SIZE: i64 = 4000 * 1024 * 1024;

    /// `are_inited_` — test-visible; the manager reads the field directly.
    #[allow(dead_code)]
    pub(crate) fn are_inited(&self) -> bool {
        self.are_inited
    }

    /// `max_video_file_size_` — test-visible; the manager reads the field
    /// directly.
    #[allow(dead_code)]
    pub(crate) fn max_video_file_size(&self) -> i64 {
        self.max_video_file_size
    }

    /// `DialogAutosaveSettings(const telegram_api::autoSaveSettings *)`
    /// (cpp:130-136).
    pub fn from_telegram(settings: &telegram_api::AutoSaveSettings) -> DialogAutosaveSettings {
        DialogAutosaveSettings {
            are_inited: true,
            autosave_photos: settings.photos,
            autosave_videos: settings.videos,
            max_video_file_size: settings
                .video_max_size
                .clamp(Self::MIN_MAX_VIDEO_FILE_SIZE, Self::MAX_MAX_VIDEO_FILE_SIZE),
        }
    }

    /// `DialogAutosaveSettings(const td_api::scopeAutosaveSettings *)`
    /// (cpp:138-146) — the null settings stay the not-inited defaults (the
    /// C++ early return).
    pub fn from_td_api(settings: Option<&td_api::ScopeAutosaveSettings>) -> DialogAutosaveSettings {
        let Some(settings) = settings else {
            return DialogAutosaveSettings::default();
        };
        DialogAutosaveSettings {
            are_inited: true,
            autosave_photos: settings.autosave_photos,
            autosave_videos: settings.autosave_videos,
            max_video_file_size: settings
                .max_video_file_size
                .clamp(Self::MIN_MAX_VIDEO_FILE_SIZE, Self::MAX_MAX_VIDEO_FILE_SIZE),
        }
    }

    /// `get_input_auto_save_settings` (cpp:148-156) — the wire object; the
    /// `VIDEO_MAX_SIZE_MASK` flag carries the size only when inited (the
    /// generated storer ORs the bare bool flags itself).
    pub fn get_input_auto_save_settings(&self) -> telegram_api::AutoSaveSettings {
        let flags = if self.are_inited {
            VIDEO_MAX_SIZE_MASK
        } else {
            0
        };
        telegram_api::AutoSaveSettings {
            flags,
            photos: self.autosave_photos,
            videos: self.autosave_videos,
            video_max_size: self.max_video_file_size,
        }
    }

    /// `get_scope_autosave_settings_object` (cpp:158-164) — `None` while
    /// not inited (the C++ nullptr).
    pub fn get_scope_autosave_settings_object(&self) -> Option<td_api::ScopeAutosaveSettings> {
        if !self.are_inited {
            return None;
        }
        Some(td_api::ScopeAutosaveSettings {
            autosave_photos: self.autosave_photos,
            autosave_videos: self.autosave_videos,
            max_video_file_size: self.max_video_file_size,
        })
    }

    /// `get_autosave_settings_exception_object` (cpp:166-172) — the chat id
    /// is resolved by the caller through
    /// [`dialog_manager::get_chat_id_object`] (the C++ `const Td *` hop).
    pub fn get_autosave_settings_exception_object(
        &self,
        chat_id: i64,
    ) -> td_api::AutosaveSettingsException {
        td_api::AutosaveSettingsException {
            chat_id,
            settings: self.get_scope_autosave_settings_object(),
        }
    }

    /// `DialogAutosaveSettings::store` (cpp:183-191).
    pub fn store(&self, out: &mut Vec<u8>) {
        assert!(
            self.are_inited,
            "storing a not-inited DialogAutosaveSettings"
        );
        store_parse::store_flags(out, [self.autosave_photos, self.autosave_videos]);
        store_parse::store_i64(out, self.max_video_file_size);
    }

    /// `DialogAutosaveSettings::parse` (cpp:193-201).
    pub fn parse(parser: &mut Parser) -> Result<DialogAutosaveSettings, &'static str> {
        let [autosave_photos, autosave_videos] = parser.parse_flags()?;
        let max_video_file_size = parser.i64_()?;
        Ok(DialogAutosaveSettings {
            are_inited: true,
            autosave_photos,
            autosave_videos,
            max_video_file_size,
        })
    }
}

/// `AutosaveManager::AutosaveSettings` (AutosaveManager.h:74-90) — the
/// full settings with the scope triple, the exceptions and the reload
/// machine.
#[derive(Clone, Debug, Default)]
pub struct AutosaveSettings {
    are_inited: bool,
    are_being_reloaded: bool,
    need_reload: bool,
    user_settings: DialogAutosaveSettings,
    chat_settings: DialogAutosaveSettings,
    broadcast_settings: DialogAutosaveSettings,
    exceptions: HashMap<DialogId, DialogAutosaveSettings>,
}

impl PartialEq for AutosaveSettings {
    /// The C++ `AutosaveSettings` has no `operator==`; the equality here
    /// covers the persisted plane only (the reload flags are machine
    /// state) — the shape the byte-equality tests need.
    fn eq(&self, other: &Self) -> bool {
        self.are_inited == other.are_inited
            && self.user_settings == other.user_settings
            && self.chat_settings == other.chat_settings
            && self.broadcast_settings == other.broadcast_settings
            && self.exceptions == other.exceptions
    }
}

impl AutosaveSettings {
    /// `get_autosave_settings_object` (cpp:203-212) — the exceptions
    /// resolve their chat ids through
    /// [`dialog_manager::get_chat_id_object`] (the C++ `const Td *` hop).
    pub fn get_autosave_settings_object(&self) -> td_api::AutosaveSettings {
        assert!(self.are_inited, "AutosaveSettings is not inited");
        let exceptions =
            self.exceptions
                .iter()
                .map(|(dialog_id, settings)| {
                    settings.get_autosave_settings_exception_object(
                        dialog_manager::get_chat_id_object(*dialog_id, "autosaveSettingsException"),
                    )
                })
                .collect();
        td_api::AutosaveSettings {
            private_chat_settings: self.user_settings.get_scope_autosave_settings_object(),
            group_settings: self.chat_settings.get_scope_autosave_settings_object(),
            channel_settings: self.broadcast_settings.get_scope_autosave_settings_object(),
            exceptions,
        }
    }

    /// `AutosaveSettings::store` (cpp:214-231).
    pub fn store(&self, out: &mut Vec<u8>) {
        assert!(self.are_inited, "storing a not-inited AutosaveSettings");
        let has_exceptions = !self.exceptions.is_empty();
        store_parse::store_flags(out, [has_exceptions]);
        self.user_settings.store(out);
        self.chat_settings.store(out);
        self.broadcast_settings.store(out);
        if has_exceptions {
            store_parse::store_u32(out, self.exceptions.len() as u32);
            for (dialog_id, settings) in &self.exceptions {
                store_parse::store_i64(out, dialog_id.get());
                settings.store(out);
            }
        }
    }

    /// `AutosaveSettings::parse` (cpp:233-256) — invalid exception dialogs
    /// are dropped (the C++ `is_valid` filter).
    pub fn parse(parser: &mut Parser) -> Result<AutosaveSettings, &'static str> {
        let [has_exceptions] = parser.parse_flags()?;
        let user_settings = DialogAutosaveSettings::parse(parser)?;
        let chat_settings = DialogAutosaveSettings::parse(parser)?;
        let broadcast_settings = DialogAutosaveSettings::parse(parser)?;
        let mut exceptions = HashMap::new();
        if has_exceptions {
            let size = parser.u32_()?;
            for _ in 0..size {
                let dialog_id = DialogId::new(parser.i64_()?);
                let settings = DialogAutosaveSettings::parse(parser)?;
                if dialog_id.is_valid() {
                    exceptions.insert(dialog_id, settings);
                }
            }
        }
        Ok(AutosaveSettings {
            are_inited: true,
            are_being_reloaded: false,
            need_reload: false,
            user_settings,
            chat_settings,
            broadcast_settings,
            exceptions,
        })
    }
}

/// The C++ `AutosaveManager` actor; the Rust manager is a plain struct
/// held by `Td` (the actor envelope collapses, the M.13 convention).
#[derive(Default)]
pub struct AutosaveManager {
    /// `settings_` (AutosaveManager.h:113).
    settings: AutosaveSettings,
    /// `load_settings_queries_` (AutosaveManager.h:114).
    load_settings_queries: Vec<Promise<td_api::AutosaveSettings>>,
    /// The `()`-typed promises parked at their `[blocked: net]` query
    /// sends — the save/delete queries own them in the C++ until the
    /// answer (the M.31 `net_blocked_promises` convention); the C++
    /// `on_error` reload obligation rides the net-step integration.
    net_blocked_promises: Vec<Promise<()>>,
}

impl AutosaveManager {
    /// `AutosaveManager::AutosaveManager` (cpp:123-125).
    pub fn new() -> AutosaveManager {
        AutosaveManager::default()
    }

    /// `get_autosave_settings` (cpp:258-264).
    pub fn get_autosave_settings(
        &mut self,
        env: &mut impl AutosaveManagerEnv,
        mut promise: Promise<td_api::AutosaveSettings>,
    ) {
        if self.settings.are_inited {
            promise.set_value(self.settings.get_autosave_settings_object());
            return;
        }

        self.load_autosave_settings(env, promise);
    }

    /// `get_autosave_settings_database_key` (cpp:266-268).
    fn get_autosave_settings_database_key() -> &'static [u8] {
        b"autosave_settings"
    }

    /// `load_autosave_settings` (cpp:270-285) — the queueing hop; the
    /// C++ async sqlite pmc get collapses to the sync env read (the
    /// answer handler runs inline).
    fn load_autosave_settings(
        &mut self,
        env: &mut impl AutosaveManagerEnv,
        promise: Promise<td_api::AutosaveSettings>,
    ) {
        self.load_settings_queries.push(promise);
        if self.load_settings_queries.len() != 1 {
            return;
        }

        if env.use_message_database() {
            let value = env.sqlite_pmc_get_raw(Self::get_autosave_settings_database_key());
            self.on_load_autosave_settings_from_database(env, &value);
            return;
        }

        self.reload_autosave_settings(env);
    }

    /// `on_load_autosave_settings_from_database` (cpp:287-335).
    fn on_load_autosave_settings_from_database(
        &mut self,
        env: &mut impl AutosaveManagerEnv,
        value: &[u8],
    ) {
        if env.close_flag() {
            return fail_promises(&mut self.load_settings_queries, &request_aborted_error());
        }
        if self.settings.are_inited {
            assert!(self.load_settings_queries.is_empty());
            return;
        }
        if value.is_empty() {
            tdutils::log_info!("Autosave settings aren't found in database");
            return self.reload_autosave_settings(env);
        }

        tdutils::log_info!("Successfully loaded autosave settings from database");

        let parse = || -> Result<AutosaveSettings, &'static str> {
            let mut parser = Parser::new_log_event(value)?;
            AutosaveSettings::parse(&mut parser)
        };
        let loaded = match parse() {
            Ok(settings) => settings,
            Err(error) => {
                tdutils::log_error!("Can't load autosave settings: {error}");
                self.settings = AutosaveSettings::default();
                // the C++ also erases the binlog pmc key here with the
                // resolve-failure branch (cpp:313-317); the collapse keeps
                // the corrupt-blob reset + reload
                return self.reload_autosave_settings(env);
            }
        };

        // `Dependencies::add_dialog_and_dependencies` +
        // `resolve_force` (cpp:309-317): the collapse force-creates every
        // exception dialog (always resolvable), with the C++
        // resolve-failure erase falling away with the dependency engine.
        for dialog_id in loaded.exceptions.keys().copied().collect::<Vec<_>>() {
            env.force_create_dialog(dialog_id);
        }

        self.settings = loaded;
        self.send_update_autosave_settings(
            env,
            td_api::AutosaveSettingsScope::autosaveSettingsScopePrivateChats(
                td_api::autosaveSettingsScopePrivateChats,
            ),
            &self.settings.user_settings,
        );
        self.send_update_autosave_settings(
            env,
            td_api::AutosaveSettingsScope::autosaveSettingsScopeGroupChats(
                td_api::autosaveSettingsScopeGroupChats,
            ),
            &self.settings.chat_settings,
        );
        self.send_update_autosave_settings(
            env,
            td_api::AutosaveSettingsScope::autosaveSettingsScopeChannelChats(
                td_api::autosaveSettingsScopeChannelChats,
            ),
            &self.settings.broadcast_settings,
        );
        let exception_dialogs: Vec<(DialogId, DialogAutosaveSettings)> = self
            .settings
            .exceptions
            .iter()
            .map(|(dialog_id, settings)| (*dialog_id, settings.clone()))
            .collect();
        for (dialog_id, settings) in &exception_dialogs {
            self.send_update_autosave_settings(
                env,
                td_api::AutosaveSettingsScope::autosaveSettingsScopeChat(
                    td_api::autosaveSettingsScopeChat {
                        chat_id: dialog_id.get(),
                    },
                ),
                settings,
            );
        }

        let promises = std::mem::take(&mut self.load_settings_queries);
        for mut promise in promises {
            promise.set_value(self.settings.get_autosave_settings_object());
        }
    }

    /// `reload_autosave_settings` (cpp:337-352) — also the C++
    /// `UpdatesManager` before/after-get-difference hook (cpp:4978/2413);
    /// the net query is the `[blocked: net]` seam.
    pub fn reload_autosave_settings(&mut self, env: &mut impl AutosaveManagerEnv) {
        if env.close_flag() {
            return fail_promises(&mut self.load_settings_queries, &request_aborted_error());
        }
        if self.settings.are_being_reloaded {
            self.settings.need_reload = true;
            return;
        }
        self.settings.are_being_reloaded = true;

        env.send_get_auto_save_settings_query();
    }

    /// `on_get_autosave_settings` (cpp:354-429) — the shared answer
    /// handler of the get query; the C++ `SCOPE_EXIT` need-reload tail
    /// runs on every exit, after the body.
    pub fn on_get_auto_save_settings(
        &mut self,
        env: &mut impl AutosaveManagerEnv,
        r_settings: Result<telegram_api::account_AutoSaveSettings, Status>,
    ) {
        assert!(
            self.settings.are_being_reloaded,
            "on_get_auto_save_settings without a pending reload"
        );
        self.settings.are_being_reloaded = false;
        let need_reload = std::mem::take(&mut self.settings.need_reload);

        let settings = match r_settings {
            Err(error) => {
                fail_promises(&mut self.load_settings_queries, &error);
                if need_reload {
                    self.reload_autosave_settings(env);
                }
                return;
            }
            Ok(settings) => settings,
        };

        // `on_get_users`/`on_get_chats` (cpp:371-372) — the wire user/chat
        // intakes are the M.4/M.5 manager seams (the AccountManagerEnv
        // convention); the answer's vectors drop here.
        drop(settings.chats);
        drop(settings.users);

        // the C++ constructor CHECKs the non-null wire settings
        let new_user_settings = DialogAutosaveSettings::from_telegram(
            settings
                .users_settings
                .as_ref()
                .expect("users_settings must be non-empty"),
        );
        let new_chat_settings = DialogAutosaveSettings::from_telegram(
            settings
                .chats_settings
                .as_ref()
                .expect("chats_settings must be non-empty"),
        );
        let new_broadcast_settings = DialogAutosaveSettings::from_telegram(
            settings
                .broadcasts_settings
                .as_ref()
                .expect("broadcasts_settings must be non-empty"),
        );

        self.settings.are_inited = true;
        if self.settings.user_settings != new_user_settings {
            self.settings.user_settings = new_user_settings;
            self.send_update_autosave_settings(
                env,
                td_api::AutosaveSettingsScope::autosaveSettingsScopePrivateChats(
                    td_api::autosaveSettingsScopePrivateChats,
                ),
                &self.settings.user_settings,
            );
        }
        if self.settings.chat_settings != new_chat_settings {
            self.settings.chat_settings = new_chat_settings;
            self.send_update_autosave_settings(
                env,
                td_api::AutosaveSettingsScope::autosaveSettingsScopeGroupChats(
                    td_api::autosaveSettingsScopeGroupChats,
                ),
                &self.settings.chat_settings,
            );
        }
        if self.settings.broadcast_settings != new_broadcast_settings {
            self.settings.broadcast_settings = new_broadcast_settings;
            self.send_update_autosave_settings(
                env,
                td_api::AutosaveSettingsScope::autosaveSettingsScopeChannelChats(
                    td_api::autosaveSettingsScopeChannelChats,
                ),
                &self.settings.broadcast_settings,
            );
        }
        let mut exception_dialog_ids: Vec<DialogId> =
            self.settings.exceptions.keys().copied().collect();
        for exception in &settings.exceptions {
            let Some(peer) = &exception.peer else {
                continue;
            };
            let dialog_id = DialogId::from_peer(peer);
            if !dialog_id.is_valid() {
                continue;
            }
            env.force_create_dialog(dialog_id);
            let new_settings = DialogAutosaveSettings::from_telegram(
                exception
                    .settings
                    .as_ref()
                    .expect("exception settings must be non-empty"),
            );
            let changed = {
                let current_settings = self.settings.exceptions.entry(dialog_id).or_default();
                let changed = *current_settings != new_settings;
                if changed {
                    *current_settings = new_settings.clone();
                }
                changed
            };
            if changed {
                self.send_update_autosave_settings(
                    env,
                    td_api::AutosaveSettingsScope::autosaveSettingsScopeChat(
                        td_api::autosaveSettingsScopeChat {
                            chat_id: dialog_manager::get_chat_id_object(
                                dialog_id,
                                "autosaveSettingsScopeChat",
                            ),
                        },
                    ),
                    &new_settings,
                );
            }
            exception_dialog_ids.retain(|&seen| seen != dialog_id);
        }
        for dialog_id in exception_dialog_ids {
            self.settings.exceptions.remove(&dialog_id);
            self.send_update_autosave_settings(
                env,
                td_api::AutosaveSettingsScope::autosaveSettingsScopeChat(
                    td_api::autosaveSettingsScopeChat {
                        chat_id: dialog_manager::get_chat_id_object(
                            dialog_id,
                            "autosaveSettingsScopeChat 2",
                        ),
                    },
                ),
                &DialogAutosaveSettings::default(),
            );
        }

        self.save_autosave_settings(env);

        let promises = std::mem::take(&mut self.load_settings_queries);
        for mut promise in promises {
            promise.set_value(self.settings.get_autosave_settings_object());
        }

        // the C++ SCOPE_EXIT tail (cpp:360-365)
        if need_reload {
            self.reload_autosave_settings(env);
        }
    }

    /// `save_autosave_settings` (cpp:431-437).
    fn save_autosave_settings(&self, env: &mut impl AutosaveManagerEnv) {
        if env.use_message_database() {
            tdutils::log_info!("Save autosave settings to database");
            let mut data = Vec::new();
            store_parse::store_log_event_version(&mut data);
            self.settings.store(&mut data);
            env.sqlite_pmc_set_raw(Self::get_autosave_settings_database_key(), &data);
        }
    }

    /// `set_autosave_settings` (cpp:439-496).
    pub fn set_autosave_settings(
        &mut self,
        env: &mut impl AutosaveManagerEnv,
        scope: Option<td_api::AutosaveSettingsScope>,
        settings: Option<td_api::ScopeAutosaveSettings>,
        mut promise: Promise<()>,
    ) {
        let Some(scope) = scope else {
            return promise.set_error(Status::error(400, "Scope must be non-empty"));
        };
        if !self.settings.are_inited {
            return promise.set_error(Status::error(400, "Autosave settings must be loaded first"));
        }
        let mut new_settings = DialogAutosaveSettings::from_td_api(settings.as_ref());

        // the scope switch (cpp:454-475): the scope-kind flags and the
        // per-dialog access check
        let mut users = false;
        let mut chats = false;
        let mut broadcasts = false;
        let mut dialog_id = DialogId::new(0);
        match &scope {
            td_api::AutosaveSettingsScope::autosaveSettingsScopePrivateChats(_) => {
                users = true;
            }
            td_api::AutosaveSettingsScope::autosaveSettingsScopeGroupChats(_) => {
                chats = true;
            }
            td_api::AutosaveSettingsScope::autosaveSettingsScopeChannelChats(_) => {
                broadcasts = true;
            }
            td_api::AutosaveSettingsScope::autosaveSettingsScopeChat(request) => {
                dialog_id = DialogId::new(request.chat_id);
                if let Err(status) = env.check_dialog_access(dialog_id) {
                    return promise.set_error(status);
                }
            }
        }
        // `old_settings = &settings_.exceptions_[dialog_id]` (cpp:471) —
        // the not-yet-known exception reads as the defaults, the C++
        // operator[] entry creation
        let old_settings = match &scope {
            td_api::AutosaveSettingsScope::autosaveSettingsScopePrivateChats(_) => {
                self.settings.user_settings.clone()
            }
            td_api::AutosaveSettingsScope::autosaveSettingsScopeGroupChats(_) => {
                self.settings.chat_settings.clone()
            }
            td_api::AutosaveSettingsScope::autosaveSettingsScopeChannelChats(_) => {
                self.settings.broadcast_settings.clone()
            }
            td_api::AutosaveSettingsScope::autosaveSettingsScopeChat(_) => self
                .settings
                .exceptions
                .get(&dialog_id)
                .cloned()
                .unwrap_or_default(),
        };

        if !dialog_id.is_valid() && !new_settings.are_inited {
            new_settings.are_inited = true;
            new_settings.max_video_file_size = DialogAutosaveSettings::DEFAULT_MAX_VIDEO_FILE_SIZE;
        }
        if old_settings == new_settings {
            return promise.set_value(());
        }

        // `new_settings.get_input_auto_save_settings()` (cpp:495) reads
        // the pre-move value (the C++ move of the POD-ish struct keeps the
        // fields readable); the Rust clone makes that explicit
        let input_settings = new_settings.get_input_auto_save_settings();
        if new_settings.are_inited {
            match &scope {
                td_api::AutosaveSettingsScope::autosaveSettingsScopePrivateChats(_) => {
                    self.settings.user_settings = new_settings.clone();
                }
                td_api::AutosaveSettingsScope::autosaveSettingsScopeGroupChats(_) => {
                    self.settings.chat_settings = new_settings.clone();
                }
                td_api::AutosaveSettingsScope::autosaveSettingsScopeChannelChats(_) => {
                    self.settings.broadcast_settings = new_settings.clone();
                }
                td_api::AutosaveSettingsScope::autosaveSettingsScopeChat(_) => {
                    self.settings
                        .exceptions
                        .insert(dialog_id, new_settings.clone());
                }
            }
            self.send_update_autosave_settings(env, scope, &new_settings);
        } else {
            assert!(dialog_id.is_valid());
            self.settings.exceptions.remove(&dialog_id);
            self.send_update_autosave_settings(env, scope, &DialogAutosaveSettings::default());
        }

        self.save_autosave_settings(env);

        env.send_save_auto_save_settings_query(
            users,
            chats,
            broadcasts,
            dialog_id,
            &input_settings,
        );
        self.net_blocked_promises.push(promise);
    }

    /// `clear_autosave_settings_exceptions` (cpp:498-509).
    pub fn clear_autosave_settings_exceptions(
        &mut self,
        env: &mut impl AutosaveManagerEnv,
        mut promise: Promise<()>,
    ) {
        if !self.settings.are_inited {
            return promise.set_error(Status::error(400, "Autosave settings must be loaded first"));
        }
        let exception_dialogs: Vec<DialogId> = self.settings.exceptions.keys().copied().collect();
        for dialog_id in exception_dialogs {
            self.send_update_autosave_settings(
                env,
                td_api::AutosaveSettingsScope::autosaveSettingsScopeChat(
                    td_api::autosaveSettingsScopeChat {
                        chat_id: dialog_id.get(),
                    },
                ),
                &DialogAutosaveSettings::default(),
            );
        }
        self.settings.exceptions.clear();
        self.save_autosave_settings(env);
        env.send_delete_auto_save_exceptions_query();
        self.net_blocked_promises.push(promise);
    }

    /// `get_update_autosave_settings` (cpp:511-515).
    fn get_update_autosave_settings(
        scope: td_api::AutosaveSettingsScope,
        settings: &DialogAutosaveSettings,
    ) -> td_api::Update {
        td_api::Update::updateAutosaveSettings(td_api::updateAutosaveSettings {
            scope: Some(scope),
            settings: settings.get_scope_autosave_settings_object(),
        })
    }

    /// `send_update_autosave_settings` (cpp:517-520) — the collapsed
    /// `send_closure(G()->td(), &Td::send_update, ...)` hop goes through
    /// the env outbox.
    fn send_update_autosave_settings(
        &self,
        env: &mut impl AutosaveManagerEnv,
        scope: td_api::AutosaveSettingsScope,
        settings: &DialogAutosaveSettings,
    ) {
        env.send_update(Self::get_update_autosave_settings(scope, settings));
    }

    /// `get_current_state` (cpp:522-536; Requests.cpp:2243).
    pub fn get_current_state(
        &self,
        _env: &impl AutosaveManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if !self.settings.are_inited {
            return;
        }
        updates.push(Self::get_update_autosave_settings(
            td_api::AutosaveSettingsScope::autosaveSettingsScopePrivateChats(
                td_api::autosaveSettingsScopePrivateChats,
            ),
            &self.settings.user_settings,
        ));
        updates.push(Self::get_update_autosave_settings(
            td_api::AutosaveSettingsScope::autosaveSettingsScopeGroupChats(
                td_api::autosaveSettingsScopeGroupChats,
            ),
            &self.settings.chat_settings,
        ));
        updates.push(Self::get_update_autosave_settings(
            td_api::AutosaveSettingsScope::autosaveSettingsScopeChannelChats(
                td_api::autosaveSettingsScopeChannelChats,
            ),
            &self.settings.broadcast_settings,
        ));
        for (dialog_id, settings) in &self.settings.exceptions {
            updates.push(Self::get_update_autosave_settings(
                td_api::AutosaveSettingsScope::autosaveSettingsScopeChat(
                    td_api::autosaveSettingsScopeChat {
                        chat_id: dialog_id.get(),
                    },
                ),
                settings,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tdutils::promise::PromiseInterface;

    struct TestEnv {
        close_flag: bool,
        use_message_database: bool,
        sqlite_pmc: BTreeMap<Vec<u8>, Vec<u8>>,
        updates: Vec<td_api::Update>,
        query_log: Vec<String>,
    }

    impl TestEnv {
        fn new(use_message_database: bool) -> TestEnv {
            TestEnv {
                close_flag: false,
                use_message_database,
                sqlite_pmc: BTreeMap::new(),
                updates: Vec::new(),
                query_log: Vec::new(),
            }
        }

        fn saved_settings(&self) -> Option<Vec<u8>> {
            self.sqlite_pmc
                .get(b"autosave_settings".as_slice())
                .cloned()
        }
    }

    impl AutosaveManagerEnv for TestEnv {
        fn close_flag(&self) -> bool {
            self.close_flag
        }

        fn use_message_database(&self) -> bool {
            self.use_message_database
        }

        fn sqlite_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.sqlite_pmc.get(key).cloned().unwrap_or_default()
        }

        fn sqlite_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.sqlite_pmc.insert(key.to_vec(), value.to_vec());
        }

        fn binlog_pmc_erase(&mut self, key: &str) {
            self.query_log.push(format!("binlog erase {key}"));
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn check_dialog_access(&mut self, dialog_id: DialogId) -> Result<(), Status> {
            if dialog_id.is_valid() {
                Ok(())
            } else {
                Err(Status::error(400, "Chat not found"))
            }
        }

        fn force_create_dialog(&mut self, dialog_id: DialogId) {
            self.query_log.push(format!("force create {dialog_id:?}"));
        }

        fn send_get_auto_save_settings_query(&mut self) {
            self.query_log.push("get".to_string());
        }

        fn send_save_auto_save_settings_query(
            &mut self,
            users: bool,
            chats: bool,
            broadcasts: bool,
            dialog_id: DialogId,
            _settings: &telegram_api::AutoSaveSettings,
        ) {
            self.query_log
                .push(format!("save {users} {chats} {broadcasts} {dialog_id:?}"));
        }

        fn send_delete_auto_save_exceptions_query(&mut self) {
            self.query_log.push("delete all".to_string());
        }
    }

    #[derive(Default)]
    struct Recorded<T>(Arc<Mutex<Option<T>>>);
    struct RecordingPromise<T: Send + 'static> {
        recorded: Recorded<T>,
    }

    impl<T: Send + 'static> PromiseInterface<T> for RecordingPromise<T> {
        fn set_value(&mut self, value: T) {
            *self.recorded.0.lock().unwrap() = Some(value);
        }

        fn set_error(&mut self, _error: Status) {}
    }

    fn recording_promise<T: Send + 'static>() -> (Promise<T>, Recorded<T>) {
        let recorded = Recorded(Arc::new(Mutex::new(None)));
        (
            Promise::from_interface(RecordingPromise {
                recorded: Recorded(recorded.0.clone()),
            }),
            recorded,
        )
    }

    fn wire_settings(photos: bool, videos: bool, size: i64) -> telegram_api::AutoSaveSettings {
        telegram_api::AutoSaveSettings {
            flags: VIDEO_MAX_SIZE_MASK,
            photos,
            videos,
            video_max_size: size,
        }
    }

    fn scope_chat(chat_id: i64) -> td_api::AutosaveSettingsScope {
        td_api::AutosaveSettingsScope::autosaveSettingsScopeChat(
            td_api::autosaveSettingsScopeChat { chat_id },
        )
    }

    fn private_scope() -> td_api::AutosaveSettingsScope {
        td_api::AutosaveSettingsScope::autosaveSettingsScopePrivateChats(
            td_api::autosaveSettingsScopePrivateChats,
        )
    }

    fn group_scope() -> td_api::AutosaveSettingsScope {
        td_api::AutosaveSettingsScope::autosaveSettingsScopeGroupChats(
            td_api::autosaveSettingsScopeGroupChats,
        )
    }

    fn inited_manager(user: bool, videos: bool, size: i64) -> AutosaveManager {
        let mut manager = AutosaveManager::new();
        manager.settings.are_inited = true;
        manager.settings.user_settings =
            DialogAutosaveSettings::from_telegram(&wire_settings(user, videos, size));
        // the stored state always carries all three scopes inited (the C++
        // settings_ only become inited through a full wire load)
        manager.settings.chat_settings =
            DialogAutosaveSettings::from_telegram(&wire_settings(false, false, 1));
        manager.settings.broadcast_settings =
            DialogAutosaveSettings::from_telegram(&wire_settings(false, false, 1));
        manager
    }

    fn update_scope(update: &td_api::Update) -> String {
        match update {
            td_api::Update::updateAutosaveSettings(u) => match &u.scope {
                Some(td_api::AutosaveSettingsScope::autosaveSettingsScopePrivateChats(_)) => {
                    "private".to_string()
                }
                Some(td_api::AutosaveSettingsScope::autosaveSettingsScopeGroupChats(_)) => {
                    "group".to_string()
                }
                Some(td_api::AutosaveSettingsScope::autosaveSettingsScopeChannelChats(_)) => {
                    "channel".to_string()
                }
                Some(td_api::AutosaveSettingsScope::autosaveSettingsScopeChat(c)) => {
                    format!("chat {}", c.chat_id)
                }
                None => "none".to_string(),
            },
            _ => "other".to_string(),
        }
    }

    fn update_settings(update: &td_api::Update) -> Option<td_api::ScopeAutosaveSettings> {
        match update {
            td_api::Update::updateAutosaveSettings(u) => u.settings.clone(),
            _ => panic!("expected updateAutosaveSettings"),
        }
    }

    fn ok_answer(
        users_settings: telegram_api::AutoSaveSettings,
        chats_settings: telegram_api::AutoSaveSettings,
        broadcasts_settings: telegram_api::AutoSaveSettings,
        exceptions: Vec<telegram_api::AutoSaveException>,
    ) -> telegram_api::account_AutoSaveSettings {
        telegram_api::account_AutoSaveSettings {
            users_settings: Some(users_settings),
            chats_settings: Some(chats_settings),
            broadcasts_settings: Some(broadcasts_settings),
            exceptions,
            chats: Vec::new(),
            users: Vec::new(),
        }
    }

    #[test]
    fn dialog_settings_clamp_the_video_size() {
        let settings = DialogAutosaveSettings::from_telegram(&wire_settings(true, true, 1));
        assert_eq!(
            settings.max_video_file_size(),
            DialogAutosaveSettings::MIN_MAX_VIDEO_FILE_SIZE
        );
        let settings = DialogAutosaveSettings::from_telegram(&wire_settings(true, true, i64::MAX));
        assert_eq!(
            settings.max_video_file_size(),
            DialogAutosaveSettings::MAX_MAX_VIDEO_FILE_SIZE
        );
        let settings =
            DialogAutosaveSettings::from_telegram(&wire_settings(true, true, 50_000_000));
        assert_eq!(settings.max_video_file_size(), 50_000_000);
    }

    #[test]
    fn td_api_settings_null_stays_not_inited() {
        let settings = DialogAutosaveSettings::from_td_api(None);
        assert!(!settings.are_inited());
        assert!(settings.get_scope_autosave_settings_object().is_none());
        // the wire object carries no video size flag and no bare flags
        let input = settings.get_input_auto_save_settings();
        assert_eq!(input.flags & VIDEO_MAX_SIZE_MASK, 0);
        assert!(!input.photos && !input.videos);

        let settings = DialogAutosaveSettings::from_td_api(Some(&td_api::ScopeAutosaveSettings {
            autosave_photos: true,
            autosave_videos: false,
            max_video_file_size: 10_000_000,
        }));
        assert!(settings.are_inited());
        assert_eq!(settings.max_video_file_size(), 10_000_000);
    }

    #[test]
    fn get_parks_until_reloaded_and_answers_the_query() {
        let mut env = TestEnv::new(true);
        let mut manager = AutosaveManager::new();

        // the first request loads: pmc is empty -> the net query goes out
        let (promise, recorded) = recording_promise();
        manager.get_autosave_settings(&mut env, promise);
        assert!(recorded.0.lock().unwrap().is_none());
        assert_eq!(env.query_log, vec!["get".to_string()]);
        assert_eq!(manager.load_settings_queries.len(), 1);

        // the second request queues behind the first (no new query)
        let (promise, _) = recording_promise();
        manager.get_autosave_settings(&mut env, promise);
        assert_eq!(env.query_log.len(), 1);
        assert_eq!(manager.load_settings_queries.len(), 2);

        // the answer inits the settings and resolves the whole queue
        manager.on_get_auto_save_settings(
            &mut env,
            Ok(ok_answer(
                wire_settings(true, false, 10_000_000),
                wire_settings(false, true, 20_000_000),
                wire_settings(true, true, 30_000_000),
                Vec::new(),
            )),
        );
        assert!(manager.load_settings_queries.is_empty());
        assert!(manager.settings.are_inited);
        assert!(!env.saved_settings().unwrap().is_empty(), "saved to pmc");
        assert_eq!(env.updates.len(), 3, "one update per changed scope");
        assert_eq!(update_scope(&env.updates[0]), "private");
        assert_eq!(update_scope(&env.updates[1]), "group");
        assert_eq!(update_scope(&env.updates[2]), "channel");

        // the next request answers immediately from memory
        let (promise, recorded) = recording_promise();
        manager.get_autosave_settings(&mut env, promise);
        let value = recorded.0.lock().unwrap().take().unwrap();
        assert!(value.exceptions.is_empty());
        assert!(value.private_chat_settings.unwrap().autosave_photos);
        assert_eq!(env.query_log.len(), 1, "no re-query");
    }

    #[test]
    fn get_loads_the_saved_settings_from_the_database() {
        // seed the pmc through a first manager's save
        let mut env = TestEnv::new(true);
        let mut manager = inited_manager(true, true, 40_000_000);
        manager.settings.chat_settings =
            DialogAutosaveSettings::from_telegram(&wire_settings(false, false, 1));
        manager.settings.exceptions.insert(
            DialogId::new(200),
            DialogAutosaveSettings::from_telegram(&wire_settings(true, false, 2)),
        );
        manager.save_autosave_settings(&mut env);

        // a fresh manager loads them and answers without the net query
        let mut fresh_env = TestEnv::new(true);
        fresh_env.sqlite_pmc = env.sqlite_pmc.clone();
        let mut fresh = AutosaveManager::new();
        let (promise, recorded) = recording_promise();
        fresh.get_autosave_settings(&mut fresh_env, promise);
        assert!(recorded.0.lock().unwrap().is_some());
        assert!(
            !fresh_env.query_log.iter().any(|q| q == "get"),
            "no net query for a db hit"
        );
        // the load replays the per-scope updates
        assert_eq!(fresh_env.updates.len(), 4);
        assert_eq!(update_scope(&fresh_env.updates[0]), "private");
        assert_eq!(update_scope(&fresh_env.updates[1]), "group");
        assert_eq!(update_scope(&fresh_env.updates[2]), "channel");
        assert_eq!(update_scope(&fresh_env.updates[3]), "chat 200");
        assert_eq!(fresh.settings, manager.settings);
    }

    #[test]
    fn get_resets_and_reloads_on_a_corrupt_blob() {
        let mut env = TestEnv::new(true);
        env.sqlite_pmc
            .insert(b"autosave_settings".to_vec(), vec![0x01, 0x02, 0x03]);
        let mut manager = AutosaveManager::new();
        let (promise, recorded) = recording_promise();
        manager.get_autosave_settings(&mut env, promise);
        assert!(recorded.0.lock().unwrap().is_none(), "rides the reload");
        assert_eq!(env.query_log, vec!["get".to_string()]);
        assert!(!manager.settings.are_inited);
    }

    #[test]
    fn on_get_applies_exception_changes_and_removals() {
        let mut env = TestEnv::new(true);
        let mut manager = AutosaveManager::new();
        manager.settings.are_being_reloaded = true;

        let exception = telegram_api::AutoSaveException {
            peer: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                user_id: 100,
            })),
            settings: Some(wire_settings(true, true, 5_000_000)),
        };
        manager.on_get_auto_save_settings(
            &mut env,
            Ok(ok_answer(
                wire_settings(true, false, 1),
                wire_settings(true, false, 1),
                wire_settings(true, false, 1),
                vec![exception],
            )),
        );
        assert_eq!(manager.settings.exceptions.len(), 1);
        let chat_updates = env
            .updates
            .iter()
            .filter(|u| update_scope(u) == "chat 100")
            .count();
        assert_eq!(chat_updates, 1, "the new exception sends its update");
        assert_eq!(
            update_settings(env.updates.last().unwrap())
                .unwrap()
                .max_video_file_size,
            5_000_000
        );

        // the same settings again: no update
        env.updates.clear();
        manager.settings.are_being_reloaded = true;
        manager.on_get_auto_save_settings(
            &mut env,
            Ok(ok_answer(
                wire_settings(true, false, 1),
                wire_settings(true, false, 1),
                wire_settings(true, false, 1),
                vec![telegram_api::AutoSaveException {
                    peer: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                        user_id: 100,
                    })),
                    settings: Some(wire_settings(true, true, 5_000_000)),
                }],
            )),
        );
        assert!(env.updates.is_empty());

        // the exception gone from the answer: removed + the reset update
        manager.settings.are_being_reloaded = true;
        manager.on_get_auto_save_settings(
            &mut env,
            Ok(ok_answer(
                wire_settings(true, false, 1),
                wire_settings(true, false, 1),
                wire_settings(true, false, 1),
                Vec::new(),
            )),
        );
        assert!(manager.settings.exceptions.is_empty());
        assert_eq!(update_scope(&env.updates[0]), "chat 100");
        assert!(update_settings(&env.updates[0]).is_none());
    }

    #[test]
    fn on_get_error_fails_the_queue_and_honors_need_reload() {
        let mut env = TestEnv::new(true);
        let mut manager = AutosaveManager::new();
        let (promise, _) = recording_promise();
        manager.get_autosave_settings(&mut env, promise);

        manager.on_get_auto_save_settings(&mut env, Err(Status::error(429, "Too Many Requests")));
        assert!(manager.load_settings_queries.is_empty());
        assert!(!manager.settings.are_inited);

        // a reload request that arrives mid-flight sets need_reload; the
        // answer applies, resolves and then restarts the query (the C++
        // SCOPE_EXIT tail)
        let (promise, _) = recording_promise();
        manager.get_autosave_settings(&mut env, promise);
        assert!(manager.settings.are_being_reloaded);
        manager.reload_autosave_settings(&mut env);
        assert!(manager.settings.need_reload);
        manager.on_get_auto_save_settings(
            &mut env,
            Ok(ok_answer(
                wire_settings(true, false, 1),
                wire_settings(true, false, 1),
                wire_settings(true, false, 1),
                Vec::new(),
            )),
        );
        assert!(manager.settings.are_inited, "the answer applied");
        assert_eq!(env.query_log.last().unwrap(), "get", "reloaded");
        assert!(manager.settings.are_being_reloaded, "the reload is pending");
    }

    #[test]
    fn set_rejects_bad_scope_and_not_inited_settings() {
        let mut env = TestEnv::new(true);
        let mut manager = AutosaveManager::new();

        // the null scope rejects
        let (promise, _) = recording_promise::<()>();
        manager.set_autosave_settings(&mut env, None, None, promise);
        assert!(env.query_log.is_empty());
        assert!(manager.net_blocked_promises.is_empty());

        // the not-inited settings reject before everything else
        let (promise, _) = recording_promise::<()>();
        manager.set_autosave_settings(&mut env, Some(private_scope()), None, promise);
        assert!(env.query_log.is_empty());
        assert!(manager.net_blocked_promises.is_empty());
    }

    #[test]
    fn set_updates_scope_settings_and_saves() {
        let mut env = TestEnv::new(true);
        let mut manager = inited_manager(false, false, 10_000_000);

        // identical settings answer without a query
        let (promise, recorded) = recording_promise();
        manager.set_autosave_settings(
            &mut env,
            Some(private_scope()),
            Some(td_api::ScopeAutosaveSettings {
                autosave_photos: false,
                autosave_videos: false,
                max_video_file_size: 10_000_000,
            }),
            promise,
        );
        assert!(recorded.0.lock().unwrap().is_some());
        assert!(env.query_log.is_empty());
        assert!(env.updates.is_empty());

        // a change sends the update, saves and parks the promise at the query
        let (promise, _) = recording_promise();
        manager.set_autosave_settings(
            &mut env,
            Some(private_scope()),
            Some(td_api::ScopeAutosaveSettings {
                autosave_photos: true,
                autosave_videos: true,
                max_video_file_size: 20_000_000,
            }),
            promise,
        );
        assert_eq!(env.query_log.len(), 1);
        assert!(env.query_log[0].starts_with("save true false false"));
        assert_eq!(env.updates.len(), 1);
        assert_eq!(update_scope(&env.updates[0]), "private");
        assert_eq!(manager.net_blocked_promises.len(), 1);
        assert_eq!(
            manager.settings.user_settings.max_video_file_size(),
            20_000_000
        );
        assert!(env.saved_settings().is_some());

        // a null settings object resets the scope to the default with the
        // DEFAULT_MAX_VIDEO_FILE_SIZE fill-in (cpp:476-479)
        let (promise, _) = recording_promise();
        manager.set_autosave_settings(&mut env, Some(group_scope()), None, promise);
        assert_eq!(
            manager.settings.chat_settings.max_video_file_size(),
            DialogAutosaveSettings::DEFAULT_MAX_VIDEO_FILE_SIZE
        );
        assert_eq!(env.query_log.len(), 2);
        assert!(env.query_log[1].starts_with("save false true false"));
    }

    #[test]
    fn set_upserts_and_deletes_exception_dialogs() {
        let mut env = TestEnv::new(true);
        let mut manager = inited_manager(false, false, 10_000_000);

        // the exception insert
        let (promise, _) = recording_promise();
        manager.set_autosave_settings(
            &mut env,
            Some(scope_chat(300)),
            Some(td_api::ScopeAutosaveSettings {
                autosave_photos: true,
                autosave_videos: false,
                max_video_file_size: 3_000_000,
            }),
            promise,
        );
        assert_eq!(manager.settings.exceptions.len(), 1);
        assert_eq!(env.updates.len(), 1);
        assert_eq!(update_scope(&env.updates[0]), "chat 300");
        let queries_so_far = env.query_log.len();

        // the invalid chat id rejects through the access seam
        let (promise, _) = recording_promise::<()>();
        manager.set_autosave_settings(&mut env, Some(scope_chat(0)), None, promise);
        assert_eq!(
            env.query_log.len(),
            queries_so_far,
            "rejected before the query"
        );

        // the null settings delete the exception (cpp:487-490)
        let (promise, _) = recording_promise();
        manager.set_autosave_settings(&mut env, Some(scope_chat(300)), None, promise);
        assert!(manager.settings.exceptions.is_empty());
        assert_eq!(update_scope(env.updates.last().unwrap()), "chat 300");
        assert!(update_settings(env.updates.last().unwrap()).is_none());
        assert_eq!(env.query_log.len(), queries_so_far + 1);
    }

    #[test]
    fn clear_removes_every_exception() {
        let mut env = TestEnv::new(true);
        let mut manager = inited_manager(false, false, 10_000_000);
        manager.settings.exceptions.insert(
            DialogId::new(11),
            DialogAutosaveSettings::from_telegram(&wire_settings(true, true, 1)),
        );
        manager.settings.exceptions.insert(
            DialogId::new(22),
            DialogAutosaveSettings::from_telegram(&wire_settings(true, true, 2)),
        );

        let (promise, _) = recording_promise();
        manager.clear_autosave_settings_exceptions(&mut env, promise);
        assert!(manager.settings.exceptions.is_empty());
        assert_eq!(env.updates.len(), 2);
        let scopes: Vec<String> = env.updates.iter().map(update_scope).collect();
        assert!(scopes.contains(&"chat 11".to_string()));
        assert!(scopes.contains(&"chat 22".to_string()));
        assert_eq!(env.query_log.last().unwrap(), "delete all");
        assert_eq!(manager.net_blocked_promises.len(), 1);
        assert!(env.saved_settings().is_some());

        // not-inited settings reject
        let mut fresh = AutosaveManager::new();
        let mut fresh_env = TestEnv::new(true);
        let (promise, _) = recording_promise::<()>();
        fresh.clear_autosave_settings_exceptions(&mut fresh_env, promise);
        assert!(fresh.net_blocked_promises.is_empty());
        assert!(fresh_env.query_log.is_empty());
    }

    #[test]
    fn get_current_state_emits_the_inited_scope_blocks() {
        let env = TestEnv::new(true);
        let mut manager = inited_manager(true, false, 10_000_000);
        manager.settings.exceptions.insert(
            DialogId::new(7),
            DialogAutosaveSettings::from_telegram(&wire_settings(true, true, 7)),
        );

        let mut updates = Vec::new();
        manager.get_current_state(&env, &mut updates);
        assert_eq!(updates.len(), 4);
        assert_eq!(update_scope(&updates[0]), "private");
        assert_eq!(update_scope(&updates[1]), "group");
        assert_eq!(update_scope(&updates[2]), "channel");
        assert_eq!(update_scope(&updates[3]), "chat 7");

        // not-inited settings emit nothing
        let mut updates = Vec::new();
        let fresh = AutosaveManager::new();
        fresh.get_current_state(&env, &mut updates);
        assert!(updates.is_empty());
    }

    #[test]
    fn reload_fails_the_queue_on_close() {
        let mut env = TestEnv::new(true);
        env.close_flag = true;
        let mut manager = AutosaveManager::new();
        let (promise, _) = recording_promise();
        manager.get_autosave_settings(&mut env, promise);
        assert!(env.query_log.is_empty());
    }

    #[test]
    fn store_parse_round_trips_the_settings() {
        let mut manager = inited_manager(true, false, 10_000_000);
        manager.settings.chat_settings =
            DialogAutosaveSettings::from_telegram(&wire_settings(false, true, 2));
        manager.settings.broadcast_settings =
            DialogAutosaveSettings::from_telegram(&wire_settings(true, true, 3));
        manager.settings.exceptions.insert(
            DialogId::new(-1001),
            DialogAutosaveSettings::from_telegram(&wire_settings(true, true, 4)),
        );
        manager.settings.exceptions.insert(
            DialogId::new(5000),
            DialogAutosaveSettings::from_telegram(&wire_settings(false, false, 5)),
        );

        let mut data = Vec::new();
        store_parse::store_log_event_version(&mut data);
        manager.settings.store(&mut data);

        let mut parser = Parser::new_log_event(&data).unwrap();
        let parsed = AutosaveSettings::parse(&mut parser).unwrap();
        assert_eq!(parsed, manager.settings);
        assert_eq!(parsed.exceptions.len(), 2);
    }
}
