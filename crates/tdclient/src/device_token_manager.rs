//! `DeviceTokenManager` (DeviceTokenManager.cpp/h) — the push-device
//! token registry feeding the M.13.6b push pipeline (M.13S.3).
//!
//! The C++ actor collapses into a plain struct owned by
//! [`Td`](crate::td::Td): `start_up` becomes the explicit
//! [`load_states`](DeviceTokenManager::load_states) call, the net query
//! dispatch in `loop` is a `[blocked: net]` seam that marks the token
//! in-flight (the `u64::MAX` sentinel net-query id), and `on_result` is
//! driven by the net step once it lands. The `sync_cnt`/`force_sync`
//! promise hop collapses into the synchronous pmc write (the gate only
//! matters once `loop` really sends).

use crate::misc::clean_input_string;
use crate::notification_manager::NotificationManagerEnv;
use crate::store_parse::{Parser, store_flags, store_i32, store_i64, store_string};
use crate::td_api;
use tdmtproto::dh_handshake::DhHandshake;
use tdutils::promise::Promise;
use tdutils::random::secure_bytes;
use tdutils::status::Status;

/// The `TokenType` enum (DeviceTokenManager.h:23-38) — the pmc keys and
/// the wire enum value share the numbering; index 0 stays unused so the
/// ids line up with the C++ array.
pub const TOKEN_TYPE_SIZE: usize = 14;

const MIN_ENCRYPTION_KEY_ID: i64 = 10_000_000_000_000;
const ENCRYPTION_KEY_LENGTH: usize = 256;

/// `TokenInfo::State` (DeviceTokenManager.h:33).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TokenState {
    #[default]
    Sync,
    Unregister,
    Register,
    Reregister,
}

/// `DeviceTokenManager::TokenInfo` (DeviceTokenManager.h:34-53); the
/// promise is the register request waiting for the server ack.
pub struct TokenInfo {
    pub state: TokenState,
    pub token: String,
    pub net_query_id: u64,
    pub other_user_ids: Vec<i64>,
    pub is_app_sandbox: bool,
    pub encrypt: bool,
    pub encryption_key: Vec<u8>,
    pub encryption_key_id: i64,
    pub promise: Option<Promise<td_api::PushReceiverId>>,
}

impl Default for TokenInfo {
    fn default() -> Self {
        TokenInfo {
            state: TokenState::Sync,
            token: String::new(),
            net_query_id: 0,
            other_user_ids: Vec::new(),
            is_app_sandbox: false,
            encrypt: false,
            encryption_key: Vec::new(),
            encryption_key_id: 0,
            promise: None,
        }
    }
}

impl TokenInfo {
    /// The `TokenInfo::store` half (DeviceTokenManager.cpp:35-60) — the
    /// flags are `false, is_sync, is_unregister, is_register,
    /// is_app_sandbox, encrypt, has_other_user_ids`; stored WITHOUT the
    /// log-event version prefix (`td::serialize`).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_other_user_ids = !self.other_user_ids.is_empty();
        let is_sync = self.state == TokenState::Sync;
        let is_unregister = self.state == TokenState::Unregister;
        let is_register = self.state == TokenState::Register;
        debug_assert!(self.state != TokenState::Reregister, "the C++ CHECKs");
        store_flags(
            out,
            [
                false,
                is_sync,
                is_unregister,
                is_register,
                self.is_app_sandbox,
                self.encrypt,
                has_other_user_ids,
            ],
        );
        store_string(out, &self.token);
        if has_other_user_ids {
            store_i32(out, self.other_user_ids.len() as i32);
            for user_id in &self.other_user_ids {
                store_i64(out, *user_id);
            }
        }
        if self.encrypt {
            store_string(out, &String::from_utf8_lossy(&self.encryption_key));
            store_i64(out, self.encryption_key_id);
        }
    }

    /// The `TokenInfo::parse` half (DeviceTokenManager.cpp:62-100).
    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let [
            has_other_user_ids_legacy,
            is_sync,
            is_unregister,
            is_register,
            is_app_sandbox,
            encrypt,
            has_other_user_ids,
        ] = parser.parse_flags::<7>()?;
        if (is_sync as i32) + (is_unregister as i32) + (is_register as i32) != 1 {
            return Err("Invalid token state flags");
        }
        let state = if is_sync {
            TokenState::Sync
        } else if is_unregister {
            TokenState::Unregister
        } else {
            TokenState::Register
        };
        let token = parser.string()?;
        let mut other_user_ids = Vec::new();
        if has_other_user_ids_legacy {
            let size = parser.i32_()?;
            if size < 0 || parser.left_len() < size as usize * 4 {
                return Err("Wrong vector length");
            }
            for _ in 0..size {
                other_user_ids.push(i64::from(parser.i32_()?));
            }
        }
        if has_other_user_ids {
            let size = parser.i32_()?;
            if size < 0 || parser.left_len() < size as usize * 8 {
                return Err("Wrong vector length");
            }
            for _ in 0..size {
                other_user_ids.push(parser.i64_()?);
            }
        }
        let mut encryption_key = Vec::new();
        let mut encryption_key_id = 0;
        if encrypt {
            encryption_key = parser.string()?.into_bytes();
            encryption_key_id = parser.i64_()?;
        }
        Ok(TokenInfo {
            state,
            token,
            net_query_id: 0,
            other_user_ids,
            is_app_sandbox,
            encrypt,
            encryption_key,
            encryption_key_id,
            promise: None,
        })
    }
}

/// `is_base64url` (base64.cpp:150-202, the `is_url` instantiation) — the
/// trailing padding rules plus the url-safe alphabet and the
/// last-character bit checks.
fn is_base64url(mut input: &[u8]) -> bool {
    let mut padding_length = 0usize;
    while let Some(&b'=') = input.last() {
        input = &input[..input.len() - 1];
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

    let table = |c: u8| -> u8 {
        match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => 64,
        }
    };
    for &c in input {
        if table(c) == 64 {
            return false;
        }
    }

    if input.len() & 3 == 2 {
        let value = table(*input.last().expect("len % 4 == 2 implies non-empty"));
        if value & 15 != 0 {
            return false;
        }
    }
    if input.len() & 3 == 3 {
        let value = table(*input.last().expect("len % 4 == 3 implies non-empty"));
        if value & 3 != 0 {
            return false;
        }
    }
    true
}

/// The `G()` surface of the manager.
pub trait DeviceTokenManagerEnv: NotificationManagerEnv {
    /// `G()->td_db()->get_binlog_pmc()->get` for the binary token blobs.
    fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;

    /// `G()->td_db()->get_binlog_pmc()->set` for the binary token blobs.
    fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]);
}

/// The collapsed manager state.
pub struct DeviceTokenManager {
    tokens: Vec<TokenInfo>,
    /// The `set_timeout_in(clamp(retry_after, 1, 3600))` Reregister retry
    /// (DeviceTokenManager.cpp:457) — a single slot, monotonic seconds.
    retry_deadline: Option<f64>,
}

impl Default for DeviceTokenManager {
    fn default() -> Self {
        DeviceTokenManager {
            tokens: (0..TOKEN_TYPE_SIZE).map(|_| TokenInfo::default()).collect(),
            retry_deadline: None,
        }
    }
}

impl DeviceTokenManager {
    pub fn new() -> Self {
        DeviceTokenManager::default()
    }

    /// `start_up` (DeviceTokenManager.cpp:329-365) — the pmc load with the
    /// legacy one-character formats; a loaded Sync token with a value
    /// becomes Reregister.
    pub fn load_states(&mut self, env: &mut impl DeviceTokenManagerEnv) {
        for token_type in 1..TOKEN_TYPE_SIZE as i32 {
            let key = format!("device_token{token_type}");
            let serialized = env.pmc_get_raw(key.as_bytes());
            if serialized.is_empty() {
                continue;
            }

            let token = &mut self.tokens[token_type as usize];
            match serialized[0] {
                b'*' => match TokenInfo::parse(&mut Parser::new(&serialized[1..])) {
                    Ok(parsed) => *token = parsed,
                    Err(error) => {
                        *token = TokenInfo::default();
                        tdutils::log_error!("Invalid serialized TokenInfo: {error}");
                        continue;
                    }
                },
                // legacy
                b'+' => token.state = TokenState::Register,
                b'-' => token.state = TokenState::Unregister,
                b'=' => token.state = TokenState::Sync,
                _ => {
                    tdutils::log_error!("Invalid serialized TokenInfo");
                    continue;
                }
            }
            if serialized[0] != b'*' {
                token.token = String::from_utf8_lossy(&serialized[1..]).to_string();
            }
            if token.state == TokenState::Sync && !token.token.is_empty() {
                token.state = TokenState::Reregister;
            }
        }
        self.loop_(env);
    }

    /// `save_info` (DeviceTokenManager.cpp:367-377) without the
    /// force_sync promise hop (the sync_cnt gate only matters once the
    /// net loop really sends).
    fn save_info(&mut self, env: &mut impl DeviceTokenManagerEnv, token_type: i32) {
        let key = format!("device_token{token_type}");
        let info = &self.tokens[token_type as usize];
        if info.token.is_empty() {
            env.pmc_erase(&key);
        } else {
            let mut data = vec![b'*'];
            info.store(&mut data);
            env.pmc_set_raw(key.as_bytes(), &data);
        }
    }

    /// `register_device` (DeviceTokenManager.cpp:132-298).
    pub fn register_device(
        &mut self,
        env: &mut impl DeviceTokenManagerEnv,
        device_token: Option<td_api::DeviceToken>,
        other_user_ids: &[i64],
        mut promise: Promise<td_api::PushReceiverId>,
    ) {
        let Some(device_token) = device_token else {
            return promise.set_error(Status::error(400, "Device token must be non-empty"));
        };
        let token_type;
        let mut token = String::new();
        let mut is_app_sandbox = false;
        let mut encrypt = false;
        match &device_token {
            td_api::DeviceToken::deviceTokenApplePush(t) => {
                token = t.device_token.clone();
                token_type = 1; // Apns
                is_app_sandbox = t.is_app_sandbox;
            }
            td_api::DeviceToken::deviceTokenFirebaseCloudMessaging(t) => {
                token = t.token.clone();
                token_type = 2; // Fcm
                encrypt = t.encrypt;
            }
            td_api::DeviceToken::deviceTokenMicrosoftPush(t) => {
                token = t.channel_uri.clone();
                token_type = 3; // Mpns
            }
            td_api::DeviceToken::deviceTokenSimplePush(t) => {
                token = t.endpoint.clone();
                token_type = 4; // SimplePush
            }
            td_api::DeviceToken::deviceTokenUbuntuPush(t) => {
                token = t.token.clone();
                token_type = 5; // UbuntuPhone
            }
            td_api::DeviceToken::deviceTokenBlackBerryPush(t) => {
                token = t.token.clone();
                token_type = 6; // BlackBerry
            }
            td_api::DeviceToken::deviceTokenWindowsPush(t) => {
                token = t.access_token.clone();
                token_type = 8; // Wns
            }
            td_api::DeviceToken::deviceTokenApplePushVoIP(t) => {
                token = t.device_token.clone();
                token_type = 9; // ApnsVoip
                is_app_sandbox = t.is_app_sandbox;
                encrypt = t.encrypt;
            }
            td_api::DeviceToken::deviceTokenWebPush(t) => {
                if t.endpoint.contains(',') {
                    return promise.set_error(Status::error(400, "Illegal endpoint value"));
                }
                if !is_base64url(t.p256dh_base64url.as_bytes()) {
                    return promise
                        .set_error(Status::error(400, "Public key must be base64url-encoded"));
                }
                if !is_base64url(t.auth_base64url.as_bytes()) {
                    return promise.set_error(Status::error(
                        400,
                        "Authentication secret must be base64url-encoded",
                    ));
                }
                let Some(cleaned) = clean_input_string(&t.endpoint) else {
                    return promise
                        .set_error(Status::error(400, "Endpoint must be encoded in UTF-8"));
                };
                if !cleaned.is_empty() {
                    token = tdutils::json::json_encode(
                        &tdutils::json::JsonValue::Object(tdutils::json::JsonObject::from_fields(
                            vec![
                                (
                                    "endpoint".to_owned(),
                                    tdutils::json::JsonValue::String(cleaned),
                                ),
                                (
                                    "keys".to_owned(),
                                    tdutils::json::JsonValue::Object(
                                        tdutils::json::JsonObject::from_fields(vec![
                                            (
                                                "p256dh".to_owned(),
                                                tdutils::json::JsonValue::String(
                                                    t.p256dh_base64url.clone(),
                                                ),
                                            ),
                                            (
                                                "auth".to_owned(),
                                                tdutils::json::JsonValue::String(
                                                    t.auth_base64url.clone(),
                                                ),
                                            ),
                                        ]),
                                    ),
                                ),
                            ],
                        )),
                        false,
                    );
                }
                token_type = 10; // WebPush
            }
            td_api::DeviceToken::deviceTokenMicrosoftPushVoIP(t) => {
                token = t.channel_uri.clone();
                token_type = 11; // MpnsVoip
            }
            td_api::DeviceToken::deviceTokenTizenPush(t) => {
                token = t.reg_id.clone();
                token_type = 12; // Tizen
            }
            td_api::DeviceToken::deviceTokenHuaweiPush(t) => {
                token = t.token.clone();
                token_type = 13; // Huawei
                encrypt = t.encrypt;
            } // `deviceTokenSimplePush`-family drift guard: the generated
              // enum is exhaustive over the scheme, the C++ default is
              // UNREACHABLE
        }

        let Some(cleaned_token) = clean_input_string(&token) else {
            return promise.set_error(Status::error(400, "Device token must be encoded in UTF-8"));
        };
        token = cleaned_token;
        for &user_id in other_user_ids {
            // `UserId::is_valid()` — positive int53
            if user_id <= 0 {
                return promise
                    .set_error(Status::error(400, "Invalid user_id among other user_ids"));
            }
        }
        let input_user_ids = other_user_ids.to_vec();

        let info = &mut self.tokens[token_type];
        if token.is_empty() {
            if info.token.is_empty() {
                // already unregistered
                return promise.set_value(td_api::PushReceiverId { id: 0 });
            }
            info.state = TokenState::Unregister;
        } else {
            let same = (info.state == TokenState::Reregister || info.state == TokenState::Sync)
                && info.token == token
                && info.other_user_ids == input_user_ids
                && info.is_app_sandbox == is_app_sandbox
                && encrypt == info.encrypt;
            if same {
                let push_token_id = if encrypt {
                    info.encryption_key_id
                } else {
                    env.get_option_integer("my_id", 0)
                };
                return promise.set_value(td_api::PushReceiverId { id: push_token_id });
            }
            info.state = TokenState::Register;
            info.token = token;
        }
        info.net_query_id = 0;
        info.other_user_ids = input_user_ids;
        info.is_app_sandbox = is_app_sandbox;
        if encrypt != info.encrypt {
            if encrypt {
                let mut encryption_key = vec![0u8; ENCRYPTION_KEY_LENGTH];
                loop {
                    secure_bytes(&mut encryption_key);
                    let encryption_key_id = DhHandshake::calc_key_id(&encryption_key);
                    if encryption_key_id <= -MIN_ENCRYPTION_KEY_ID
                        || encryption_key_id >= MIN_ENCRYPTION_KEY_ID
                    {
                        // ensure that encryption key ID never collide with anything
                        info.encryption_key_id = encryption_key_id;
                        break;
                    }
                }
                info.encryption_key = encryption_key;
            } else {
                info.encryption_key.clear();
                info.encryption_key_id = 0;
            }
            info.encrypt = encrypt;
        }
        if let Some(mut old_promise) = info.promise.take() {
            old_promise.set_value(td_api::PushReceiverId { id: 0 });
        }
        info.promise = Some(promise);
        self.save_info(env, token_type as i32);
        // the C++ reaches `loop` through the save_info force_sync promise
        // (dec_sync_cnt); the collapsed write is synchronous
        self.loop_(env);
    }

    /// `reregister_device` (DeviceTokenManager.cpp:300-308).
    pub fn reregister_device(&mut self, env: &mut impl DeviceTokenManagerEnv) {
        for token_type in 1..TOKEN_TYPE_SIZE {
            let token = &mut self.tokens[token_type];
            if token.state == TokenState::Sync && !token.token.is_empty() {
                token.state = TokenState::Reregister;
            }
        }
        self.loop_(env);
    }

    /// `get_encryption_keys` (DeviceTokenManager.cpp:310-323).
    pub fn get_encryption_keys(&self, env: &impl DeviceTokenManagerEnv) -> Vec<(i64, Vec<u8>)> {
        let mut result = Vec::new();
        for token_type in 1..TOKEN_TYPE_SIZE {
            let info = &self.tokens[token_type];
            if !info.token.is_empty() && info.state != TokenState::Unregister {
                if info.encrypt {
                    result.push((info.encryption_key_id, info.encryption_key.clone()));
                } else {
                    result.push((env.get_option_integer("my_id", 0), Vec::new()));
                }
            }
        }
        result
    }

    /// `loop` (DeviceTokenManager.cpp:384-410) — the dispatch is the net
    /// seam: an in-flight mark (`u64::MAX`) stands in for the sent query.
    pub fn loop_(&mut self, env: &mut impl DeviceTokenManagerEnv) {
        if env.close_flag() {
            return;
        }
        for token_type in 1..TOKEN_TYPE_SIZE {
            let info = &self.tokens[token_type];
            if info.state == TokenState::Sync || info.net_query_id != 0 {
                continue;
            }
            // have to send query — account_registerDevice/account_unregisterDevice
            tdutils::log_info!("Device token {token_type} sync query — [blocked: net]");
            let info = &mut self.tokens[token_type];
            info.net_query_id = u64::MAX;
        }
    }

    /// `on_result` (DeviceTokenManager.cpp:412-467) over a fetched
    /// register/unregister result; the net step drives this.
    pub fn on_register_device_result(
        &mut self,
        env: &mut impl DeviceTokenManagerEnv,
        token_type: i32,
        net_query_id: u64,
        r_flag: Result<bool, Status>,
    ) {
        let info = &mut self.tokens[token_type as usize];
        if info.net_query_id != net_query_id {
            return;
        }
        info.net_query_id = 0;
        let mut retry_after = 0i64;

        if let Ok(true) = &r_flag {
            if let Some(mut promise) = info.promise.take() {
                let mut push_token_id = 0;
                if info.state == TokenState::Register {
                    push_token_id = if info.encrypt {
                        info.encryption_key_id
                    } else {
                        env.get_option_integer("my_id", 0)
                    };
                }
                promise.set_value(td_api::PushReceiverId { id: push_token_id });
            }
            if info.state == TokenState::Unregister {
                info.token.clear();
            }
            info.state = TokenState::Sync;
        } else {
            match &r_flag {
                Err(error) => {
                    // `Global::get_retry_after` rides the net step; until
                    // then no retry delay is known
                    retry_after = 0;
                    if let Some(mut promise) = info.promise.take() {
                        promise.set_error(error.clone());
                    }
                    let _ = error;
                }
                Ok(false) => {
                    if let Some(mut promise) = info.promise.take() {
                        promise.set_error(Status::error(
                            400,
                            "Receive false as result of registerDevice server request",
                        ));
                    }
                }
                _ => {}
            }
            if info.state == TokenState::Reregister {
                // keep trying to reregister the token; the C++ returns
                // into set_timeout_in WITHOUT the trailing save_info
                self.retry_deadline = Some(env.now() + retry_after.clamp(1, 3600) as f64);
                return;
            } else if info.state == TokenState::Register {
                info.state = TokenState::Unregister;
            } else {
                info.state = TokenState::Sync;
                info.token.clear();
            }
        }
        self.save_info(env, token_type);
    }

    /// The retry timeout pump — fires `loop` at the armed retry deadline.
    pub fn tick_timeouts(&mut self, env: &mut impl DeviceTokenManagerEnv) {
        if let Some(deadline) = self.retry_deadline
            && deadline <= env.now()
        {
            self.retry_deadline = None;
            self.loop_(env);
        }
    }

    /// The retry deadline merge point for the Td timeout slot.
    pub fn next_deadline(&self) -> Option<f64> {
        self.retry_deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notification::NotificationTypeEnv;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    struct TestTokenEnv {
        pmc: HashMap<Vec<u8>, Vec<u8>>,
        options: HashMap<String, i64>,
        time: f64,
    }

    impl TestTokenEnv {
        fn new() -> Self {
            let mut options = HashMap::new();
            options.insert("my_id".to_owned(), 42i64);
            TestTokenEnv {
                pmc: HashMap::new(),
                options,
                time: 0.0,
            }
        }
    }

    impl NotificationTypeEnv for TestTokenEnv {}

    impl NotificationManagerEnv for TestTokenEnv {
        fn is_disabled(&self) -> bool {
            false
        }
        fn close_flag(&self) -> bool {
            false
        }
        fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
            self.options.get(name).copied().unwrap_or(default_value)
        }
        fn get_option_boolean(&self, _name: &str, default_value: bool) -> bool {
            default_value
        }
        fn unix_time(&self) -> i32 {
            1_000_000
        }
        fn now(&self) -> f64 {
            self.time
        }
        fn send_update(&mut self, _update: td_api::Update) {}
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

    impl DeviceTokenManagerEnv for TestTokenEnv {
        fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.pmc.get(key).cloned().unwrap_or_default()
        }
        fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.pmc.insert(key.to_vec(), value.to_vec());
        }
    }

    type Captured = Arc<Mutex<Option<Result<i64, Status>>>>;

    fn capturing_promise() -> (Promise<td_api::PushReceiverId>, Captured) {
        let captured: Captured = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&captured);
        let promise = tdutils::promise::PromiseCreator::lambda_result(
            move |result: tdutils::status::Result<td_api::PushReceiverId>| {
                // ignore poisoning: a panicking assertion must not turn
                // the promise destructor into a process abort
                if let Ok(mut slot) = sink.lock() {
                    *slot = Some(match result {
                        tdutils::status::Result::Ok(value) => Ok(value.id),
                        tdutils::status::Result::Err(status) => Err(status),
                    });
                }
            },
        );
        (promise, captured)
    }

    fn apns_token(device_token: &str) -> Option<td_api::DeviceToken> {
        Some(td_api::DeviceToken::deviceTokenApplePush(
            td_api::deviceTokenApplePush {
                device_token: device_token.to_owned(),
                is_app_sandbox: false,
            },
        ))
    }

    fn web_push_token(endpoint: &str, p256dh: &str, auth: &str) -> Option<td_api::DeviceToken> {
        Some(td_api::DeviceToken::deviceTokenWebPush(
            td_api::deviceTokenWebPush {
                endpoint: endpoint.to_owned(),
                p256dh_base64url: p256dh.to_owned(),
                auth_base64url: auth.to_owned(),
            },
        ))
    }

    #[test]
    fn register_device_validation_errors_match_cpp() {
        let mut env = TestTokenEnv::new();
        let mut manager = DeviceTokenManager::new();

        // null token
        let (promise, captured) = capturing_promise();
        manager.register_device(&mut env, None, &[], promise);
        assert_eq!(
            captured
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|r| r.as_ref().err())
                .map(|e| e.code()),
            Some(400)
        );

        // web push endpoint with a comma
        let (promise, captured) = capturing_promise();
        manager.register_device(&mut env, web_push_token("a,b", "aa", "aa"), &[], promise);
        assert_eq!(
            captured
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|r| r.as_ref().err())
                .map(|e| e.message().to_owned()),
            Some("Illegal endpoint value".to_owned())
        );

        // non-base64url keys
        let (promise, captured) = capturing_promise();
        manager.register_device(
            &mut env,
            web_push_token("https://e", "a+b", "ag"),
            &[],
            promise,
        );
        assert_eq!(
            captured
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|r| r.as_ref().err())
                .map(|e| e.message().to_owned()),
            Some("Public key must be base64url-encoded".to_owned())
        );
        let (promise, captured) = capturing_promise();
        manager.register_device(
            &mut env,
            web_push_token("https://e", "ag", "a~b"),
            &[],
            promise,
        );
        assert_eq!(
            captured
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|r| r.as_ref().err())
                .map(|e| e.message().to_owned()),
            Some("Authentication secret must be base64url-encoded".to_owned())
        );

        // invalid other user ids
        let (promise, captured) = capturing_promise();
        manager.register_device(&mut env, apns_token("tok"), &[-1], promise);
        assert_eq!(
            captured
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|r| r.as_ref().err())
                .map(|e| e.message().to_owned()),
            Some("Invalid user_id among other user_ids".to_owned())
        );
    }

    #[test]
    fn register_device_stores_and_marks_in_flight() {
        let mut env = TestTokenEnv::new();
        let mut manager = DeviceTokenManager::new();

        let (promise, captured) = capturing_promise();
        manager.register_device(&mut env, apns_token("tok1"), &[7, 8], promise);
        // no answer until the server acks
        assert!(captured.lock().unwrap().is_none());
        let info = &manager.tokens[1];
        assert_eq!(info.state, TokenState::Register);
        assert_eq!(info.token, "tok1");
        assert_eq!(info.other_user_ids, vec![7, 8]);
        assert_eq!(
            info.net_query_id,
            u64::MAX,
            "the seam marks the query in flight"
        );
        // the pmc blob is stored with the '*' prefix and parses back
        let stored = env.pmc.get(&b"device_token1"[..]).expect("persisted");
        assert_eq!(stored[0], b'*');
        let parsed = TokenInfo::parse(&mut Parser::new(&stored[1..])).expect("parse");
        assert_eq!(parsed.state, TokenState::Register);
        assert_eq!(parsed.token, "tok1");
        assert_eq!(parsed.other_user_ids, vec![7, 8]);
    }

    #[test]
    fn register_device_empty_token_and_fast_path() {
        let mut env = TestTokenEnv::new();
        let mut manager = DeviceTokenManager::new();

        // empty token with nothing registered answers immediately
        let (promise, captured) = capturing_promise();
        manager.register_device(&mut env, apns_token(""), &[], promise);
        assert_eq!(
            captured
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|r| r.as_ref().ok()),
            Some(&0)
        );

        // register + ack -> Sync; a repeat with the same fields is the
        // fast path answering my_id
        let (promise, captured) = capturing_promise();
        manager.register_device(&mut env, apns_token("tok"), &[], promise);
        manager.on_register_device_result(&mut env, 1, u64::MAX, Ok(true));
        assert_eq!(
            captured
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|r| r.as_ref().ok()),
            Some(&42)
        );
        assert_eq!(manager.tokens[1].state, TokenState::Sync);

        let (promise, captured) = capturing_promise();
        manager.register_device(&mut env, apns_token("tok"), &[], promise);
        assert_eq!(
            captured
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|r| r.as_ref().ok()),
            Some(&42)
        );

        // a changed token re-registers; a PENDING predecessor promise is
        // answered empty (the fast path above answered directly, so this
        // one arrives with nothing stored)
        let (promise_a, captured_a) = capturing_promise();
        manager.register_device(&mut env, apns_token("tokA"), &[], promise_a);
        let (promise_b, captured_b) = capturing_promise();
        manager.register_device(&mut env, apns_token("tokB"), &[], promise_b);
        assert_eq!(
            captured_a
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|r| r.as_ref().ok()),
            Some(&0),
            "the superseded pending register answers an empty pushReceiverId"
        );
        assert!(
            captured_b.lock().unwrap().is_none(),
            "the new register waits for the ack"
        );
        assert_eq!(manager.tokens[1].token, "tokB");
    }

    #[test]
    fn on_result_transitions_match_cpp() {
        let mut env = TestTokenEnv::new();
        let mut manager = DeviceTokenManager::new();

        // Register + error -> Unregister
        let (promise, _captured) = capturing_promise();
        manager.register_device(&mut env, apns_token("tok"), &[], promise);
        manager.on_register_device_result(
            &mut env,
            1,
            u64::MAX,
            Err(Status::error(400, "FLOOD_WAIT_1")),
        );
        assert_eq!(manager.tokens[1].state, TokenState::Unregister);
        assert_eq!(manager.tokens[1].token, "tok");

        // Unregister + error -> Sync with a cleared token
        manager.tokens[1].net_query_id = u64::MAX;
        manager.on_register_device_result(&mut env, 1, u64::MAX, Err(Status::error(400, "oops")));
        assert_eq!(manager.tokens[1].state, TokenState::Sync);
        assert_eq!(manager.tokens[1].token, "");
        assert!(
            !env.pmc.contains_key(&b"device_token1"[..]),
            "the cleared token erases the key"
        );

        // Reregister + error arms the retry (clamp(1, 3600) with no
        // retry_after known -> 1 second) and keeps the token
        manager.tokens[2].state = TokenState::Reregister;
        manager.tokens[2].token = "rt".to_owned();
        manager.loop_(&mut env);
        assert_eq!(manager.tokens[2].net_query_id, u64::MAX);
        manager.on_register_device_result(&mut env, 2, u64::MAX, Err(Status::error(500, "down")));
        assert_eq!(manager.tokens[2].state, TokenState::Reregister);
        assert_eq!(manager.next_deadline(), Some(1.0));

        // a mismatched query id is ignored
        manager.on_register_device_result(&mut env, 2, 12345, Ok(true));
        assert_eq!(manager.tokens[2].state, TokenState::Reregister);
    }

    #[test]
    fn encryption_key_generation_and_answers() {
        let mut env = TestTokenEnv::new();
        let mut manager = DeviceTokenManager::new();

        let (promise, captured) = capturing_promise();
        manager.register_device(
            &mut env,
            Some(td_api::DeviceToken::deviceTokenFirebaseCloudMessaging(
                td_api::deviceTokenFirebaseCloudMessaging {
                    token: "fcm".to_owned(),
                    encrypt: true,
                },
            )),
            &[],
            promise,
        );
        let info = &manager.tokens[2];
        assert!(info.encrypt);
        assert_eq!(info.encryption_key.len(), ENCRYPTION_KEY_LENGTH);
        assert!(
            info.encryption_key_id <= -MIN_ENCRYPTION_KEY_ID
                || info.encryption_key_id >= MIN_ENCRYPTION_KEY_ID,
            "the key id must be outside the colliding range"
        );
        // the ack answers with the encryption key id, and the key round-trips
        let key = info.encryption_key.clone();
        let key_id = info.encryption_key_id;
        manager.on_register_device_result(&mut env, 2, u64::MAX, Ok(true));
        assert_eq!(
            captured
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|r| r.as_ref().ok()),
            Some(&key_id)
        );

        let keys = manager.get_encryption_keys(&env);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].0, key_id);
        assert_eq!(keys[0].1, key);

        // the non-encrypting token answers with my_id and an empty key
        manager.reregister_device(&mut env); // no-op for Sync states
        assert!(manager.next_deadline().is_none());
    }

    #[test]
    fn legacy_load_forms() {
        let mut env = TestTokenEnv::new();
        env.pmc
            .insert(b"device_token3".to_vec(), b"+legacy".to_vec());
        env.pmc
            .insert(b"device_token4".to_vec(), b"=synced".to_vec());
        env.pmc.insert(b"device_token5".to_vec(), b"xjunk".to_vec());
        let mut manager = DeviceTokenManager::new();
        manager.load_states(&mut env);

        assert_eq!(manager.tokens[3].state, TokenState::Register);
        assert_eq!(manager.tokens[3].token, "legacy");
        // a loaded Sync token with a value becomes Reregister
        assert_eq!(manager.tokens[4].state, TokenState::Reregister);
        assert_eq!(manager.tokens[4].token, "synced");
        // the invalid prefix is skipped
        assert_eq!(manager.tokens[5].token, "");
        assert_eq!(manager.tokens[5].state, TokenState::Sync);
    }

    #[test]
    fn token_info_round_trip_with_encryption() {
        let info = TokenInfo {
            state: TokenState::Register,
            token: "tok".to_owned(),
            net_query_id: 0,
            other_user_ids: vec![1, 2, 3],
            is_app_sandbox: true,
            encrypt: true,
            encryption_key: vec![7u8; 256],
            encryption_key_id: -20_000_000_000_000,
            promise: None,
        };
        let mut out = Vec::new();
        info.store(&mut out);
        let parsed = TokenInfo::parse(&mut Parser::new(&out)).expect("parse");
        assert_eq!(parsed.state, TokenState::Register);
        assert_eq!(parsed.token, "tok");
        assert_eq!(parsed.other_user_ids, vec![1, 2, 3]);
        assert!(parsed.is_app_sandbox);
        assert!(parsed.encrypt);
        assert_eq!(parsed.encryption_key, vec![7u8; 256]);
        assert_eq!(parsed.encryption_key_id, -20_000_000_000_000);
    }

    #[test]
    fn is_base64url_matches_cpp_table_checks() {
        assert!(is_base64url(b""));
        assert!(is_base64url(b"abc")); // len%4==3, 'c' low 2 bits zero
        assert!(!is_base64url(b"ab")); // len%4==2, 'b' low 4 bits non-zero
        assert!(!is_base64url(b"a-b_c")); // len 5 -> len%4 == 1
        assert!(is_base64url(b"a-b_"));
        assert!(!is_base64url(b"a+b")); // '+' is not url-safe
        assert!(!is_base64url(b"a/b"));
        assert!(is_base64url(b"abc=")); // (3+1)&3 == 0, tail bits fine
        assert!(!is_base64url(b"abc==")); // (3+2)&3 != 0 with padding
        assert!(!is_base64url(b"a===")); // padding >= 3
        assert!(!is_base64url(b"a=")); // (1+1)&3 != 0 with padding
        assert!(!is_base64url(b"ab=")); // (2+1)&3 != 0 with padding
        assert!(!is_base64url(b"a")); // len % 4 == 1
        assert!(!is_base64url(b"aC")); // len%4==2, 'C' = 2 -> low 4 bits != 0
        assert!(is_base64url(b"aA")); // 'A' = 0 -> low 4 bits zero
        assert!(!is_base64url(b"aAC")); // len%4==3, 'C' low 2 bits != 0
        assert!(is_base64url(b"aAA")); // 'A' low bits zero
    }
}
