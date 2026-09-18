//! Port of `td/telegram/ConfigManager.{h,cpp}` (Wave 1, ConfigManager
//! step): the server config fetch (the post-init `help.getConfig` request,
//! its expire/reload state machine and the `process_config` option
//! application), the app config (`help.getAppConfig` → `process_app_config`
//! JSON → options, persisted in the binlog pmc), the content settings
//! (`account.getContentSettings` / `account.setContentSettings` — the
//! `can_ignore_sensitive_content_restrictions` /
//! `ignore_sensitive_content_restrictions` options) and the
//! `dc_options_update` pmc persistence.
//!
//! The C++ manager is an actor that dispatches its own `NetQuery`s and
//! reads `G()` everywhere. The Rust port is an inline manager on the Td
//! actor: the network queries are returned as [`ConfigQuery`] values for
//! the Td to hold (the client-side dispatcher lands with the net step),
//! the binlog pmc and the [`OptionManager`] are passed in by the Td at
//! each call, and the pending promises are request ids.
//!
//! Divergences (all documented inline):
//! - `ConfigRecoverer` (the DNS fallback for a missing config) lives in
//!   [`crate::config_recoverer`] as an inline state machine driven by the
//!   Td (M.3.7.1: the state + config application; the Td wiring —
//!   including the saved `dc_options_update` replay into the recoverer —
//!   lands with the recoverer wiring sub-step).
//! - The expire timer rides the Td actor's single `set_timeout` slot; the
//!   Td re-syncs it after every ConfigManager interaction (the C++ manager
//!   is its own actor with its own timer).
//! - The manager fan-outs to ConnectionCreator / LinkManager are
//!   consumed by the Td (M.3.9.1: the dc-options ctor filter + consumer
//!   call, the autologin-token stamp). The four M.3.9.2 fan-outs —
//!   TranscriptionManager / UserManager / AccountManager /
//!   GroupCallManager — are carried as [`ManagerUpdate`]s for the Td's
//!   [`crate::manager_option_states`].
//! - Failed content-settings fetches leave the pending `getOption`s
//!   unanswered, exactly like the C++ value-only promise (the errors are
//!   dropped).

use std::collections::HashMap;

use tddb::binlog_key_value::BinlogKeyValue;
use tdmtproto::telegram_api;
use tdutils::status::Status;

use crate::manager_option_states::{AgeVerificationParameters, ManagerUpdate};
use crate::option_manager::OptionManager;
use crate::td_api;

/// `ConfigManager::AppConfig::CURRENT_VERSION` (ConfigManager.h:77).
pub const APP_CONFIG_CURRENT_VERSION: i32 = 135;

/// The pending-request bookkeeping every result handler returns: the Td
/// actor completes the requests, applies the premium-options refresh and
/// holds the follow-up network queries.
#[derive(Default)]
pub struct ConfigActions {
    /// Request ids to answer with `ok`.
    pub ok_ids: Vec<u64>,
    /// Request ids to fail with the error.
    pub errors: Vec<(u64, Status)>,
    /// `getOption` requests that complete now, answered from the option
    /// store (the C++ `wrap_promise` in `OptionManager::get_option`).
    pub get_option_answers: Vec<(u64, String)>,
    /// `getApplicationConfig` requests, answered with the app config.
    pub app_config_answers: Vec<(u64, td_api::JsonValue)>,
    /// Follow-up network queries to dispatch.
    pub queries: Vec<ConfigQuery>,
    /// `ConnectionCreator::on_dc_options` (ConfigManager.cpp:1191): the
    /// config's dc options for the fan-out — `Some` exactly when
    /// `process_config` ran (an option-less config still carries
    /// `Some(vec![])`, which the C++ call persists as the empty list).
    /// The Td runs the `DcOptions(server_dc_options)` ctor filter and
    /// the consumer call.
    pub connection_creator_dc_options: Option<Vec<telegram_api::DcOption>>,
    /// `LinkManager::update_autologin_token` (ConfigManager.cpp:1196) —
    /// the token for the fan-out, `Some` when `process_config` ran.
    pub autologin_token: Option<String>,
    /// The manager fan-outs of `process_app_config` (M.3.9.2) — the C++
    /// `send_closure`s the Td applies to its
    /// [`crate::manager_option_states::ManagerOptionStates`], in the
    /// emission order (the in-loop group-call one first, then the three
    /// after the key loop).
    pub manager_updates: Vec<ManagerUpdate>,
    /// `OptionManager::update_premium_options` ran after the app config
    /// arrived (ConfigManager.cpp:1112).
    pub refresh_premium_options: bool,
}

impl ConfigActions {
    fn extend(&mut self, other: ConfigActions) {
        self.ok_ids.extend(other.ok_ids);
        self.errors.extend(other.errors);
        self.get_option_answers.extend(other.get_option_answers);
        self.app_config_answers.extend(other.app_config_answers);
        self.queries.extend(other.queries);
        self.connection_creator_dc_options = other
            .connection_creator_dc_options
            .or(self.connection_creator_dc_options.take());
        self.autologin_token = other.autologin_token.or(self.autologin_token.take());
        self.manager_updates.extend(other.manager_updates);
        self.refresh_premium_options |= other.refresh_premium_options;
    }
}

/// One of the four network queries the ConfigManager sends — the C++
/// `net_query_creator().create[_unauth](...)` calls. The Td holds the
/// returned queries until the client-side dispatcher lands (the net step).
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigQuery {
    /// `request_config_from_dc_impl` — `help.getConfig` on `dc_id`
    /// (unauth).
    GetConfig { dc_id: i32, reopen_sessions: bool },
    /// `try_request_app_config` — `help.getAppConfig` (unauth).
    GetAppConfig { hash: i32 },
    /// `get_content_settings` — `account.getContentSettings` (auth).
    GetContentSettings,
    /// `set_content_settings` — `account.setContentSettings` (auth).
    SetContentSettings { sensitive_enabled: bool },
}

impl ConfigQuery {
    /// The serialized TL body (constructor id + fields) — the bytes the
    /// dispatcher sends. Tested via the generated parse entry points.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            ConfigQuery::GetConfig { .. } => {
                telegram_api::help_getConfig.store_boxed(&mut out);
            }
            ConfigQuery::GetAppConfig { hash } => {
                telegram_api::help_getAppConfig { hash: *hash }.store_boxed(&mut out);
            }
            ConfigQuery::GetContentSettings => {
                telegram_api::account_getContentSettings.store_boxed(&mut out);
            }
            ConfigQuery::SetContentSettings { sensitive_enabled } => {
                telegram_api::account_setContentSettings {
                    flags: if *sensitive_enabled { 1 } else { 0 },
                    sensitive_enabled: *sensitive_enabled,
                }
                .store_boxed(&mut out);
            }
        }
        out
    }
}

/// `help.getAppConfig` response — either the payload or the hash-matched
/// `help.appConfigNotModified`.
#[derive(Clone, Debug, PartialEq)]
pub enum AppConfigQueryResult {
    NotModified,
    Config {
        hash: i32,
        config: Box<telegram_api::JSONValue>,
    },
}

/// The completed-query payloads the Td feeds back into the manager (the
/// C++ `ConfigManager::on_result` token dispatch).
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // the payload types are the scheme's own (`Config` is huge in the C++ too, but lives behind a tl_object_ptr)
pub enum ConfigQueryResult {
    GetConfig(Result<telegram_api::Config, Status>),
    GetAppConfig(Result<AppConfigQueryResult, Status>),
    GetContentSettings(Result<telegram_api::account_ContentSettings, Status>),
    SetContentSettings(Result<(), Status>),
}

/// `ConfigManager::AppConfig` (ConfigManager.h:76-87) — the persisted app
/// config: version + hash + the (filtered) JSON object.
#[derive(Clone, Debug)]
struct AppConfig {
    version: i32,
    hash: i32,
    config: Box<telegram_api::JSONValue>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 0,
            hash: 0,
            config: Box::new(telegram_api::JSONValue::jsonNull(telegram_api::jsonNull)),
        }
    }
}

impl AppConfig {
    /// The C++ `store`: two raw ints then the raw TL buffer of the JSON
    /// value (`fetch_string_raw` + `TlBufferParser`).
    fn store(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.version.to_le_bytes());
        out.extend_from_slice(&self.hash.to_le_bytes());
        self.config.store(&mut out);
        out
    }

    /// The C++ `parse`; an empty or truncated value fails like the C++
    /// `log_event_parse` on an empty pmc slice.
    fn parse(input: &[u8]) -> Result<Self, &'static str> {
        if input.len() < 8 {
            return Err("Invalid config");
        }
        let version = i32::from_le_bytes(input[0..4].try_into().expect("length checked"));
        if version != APP_CONFIG_CURRENT_VERSION {
            return Err("Invalid config version");
        }
        let hash = i32::from_le_bytes(input[4..8].try_into().expect("length checked"));
        let config = Box::new(telegram_api::JSONValue::parse(&input[8..])?);
        Ok(Self {
            version,
            hash,
            config,
        })
    }
}

/// `DcOption::Flags` (DcOptions.h:24) — the stored-form flags.
pub(crate) const DC_OPTION_IPV6: i32 = 1;
pub(crate) const DC_OPTION_MEDIA_ONLY: i32 = 2;
pub(crate) const DC_OPTION_OBFUSCATED_TCP_ONLY: i32 = 4;
pub(crate) const DC_OPTION_CDN: i32 = 8;
pub(crate) const DC_OPTION_STATIC: i32 = 16;
pub(crate) const DC_OPTION_HAS_SECRET: i32 = 32;

/// `DcId::MAX_RAW_DC_ID` (net/DcId.h:20).
const MAX_RAW_DC_ID: i32 = 1000;

/// The persisted `dc_options_update` entry — the C++ `DcOption::store`
/// form: flags, raw dc id (the plain id — the `Cdn` flag carries the
/// externality, `DcId::get_raw_id()`), ip string, port, secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredDcOption {
    pub(crate) flags: i32,
    pub(crate) raw_dc_id: i32,
    pub(crate) ip: Vec<u8>,
    pub(crate) port: i32,
    pub(crate) secret: Vec<u8>,
}

impl StoredDcOption {
    /// The `raw_dc_id` field (the recoverer's option narrowing).
    pub fn raw_dc_id(&self) -> i32 {
        self.raw_dc_id
    }

    /// The `ip` field as the stored address string.
    pub fn ip_address(&self) -> &str {
        std::str::from_utf8(&self.ip).unwrap_or("")
    }

    /// The `port` field.
    pub fn port(&self) -> i32 {
        self.port
    }

    /// The `secret` field (empty without the `DC_OPTION_HAS_SECRET` flag).
    pub fn secret(&self) -> &[u8] {
        &self.secret
    }

    /// `DcOption::is_ipv6` (DcOptions.h:113).
    pub(crate) fn is_ipv6(&self) -> bool {
        self.flags & DC_OPTION_IPV6 != 0
    }

    /// `DcOption::is_media_only` (DcOptions.h:117).
    pub(crate) fn is_media_only(&self) -> bool {
        self.flags & DC_OPTION_MEDIA_ONLY != 0
    }

    /// `DcOption::is_obfuscated_tcp_only` (DcOptions.h:121).
    pub(crate) fn is_obfuscated_tcp_only(&self) -> bool {
        self.flags & DC_OPTION_OBFUSCATED_TCP_ONLY != 0
    }

    /// `DcOption::is_static` (DcOptions.h:125).
    pub(crate) fn is_static(&self) -> bool {
        self.flags & DC_OPTION_STATIC != 0
    }

    /// `DcId::is_external` — the `Cdn` flag carries the externality
    /// (DcOptions.h:88).
    pub(crate) fn is_cdn(&self) -> bool {
        self.flags & DC_OPTION_CDN != 0
    }

    /// The C++ `DcOption(const telegram_api::dcOption &)` + the
    /// `is_valid` filter at the DcOptions constructor (the Td's recoverer
    /// feed routes through it too).
    pub(crate) fn from_telegram(option: &telegram_api::DcOption) -> Option<Self> {
        if !(1..=MAX_RAW_DC_ID).contains(&option.id) {
            return None;
        }
        let mut flags = 0;
        if option.ipv6 {
            flags |= DC_OPTION_IPV6;
        }
        if option.media_only {
            flags |= DC_OPTION_MEDIA_ONLY;
        }
        if option.tcpo_only {
            flags |= DC_OPTION_OBFUSCATED_TCP_ONLY;
        }
        if option.cdn {
            flags |= DC_OPTION_CDN;
        }
        if option.r#static {
            flags |= DC_OPTION_STATIC;
        }
        if !option.secret.is_empty() {
            flags |= DC_OPTION_HAS_SECRET;
        }
        // `is_valid`: a valid ip address and an exact dc id (the ip parse
        // itself lands with the net step — an empty address is dropped).
        if option.ip_address.is_empty() {
            return None;
        }
        Some(Self {
            flags,
            raw_dc_id: option.id,
            ip: option.ip_address.as_bytes().to_vec(),
            port: option.port,
            secret: option.secret.clone(),
        })
    }

    pub fn store(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.raw_dc_id.to_le_bytes());
        store_log_event_string(out, &self.ip);
        out.extend_from_slice(&self.port.to_le_bytes());
        if self.flags & DC_OPTION_HAS_SECRET != 0 {
            store_log_event_string(out, &self.secret);
        }
    }

    pub fn parse(input: &[u8], pos: &mut usize) -> Result<Self, &'static str> {
        let flags = fetch_i32(input, pos)?;
        let raw_dc_id = fetch_i32(input, pos)?;
        let ip = fetch_log_event_string(input, pos)?;
        let port = fetch_i32(input, pos)?;
        let secret = if flags & DC_OPTION_HAS_SECRET != 0 {
            fetch_log_event_string(input, pos)?
        } else {
            Vec::new()
        };
        Ok(Self {
            flags,
            raw_dc_id,
            ip,
            port,
            secret,
        })
    }
}

/// A raw little-endian int32 at `pos`.
fn fetch_i32(input: &[u8], pos: &mut usize) -> Result<i32, &'static str> {
    if *pos + 4 > input.len() {
        return Err("Invalid int");
    }
    let value = i32::from_le_bytes(input[*pos..*pos + 4].try_into().expect("length checked"));
    *pos += 4;
    Ok(value)
}

/// `td::store(Slice)` in `tl_helpers.h` — the TL string length prefix
/// rules (1 byte under 254, `254` + 3 LE bytes under 2^24, else `255` +
/// 4 LE bytes + 3 zero bytes) followed by the raw bytes.
fn store_log_event_string(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = bytes.len();
    if len < 254 {
        out.push(len as u8);
    } else if len < (1 << 24) {
        out.push(254);
        out.extend_from_slice(&(len as u32).to_le_bytes()[..3]);
    } else {
        out.push(255);
        out.extend_from_slice(&(len as u32).to_le_bytes());
        out.extend_from_slice(&[0, 0, 0]);
    }
    out.extend_from_slice(bytes);
}

/// `parser.fetch_string<Slice>()` — the inverse of [`store_log_event_string`].
fn fetch_log_event_string(input: &[u8], pos: &mut usize) -> Result<Vec<u8>, &'static str> {
    if *pos >= input.len() {
        return Err("Invalid string");
    }
    let first = input[*pos];
    *pos += 1;
    let len = match first {
        0..=253 => first as usize,
        254 => {
            if *pos + 3 > input.len() {
                return Err("Invalid string");
            }
            let mut bytes = [0u8; 4];
            bytes[..3].copy_from_slice(&input[*pos..*pos + 3]);
            *pos += 3;
            u32::from_le_bytes(bytes) as usize
        }
        255 => {
            if *pos + 7 > input.len() {
                return Err("Invalid string");
            }
            let len = u32::from_le_bytes(input[*pos..*pos + 4].try_into().expect("length checked"));
            *pos += 7;
            len as usize
        }
    };
    if *pos + len > input.len() {
        return Err("Invalid string");
    }
    let bytes = input[*pos..*pos + len].to_vec();
    *pos += len;
    Ok(bytes)
}

/// The persisted `dc_options_update` — the C++ `DcOptions::store`:
/// an int32 count followed by the stored options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoredDcOptions {
    pub(crate) options: Vec<StoredDcOption>,
}

impl StoredDcOptions {
    /// The stored options (the recoverer's replay reads them).
    pub fn options(&self) -> &[StoredDcOption] {
        &self.options
    }

    pub fn store(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.options.len() as i32).to_le_bytes());
        for option in &self.options {
            option.store(&mut out);
        }
        out
    }

    pub fn parse(input: &[u8]) -> Result<Self, &'static str> {
        if input.len() < 4 {
            return Err("Invalid dc options");
        }
        let mut pos = 0;
        let count = i32::from_le_bytes(input[0..4].try_into().expect("length checked")) as usize;
        pos += 4;
        let mut options = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            options.push(StoredDcOption::parse(input, &mut pos)?);
        }
        Ok(Self { options })
    }
}

/// `td/utils/FloodControlStrict.h` — the strict multi-limit flood control
/// behind `lazy_request_config`.
#[derive(Debug, Default)]
struct FloodControlStrict {
    events: Vec<f64>,
    limits: Vec<FloodLimit>,
    wakeup_at: f64,
    without_update: usize,
}

#[derive(Clone, Copy, Debug)]
struct FloodLimit {
    duration: i32,
    count: usize,
    pos: usize,
}

impl FloodControlStrict {
    fn add_limit(&mut self, duration: i32, count: usize) {
        self.limits.push(FloodLimit {
            duration,
            count,
            pos: 0,
        });
        self.without_update = 0;
    }

    fn add_event(&mut self, now: f64) {
        self.events.push(now);
        if self.without_update > 0 {
            self.without_update -= 1;
        } else {
            self.update(now);
        }
    }

    fn get_wakeup_at(&self) -> f64 {
        self.wakeup_at
    }

    fn update(&mut self, now: f64) {
        let mut min_pos = self.events.len();
        self.without_update = usize::MAX;
        for limit in &mut self.limits {
            if limit.count < self.events.len() - limit.pos {
                limit.pos = self.events.len() - limit.count;
            }
            let end_time = now - f64::from(limit.duration);
            while limit.pos < self.events.len() && self.events[limit.pos] < end_time {
                limit.pos += 1;
            }
            if limit.count + limit.pos <= self.events.len() {
                assert_eq!(limit.count + limit.pos, self.events.len());
                self.wakeup_at = self
                    .wakeup_at
                    .max(self.events[limit.pos] + f64::from(limit.duration));
                self.without_update = 0;
            } else {
                self.without_update = self
                    .without_update
                    .min(limit.count + limit.pos - self.events.len() - 1);
            }
            min_pos = min_pos.min(limit.pos);
        }
        if min_pos * 2 > self.events.len() {
            for limit in &mut self.limits {
                limit.pos -= min_pos;
            }
            self.events.drain(..min_pos);
        }
    }
}

/// The full ConfigManager: the C++ fields minus the actor plumbing.
#[derive(Debug)]
pub struct ConfigManager {
    /// `ConfigManager::app_config_` — loaded from the binlog pmc.
    app_config: AppConfig,
    /// `ConfigManager::config_sent_cnt_`.
    config_sent_cnt: i32,
    /// `ConfigManager::reopen_sessions_after_get_config_`.
    reopen_sessions_after_get_config: bool,
    /// `ConfigManager::expire_time_` on the `tdutils::time::now()` basis;
    /// 0 = unset (the Td re-syncs its actor timer from this).
    expire_time: f64,
    /// `ConfigManager::lazy_request_flood_control_` — `add_limit(20, 1)`.
    lazy_request_flood_control: FloodControlStrict,
    /// `ConfigManager::reload_config_queries_` — `Some(id)` carries the
    /// request id; `None` is a unit promise (`Auto()`).
    reload_config_queries: Vec<Option<u64>>,
    /// `ConfigManager::get_app_config_queries_`.
    get_app_config_queries: Vec<u64>,
    /// `ConfigManager::reload_app_config_queries_`.
    reload_app_config_queries: Vec<Option<u64>>,
    /// `ConfigManager::get_content_settings_queries_` — `Some((id, name))`
    /// is a `getOption` getter answered from the store afterwards; `None`
    /// is the internal `Auto()` refresh.
    get_content_settings_queries: Vec<Option<(u64, String)>>,
    /// `ConfigManager::set_content_settings_queries_` — indexed by the
    /// requested value, `Some(id)` for a pending `setOption`.
    set_content_settings_queries: [Vec<Option<u64>>; 2],
    /// `ConfigManager::is_set_content_settings_request_sent_`.
    is_set_content_settings_request_sent: bool,
    /// `ConfigManager::last_set_content_settings_`.
    last_set_content_settings: bool,
}

/// `G()->close_status()`.
fn close_status() -> Status {
    Status::error(500, "Request aborted")
}

impl ConfigManager {
    /// The C++ constructor: load the app config from the binlog pmc;
    /// a missing/corrupt value resets it.
    pub fn new(app_config_pmc: &[u8]) -> Self {
        let app_config = AppConfig::parse(app_config_pmc).unwrap_or_default();
        let mut lazy_request_flood_control = FloodControlStrict::default();
        lazy_request_flood_control.add_limit(20, 1);
        Self {
            app_config,
            config_sent_cnt: 0,
            reopen_sessions_after_get_config: false,
            expire_time: 0.0,
            lazy_request_flood_control,
            reload_config_queries: Vec::new(),
            get_app_config_queries: Vec::new(),
            reload_app_config_queries: Vec::new(),
            get_content_settings_queries: Vec::new(),
            set_content_settings_queries: [Vec::new(), Vec::new()],
            is_set_content_settings_request_sent: false,
            last_set_content_settings: false,
        }
    }

    /// The C++ `start_up` (ConfigManager.cpp:866-879) — the post-init
    /// config fetch: a fresh start (or a non-bot restart) requests the
    /// config; otherwise the persisted expire time arms the timer. The
    /// `dc_options_update` replay is skipped until the ConfigRecoverer
    /// lands (divergence).
    pub fn start_up(&mut self, pmc: &BinlogKeyValue, is_bot: bool) -> ConfigActions {
        let expire_time = load_config_expire_time(pmc);
        let reload_config_on_restart = !is_bot;
        if expire_time <= tdutils::time::now() || reload_config_on_restart {
            self.request_config(false, false)
        } else {
            self.expire_time = expire_time;
            ConfigActions::default()
        }
    }

    /// `ConfigManager::request_config` (cpp:911-921).
    pub fn request_config(&mut self, reopen_sessions: bool, close_flag: bool) -> ConfigActions {
        let mut actions = ConfigActions::default();
        if close_flag {
            return actions;
        }
        if self.config_sent_cnt != 0 && !reopen_sessions {
            return actions;
        }
        self.lazy_request_flood_control
            .add_event(tdutils::time::now());
        actions
            .queries
            .push(self.request_config_from_dc_impl(1 /* DcId::main */, reopen_sessions));
        actions
    }

    /// `ConfigManager::lazy_request_config` (cpp:924-935) — arm the expire
    /// timer at the flood-control wakeup time. The Td re-syncs its timer
    /// after this call.
    pub fn lazy_request_config(&mut self, close_flag: bool) {
        if close_flag {
            return;
        }
        if self.config_sent_cnt != 0 {
            return;
        }
        let wakeup_at = self.lazy_request_flood_control.get_wakeup_at();
        tdutils::time::relax_timeout_at(&mut self.expire_time, wakeup_at);
    }

    /// `ConfigManager::reload_config` (cpp:937-945) — coalesced into one
    /// `help.getConfig`; `Some(id)` answers the request on completion.
    pub fn reload_config(&mut self, id: Option<u64>, close_flag: bool) -> ConfigActions {
        let mut actions = ConfigActions::default();
        if close_flag {
            if let Some(id) = id {
                actions.errors.push((id, close_status()));
            }
            return actions;
        }
        self.reload_config_queries.push(id);
        if self.reload_config_queries.len() != 1 {
            return actions;
        }
        actions
            .queries
            .push(self.request_config_from_dc_impl(1 /* DcId::main */, false));
        actions
    }

    /// `ConfigManager::get_app_config` (cpp:958-968) — bots answer
    /// `jsonValueNull` immediately.
    pub fn get_app_config(&mut self, id: u64, is_bot: bool, close_flag: bool) -> ConfigActions {
        let mut actions = ConfigActions::default();
        if close_flag {
            actions.errors.push((id, close_status()));
            return actions;
        }
        if is_bot {
            actions
                .app_config_answers
                .push((id, td_api::JsonValue::jsonValueNull(td_api::jsonValueNull)));
            return actions;
        }
        self.get_app_config_queries.push(id);
        if let Some(query) = self.try_request_app_config() {
            actions.queries.push(query);
        }
        actions
    }

    /// `ConfigManager::reload_app_config` (cpp:970-980).
    pub fn reload_app_config(
        &mut self,
        id: Option<u64>,
        is_bot: bool,
        close_flag: bool,
    ) -> ConfigActions {
        let mut actions = ConfigActions::default();
        if close_flag {
            if let Some(id) = id {
                actions.errors.push((id, close_status()));
            }
            return actions;
        }
        if is_bot {
            if let Some(id) = id {
                actions.ok_ids.push(id);
            }
            return actions;
        }
        self.reload_app_config_queries.push(id);
        if let Some(query) = self.try_request_app_config() {
            actions.queries.push(query);
        }
        actions
    }

    /// `ConfigManager::get_content_settings` (cpp:982-995). Unauthorized
    /// clients and bots answer immediately — the getters then read the
    /// store (the C++ `set_value(Unit())` into the `wrap_promise`).
    pub fn get_content_settings(
        &mut self,
        getter: Option<(u64, String)>,
        is_authorized: bool,
        is_bot: bool,
        close_flag: bool,
    ) -> ConfigActions {
        let mut actions = ConfigActions::default();
        if close_flag {
            // The C++ fails the value-only promise silently; the request
            // itself is aborted by `Td::clear_requests` during the close.
            return actions;
        }
        if !is_authorized || is_bot {
            if let Some(getter) = getter {
                actions.get_option_answers.push(getter);
            }
            return actions;
        }
        self.get_content_settings_queries.push(getter);
        if self.get_content_settings_queries.len() == 1 {
            actions.queries.push(ConfigQuery::GetContentSettings);
        }
        actions
    }

    /// `ConfigManager::set_content_settings` (cpp:997-1010).
    pub fn set_content_settings(
        &mut self,
        ignore_sensitive_content_restrictions: bool,
        id: Option<u64>,
        close_flag: bool,
    ) -> ConfigActions {
        let mut actions = ConfigActions::default();
        if close_flag {
            if let Some(id) = id {
                actions.errors.push((id, close_status()));
            }
            return actions;
        }
        self.last_set_content_settings = ignore_sensitive_content_restrictions;
        self.set_content_settings_queries[ignore_sensitive_content_restrictions as usize].push(id);
        if !self.is_set_content_settings_request_sent {
            self.is_set_content_settings_request_sent = true;
            actions.queries.push(ConfigQuery::SetContentSettings {
                sensitive_enabled: ignore_sensitive_content_restrictions,
            });
        }
        actions
    }

    /// `ConfigManager::on_dc_options_update` (cpp:1012-1020) — persist the
    /// pending update and expire the config immediately. The Td re-syncs
    /// its timer after this call.
    pub fn on_dc_options_update(
        &mut self,
        pmc: &mut BinlogKeyValue,
        dc_options: &[telegram_api::DcOption],
    ) {
        self.on_dc_options_update_impl(pmc, dc_options);
        if !dc_options.is_empty() {
            self.expire_time = tdutils::time::now();
            save_config_expire(pmc, self.expire_time);
        }
    }

    fn on_dc_options_update_impl(
        &mut self,
        pmc: &mut BinlogKeyValue,
        dc_options: &[telegram_api::DcOption],
    ) {
        let stored: Vec<StoredDcOption> = dc_options
            .iter()
            .filter_map(StoredDcOption::from_telegram)
            .collect();
        if stored.is_empty() {
            pmc.erase("dc_options_update");
            return;
        }
        pmc.set_raw(
            b"dc_options_update",
            &StoredDcOptions { options: stored }.store(),
        );
    }

    /// `ConfigManager::request_config_from_dc_impl` (cpp:1022-1028).
    fn request_config_from_dc_impl(&mut self, dc_id: i32, reopen_sessions: bool) -> ConfigQuery {
        self.config_sent_cnt += 1;
        self.reopen_sessions_after_get_config |= reopen_sessions;
        ConfigQuery::GetConfig {
            dc_id,
            reopen_sessions,
        }
    }

    /// `ConfigManager::try_request_app_config` (cpp:948-956) — dispatch
    /// only when exactly one request is pending.
    fn try_request_app_config(&mut self) -> Option<ConfigQuery> {
        if self.get_app_config_queries.len() + self.reload_app_config_queries.len() != 1 {
            return None;
        }
        Some(ConfigQuery::GetAppConfig {
            hash: self.app_config.hash,
        })
    }

    /// `ConfigManager::do_set_ignore_sensitive_content_restrictions`
    /// (cpp:1031-1038).
    fn do_set_ignore_sensitive_content_restrictions(
        &mut self,
        ignore_sensitive_content_restrictions: bool,
        options: &mut OptionManager,
        is_bot: bool,
        close_flag: bool,
    ) -> ConfigActions {
        let mut actions = ConfigActions::default();
        if options.have_option("ignore_sensitive_content_restrictions")
            && options.get_option_boolean("ignore_sensitive_content_restrictions", false)
                == ignore_sensitive_content_restrictions
        {
            return actions;
        }
        options.set_option_boolean(
            "ignore_sensitive_content_restrictions",
            ignore_sensitive_content_restrictions,
        );
        actions.extend(self.reload_app_config(None, is_bot, close_flag));
        actions
    }

    /// The C++ `loop()` (cpp:898-903): the expire timer fired — request
    /// the config again when the time has come. The Td calls this from
    /// `timeout_expired`.
    pub fn on_timeout(&mut self, close_flag: bool) -> ConfigActions {
        if self.expire_time != 0.0 && self.expire_time <= tdutils::time::now() {
            self.expire_time = 0.0;
            return self.request_config(self.reopen_sessions_after_get_config, close_flag);
        }
        ConfigActions::default()
    }

    /// The absolute time of the next config request (0 = no timer); the Td
    /// actor mirrors it into its own timeout slot.
    pub fn expire_time(&self) -> f64 {
        self.expire_time
    }

    /// `config_sent_cnt_` — the in-flight `help.getConfig` count; the
    /// M.3.8.2 routing tests assert the completion decrement through it.
    pub fn config_sent_cnt(&self) -> i32 {
        self.config_sent_cnt
    }

    /// The C++ `on_result` tokens 8/9 — `help.getConfig`.
    #[allow(clippy::too_many_arguments)] // the C++ handler has the same shape
    pub fn on_get_config_result(
        &mut self,
        pmc: &mut BinlogKeyValue,
        r_config: Result<telegram_api::Config, Status>,
        reopen_sessions: bool,
        close_flag: bool,
        options: &mut OptionManager,
        is_test_dc: bool,
        is_authorized: bool,
        is_bot: bool,
        main_dc_id: i32,
    ) -> ConfigActions {
        let mut actions = ConfigActions::default();
        debug_assert!(self.config_sent_cnt > 0);
        self.config_sent_cnt -= 1;
        match r_config {
            Err(error) => {
                if !close_flag {
                    tracing::warn!("Failed to get config: {error}");
                    self.expire_time = tdutils::time::now() + 60.0; // try again in a minute
                }
                for id in std::mem::take(&mut self.reload_config_queries)
                    .into_iter()
                    .flatten()
                {
                    actions.errors.push((id, error.clone()));
                }
            }
            Ok(config) => {
                self.on_dc_options_update_impl(pmc, &[]);
                actions.extend(self.process_config(
                    pmc,
                    &config,
                    close_flag,
                    options,
                    is_test_dc,
                    is_authorized,
                    is_bot,
                    main_dc_id,
                ));
                if reopen_sessions {
                    // G()->net_query_dispatcher().update_mtproto_header() —
                    // the dispatcher lands with the net step.
                    self.reopen_sessions_after_get_config = false;
                }
                for id in std::mem::take(&mut self.reload_config_queries)
                    .into_iter()
                    .flatten()
                {
                    actions.ok_ids.push(id);
                }
            }
        }
        actions
    }

    /// `ConfigManager::process_config` (cpp:1173-1319) — apply the server
    /// config to the options. Returns the follow-up actions (the app-config
    /// reload and the content-settings refresh).
    #[allow(clippy::too_many_arguments)] // the C++ function has the same shape
    fn process_config(
        &mut self,
        pmc: &mut BinlogKeyValue,
        config: &telegram_api::Config,
        close_flag: bool,
        options: &mut OptionManager,
        is_test_dc: bool,
        is_authorized: bool,
        is_bot: bool,
        main_dc_id: i32,
    ) -> ConfigActions {
        let is_from_main_dc = main_dc_id == config.this_dc;
        tracing::info!("{config:?}");
        let reload_in = (config.expires - config.date).clamp(60, 86400);
        save_config_expire(pmc, tdutils::time::now() + f64::from(reload_in));
        let reload_in = reload_in - tdutils::random::fast(0, reload_in / 5);
        let reload_in = if is_from_main_dc { reload_in } else { 0 };
        self.expire_time = tdutils::time::now() + f64::from(reload_in);
        if config.test_mode != is_test_dc {
            tracing::error!("Wrong parameter is_test");
        }

        // Do not save dc_options in the config pmc, because it will be
        // interpreted and saved by ConnectionCreator
        // (ConfigManager.cpp:1189-1191): the DcOptions ctor filter and the
        // ConnectionCreator::on_dc_options call run at the Td when the
        // actions apply. LinkManager::update_autologin_token
        // (ConfigManager.cpp:1196) rides the same actions — both fan-outs
        // fire exactly when `process_config` ran, so both initialize here.
        let mut actions = ConfigActions {
            connection_creator_dc_options: Some(config.dc_options.clone()),
            autologin_token: Some(config.autologin_token.clone()),
            ..ConfigActions::default()
        };

        options.set_option_integer(
            "recent_stickers_limit",
            i64::from(config.stickers_recent_limit),
        );
        options.set_option_integer(
            "channels_read_media_period",
            i64::from(config.channels_read_media_period),
        );

        options.set_option_boolean("test_mode", config.test_mode);
        options.set_option_integer(
            "forwarded_message_count_max",
            i64::from(config.forwarded_count_max),
        );
        options.set_option_integer("basic_group_size_max", i64::from(config.chat_size_max));
        options.set_option_integer("supergroup_size_max", i64::from(config.megagroup_size_max));
        if is_from_main_dc || !options.have_option("expect_blocking") {
            options.set_option_boolean("expect_blocking", config.blocked_mode);
        }
        if is_from_main_dc || !options.have_option("dc_txt_domain_name") {
            options.set_option_string("dc_txt_domain_name", &config.dc_txt_domain_name);
        }
        if is_from_main_dc || !options.have_option("t_me_url") {
            let mut url = config.me_url_prefix.clone();
            if !url.is_empty() {
                if !url.ends_with('/') {
                    url.push('/');
                }
                options.set_option_string("t_me_url", &url);
            }
        }
        if is_from_main_dc {
            options.set_option_integer("webfile_dc_id", i64::from(config.webfile_dc_id));
            if config.tmp_sessions > 1 {
                options.set_option_integer("session_count", i64::from(config.tmp_sessions));
            } else {
                options.set_option_empty("session_count");
            }
            if !config.suggested_lang_code.is_empty()
                || config.lang_pack_version > 0
                || config.base_lang_pack_version > 0
            {
                options
                    .set_option_string("suggested_language_pack_id", &config.suggested_lang_code);
                options.set_option_integer(
                    "language_pack_version",
                    i64::from(config.lang_pack_version),
                );
                options.set_option_integer(
                    "base_language_pack_version",
                    i64::from(config.base_lang_pack_version),
                );
            } else {
                options.set_option_empty("suggested_language_pack_id");
                options.set_option_empty("language_pack_version");
                options.set_option_empty("base_language_pack_version");
            }
        }

        if is_from_main_dc {
            options.set_option_integer("edit_time_limit", i64::from(config.edit_time_limit));
            options.set_option_boolean("revoke_pm_inbox", config.revoke_pm_inbox);
            options.set_option_integer("revoke_time_limit", i64::from(config.revoke_time_limit));
            options.set_option_integer(
                "revoke_pm_time_limit",
                i64::from(config.revoke_pm_time_limit),
            );
            options.set_option_integer("rating_e_decay", i64::from(config.rating_e_decay));
        }
        options.set_option_integer(
            "call_ring_timeout_ms",
            i64::from(config.call_ring_timeout_ms),
        );
        options.set_option_integer(
            "call_connect_timeout_ms",
            i64::from(config.call_connect_timeout_ms),
        );
        options.set_option_integer(
            "call_packet_timeout_ms",
            i64::from(config.call_packet_timeout_ms),
        );
        options.set_option_integer(
            "call_receive_timeout_ms",
            i64::from(config.call_receive_timeout_ms),
        );

        options.set_option_integer(
            "message_text_length_max",
            i64::from(config.message_length_max.clamp(4096, 1_000_000)),
        );
        options.set_option_integer(
            "message_caption_length_max",
            i64::from(config.caption_length_max.clamp(1024, 1_000_000)),
        );

        if config.gif_search_username.is_empty() {
            options.set_option_empty("animation_search_bot_username");
        } else {
            options.set_option_string("animation_search_bot_username", &config.gif_search_username);
        }
        if !options.have_option("venue_search_bot_username") {
            if config.venue_search_username.is_empty() {
                options.set_option_empty("venue_search_bot_username");
            } else {
                options
                    .set_option_string("venue_search_bot_username", &config.venue_search_username);
            }
        }
        if config.img_search_username.is_empty() {
            options.set_option_empty("photo_search_bot_username");
        } else {
            options.set_option_string("photo_search_bot_username", &config.img_search_username);
        }

        let fix_timeout_ms = |timeout_ms: i32| timeout_ms.clamp(1000, 86400 * 1000);
        options.set_option_integer(
            "online_update_period_ms",
            i64::from(fix_timeout_ms(config.online_update_period_ms)),
        );
        options.set_option_integer(
            "online_cloud_timeout_ms",
            i64::from(fix_timeout_ms(config.online_cloud_timeout_ms)),
        );
        options.set_option_integer(
            "notification_cloud_delay_ms",
            i64::from(fix_timeout_ms(config.notify_cloud_delay_ms)),
        );
        options.set_option_integer(
            "notification_default_delay_ms",
            i64::from(fix_timeout_ms(config.notify_default_delay_ms)),
        );

        if is_from_main_dc && !options.have_option("default_reaction_need_sync") {
            let reaction_string = config
                .reactions_default
                .as_ref()
                .map(reaction_type_string)
                .unwrap_or_default();
            if !reaction_string.is_empty() && reaction_string != "$" {
                options.set_option_string("default_reaction", &reaction_string);
            }
        }

        // delete outdated options
        for name in [
            "suggested_language_code",
            "chat_big_size",
            "group_size_max",
            "saved_gifs_limit",
            "sessions_count",
            "forwarded_messages_count_max",
            "broadcast_size_max",
            "group_chat_size_max",
            "chat_size_max",
            "megagroup_size_max",
            "offline_blur_timeout_ms",
            "offline_idle_timeout_ms",
            "notify_cloud_delay_ms",
            "notify_default_delay_ms",
            "large_chat_size",
            "calls_enabled",
        ] {
            options.set_option_empty(name);
        }

        // TODO implement online status updates — the commented-out
        // offline_blur_timeout_ms / offline_idle_timeout_ms / push_chat_*
        // assignments are skipped like the C++.

        if is_from_main_dc {
            actions.extend(self.reload_app_config(None, is_bot, close_flag));
            if !options.have_option("can_ignore_sensitive_content_restrictions")
                || !options.have_option("ignore_sensitive_content_restrictions")
            {
                actions.extend(self.get_content_settings(None, is_authorized, is_bot, close_flag));
            }
        }
        actions
    }

    /// The C++ `on_result` token 1 — `help.getAppConfig`.
    pub fn on_get_app_config_result(
        &mut self,
        pmc: &mut BinlogKeyValue,
        result: Result<AppConfigQueryResult, Status>,
        close_flag: bool,
        options: &mut OptionManager,
        is_authorized: bool,
        is_bot: bool,
    ) -> ConfigActions {
        let mut actions = ConfigActions::default();
        let get_ids = std::mem::take(&mut self.get_app_config_queries);
        let reload_ids = std::mem::take(&mut self.reload_app_config_queries);
        match result {
            Err(error) => {
                for id in get_ids {
                    actions.errors.push((id, error.clone()));
                }
                for id in reload_ids.into_iter().flatten() {
                    actions.errors.push((id, error.clone()));
                }
                // The unit (None) reload promises fail silently.
            }
            Ok(AppConfigQueryResult::NotModified) => {
                if self.app_config.version == 0 {
                    tracing::error!("Receive appConfigNotModified");
                    let error = Status::error(500, "Receive unexpected response");
                    for id in get_ids {
                        actions.errors.push((id, error.clone()));
                    }
                    for id in reload_ids.into_iter().flatten() {
                        actions.errors.push((id, error.clone()));
                    }
                } else {
                    for id in get_ids {
                        actions
                            .app_config_answers
                            .push((id, convert_json_value_object(&self.app_config.config)));
                    }
                    for id in reload_ids.into_iter().flatten() {
                        actions.ok_ids.push(id);
                    }
                }
            }
            Ok(AppConfigQueryResult::Config { hash, config }) => {
                let (filtered_config, mut process_actions) =
                    self.process_app_config(config, options, is_authorized, is_bot, close_flag);
                actions.extend(std::mem::take(&mut process_actions));
                self.app_config.version = APP_CONFIG_CURRENT_VERSION;
                self.app_config.hash = hash;
                self.app_config.config = filtered_config;
                pmc.set_raw(b"app_config", &self.app_config.store());
                // G()->get_option_manager()->update_premium_options() runs
                // before the promises complete (cpp:1112).
                actions.refresh_premium_options = true;
                for id in get_ids {
                    actions
                        .app_config_answers
                        .push((id, convert_json_value_object(&self.app_config.config)));
                }
                for id in reload_ids.into_iter().flatten() {
                    actions.ok_ids.push(id);
                }
            }
        }
        actions
    }

    /// The C++ `on_result` token 2 — `account.getContentSettings`.
    pub fn on_get_content_settings_result(
        &mut self,
        result: Result<telegram_api::account_ContentSettings, Status>,
        options: &mut OptionManager,
        is_bot: bool,
        close_flag: bool,
    ) -> ConfigActions {
        let mut actions = ConfigActions::default();
        match result {
            Err(error) => {
                // The C++ fails the value-only getter promises silently —
                // the pending getOption requests stay unanswered.
                tracing::warn!("Failed to get content settings: {error}");
                self.get_content_settings_queries.clear();
            }
            Ok(content_settings) => {
                let mut do_set_actions = self.do_set_ignore_sensitive_content_restrictions(
                    content_settings.sensitive_enabled,
                    options,
                    is_bot,
                    close_flag,
                );
                actions.extend(std::mem::take(&mut do_set_actions));
                options.set_option_boolean(
                    "can_ignore_sensitive_content_restrictions",
                    content_settings.sensitive_can_change,
                );
                for getter in std::mem::take(&mut self.get_content_settings_queries)
                    .into_iter()
                    .flatten()
                {
                    actions.get_option_answers.push(getter);
                }
            }
        }
        actions
    }

    /// The C++ `on_result` tokens 3/4 — `account.setContentSettings`,
    /// including the two-queue ping-pong.
    pub fn on_set_content_settings_result(
        &mut self,
        result: Result<(), Status>,
        value: bool,
        options: &mut OptionManager,
        is_bot: bool,
        close_flag: bool,
    ) -> ConfigActions {
        self.is_set_content_settings_request_sent = false;
        let mut actions = ConfigActions::default();
        match result {
            Err(error) => {
                for id in std::mem::take(&mut self.set_content_settings_queries[value as usize])
                    .into_iter()
                    .flatten()
                {
                    actions.errors.push((id, error.clone()));
                }
            }
            Ok(()) => {
                if options.get_option_boolean("can_ignore_sensitive_content_restrictions", false)
                    && self.last_set_content_settings == value
                {
                    actions.extend(self.do_set_ignore_sensitive_content_restrictions(
                        value, options, is_bot, close_flag,
                    ));
                }
                for id in std::mem::take(&mut self.set_content_settings_queries[value as usize])
                    .into_iter()
                    .flatten()
                {
                    actions.ok_ids.push(id);
                }
            }
        }
        if !self.set_content_settings_queries[!value as usize].is_empty() {
            if value == self.last_set_content_settings {
                // The queued requests already have their effective value.
                for id in std::mem::take(&mut self.set_content_settings_queries[!value as usize])
                    .into_iter()
                    .flatten()
                {
                    actions.ok_ids.push(id);
                }
            } else {
                actions.extend(self.set_content_settings(!value, None, close_flag));
            }
        }
        actions
    }
}

/// `ConfigManager::load_config_expire_time` (cpp:1159-1167) — the stored
/// wall-clock expiry, clamped to [now, now + 1 hour], mapped onto the
/// `time::now()` basis.
fn load_config_expire_time(pmc: &BinlogKeyValue) -> f64 {
    let stored: i32 = pmc.get("config_expire").parse().unwrap_or(0);
    let expires_in = f64::from(stored) - tdutils::time::system();
    if !(0.0..=3600.0).contains(&expires_in) {
        return tdutils::time::now();
    }
    tdutils::time::now() + expires_in
}

/// `ConfigManager::save_config_expire` (cpp:1169-1171).
fn save_config_expire(pmc: &mut BinlogKeyValue, expire_time: f64) {
    let expires_in = expire_time - tdutils::time::now();
    pmc.set(
        "config_expire",
        &((tdutils::time::system() + expires_in) as i32).to_string(),
    );
}

/// `ReactionType::get_string()` for the `default_reaction` option — the
/// C++ constructor (ReactionType.cpp:39-60): emoji reactions keep the
/// emoticon (unless it is itself custom or paid), custom reactions become
/// `custom:<document_id>`, paid becomes `$`, empty stays empty.
fn reaction_type_string(reaction: &telegram_api::Reaction) -> String {
    match reaction {
        telegram_api::Reaction::reactionEmpty(_) => String::new(),
        telegram_api::Reaction::reactionEmoji(emoji) => {
            if emoji.emoticon == "$" || emoji.emoticon.starts_with("custom:") {
                String::new()
            } else {
                emoji.emoticon.clone()
            }
        }
        telegram_api::Reaction::reactionCustomEmoji(custom) => {
            format!("custom:{}", custom.document_id)
        }
        telegram_api::Reaction::reactionPaid(_) => "$".to_string(),
    }
}

/// `convert_json_value_object` (JsonValue.cpp) — the telegram_api →
/// td_api JSON conversion for `getApplicationConfig`.
pub fn convert_json_value_object(json_value: &telegram_api::JSONValue) -> td_api::JsonValue {
    match json_value {
        telegram_api::JSONValue::jsonNull(_) => {
            td_api::JsonValue::jsonValueNull(td_api::jsonValueNull)
        }
        telegram_api::JSONValue::jsonBool(value) => {
            td_api::JsonValue::jsonValueBoolean(td_api::jsonValueBoolean { value: value.value })
        }
        telegram_api::JSONValue::jsonNumber(value) => {
            td_api::JsonValue::jsonValueNumber(td_api::jsonValueNumber { value: value.value })
        }
        telegram_api::JSONValue::jsonString(value) => {
            td_api::JsonValue::jsonValueString(td_api::jsonValueString {
                value: value.value.clone(),
            })
        }
        telegram_api::JSONValue::jsonArray(value) => {
            td_api::JsonValue::jsonValueArray(td_api::jsonValueArray {
                values: value
                    .value
                    .iter()
                    .map(|item| Box::new(convert_json_value_object(item)))
                    .collect(),
            })
        }
        telegram_api::JSONValue::jsonObject(value) => {
            td_api::JsonValue::jsonValueObject(td_api::jsonValueObject {
                members: value
                    .value
                    .iter()
                    .map(|member| {
                        Box::new(td_api::JsonObjectMember {
                            key: member.key.clone(),
                            value: member
                                .value
                                .as_deref()
                                .map(|value| Box::new(convert_json_value_object(value))),
                        })
                    })
                    .collect(),
            })
        }
    }
}

/// `get_json_value_bool` (JsonValue.cpp:199-205).
fn get_json_value_bool(json_value: &telegram_api::JSONValue, name: &str) -> bool {
    if let telegram_api::JSONValue::jsonBool(value) = json_value {
        return value.value;
    }
    tdutils::log_error!("Expected Boolean as {name}, but found {json_value:?}");
    false
}

/// `get_json_value_int` (JsonValue.cpp:207-213).
pub(crate) fn get_json_value_int(json_value: &telegram_api::JSONValue, name: &str) -> i32 {
    if let telegram_api::JSONValue::jsonNumber(value) = json_value {
        return value.value as i32;
    }
    tdutils::log_error!("Expected Integer as {name}, but found {json_value:?}");
    0
}

/// `get_json_value_long` (JsonValue.cpp:215-224).
pub(crate) fn get_json_value_long(json_value: &telegram_api::JSONValue, name: &str) -> i64 {
    match json_value {
        telegram_api::JSONValue::jsonString(value) => value.value.parse().unwrap_or(0),
        telegram_api::JSONValue::jsonNumber(value) => value.value as i64,
        _ => {
            tdutils::log_error!("Expected Long as {name}, but found {json_value:?}");
            0
        }
    }
}

/// `get_json_value_double` (JsonValue.cpp:226-232).
fn get_json_value_double(json_value: &telegram_api::JSONValue, name: &str) -> f64 {
    if let telegram_api::JSONValue::jsonNumber(value) = json_value {
        return value.value;
    }
    tdutils::log_error!("Expected Double as {name}, but found {json_value:?}");
    0.0
}

/// `get_json_value_string` (JsonValue.cpp:234-240).
pub(crate) fn get_json_value_string(json_value: &telegram_api::JSONValue, name: &str) -> String {
    if let telegram_api::JSONValue::jsonString(value) = json_value {
        return value.value.clone();
    }
    tdutils::log_error!("Expected String as {name}, but found {json_value:?}");
    String::new()
}

/// `get_json_value_bool` on an optional JSON value — a missing (null in
/// C++) value yields the default `false`.
pub(crate) fn get_json_value_bool_option(
    json_value: Option<&telegram_api::JSONValue>,
    name: &str,
) -> bool {
    json_value.is_some_and(|value| get_json_value_bool(value, name))
}

/// `get_json_value_int` on an optional JSON value — a missing (null in
/// C++) value yields the default `0`.
pub(crate) fn get_json_value_int_option(
    json_value: Option<&telegram_api::JSONValue>,
    name: &str,
) -> i32 {
    json_value.map_or(0, |value| get_json_value_int(value, name))
}

/// `get_json_value_long` on an optional JSON value — a missing (null in
/// C++) value yields the default `0`.
pub(crate) fn get_json_value_long_option(
    json_value: Option<&telegram_api::JSONValue>,
    name: &str,
) -> i64 {
    json_value.map_or(0, |value| get_json_value_long(value, name))
}

/// `get_json_value_double` on an optional JSON value — a missing (null in
/// C++) value yields the default `0.0`.
pub(crate) fn get_json_value_double_option(
    json_value: Option<&telegram_api::JSONValue>,
    name: &str,
) -> f64 {
    json_value.map_or(0.0, |value| get_json_value_double(value, name))
}

/// `get_json_value_string` on an optional JSON value — a missing (null in
/// C++) value yields the empty string.
pub(crate) fn get_json_value_string_option(
    json_value: Option<&telegram_api::JSONValue>,
    name: &str,
) -> String {
    json_value.map_or_else(String::new, |value| get_json_value_string(value, name))
}

/// `td::implode(vec, separator)`.
fn implode(values: &[String], separator: char) -> String {
    values.join(&separator.to_string())
}

/// `td/utils/base64.h` `is_base64url` — the padding rules, the character
/// table check and the trailing-bit checks.
fn is_base64url(input: &[u8]) -> bool {
    let mut input = input;
    let mut padding_length = 0usize;
    while let Some((last, rest)) = input.split_last() {
        if *last != b'=' {
            break;
        }
        input = rest;
        padding_length += 1;
    }
    if padding_length >= 3 {
        return false;
    }
    if padding_length > 0 && (input.len() + padding_length) & 3 != 0 {
        return false;
    }
    if input.len() & 3 == 1 {
        return false;
    }
    for &c in input {
        if base64url_value(c) == 64 {
            return false;
        }
    }
    if input.len() & 3 == 2 {
        let value = base64url_value(input[input.len() - 1]);
        if (value & 15) != 0 {
            return false;
        }
    }
    if input.len() & 3 == 3 {
        let value = base64url_value(input[input.len() - 1]);
        if (value & 3) != 0 {
            return false;
        }
    }
    true
}

/// The base64url character value (64 = invalid).
fn base64url_value(c: u8) -> u8 {
    match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'-' => 62,
        b'_' => 63,
        _ => 64,
    }
}

/// `clean_phone_number` (telegram/misc.cpp) — keep the digits only.
fn clean_phone_number(phone_number: &mut String) {
    phone_number.retain(|c| c.is_ascii_digit());
}

/// `td::to_integer_safe<int64>` (td/utils/misc.h:200-206) — a strict
/// canonical-decimal parse: the C++ round-trips the parsed value through its
/// minimal decimal form, rejecting leading `+`, leading zeros, `-0`,
/// embedded garbage and out-of-range values that `to_integer` would wrap.
/// The canonical implementation for every `to_integer_safe` site
/// (decision M.3.10.1).
pub(crate) fn to_integer_safe_i64(str: &str) -> Option<i64> {
    let bytes = str.as_bytes();
    let digits = match bytes.first() {
        Some(b'-') => &bytes[1..],
        Some(_) => bytes,
        None => return None,
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    // Leading zeros (any digit count > 1 starting with '0') don't round-trip.
    if digits.len() > 1 && digits[0] == b'0' {
        return None;
    }
    let magnitude: i128 = digits
        .iter()
        .fold(0i128, |acc, &d| acc * 10 + i128::from(d - b'0'));
    let value = if str.starts_with('-') {
        -magnitude
    } else {
        magnitude
    };
    // Values past i64::MAX/MIN would wrap in the C++ `to_integer` and fail
    // its round-trip check — clamping them keeps the same rejection.
    let value = value.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
    if value.to_string() == str {
        Some(value)
    } else {
        None
    }
}

/// `get_premium_limit_keys` (Premium.cpp:932-950).
fn premium_limit_keys() -> &'static [&'static str] {
    &[
        "channels",
        "saved_gifs",
        "stickers_faved",
        "dialog_filters",
        "dialog_filters_chats",
        "dialogs_pinned",
        "dialogs_folder_pinned",
        "channels_public",
        "caption_length",
        "about_length",
        "chatlist_invites",
        "chatlists_joined",
        "story_expiring",
        "story_caption_length",
        "stories_sent_weekly",
        "stories_sent_monthly",
        "stories_suggested_reactions",
        "recommended_channels",
        "saved_dialogs_pinned",
        "bots_create",
        "aicompose_tone_saved",
        "message_length",
    ]
}

impl ConfigManager {
    /// `ConfigManager::process_app_config` (cpp:1321-2157) — apply the app
    /// config JSON to the options and return the filtered config (the
    /// consumed keys are stripped before it is persisted, like the C++
    /// `new_values` reassignment).
    pub fn process_app_config(
        &mut self,
        mut config: Box<telegram_api::JSONValue>,
        options: &mut OptionManager,
        is_authorized: bool,
        is_bot: bool,
        close_flag: bool,
    ) -> (Box<telegram_api::JSONValue>, ConfigActions) {
        tracing::info!("Receive app config {config:?}");

        // The C++ jsonObject key maps (ConfigManager.cpp:1353-1513).
        const BOOL_KEYS: &[(&str, &str)] = &[
            (
                "autoarchive_setting_available",
                "can_archive_and_mute_new_chats_from_unknown_users",
            ),
            ("can_edit_factcheck", "can_edit_fact_check"),
            (
                "channel_revenue_withdrawal_enabled",
                "can_withdraw_chat_revenue",
            ),
            (
                "message_primary_edited_date",
                "show_message_edit_date_by_default",
            ),
            (
                "premium_gift_attach_menu_icon",
                "gift_premium_from_attachment_menu",
            ),
            (
                "premium_gift_text_field_icon",
                "gift_premium_from_input_field",
            ),
            ("settings_display_passkeys", "can_use_login_passkey"),
            ("stars_gifts_enabled", "can_gift_stars"),
            ("stars_paid_messages_available", "can_enable_paid_messages"),
            ("story_weather_preload", "can_preload_weather"),
            ("video_ignore_alt_documents", ""),
        ];
        const INTEGER_KEYS: &[(&str, &str)] = &[
            (
                "aicompose_tone_examples_num",
                "text_composition_style_example_count",
            ),
            (
                "aicompose_tone_prompt_length_max",
                "text_composition_style_prompt_length_max",
            ),
            (
                "aicompose_tone_title_length_max",
                "text_composition_style_title_length_max",
            ),
            ("authorization_autoconfirm_period", ""),
            ("boosts_channel_level_max", "chat_boost_level_max"),
            ("boosts_per_sent_gift", "premium_gift_boost_count"),
            ("bot_preview_medias_max", "bot_media_preview_count_max"),
            (
                "bot_verification_description_length_limit",
                "bot_verification_custom_description_length_max",
            ),
            ("business_chat_links_limit", "business_chat_link_count_max"),
            ("channel_autotranslation_level_min", ""),
            ("channel_bg_icon_level_min", ""),
            ("channel_custom_wallpaper_level_min", ""),
            ("channel_emoji_status_level_min", ""),
            ("channel_profile_bg_icon_level_min", ""),
            ("channel_restrict_sponsored_level_min", ""),
            ("channel_wallpaper_level_min", ""),
            ("chat_read_mark_expire_period", ""),
            ("chat_read_mark_size_threshold", ""),
            (
                "chatlist_update_period",
                "chat_folder_new_chats_update_period",
            ),
            ("community_bot_peers_limit", "community_bot_count_max"),
            ("community_peers_limit", "community_chat_count_max"),
            (
                "conference_call_size_limit",
                "group_call_participant_count_max",
            ),
            ("contact_note_length_limit", "user_note_text_length_max"),
            (
                "ephemeral_welcome_messages_max",
                "welcome_message_count_max",
            ),
            ("factcheck_length_limit", "fact_check_length_max"),
            (
                "giveaway_add_peers_max",
                "giveaway_additional_chat_count_max",
            ),
            (
                "giveaway_boosts_per_premium",
                "giveaway_boost_count_per_premium",
            ),
            ("giveaway_countries_max", "giveaway_country_count_max"),
            ("giveaway_period_max", "giveaway_duration_max"),
            (
                "group_call_message_length_limit",
                "group_call_message_text_length_max",
            ),
            ("group_call_message_ttl", "group_call_message_show_time_max"),
            ("group_custom_wallpaper_level_min", ""),
            ("group_emoji_status_level_min", ""),
            ("group_emoji_stickers_level_min", ""),
            ("group_profile_bg_icon_level_min", ""),
            ("group_transcribe_level_min", ""),
            ("group_wallpaper_level_min", ""),
            ("hidden_members_group_size_min", ""),
            (
                "intro_description_length_limit",
                "business_start_page_message_length_max",
            ),
            (
                "intro_title_length_limit",
                "business_start_page_title_length_max",
            ),
            ("message_typing_draft_ttl", "pending_text_message_period"),
            (
                "no_forwards_request_expire_period",
                "has_protected_content_disable_request_duration",
            ),
            ("passkeys_account_passkeys_max", "login_passkey_count_max"),
            ("pm_read_date_expire_period", ""),
            ("poll_answer_delete_period", ""),
            ("poll_answers_max", "poll_answer_count_max"),
            ("poll_close_period_max", "poll_open_period_max"),
            ("poll_countries_max", "poll_country_count_max"),
            ("quick_replies_limit", "quick_reply_shortcut_count_max"),
            (
                "quick_reply_messages_limit",
                "quick_reply_shortcut_message_count_max",
            ),
            ("quote_length_max", "message_reply_quote_length_max"),
            ("reactions_in_chat_max", "chat_available_reaction_count_max"),
            ("reactions_uniq_max", ""),
            ("reactions_user_max_default", ""),
            ("reactions_user_max_premium", ""),
            ("rich_message_length_limit", "rich_message_text_length_max"),
            ("rich_message_max_blocks", "rich_message_block_count_max"),
            ("rich_message_max_depth", "rich_message_depth_max"),
            ("rich_message_max_media", "rich_message_media_count_max"),
            (
                "rich_message_max_table_cols",
                "rich_message_table_column_count_max",
            ),
            ("ringtone_duration_max", "notification_sound_duration_max"),
            ("ringtone_saved_count_max", "notification_sound_count_max"),
            ("ringtone_size_max", "notification_sound_size_max"),
            (
                "stargifts_collection_gifts_limit",
                "gift_collection_size_max",
            ),
            ("stargifts_collections_limit", "gift_collection_count_max"),
            ("stargifts_convert_period_max", "gift_sell_period"),
            ("stargifts_message_length_max", "gift_text_length_max"),
            ("stargifts_pinned_to_top_limit", "pinned_gift_count_max"),
            (
                "starref_max_commission_permille",
                "affiliate_program_commission_per_mille_max",
            ),
            (
                "starref_min_commission_permille",
                "affiliate_program_commission_per_mille_min",
            ),
            (
                "stars_groupcall_message_amount_max",
                "paid_group_call_message_star_count_max",
            ),
            (
                "stars_paid_message_amount_max",
                "paid_message_star_count_max",
            ),
            (
                "stars_paid_message_commission_permille",
                "paid_message_earnings_per_mille",
            ),
            (
                "stars_paid_messages_channel_amount_default",
                "direct_channel_message_star_count_default",
            ),
            (
                "stars_paid_post_amount_max",
                "paid_media_message_star_count_max",
            ),
            (
                "stars_paid_reaction_amount_max",
                "paid_reaction_star_count_max",
            ),
            ("stars_revenue_withdrawal_max", "star_withdrawal_count_max"),
            ("stars_revenue_withdrawal_min", "star_withdrawal_count_min"),
            (
                "stars_stargift_resale_amount_max",
                "gift_resale_star_count_max",
            ),
            (
                "stars_stargift_resale_amount_min",
                "gift_resale_star_count_min",
            ),
            (
                "stars_stargift_resale_commission_permille",
                "gift_resale_star_earnings_per_mille",
            ),
            (
                "stars_subscription_amount_max",
                "subscription_star_count_max",
            ),
            (
                "stars_suggested_post_age_min",
                "suggested_post_lifetime_min",
            ),
            (
                "stars_suggested_post_amount_min",
                "suggested_post_star_count_min",
            ),
            (
                "stars_suggested_post_amount_max",
                "suggested_post_star_count_max",
            ),
            (
                "stars_suggested_post_commission_permille",
                "suggested_post_star_earnings_per_mille",
            ),
            (
                "stars_suggested_post_future_min",
                "suggested_post_send_delay_min",
            ),
            (
                "stars_suggested_post_future_max",
                "suggested_post_send_delay_max",
            ),
            ("stars_usd_sell_rate_x1000", "usd_to_thousand_star_rate"),
            ("stars_usd_withdraw_rate_x1000", "thousand_star_to_usd_rate"),
            ("stickers_premium_by_emoji_num", ""),
            ("stickers_normal_by_emoji_per_premium_num", ""),
            ("stories_album_stories_limit", "story_album_size_max"),
            ("stories_albums_limit", "story_album_count_max"),
            ("stories_area_url_max", "story_link_area_count_max"),
            ("stories_pinned_to_top_count_max", "pinned_story_count_max"),
            (
                "stories_stealth_cooldown_period",
                "story_stealth_mode_cooldown_period",
            ),
            (
                "stories_stealth_future_period",
                "story_stealth_mode_future_period",
            ),
            (
                "stories_stealth_past_period",
                "story_stealth_mode_past_period",
            ),
            (
                "story_viewers_expire_period",
                "story_viewers_expiration_delay",
            ),
            (
                "telegram_antispam_group_size_min",
                "aggressive_anti_spam_supergroup_member_count_min",
            ),
            ("todo_item_length_max", "checklist_task_text_length_max"),
            ("todo_items_max", "checklist_task_count_max"),
            ("todo_title_length_max", "checklist_title_length_max"),
            (
                "ton_stargift_resale_commission_permille",
                "gift_resale_gram_earnings_per_mille",
            ),
            (
                "ton_suggested_post_commission_permille",
                "suggested_post_gram_earnings_per_mille",
            ),
            ("topics_pinned_limit", "pinned_forum_topic_count_max"),
            (
                "upload_premium_speedup_download",
                "premium_download_speedup",
            ),
            ("upload_premium_speedup_notify_period", ""),
            ("upload_premium_speedup_upload", "premium_upload_speedup"),
        ];
        const LONG_KEYS: &[(&str, &str)] = &[
            ("stories_changelog_user_id", "stories_changelog_user_id"),
            ("telegram_antispam_user_id", "anti_spam_bot_user_id"),
            (
                "ton_stakedice_stake_amount_max",
                "stake_dice_stake_amount_max",
            ),
            (
                "ton_stakedice_stake_amount_min",
                "stake_dice_stake_amount_min",
            ),
        ];
        const STRING_KEYS: &[(&str, &str)] = &[
            ("gif_search_branding", "animation_search_provider"),
            ("music_search_username", "audio_search_bot_username"),
            ("phone_country_iso2", ""),
            ("premium_manage_subscription_url", ""),
            ("stories_venue_search_username", "venue_search_bot_username"),
            ("ton_blockchain_explorer_url", ""),
            ("ton_proxy_address", ""),
            ("ton_topup_url", "gram_top_up_url"),
            ("weather_search_username", "weather_bot_username"),
        ];
        const IGNORED_OPTIONS: &[&str] = &[
            "channel_color_level_min",
            "default_emoji_statuses_stickerset_id",
            "dialog_filters_enabled",
            "dialog_filters_tooltip",
            "forum_upgrade_participants_min",
            "getfile_experimental_params",
            "giveaway_gifts_purchase_available",
            "groupcall_video_participants_max",
            "ios_display_passkeys",
            "message_animated_emoji_max",
            "qr_login_camera",
            "qr_login_code",
            "stargifts_blocked",
            "starref_connect_allowed",
            "starref_program_allowed",
            "stars_purchase_blocked",
            "stars_rating_learnmore_url",
            "stickers_emoji_cache_time",
            "stories_export_nopublic_link",
            "stories_posting",
            "story_expire_period",
            "test",
            "upload_max_fileparts_default",
            "upload_max_fileparts_premium",
        ];

        let mut new_values: Vec<Box<telegram_api::JSONObjectValue>> = Vec::new();
        // The C++ locals consumed by the option block at the end.
        let mut ignored_restriction_reasons = String::new();
        let mut restriction_add_platforms = String::new();
        let mut dice_emojis: Vec<String> = Vec::new();
        let mut dice_emoji_index: HashMap<String, usize> = HashMap::new();
        let mut dice_emoji_success_value: HashMap<String, String> = HashMap::new();
        let mut emoji_sounds: Vec<String> = Vec::new();
        let mut animation_search_emojis = String::new();
        let mut animated_emoji_zoom = 0.0;
        let mut premium_features: Vec<String> = Vec::new();
        let mut premium_bot_username = String::new();
        let mut premium_invoice_slug = String::new();
        let mut is_premium_available = false;
        let mut fragment_prefixes: Vec<String> = Vec::new();
        // The M.3.9.2 fan-out locals (ConfigManager.cpp:1344-1357).
        let mut transcribe_audio_trial_weekly_number = 0;
        let mut transcribe_audio_trial_duration_max = 0;
        let mut transcribe_audio_trial_cooldown_until = 0;
        let mut freeze_since_date = 0;
        let mut freeze_until_date = 0;
        let mut freeze_appeal_url = String::new();
        let mut need_age_video_verification = false;
        let mut verify_age_bot_username = String::new();
        let mut verify_age_country = String::new();
        let mut verify_age_min = 0;
        let mut business_features: Vec<String> = Vec::new();
        let mut need_premium_for_new_chat_privacy = true;
        let mut starref_start_param_prefixes: Vec<String> = Vec::new();
        let mut can_accept_calls = true;
        let mut whitelisted_bots = String::new();
        let mut ton_stakedice_stake_suggested_amounts = String::new();
        let mut gift_craft_probabilities = String::new();
        let mut actions = ConfigActions::default();

        if let telegram_api::JSONValue::jsonObject(object) = config.as_ref() {
            for key_value in &object.value {
                let key = key_value.key.as_str();

                if let Some((_, option_name)) =
                    BOOL_KEYS.iter().find(|(json_key, _)| *json_key == key)
                {
                    let option_name = if option_name.is_empty() {
                        key
                    } else {
                        *option_name
                    };
                    options.set_option_boolean(
                        option_name,
                        get_json_value_bool_option(key_value.value.as_deref(), key),
                    );
                    continue;
                }
                if let Some((_, option_name)) =
                    INTEGER_KEYS.iter().find(|(json_key, _)| *json_key == key)
                {
                    let option_name = if option_name.is_empty() {
                        key
                    } else {
                        *option_name
                    };
                    options.set_option_integer(
                        option_name,
                        i64::from(
                            get_json_value_int_option(key_value.value.as_deref(), key).max(0),
                        ),
                    );
                    continue;
                }
                if let Some((_, option_name)) =
                    LONG_KEYS.iter().find(|(json_key, _)| *json_key == key)
                {
                    let option_name = if option_name.is_empty() {
                        key
                    } else {
                        *option_name
                    };
                    options.set_option_integer(
                        option_name,
                        get_json_value_long_option(key_value.value.as_deref(), key).max(0),
                    );
                    continue;
                }
                if let Some((_, option_name)) =
                    STRING_KEYS.iter().find(|(json_key, _)| *json_key == key)
                {
                    let option_name = if option_name.is_empty() {
                        key
                    } else {
                        *option_name
                    };
                    options.set_option_string(
                        option_name,
                        &get_json_value_string_option(key_value.value.as_deref(), key),
                    );
                    continue;
                }
                if IGNORED_OPTIONS.contains(&key) {
                    continue;
                }

                let value = key_value.value.as_deref();
                match key {
                    "ignore_restriction_reasons" => {
                        if let Some(telegram_api::JSONValue::jsonArray(array)) = value {
                            for reason in &array.value {
                                let reason_name = get_json_value_string(reason, key);
                                if !reason_name.is_empty() && !reason_name.contains(',') {
                                    if !ignored_restriction_reasons.is_empty() {
                                        ignored_restriction_reasons.push(',');
                                    }
                                    ignored_restriction_reasons += &reason_name;
                                } else {
                                    tdutils::log_error!(
                                        "Receive unexpected restriction reason {reason_name}"
                                    );
                                }
                            }
                        } else {
                            tdutils::log_error!(
                                "Receive unexpected ignore_restriction_reasons {value:?}"
                            );
                        }
                        continue;
                    }
                    "restriction_add_platforms" => {
                        if let Some(telegram_api::JSONValue::jsonArray(array)) = value {
                            for platform in &array.value {
                                let platform_name = get_json_value_string(platform, key);
                                if !platform_name.is_empty() && !platform_name.contains(',') {
                                    if !restriction_add_platforms.is_empty() {
                                        restriction_add_platforms.push(',');
                                    }
                                    restriction_add_platforms += &platform_name;
                                } else {
                                    tdutils::log_error!(
                                        "Receive unexpected restriction platform {platform_name}"
                                    );
                                }
                            }
                        } else {
                            tdutils::log_error!(
                                "Receive unexpected restriction_add_platforms {value:?}"
                            );
                        }
                        continue;
                    }
                    "emojies_animated_zoom" => {
                        animated_emoji_zoom = get_json_value_double_option(value, key);
                        continue;
                    }
                    "emojies_send_dice" => {
                        if let Some(telegram_api::JSONValue::jsonArray(array)) = value {
                            for emoji in &array.value {
                                let emoji_text = get_json_value_string(emoji, key);
                                if !emoji_text.is_empty() {
                                    dice_emoji_index.insert(emoji_text.clone(), dice_emojis.len());
                                    dice_emojis.push(emoji_text);
                                } else {
                                    tdutils::log_error!("Receive empty dice emoji");
                                }
                            }
                        } else {
                            tdutils::log_error!("Receive unexpected emojies_send_dice {value:?}");
                        }
                        continue;
                    }
                    "emojies_send_dice_success" => {
                        if let Some(telegram_api::JSONValue::jsonObject(success_object)) = value {
                            for success_value in &success_object.value {
                                if success_value.key.is_empty() {
                                    continue;
                                }
                                let Some(telegram_api::JSONValue::jsonObject(inner)) =
                                    success_value.value.as_deref()
                                else {
                                    tdutils::log_error!(
                                        "Receive unexpected dice success value {success_value:?}"
                                    );
                                    continue;
                                };
                                let mut dice_value = -1;
                                let mut frame_start = -1;
                                for dice_key_value in &inner.value {
                                    let Some(telegram_api::JSONValue::jsonNumber(number)) =
                                        dice_key_value.value.as_deref()
                                    else {
                                        continue;
                                    };
                                    let current_value = number.value as i32;
                                    if dice_key_value.key == "value" {
                                        dice_value = current_value;
                                    }
                                    if dice_key_value.key == "frame_start" {
                                        frame_start = current_value;
                                    }
                                }
                                if dice_value < 0 || frame_start < 0 {
                                    tdutils::log_error!(
                                        "Receive unexpected dice success value {success_value:?}"
                                    );
                                } else {
                                    dice_emoji_success_value.insert(
                                        success_value.key.clone(),
                                        format!("{dice_value}:{frame_start}"),
                                    );
                                }
                            }
                        } else {
                            tdutils::log_error!(
                                "Receive unexpected emojies_send_dice_success {value:?}"
                            );
                        }
                        continue;
                    }
                    "emojies_sounds" => {
                        if let Some(telegram_api::JSONValue::jsonObject(sounds)) = value {
                            for sound in &sounds.value {
                                let Some(telegram_api::JSONValue::jsonObject(inner)) =
                                    sound.value.as_deref()
                                else {
                                    tdutils::log_error!("Receive unexpected emoji sound {sound:?}");
                                    continue;
                                };
                                let mut id = String::new();
                                let mut access_hash = String::new();
                                let mut file_reference_base64 = String::new();
                                for sound_key_value in &inner.value {
                                    if !matches!(
                                        sound_key_value.value.as_deref(),
                                        Some(telegram_api::JSONValue::jsonString(_))
                                    ) {
                                        continue;
                                    }
                                    let current_value = get_json_value_string_option(
                                        sound_key_value.value.as_deref(),
                                        "",
                                    );
                                    match sound_key_value.key.as_str() {
                                        "id" => id = current_value,
                                        "access_hash" => access_hash = current_value,
                                        "file_reference_base64" => {
                                            file_reference_base64 = current_value
                                        }
                                        _ => {}
                                    }
                                }
                                if to_integer_safe_i64(&id).is_none()
                                    || to_integer_safe_i64(&access_hash).is_none()
                                    || !is_base64url(file_reference_base64.as_bytes())
                                    || !tdutils::emoji::is_emoji(&sound.key)
                                {
                                    tdutils::log_error!("Receive unexpected sound value {sound:?}");
                                } else {
                                    emoji_sounds.push(sound.key.clone());
                                    emoji_sounds.push(format!(
                                        "{id}:{access_hash}:{file_reference_base64}"
                                    ));
                                }
                            }
                        } else {
                            tdutils::log_error!("Receive unexpected emojies_sounds {value:?}");
                        }
                        continue;
                    }
                    "gif_search_emojies" => {
                        if let Some(telegram_api::JSONValue::jsonArray(array)) = value {
                            for emoji in &array.value {
                                let emoji_str = get_json_value_string(emoji, key);
                                if !emoji_str.is_empty() && !emoji_str.contains(',') {
                                    if !animation_search_emojis.is_empty() {
                                        animation_search_emojis.push(',');
                                    }
                                    animation_search_emojis += &emoji_str;
                                } else {
                                    tdutils::log_error!(
                                        "Receive unexpected animation search emoji {emoji_str}"
                                    );
                                }
                            }
                        } else {
                            tdutils::log_error!("Receive unexpected gif_search_emojies {value:?}");
                        }
                        continue;
                    }
                    "autologin_domains" | "url_auth_domains" | "whitelisted_domains" => {
                        // LinkManager::update_autologin_domains — lands with
                        // the LinkManager; the arrays are not stored.
                        tdutils::log_debug!("Skip {key} until the LinkManager lands");
                        continue;
                    }
                    "round_video_encoding" => {
                        if let Some(telegram_api::JSONValue::jsonObject(settings)) = value {
                            for video_note_setting in &settings.value {
                                let option_name = match video_note_setting.key.as_str() {
                                    "diameter" => "suggested_video_note_length",
                                    "video_bitrate" => "suggested_video_note_video_bitrate",
                                    "audio_bitrate" => "suggested_video_note_audio_bitrate",
                                    "max_size" => "video_note_size_max",
                                    _ => continue,
                                };
                                if let Some(telegram_api::JSONValue::jsonNumber(number)) =
                                    video_note_setting.value.as_deref()
                                {
                                    let setting_value = number.value as i32;
                                    if setting_value > 0 {
                                        options.set_option_integer(
                                            option_name,
                                            i64::from(setting_value),
                                        );
                                    }
                                } else {
                                    tdutils::log_error!(
                                        "Receive unexpected video note setting {video_note_setting:?}"
                                    );
                                }
                            }
                        } else {
                            tdutils::log_error!(
                                "Receive unexpected round_video_encoding {value:?}"
                            );
                        }
                        continue;
                    }
                    "premium_promo_order" => {
                        if let Some(telegram_api::JSONValue::jsonArray(array)) = value {
                            for feature in &array.value {
                                let premium_feature = get_json_value_string(feature, key);
                                if !premium_feature.contains(',') {
                                    premium_features.push(premium_feature);
                                }
                            }
                        } else {
                            tdutils::log_error!("Receive unexpected premium_promo_order {value:?}");
                        }
                        continue;
                    }
                    _ => {}
                }

                // The premium limit keys (`<key>_limit_default|premium`).
                let mut is_premium_limit_key = false;
                for premium_limit_key in premium_limit_keys() {
                    if let Some(suffix) = key.strip_prefix(premium_limit_key)
                        && (suffix == "_limit_default" || suffix == "_limit_premium")
                    {
                        let setting_value =
                            get_json_value_int_option(key_value.value.as_deref(), key);
                        if setting_value > 0 {
                            options.set_option_integer(key, i64::from(setting_value));
                        } else {
                            tdutils::log_error!("Receive invalid value {setting_value} for {key}");
                        }
                        is_premium_limit_key = true;
                        break;
                    }
                }
                if is_premium_limit_key {
                    continue;
                }

                match key {
                    "premium_bot_username" => {
                        premium_bot_username =
                            get_json_value_string_option(key_value.value.as_deref(), key);
                        continue;
                    }
                    "premium_invoice_slug" => {
                        premium_invoice_slug =
                            get_json_value_string_option(key_value.value.as_deref(), key);
                        continue;
                    }
                    "premium_purchase_blocked" => {
                        is_premium_available =
                            !get_json_value_bool_option(key_value.value.as_deref(), key);
                        continue;
                    }
                    "fragment_prefixes" => {
                        if let Some(telegram_api::JSONValue::jsonArray(array)) = value {
                            for prefix in &array.value {
                                let mut prefix_text = get_json_value_string(prefix, key);
                                clean_phone_number(&mut prefix_text);
                                if !prefix_text.is_empty() {
                                    fragment_prefixes.push(prefix_text);
                                } else {
                                    tdutils::log_error!("Receive an invalid Fragment prefix");
                                }
                            }
                        } else {
                            tdutils::log_error!("Receive unexpected fragment_prefixes {value:?}");
                        }
                        continue;
                    }
                    "stories_entities" => {
                        options.set_option_boolean(
                            "need_premium_for_story_caption_entities",
                            get_json_value_string_option(key_value.value.as_deref(), key)
                                == "premium",
                        );
                        continue;
                    }
                    "transcribe_audio_trial_weekly_number" => {
                        transcribe_audio_trial_weekly_number =
                            get_json_value_int_option(key_value.value.as_deref(), key);
                        continue;
                    }
                    "transcribe_audio_trial_duration_max" => {
                        transcribe_audio_trial_duration_max =
                            get_json_value_int_option(key_value.value.as_deref(), key);
                        continue;
                    }
                    "transcribe_audio_trial_cooldown_until" => {
                        transcribe_audio_trial_cooldown_until =
                            get_json_value_int_option(key_value.value.as_deref(), key);
                        continue;
                    }
                    "business_promo_order" => {
                        if let Some(telegram_api::JSONValue::jsonArray(array)) = value {
                            for feature in &array.value {
                                let business_feature = get_json_value_string(feature, key);
                                if !business_feature.contains(',') {
                                    business_features.push(business_feature);
                                }
                            }
                        } else {
                            tdutils::log_error!(
                                "Receive unexpected business_promo_order {value:?}"
                            );
                        }
                        continue;
                    }
                    "new_noncontact_peers_require_premium_without_ownpremium" => {
                        need_premium_for_new_chat_privacy =
                            !get_json_value_bool_option(key_value.value.as_deref(), key);
                        continue;
                    }
                    "web_app_allowed_protocols" => {
                        if let Some(telegram_api::JSONValue::jsonArray(array)) = value {
                            let mut protocol_names: Vec<String> = Vec::new();
                            for protocol in &array.value {
                                let protocol_name = get_json_value_string(protocol, key);
                                if !protocol_name.contains(' ') {
                                    protocol_names.push(protocol_name);
                                }
                            }
                            options.set_option_string(
                                "web_app_allowed_protocols",
                                &implode(&protocol_names, ' '),
                            );
                        } else {
                            tdutils::log_error!(
                                "Receive unexpected web_app_allowed_protocols {value:?}"
                            );
                        }
                        continue;
                    }
                    "starref_start_param_prefixes" => {
                        if let Some(telegram_api::JSONValue::jsonArray(array)) = value {
                            for prefix in &array.value {
                                let prefix_text = get_json_value_string(prefix, key);
                                if !prefix_text.is_empty() && !prefix_text.contains(' ') {
                                    starref_start_param_prefixes.push(prefix_text);
                                } else {
                                    tdutils::log_error!(
                                        "Receive an invalid affiliate program link prefix"
                                    );
                                }
                            }
                        } else {
                            tdutils::log_error!(
                                "Receive unexpected starref_start_param_prefixes {value:?}"
                            );
                        }
                        continue;
                    }
                    "freeze_since_date" => {
                        freeze_since_date =
                            get_json_value_int_option(key_value.value.as_deref(), key);
                        continue;
                    }
                    "freeze_until_date" => {
                        freeze_until_date =
                            get_json_value_int_option(key_value.value.as_deref(), key);
                        continue;
                    }
                    "freeze_appeal_url" => {
                        freeze_appeal_url =
                            get_json_value_string_option(key_value.value.as_deref(), key);
                        continue;
                    }
                    "call_requests_disabled" => {
                        can_accept_calls =
                            !get_json_value_bool_option(key_value.value.as_deref(), key);
                        continue;
                    }
                    "ton_suggested_post_amount_min" => {
                        options.set_option_integer(
                            "suggested_post_gram_cent_count_min",
                            get_json_value_long_option(key_value.value.as_deref(), key) / 10000000,
                        );
                        continue;
                    }
                    "ton_suggested_post_amount_max" => {
                        options.set_option_integer(
                            "suggested_post_gram_cent_count_max",
                            get_json_value_long_option(key_value.value.as_deref(), key) / 10000000,
                        );
                        continue;
                    }
                    "ton_stargift_resale_amount_min" => {
                        options.set_option_integer(
                            "gift_resale_gram_cent_count_min",
                            get_json_value_long_option(key_value.value.as_deref(), key) / 10000000,
                        );
                        continue;
                    }
                    "ton_stargift_resale_amount_max" => {
                        options.set_option_integer(
                            "gift_resale_gram_cent_count_max",
                            get_json_value_long_option(key_value.value.as_deref(), key) / 10000000,
                        );
                        continue;
                    }
                    "ton_usd_rate" => {
                        options.set_option_integer(
                            "million_gram_to_usd_rate",
                            (get_json_value_double_option(key_value.value.as_deref(), key)
                                * 1000000.0) as i64,
                        );
                        continue;
                    }
                    "need_age_video_verification" => {
                        need_age_video_verification =
                            get_json_value_bool_option(key_value.value.as_deref(), key);
                        continue;
                    }
                    "verify_age_bot_username" => {
                        verify_age_bot_username =
                            get_json_value_string_option(key_value.value.as_deref(), key);
                        continue;
                    }
                    "verify_age_country" => {
                        verify_age_country =
                            get_json_value_string_option(key_value.value.as_deref(), key);
                        continue;
                    }
                    "verify_age_min" => {
                        verify_age_min = get_json_value_int_option(key_value.value.as_deref(), key);
                        continue;
                    }
                    "whitelisted_bots" => {
                        if let Some(telegram_api::JSONValue::jsonArray(array)) = value {
                            for bot_user_id in &array.value {
                                let user_id = get_json_value_long(bot_user_id, key);
                                if user_id > 0 {
                                    if !whitelisted_bots.is_empty() {
                                        whitelisted_bots.push(',');
                                    }
                                    whitelisted_bots += &user_id.to_string();
                                } else {
                                    tdutils::log_error!(
                                        "Receive unexpected bot user identifier {user_id}"
                                    );
                                }
                            }
                        } else {
                            tdutils::log_error!("Receive unexpected whitelisted_bots {value:?}");
                        }
                        continue;
                    }
                    "stars_groupcall_message_limits" => {
                        // GroupCallManager::on_update_group_call_message_limits
                        // (ConfigManager.cpp:1975-1978) — the raw value;
                        // the manager side parses it.
                        actions
                            .manager_updates
                            .push(ManagerUpdate::GroupCallMessageLimits(
                                key_value.value.clone(),
                            ));
                        continue;
                    }
                    "ton_stakedice_stake_suggested_amounts" => {
                        if let Some(telegram_api::JSONValue::jsonArray(array)) = value {
                            for amount in &array.value {
                                let ton_amount = get_json_value_long(amount, key);
                                if ton_amount > 0 {
                                    if !ton_stakedice_stake_suggested_amounts.is_empty() {
                                        ton_stakedice_stake_suggested_amounts.push(',');
                                    }
                                    ton_stakedice_stake_suggested_amounts +=
                                        &ton_amount.to_string();
                                } else {
                                    tdutils::log_error!(
                                        "Receive unexpected ton amount {ton_amount}"
                                    );
                                }
                            }
                        } else {
                            tdutils::log_error!(
                                "Receive unexpected ton_stakedice_stake_suggested_amounts {value:?}"
                            );
                        }
                        continue;
                    }
                    "stargifts_craft_attribute_permilles" => {
                        if let Some(telegram_api::JSONValue::jsonArray(array)) = value {
                            let mut row_count = 0i32;
                            for probability_row in &array.value {
                                row_count += 1;
                                let telegram_api::JSONValue::jsonArray(probabilities) =
                                    probability_row.as_ref()
                                else {
                                    tdutils::log_error!("Receive unexpected probability row");
                                    break;
                                };
                                let mut count = 0i32;
                                for probability in &probabilities.value {
                                    let per_mille = get_json_value_int(probability, key);
                                    if 0 < per_mille && per_mille <= 1000 {
                                        if !gift_craft_probabilities.is_empty() {
                                            gift_craft_probabilities.push(',');
                                        }
                                        gift_craft_probabilities += &per_mille.to_string();
                                        count += 1;
                                    } else {
                                        tdutils::log_error!(
                                            "Receive unexpected probability {per_mille}"
                                        );
                                    }
                                }
                                if count != row_count {
                                    tdutils::log_error!(
                                        "Receive {count} gift craft probability in row {row_count}"
                                    );
                                    gift_craft_probabilities.clear();
                                    break;
                                }
                            }
                            if row_count != 4 {
                                tdutils::log_error!(
                                    "Receive {row_count} gift craft probability rows"
                                );
                                gift_craft_probabilities.clear();
                            }
                        } else {
                            tdutils::log_error!(
                                "Receive unexpected stargifts_craft_attribute_permilles {value:?}"
                            );
                        }
                        continue;
                    }
                    _ => {}
                }

                new_values.push(key_value.clone());
            }
        } else {
            tdutils::log_error!("Receive wrong app config {config:?}");
        }

        config = Box::new(telegram_api::JSONValue::jsonObject(
            telegram_api::jsonObject { value: new_values },
        ));

        // The C++ fan-outs: LinkManager::update_autologin_domains stays
        // skipped until the LinkManager (see the key handlers above);
        // the three M.3.9.2 fan-outs (ConfigManager.cpp:2050-2059) are
        // carried for the Td's manager states.
        actions
            .manager_updates
            .push(ManagerUpdate::TrialParameters {
                weekly_number: transcribe_audio_trial_weekly_number,
                duration_max: transcribe_audio_trial_duration_max,
                cooldown_until: transcribe_audio_trial_cooldown_until,
            });
        actions.manager_updates.push(ManagerUpdate::FreezeState {
            since_date: freeze_since_date,
            until_date: freeze_until_date,
            appeal_url: freeze_appeal_url,
        });
        actions
            .manager_updates
            .push(ManagerUpdate::AgeVerificationParameters(
                AgeVerificationParameters::new(
                    need_age_video_verification,
                    verify_age_bot_username,
                    verify_age_country,
                    verify_age_min,
                ),
            ));

        if ignored_restriction_reasons.is_empty() {
            options.set_option_empty("ignored_restriction_reasons");
            if options.get_option_boolean("ignore_sensitive_content_restrictions", true)
                || options.get_option_boolean("can_ignore_sensitive_content_restrictions", true)
            {
                actions.extend(self.get_content_settings(None, is_authorized, is_bot, close_flag));
            }
        } else {
            options.set_option_string("ignored_restriction_reasons", &ignored_restriction_reasons);
            if !options.get_option_boolean("can_ignore_sensitive_content_restrictions", false)
                || !options.get_option_boolean("ignore_sensitive_content_restrictions", false)
            {
                actions.extend(self.get_content_settings(None, is_authorized, is_bot, close_flag));
            }
        }
        if restriction_add_platforms.is_empty() {
            options.set_option_empty("restriction_add_platforms");
        } else {
            options.set_option_string("restriction_add_platforms", &restriction_add_platforms);
        }
        options.set_option_string("whitelisted_bots", &whitelisted_bots);

        if !dice_emojis.is_empty() {
            let mut dice_success_values = vec![String::new(); dice_emojis.len()];
            for (emoji, value) in &dice_emoji_success_value {
                let Some(index) = dice_emoji_index.get(emoji) else {
                    tdutils::log_error!("Can't find emoji {emoji}");
                    continue;
                };
                dice_success_values[*index] = value.clone();
            }
            options.set_option_string("dice_success_values", &implode(&dice_success_values, ','));
            options.set_option_string("dice_emojis", &implode(&dice_emojis, '\x01'));
        }

        options.set_option_string("fragment_prefixes", &implode(&fragment_prefixes, ','));

        if starref_start_param_prefixes.is_empty() {
            options.set_option_empty("starref_start_param_prefixes");
        } else {
            options.set_option_string(
                "starref_start_param_prefixes",
                &implode(&starref_start_param_prefixes, ' '),
            );
        }

        options.set_option_string("emoji_sounds", &implode(&emoji_sounds, ','));

        if animated_emoji_zoom <= 0.0 || animated_emoji_zoom > 2.0 {
            options.set_option_empty("animated_emoji_zoom");
        } else {
            options.set_option_integer("animated_emoji_zoom", (animated_emoji_zoom * 1e9) as i64);
        }
        if animation_search_emojis.is_empty() {
            options.set_option_empty("animation_search_emojis");
        } else {
            options.set_option_string("animation_search_emojis", &animation_search_emojis);
        }

        options.set_option_boolean("can_accept_calls", can_accept_calls);

        if !is_premium_available {
            premium_bot_username.clear(); // just in case
            premium_invoice_slug.clear(); // just in case
            premium_features.clear(); // just in case
            business_features.clear(); // just in case
            options.set_option_empty("is_premium_available");
        } else {
            options.set_option_boolean("is_premium_available", is_premium_available);
        }
        options.set_option_string("premium_features", &implode(&premium_features, ','));
        options.set_option_string("business_features", &implode(&business_features, ','));
        if premium_bot_username.is_empty() {
            options.set_option_empty("premium_bot_username");
        } else {
            options.set_option_string("premium_bot_username", &premium_bot_username);
        }
        if premium_invoice_slug.is_empty() {
            options.set_option_empty("premium_invoice_slug");
        } else {
            options.set_option_string("premium_invoice_slug", &premium_invoice_slug);
        }
        options.set_option_string(
            "ton_stakedice_stake_suggested_amounts",
            &ton_stakedice_stake_suggested_amounts,
        );
        if gift_craft_probabilities.is_empty() {
            options.set_option_empty("stargifts_craft_attribute_permilles");
        } else {
            options.set_option_string(
                "stargifts_craft_attribute_permilles",
                &gift_craft_probabilities,
            );
        }

        options.set_option_boolean(
            "need_premium_for_new_chat_privacy",
            need_premium_for_new_chat_privacy,
        );

        options.set_option_empty("default_ton_blockchain_config");
        options.set_option_empty("default_ton_blockchain_name");
        options.set_option_empty("story_viewers_expire_period");

        (config, actions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tddb::db_key::DbKey;

    fn options() -> OptionManager {
        OptionManager::new(false)
    }

    /// A writable pmc over a temporary binlog file (the shared M.2.1 test
    /// pattern): `BinlogKeyValue` panics on writes until a binlog is
    /// attached.
    struct TempPmc {
        pmc: BinlogKeyValue,
        path: String,
    }

    impl TempPmc {
        fn new(name: &str) -> TempPmc {
            let path = std::env::temp_dir()
                .join(format!("td-rs-cfg-{name}-{}.binlog", std::process::id()))
                .to_string_lossy()
                .into_owned();
            let _ = BinlogKeyValue::destroy(&path);
            let mut pmc = BinlogKeyValue::new();
            pmc.init(&path, DbKey::empty()).ensure();
            TempPmc { pmc, path }
        }
    }

    impl std::ops::Deref for TempPmc {
        type Target = BinlogKeyValue;

        fn deref(&self) -> &BinlogKeyValue {
            &self.pmc
        }
    }

    impl std::ops::DerefMut for TempPmc {
        fn deref_mut(&mut self) -> &mut BinlogKeyValue {
            &mut self.pmc
        }
    }

    impl Drop for TempPmc {
        fn drop(&mut self) {
            self.pmc.close();
            let _ = BinlogKeyValue::destroy(&self.path);
        }
    }

    fn json_bool(value: bool) -> Box<telegram_api::JSONValue> {
        Box::new(telegram_api::JSONValue::jsonBool(telegram_api::jsonBool {
            value,
        }))
    }

    fn json_number(value: f64) -> Box<telegram_api::JSONValue> {
        Box::new(telegram_api::JSONValue::jsonNumber(
            telegram_api::jsonNumber { value },
        ))
    }

    fn json_string(value: &str) -> Box<telegram_api::JSONValue> {
        Box::new(telegram_api::JSONValue::jsonString(
            telegram_api::jsonString {
                value: value.to_string(),
            },
        ))
    }

    fn json_object(
        values: Vec<(String, Box<telegram_api::JSONValue>)>,
    ) -> Box<telegram_api::JSONValue> {
        Box::new(telegram_api::JSONValue::jsonObject(
            telegram_api::jsonObject {
                value: values
                    .into_iter()
                    .map(|(key, value)| {
                        Box::new(telegram_api::JSONObjectValue {
                            key,
                            value: Some(value),
                        })
                    })
                    .collect(),
            },
        ))
    }

    fn test_config() -> telegram_api::Config {
        telegram_api::Config {
            flags: 0,
            default_p2p_contacts: false,
            preload_featured_stickers: false,
            revoke_pm_inbox: true,
            blocked_mode: true,
            force_try_ipv6: false,
            date: 1000,
            expires: 2000,
            test_mode: false,
            this_dc: 1,
            dc_options: Vec::new(),
            dc_txt_domain_name: "example.com".to_string(),
            chat_size_max: 200,
            megagroup_size_max: 10000,
            forwarded_count_max: 100,
            online_update_period_ms: 120000,
            offline_blur_timeout_ms: 0,
            offline_idle_timeout_ms: 0,
            online_cloud_timeout_ms: 300000,
            notify_cloud_delay_ms: 30000,
            notify_default_delay_ms: 1500,
            push_chat_period_ms: 0,
            push_chat_limit: 0,
            edit_time_limit: 172800,
            revoke_time_limit: 172800,
            revoke_pm_time_limit: 172800,
            rating_e_decay: 2419200,
            stickers_recent_limit: 30,
            channels_read_media_period: 604800,
            tmp_sessions: 100,
            call_receive_timeout_ms: 20000,
            call_ring_timeout_ms: 90000,
            call_connect_timeout_ms: 30000,
            call_packet_timeout_ms: 10000,
            me_url_prefix: "https://t.me".to_string(),
            autoupdate_url_prefix: String::new(),
            gif_search_username: "gif".to_string(),
            venue_search_username: "foursquare".to_string(),
            img_search_username: "bing".to_string(),
            static_maps_provider: String::new(),
            caption_length_max: 1024,
            message_length_max: 4096,
            webfile_dc_id: 4,
            suggested_lang_code: "en".to_string(),
            lang_pack_version: 5,
            base_lang_pack_version: 4,
            reactions_default: Some(telegram_api::Reaction::reactionEmoji(
                telegram_api::reactionEmoji {
                    emoticon: "👍".to_string(),
                },
            )),
            autologin_token: "token".to_string(),
        }
    }

    #[test]
    fn flood_control_limits_consecutive_events() {
        let mut flood = FloodControlStrict::default();
        flood.add_limit(20, 1);
        flood.add_event(100.0);
        // One event of the allowed 1 per 20s: the next is allowed at +20.
        assert_eq!(flood.get_wakeup_at(), 120.0);
        flood.add_event(100.0);
        assert_eq!(flood.get_wakeup_at(), 120.0);
    }

    #[test]
    fn app_config_bytes_match_the_cpp_layout() {
        let app_config = AppConfig {
            version: APP_CONFIG_CURRENT_VERSION,
            hash: 0,
            config: Box::new(telegram_api::JSONValue::jsonObject(
                telegram_api::jsonObject { value: Vec::new() },
            )),
        };
        let bytes = app_config.store();
        let mut expected = Vec::new();
        expected.extend_from_slice(&APP_CONFIG_CURRENT_VERSION.to_le_bytes());
        expected.extend_from_slice(&0i32.to_le_bytes());
        expected.extend_from_slice(&telegram_api::jsonObject::ID.to_le_bytes());
        expected.extend_from_slice(&telegram_api::tl::VECTOR_ID.to_le_bytes());
        expected.extend_from_slice(&0i32.to_le_bytes());
        assert_eq!(bytes, expected);

        let parsed = AppConfig::parse(&bytes).unwrap();
        assert_eq!(parsed.version, APP_CONFIG_CURRENT_VERSION);
        assert_eq!(parsed.hash, 0);
        assert!(matches!(
            parsed.config.as_ref(),
            telegram_api::JSONValue::jsonObject(_)
        ));
        assert!(AppConfig::parse(b"").is_err());
        assert!(AppConfig::parse(b"short").is_err());
    }

    #[test]
    fn start_up_requests_the_config_post_init() {
        let mut manager = ConfigManager::new(b"");
        let actions = manager.start_up(&BinlogKeyValue::new(), false);
        assert_eq!(
            actions.queries,
            vec![ConfigQuery::GetConfig {
                dc_id: 1,
                reopen_sessions: false,
            }]
        );
        assert_eq!(manager.expire_time(), 0.0);
        assert_eq!(manager.config_sent_cnt, 1);
    }

    #[test]
    fn start_up_bot_keeps_a_future_expire_time() {
        let mut pmc = TempPmc::new("startup-bot");
        let stored = tdutils::time::system() as i32 + 300;
        pmc.set("config_expire", &stored.to_string());
        let mut manager = ConfigManager::new(b"");
        let actions = manager.start_up(&pmc, true);
        assert!(actions.queries.is_empty());
        assert!(manager.expire_time() > tdutils::time::now() + 200.0);
    }

    #[test]
    fn request_config_is_coalesced_while_in_flight() {
        let mut manager = ConfigManager::new(b"");
        assert_eq!(manager.request_config(false, false).queries.len(), 1);
        assert!(manager.request_config(false, false).queries.is_empty());
        // reopen_sessions forces a second query (the C++ token 9).
        assert_eq!(manager.request_config(true, false).queries.len(), 1);
    }

    #[test]
    fn process_config_applies_the_options() {
        let mut pmc = TempPmc::new("process-config");
        let mut options = options();
        let before = options.all_options();
        let mut manager = ConfigManager::new(b"");
        manager.request_config(false, false);
        let config = test_config();
        let actions = manager.on_get_config_result(
            &mut pmc,
            Ok(config.clone()),
            false,
            false,
            &mut options,
            false,
            false,
            false,
            1,
        );

        assert_eq!(options.get_option_integer("recent_stickers_limit", 0), 30);
        assert_eq!(
            options.get_option_integer("channels_read_media_period", 0),
            604800
        );
        assert!(!options.get_option_boolean("test_mode", true));
        assert!(options.get_option_boolean("expect_blocking", false));
        assert_eq!(
            options.get_option_string("dc_txt_domain_name", ""),
            "example.com"
        );
        assert_eq!(options.get_option_string("t_me_url", ""), "https://t.me/");
        assert_eq!(options.get_option_integer("webfile_dc_id", 0), 4);
        assert_eq!(options.get_option_integer("session_count", 0), 100);
        assert_eq!(
            options.get_option_string("suggested_language_pack_id", ""),
            "en"
        );
        assert_eq!(options.get_option_integer("edit_time_limit", 0), 172800);
        assert!(options.get_option_boolean("revoke_pm_inbox", false));
        assert_eq!(options.get_option_integer("rating_e_decay", 0), 2419200);
        assert_eq!(
            options.get_option_integer("message_text_length_max", 0),
            4096
        );
        assert_eq!(
            options.get_option_integer("message_caption_length_max", 0),
            1024
        );
        assert_eq!(
            options.get_option_string("animation_search_bot_username", ""),
            "gif"
        );
        assert_eq!(
            options.get_option_string("venue_search_bot_username", ""),
            "foursquare"
        );
        assert_eq!(options.get_option_string("default_reaction", ""), "👍");
        // The outdated options are erased.
        for name in [
            "chat_big_size",
            "saved_gifs_limit",
            "forwarded_messages_count_max",
            "calls_enabled",
        ] {
            assert!(!options.have_option(name), "{name} must be erased");
        }

        // The expire timer and the pmc value: 2000 - 1000, jittered by
        // [0, 200).
        let expires_in = f64::from(config.expires - config.date);
        let stored = pmc.get("config_expire").parse::<i32>().unwrap();
        assert!((stored as f64 - tdutils::time::system() - expires_in).abs() < 2.0);
        assert!(manager.expire_time() > tdutils::time::now() + 700.0);
        assert!(manager.expire_time() <= tdutils::time::now() + 1000.0);

        // The follow-up: reload_app_config (GetAppConfig) and — unauthorized —
        // an immediate content-settings refresh (no query).
        assert_eq!(actions.queries, vec![ConfigQuery::GetAppConfig { hash: 0 }]);

        // The full store: exactly the C++ table, nothing more, nothing less.
        assert_option_snapshot(&options, &before, &MAIN_DC_SET, &OUTDATED_OPTIONS);
    }

    #[test]
    fn process_config_from_another_dc_limits_the_options() {
        let mut pmc = TempPmc::new("other-dc");
        let mut options = options();
        let before = options.all_options();
        let mut manager = ConfigManager::new(b"");
        manager.request_config(false, false);
        let actions = manager.on_get_config_result(
            &mut pmc,
            Ok(test_config()),
            false,
            false,
            &mut options,
            false,
            false,
            false,
            5, // the main DC is not this_dc
        );
        // is_from_main_dc-only options stay untouched (t_me_url and the
        // other `is_from_main_dc || !have_option` guards still apply from
        // any DC when unset, like the C++).
        assert!(!options.have_option("webfile_dc_id"));
        assert!(!options.have_option("session_count"));
        assert!(!options.have_option("suggested_language_pack_id"));
        // No follow-up queries and an immediate re-request (reload_in = 0).
        assert!(actions.queries.is_empty());
        assert!(manager.expire_time() <= tdutils::time::now());
        assert_option_snapshot(&options, &before, &OTHER_DC_SET, &OUTDATED_OPTIONS);
    }

    /// The exact effect of `process_config` on the option store: the
    /// golden `set` applied to the captured baseline with `erased`
    /// removed last — the same order the C++ applies them. A stray
    /// write, a missed set or a wrong value anywhere fails.
    fn assert_option_snapshot(
        options: &OptionManager,
        before: &[(String, String)],
        set: &[(&str, &str)],
        erased: &[&str],
    ) {
        let mut expected: std::collections::BTreeMap<String, String> =
            before.iter().cloned().collect();
        for (name, value) in set {
            expected.insert(name.to_string(), value.to_string());
        }
        for name in erased {
            expected.remove(*name);
        }
        let actual: std::collections::BTreeMap<String, String> =
            options.all_options().into_iter().collect();
        assert_eq!(actual, expected);
    }

    /// The C++ "delete outdated options" list (ConfigManager.cpp:1287-1303).
    const OUTDATED_OPTIONS: [&str; 16] = [
        "suggested_language_code",
        "chat_big_size",
        "group_size_max",
        "saved_gifs_limit",
        "sessions_count",
        "forwarded_messages_count_max",
        "broadcast_size_max",
        "group_chat_size_max",
        "chat_size_max",
        "megagroup_size_max",
        "offline_blur_timeout_ms",
        "offline_idle_timeout_ms",
        "notify_cloud_delay_ms",
        "notify_default_delay_ms",
        "large_chat_size",
        "calls_enabled",
    ];

    /// The golden snapshot for `test_config()` from the main DC: every
    /// option the C++ table sets (ConfigManager.cpp:1193-1285), in the
    /// store's coded form. The dc_options/autologin_token fan-outs
    /// (M.3.9.1) set no options.
    const MAIN_DC_SET: [(&str, &str); 33] = [
        ("recent_stickers_limit", "I30"),
        ("channels_read_media_period", "I604800"),
        ("test_mode", "Bfalse"),
        ("forwarded_message_count_max", "I100"),
        ("basic_group_size_max", "I200"),
        ("supergroup_size_max", "I10000"),
        ("expect_blocking", "Btrue"),
        ("dc_txt_domain_name", "Sexample.com"),
        ("t_me_url", "Shttps://t.me/"),
        ("webfile_dc_id", "I4"),
        ("session_count", "I100"),
        ("suggested_language_pack_id", "Sen"),
        ("language_pack_version", "I5"),
        ("base_language_pack_version", "I4"),
        ("edit_time_limit", "I172800"),
        ("revoke_pm_inbox", "Btrue"),
        ("revoke_time_limit", "I172800"),
        ("revoke_pm_time_limit", "I172800"),
        ("rating_e_decay", "I2419200"),
        ("call_ring_timeout_ms", "I90000"),
        ("call_connect_timeout_ms", "I30000"),
        ("call_packet_timeout_ms", "I10000"),
        ("call_receive_timeout_ms", "I20000"),
        ("message_text_length_max", "I4096"),
        ("message_caption_length_max", "I1024"),
        ("animation_search_bot_username", "Sgif"),
        ("venue_search_bot_username", "Sfoursquare"),
        ("photo_search_bot_username", "Sbing"),
        ("online_update_period_ms", "I120000"),
        ("online_cloud_timeout_ms", "I300000"),
        ("notification_cloud_delay_ms", "I30000"),
        ("notification_default_delay_ms", "I1500"),
        ("default_reaction", "S👍"),
    ];

    /// From a non-main DC the main-DC-only options are skipped
    /// (ConfigManager.cpp:1217-1243, 1280-1285); the guarded options
    /// still land because they were unset.
    const OTHER_DC_SET: [(&str, &str); 22] = [
        ("recent_stickers_limit", "I30"),
        ("channels_read_media_period", "I604800"),
        ("test_mode", "Bfalse"),
        ("forwarded_message_count_max", "I100"),
        ("basic_group_size_max", "I200"),
        ("supergroup_size_max", "I10000"),
        ("expect_blocking", "Btrue"),
        ("dc_txt_domain_name", "Sexample.com"),
        ("t_me_url", "Shttps://t.me/"),
        ("call_ring_timeout_ms", "I90000"),
        ("call_connect_timeout_ms", "I30000"),
        ("call_packet_timeout_ms", "I10000"),
        ("call_receive_timeout_ms", "I20000"),
        ("message_text_length_max", "I4096"),
        ("message_caption_length_max", "I1024"),
        ("animation_search_bot_username", "Sgif"),
        ("venue_search_bot_username", "Sfoursquare"),
        ("photo_search_bot_username", "Sbing"),
        ("online_update_period_ms", "I120000"),
        ("online_cloud_timeout_ms", "I300000"),
        ("notification_cloud_delay_ms", "I30000"),
        ("notification_default_delay_ms", "I1500"),
    ];

    #[test]
    fn process_config_clamps_and_skips_empty_options() {
        let mut pmc = TempPmc::new("clamps");
        let mut options = options();
        let before = options.all_options();
        let mut manager = ConfigManager::new(b"");
        manager.request_config(false, false);
        let config = telegram_api::Config {
            suggested_lang_code: String::new(),
            lang_pack_version: 0,
            base_lang_pack_version: 0,
            tmp_sessions: 0,
            gif_search_username: String::new(),
            venue_search_username: String::new(),
            img_search_username: String::new(),
            reactions_default: Some(telegram_api::Reaction::reactionEmpty(
                telegram_api::reactionEmpty,
            )),
            message_length_max: 5,
            caption_length_max: 2_000_000,
            online_update_period_ms: 100,
            online_cloud_timeout_ms: 100_000_000,
            notify_cloud_delay_ms: 0,
            notify_default_delay_ms: 0,
            me_url_prefix: "https://example.org".to_string(),
            ..test_config()
        };
        manager.on_get_config_result(
            &mut pmc,
            Ok(config),
            false,
            false,
            &mut options,
            false,
            false,
            false,
            1,
        );
        // The language trio, session_count, the three search bots and
        // default_reaction stay absent; the clamps and the t_me_url
        // slash match the C++.
        let set = [
            ("recent_stickers_limit", "I30"),
            ("channels_read_media_period", "I604800"),
            ("test_mode", "Bfalse"),
            ("forwarded_message_count_max", "I100"),
            ("basic_group_size_max", "I200"),
            ("supergroup_size_max", "I10000"),
            ("expect_blocking", "Btrue"),
            ("dc_txt_domain_name", "Sexample.com"),
            ("t_me_url", "Shttps://example.org/"),
            ("webfile_dc_id", "I4"),
            ("edit_time_limit", "I172800"),
            ("revoke_pm_inbox", "Btrue"),
            ("revoke_time_limit", "I172800"),
            ("revoke_pm_time_limit", "I172800"),
            ("rating_e_decay", "I2419200"),
            ("call_ring_timeout_ms", "I90000"),
            ("call_connect_timeout_ms", "I30000"),
            ("call_packet_timeout_ms", "I10000"),
            ("call_receive_timeout_ms", "I20000"),
            ("message_text_length_max", "I4096"),
            ("message_caption_length_max", "I1000000"),
            ("online_update_period_ms", "I1000"),
            ("online_cloud_timeout_ms", "I86400000"),
            ("notification_cloud_delay_ms", "I1000"),
            ("notification_default_delay_ms", "I1000"),
        ];
        assert_option_snapshot(&options, &before, &set, &OUTDATED_OPTIONS);
    }

    #[test]
    fn process_config_guards_preexisting_options() {
        // Main DC: a pre-set venue bot name and the need-sync flag
        // survive the config; expect_blocking is overwritten.
        let mut pmc = TempPmc::new("guards-main");
        let mut main_options = options();
        main_options.set_option_string("venue_search_bot_username", "custom");
        main_options.set_option_string("default_reaction_need_sync", "1");
        main_options.set_option_boolean("expect_blocking", false);
        let mut main_manager = ConfigManager::new(b"");
        main_manager.request_config(false, false);
        main_manager.on_get_config_result(
            &mut pmc,
            Ok(test_config()),
            false,
            false,
            &mut main_options,
            false,
            false,
            false,
            1,
        );
        assert_eq!(
            main_options.get_option_string("venue_search_bot_username", ""),
            "custom"
        );
        assert!(!main_options.have_option("default_reaction"));
        // Overwritten to blocked_mode on the main DC, unlike the guards.
        assert!(main_options.get_option_boolean("expect_blocking", false));

        // Non-main DC: the guarded options keep their pre-set values.
        let mut pmc = TempPmc::new("guards-other");
        let mut other_options = options();
        other_options.set_option_boolean("expect_blocking", false);
        other_options.set_option_string("dc_txt_domain_name", "first.example");
        other_options.set_option_string("t_me_url", "https://first.example/");
        let mut other_manager = ConfigManager::new(b"");
        other_manager.request_config(false, false);
        other_manager.on_get_config_result(
            &mut pmc,
            Ok(test_config()),
            false,
            false,
            &mut other_options,
            false,
            false,
            false,
            5,
        );
        assert!(!other_options.get_option_boolean("expect_blocking", true));
        assert_eq!(
            other_options.get_option_string("dc_txt_domain_name", ""),
            "first.example"
        );
        assert_eq!(
            other_options.get_option_string("t_me_url", ""),
            "https://first.example/"
        );
    }

    #[test]
    fn config_failure_retries_after_a_minute_and_fails_reloads() {
        let mut pmc = BinlogKeyValue::new();
        let mut options = options();
        let mut manager = ConfigManager::new(b"");
        manager.request_config(false, false);
        manager.reload_config(Some(7), false);
        let actions = manager.on_get_config_result(
            &mut pmc,
            Err(Status::error(500, "Server error")),
            false,
            false,
            &mut options,
            false,
            false,
            false,
            1,
        );
        assert_eq!(actions.errors.len(), 1);
        assert_eq!(actions.errors[0].0, 7);
        assert_eq!(actions.errors[0].1.code(), 500);
        assert_eq!(actions.errors[0].1.message(), "Server error");
        let retry_in = manager.expire_time() - tdutils::time::now();
        assert!(retry_in > 55.0 && retry_in <= 60.0);
    }

    #[test]
    fn expire_timeout_rerequests_the_config() {
        let mut pmc = TempPmc::new("expire-timeout");
        let mut options = options();
        let mut manager = ConfigManager::new(b"");
        manager.request_config(true, false);
        manager.on_get_config_result(
            &mut pmc,
            Ok(test_config()),
            true,
            false,
            &mut options,
            false,
            false,
            false,
            1,
        );
        // The timer is ~1000s out: an early timeout is a no-op.
        assert!(manager.on_timeout(false).queries.is_empty());
        // A non-main-DC answer sets reload_in = 0 — the timer is already
        // due, and the reopen flag consumed by the first answer keeps the
        // re-request plain.
        manager.request_config(false, false);
        manager.on_get_config_result(
            &mut pmc,
            Ok(test_config()),
            false,
            false,
            &mut options,
            false,
            false,
            false,
            5,
        );
        let actions = manager.on_timeout(false);
        assert_eq!(
            actions.queries,
            vec![ConfigQuery::GetConfig {
                dc_id: 1,
                reopen_sessions: false,
            }]
        );
        // The fired timer is spent: a second timeout is a no-op.
        assert!(manager.on_timeout(false).queries.is_empty());
    }

    #[test]
    fn reload_config_completes_the_promises_on_success() {
        let mut pmc = TempPmc::new("reload-success");
        let mut options = options();
        let mut manager = ConfigManager::new(b"");
        let first = manager.reload_config(Some(7), false);
        assert_eq!(first.queries.len(), 1);
        // The second reload is coalesced into the in-flight query.
        assert!(manager.reload_config(Some(8), false).queries.is_empty());
        let actions = manager.on_get_config_result(
            &mut pmc,
            Ok(test_config()),
            false,
            false,
            &mut options,
            false,
            false,
            false,
            1,
        );
        assert_eq!(actions.ok_ids, vec![7, 8]);
    }

    #[test]
    fn app_config_answer_and_persistence() {
        let mut pmc = TempPmc::new("app-config");
        let mut options = options();
        let mut manager = ConfigManager::new(b"");
        let actions = manager.get_app_config(7, false, false);
        assert_eq!(actions.queries, vec![ConfigQuery::GetAppConfig { hash: 0 }]);

        let config = json_object(vec![
            ("quote_length_max".to_string(), json_number(2000.0)),
            ("can_edit_factcheck".to_string(), json_bool(true)),
        ]);
        let actions = manager.on_get_app_config_result(
            &mut pmc,
            Ok(AppConfigQueryResult::Config { hash: 42, config }),
            false,
            &mut options,
            false,
            false,
        );
        assert!(actions.refresh_premium_options);
        assert_eq!(actions.app_config_answers.len(), 1);
        let (id, value) = &actions.app_config_answers[0];
        assert_eq!(*id, 7);
        assert!(matches!(value, td_api::JsonValue::jsonValueObject(_)));
        assert_eq!(
            options.get_option_integer("message_reply_quote_length_max", 0),
            2000
        );
        assert!(options.get_option_boolean("can_edit_fact_check", false));

        // The filtered config persists with the hash.
        let parsed = AppConfig::parse(&pmc.get_raw(b"app_config")).unwrap();
        assert_eq!(parsed.hash, 42);
        assert!(matches!(
            parsed.config.as_ref(),
            telegram_api::JSONValue::jsonObject(object) if object.value.is_empty()
        ));
    }

    #[test]
    fn emoji_sounds_are_validated_like_the_cpp() {
        let mut pmc = TempPmc::new("emoji-sounds");
        let mut options = options();
        let mut manager = ConfigManager::new(b"");
        manager.get_app_config(7, false, false);

        let sound_value = |id: &str| {
            json_object(vec![
                ("id".to_string(), json_string(id)),
                ("access_hash".to_string(), json_string("456")),
                ("file_reference_base64".to_string(), json_string("QmFzZTY0")),
            ])
        };
        let config = json_object(vec![(
            "emojies_sounds".to_string(),
            json_object(vec![
                // A table emoji with canonical ids is accepted.
                ("⌚".to_string(), sound_value("123")),
                // A non-emoji key is rejected (ConfigManager.cpp:1669).
                ("not-an-emoji".to_string(), sound_value("123")),
                // A leading-zero id fails to_integer_safe's round trip.
                ("⌛".to_string(), sound_value("0123")),
                // An emoji + variation selector and i64::MIN are accepted.
                (
                    "⌚\u{FE0F}".to_string(),
                    sound_value("-9223372036854775808"),
                ),
            ]),
        )]);
        manager.on_get_app_config_result(
            &mut pmc,
            Ok(AppConfigQueryResult::Config { hash: 1, config }),
            false,
            &mut options,
            false,
            false,
        );
        assert_eq!(
            options.get_option_string("emoji_sounds", ""),
            "⌚,123:456:QmFzZTY0,⌚\u{FE0F},-9223372036854775808:456:QmFzZTY0"
        );
    }

    #[test]
    fn to_integer_safe_matches_the_cpp_round_trip() {
        assert_eq!(to_integer_safe_i64("0"), Some(0));
        assert_eq!(to_integer_safe_i64("-1"), Some(-1));
        assert_eq!(
            to_integer_safe_i64("123456789012345678"),
            Some(123456789012345678)
        );
        assert_eq!(to_integer_safe_i64("9223372036854775807"), Some(i64::MAX));
        assert_eq!(to_integer_safe_i64("-9223372036854775808"), Some(i64::MIN));
        // Out-of-range values wrap in to_integer and fail the round trip.
        assert_eq!(to_integer_safe_i64("9223372036854775808"), None);
        assert_eq!(to_integer_safe_i64("-9223372036854775809"), None);
        // Non-canonical forms the C++ rejects via the PSLICE round trip.
        assert_eq!(to_integer_safe_i64("+5"), None);
        assert_eq!(to_integer_safe_i64("05"), None);
        assert_eq!(to_integer_safe_i64("-0"), None);
        assert_eq!(to_integer_safe_i64(""), None);
        assert_eq!(to_integer_safe_i64("-"), None);
        assert_eq!(to_integer_safe_i64("1x"), None);
        assert_eq!(to_integer_safe_i64(" 1"), None);
        assert_eq!(to_integer_safe_i64("1 "), None);
    }

    #[test]
    fn app_config_not_modified_fails_before_the_first_config() {
        let mut pmc = BinlogKeyValue::new();
        let mut options = options();
        let mut manager = ConfigManager::new(b"");
        manager.get_app_config(7, false, false);
        let actions = manager.on_get_app_config_result(
            &mut pmc,
            Ok(AppConfigQueryResult::NotModified),
            false,
            &mut options,
            false,
            false,
        );
        assert_eq!(actions.errors.len(), 1);
        assert_eq!(actions.errors[0].0, 7);
        assert_eq!(actions.errors[0].1.code(), 500);
        assert_eq!(actions.errors[0].1.message(), "Receive unexpected response");
    }

    #[test]
    fn bot_gets_null_app_config() {
        let mut manager = ConfigManager::new(b"");
        let actions = manager.get_app_config(7, true, false);
        assert!(actions.queries.is_empty());
        assert!(matches!(
            actions.app_config_answers[0].1,
            td_api::JsonValue::jsonValueNull(_)
        ));
    }

    #[test]
    fn app_config_fan_outs_reach_the_manager_states() {
        let mut pmc = TempPmc::new("app-config-m392");
        let mut options = options();
        let mut manager = ConfigManager::new(b"");
        manager.get_app_config(7, false, false);
        let groupcall_limits = Box::new(telegram_api::JSONValue::jsonArray(
            telegram_api::jsonArray {
                value: vec![Box::new(telegram_api::JSONValue::jsonObject(
                    telegram_api::jsonObject {
                        value: vec![Box::new(telegram_api::JSONObjectValue {
                            key: "stars".to_string(),
                            value: Some(json_number(100.0)),
                        })],
                    },
                ))],
            },
        ));
        let config = json_object(vec![
            (
                "stars_groupcall_message_limits".to_string(),
                groupcall_limits,
            ),
            (
                "transcribe_audio_trial_weekly_number".to_string(),
                json_number(10.0),
            ),
            (
                "transcribe_audio_trial_duration_max".to_string(),
                json_number(300.0),
            ),
            (
                "transcribe_audio_trial_cooldown_until".to_string(),
                json_number(2000.0),
            ),
            ("freeze_since_date".to_string(), json_number(100.0)),
            ("freeze_until_date".to_string(), json_number(200.0)),
            (
                "freeze_appeal_url".to_string(),
                json_string("https://example.org"),
            ),
            ("need_age_video_verification".to_string(), json_bool(true)),
            ("verify_age_bot_username".to_string(), json_string("bot")),
            ("verify_age_country".to_string(), json_string("DE")),
            ("verify_age_min".to_string(), json_number(18.0)),
        ]);
        let actions = manager.on_get_app_config_result(
            &mut pmc.pmc,
            Ok(AppConfigQueryResult::Config { hash: 42, config }),
            false,
            &mut options,
            true,
            false,
        );

        // The fan-outs in the C++ emission order: the in-loop group-call
        // one, then the three after the key loop.
        assert_eq!(actions.manager_updates.len(), 4);
        assert!(matches!(
            actions.manager_updates[0],
            ManagerUpdate::GroupCallMessageLimits(_)
        ));
        let trial = match &actions.manager_updates[1] {
            ManagerUpdate::TrialParameters {
                weekly_number,
                duration_max,
                cooldown_until,
            } => (*weekly_number, *duration_max, *cooldown_until),
            _ => panic!("trial parameters fan-out"),
        };
        assert_eq!(trial, (10, 300, 2000));
        let freeze = match &actions.manager_updates[2] {
            ManagerUpdate::FreezeState {
                since_date,
                until_date,
                appeal_url,
            } => (*since_date, *until_date, appeal_url.clone()),
            _ => panic!("freeze state fan-out"),
        };
        assert_eq!(freeze, (100, 200, "https://example.org".to_string()));
        match &actions.manager_updates[3] {
            ManagerUpdate::AgeVerificationParameters(parameters) => assert_eq!(
                *parameters,
                AgeVerificationParameters::new(true, "bot".into(), "DE".into(), 18)
            ),
            _ => panic!("age verification fan-out"),
        }

        // Applying them drives the manager states and persists all four
        // pmc keys.
        let mut states = crate::manager_option_states::ManagerOptionStates::default();
        let mut sent = Vec::new();
        for update in actions.manager_updates {
            let update = { states.on_manager_update(&mut pmc.pmc, update, true, false, 1000) };
            sent.extend(update);
        }
        assert_eq!(sent.len(), 4);
        assert!(matches!(
            sent[0],
            td_api::Object::updateGroupCallMessageLevels(_)
        ));
        assert!(matches!(
            sent[1],
            td_api::Object::updateSpeechRecognitionTrial(_)
        ));
        assert!(matches!(sent[2], td_api::Object::updateFreezeState(_)));
        assert!(matches!(
            sent[3],
            td_api::Object::updateAgeVerificationParameters(_)
        ));
        assert!(pmc.get_raw(b"speech_recognition_trial").len() > 4);
        assert_eq!(pmc.get("freeze_state"), "100 200 https://example.org");
        assert!(pmc.get_raw(b"age_verification").len() > 4);
        assert!(pmc.get_raw(b"group_call_message_limits").len() > 8);

        // The ten consumed keys stay out of the filtered config.
        let parsed = AppConfig::parse(&pmc.get_raw(b"app_config")).unwrap();
        assert!(matches!(
            parsed.config.as_ref(),
            telegram_api::JSONValue::jsonObject(object) if object.value.is_empty()
        ));
    }

    #[test]
    fn content_settings_getter_answers_immediately_unauthorized() {
        let mut manager = ConfigManager::new(b"");
        let actions = manager.get_content_settings(
            Some((7, "ignore_sensitive_content_restrictions".to_string())),
            false,
            false,
            false,
        );
        assert!(actions.queries.is_empty());
        assert_eq!(
            actions.get_option_answers,
            vec![(7, "ignore_sensitive_content_restrictions".to_string())]
        );
    }

    #[test]
    fn content_settings_result_applies_options_and_reloads_app_config() {
        let mut options = options();
        let mut manager = ConfigManager::new(b"");
        let actions = manager.get_content_settings(
            Some((7, "ignore_sensitive_content_restrictions".to_string())),
            true,
            false,
            false,
        );
        assert_eq!(actions.queries, vec![ConfigQuery::GetContentSettings]);
        // A second getter coalesces into the in-flight query.
        let actions = manager.get_content_settings(
            Some((8, "can_ignore_sensitive_content_restrictions".to_string())),
            true,
            false,
            false,
        );
        assert!(actions.queries.is_empty());

        let actions = manager.on_get_content_settings_result(
            Ok(telegram_api::account_ContentSettings {
                flags: 0,
                sensitive_enabled: true,
                sensitive_can_change: true,
            }),
            &mut options,
            false,
            false,
        );
        assert!(options.get_option_boolean("can_ignore_sensitive_content_restrictions", false));
        assert!(options.get_option_boolean("ignore_sensitive_content_restrictions", false));
        assert_eq!(
            actions.get_option_answers,
            vec![
                (7, "ignore_sensitive_content_restrictions".to_string()),
                (8, "can_ignore_sensitive_content_restrictions".to_string()),
            ]
        );
        // do_set_ignore → reload_app_config.
        assert_eq!(actions.queries, vec![ConfigQuery::GetAppConfig { hash: 0 }]);
    }

    #[test]
    fn set_content_settings_ping_pong_answers_both_values() {
        let mut options = options();
        options.set_option_boolean("can_ignore_sensitive_content_restrictions", true);
        let mut manager = ConfigManager::new(b"");
        let actions = manager.set_content_settings(true, Some(1), false);
        assert_eq!(
            actions.queries,
            vec![ConfigQuery::SetContentSettings {
                sensitive_enabled: true,
            }]
        );
        // One query in flight: the opposite value queues without a query
        // and becomes `last_set_content_settings` (the C++ sets it on every
        // call, so the true result below does not apply the option).
        assert!(
            manager
                .set_content_settings(false, Some(2), false)
                .queries
                .is_empty()
        );

        let actions =
            manager.on_set_content_settings_result(Ok(()), true, &mut options, false, false);
        assert_eq!(actions.ok_ids, vec![1]);
        assert!(!options.have_option("ignore_sensitive_content_restrictions"));
        assert_eq!(
            actions.queries,
            vec![ConfigQuery::SetContentSettings {
                sensitive_enabled: false,
            }]
        );

        let actions =
            manager.on_set_content_settings_result(Ok(()), false, &mut options, false, false);
        assert_eq!(actions.ok_ids, vec![2]);
        assert!(!options.get_option_boolean("ignore_sensitive_content_restrictions", true));
        // do_set_ignore → reload_app_config.
        assert_eq!(actions.queries, vec![ConfigQuery::GetAppConfig { hash: 0 }]);
    }

    #[test]
    fn set_content_settings_error_fails_the_queue() {
        let mut options = options();
        let mut manager = ConfigManager::new(b"");
        manager.set_content_settings(true, Some(1), false);
        let actions = manager.on_set_content_settings_result(
            Err(Status::error(500, "X")),
            true,
            &mut options,
            false,
            false,
        );
        assert_eq!(actions.errors.len(), 1);
        assert_eq!(actions.errors[0].0, 1);
        assert_eq!(actions.errors[0].1.code(), 500);
        assert_eq!(actions.errors[0].1.message(), "X");
    }

    #[test]
    fn dc_options_update_persists_and_replays() {
        let mut pmc = TempPmc::new("dc-options");
        let mut manager = ConfigManager::new(b"");
        let option = telegram_api::DcOption {
            flags: 0,
            ipv6: false,
            media_only: false,
            tcpo_only: true,
            cdn: false,
            r#static: true,
            this_port_only: false,
            id: 2,
            ip_address: "149.154.175.50".to_string(),
            port: 443,
            secret: Vec::new(),
        };
        manager.on_dc_options_update(&mut pmc, std::slice::from_ref(&option));
        let stored = StoredDcOptions::parse(&pmc.get_raw(b"dc_options_update")).unwrap();
        assert_eq!(stored.options.len(), 1);
        assert_eq!(
            stored.options[0].flags,
            DC_OPTION_OBFUSCATED_TCP_ONLY | DC_OPTION_STATIC
        );
        assert_eq!(stored.options[0].raw_dc_id, 2);
        assert_eq!(stored.options[0].ip, b"149.154.175.50".to_vec());
        assert_eq!(stored.options[0].port, 443);
        assert!(stored.options[0].secret.is_empty());

        // A CDN option keeps the plain raw id — the `Cdn` flag carries the
        // externality (`DcId::get_raw_id()` returns the id itself, and the
        // C++ parser rebuilds `DcId::external` from the flag).
        manager.on_dc_options_update(
            &mut pmc,
            &[telegram_api::DcOption {
                cdn: true,
                id: 3,
                ..option.clone()
            }],
        );
        let stored = StoredDcOptions::parse(&pmc.get_raw(b"dc_options_update")).unwrap();
        assert_eq!(stored.options[0].raw_dc_id, 3);
        assert_ne!(stored.options[0].flags & DC_OPTION_CDN, 0);

        // The dc options update expires the config immediately.
        assert!(manager.expire_time() <= tdutils::time::now());
        assert!(pmc.get("config_expire").parse::<i32>().is_ok());

        // An empty update clears the pmc.
        manager.on_dc_options_update(&mut pmc, &[]);
        assert!(pmc.get_raw(b"dc_options_update").is_empty());
    }

    #[test]
    fn json_value_helpers_default_on_wrong_types() {
        let boolean = telegram_api::JSONValue::jsonBool(telegram_api::jsonBool { value: true });
        assert!(get_json_value_bool(&boolean, "x"));
        assert_eq!(get_json_value_int(&boolean, "x"), 0);
        assert_eq!(get_json_value_long(&boolean, "x"), 0);
        assert_eq!(get_json_value_double(&boolean, "x"), 0.0);
        assert_eq!(get_json_value_string(&boolean, "x"), "");

        let number = telegram_api::JSONValue::jsonNumber(telegram_api::jsonNumber { value: 3.5 });
        assert!(!get_json_value_bool(&number, "x"));
        assert_eq!(get_json_value_int(&number, "x"), 3);
        assert_eq!(get_json_value_long(&number, "x"), 3);
        assert_eq!(get_json_value_double(&number, "x"), 3.5);
        assert_eq!(get_json_value_string(&number, "x"), "");

        let string = telegram_api::JSONValue::jsonString(telegram_api::jsonString {
            value: "7".to_string(),
        });
        assert_eq!(get_json_value_long(&string, "x"), 7);
        assert_eq!(get_json_value_string(&string, "x"), "7");
        assert_eq!(
            get_json_value_long(
                &telegram_api::JSONValue::jsonString(telegram_api::jsonString {
                    value: "bad".to_string(),
                }),
                "x"
            ),
            0
        );
    }

    #[test]
    fn convert_json_value_object_preserves_the_shape() {
        let source = json_object(vec![
            ("a".to_string(), json_number(1.5)),
            ("b".to_string(), json_string("x")),
            (
                "c".to_string(),
                Box::new(telegram_api::JSONValue::jsonArray(
                    telegram_api::jsonArray {
                        value: vec![json_bool(true)],
                    },
                )),
            ),
        ]);
        let converted = convert_json_value_object(&source);
        let td_api::JsonValue::jsonValueObject(object) = converted else {
            panic!("not an object");
        };
        assert_eq!(object.members.len(), 3);
        let keys: Vec<&str> = object.members.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn config_query_serialization_round_trips() {
        let queries = vec![
            ConfigQuery::GetConfig {
                dc_id: 1,
                reopen_sessions: false,
            },
            ConfigQuery::GetAppConfig { hash: 5 },
            ConfigQuery::GetContentSettings,
            ConfigQuery::SetContentSettings {
                sensitive_enabled: true,
            },
        ];
        for query in queries {
            let bytes = query.serialize();
            let id = i32::from_le_bytes(bytes[0..4].try_into().unwrap());
            match &query {
                ConfigQuery::GetConfig { .. } => {
                    // help.getConfig has no fields — the body is the id only.
                    assert_eq!(id, telegram_api::help_getConfig::ID);
                    assert_eq!(bytes.len(), 4);
                }
                ConfigQuery::GetAppConfig { hash } => {
                    assert_eq!(id, telegram_api::help_getAppConfig::ID);
                    let mut reader = telegram_api::tl::Reader::new(&bytes[4..]);
                    let parsed = telegram_api::help_getAppConfig::read_bare(&mut reader).unwrap();
                    assert_eq!(parsed.hash, *hash);
                }
                ConfigQuery::GetContentSettings => {
                    // account.getContentSettings has no fields either.
                    assert_eq!(id, telegram_api::account_getContentSettings::ID);
                    assert_eq!(bytes.len(), 4);
                }
                ConfigQuery::SetContentSettings { sensitive_enabled } => {
                    assert_eq!(id, telegram_api::account_setContentSettings::ID);
                    let mut reader = telegram_api::tl::Reader::new(&bytes[4..]);
                    let parsed =
                        telegram_api::account_setContentSettings::read_bare(&mut reader).unwrap();
                    assert_eq!(parsed.sensitive_enabled, *sensitive_enabled);
                }
            }
        }
    }

    #[test]
    fn reaction_type_string_matches_the_cpp() {
        use telegram_api::Reaction;
        assert_eq!(
            reaction_type_string(&Reaction::reactionEmpty(telegram_api::reactionEmpty)),
            ""
        );
        assert_eq!(
            reaction_type_string(&Reaction::reactionEmoji(telegram_api::reactionEmoji {
                emoticon: "👍".to_string(),
            })),
            "👍"
        );
        // An emoji reaction that is itself custom or paid clears to empty.
        assert_eq!(
            reaction_type_string(&Reaction::reactionEmoji(telegram_api::reactionEmoji {
                emoticon: "$".to_string(),
            })),
            ""
        );
        assert_eq!(
            reaction_type_string(&Reaction::reactionCustomEmoji(
                telegram_api::reactionCustomEmoji { document_id: 42 }
            )),
            "custom:42"
        );
        assert_eq!(
            reaction_type_string(&Reaction::reactionPaid(telegram_api::reactionPaid)),
            "$"
        );
    }

    #[test]
    fn is_base64url_matches_the_cpp_checks() {
        assert!(is_base64url(b""));
        assert!(is_base64url(b"QmFzZTY0"));
        assert!(is_base64url(b"aA"));
        assert!(!is_base64url(b"ab"));
        assert!(!is_base64url(b"ab="));
        assert!(!is_base64url(b"a+b"));
        assert!(!is_base64url(b"a/b"));
    }

    /// A real `help.getAppConfig` answer, recorded 2026-09-15 from the
    /// Telegram test DC 149.154.175.10 over the S66 scratch session (232
    /// keys, hash 1231804985). The M.3.4 tests hand-build tiny objects;
    /// this golden pins the exact option store the recorded bytes produce.
    /// The FULL store is pinned, not a diff: about fifty recorded values
    /// equal the `OptionManager::new` pre-seeded defaults, and a diff would
    /// silently hide those writes.
    #[test]
    fn recorded_app_config_golden_snapshot() {
        let mut pmc = TempPmc::new("recorded-app-config");
        let mut options = options();
        let mut manager = ConfigManager::new(b"");
        manager.get_app_config(7, false, false);
        let telegram_api::help_AppConfig::help_appConfig(answer) =
            telegram_api::help_AppConfig::parse(include_bytes!(
                "testdata/app_config_test_dc_2026-09-15.bin"
            ))
            .unwrap()
        else {
            panic!("not help.appConfig");
        };
        assert_eq!(answer.hash, 1231804985);
        let actions = manager.on_get_app_config_result(
            &mut pmc,
            Ok(AppConfigQueryResult::Config {
                hash: answer.hash,
                config: answer
                    .config
                    .expect("help.appConfig always carries a config"),
            }),
            false,
            &mut options,
            false,
            false,
        );
        assert!(actions.refresh_premium_options);
        assert_eq!(actions.app_config_answers.len(), 1);

        // Hand-checked anchors against the recorded JSON (the full table
        // below carries the rest): the quote limit rename, the ton→cent
        // division (10000000 nanogram / 10000000 = 1 cent), the usd rate
        // scaling (1.319874 * 1e6), the dice aggregate and the cleaned
        // Fragment prefix.
        assert_eq!(
            options.get_option("message_reply_quote_length_max"),
            "I1024"
        );
        assert_eq!(
            options.get_option("suggested_post_gram_cent_count_min"),
            "I1"
        );
        assert_eq!(options.get_option("million_gram_to_usd_rate"), "I1319874");
        assert_eq!(options.get_option("dice_emojis"), "S🎲");
        assert_eq!(options.get_option("fragment_prefixes"), "S888");

        let expected: Vec<(String, String)> =
            include_str!("testdata/app_config_test_dc_2026-09-15.snapshot")
                .lines()
                .map(|line| {
                    let (name, value) = line.split_once('\t').expect("tab-separated snapshot");
                    (name.to_string(), value.to_string())
                })
                // The `utc_time_offset` seed is the machine-local
                // `Clocks::tz_offset()` (OptionManager.cpp:80-83); the
                // snapshot pins the recorder machine's UTC 0. It is masked
                // on both sides — the C++ itself produces a machine-dependent
                // value here, and the recorded config never sets the option.
                .filter(|(name, _)| name != "utc_time_offset")
                .collect();
        let actual: Vec<(String, String)> = options
            .all_options()
            .into_iter()
            .filter(|(name, _)| name != "utc_time_offset")
            .collect();
        assert_eq!(actual, expected);

        // The filtered config persists with the hash — every consumed key
        // is stripped; only the pass-through keys survive (the C++
        // `new_values` reassignment).
        let stored = AppConfig::parse(&pmc.get_raw(b"app_config")).unwrap();
        assert_eq!(stored.version, APP_CONFIG_CURRENT_VERSION);
        assert_eq!(stored.hash, 1231804985);
        let telegram_api::JSONValue::jsonObject(object) = stored.config.as_ref() else {
            panic!("stored config not an object");
        };
        let mut keys: Vec<&str> = object.value.iter().map(|kv| kv.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "ios_disable_parallel_channel_reset",
                "large_queue_max_active_operations_count",
                "lite_app_options",
                "lite_battery_min_pct",
                "lite_device_class",
                "premium_playmarket_direct_currency_list",
                "rich_message_posting",
                "small_queue_max_active_operations_count",
                "stars_spend_topup_invoice_disabled",
                "stickers_emoji_suggest_only_api",
                "stories_all_hidden",
                "translations_auto_enabled",
                "translations_manual_enabled",
                "youtube_pip",
            ]
        );
        // The td_api answer carries the same filtered object.
        let td_api::JsonValue::jsonValueObject(answer_object) = &actions.app_config_answers[0].1
        else {
            panic!("answer not an object");
        };
        assert_eq!(answer_object.members.len(), 14);
    }

    /// The recorded store drives the version-bump and NotModified paths:
    /// a current-version store keeps its hash (conditional query, served
    /// from the store on NotModified), an outdated stored version resets
    /// it (unconditional query, NotModified fails like before the first
    /// config).
    #[test]
    fn recorded_app_config_version_bump_refetches() {
        let mut pmc = TempPmc::new("recorded-version-bump");
        let mut options = options();
        let mut manager = ConfigManager::new(b"");
        let telegram_api::help_AppConfig::help_appConfig(answer) =
            telegram_api::help_AppConfig::parse(include_bytes!(
                "testdata/app_config_test_dc_2026-09-15.bin"
            ))
            .unwrap()
        else {
            panic!("not help.appConfig");
        };
        manager.on_get_app_config_result(
            &mut pmc,
            Ok(AppConfigQueryResult::Config {
                hash: answer.hash,
                config: answer
                    .config
                    .expect("help.appConfig always carries a config"),
            }),
            false,
            &mut options,
            false,
            false,
        );
        let stored = pmc.get_raw(b"app_config");

        // The current version preserves the hash — a conditional query.
        let mut current = ConfigManager::new(&stored);
        let actions = current.get_app_config(9, false, false);
        assert_eq!(
            actions.queries,
            vec![ConfigQuery::GetAppConfig { hash: 1231804985 }]
        );
        let actions = current.on_get_app_config_result(
            &mut BinlogKeyValue::new(),
            Ok(AppConfigQueryResult::NotModified),
            false,
            &mut options,
            false,
            false,
        );
        assert!(actions.queries.is_empty());
        assert_eq!(actions.app_config_answers.len(), 1);
        let td_api::JsonValue::jsonValueObject(object) = &actions.app_config_answers[0].1 else {
            panic!("answer not an object");
        };
        assert_eq!(object.members.len(), 14);

        // A version bump (the stored layout is one commit behind) resets
        // the store — the query goes out with hash 0 and a NotModified
        // answer fails with the C++ 500.
        let mut outdated = stored.clone();
        outdated[0..4].copy_from_slice(&(APP_CONFIG_CURRENT_VERSION - 1).to_le_bytes());
        let mut bumped = ConfigManager::new(&outdated);
        let actions = bumped.get_app_config(10, false, false);
        assert_eq!(actions.queries, vec![ConfigQuery::GetAppConfig { hash: 0 }]);
        let actions = bumped.on_get_app_config_result(
            &mut BinlogKeyValue::new(),
            Ok(AppConfigQueryResult::NotModified),
            false,
            &mut options,
            false,
            false,
        );
        assert_eq!(actions.errors.len(), 1);
        assert_eq!(actions.errors[0].0, 10);
        assert_eq!(actions.errors[0].1.message(), "Receive unexpected response");
    }
}
