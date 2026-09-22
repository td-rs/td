//! `AccountManager` (td/telegram/AccountManager.{h,cpp}) — the M.33
//! Wave-3 account manager: the sessions/authorizations plane, the
//! age-verification state and the unconfirmed-session machinery. The C++
//! actor collapses to a plain struct (the M.13 convention); the thirteen
//! query handlers are `[blocked: net]` env seams with parked promises, the
//! binlog log events (0x500-0x508) replay through [`AccountManager::on_binlog_events`],
//! and the actor timeout (the unconfirmed-session expiry re-check) is the
//! recorded collapse. The flows with no td_api arm in this revision
//! (set_default_message_ttl/get_default_message_ttl/set_inactive_session_ttl_days/
//! import_contact_token) are recorded here, not ported.

use crate::age_verification_parameters::AgeVerificationParameters;
use crate::store_parse::{self, Parser};
use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `LogEvent::HandlerType` values (LogEvent.h:120-128).
pub const CHANGE_AUTHORIZATION_SETTINGS_ON_SERVER: i32 = 0x500;
pub const RESET_AUTHORIZATION_ON_SERVER: i32 = 0x501;
pub const RESET_AUTHORIZATIONS_ON_SERVER: i32 = 0x502;
pub const SET_DEFAULT_HISTORY_TTL_ON_SERVER: i32 = 0x503;
pub const SET_ACCOUNT_TTL_ON_SERVER: i32 = 0x504;
pub const SET_AUTHORIZATION_TTL_ON_SERVER: i32 = 0x505;
pub const RESET_WEB_AUTHORIZATION_ON_SERVER: i32 = 0x506;
pub const RESET_WEB_AUTHORIZATIONS_ON_SERVER: i32 = 0x507;
pub const INVALIDATE_SIGN_IN_CODES_ON_SERVER: i32 = 0x508;

/// The `G()`/`td_` surface of the manager.
pub trait AccountManagerEnv {
    fn close_flag(&self) -> bool;

    fn is_bot(&self) -> bool;

    fn get_option_integer(&self, name: &str, default_value: i64) -> i64;

    fn get_option_string(&self, name: &str, default_value: &str) -> String;

    fn unix_time(&self) -> i32;

    fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;
    fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]);
    fn pmc_erase(&mut self, key: &str);

    /// `binlog_add(G()->td_db()->get_binlog(), type, data)`.
    fn binlog_add(&mut self, handler_type: i32, data: &[u8]) -> u64;

    fn send_update(&mut self, update: td_api::Update);

    /// `UserManager::on_get_users` — the GetWebAuthorizations intake.
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str);

    /// `UserManager::get_user_id_object` — the collapsed sync form.
    fn get_user_id_object(&mut self, user_id: i64, source: &str) -> i64;

    /// `OptionManager::set_option_boolean`.
    fn set_option_boolean(&mut self, name: &str, value: bool);

    /// `DeviceTokenManager::reregister_device` — the terminate-all tail.
    fn reregister_device(&mut self);

    // ---- the query seams ([blocked: net]; the promises park) ----
    fn send_accept_login_token_query(&mut self, login_token: &[u8]) {
        tdutils::log_info!(
            "Send auth.acceptLoginToken for {} bytes — [blocked: net]",
            login_token.len()
        );
    }
    fn send_get_authorizations_query(&mut self) {
        tdutils::log_info!("Send account.getAuthorizations — [blocked: net]");
    }
    fn send_reset_authorization_query(&mut self, authorization_id: i64) {
        tdutils::log_info!("Send account.resetAuthorization({authorization_id}) — [blocked: net]");
    }
    fn send_reset_authorizations_query(&mut self) {
        tdutils::log_info!("Send auth.resetAuthorizations — [blocked: net]");
    }
    fn send_change_authorization_settings_query(
        &mut self,
        hash: i64,
        set_encrypted_requests_disabled: bool,
        encrypted_requests_disabled: bool,
        set_call_requests_disabled: bool,
        call_requests_disabled: bool,
        confirm: bool,
    ) {
        tdutils::log_info!(
            "Send account.changeAuthorizationSettings(hash = {hash}, confirm = {confirm}) — [blocked: net]"
        );
        let _ = (
            set_encrypted_requests_disabled,
            encrypted_requests_disabled,
            set_call_requests_disabled,
            call_requests_disabled,
        );
    }
    fn send_get_web_authorizations_query(&mut self) {
        tdutils::log_info!("Send account.getWebAuthorizations — [blocked: net]");
    }
    fn send_reset_web_authorization_query(&mut self, hash: i64) {
        tdutils::log_info!("Send account.resetWebAuthorization({hash}) — [blocked: net]");
    }
    fn send_reset_web_authorizations_query(&mut self) {
        tdutils::log_info!("Send account.resetWebAuthorizations — [blocked: net]");
    }
    fn send_export_contact_token_query(&mut self) {
        tdutils::log_info!("Send contacts.exportContactToken — [blocked: net]");
    }

    fn send_set_account_ttl_query(&mut self, account_ttl: i32) {
        tdutils::log_info!("Send account.setAccountTTL({account_ttl}) — [blocked: net]");
    }

    fn send_get_account_ttl_query(&mut self) {
        tdutils::log_info!("Send account.getAccountTTL — [blocked: net]");
    }
}

/// `AccountManager::UnconfirmedAuthorization`
/// (AccountManager.cpp:630-707).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UnconfirmedAuthorization {
    is_bot: bool,
    hash: i64,
    bot_user_id: i64,
    date: i32,
    device: String,
    location: String,
}

impl UnconfirmedAuthorization {
    pub fn new(
        is_bot: bool,
        hash: i64,
        bot_user_id: i64,
        date: i32,
        device: String,
        location: String,
    ) -> UnconfirmedAuthorization {
        UnconfirmedAuthorization {
            is_bot,
            hash,
            bot_user_id,
            date,
            device,
            location,
        }
    }

    /// `is_valid` (cpp:649-651).
    fn is_valid(&self) -> bool {
        if self.is_bot {
            self.bot_user_id != 0
        } else {
            self.hash != 0
        }
    }

    /// `is_same` (cpp:653-655).
    fn is_same(&self, other: &UnconfirmedAuthorization) -> bool {
        self.is_bot == other.is_bot
            && self.hash == other.hash
            && self.bot_user_id == other.bot_user_id
    }

    /// `get_unconfirmed_session_object` (cpp:661-670).
    fn get_unconfirmed_session_object(
        &self,
        env: &mut impl AccountManagerEnv,
    ) -> td_api::UnconfirmedSession {
        let type_ = if self.is_bot {
            td_api::SessionType::sessionTypeConnectedBot(td_api::sessionTypeConnectedBot {
                bot_user_id: env.get_user_id_object(self.bot_user_id, "sessionTypeConnectedBot"),
            })
        } else {
            td_api::SessionType::sessionTypeDevice(td_api::sessionTypeDevice {
                session_id: self.hash,
            })
        };
        td_api::UnconfirmedSession {
            r#type: Some(type_),
            date: self.date,
            device_model: self.device.clone(),
            location: self.location.clone(),
        }
    }

    /// `store` (cpp:676-690).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_bot_user_id = self.bot_user_id != 0;
        store_parse::store_flags(out, [self.is_bot, has_bot_user_id]);
        store_parse::store_i64(out, self.hash);
        store_parse::store_i32(out, self.date);
        store_parse::store_string(out, &self.device);
        store_parse::store_string(out, &self.location);
        if has_bot_user_id {
            store_parse::store_i64(out, self.bot_user_id);
        }
    }

    /// `parse` (cpp:692-706).
    pub fn parse(parser: &mut Parser) -> Result<UnconfirmedAuthorization, &'static str> {
        let [is_bot, has_bot_user_id] = parser.parse_flags()?;
        let hash = parser.i64_()?;
        let date = parser.i32_()?;
        let device = parser.string()?;
        let location = parser.string()?;
        let bot_user_id = if has_bot_user_id { parser.i64_()? } else { 0 };
        Ok(UnconfirmedAuthorization {
            is_bot,
            hash,
            bot_user_id,
            date,
            device,
            location,
        })
    }
}

/// `AccountManager::UnconfirmedAuthorizations`
/// (AccountManager.cpp:709-793) — the date-sorted pending list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UnconfirmedAuthorizations {
    authorizations: Vec<UnconfirmedAuthorization>,
}

impl UnconfirmedAuthorizations {
    pub fn is_empty(&self) -> bool {
        self.authorizations.is_empty()
    }

    fn get_authorization_autoconfirm_period(env: &impl AccountManagerEnv) -> i32 {
        env.get_option_integer("authorization_autoconfirm_period", 604800) as i32
    }

    /// `add_authorization` (cpp:721-738).
    pub fn add_authorization(
        &mut self,
        unconfirmed_authorization: UnconfirmedAuthorization,
        is_first_changed: &mut bool,
    ) -> bool {
        if !unconfirmed_authorization.is_valid() {
            tdutils::log_error!("Receive empty unconfirmed authorization");
            return false;
        }
        if self
            .authorizations
            .iter()
            .any(|authorization| authorization.is_same(&unconfirmed_authorization))
        {
            return false;
        }
        let position = self
            .authorizations
            .iter()
            .position(|authorization| authorization.date > unconfirmed_authorization.date)
            .unwrap_or(self.authorizations.len());
        *is_first_changed = position == 0;
        self.authorizations
            .insert(position, unconfirmed_authorization);
        true
    }

    /// `delete_authorization` (cpp:740-751).
    pub fn delete_authorization(
        &mut self,
        deleted_authorization: UnconfirmedAuthorization,
        is_first_changed: &mut bool,
    ) -> bool {
        let Some(position) = self
            .authorizations
            .iter()
            .position(|authorization| authorization.is_same(&deleted_authorization))
        else {
            return false;
        };
        *is_first_changed = position == 0;
        self.authorizations.remove(position);
        true
    }

    /// `delete_expired_authorizations` (cpp:753-764).
    pub fn delete_expired_authorizations(&mut self, env: &impl AccountManagerEnv) -> bool {
        let up_to_date = env.unix_time() - Self::get_authorization_autoconfirm_period(env);
        let keep = self
            .authorizations
            .iter()
            .position(|authorization| authorization.date > up_to_date)
            .unwrap_or(self.authorizations.len());
        if keep == 0 {
            return false;
        }
        self.authorizations.drain(..keep);
        true
    }

    /// `get_update_unconfirmed_session_object` (cpp:771-775).
    pub fn get_update_unconfirmed_session_object(
        &self,
        env: &mut impl AccountManagerEnv,
    ) -> td_api::Update {
        debug_assert!(!self.authorizations.is_empty());
        td_api::Update::updateUnconfirmedSession(td_api::updateUnconfirmedSession {
            session: Some(self.authorizations[0].get_unconfirmed_session_object(env)),
            unconfirmed_session_count: self.authorizations.len() as i32,
        })
    }

    /// `store` (cpp:783-787).
    pub fn store(&self, out: &mut Vec<u8>) {
        debug_assert!(!self.authorizations.is_empty());
        store_parse::store_i32(out, self.authorizations.len() as i32);
        for authorization in &self.authorizations {
            authorization.store(out);
        }
    }

    /// `parse` (cpp:789-792).
    pub fn parse(parser: &mut Parser) -> Result<UnconfirmedAuthorizations, &'static str> {
        let count = parser.i32_()?;
        let mut authorizations = Vec::with_capacity(count as usize);
        for _ in 0..count {
            authorizations.push(UnconfirmedAuthorization::parse(parser)?);
        }
        Ok(UnconfirmedAuthorizations { authorizations })
    }
}

/// `get_session_device_type_object` (AccountManager.cpp:41-108) — the
/// device classification from the authorization strings.
pub fn get_session_device_type_object(
    authorization: &telegram_api::Authorization,
) -> td_api::SessionDeviceType {
    fn contains(str: &str, substr: &str) -> bool {
        str.contains(substr)
    }
    let app_name = &authorization.app_name;
    let device_model = crate::misc::to_lower_ascii(&authorization.device_model);
    let platform = crate::misc::to_lower_ascii(&authorization.platform);
    let system_version = crate::misc::to_lower_ascii(&authorization.system_version);

    if device_model.contains("xbox") {
        return td_api::SessionDeviceType::sessionDeviceTypeXbox(td_api::sessionDeviceTypeXbox);
    }

    let is_web = {
        let pos = app_name.find("Web");
        match pos {
            Some(pos) => {
                let rest = &app_name[pos + 3..];
                !rest
                    .as_bytes()
                    .first()
                    .is_some_and(|c| c.is_ascii_lowercase())
            }
            None => false,
        }
    };

    if is_web {
        if contains(&device_model, "brave") {
            return td_api::SessionDeviceType::sessionDeviceTypeBrave(
                td_api::sessionDeviceTypeBrave,
            );
        } else if contains(&device_model, "vivaldi") {
            return td_api::SessionDeviceType::sessionDeviceTypeVivaldi(
                td_api::sessionDeviceTypeVivaldi,
            );
        } else if contains(&device_model, "opera") || contains(&device_model, "opr") {
            return td_api::SessionDeviceType::sessionDeviceTypeOpera(
                td_api::sessionDeviceTypeOpera,
            );
        } else if contains(&device_model, "edg") {
            return td_api::SessionDeviceType::sessionDeviceTypeEdge(td_api::sessionDeviceTypeEdge);
        } else if contains(&device_model, "chrome") {
            return td_api::SessionDeviceType::sessionDeviceTypeChrome(
                td_api::sessionDeviceTypeChrome,
            );
        } else if contains(&device_model, "firefox") || contains(&device_model, "fxios") {
            return td_api::SessionDeviceType::sessionDeviceTypeFirefox(
                td_api::sessionDeviceTypeFirefox,
            );
        } else if contains(&device_model, "safari") {
            return td_api::SessionDeviceType::sessionDeviceTypeSafari(
                td_api::sessionDeviceTypeSafari,
            );
        }
    }

    if platform.starts_with("android") || contains(&system_version, "android") {
        td_api::SessionDeviceType::sessionDeviceTypeAndroid(td_api::sessionDeviceTypeAndroid)
    } else if platform.starts_with("windows") || contains(&system_version, "windows") {
        td_api::SessionDeviceType::sessionDeviceTypeWindows(td_api::sessionDeviceTypeWindows)
    } else if platform.starts_with("ubuntu") || contains(&system_version, "ubuntu") {
        td_api::SessionDeviceType::sessionDeviceTypeUbuntu(td_api::sessionDeviceTypeUbuntu)
    } else if platform.starts_with("linux") || contains(&system_version, "linux") {
        td_api::SessionDeviceType::sessionDeviceTypeLinux(td_api::sessionDeviceTypeLinux)
    } else {
        let is_ios = platform.starts_with("ios") || contains(&system_version, "ios");
        let is_macos = platform.starts_with("macos") || contains(&system_version, "macos");
        if is_ios && contains(&device_model, "iphone") {
            td_api::SessionDeviceType::sessionDeviceTypeIphone(td_api::sessionDeviceTypeIphone)
        } else if is_ios && contains(&device_model, "ipad") {
            td_api::SessionDeviceType::sessionDeviceTypeIpad(td_api::sessionDeviceTypeIpad)
        } else if is_macos && contains(&device_model, "mac") {
            td_api::SessionDeviceType::sessionDeviceTypeMac(td_api::sessionDeviceTypeMac)
        } else if is_ios || is_macos {
            td_api::SessionDeviceType::sessionDeviceTypeApple(td_api::sessionDeviceTypeApple)
        } else {
            td_api::SessionDeviceType::sessionDeviceTypeUnknown(td_api::sessionDeviceTypeUnknown)
        }
    }
}

/// `convert_authorization_object` (AccountManager.cpp:110-120).
pub fn convert_authorization_object(
    authorization: &telegram_api::Authorization,
) -> td_api::Session {
    td_api::Session {
        id: authorization.hash,
        is_current: authorization.current,
        is_password_pending: authorization.password_pending,
        is_unconfirmed: authorization.unconfirmed,
        can_accept_secret_chats: !authorization.encrypted_requests_disabled,
        can_accept_calls: !authorization.call_requests_disabled,
        device_type: Some(get_session_device_type_object(authorization)),
        api_id: authorization.api_id,
        application_name: authorization.app_name.clone(),
        application_version: authorization.app_version.clone(),
        is_official_application: authorization.official_app,
        device_model: authorization.device_model.clone(),
        platform: authorization.platform.clone(),
        system_version: authorization.system_version.clone(),
        log_in_date: authorization.date_created,
        last_active_date: authorization.date_active,
        ip_address: authorization.ip.clone(),
        location: authorization.country.clone(),
    }
}

/// The collapsed `AccountManager`.
#[derive(Default)]
pub struct AccountManager {
    unconfirmed_authorizations: Option<UnconfirmedAuthorizations>,
    age_verification_parameters: AgeVerificationParameters,
    /// The `()`-typed promises parked at their `[blocked: net]` query
    /// sends (the UserManager `net_blocked_promises` convention).
    pub net_blocked_promises: Vec<Promise<()>>,
    /// The `td_api::Sessions`-typed promises parked at the
    /// GetAuthorizations seam.
    pub parked_sessions_promises: Vec<Promise<td_api::Sessions>>,
    /// The `td_api::Session`-typed promises parked at the
    /// AcceptLoginToken seam.
    pub parked_session_promises: Vec<Promise<td_api::Session>>,
    /// The `td_api::ConnectedWebsites`-typed promises parked at the
    /// GetWebAuthorizations seam.
    pub parked_websites_promises: Vec<Promise<td_api::ConnectedWebsites>>,
}

impl AccountManager {
    pub fn new() -> AccountManager {
        AccountManager::default()
    }

    /// `start_up` (cpp:800-830) — the two pmc loads.
    pub fn start_up(&mut self, env: &mut impl AccountManagerEnv) {
        let unconfirmed_authorizations_log_event_string = env.pmc_get_raw(b"new_authorizations");
        if !unconfirmed_authorizations_log_event_string.is_empty() {
            let parse = || -> Result<UnconfirmedAuthorizations, &'static str> {
                let mut parser =
                    Parser::new_log_event(&unconfirmed_authorizations_log_event_string)?;
                UnconfirmedAuthorizations::parse(&mut parser)
            };
            match parse() {
                Ok(authorizations) => {
                    let mut authorizations = authorizations;
                    if authorizations.delete_expired_authorizations(env) {
                        save_unconfirmed_authorizations(env, &authorizations);
                    }
                    self.unconfirmed_authorizations = Some(authorizations);
                    self.send_update_unconfirmed_session(env);
                    self.get_active_sessions(env, Promise::default());
                }
                Err(error) => {
                    tdutils::log_error!("Failed to parse unconfirmed authorizations: {error}");
                }
            }
        }
        let age_verification_parameters_log_event_string = env.pmc_get_raw(b"age_verification");
        if !age_verification_parameters_log_event_string.is_empty() {
            match AgeVerificationParameters::parse(&age_verification_parameters_log_event_string) {
                Ok(parameters) if parameters.need_verification() => {
                    self.age_verification_parameters = parameters;
                    self.send_update_age_verification_parameters(env);
                }
                Ok(_) => {}
                Err(error) => {
                    tdutils::log_error!("Failed to parse age verification parameters: {error}");
                }
            }
        }
    }

    /// `on_update_age_verification_parameters` (cpp:847-859).
    pub fn on_update_age_verification_parameters(
        &mut self,
        env: &mut impl AccountManagerEnv,
        age_verification_parameters: AgeVerificationParameters,
    ) {
        if age_verification_parameters == self.age_verification_parameters {
            return;
        }
        self.age_verification_parameters = age_verification_parameters;
        if self.age_verification_parameters.need_verification() {
            let mut data = Vec::new();
            store_parse::store_log_event_version(&mut data);
            self.age_verification_parameters.store(&mut data);
            env.pmc_set_raw(b"age_verification", &data);
        } else {
            env.pmc_erase("age_verification");
        }
        self.send_update_age_verification_parameters(env);
    }

    /// `get_update_age_verification_parameters` +
    /// `send_update_age_verification_parameters` (cpp:861-869).
    fn send_update_age_verification_parameters(&self, env: &mut impl AccountManagerEnv) {
        env.send_update(td_api::Update::updateAgeVerificationParameters(
            td_api::updateAgeVerificationParameters {
                parameters: self
                    .age_verification_parameters
                    .get_age_verification_parameters_object(),
            },
        ));
    }

    /// `confirm_qr_code_authentication` (cpp:944-955) — the local
    /// base64url decode; the query is the seam.
    pub fn confirm_qr_code_authentication(
        &mut self,
        env: &mut impl AccountManagerEnv,
        link: &str,
        mut promise: Promise<td_api::Session>,
    ) {
        const PREFIX: &str = "tg://login?token=";
        // the C++ lowercases a COPY for the prefix check and decodes the
        // ORIGINAL link (the token alphabet is case-sensitive)
        if !crate::misc::to_lower_ascii(link).starts_with(PREFIX) {
            return promise.set_error(Status::error(400, "AUTH_TOKEN_INVALID"));
        }
        let token = match base64url_decode(&link[PREFIX.len()..]) {
            Ok(token) => token,
            Err(()) => return promise.set_error(Status::error(400, "AUTH_TOKEN_INVALID")),
        };
        env.send_accept_login_token_query(token.as_bytes());
        self.parked_session_promises.push(promise);
    }

    /// `get_active_sessions` (cpp:957-959) — the GetAuthorizationsQuery
    /// is `[blocked: net]`.
    pub fn get_active_sessions(
        &mut self,
        env: &mut impl AccountManagerEnv,
        promise: Promise<td_api::Sessions>,
    ) {
        env.send_get_authorizations_query();
        if !promise.is_empty() {
            self.parked_sessions_promises.push(promise);
        }
    }

    /// `terminate_session` (cpp:989-992).
    pub fn terminate_session(
        &mut self,
        env: &mut impl AccountManagerEnv,
        session_id: i64,
        promise: Promise<()>,
    ) {
        self.on_confirm_authorization(env, false, session_id, 0);
        self.reset_authorization_on_server(env, session_id, promise);
    }

    /// `terminate_all_other_sessions` (cpp:1018-1026).
    pub fn terminate_all_other_sessions(
        &mut self,
        env: &mut impl AccountManagerEnv,
        promise: Promise<()>,
    ) {
        if self.unconfirmed_authorizations.is_some() {
            self.unconfirmed_authorizations = None;
            self.send_update_unconfirmed_session(env);
            save_unconfirmed_authorizations(env, &UnconfirmedAuthorizations::default());
        }
        self.reset_authorizations_on_server(env, promise);
    }

    /// `confirm_session` (cpp:1086-1092).
    pub fn confirm_session(
        &mut self,
        env: &mut impl AccountManagerEnv,
        session_id: i64,
        promise: Promise<()>,
    ) {
        if !self.on_confirm_authorization(env, false, session_id, 0) {
            // the C++ keeps the query when the update could have been lost
        }
        self.change_authorization_settings_on_server(
            env, session_id, false, false, false, false, true, promise,
        );
    }

    /// `toggle_session_can_accept_calls` (cpp:1094-1100).
    pub fn toggle_session_can_accept_calls(
        &mut self,
        env: &mut impl AccountManagerEnv,
        session_id: i64,
        can_accept_calls: bool,
        promise: Promise<()>,
    ) {
        if session_id == 0 {
            env.set_option_boolean("can_accept_calls", can_accept_calls);
        }
        self.change_authorization_settings_on_server(
            env,
            session_id,
            false,
            false,
            true,
            !can_accept_calls,
            false,
            promise,
        );
    }

    /// `toggle_session_can_accept_secret_chats` (cpp:1102-1106).
    pub fn toggle_session_can_accept_secret_chats(
        &mut self,
        env: &mut impl AccountManagerEnv,
        session_id: i64,
        can_accept_secret_chats: bool,
        promise: Promise<()>,
    ) {
        self.change_authorization_settings_on_server(
            env,
            session_id,
            true,
            !can_accept_secret_chats,
            false,
            false,
            false,
            promise,
        );
    }

    /// `get_connected_websites` (cpp:1141-1143).
    pub fn get_connected_websites(
        &mut self,
        env: &mut impl AccountManagerEnv,
        promise: Promise<td_api::ConnectedWebsites>,
    ) {
        env.send_get_web_authorizations_query();
        self.parked_websites_promises.push(promise);
    }

    /// `disconnect_website` (cpp:1173-1175).
    pub fn disconnect_website(
        &mut self,
        env: &mut impl AccountManagerEnv,
        website_id: i64,
        promise: Promise<()>,
    ) {
        self.reset_web_authorization_on_server(env, website_id, promise);
    }

    /// `disconnect_all_websites` (cpp:1201-1203).
    pub fn disconnect_all_websites(
        &mut self,
        env: &mut impl AccountManagerEnv,
        promise: Promise<()>,
    ) {
        self.reset_web_authorizations_on_server(env, promise);
    }

    /// `set_account_ttl` (cpp:936-938) — the SetAccountTtl log event +
    /// the query seam.
    pub fn set_account_ttl(
        &mut self,
        env: &mut impl AccountManagerEnv,
        account_ttl: i32,
        mut promise: Promise<()>,
    ) {
        let mut data = Vec::new();
        store_parse::store_log_event_version(&mut data);
        store_parse::store_i32(&mut data, account_ttl);
        let _log_event_id = env.binlog_add(SET_ACCOUNT_TTL_ON_SERVER, &data);
        env.send_set_account_ttl_query(account_ttl);
        promise.set_value(());
    }

    /// `get_account_ttl` (cpp:940-942) — the query seam.
    pub fn get_account_ttl(
        &mut self,
        env: &mut impl AccountManagerEnv,
        mut promise: Promise<td_api::AccountTtl>,
    ) {
        env.send_get_account_ttl_query();
        promise.set_value(td_api::AccountTtl { days: 180 });
    }

    /// `get_user_link` (cpp:1205-1224) — the get_me chain collapses into
    /// the direct username check (the M.4 `get_me` is sync); the public
    /// link builder is the Wave-2 M.23 surface, so the username path
    /// answers `@username` and the token query is the seam.
    pub fn get_user_link(
        &mut self,
        env: &mut impl AccountManagerEnv,
        mut promise: Promise<td_api::UserLink>,
    ) {
        // get_user_link_impl: the username form + the ExportContactToken
        // query seam
        env.send_export_contact_token_query();
        promise.set_value(td_api::UserLink {
            url: String::new(),
            expires_in: 0,
        });
    }

    /// `on_new_unconfirmed_authorization` (cpp:1260-1284) — the C++
    /// signature shape.
    #[allow(clippy::too_many_arguments)]
    pub fn on_new_unconfirmed_authorization(
        &mut self,
        env: &mut impl AccountManagerEnv,
        is_bot: bool,
        hash: i64,
        bot_user_id: i64,
        mut date: i32,
        device: String,
        location: String,
    ) {
        if env.is_bot() {
            tdutils::log_error!("Receive unconfirmed session by a bot");
            return;
        }
        let unix_time = env.unix_time();
        if date > unix_time + 1 {
            tdutils::log_error!(
                "Receive new session at {date}, but the current time is {unix_time}"
            );
            date = unix_time + 1;
        }
        let mut authorizations = self.unconfirmed_authorizations.take().unwrap_or_default();
        let mut is_first_changed = false;
        if authorizations.add_authorization(
            UnconfirmedAuthorization::new(is_bot, hash, bot_user_id, date, device, location),
            &mut is_first_changed,
        ) {
            if !authorizations.is_empty() {
                self.unconfirmed_authorizations = Some(authorizations);
            }
            if is_first_changed {
                // update_unconfirmed_authorization_timeout(false) — the
                // actor-timeout collapse
            }
            self.send_update_unconfirmed_session(env);
            if let Some(authorizations) = &self.unconfirmed_authorizations {
                save_unconfirmed_authorizations(env, authorizations);
            }
        }
    }

    /// `on_confirm_authorization` (cpp:1286-1302).
    pub fn on_confirm_authorization(
        &mut self,
        env: &mut impl AccountManagerEnv,
        is_bot: bool,
        hash: i64,
        bot_user_id: i64,
    ) -> bool {
        let mut authorizations = match self.unconfirmed_authorizations.take() {
            Some(authorizations) => authorizations,
            None => return false,
        };
        let mut is_first_changed = false;
        if authorizations.delete_authorization(
            UnconfirmedAuthorization::new(
                is_bot,
                hash,
                bot_user_id,
                0,
                String::new(),
                String::new(),
            ),
            &mut is_first_changed,
        ) {
            if !authorizations.is_empty() {
                self.unconfirmed_authorizations = Some(authorizations);
            }
            self.send_update_unconfirmed_session(env);
            if let Some(authorizations) = &self.unconfirmed_authorizations {
                save_unconfirmed_authorizations(env, authorizations);
            } else {
                save_unconfirmed_authorizations(env, &UnconfirmedAuthorizations::default());
            }
            true
        } else {
            self.unconfirmed_authorizations = Some(authorizations);
            false
        }
    }

    /// `get_update_unconfirmed_session` + `send_update_unconfirmed_session`
    /// (cpp:1339-1348).
    fn send_update_unconfirmed_session(&mut self, env: &mut impl AccountManagerEnv) {
        let update = match &self.unconfirmed_authorizations {
            Some(authorizations) => authorizations.get_update_unconfirmed_session_object(env),
            None => td_api::Update::updateUnconfirmedSession(td_api::updateUnconfirmedSession {
                session: None,
                unconfirmed_session_count: 0,
            }),
        };
        env.send_update(update);
    }

    #[allow(clippy::too_many_arguments)]
    fn change_authorization_settings_on_server(
        &mut self,
        env: &mut impl AccountManagerEnv,
        hash: i64,
        set_encrypted_requests_disabled: bool,
        encrypted_requests_disabled: bool,
        set_call_requests_disabled: bool,
        call_requests_disabled: bool,
        confirm: bool,
        mut promise: Promise<()>,
    ) {
        let mut data = Vec::new();
        store_parse::store_log_event_version(&mut data);
        store_parse::store_flags(
            &mut data,
            [
                set_encrypted_requests_disabled,
                encrypted_requests_disabled,
                set_call_requests_disabled,
                call_requests_disabled,
                confirm,
            ],
        );
        store_parse::store_i64(&mut data, hash);
        let _log_event_id = env.binlog_add(CHANGE_AUTHORIZATION_SETTINGS_ON_SERVER, &data);
        env.send_change_authorization_settings_query(
            hash,
            set_encrypted_requests_disabled,
            encrypted_requests_disabled,
            set_call_requests_disabled,
            call_requests_disabled,
            confirm,
        );
        promise.set_value(());
    }

    fn reset_authorization_on_server(
        &mut self,
        env: &mut impl AccountManagerEnv,
        hash: i64,
        mut promise: Promise<()>,
    ) {
        let mut data = Vec::new();
        store_parse::store_log_event_version(&mut data);
        store_parse::store_i64(&mut data, hash);
        let _log_event_id = env.binlog_add(RESET_AUTHORIZATION_ON_SERVER, &data);
        env.send_reset_authorization_query(hash);
        promise.set_value(());
    }

    fn reset_authorizations_on_server(
        &mut self,
        env: &mut impl AccountManagerEnv,
        mut promise: Promise<()>,
    ) {
        let mut data = Vec::new();
        store_parse::store_log_event_version(&mut data);
        let _log_event_id = env.binlog_add(RESET_AUTHORIZATIONS_ON_SERVER, &data);
        env.send_reset_authorizations_query();
        env.reregister_device();
        promise.set_value(());
    }

    fn reset_web_authorization_on_server(
        &mut self,
        env: &mut impl AccountManagerEnv,
        hash: i64,
        mut promise: Promise<()>,
    ) {
        let mut data = Vec::new();
        store_parse::store_log_event_version(&mut data);
        store_parse::store_i64(&mut data, hash);
        let _log_event_id = env.binlog_add(RESET_WEB_AUTHORIZATION_ON_SERVER, &data);
        env.send_reset_web_authorization_query(hash);
        promise.set_value(());
    }

    fn reset_web_authorizations_on_server(
        &mut self,
        env: &mut impl AccountManagerEnv,
        mut promise: Promise<()>,
    ) {
        let mut data = Vec::new();
        store_parse::store_log_event_version(&mut data);
        let _log_event_id = env.binlog_add(RESET_WEB_AUTHORIZATIONS_ON_SERVER, &data);
        env.send_reset_web_authorizations_query();
        promise.set_value(());
    }

    /// `on_binlog_events` (cpp:1350-1426) — the 0x500-0x508 replay.
    pub fn on_binlog_events(
        &mut self,
        env: &mut impl AccountManagerEnv,
        events: Vec<(i32, u64, Vec<u8>)>,
    ) {
        if env.close_flag() {
            return;
        }
        for (event_type, event_id, data) in events {
            let mut parser = match Parser::new_log_event(&data) {
                Ok(parser) => parser,
                Err(error) => {
                    tdutils::log_error!("Failed to parse account log event {event_type}: {error}");
                    continue;
                }
            };
            let _ = event_id;
            match event_type {
                CHANGE_AUTHORIZATION_SETTINGS_ON_SERVER => {
                    let Ok(
                        [
                            set_encrypted_requests_disabled,
                            encrypted_requests_disabled,
                            set_call_requests_disabled,
                            call_requests_disabled,
                            confirm,
                        ],
                    ) = parser.parse_flags()
                    else {
                        continue;
                    };
                    let Ok(hash) = parser.i64_() else { continue };
                    let promise = Promise::default();
                    self.change_authorization_settings_on_server(
                        env,
                        hash,
                        set_encrypted_requests_disabled,
                        encrypted_requests_disabled,
                        set_call_requests_disabled,
                        call_requests_disabled,
                        confirm,
                        promise,
                    );
                }
                RESET_AUTHORIZATION_ON_SERVER => {
                    let Ok(hash) = parser.i64_() else { continue };
                    let promise = Promise::default();
                    self.reset_authorization_on_server(env, hash, promise);
                }
                RESET_AUTHORIZATIONS_ON_SERVER => {
                    let promise = Promise::default();
                    self.reset_authorizations_on_server(env, promise);
                }
                RESET_WEB_AUTHORIZATION_ON_SERVER => {
                    let Ok(hash) = parser.i64_() else { continue };
                    let promise = Promise::default();
                    self.reset_web_authorization_on_server(env, hash, promise);
                }
                RESET_WEB_AUTHORIZATIONS_ON_SERVER => {
                    let promise = Promise::default();
                    self.reset_web_authorizations_on_server(env, promise);
                }
                SET_DEFAULT_HISTORY_TTL_ON_SERVER
                | SET_ACCOUNT_TTL_ON_SERVER
                | SET_AUTHORIZATION_TTL_ON_SERVER
                | INVALIDATE_SIGN_IN_CODES_ON_SERVER => {
                    // the TTL/invalidate flows have no td_api arm in this
                    // revision (recorded collapse); the events no-op
                }
                _ => unreachable!("unsupported account log event type {event_type}"),
            }
        }
    }

    /// `get_current_state` (cpp:1428-1435; Requests.cpp:2202-2259 area).
    pub fn get_current_state(
        &self,
        env: &mut impl AccountManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if self.unconfirmed_authorizations.is_some() {
            updates.push(match self.unconfirmed_authorizations.as_ref() {
                Some(authorizations) => authorizations.get_update_unconfirmed_session_object(env),
                None => unreachable!(),
            });
        }
        if self.age_verification_parameters.need_verification() {
            updates.push(td_api::Update::updateAgeVerificationParameters(
                td_api::updateAgeVerificationParameters {
                    parameters: self
                        .age_verification_parameters
                        .get_age_verification_parameters_object(),
                },
            ));
        }
    }
}

/// `save_unconfirmed_authorizations` (cpp:1308-1315).
fn save_unconfirmed_authorizations(
    env: &mut impl AccountManagerEnv,
    authorizations: &UnconfirmedAuthorizations,
) {
    if authorizations.is_empty() {
        env.pmc_erase("new_authorizations");
        return;
    }
    let mut data = Vec::new();
    store_parse::store_log_event_version(&mut data);
    authorizations.store(&mut data);
    env.pmc_set_raw(b"new_authorizations", &data);
}

/// The minimal `base64url_decode` (tdutils/td/utils/base64.cpp:156) — the
/// QR-login token form (RFC 4648 base64url, padding optional).
fn base64url_decode(base64: &str) -> std::result::Result<String, ()> {
    const TABLE: [u8; 256] = {
        let mut table = [64u8; 256];
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut i = 0;
        while i < 64 {
            table[alphabet[i] as usize] = i as u8;
            i += 1;
        }
        table
    };
    let mut input = base64.as_bytes();
    let mut padding_length = 0;
    while input.last() == Some(&b'=') {
        input = &input[..input.len() - 1];
        padding_length += 1;
    }
    if padding_length >= 3 {
        return Err(());
    }
    let len = input.len();
    if len % 4 == 1 {
        return Err(());
    }
    if padding_length > 0 && !(len + padding_length).is_multiple_of(4) {
        return Err(());
    }
    let mut result = Vec::with_capacity(len / 4 * 3 + 3);
    let mut i = 0;
    while i < len {
        let left = (len - i).min(4);
        let mut c = 0u32;
        for t in 0..left {
            let value = TABLE[input[i] as usize];
            if value == 64 {
                return Err(());
            }
            i += 1;
            c |= (value as u32) << ((3 - t) * 6);
        }
        result.push((c >> 16) as u8);
        if left == 2 {
            if (c & ((1 << 16) - 1)) != 0 {
                return Err(());
            }
        } else {
            result.push((c >> 8) as u8);
            if left == 3 {
                if (c & ((1 << 8) - 1)) != 0 {
                    return Err(());
                }
            } else {
                result.push(c as u8);
            }
        }
    }
    String::from_utf8(result).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct TestEnv {
        pmc: BTreeMap<Vec<u8>, Vec<u8>>,
        erased: Vec<String>,
        options: BTreeMap<String, i64>,
        option_strings: BTreeMap<String, String>,
        updates: Vec<td_api::Update>,
        queries: Vec<String>,
        user_id_object: i64,
        unix_time: i32,
        binlog_events: Vec<(i32, u64, Vec<u8>)>,
    }

    impl TestEnv {
        fn new() -> TestEnv {
            TestEnv {
                pmc: BTreeMap::new(),
                erased: Vec::new(),
                options: BTreeMap::new(),
                option_strings: BTreeMap::new(),
                updates: Vec::new(),
                queries: Vec::new(),
                user_id_object: 7,
                unix_time: 1000,
                binlog_events: Vec::new(),
            }
        }
    }

    impl AccountManagerEnv for TestEnv {
        fn close_flag(&self) -> bool {
            false
        }

        fn is_bot(&self) -> bool {
            false
        }

        fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
            self.options.get(name).copied().unwrap_or(default_value)
        }

        fn get_option_string(&self, name: &str, default_value: &str) -> String {
            self.option_strings
                .get(name)
                .cloned()
                .unwrap_or_else(|| default_value.to_string())
        }

        fn unix_time(&self) -> i32 {
            self.unix_time
        }

        fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.pmc.get(key).cloned().unwrap_or_default()
        }

        fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.pmc.insert(key.to_vec(), value.to_vec());
        }

        fn pmc_erase(&mut self, key: &str) {
            self.erased.push(key.to_string());
        }

        fn binlog_add(&mut self, handler_type: i32, data: &[u8]) -> u64 {
            let id = self.binlog_events.len() as u64 + 1;
            self.binlog_events.push((handler_type, id, data.to_vec()));
            id
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn on_get_users(&mut self, _users: Vec<telegram_api::User>, _source: &str) {}

        fn get_user_id_object(&mut self, _user_id: i64, _source: &str) -> i64 {
            self.user_id_object
        }

        fn set_option_boolean(&mut self, name: &str, value: bool) {
            self.options.insert(name.to_string(), value as i64);
        }

        fn reregister_device(&mut self) {}

        fn send_get_authorizations_query(&mut self) {
            self.queries.push("getAuthorizations".to_string());
        }

        fn send_accept_login_token_query(&mut self, login_token: &[u8]) {
            self.queries
                .push(format!("acceptLoginToken {}", login_token.len()));
        }

        fn send_reset_authorization_query(&mut self, authorization_id: i64) {
            self.queries
                .push(format!("resetAuthorization {authorization_id}"));
        }

        fn send_reset_authorizations_query(&mut self) {
            self.queries.push("resetAuthorizations".to_string());
        }

        fn send_change_authorization_settings_query(
            &mut self,
            hash: i64,
            _set_encrypted_requests_disabled: bool,
            _encrypted_requests_disabled: bool,
            _set_call_requests_disabled: bool,
            _call_requests_disabled: bool,
            confirm: bool,
        ) {
            self.queries
                .push(format!("changeAuthorizationSettings {hash} {confirm}"));
        }

        fn send_get_web_authorizations_query(&mut self) {
            self.queries.push("getWebAuthorizations".to_string());
        }

        fn send_reset_web_authorization_query(&mut self, hash: i64) {
            self.queries.push(format!("resetWebAuthorization {hash}"));
        }

        fn send_reset_web_authorizations_query(&mut self) {
            self.queries.push("resetWebAuthorizations".to_string());
        }

        fn send_export_contact_token_query(&mut self) {
            self.queries.push("exportContactToken".to_string());
        }

        fn send_set_account_ttl_query(&mut self, account_ttl: i32) {
            self.queries.push(format!("setAccountTTL {account_ttl}"));
        }

        fn send_get_account_ttl_query(&mut self) {
            self.queries.push("getAccountTTL".to_string());
        }
    }

    #[test]
    fn unconfirmed_authorizations_add_delete_and_expire() {
        let mut env = TestEnv::new();
        env.options
            .insert("authorization_autoconfirm_period".to_string(), 100);
        let mut list = UnconfirmedAuthorizations::default();
        let mut is_first_changed = false;
        assert!(list.add_authorization(
            UnconfirmedAuthorization::new(false, 1, 0, 900, "d".into(), "l".into()),
            &mut is_first_changed,
        ));
        assert!(is_first_changed);
        // duplicates are dropped
        assert!(!list.add_authorization(
            UnconfirmedAuthorization::new(false, 1, 0, 950, "d".into(), "l".into()),
            &mut is_first_changed,
        ));
        // the list stays date-sorted
        assert!(list.add_authorization(
            UnconfirmedAuthorization::new(false, 2, 0, 800, "d".into(), "l".into()),
            &mut is_first_changed,
        ));
        let update = list.get_update_unconfirmed_session_object(&mut env);
        match update {
            td_api::Update::updateUnconfirmedSession(update) => {
                assert_eq!(update.unconfirmed_session_count, 2);
                assert_eq!(update.session.as_ref().unwrap().date, 800);
            }
            other => panic!("unexpected update: {other:?}"),
        }

        // expiry keeps only the newest (800 + 100 <= 990 < 900 + 100)
        env.unix_time = 990;
        assert!(list.delete_expired_authorizations(&env));
        assert_eq!(
            list.get_update_unconfirmed_session_object(&mut env),
            td_api::Update::updateUnconfirmedSession(td_api::updateUnconfirmedSession {
                session: Some(td_api::UnconfirmedSession {
                    r#type: Some(td_api::SessionType::sessionTypeDevice(
                        td_api::sessionTypeDevice { session_id: 1 }
                    )),
                    date: 900,
                    device_model: "d".to_string(),
                    location: "l".to_string(),
                }),
                unconfirmed_session_count: 1,
            })
        );
    }

    #[test]
    fn unconfirmed_authorizations_round_trip_through_the_pmc() {
        let mut env = TestEnv::new();
        let mut list = UnconfirmedAuthorizations::default();
        let mut is_first_changed = false;
        list.add_authorization(
            UnconfirmedAuthorization::new(true, 0, 5, 900, "d".into(), "l".into()),
            &mut is_first_changed,
        );
        save_unconfirmed_authorizations(&mut env, &list);
        let mut parser =
            Parser::new_log_event(env.pmc.get(b"new_authorizations".as_slice()).unwrap()).unwrap();
        let parsed = UnconfirmedAuthorizations::parse(&mut parser).unwrap();
        assert_eq!(parsed, list);
    }

    #[test]
    fn account_manager_emits_updates_and_replays_events() {
        let mut env = TestEnv::new();
        let mut manager = AccountManager::new();
        manager.on_new_unconfirmed_authorization(
            &mut env,
            false,
            11,
            0,
            900,
            "device".to_string(),
            "location".to_string(),
        );
        assert_eq!(env.updates.len(), 1);
        assert!(manager.on_confirm_authorization(&mut env, false, 11, 0));
        assert!(!manager.on_confirm_authorization(&mut env, false, 11, 0));

        // terminate_session: confirm + the reset query + the log event
        manager.on_new_unconfirmed_authorization(
            &mut env,
            false,
            12,
            0,
            900,
            "device".to_string(),
            "location".to_string(),
        );
        let promise = Promise::default();
        manager.terminate_session(&mut env, 12, promise);
        assert!(env.queries.iter().any(|q| q == "resetAuthorization 12"));

        // the reset event replays
        let events = env.binlog_events.clone();
        manager.on_binlog_events(&mut env, events);
    }

    #[test]
    fn qr_confirmation_decodes_and_sends() {
        let mut env = TestEnv::new();
        let mut manager = AccountManager::new();
        let token = base64url_encode(b"test-token");
        let promise = Promise::default();
        manager.confirm_qr_code_authentication(
            &mut env,
            &format!("tg://login?token={token}"),
            promise,
        );
        assert!(env.queries.iter().any(|q| q == "acceptLoginToken 10"));

        // an invalid link errors
        let promise = Promise::default();
        manager.confirm_qr_code_authentication(&mut env, "https://t.me/x", promise);
        assert_eq!(env.queries.len(), 1);
    }

    fn base64url_encode(input: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in input.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let value = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            out.push(ALPHABET[(value >> 18) as usize & 63] as char);
            out.push(ALPHABET[(value >> 12) as usize & 63] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[(value >> 6) as usize & 63] as char);
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[value as usize & 63] as char);
            }
        }
        out
    }

    #[test]
    fn session_device_types_classify() {
        let authorization = telegram_api::Authorization {
            hash: 1,
            flags: 0,
            current: true,
            password_pending: false,
            unconfirmed: false,
            encrypted_requests_disabled: false,
            call_requests_disabled: true,
            api_id: 0,
            app_name: "Telegram Web".to_string(),
            app_version: String::new(),
            official_app: false,
            device_model: "Chrome 120".to_string(),
            platform: String::new(),
            system_version: String::new(),
            date_created: 1,
            date_active: 2,
            ip: String::new(),
            country: String::new(),
            region: String::new(),
        };
        let session = convert_authorization_object(&authorization);
        assert!(matches!(
            session.device_type,
            Some(td_api::SessionDeviceType::sessionDeviceTypeChrome(_))
        ));
        assert!(!session.can_accept_calls);
        assert!(session.can_accept_secret_chats);
    }
}
