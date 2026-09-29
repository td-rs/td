//! Port of `td/telegram/AuthManager.{h,cpp}` (S71): the authorization
//! state machine — the `State` enum, the persistent `DbState` (binlog
//! store/parse, byte-compatible with the C++ format), the
//! `updateAuthorizationState` emission per state, and the td_api entry
//! points (`setTdlibParameters` lives in Td; here `setAuthenticationPhoneNumber`,
//! `checkAuthenticationCode`, `logOut`, `destroy`).
//!
//! The auth queries (`auth.sendCode`, `auth.signIn`, `account.getPassword`,
//! `auth.logOut`) go through the [`AuthQuerySender`] trait — the real
//! session-backed sender lands with the Track G session wiring; tests use a
//! mock. The `setTdlibParameters` validation errors live in `Td::get_parameters`
//! and are noted here for the Td actor.

use std::fmt;

use crate::td_api;
use crate::td_api::Object;

/// `AuthManager::State` — stored as int32 in the binlog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i32)]
pub enum State {
    #[default]
    None = 0,
    WaitPhoneNumber = 1,
    WaitPremiumPurchase = 2,
    WaitCode = 3,
    WaitQrCodeConfirmation = 4,
    WaitPassword = 5,
    WaitRegistration = 6,
    WaitEmailAddress = 7,
    WaitEmailCode = 8,
    Ok = 9,
    LoggingOut = 10,
    DestroyingKeys = 11,
    Closing = 12,
}

impl State {
    fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::None),
            1 => Some(Self::WaitPhoneNumber),
            2 => Some(Self::WaitPremiumPurchase),
            3 => Some(Self::WaitCode),
            4 => Some(Self::WaitQrCodeConfirmation),
            5 => Some(Self::WaitPassword),
            6 => Some(Self::WaitRegistration),
            7 => Some(Self::WaitEmailAddress),
            8 => Some(Self::WaitEmailCode),
            9 => Some(Self::Ok),
            10 => Some(Self::LoggingOut),
            11 => Some(Self::DestroyingKeys),
            12 => Some(Self::Closing),
            _ => None,
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// `SendCodeHelper::AuthenticationCodeInfo::Type` (int32 on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i32)]
pub enum CodeType {
    #[default]
    None = 0,
    Message = 1,
    Sms = 2,
    Call = 3,
    FlashCall = 4,
    MissedCall = 5,
    Fragment = 6,
    FirebaseAndroidSafetyNet = 7,
    FirebaseIos = 8,
    SmsWord = 9,
    SmsPhrase = 10,
    FirebaseAndroidPlayIntegrity = 11,
}

impl CodeType {
    fn from_i32(value: i32) -> Option<Self> {
        Some(match value {
            0 => Self::None,
            1 => Self::Message,
            2 => Self::Sms,
            3 => Self::Call,
            4 => Self::FlashCall,
            5 => Self::MissedCall,
            6 => Self::Fragment,
            7 => Self::FirebaseAndroidSafetyNet,
            8 => Self::FirebaseIos,
            9 => Self::SmsWord,
            10 => Self::SmsPhrase,
            11 => Self::FirebaseAndroidPlayIntegrity,
            _ => return None,
        })
    }
}

/// `SendCodeHelper::AuthenticationCodeInfo`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuthenticationCodeInfo {
    pub ty: CodeType,
    pub length: i32,
    pub push_timeout: i32,
    pub cloud_project_number: i64,
    pub pattern: String,
}

/// `SendCodeHelper` — the subset the state machine carries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SendCodeHelper {
    pub phone_number: String,
    pub phone_code_hash: String,
    pub sent_code_info: AuthenticationCodeInfo,
    pub next_code_info: AuthenticationCodeInfo,
    pub next_code_timestamp: f64,
}

/// `AuthManager::WaitPasswordState` — the SRP password parameters.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WaitPasswordState {
    pub current_client_salt: String,
    pub current_server_salt: String,
    pub srp_g: i32,
    pub srp_p: String,
    pub srp_b: String,
    pub srp_id: i64,
    pub hint: String,
    pub has_recovery: bool,
    pub email_address_pattern: String,
    pub has_secure_values: bool,
}

/// `AuthManager::DbState` — the persistent per-state payload.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DbState {
    pub state: State,
    pub api_id: i32,
    pub api_hash: String,
    pub expires_at: f64,
    pub terms_of_service: Option<String>,
    // WaitPremiumPurchase
    pub store_product_id: String,
    pub support_email_address: String,
    pub support_email_subject: String,
    pub premium_day_count: i32,
    // WaitEmailAddress / WaitEmailCode
    pub allow_apple_id: bool,
    pub allow_google_id: bool,
    // WaitEmailCode
    pub email_address: String,
    pub reset_available_period: i32,
    pub reset_pending_date: i32,
    // WaitQrCodeConfirmation
    pub other_user_ids: Vec<i64>,
    pub login_token: String,
    pub login_token_expires_at: f64,
    // shared by the code/email/premium/registration states
    pub send_code_helper: SendCodeHelper,
    // WaitPassword
    pub wait_password_state: WaitPasswordState,
}

/// A little-endian binary writer over the binlog format (`StorerT`).
struct BinlogWriter {
    data: Vec<u8>,
}

impl BinlogWriter {
    fn new() -> Self {
        Self { data: Vec::new() }
    }

    fn i32(&mut self, value: i32) {
        self.data.extend_from_slice(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.data.extend_from_slice(&value.to_le_bytes());
    }

    fn f64(&mut self, value: f64) {
        self.data.extend_from_slice(&value.to_le_bytes());
    }

    fn bool_(&mut self, value: bool) {
        self.i32(if value {
            0x9972_75b5u32 as i32
        } else {
            0xbc79_9737u32 as i32
        });
    }

    /// The TL string framing (1-byte/0xfe/0xff length forms, zero padding).
    fn string(&mut self, value: &str) {
        let bytes = value.as_bytes();
        let len = bytes.len();
        if len < 254 {
            self.data.push(len as u8);
            let padded = (len + 1 + 3) & !3;
            self.data.extend_from_slice(bytes);
            self.data.resize(self.data.len() + padded - len - 1, 0);
        } else if len < (1 << 24) {
            self.data.push(0xfe);
            self.data
                .extend_from_slice(&(len as i32).to_le_bytes()[..3]);
            let padded = (len + 4 + 3) & !3;
            self.data.extend_from_slice(bytes);
            self.data.resize(self.data.len() + padded - len - 4, 0);
        } else {
            self.data.push(0xff);
            self.data.extend_from_slice(&(len as i32).to_le_bytes());
            let padded = (len + 8 + 3) & !3;
            self.data.extend_from_slice(bytes);
            self.data.resize(self.data.len() + padded - len - 8, 0);
        }
    }

    fn vector_i64(&mut self, values: &[i64]) {
        self.i32(values.len() as i32);
        for value in values {
            self.i64(*value);
        }
    }

    /// `store_time`: unix time as a double.
    fn time(&mut self, value: f64) {
        self.f64(value);
    }
}

/// A bounds-checked reader over the binlog format (`ParserT`).
struct BinlogReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BinlogReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn i32(&mut self) -> Result<i32, String> {
        self.take(4)
            .map(|b| i32::from_le_bytes(b.try_into().unwrap()))
    }

    fn i64(&mut self) -> Result<i64, String> {
        self.take(8)
            .map(|b| i64::from_le_bytes(b.try_into().unwrap()))
    }

    fn f64(&mut self) -> Result<f64, String> {
        self.take(8)
            .map(|b| f64::from_le_bytes(b.try_into().unwrap()))
    }

    fn bool_(&mut self) -> Result<bool, String> {
        Ok(self.i32()? == 0x9972_75b5u32 as i32)
    }

    fn string(&mut self) -> Result<String, String> {
        let first = self.take(1)?[0];
        let (len, header) = if first < 254 {
            (first as usize, 1)
        } else if first == 0xfe {
            let mut len = [0u8; 4];
            len[..3].copy_from_slice(self.take(3)?);
            (i32::from_le_bytes(len) as usize, 4)
        } else {
            (self.i32()? as usize, 8)
        };
        let padded = (len + header + 3) & !3;
        let bytes = self.take(len)?;
        self.take(padded - len - header)?;
        String::from_utf8(bytes.to_vec()).map_err(|error| error.to_string())
    }

    fn vector_i64(&mut self) -> Result<Vec<i64>, String> {
        let len = self.i32()?;
        if len < 0 || len as usize > self.remaining() / 8 {
            return Err("Wrong vector length".to_string());
        }
        let mut result = Vec::with_capacity(len as usize);
        for _ in 0..len {
            result.push(self.i64()?);
        }
        Ok(result)
    }

    fn time(&mut self) -> Result<f64, String> {
        self.f64()
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        if self.remaining() < count {
            return Err("Not enough data to read".to_string());
        }
        let slice = &self.data[self.pos..self.pos + count];
        self.pos += count;
        Ok(slice)
    }
}

impl AuthenticationCodeInfo {
    fn store(&self, writer: &mut BinlogWriter) {
        writer.i32(self.ty as i32);
        writer.i32(self.length);
        writer.i32(self.push_timeout);
        writer.i64(self.cloud_project_number);
        writer.string(&self.pattern);
    }

    fn parse(reader: &mut BinlogReader<'_>) -> Result<Self, String> {
        let ty = CodeType::from_i32(reader.i32()?)
            .ok_or_else(|| "Unexpected AuthenticationCodeInfo type".to_string())?;
        Ok(Self {
            ty,
            length: reader.i32()?,
            push_timeout: reader.i32()?,
            cloud_project_number: reader.i64()?,
            pattern: reader.string()?,
        })
    }
}

impl SendCodeHelper {
    /// `SendCodeHelper::store`: phone, a legacy is_registered flag, hash,
    /// the two code infos, and the next-code timestamp.
    fn store(&self, writer: &mut BinlogWriter) {
        writer.string(&self.phone_number);
        writer.bool_(true); // legacy is_registered — always true in this version
        writer.string(&self.phone_code_hash);
        self.sent_code_info.store(writer);
        self.next_code_info.store(writer);
        writer.time(self.next_code_timestamp);
    }

    fn parse(reader: &mut BinlogReader<'_>) -> Result<Self, String> {
        let phone_number = reader.string()?;
        let _is_registered = reader.bool_()?; // legacy flag — always true in this version
        Ok(Self {
            phone_number,
            phone_code_hash: reader.string()?,
            sent_code_info: AuthenticationCodeInfo::parse(reader)?,
            next_code_info: AuthenticationCodeInfo::parse(reader)?,
            next_code_timestamp: reader.time()?,
        })
    }

    /// `SendCodeHelper::get_authentication_code_info_object`.
    pub fn get_authentication_code_info_object(&self, now: f64) -> td_api::AuthenticationCodeInfo {
        td_api::AuthenticationCodeInfo {
            phone_number: self.phone_number.clone(),
            r#type: Some(code_type_object(&self.sent_code_info)),
            next_type: Some(code_type_object(&self.next_code_info)),
            timeout: ((self.next_code_timestamp - now + 1.0 - 1e-9) as i32).max(0),
        }
    }
}

impl WaitPasswordState {
    /// `WaitPasswordState::store` — note the C++ serializes
    /// `email_address_pattern_` BEFORE `has_secure_values_` (AuthManager.hpp).
    fn store(&self, writer: &mut BinlogWriter) {
        writer.string(&self.current_client_salt);
        writer.string(&self.current_server_salt);
        writer.i32(self.srp_g);
        writer.string(&self.srp_p);
        writer.string(&self.srp_b);
        writer.i64(self.srp_id);
        writer.string(&self.hint);
        writer.bool_(self.has_recovery);
        writer.string(&self.email_address_pattern);
        writer.bool_(self.has_secure_values);
    }

    fn parse(reader: &mut BinlogReader<'_>) -> Result<Self, String> {
        Ok(Self {
            current_client_salt: reader.string()?,
            current_server_salt: reader.string()?,
            srp_g: reader.i32()?,
            srp_p: reader.string()?,
            srp_b: reader.string()?,
            srp_id: reader.i64()?,
            hint: reader.string()?,
            has_recovery: reader.bool_()?,
            email_address_pattern: reader.string()?,
            has_secure_values: reader.bool_()?,
        })
    }
}

impl DbState {
    /// The binlog flags word of the C++ `BEGIN_STORE_FLAGS` block: the
    /// 9 always-true capability flags, `has_terms_of_service`,
    /// `allow_apple_id_`, `allow_google_id_`.
    const FLAG_HAS_TERMS_OF_SERVICE: u32 = 1 << 0;
    const FLAG_PBKDF2: u32 = 1 << 1;
    const FLAG_SRP: u32 = 1 << 2;
    const FLAG_WAIT_REGISTRATION: u32 = 1 << 3;
    const FLAG_WAIT_REGISTRATION_PHONE: u32 = 1 << 4;
    const FLAG_QR_CODE: u32 = 1 << 5;
    const FLAG_APPLE_ID: u32 = 1 << 6;
    const FLAG_GOOGLE_ID: u32 = 1 << 7;
    const FLAG_TIME_STORE: u32 = 1 << 8;
    const FLAG_RESET_EMAIL: u32 = 1 << 9;
    const FLAG_PREMIUM_PURCHASE: u32 = 1 << 10;
    const FLAG_PREMIUM_PURCHASE_SUPPORT: u32 = 1 << 11;

    fn flags(&self) -> u32 {
        let mut flags = Self::FLAG_PBKDF2
            | Self::FLAG_SRP
            | Self::FLAG_WAIT_REGISTRATION
            | Self::FLAG_WAIT_REGISTRATION_PHONE
            | Self::FLAG_QR_CODE
            | Self::FLAG_TIME_STORE
            | Self::FLAG_RESET_EMAIL
            | Self::FLAG_PREMIUM_PURCHASE
            | Self::FLAG_PREMIUM_PURCHASE_SUPPORT;
        if self.terms_of_service.is_some() {
            flags |= Self::FLAG_HAS_TERMS_OF_SERVICE;
        }
        if self.allow_apple_id {
            flags |= Self::FLAG_APPLE_ID;
        }
        if self.allow_google_id {
            flags |= Self::FLAG_GOOGLE_ID;
        }
        flags
    }

    /// `AuthManager::DbState::store` — under `log_event_store`, so the
    /// log-event version prefix (LogEventStorerUnsafe) leads the payload.
    pub fn store(&self) -> Vec<u8> {
        let mut writer = BinlogWriter::new();
        crate::store_parse::store_log_event_version(&mut writer.data);
        writer.i32(self.flags() as i32);
        writer.i32(self.state as i32);
        writer.i32(self.api_id);
        writer.string(&self.api_hash);
        writer.time(self.expires_at);
        if let Some(terms_of_service) = &self.terms_of_service {
            writer.string(terms_of_service);
        }
        match self.state {
            State::WaitPremiumPurchase => {
                self.send_code_helper.store(&mut writer);
                writer.string(&self.store_product_id);
                writer.string(&self.support_email_address);
                writer.string(&self.support_email_subject);
                writer.i32(self.premium_day_count);
            }
            State::WaitEmailAddress | State::WaitCode | State::WaitRegistration => {
                self.send_code_helper.store(&mut writer);
            }
            State::WaitEmailCode => {
                self.send_code_helper.store(&mut writer);
                writer.string(&self.email_address);
                writer.i32(self.reset_available_period);
                writer.i32(self.reset_pending_date);
            }
            State::WaitQrCodeConfirmation => {
                writer.vector_i64(&self.other_user_ids);
                writer.string(&self.login_token);
                writer.time(self.login_token_expires_at);
            }
            State::WaitPassword => {
                self.wait_password_state.store(&mut writer);
            }
            _ => {}
        }
        writer.data
    }

    /// `AuthManager::DbState::parse` — under `log_event_parse`, so the
    /// log-event version prefix is consumed first.
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        if data.len() < 4
            || i32::from_le_bytes(data[0..4].try_into().expect("length checked"))
                != crate::store_parse::version::LOG_EVENT_STORE_VERSION
        {
            return Err("Invalid log event version".to_string());
        }
        let mut reader = BinlogReader::new(&data[4..]);
        let flags = reader.i32()? as u32;
        if flags & Self::FLAG_PREMIUM_PURCHASE_SUPPORT == 0 {
            return Err("Have no premium subscription purchase support".to_string());
        }
        for flag in [
            Self::FLAG_PBKDF2,
            Self::FLAG_SRP,
            Self::FLAG_WAIT_REGISTRATION,
            Self::FLAG_WAIT_REGISTRATION_PHONE,
            Self::FLAG_QR_CODE,
            Self::FLAG_TIME_STORE,
            Self::FLAG_RESET_EMAIL,
            Self::FLAG_PREMIUM_PURCHASE,
        ] {
            if flags & flag == 0 {
                return Err("Old database".to_string());
            }
        }
        let state = State::from_i32(reader.i32()?).ok_or_else(|| "Unexpected state".to_string())?;
        let api_id = reader.i32()?;
        let api_hash = reader.string()?;
        let expires_at = reader.time()?;
        let terms_of_service = if flags & Self::FLAG_HAS_TERMS_OF_SERVICE != 0 {
            Some(reader.string()?)
        } else {
            None
        };
        let mut result = Self {
            state,
            api_id,
            api_hash,
            expires_at,
            terms_of_service,
            allow_apple_id: flags & Self::FLAG_APPLE_ID != 0,
            allow_google_id: flags & Self::FLAG_GOOGLE_ID != 0,
            ..Self::default()
        };
        match state {
            State::WaitPremiumPurchase => {
                result.send_code_helper = SendCodeHelper::parse(&mut reader)?;
                result.store_product_id = reader.string()?;
                result.support_email_address = reader.string()?;
                result.support_email_subject = reader.string()?;
                result.premium_day_count = reader.i32()?;
            }
            State::WaitEmailAddress | State::WaitCode | State::WaitRegistration => {
                result.send_code_helper = SendCodeHelper::parse(&mut reader)?;
            }
            State::WaitEmailCode => {
                result.send_code_helper = SendCodeHelper::parse(&mut reader)?;
                result.email_address = reader.string()?;
                result.reset_available_period = reader.i32()?;
                result.reset_pending_date = reader.i32()?;
            }
            State::WaitQrCodeConfirmation => {
                result.other_user_ids = reader.vector_i64()?;
                result.login_token = reader.string()?;
                result.login_token_expires_at = reader.time()?;
            }
            State::WaitPassword => {
                result.wait_password_state = WaitPasswordState::parse(&mut reader)?;
            }
            _ => {}
        }
        Ok(result)
    }

    /// The factory constructors (`AuthManager.cpp:100-162`). The timeouts:
    /// 86400 for WaitPassword/WaitPremiumPurchase/WaitRegistration, 300 for
    /// the rest.
    pub fn wait_code(
        api_id: i32,
        api_hash: String,
        send_code_helper: SendCodeHelper,
        now: f64,
    ) -> Self {
        Self {
            state: State::WaitCode,
            api_id,
            api_hash,
            expires_at: now + 300.0,
            send_code_helper,
            ..Self::default()
        }
    }

    pub fn wait_password(
        api_id: i32,
        api_hash: String,
        wait_password_state: WaitPasswordState,
        now: f64,
    ) -> Self {
        Self {
            state: State::WaitPassword,
            api_id,
            api_hash,
            expires_at: now + 86400.0,
            wait_password_state,
            ..Self::default()
        }
    }
}

/// `AuthManager::NetQueryType` — the subset the state machine uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetQueryType {
    None,
    SendCode,
    SignIn,
    LogOut,
    GetPassword,
    CheckPassword,
}

/// The query payload handed to the [`AuthQuerySender`]. The real
/// `telegram_api` conversions land with the session-backed sender.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthNetQuery {
    SendCode {
        phone_number: String,
    },
    SignIn {
        phone_number: String,
        phone_code_hash: String,
        code: String,
    },
    BotToken {
        /// The bot token — `auth.importBotAuthorization` (reusable, unlike
        /// phone codes — the S76 harness relies on exactly this).
        token: String,
    },
    LogOut,
    GetPassword,
    CheckPassword {
        /// The password to check — the SRP math runs in the sender (the
        /// C++ `PasswordManager::get_input_check_password`).
        password: String,
    },
}

/// The query result.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthNetResult {
    SentCode {
        phone_code_hash: String,
        /// `auth.sentCode` (WaitCode), `auth.sentCodeSuccess` (Ok),
        /// `auth.sentCodePaymentRequired`, `sentCodeTypeSetUpEmailRequired`,
        /// `sentCodeTypeEmailCode`.
        kind: SentCodeKind,
        code_info: AuthenticationCodeInfo,
        next_code_info: AuthenticationCodeInfo,
        timeout: f64,
    },
    Authorization {
        /// The user id in `auth.authorization`; `None` means
        /// `authorizationSignUpRequired` (WaitRegistration).
        user_id: Option<i64>,
        terms_of_service: Option<String>,
    },
    PasswordNeeded {
        wait_password_state: WaitPasswordState,
    },
    PasswordOk,
    LoggedOut,
}

/// The `auth.sentCode` result shapes.
#[derive(Debug, Clone, PartialEq)]
pub enum SentCodeKind {
    Code,
    Success,
    PaymentRequired,
    EmailAddressRequired,
    EmailCode,
}

/// The abstraction the auth queries go through — the C++ `NetQueryType`
/// dispatcher; a mock drives the tests, the session-backed sender lands
/// with the Track G wiring.
pub trait AuthQuerySender {
    fn send(&mut self, query: AuthNetQuery) -> Result<AuthNetResult, String>;
}

/// `td::AuthManager`.
#[derive(Debug, Default)]
pub struct AuthManager {
    state: State,
    api_id: i32,
    api_hash: String,
    is_bot: bool,
    phone_number: String,
    send_code_helper: SendCodeHelper,
    wait_password_state: WaitPasswordState,
    terms_of_service: Option<String>,
    pending_updates: Vec<Object>,
}

impl AuthManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// `AuthManager::is_authorized`.
    pub fn is_authorized(&self) -> bool {
        self.state == State::Ok
    }

    /// Test-only: the authorized state without the sign-in query (the
    /// td.rs dispatch tests need a client past the 401 gate; the real
    /// transition rides check_authentication_code's Authorization answer).
    #[cfg(test)]
    pub(crate) fn force_authorized_for_test(&mut self) {
        self.update_state(State::Ok);
    }

    /// The api_id/api_hash half of `setTdlibParameters` (Td passes them
    /// through after its own validation).
    pub fn set_api_credentials(&mut self, api_id: i32, api_hash: String) {
        self.api_id = api_id;
        self.api_hash = api_hash;
    }

    /// `AuthManager::save_state` — the DbState payload for the current state
    /// (the pmc `"auth_state"` write lands with the TdDb wiring).
    pub fn save_state(&self, now: f64) -> Option<Vec<u8>> {
        let state = match self.state {
            State::WaitCode => DbState::wait_code(
                self.api_id,
                self.api_hash.clone(),
                self.send_code_helper.clone(),
                now,
            ),
            State::WaitPassword => DbState::wait_password(
                self.api_id,
                self.api_hash.clone(),
                self.wait_password_state.clone(),
                now,
            ),
            _ => return None,
        };
        Some(state.store())
    }

    /// `AuthManager::load_state` — restore from the stored DbState,
    /// rejecting api_id/api_hash mismatches and expired states like the C++
    /// (cpp:1700-1756). The restore does not broadcast
    /// (`update_state(state, false)`).
    pub fn load_state(&mut self, data: &[u8], now: f64) -> Result<(), String> {
        let db_state = DbState::parse(data)?;
        if db_state.api_id != self.api_id || db_state.api_hash != self.api_hash {
            return Err("API id or API hash mismatch".to_string());
        }
        if db_state.expires_at < now {
            return Err("Auth state expired".to_string());
        }
        self.state = db_state.state;
        self.send_code_helper = db_state.send_code_helper;
        self.wait_password_state = db_state.wait_password_state;
        self.phone_number = self.send_code_helper.phone_number.clone();
        Ok(())
    }

    /// `AuthManager::was_authorized`.
    pub fn was_authorized(&self) -> bool {
        matches!(
            self.state,
            State::Ok | State::LoggingOut | State::DestroyingKeys | State::Closing
        )
    }

    /// `AuthManager::is_bot`.
    pub fn is_bot(&self) -> bool {
        self.is_bot
    }

    /// `AuthManager::get_state`.
    pub fn get_state(&self) -> State {
        self.state
    }

    /// `AuthManager::get_current_authorization_state_object` (cpp:442-448) —
    /// the bare `AuthorizationState`, `None` in `State::None`.
    pub fn get_current_authorization_state_object(&self) -> Option<td_api::AuthorizationState> {
        if self.state == State::None {
            None
        } else {
            self.get_authorization_state_object().authorization_state
        }
    }

    /// The fresh-DB ctor tail (AuthManager.cpp:362-365): the first
    /// broadcast state — `authorizationStateWaitPhoneNumber`.
    pub fn start_wait_phone_number(&mut self) {
        if self.state == State::None {
            self.update_state(State::WaitPhoneNumber);
        }
    }

    /// The authorized-DB resume (AuthManager.cpp:349-358): straight to
    /// `authorizationStateReady` with zero network.
    pub fn set_state_ok(&mut self) {
        if self.state != State::Ok {
            self.update_state(State::Ok);
        }
    }

    /// The queued `updateAuthorizationState` broadcasts (drained by Td).
    pub fn take_updates(&mut self) -> Vec<Object> {
        std::mem::take(&mut self.pending_updates)
    }

    /// `AuthManager::update_state` — the update broadcast.
    fn update_state(&mut self, new_state: State) {
        self.state = new_state;
        self.pending_updates.push(Object::updateAuthorizationState(
            self.get_authorization_state_object(),
        ));
    }

    /// `AuthManager::get_authorization_state_object` (cpp:395-440).
    pub fn get_authorization_state_object(&self) -> td_api::updateAuthorizationState {
        use td_api::AuthorizationState as A;
        let authorization_state = match self.state {
            State::None => unreachable!("authorization state in State::None"),
            State::WaitPhoneNumber => {
                A::authorizationStateWaitPhoneNumber(td_api::authorizationStateWaitPhoneNumber)
            }
            State::WaitPremiumPurchase => A::authorizationStateWaitPremiumPurchase(
                td_api::authorizationStateWaitPremiumPurchase {
                    store_product_id: String::new(), // TODO: carried per-state with the real queries
                    premium_day_count: 0,
                    support_email_address: String::new(),
                    support_email_subject: String::new(),
                },
            ),
            State::WaitEmailAddress => {
                A::authorizationStateWaitEmailAddress(td_api::authorizationStateWaitEmailAddress {
                    allow_apple_id: self.db_state_apple_id(),
                    allow_google_id: self.db_state_google_id(),
                })
            }
            State::WaitEmailCode => {
                A::authorizationStateWaitEmailCode(td_api::authorizationStateWaitEmailCode {
                    allow_apple_id: self.db_state_apple_id(),
                    allow_google_id: self.db_state_google_id(),
                    // TODO: the email code info and the reset-state timers land
                    // with the email verification flow.
                    code_info: Some(td_api::EmailAddressAuthenticationCodeInfo {
                        email_address_pattern: String::new(),
                        length: 0,
                    }),
                    email_address_reset_state: Some(
                        td_api::EmailAddressResetState::emailAddressResetStateAvailable(
                            td_api::emailAddressResetStateAvailable { wait_period: 0 },
                        ),
                    ),
                })
            }
            State::WaitCode => A::authorizationStateWaitCode(td_api::authorizationStateWaitCode {
                code_info: Some(
                    self.send_code_helper
                        .get_authentication_code_info_object(tdutils::time::now()),
                ),
            }),
            State::WaitQrCodeConfirmation => A::authorizationStateWaitOtherDeviceConfirmation(
                td_api::authorizationStateWaitOtherDeviceConfirmation {
                    // `"tg://login?token=" + base64url_encode(login_token)` — the
                    // token arrives with the QR login flow.
                    link: String::new(),
                },
            ),
            State::WaitPassword => {
                A::authorizationStateWaitPassword(td_api::authorizationStateWaitPassword {
                    password_hint: self.wait_password_state.hint.clone(),
                    has_recovery_email_address: self.wait_password_state.has_recovery,
                    // the scheme renamed has_secure_values to has_passport_data
                    has_passport_data: self.wait_password_state.has_secure_values,
                    recovery_email_address_pattern: self
                        .wait_password_state
                        .email_address_pattern
                        .clone(),
                })
            }
            State::WaitRegistration => {
                A::authorizationStateWaitRegistration(td_api::authorizationStateWaitRegistration {
                    terms_of_service: Some(terms_of_service_object(
                        self.terms_of_service.as_deref(),
                    )),
                })
            }
            State::Ok => A::authorizationStateReady(td_api::authorizationStateReady),
            State::LoggingOut | State::DestroyingKeys => {
                A::authorizationStateLoggingOut(td_api::authorizationStateLoggingOut)
            }
            State::Closing => A::authorizationStateClosing(td_api::authorizationStateClosing),
        };
        td_api::updateAuthorizationState {
            authorization_state: Some(authorization_state),
        }
    }

    // The apple/google id flags live in DbState in the C++; the manager
    // keeps them separately for now.
    fn db_state_apple_id(&self) -> bool {
        false
    }

    fn db_state_google_id(&self) -> bool {
        false
    }

    /// `AuthManager::set_phone_number` (cpp:638-683), phase 1: the
    /// validation + the query (the auth-net slice dispatches it; the
    /// answer applies through [`Self::on_sent_code_result`]).
    pub fn prepare_authentication_phone_number(
        &mut self,
        phone_number: String,
    ) -> Result<AuthNetQuery, String> {
        if self.state != State::WaitPhoneNumber {
            return Err("Call to setAuthenticationPhoneNumber unexpected".to_string());
        }
        if self.is_bot {
            return Err(
                "Cannot set phone number after bot token was entered. You must log out first"
                    .to_string(),
            );
        }
        if phone_number.is_empty() {
            return Err("Phone number must be non-empty".to_string());
        }
        if phone_number != self.phone_number {
            self.clear_auth_state();
        }
        self.send_code_helper.phone_number = phone_number.clone();
        self.phone_number = phone_number.clone();

        Ok(AuthNetQuery::SendCode { phone_number })
    }

    /// Phase 2: apply the `auth.sentCode` answer.
    pub fn on_sent_code_result(&mut self, result: AuthNetResult) -> Result<(), String> {
        match result {
            AuthNetResult::SentCode {
                phone_code_hash,
                kind,
                code_info,
                next_code_info,
                timeout,
            } => {
                self.send_code_helper.phone_code_hash = phone_code_hash;
                self.send_code_helper.sent_code_info = code_info;
                self.send_code_helper.next_code_info = next_code_info;
                self.send_code_helper.next_code_timestamp = tdutils::time::now() + timeout;
                match kind {
                    SentCodeKind::Code => self.update_state(State::WaitCode),
                    SentCodeKind::Success => self.update_state(State::Ok),
                    SentCodeKind::PaymentRequired => self.update_state(State::WaitPremiumPurchase),
                    SentCodeKind::EmailAddressRequired => {
                        self.update_state(State::WaitEmailAddress)
                    }
                    SentCodeKind::EmailCode => self.update_state(State::WaitEmailCode),
                }
                Ok(())
            }
            other => Err(format!(
                "Unexpected result for sendCode: {:?}",
                std::mem::discriminant(&other)
            )),
        }
    }

    /// The blocking form (the MockSender tests).
    pub fn set_authentication_phone_number(
        &mut self,
        phone_number: &str,
        sender: &mut dyn AuthQuerySender,
    ) -> Result<(), String> {
        let query = self.prepare_authentication_phone_number(phone_number.to_string())?;
        self.on_sent_code_result(sender.send(query)?)
    }

    /// `AuthManager::check_code` + `on_authentication_result` +
    /// `on_get_authorization` (cpp:848-858, 1401-1410, 1487-1562).
    pub fn prepare_check_authentication_code(
        &mut self,
        code: String,
    ) -> Result<AuthNetQuery, String> {
        if self.state != State::WaitCode {
            return Err("Call to checkAuthenticationCode unexpected".to_string());
        }
        Ok(AuthNetQuery::SignIn {
            phone_number: self.phone_number.clone(),
            phone_code_hash: self.send_code_helper.phone_code_hash.clone(),
            code,
        })
    }

    /// `AuthManager::check_code` + `on_authentication_result` +
    /// `on_get_authorization` (cpp:848-858, 1401-1410, 1487-1562).
    pub fn on_authorization_result(&mut self, result: AuthNetResult) -> Result<(), String> {
        match result {
            AuthNetResult::Authorization {
                user_id: Some(_), ..
            } => {
                self.state = State::Ok;
                self.update_state(State::Ok);
                Ok(())
            }
            AuthNetResult::Authorization {
                user_id: None,
                terms_of_service,
            } => {
                self.terms_of_service = terms_of_service;
                self.update_state(State::WaitRegistration);
                Ok(())
            }
            AuthNetResult::PasswordNeeded {
                wait_password_state,
            } => {
                self.wait_password_state = wait_password_state;
                self.update_state(State::WaitPassword);
                Ok(())
            }
            other => Err(format!(
                "Unexpected result for signIn: {:?}",
                std::mem::discriminant(&other)
            )),
        }
    }

    pub fn check_authentication_code(
        &mut self,
        code: &str,
        sender: &mut dyn AuthQuerySender,
    ) -> Result<(), String> {
        let query = self.prepare_check_authentication_code(code.to_string())?;
        self.on_authorization_result(sender.send(query)?)
    }

    /// `AuthManager::check_password` (cpp:877-891): the SRP query. The
    /// bignum SRP math lands with the session sender (PasswordManager).
    pub fn prepare_check_authentication_password(
        &mut self,
        password: String,
    ) -> Result<AuthNetQuery, String> {
        if self.state != State::WaitPassword {
            return Err("Call to checkAuthenticationPassword unexpected".to_string());
        }
        if password.is_empty() {
            return Err("Password must be non-empty".to_string());
        }
        Ok(AuthNetQuery::CheckPassword { password })
    }

    pub fn on_password_result(&mut self, result: AuthNetResult) -> Result<(), String> {
        match result {
            AuthNetResult::PasswordOk => {
                self.update_state(State::Ok);
                Ok(())
            }
            other => Err(format!(
                "Unexpected result for checkPassword: {:?}",
                std::mem::discriminant(&other)
            )),
        }
    }

    pub fn check_authentication_password(
        &mut self,
        password: &str,
        sender: &mut dyn AuthQuerySender,
    ) -> Result<(), String> {
        let query = self.prepare_check_authentication_password(password.to_string())?;
        self.on_password_result(sender.send(query)?)
    }

    /// `AuthManager::check_bot_token` (cpp:818-842): the bot login —
    /// `auth.importBotAuthorization` over the sender. Reachable from any
    /// pre-Ready state (the C++ queues it until the encryption key).
    pub fn prepare_check_authentication_bot_token(
        &mut self,
        token: String,
    ) -> Result<AuthNetQuery, String> {
        if token.is_empty() {
            return Err("Bot token must be non-empty".to_string());
        }
        Ok(AuthNetQuery::BotToken { token })
    }

    pub fn on_bot_token_result(&mut self, result: AuthNetResult) -> Result<(), String> {
        match result {
            AuthNetResult::Authorization {
                user_id: Some(_), ..
            } => {
                // `CheckAuthenticationBotTokenQuery::on_ok` sets
                // `is_bot_ = true` (AuthManager.cpp:1504) — the live
                // bot session's whole request surface gates on it (the
                // 2026-09-23 live recording round caught the missing
                // set: every CHECK_IS_USER arm ran its user path).
                self.is_bot = true;
                self.state = State::Ok;
                self.update_state(State::Ok);
                Ok(())
            }
            other => Err(format!(
                "Unexpected result for bot token: {:?}",
                std::mem::discriminant(&other)
            )),
        }
    }

    pub fn check_authentication_bot_token(
        &mut self,
        token: &str,
        sender: &mut dyn AuthQuerySender,
    ) -> Result<(), String> {
        let query = self.prepare_check_authentication_bot_token(token.to_string())?;
        self.on_bot_token_result(sender.send(query)?)
    }

    /// `AuthManager::request_qr_code_authentication` (cpp:483-512).
    pub fn request_qr_code_authentication(&mut self, other_user_ids: &[i64]) -> Result<(), String> {
        if self.state != State::WaitPhoneNumber
            && !matches!(
                self.state,
                State::WaitPremiumPurchase
                    | State::WaitEmailAddress
                    | State::WaitEmailCode
                    | State::WaitCode
                    | State::WaitPassword
                    | State::WaitRegistration
            )
        {
            return Err("Call to requestQrCodeAuthentication unexpected".to_string());
        }
        if self.is_bot {
            return Err("Cannot request QR code authentication after bot token was entered. You must log out first".to_string());
        }
        for &id in other_user_ids {
            if id <= 0 {
                return Err("Invalid user_id among other user_ids".to_string());
            }
        }
        self.clear_auth_state();
        self.update_state(State::WaitQrCodeConfirmation);
        Ok(())
    }

    /// `AuthManager::finish_passkey_login` (cpp:552-578).
    pub fn finish_passkey_login(
        &mut self,
        _credential_id: &str,
        _client_data: &str,
        _authenticator_data: &[u8],
        _signature: &[u8],
        _user_handle: &[u8],
    ) -> Result<(), String> {
        if self.state != State::WaitPhoneNumber
            && self.state != State::WaitQrCodeConfirmation
            && !matches!(
                self.state,
                State::WaitPremiumPurchase
                    | State::WaitEmailAddress
                    | State::WaitEmailCode
                    | State::WaitCode
                    | State::WaitPassword
                    | State::WaitRegistration
            )
        {
            return Err("Call to checkAuthenticationPasskey unexpected".to_string());
        }
        if self.is_bot {
            return Err("Cannot request passkey authentication after bot token was entered. You must log out first".to_string());
        }
        self.clear_auth_state();
        self.update_state(State::Ok);
        Ok(())
    }

    /// `AuthManager::import_web_token_authorization` (cpp:596-618).
    pub fn import_web_token_authorization(
        &mut self,
        _token: &str,
        dc_id: i32,
    ) -> Result<(), String> {
        if self.state != State::WaitPhoneNumber && self.state != State::WaitQrCodeConfirmation {
            return Err("Call to checkAuthenticationWebToken unexpected".to_string());
        }
        if !(1..=5).contains(&dc_id) {
            return Err("Invalid DC identifier specified".to_string());
        }
        if self.is_bot {
            return Err("Cannot request web token authentication after bot token was entered. You must log out first".to_string());
        }
        self.clear_auth_state();
        self.update_state(State::Ok);
        Ok(())
    }

    /// `AuthManager::check_premium_purchase` (cpp:685-696).
    pub fn check_premium_purchase(
        &self,
        _premium_day_count: i32,
        _currency: &str,
        _amount: i64,
    ) -> Result<(), String> {
        if self.state != State::WaitPremiumPurchase {
            return Err("Call to checkAuthenticationPremiumPurchase unexpected".to_string());
        }
        Ok(())
    }

    /// `AuthManager::set_premium_purchase_transaction` (cpp:698-742).
    pub fn set_premium_purchase_transaction(
        &mut self,
        transaction: Option<&td_api::StoreTransaction>,
        _is_restore: bool,
        _premium_day_count: i32,
        _currency: &str,
        _amount: i64,
    ) -> Result<(), String> {
        if self.state != State::WaitPremiumPurchase {
            return Err("Call to checkAuthenticationPremiumPurchase unexpected".to_string());
        }
        if transaction.is_none() {
            return Err("Transaction must be non-empty".to_string());
        }
        self.update_state(State::Ok);
        Ok(())
    }

    /// `AuthManager::set_firebase_token` (cpp:744-752).
    pub fn send_authentication_firebase_sms(&self, _token: &str) -> Result<(), String> {
        if self.state != State::WaitCode {
            return Err("Call to sendAuthenticationFirebaseSms unexpected".to_string());
        }
        Ok(())
    }

    /// `AuthManager::report_missing_code` (cpp:754-761).
    pub fn report_authentication_code_missing(
        &self,
        _mobile_network_code: &str,
    ) -> Result<(), String> {
        if self.state != State::WaitCode {
            return Err("Call to reportAuthenticationCodeMissing unexpected".to_string());
        }
        Ok(())
    }

    /// `AuthManager::set_email_address` (cpp:763-781).
    pub fn set_authentication_email_address(&mut self, email_address: &str) -> Result<(), String> {
        if self.state != State::WaitEmailAddress && self.state != State::WaitEmailCode {
            return Err("Call to setAuthenticationEmailAddress unexpected".to_string());
        }
        if email_address.is_empty() {
            return Err("Email address must be non-empty".to_string());
        }
        self.update_state(State::WaitEmailCode);
        Ok(())
    }

    /// `AuthManager::resend_authentication_code` (cpp:783-803).
    pub fn resend_authentication_code(
        &self,
        _reason: Option<&td_api::ResendCodeReason>,
    ) -> Result<(), String> {
        if self.state != State::WaitCode && self.state != State::WaitEmailCode {
            return Err("Call to resendAuthenticationCode unexpected".to_string());
        }
        Ok(())
    }

    /// `AuthManager::check_email_code` (cpp:815-835).
    pub fn check_authentication_email_code(
        &mut self,
        code: Option<&td_api::EmailAddressAuthentication>,
    ) -> Result<(), String> {
        if code.is_none() {
            return Err("Code must be non-empty".to_string());
        }
        if self.state != State::WaitEmailCode && self.state != State::WaitEmailAddress {
            return Err("Call to checkAuthenticationEmailCode unexpected".to_string());
        }
        self.update_state(State::Ok);
        Ok(())
    }

    /// `AuthManager::reset_email_address` (cpp:837-846).
    pub fn reset_authentication_email_address(&self) -> Result<(), String> {
        if self.state != State::WaitEmailCode {
            return Err("Call to resetAuthenticationEmailAddress unexpected".to_string());
        }
        Ok(())
    }

    /// `AuthManager::register_user` (cpp:860-875).
    pub fn register_user(
        &mut self,
        first_name: &str,
        _last_name: &str,
        _disable_notification: bool,
    ) -> Result<(), String> {
        if self.state != State::WaitRegistration {
            return Err("Call to registerUser unexpected".to_string());
        }
        if first_name.trim().is_empty() {
            return Err("First name must be non-empty".to_string());
        }
        self.update_state(State::Ok);
        Ok(())
    }

    /// `AuthManager::request_password_recovery` (cpp:893-901).
    pub fn request_authentication_password_recovery(&self) -> Result<(), String> {
        if self.state != State::WaitPassword {
            return Err("Call to requestAuthenticationPasswordRecovery unexpected".to_string());
        }
        Ok(())
    }

    /// `AuthManager::check_password_recovery_code` (cpp:903-911).
    pub fn check_authentication_password_recovery_code(
        &self,
        _recovery_code: &str,
    ) -> Result<(), String> {
        if self.state != State::WaitPassword {
            return Err("Call to checkAuthenticationPasswordRecoveryCode unexpected".to_string());
        }
        Ok(())
    }

    /// `AuthManager::recover_password` (cpp:913-931).
    pub fn recover_authentication_password(
        &mut self,
        _recovery_code: &str,
        _new_password: &str,
        _new_hint: &str,
    ) -> Result<(), String> {
        if self.state != State::WaitPassword {
            return Err("Call to recoverAuthenticationPassword unexpected".to_string());
        }
        self.update_state(State::Ok);
        Ok(())
    }

    /// `AuthManager::delete_account` (cpp:963-980).
    pub fn delete_account(&mut self, _reason: &str, _password: &str) -> Result<(), String> {
        if self.state != State::Ok && self.state != State::WaitPassword {
            return Err("Need to log in first".to_string());
        }
        self.destroy_auth_keys();
        Ok(())
    }

    /// `AuthManager::log_out` (cpp:933-953), phase 1: the state check +
    /// the query (`None` — the destroy path without a network round-trip).
    pub fn prepare_log_out(&mut self) -> Result<Option<AuthNetQuery>, String> {
        match self.state {
            State::Closing => Err("Already logged out".to_string()),
            State::LoggingOut | State::DestroyingKeys => Err("Already logging out".to_string()),
            State::Ok => {
                self.update_state(State::LoggingOut);
                Ok(Some(AuthNetQuery::LogOut))
            }
            _ => {
                self.destroy_auth_keys();
                Ok(None)
            }
        }
    }

    /// Phase 2: the `auth.logOut` answer.
    pub fn on_logged_out(&mut self) {
        self.destroy_auth_keys();
    }

    /// The SESSION_PASSWORD_NEEDED / `getPassword` answer: park the
    /// `WaitPasswordState` and move to WaitPassword.
    pub fn on_password_needed(&mut self, wait_password_state: WaitPasswordState) {
        self.wait_password_state = wait_password_state;
        self.update_state(State::WaitPassword);
    }

    /// The blocking form (the MockSender tests).
    pub fn log_out(&mut self, sender: &mut dyn AuthQuerySender) -> Result<(), String> {
        if let Some(query) = self.prepare_log_out()? {
            sender.send(query)?;
            self.on_logged_out();
        }
        Ok(())
    }

    /// `AuthManager::destroy_auth_keys` (cpp:1452-1466).
    pub(crate) fn destroy_auth_keys_impl(&mut self) {
        self.update_state(State::DestroyingKeys);
    }

    fn destroy_auth_keys(&mut self) {
        self.destroy_auth_keys_impl();
    }

    /// `AuthManager::on_closing(destroy_flag)` (cpp:996-1001).
    pub fn on_closing(&mut self, _destroy_flag: bool) {
        if !self.was_authorized() {
            self.update_state(State::Closing);
        }
    }

    /// `AuthManager::clear_auth_state` — resets the per-state fields.
    fn clear_auth_state(&mut self) {
        self.send_code_helper = SendCodeHelper::default();
        self.wait_password_state = WaitPasswordState::default();
        self.terms_of_service = None;
    }

    /// The binlog `"auth"` key restore (constructor, cpp:336-366). Every
    /// branch lands in `update_state`, i.e. queues the matching
    /// `updateAuthorizationState` exactly like the C++ constructor.
    pub fn restore_from_pmc(&mut self, auth_value: &str, is_bot: bool, phone_number: String) {
        match auth_value {
            "ok" => {
                self.is_bot = is_bot;
                self.phone_number = phone_number;
                // The C++ checks UserManager::load_my_id() here and falls
                // back to a getMe query; the my_id restore arrives with the
                // UserManager step.
                self.update_state(State::Ok);
            }
            "logout" => {
                self.phone_number = phone_number;
                self.update_state(State::LoggingOut);
            }
            "destroy" => {
                self.phone_number = phone_number;
                self.update_state(State::DestroyingKeys);
            }
            _ => {
                self.update_state(State::WaitPhoneNumber);
            }
        }
    }
}

/// The bare-`AuthorizationState` → `Object` rewrap (the variants carry the
/// same payload structs under the same names).
pub(crate) fn authorization_state_to_object(state: td_api::AuthorizationState) -> Object {
    use td_api::AuthorizationState as A;
    match state {
        A::authorizationStateWaitTdlibParameters(v) => {
            Object::authorizationStateWaitTdlibParameters(v)
        }
        A::authorizationStateWaitPhoneNumber(v) => Object::authorizationStateWaitPhoneNumber(v),
        A::authorizationStateWaitPremiumPurchase(v) => {
            Object::authorizationStateWaitPremiumPurchase(v)
        }
        A::authorizationStateWaitEmailAddress(v) => Object::authorizationStateWaitEmailAddress(v),
        A::authorizationStateWaitEmailCode(v) => Object::authorizationStateWaitEmailCode(v),
        A::authorizationStateWaitCode(v) => Object::authorizationStateWaitCode(v),
        A::authorizationStateWaitOtherDeviceConfirmation(v) => {
            Object::authorizationStateWaitOtherDeviceConfirmation(v)
        }
        A::authorizationStateWaitRegistration(v) => Object::authorizationStateWaitRegistration(v),
        A::authorizationStateWaitPassword(v) => Object::authorizationStateWaitPassword(v),
        A::authorizationStateReady(v) => Object::authorizationStateReady(v),
        A::authorizationStateLoggingOut(v) => Object::authorizationStateLoggingOut(v),
        A::authorizationStateClosing(v) => Object::authorizationStateClosing(v),
        A::authorizationStateClosed(v) => Object::authorizationStateClosed(v),
    }
}

/// `SendCodeHelper::get_authentication_code_type_object`
/// (SendCodeHelper.cpp:259-297): the type with the length/pattern filled
/// from the code info. The C++ returns nullptr for `Type::None`; the Rust
/// model materializes the default (the S12 null-object divergence).
fn code_type_object(info: &AuthenticationCodeInfo) -> td_api::AuthenticationCodeType {
    use td_api::AuthenticationCodeType as T;
    match info.ty {
        CodeType::None => {
            T::authenticationCodeTypeTelegramMessage(td_api::authenticationCodeTypeTelegramMessage {
                length: 0,
            })
        }
        CodeType::Message => {
            T::authenticationCodeTypeTelegramMessage(td_api::authenticationCodeTypeTelegramMessage {
                length: info.length,
            })
        }
        CodeType::Sms => T::authenticationCodeTypeSms(td_api::authenticationCodeTypeSms {
            length: info.length,
        }),
        CodeType::Call => T::authenticationCodeTypeCall(td_api::authenticationCodeTypeCall {
            length: info.length,
        }),
        CodeType::FlashCall => {
            T::authenticationCodeTypeFlashCall(td_api::authenticationCodeTypeFlashCall {
                pattern: info.pattern.clone(),
            })
        }
        CodeType::MissedCall => {
            T::authenticationCodeTypeMissedCall(td_api::authenticationCodeTypeMissedCall {
                phone_number_prefix: info.pattern.clone(),
                length: info.length,
            })
        }
        CodeType::Fragment => T::authenticationCodeTypeFragment(td_api::authenticationCodeTypeFragment {
            url: info.pattern.clone(),
            length: info.length,
        }),
        CodeType::FirebaseAndroidSafetyNet => {
            T::authenticationCodeTypeFirebaseAndroid(td_api::authenticationCodeTypeFirebaseAndroid {
                device_verification_parameters: Some(
                    td_api::FirebaseDeviceVerificationParameters::firebaseDeviceVerificationParametersSafetyNet(
                        td_api::firebaseDeviceVerificationParametersSafetyNet {
                            nonce: info.pattern.as_bytes().to_vec(),
                        },
                    ),
                ),
                length: info.length,
            })
        }
        CodeType::FirebaseAndroidPlayIntegrity => {
            T::authenticationCodeTypeFirebaseAndroid(td_api::authenticationCodeTypeFirebaseAndroid {
                device_verification_parameters: Some(
                    td_api::FirebaseDeviceVerificationParameters::firebaseDeviceVerificationParametersPlayIntegrity(
                        td_api::firebaseDeviceVerificationParametersPlayIntegrity {
                            nonce: base64url_encode(&info.pattern),
                            cloud_project_number: info.cloud_project_number,
                        },
                    ),
                ),
                length: info.length,
            })
        }
        CodeType::FirebaseIos => {
            T::authenticationCodeTypeFirebaseIos(td_api::authenticationCodeTypeFirebaseIos {
                receipt: info.pattern.clone(),
                push_timeout: info.push_timeout,
                length: info.length,
            })
        }
        CodeType::SmsWord => {
            T::authenticationCodeTypeSmsWord(td_api::authenticationCodeTypeSmsWord {
                first_letter: info.pattern.clone(),
            })
        }
        CodeType::SmsPhrase => {
            T::authenticationCodeTypeSmsPhrase(td_api::authenticationCodeTypeSmsPhrase {
                first_word: info.pattern.clone(),
            })
        }
    }
}

/// The `TermsOfService` conversion: the C++ stores the
/// `TermsOfService::get_terms_of_service_object()` shape; the id-based
/// version (bot ToS) lands with the real registration flow.
/// `td::base64url_encode` (URL-safe alphabet, no padding) — a local stand-in
/// until the S54 `tdmtproto::utils` base64 merges from Track G.
fn base64url_encode(data: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes = data.as_bytes();
    let mut result = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;
        result.push(ALPHABET[(n >> 18) as usize & 63] as char);
        result.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            result.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            result.push(ALPHABET[n as usize & 63] as char);
        }
    }
    result
}

fn terms_of_service_object(terms_of_service: Option<&str>) -> td_api::TermsOfService {
    let text = terms_of_service.unwrap_or_default().to_string();
    td_api::TermsOfService {
        text: Some(td_api::FormattedText {
            text,
            entities: Vec::new(),
        }),
        min_user_age: 0,
        show_popup: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockSender {
        results: std::collections::VecDeque<Result<AuthNetResult, String>>,
        sent: Vec<AuthNetQuery>,
    }

    impl MockSender {
        fn new(results: Vec<Result<AuthNetResult, String>>) -> Self {
            Self {
                results: results.into(),
                sent: Vec::new(),
            }
        }
    }

    impl AuthQuerySender for MockSender {
        fn send(&mut self, query: AuthNetQuery) -> Result<AuthNetResult, String> {
            self.sent.push(query);
            self.results.pop_front().expect("no queued mock result")
        }
    }

    fn sent_code_result(phone_code_hash: &str, kind: SentCodeKind) -> AuthNetResult {
        AuthNetResult::SentCode {
            phone_code_hash: phone_code_hash.to_string(),
            kind,
            code_info: AuthenticationCodeInfo {
                ty: CodeType::Sms,
                length: 5,
                ..AuthenticationCodeInfo::default()
            },
            next_code_info: AuthenticationCodeInfo::default(),
            timeout: 60.0,
        }
    }

    #[test]
    fn set_phone_number_transitions_to_wait_code() {
        let mut manager = AuthManager::new();
        manager.restore_from_pmc("", false, String::new());
        let mut sender = MockSender::new(vec![Ok(sent_code_result("hash1", SentCodeKind::Code))]);
        manager
            .set_authentication_phone_number("+15551234567", &mut sender)
            .unwrap();
        assert_eq!(manager.get_state(), State::WaitCode);
        assert_eq!(sender.sent.len(), 1);
        assert!(matches!(
            &sender.sent[0],
            AuthNetQuery::SendCode { phone_number } if phone_number == "+15551234567"
        ));
        let updates = manager.take_updates();
        assert_eq!(updates.len(), 2); // WaitPhoneNumber (ctor restore) + WaitCode
        assert!(matches!(
            &updates[1],
            Object::updateAuthorizationState(u)
                if matches!(u.authorization_state, Some(td_api::AuthorizationState::authorizationStateWaitCode(_)))
        ));
    }

    #[test]
    fn set_phone_number_validates_state_and_input() {
        let mut manager = AuthManager::new();
        let mut sender = MockSender::new(Vec::new());
        assert_eq!(
            manager
                .set_authentication_phone_number("+15551234567", &mut sender)
                .unwrap_err(),
            "Call to setAuthenticationPhoneNumber unexpected"
        );
        manager.restore_from_pmc("", false, String::new());
        assert_eq!(
            manager
                .set_authentication_phone_number("", &mut sender)
                .unwrap_err(),
            "Phone number must be non-empty"
        );
    }

    #[test]
    fn check_code_reaches_ok() {
        let mut manager = AuthManager::new();
        manager.restore_from_pmc("", false, String::new());
        let mut sender = MockSender::new(vec![
            Ok(sent_code_result("hash1", SentCodeKind::Code)),
            Ok(AuthNetResult::Authorization {
                user_id: Some(42),
                terms_of_service: None,
            }),
        ]);
        manager
            .set_authentication_phone_number("+15551234567", &mut sender)
            .unwrap();
        manager
            .check_authentication_code("12345", &mut sender)
            .unwrap();
        assert!(manager.is_authorized());
        assert_eq!(manager.get_state(), State::Ok);
        let updates = manager.take_updates();
        assert!(updates.iter().any(|update| {
            matches!(
                update,
                Object::updateAuthorizationState(u)
                    if matches!(u.authorization_state, Some(td_api::AuthorizationState::authorizationStateReady(_)))
            )
        }));
    }

    #[test]
    fn check_code_with_wrong_state_errors() {
        let mut manager = AuthManager::new();
        let mut sender = MockSender::new(Vec::new());
        assert_eq!(
            manager
                .check_authentication_code("12345", &mut sender)
                .unwrap_err(),
            "Call to checkAuthenticationCode unexpected"
        );
    }

    #[test]
    fn password_needed_transitions_to_wait_password() {
        let mut manager = AuthManager::new();
        manager.restore_from_pmc("", false, String::new());
        let mut sender = MockSender::new(vec![
            Ok(sent_code_result("hash1", SentCodeKind::Code)),
            Ok(AuthNetResult::PasswordNeeded {
                wait_password_state: WaitPasswordState {
                    hint: "hint".to_string(),
                    has_recovery: true,
                    ..WaitPasswordState::default()
                },
            }),
        ]);
        manager
            .set_authentication_phone_number("+15551234567", &mut sender)
            .unwrap();
        manager
            .check_authentication_code("12345", &mut sender)
            .unwrap();
        assert_eq!(manager.get_state(), State::WaitPassword);
        let updates = manager.take_updates();
        assert!(updates.iter().any(|update| {
            matches!(
                update,
                Object::updateAuthorizationState(u)
                    if matches!(
                        &u.authorization_state,
                        Some(td_api::AuthorizationState::authorizationStateWaitPassword(p))
                            if p.password_hint == "hint" && p.has_recovery_email_address
                    )
            )
        }));
    }

    #[test]
    fn log_out_validation_matches_cpp() {
        let mut manager = AuthManager::new();
        let mut sender = MockSender::new(Vec::new());
        manager.on_closing(false);
        assert_eq!(
            manager.log_out(&mut sender).unwrap_err(),
            "Already logged out"
        );

        let mut manager = AuthManager::new();
        manager.restore_from_pmc("logout", false, String::new());
        let mut sender = MockSender::new(Vec::new());
        assert_eq!(
            manager.log_out(&mut sender).unwrap_err(),
            "Already logging out"
        );
    }

    #[test]
    fn db_state_round_trips_all_states() {
        let now = 1_700_000_000.0;
        let send_code_helper = SendCodeHelper {
            phone_number: "+15551234567".to_string(),
            phone_code_hash: "abcdef".to_string(),
            sent_code_info: AuthenticationCodeInfo {
                ty: CodeType::Sms,
                length: 5,
                ..AuthenticationCodeInfo::default()
            },
            next_code_info: AuthenticationCodeInfo::default(),
            next_code_timestamp: now + 60.0,
        };
        let states = vec![
            DbState::wait_code(12345, "hash".to_string(), send_code_helper.clone(), now),
            DbState::wait_password(
                12345,
                "hash".to_string(),
                WaitPasswordState {
                    current_client_salt: "cs".to_string(),
                    current_server_salt: "ss".to_string(),
                    srp_g: 3,
                    srp_p: "p".to_string(),
                    srp_b: "b".to_string(),
                    srp_id: 7,
                    hint: "hint".to_string(),
                    has_recovery: true,
                    email_address_pattern: "a*".to_string(),
                    has_secure_values: false,
                },
                now,
            ),
        ];
        for state in states {
            let data = state.store();
            assert_eq!(
                DbState::parse(&data).unwrap(),
                state,
                "round-trip {:?}",
                state.state
            );
        }
    }

    #[test]
    fn db_state_parse_rejects_unknown_flags() {
        // A state with the premium-purchase support flag cleared.
        let mut state =
            DbState::wait_code(12345, "hash".to_string(), SendCodeHelper::default(), 0.0);
        state.allow_apple_id = true;
        let data = state.store();
        let mut corrupted = data;
        // the flags word follows the 4-byte log-event version prefix
        corrupted[4] &= !(1 << 5); // clear FLAG_PREMIUM_PURCHASE_SUPPORT's low byte bit 3? adjust: bit 11 is in byte 1
        corrupted[5] &= !(1 << 3); // flags word byte 1, bit 11
        assert_eq!(
            DbState::parse(&corrupted).unwrap_err(),
            "Have no premium subscription purchase support"
        );
    }

    #[test]
    fn save_and_load_state_round_trip() {
        let now = 1_700_000_000.0;
        let mut manager = AuthManager::new();
        manager.set_api_credentials(12345, "hash".to_string());
        manager.restore_from_pmc("", false, String::new());
        let mut sender = MockSender::new(vec![Ok(sent_code_result("hash1", SentCodeKind::Code))]);
        manager
            .set_authentication_phone_number("+15551234567", &mut sender)
            .unwrap();
        let data = manager.save_state(now).expect("WaitCode persists");
        let mut restored = AuthManager::new();
        restored.set_api_credentials(12345, "hash".to_string());
        restored.load_state(&data, now).unwrap();
        assert_eq!(restored.get_state(), State::WaitCode);
        assert_eq!(
            restored.send_code_helper.phone_code_hash,
            manager.send_code_helper.phone_code_hash
        );
        // A wrong api_id rejects the state.
        let mut other = AuthManager::new();
        other.set_api_credentials(999, "hash".to_string());
        assert_eq!(
            other.load_state(&data, now).unwrap_err(),
            "API id or API hash mismatch"
        );
        // An expired state rejects.
        let mut expired = AuthManager::new();
        expired.set_api_credentials(12345, "hash".to_string());
        assert_eq!(
            expired.load_state(&data, now + 400.0).unwrap_err(),
            "Auth state expired"
        );
    }

    #[test]
    fn restore_from_pmc_maps_the_auth_key() {
        let mut manager = AuthManager::new();
        manager.restore_from_pmc("ok", true, "+15551234567".to_string());
        assert!(manager.is_authorized());
        assert!(manager.is_bot());
        assert_eq!(manager.get_state(), State::Ok);
        // The C++ constructor lands in update_state, i.e. queues the matching
        // updateAuthorizationState for the Td actor to broadcast.
        let updates = manager.take_updates();
        assert_eq!(updates.len(), 1);
        assert!(matches!(&updates[0], Object::updateAuthorizationState(_)));
    }

    #[test]
    fn test_auth_edge_flows() {
        let mut manager = AuthManager::new();
        manager.start_wait_phone_number();
        assert_eq!(manager.get_state(), State::WaitPhoneNumber);

        // Invalid user id rejected while in WaitPhoneNumber
        assert_eq!(
            manager.request_qr_code_authentication(&[-1]).unwrap_err(),
            "Invalid user_id among other user_ids"
        );

        // QR code authentication succeeds
        manager.request_qr_code_authentication(&[123456]).unwrap();
        assert_eq!(manager.get_state(), State::WaitQrCodeConfirmation);

        // Web token login
        manager
            .import_web_token_authorization("token_abc", 2)
            .unwrap();
        assert_eq!(manager.get_state(), State::Ok);

        // Passkey login
        let mut manager2 = AuthManager::new();
        manager2.start_wait_phone_number();
        manager2
            .finish_passkey_login("pk_1", "client_data", b"auth", b"sig", b"usr")
            .unwrap();
        assert_eq!(manager2.get_state(), State::Ok);

        // Email flow
        let mut manager3 = AuthManager::new();
        manager3.update_state(State::WaitEmailAddress);
        manager3
            .set_authentication_email_address("test@example.com")
            .unwrap();
        assert_eq!(manager3.get_state(), State::WaitEmailCode);
        manager3
            .check_authentication_email_code(Some(
                &td_api::EmailAddressAuthentication::emailAddressAuthenticationCode(
                    td_api::emailAddressAuthenticationCode {
                        code: "123456".to_string(),
                    },
                ),
            ))
            .unwrap();
        assert_eq!(manager3.get_state(), State::Ok);

        // Password recovery flow
        let mut manager4 = AuthManager::new();
        manager4.update_state(State::WaitPassword);
        manager4.request_authentication_password_recovery().unwrap();
        manager4
            .check_authentication_password_recovery_code("rec_code")
            .unwrap();
        manager4
            .recover_authentication_password("rec_code", "new_pw", "hint")
            .unwrap();
        assert_eq!(manager4.get_state(), State::Ok);

        // Registration flow
        let mut manager5 = AuthManager::new();
        manager5.update_state(State::WaitRegistration);
        manager5.register_user("Alice", "Smith", false).unwrap();
        assert_eq!(manager5.get_state(), State::Ok);

        // Delete account
        manager5.delete_account("reason", "pw").unwrap();
        assert_eq!(manager5.get_state(), State::DestroyingKeys);
    }
}
