//! `WebBrowserManager` (td/telegram/WebBrowserManager.{h,cpp}) — the M.31
//! Wave-3 slice: the in-app web browser settings, the domain exceptions
//! and the `getLinkWebBrowserType` classification. The four server queries
//! are `[blocked: net]` seams (the result handlers are real and unit-tested
//! against constructed answers); the settings ride the binlog pmc under the
//! `web_browser_settings` key. The C++ `Td::ResultHandler` query classes
//! collapse into the [`WebBrowserManagerEnv`] query seams, the
//! `account_WebBrowserSettings` wire union splits by constructor id in
//! [`decode_web_browser_settings_answer`] (the `config_result` precedent).

use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::web_browser_settings::WebBrowserSettings;
use tdmtproto::telegram_api;
use tdutils::http_url::parse_url;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The `G()`/`td_` surface of the manager. The cross-manager arms are the
/// `[blocked: net]` query seams; the `send_update` outbox is drained by
/// [`crate::td::Td::flush_user_updates`].
pub trait WebBrowserManagerEnv {
    /// `td_->auth_manager_->is_authorized()`.
    fn is_authorized(&self) -> bool;

    /// `td_->auth_manager_->is_bot()`.
    fn is_bot(&self) -> bool;

    /// `G()->td_db()->get_binlog_pmc()->get`.
    fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;

    /// `G()->td_db()->get_binlog_pmc()->set`.
    fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]);

    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    /// `GetWebBrowserSettingsQuery::send` (cpp:34-36) — `[blocked: net]`;
    /// the answer arrives through
    /// [`WebBrowserManager::on_get_web_browser_settings`].
    fn send_get_web_browser_settings_query(&mut self, hash: i64) {
        tdutils::log_info!("Send GetWebBrowserSettings(hash = {hash}) — [blocked: net]");
    }

    /// `UpdateWebBrowserSettingsQuery::send` (cpp:61-64) — `[blocked: net]`.
    fn send_update_web_browser_settings_query(
        &mut self,
        open_external_browser: bool,
        display_close_button: bool,
    ) {
        tdutils::log_info!(
            "Send UpdateWebBrowserSettings(open_external_browser = {open_external_browser}, \
             display_close_button = {display_close_button}) — [blocked: net]"
        );
    }

    /// `ToggleWebBrowserSettingsExceptionQuery::send` (cpp:87-94) —
    /// `[blocked: net]`; the C++ `on_result` routes the answer into
    /// `updates_manager_->on_get_updates`.
    fn send_toggle_web_browser_settings_exception_query(
        &mut self,
        is_deletion: bool,
        open_external_browser: bool,
        url: &str,
    ) {
        tdutils::log_info!(
            "Send ToggleWebBrowserSettingsException(is_deletion = {is_deletion}, \
             open_external_browser = {open_external_browser}, url = {url}) — [blocked: net]"
        );
    }

    /// `DeleteWebBrowserSettingsExceptionsQuery::send` (cpp:121-123) —
    /// `[blocked: net]`.
    fn send_delete_web_browser_settings_exceptions_query(&mut self) {
        tdutils::log_info!("Send DeleteWebBrowserSettingsExceptions — [blocked: net]");
    }
}

/// The `account.WebBrowserSettings` answer split by constructor id — the
/// C++ `on_result` checks the id before the typed parse (the
/// `config_result::decode_app_config_answer` precedent).
#[derive(Debug)]
pub enum WebBrowserSettingsAnswer {
    NotModified,
    Settings(telegram_api::account_webBrowserSettings),
}

/// The `GetWebBrowserSettingsQuery::on_result` decode
/// (WebBrowserManager.cpp:38-49).
pub fn decode_web_browser_settings_answer(
    bytes: &[u8],
) -> std::result::Result<WebBrowserSettingsAnswer, Status> {
    if bytes.len() >= 4
        && i32::from_le_bytes(bytes[0..4].try_into().unwrap())
            == telegram_api::account_webBrowserSettingsNotModified::ID
    {
        return Ok(WebBrowserSettingsAnswer::NotModified);
    }
    match telegram_api::account_webBrowserSettings::parse(bytes) {
        Ok(settings) => Ok(WebBrowserSettingsAnswer::Settings(settings)),
        Err(error) => {
            tdutils::log_error!("Can't parse account.webBrowserSettings: {error}");
            Err(Status::error(500, error))
        }
    }
}

/// The C++ `WebBrowserManager` actor; the Rust manager is a plain struct
/// held by `Td` (the actor envelope collapses, the M.13 convention).
#[derive(Default)]
pub struct WebBrowserManager {
    /// `settings_` (WebBrowserManager.h:53).
    settings: WebBrowserSettings,
    /// The `()`-typed promises parked at their `[blocked: net]` query
    /// sends — the C++ promise rides the query actor until the answer,
    /// the Rust collapse parks it here until the net query actors land
    /// (the UserManager `net_blocked_promises` convention).
    net_blocked_promises: Vec<Promise<()>>,
}

impl WebBrowserManager {
    /// `WebBrowserManager()` (cpp:139-140).
    pub fn new() -> WebBrowserManager {
        WebBrowserManager {
            settings: WebBrowserSettings::default(),
            net_blocked_promises: Vec::new(),
        }
    }

    /// `start_up` (cpp:142-144) — `load_web_browser_settings`.
    pub fn load_web_browser_settings(&mut self, env: &mut impl WebBrowserManagerEnv) {
        if !env.is_authorized() || env.is_bot() {
            return;
        }

        let log_event_string = env.pmc_get_raw(b"web_browser_settings");
        if !log_event_string.is_empty() {
            // `log_event_parse(settings_, log_event_string)`; the C++ only
            // checks the status and resets to defaults on failure.
            let parse = || -> std::result::Result<WebBrowserSettings, &'static str> {
                let mut parser = Parser::new_log_event(&log_event_string)?;
                WebBrowserSettings::parse(&mut parser)
            };
            match parse() {
                Ok(settings) => self.settings = settings,
                Err(error) => {
                    tdutils::log_error!(
                        "Failed to parse web browser settings from binlog: {error}"
                    );
                    self.settings = WebBrowserSettings::default();
                }
            }
        }
        self.send_update_web_browser_settings(env);

        if self.settings.get_hash() == 0 {
            self.reload_web_browser_settings(env);
        }
    }

    /// `save_web_browser_settings` (cpp:182-185).
    fn save_web_browser_settings(&self, env: &mut impl WebBrowserManagerEnv) {
        let mut data = Vec::new();
        store_parse::store_log_event_version(&mut data);
        self.settings.store(&mut data);
        env.pmc_set_raw(b"web_browser_settings", &data);
    }

    /// `get_update_web_browser_settings_object` +
    /// `send_update_web_browser_settings` (cpp:282-288) — the collapsed
    /// `send_closure(G()->td(), &Td::send_update, ...)` hop goes through
    /// the env outbox.
    fn send_update_web_browser_settings(&self, env: &mut impl WebBrowserManagerEnv) {
        env.send_update(td_api::Update::updateWebBrowserSettings(
            td_api::updateWebBrowserSettings {
                settings: Some(self.settings.get_web_browser_settings_object()),
            },
        ));
    }

    /// `on_authorization_success` (cpp:150-156; called from
    /// AuthManager.cpp:1556).
    pub fn on_authorization_success(&mut self, env: &mut impl WebBrowserManagerEnv) {
        if env.is_bot() {
            return;
        }
        self.send_update_web_browser_settings(env);
        self.reload_web_browser_settings(env);
    }

    /// `reload_web_browser_settings` (cpp:187-194) — the
    /// `GetWebBrowserSettingsQuery` is `[blocked: net]`; the answer handler
    /// is [`WebBrowserManager::on_get_web_browser_settings`]. The C++ also
    /// reloads from `UpdatesManager::after_get_difference` (cpp:2466), a
    /// seam for the M.7 integration.
    pub fn reload_web_browser_settings(&mut self, env: &mut impl WebBrowserManagerEnv) {
        env.send_get_web_browser_settings_query(self.settings.get_hash());
    }

    /// `on_get_web_browser_settings` (cpp:196-214) — the shared result
    /// handler of the get/update/delete-exceptions queries.
    pub fn on_get_web_browser_settings(
        &mut self,
        env: &mut impl WebBrowserManagerEnv,
        result: std::result::Result<WebBrowserSettingsAnswer, Status>,
        mut promise: Promise<()>,
    ) {
        match result {
            Err(error) => promise.set_error(error),
            Ok(WebBrowserSettingsAnswer::NotModified) => promise.set_value(()),
            Ok(WebBrowserSettingsAnswer::Settings(settings)) => {
                let new_settings = WebBrowserSettings::from_telegram(settings);
                if new_settings != self.settings {
                    self.settings = new_settings;
                    self.save_web_browser_settings(env);
                    self.send_update_web_browser_settings(env);
                }
                promise.set_value(());
            }
        }
    }

    /// `on_update_web_browser_settings` (cpp:216-222) — the update intake
    /// routes the telegram update here (the M.7 fan-out flips at the sink
    /// integration).
    pub fn on_update_web_browser_settings(
        &mut self,
        env: &mut impl WebBrowserManagerEnv,
        update: telegram_api::updateWebBrowserSettings,
    ) {
        if self.settings.update_from(update) {
            self.save_web_browser_settings(env);
            self.send_update_web_browser_settings(env);
        }
    }

    /// `on_update_web_browser_exception` (cpp:224-230).
    pub fn on_update_web_browser_exception(
        &mut self,
        env: &mut impl WebBrowserManagerEnv,
        update: telegram_api::updateWebBrowserException,
    ) {
        if self.settings.update_from_exception(update) {
            self.save_web_browser_settings(env);
            self.send_update_web_browser_settings(env);
        }
    }

    /// `update_web_browser_settings` (cpp:232-242) — the
    /// `UpdateWebBrowserSettingsQuery` is `[blocked: net]`; the promise
    /// parks until the query answer arrives.
    pub fn update_web_browser_settings(
        &mut self,
        env: &mut impl WebBrowserManagerEnv,
        open_external_browser: bool,
        display_close_button: bool,
        promise: Promise<()>,
    ) {
        env.send_update_web_browser_settings_query(open_external_browser, display_close_button);
        self.net_blocked_promises.push(promise);
    }

    /// `add_web_browser_settings_exception` (cpp:244-248).
    pub fn add_web_browser_settings_exception(
        &mut self,
        env: &mut impl WebBrowserManagerEnv,
        open_external_browser: bool,
        url: &str,
        promise: Promise<()>,
    ) {
        env.send_toggle_web_browser_settings_exception_query(false, open_external_browser, url);
        self.net_blocked_promises.push(promise);
    }

    /// `remove_web_browser_settings_exception` (cpp:250-252).
    pub fn remove_web_browser_settings_exception(
        &mut self,
        env: &mut impl WebBrowserManagerEnv,
        url: &str,
        promise: Promise<()>,
    ) {
        env.send_toggle_web_browser_settings_exception_query(true, false, url);
        self.net_blocked_promises.push(promise);
    }

    /// `remove_all_web_browser_settings_exceptions` (cpp:254-262).
    pub fn remove_all_web_browser_settings_exceptions(
        &mut self,
        env: &mut impl WebBrowserManagerEnv,
        promise: Promise<()>,
    ) {
        env.send_delete_web_browser_settings_exceptions_query();
        self.net_blocked_promises.push(promise);
    }

    /// `get_web_browser_type` (cpp:264-280) — the classification is local
    /// (no net), answered immediately.
    pub fn get_web_browser_type(
        &self,
        url: &str,
    ) -> std::result::Result<td_api::WebBrowserType, Status> {
        match parse_url(url).into_inner() {
            Ok(http_url) => {
                if self.settings.get_open_external_browser(&http_url.host) {
                    Ok(td_api::WebBrowserType::webBrowserTypeExternal(
                        td_api::webBrowserTypeExternal,
                    ))
                } else {
                    Ok(td_api::WebBrowserType::webBrowserTypeInApp(
                        td_api::webBrowserTypeInApp,
                    ))
                }
            }
            Err(_) => {
                let url = url.to_ascii_lowercase();
                if url.starts_with("tonsite://") {
                    return Ok(td_api::WebBrowserType::webBrowserTypeInApp(
                        td_api::webBrowserTypeInApp,
                    ));
                }
                Err(Status::error(400, "Invalid HTTP URL specified"))
            }
        }
    }

    /// `get_current_state` (cpp:290-296; Requests.cpp:2259).
    pub fn get_current_state(
        &self,
        env: &impl WebBrowserManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if !env.is_authorized() || env.is_bot() {
            return;
        }
        updates.push(td_api::Update::updateWebBrowserSettings(
            td_api::updateWebBrowserSettings {
                settings: Some(self.settings.get_web_browser_settings_object()),
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tdutils::promise::PromiseInterface;

    struct TestEnv {
        is_authorized: bool,
        is_bot: bool,
        pmc: BTreeMap<Vec<u8>, Vec<u8>>,
        updates: Vec<td_api::Update>,
        query_log: Vec<String>,
    }

    impl TestEnv {
        fn new(is_authorized: bool, is_bot: bool) -> TestEnv {
            TestEnv {
                is_authorized,
                is_bot,
                pmc: BTreeMap::new(),
                updates: Vec::new(),
                query_log: Vec::new(),
            }
        }
    }

    impl WebBrowserManagerEnv for TestEnv {
        fn is_authorized(&self) -> bool {
            self.is_authorized
        }

        fn is_bot(&self) -> bool {
            self.is_bot
        }

        fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.pmc.get(key).cloned().unwrap_or_default()
        }

        fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.pmc.insert(key.to_vec(), value.to_vec());
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn send_get_web_browser_settings_query(&mut self, hash: i64) {
            self.query_log.push(format!("get {hash}"));
        }

        fn send_update_web_browser_settings_query(
            &mut self,
            open_external_browser: bool,
            display_close_button: bool,
        ) {
            self.query_log.push(format!(
                "update {open_external_browser} {display_close_button}"
            ));
        }

        fn send_toggle_web_browser_settings_exception_query(
            &mut self,
            is_deletion: bool,
            open_external_browser: bool,
            url: &str,
        ) {
            self.query_log.push(format!(
                "toggle {is_deletion} {open_external_browser} {url}"
            ));
        }

        fn send_delete_web_browser_settings_exceptions_query(&mut self) {
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

    fn settings_with_hash(hash: i64) -> telegram_api::account_webBrowserSettings {
        telegram_api::account_webBrowserSettings {
            flags: 0,
            open_external_browser: true,
            display_close_button: false,
            external_exceptions: Vec::new(),
            inapp_exceptions: Vec::new(),
            hash,
        }
    }

    fn manager_with_settings() -> (WebBrowserManager, WebBrowserSettings) {
        let settings = WebBrowserSettings::from_telegram(settings_with_hash(42));
        let manager = WebBrowserManager {
            settings: settings.clone(),
            net_blocked_promises: Vec::new(),
        };
        (manager, settings)
    }

    #[test]
    fn load_skips_when_unauthorized_or_bot() {
        for env_flags in [(false, false), (true, true)] {
            let mut env = TestEnv::new(env_flags.0, env_flags.1);
            let mut manager = WebBrowserManager::new();
            manager.load_web_browser_settings(&mut env);
            assert!(env.updates.is_empty());
            assert!(env.query_log.is_empty());
        }
    }

    #[test]
    fn load_restores_the_saved_settings_and_reloads_on_empty_hash() {
        let mut env = TestEnv::new(true, false);
        let (manager, settings) = manager_with_settings();
        manager.save_web_browser_settings(&mut env);

        let mut fresh_env = TestEnv::new(true, false);
        fresh_env.pmc = env.pmc.clone();
        let mut fresh = WebBrowserManager::new();
        fresh.load_web_browser_settings(&mut fresh_env);
        assert_eq!(fresh.settings, settings);
        assert_eq!(fresh_env.updates.len(), 1);
        // hash != 0 → no reload query
        assert!(fresh_env.query_log.is_empty());
    }

    #[test]
    fn load_sends_update_and_reloads_without_saved_settings() {
        let mut env = TestEnv::new(true, false);
        let mut manager = WebBrowserManager::new();
        manager.load_web_browser_settings(&mut env);
        assert_eq!(env.updates.len(), 1);
        assert_eq!(env.query_log, vec!["get 0".to_string()]);
    }

    #[test]
    fn load_resets_to_defaults_on_a_corrupt_blob() {
        let mut env = TestEnv::new(true, false);
        env.pmc
            .insert(b"web_browser_settings".to_vec(), vec![0x01, 0x02, 0x03]);
        let mut manager = WebBrowserManager::new();
        manager.load_web_browser_settings(&mut env);
        assert_eq!(manager.settings, WebBrowserSettings::default());
        assert_eq!(env.updates.len(), 1);
        assert_eq!(env.query_log, vec!["get 0".to_string()]);
    }

    #[test]
    fn on_get_web_browser_settings_applies_saves_and_answers() {
        let mut env = TestEnv::new(true, false);
        let mut manager = WebBrowserManager::new();
        let (promise, recorded) = recording_promise();
        manager.on_get_web_browser_settings(
            &mut env,
            Ok(WebBrowserSettingsAnswer::Settings(settings_with_hash(7))),
            promise,
        );
        assert!(recorded.0.lock().unwrap().is_some());
        assert!(!env.pmc.is_empty(), "the new settings are saved");
        assert_eq!(env.updates.len(), 1);
    }

    #[test]
    fn on_get_web_browser_settings_handles_not_modified_and_unchanged() {
        let mut env = TestEnv::new(true, false);
        let (manager, _) = manager_with_settings();
        let (promise, recorded) = recording_promise();
        let mut manager = manager;
        manager.on_get_web_browser_settings(
            &mut env,
            Ok(WebBrowserSettingsAnswer::NotModified),
            promise,
        );
        assert!(recorded.0.lock().unwrap().is_some());
        assert!(env.pmc.is_empty());

        // identical settings → no save, no update
        let (promise, recorded) = recording_promise();
        manager.on_get_web_browser_settings(
            &mut env,
            Ok(WebBrowserSettingsAnswer::Settings(settings_with_hash(42))),
            promise,
        );
        assert!(recorded.0.lock().unwrap().is_some());
        assert!(env.pmc.is_empty());
        assert!(env.updates.is_empty());
    }

    #[test]
    fn on_update_web_browser_settings_saves_on_change_only() {
        let mut env = TestEnv::new(true, false);
        let mut manager = WebBrowserManager::new();
        manager.on_update_web_browser_settings(
            &mut env,
            telegram_api::updateWebBrowserSettings {
                flags: 0,
                open_external_browser: false,
                display_close_button: false,
            },
        );
        assert!(env.pmc.is_empty(), "no change, no save");
        manager.on_update_web_browser_settings(
            &mut env,
            telegram_api::updateWebBrowserSettings {
                flags: 0,
                open_external_browser: true,
                display_close_button: false,
            },
        );
        assert!(!env.pmc.is_empty());
        assert_eq!(env.updates.len(), 1);
    }

    #[test]
    fn on_update_web_browser_exception_saves_on_change() {
        let mut env = TestEnv::new(true, false);
        let mut manager = WebBrowserManager::new();
        manager.on_update_web_browser_exception(
            &mut env,
            telegram_api::updateWebBrowserException {
                flags: 0,
                exception: Some(telegram_api::WebDomainException {
                    flags: 0,
                    domain: "a.com".to_string(),
                    url: "https://a.com".to_string(),
                    title: String::new(),
                    favicon: 0,
                }),
                delete: false,
                open_external_browser: true,
            },
        );
        assert!(!env.pmc.is_empty());
        assert_eq!(env.updates.len(), 1);
    }

    #[test]
    fn setters_park_the_promise_and_send_the_query() {
        let mut env = TestEnv::new(true, false);
        let mut manager = WebBrowserManager::new();
        let (promise, _) = recording_promise();
        manager.update_web_browser_settings(&mut env, true, false, promise);
        let (promise, _) = recording_promise();
        manager.add_web_browser_settings_exception(&mut env, true, "https://a.com", promise);
        let (promise, _) = recording_promise();
        manager.remove_web_browser_settings_exception(&mut env, "https://a.com", promise);
        let (promise, _) = recording_promise();
        manager.remove_all_web_browser_settings_exceptions(&mut env, promise);
        assert_eq!(manager.net_blocked_promises.len(), 4);
        assert_eq!(env.query_log.len(), 4);
        assert!(env.query_log[0].starts_with("update"));
        assert!(env.query_log[1].starts_with("toggle false"));
        assert!(env.query_log[2].starts_with("toggle true"));
        assert_eq!(env.query_log[3], "delete all");
    }

    #[test]
    fn get_web_browser_type_classifies_urls() {
        let (manager, _) = manager_with_settings();
        // open_external_browser = true in the settings above
        let object = manager.get_web_browser_type("https://example.com").unwrap();
        assert!(matches!(
            object,
            td_api::WebBrowserType::webBrowserTypeExternal(_)
        ));

        // tonsite:// answers in-app even without a valid HTTP url
        let object = manager.get_web_browser_type("TONSITE://x").unwrap();
        assert!(matches!(
            object,
            td_api::WebBrowserType::webBrowserTypeInApp(_)
        ));

        let error = manager.get_web_browser_type("not a url");
        assert_eq!(error.unwrap_err().code(), 400);

        // the magic.org domain always opens in-app
        let mut manager = manager;
        manager.settings = WebBrowserSettings::default();
        let object = manager.get_web_browser_type("https://x.magic.org").unwrap();
        assert!(matches!(
            object,
            td_api::WebBrowserType::webBrowserTypeInApp(_)
        ));
    }

    #[test]
    fn get_current_state_guards_authorization_and_bot() {
        let (manager, _) = manager_with_settings();
        let mut updates = Vec::new();
        let env = TestEnv::new(false, false);
        manager.get_current_state(&env, &mut updates);
        assert!(updates.is_empty());

        let env = TestEnv::new(true, true);
        manager.get_current_state(&env, &mut updates);
        assert!(updates.is_empty());

        let env = TestEnv::new(true, false);
        manager.get_current_state(&env, &mut updates);
        assert_eq!(updates.len(), 1);
    }

    #[test]
    fn decode_answer_splits_not_modified_from_settings() {
        let mut bytes = Vec::new();
        telegram_api::account_webBrowserSettingsNotModified.store_boxed(&mut bytes);
        assert!(matches!(
            decode_web_browser_settings_answer(&bytes),
            Ok(WebBrowserSettingsAnswer::NotModified)
        ));

        let mut bytes = Vec::new();
        settings_with_hash(9).store_boxed(&mut bytes);
        match decode_web_browser_settings_answer(&bytes) {
            Ok(WebBrowserSettingsAnswer::Settings(settings)) => assert_eq!(settings.hash, 9),
            _ => panic!("expected the settings variant"),
        }
    }
}
