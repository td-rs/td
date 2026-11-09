//! The Track G gate (S66): full authorization from Rust against a Telegram
//! test DC with updates streamed — handshake (S60) → session (S62) →
//! `auth.sendCode`/`auth.signIn`/`auth.signUp` over the generated
//! telegram_api types, with the auth state persisted through the S61
//! byte-compatible layout and reloaded for the sign-in leg (an encrypted
//! session WITHOUT a second handshake — the persistence proof of the gate).
//!
//! Usage:
//!   ```text
//!   mtproto_auth --handshake                                              (live key creation only)
//!   mtproto_auth <phone> <api_id> <api_hash> [port]                       (interactive one-shot)
//!   mtproto_auth --send-code <phone> <api_id> <api_hash> <state-file>
//!   mtproto_auth --sign-in <state-file> <code>
//!   ```
//!
//! The two-phase form exists so the confirmation code can arrive between
//! processes: `--send-code` persists the auth state plus the phone-code
//! hash and exits; `--sign-in` reloads it and completes the authorization.
//! `PHONE_NUMBER_UNOCCUPIED` is answered with `auth.signUp` (names from
//! `MTPROTO_AUTH_FIRST_NAME`/`MTPROTO_AUTH_LAST_NAME`, defaulting to
//! "td-rs"/"test"). Exit codes: 3 = wrong/expired code (rerun `--sign-in`
//! with a fresh code), 4 = cloud password required (SRP is deferred to the
//! Track I query layer), 1 = any other failure.

use std::io::Write as _;
use std::time::{Duration, Instant};

use tdmtproto::auth_data::{AuthData, ServerSalt};
use tdmtproto::dh_callback::DhCallback;
use tdmtproto::dh_handshake::DhHandshake;
use tdmtproto::handshake::{AuthKeyHandshake, AuthKeyHandshakeContext};
use tdmtproto::handshake_actor::run_handshake;
use tdmtproto::message_id::MessageId;
use tdmtproto::persistence::{load_auth_data, save_auth_data};
use tdmtproto::proxy_secret::ProxySecret;
use tdmtproto::raw_connection::RawConnection;
use tdmtproto::rsa::{PublicRsaKeyInterface, Rsa, RsaKey};
use tdmtproto::session_connection::{SessionConnection, SessionConnectionCallback, SessionMode};
use tdmtproto::telegram_api;
use tdmtproto::transport_type::{TransportType, TransportTypeEnum};
use tdutils::bignum::{BigNum, BigNumContext};
use tdutils::buffer::BufferSlice;
use tdutils::random;
use tdutils::status::{Result, Status};
use tokio::net::TcpStream;

/// The tdlib test-DC key (`PublicRsaKeySharedMain.cpp`).
const TEST_DC_KEY: &str = "-----BEGIN RSA PUBLIC KEY-----\n\
MIIBCgKCAQEAyMEdY1aR+sCR3ZSJrtztKTKqigvO/vBfqACJLZtS7QMgCGXJ6XIR\n\
yy7mx66W0/sOFa7/1mAZtEoIokDP3ShoqF4fVNb6XeqgQfaUHd8wJpDWHcR2OFwv\n\
plUUI1PLTktZ9uW2WE23b+ixNwJjJGwBDJPQEQFBE+vfmH0JP503wr5INS1poWg/\n\
j25sIWeYPHYeOrFp/eXaqhISP6G+q2IeTaWTXpwZj4LzXq5YOpk4bYEQ6mvRq7D1\n\
aHWfYmlEGepfaYR8Q0YqvvhYtMte3ITnuSJs171+GDqpdKcSwHnd6FudwGO4pcCO\n\
j4WcDuXc2CTHgH8gFTNhp/Y8/SpDOhvn9QIDAQAB\n\
-----END RSA PUBLIC KEY-----";

/// The Telegram test DCs (ip; the `p_q_inner_data_dc` field and the
/// obfuscation dc are 10000 + n, main keys).
const TEST_DCS: [&str; 3] = ["149.154.175.10", "149.154.167.40", "149.154.175.117"];

/// (ip, handshake dc, transport dc) for a test DC number 1..=3.
fn test_dc(dc: u8) -> (&'static str, i32, i16) {
    let ip = TEST_DCS[(dc as usize - 1).clamp(0, 2)];
    (ip, 10000 + dc as i32, (10000 + dc as i32) as i16)
}

/// How long the sign-in leg streams updates after the authorization.
const STREAM_WINDOW: Duration = Duration::from_secs(30);
/// Wall-clock budget for the whole sign-in leg.
const LOGIN_BUDGET: Duration = Duration::from_secs(240);

struct RsaInterface {
    rsa: Rsa,
}

impl PublicRsaKeyInterface for RsaInterface {
    fn get_rsa_key(&self, fingerprints: &[i64]) -> Result<RsaKey> {
        let fingerprint = self.rsa.get_fingerprint();
        if fingerprints.contains(&fingerprint) {
            Result::Ok(RsaKey {
                rsa: self.rsa.clone(),
                fingerprint,
            })
        } else {
            Result::Err(Status::error_message("No matching RSA key"))
        }
    }

    fn drop_keys(&mut self) {}
}

struct Context {
    rsa: RsaInterface,
}

impl AuthKeyHandshakeContext for Context {
    fn get_dh_callback(&self) -> &dyn DhCallback {
        static CALLBACK: PrimeCallback = PrimeCallback;
        &CALLBACK
    }

    fn get_public_rsa_key_interface(&mut self) -> &mut dyn PublicRsaKeyInterface {
        &mut self.rsa
    }
}

/// Accepts the built-in Telegram prime, checks others on the fly.
struct PrimeCallback;

impl DhCallback for PrimeCallback {
    fn is_good_prime(&self, _prime_str: &[u8]) -> i32 {
        -1 // run the full primality checks
    }
    fn add_good_prime(&self, _prime_str: &[u8]) {}
    fn add_bad_prime(&self, _prime_str: &[u8]) {}
}

/// Which flow the session callback drives.
enum Mode {
    /// Legacy single-process flow: the code is read from stdin and updates
    /// stream until the process is killed.
    Interactive,
    /// Phase A: stop once `auth.sentCode` arrives.
    SendCode,
    /// Phase B: sign in (and sign up when the number is unoccupied) with
    /// the code given on the command line.
    SignIn,
}

/// The session callback: performs authorization and streams updates.
struct AuthCallback {
    mode: Mode,
    phone: String,
    api_id: i32,
    phone_code_hash: String,
    /// The cloud-password for the SRP flow (`--password`), if any.
    password: Option<String>,
    logged_in: bool,
    closed: bool,
    got_sent_code: bool,
    updates: Vec<String>,
    /// (rpc code, message, process exit code).
    fatal: Option<(i32, String, i32)>,
    /// Set when the authorization lands; the driver stops at the deadline.
    stream_deadline: Option<Instant>,
    /// The C++ callback sends straight into the connection it is flushing;
    /// the Rust callback queues instead and the driver pumps it after the
    /// flush returns.
    pending_query: Option<Vec<u8>>,
}

impl AuthCallback {
    fn queue_sign_in(&mut self, code: &str) {
        // flags bit 0: `phone_code` is a `flags.0?string` — the generated
        // store gates it on the flag, so bit 0 MUST be set for the code to
        // reach the wire.
        let sign_in = telegram_api::Function::auth_signIn(telegram_api::auth_signIn {
            flags: 1,
            phone_number: self.phone.clone(),
            phone_code_hash: self.phone_code_hash.clone(),
            phone_code: code.to_string(),
            email_verification: Some(telegram_api::EmailVerification::emailVerificationCode(
                telegram_api::emailVerificationCode::default(),
            )),
        });
        self.pending_query = Some(wrap_query(self.api_id, sign_in));
    }

    fn queue_sign_up(&mut self) {
        let first_name =
            std::env::var("MTPROTO_AUTH_FIRST_NAME").unwrap_or_else(|_| "td-rs".to_string());
        let last_name =
            std::env::var("MTPROTO_AUTH_LAST_NAME").unwrap_or_else(|_| "test".to_string());
        let sign_up = telegram_api::Function::auth_signUp(telegram_api::auth_signUp {
            flags: 0,
            no_joined_notifications: false,
            phone_number: self.phone.clone(),
            phone_code_hash: self.phone_code_hash.clone(),
            first_name,
            last_name,
        });
        self.pending_query = Some(wrap_query(self.api_id, sign_up));
    }
}

/// Every API query rides in `invokeWithLayer{initConnection{...}}` — without
/// it the DC answers RPC_ERROR CONNECTION_NOT_INITED. Layer 230 = td 1.8.68
/// (`td/telegram/Version.h` MTPROTO_LAYER).
fn wrap_query(api_id: i32, query: telegram_api::Function) -> Vec<u8> {
    let mut out = Vec::new();
    telegram_api::invokeWithLayer {
        layer: 233,
        query: Some(Box::new(telegram_api::Function::initConnection(
            telegram_api::initConnection {
                flags: 0,
                api_id,
                device_model: "td-rs".to_string(),
                system_version: "linux".to_string(),
                app_version: "0.1.0".to_string(),
                system_lang_code: "en".to_string(),
                lang_pack: String::new(),
                lang_code: "en".to_string(),
                proxy: Some(telegram_api::InputClientProxy {
                    address: String::new(),
                    port: 0,
                }),
                params: Some(Box::new(telegram_api::JSONValue::jsonNull(
                    telegram_api::jsonNull {},
                ))),
                query: Some(Box::new(query)),
            },
        ))),
    }
    .store_boxed(&mut out);
    out
}
/// One-line description of a pushed update (the `Updates` umbrella).
fn describe_update(data: &[u8]) -> String {
    use telegram_api::Updates as U;
    if let Ok(update) = U::parse(data) {
        return match update {
            U::updatesTooLong(_) => "updatesTooLong".to_string(),
            U::updateShort(m) => {
                let mut inner = Vec::new();
                m.update.unwrap_or_default().store(&mut inner);
                let ctor = i32::from_le_bytes(inner[..4].try_into().unwrap());
                format!("updateShort date={} inner=0x{ctor:08x}", m.date)
            }
            U::updateShortMessage(m) => {
                let message: String = m.message.chars().take(40).collect();
                format!(
                    "updateShortMessage id={} user_id={} date={} message={message:?}",
                    m.id, m.user_id, m.date
                )
            }
            U::updateShortChatMessage(m) => format!(
                "updateShortChatMessage id={} chat_id={} from_id={} date={}",
                m.id, m.chat_id, m.from_id, m.date
            ),
            U::updatesCombined(m) => format!(
                "updatesCombined date={} seq_start={} seq={} updates={} chats={} users={}",
                m.date,
                m.seq_start,
                m.seq,
                m.updates.len(),
                m.chats.len(),
                m.users.len()
            ),
            U::updates(m) => format!(
                "updates date={} seq={} updates={} chats={} users={}",
                m.date,
                m.seq,
                m.updates.len(),
                m.chats.len(),
                m.users.len()
            ),
            U::updateShortSentMessage(m) => {
                format!("updateShortSentMessage id={} date={}", m.id, m.date)
            }
        };
    }
    let ctor = data
        .get(..4)
        .map(|b| i32::from_le_bytes(b.try_into().unwrap()))
        .unwrap_or(0);
    format!("ctor 0x{ctor:08x}, {} bytes", data.len())
}

impl SessionConnectionCallback for AuthCallback {
    fn on_connected(&mut self) {}

    fn on_closed(&mut self, status: Status) {
        eprintln!("session closed: {}", status.message());
        self.closed = true;
    }

    fn on_server_salt_updated(&mut self) {}

    fn on_server_time_difference_updated(&mut self, _force: bool) {}

    fn on_new_session_created(&mut self, _unique_id: u64, _first_message_id: MessageId) {}

    fn on_session_failed(&mut self, status: Status) {
        eprintln!("session failed: {}", status.message());
    }

    fn on_container_sent(
        &mut self,
        _container_message_id: MessageId,
        _message_ids: Vec<MessageId>,
    ) {
    }

    fn on_pong(&mut self, _ping_time: f64, _pong_time: f64, _current_time: f64) -> Status {
        Status::ok()
    }

    fn on_update(&mut self, packet: BufferSlice) -> Status {
        let description = describe_update(packet.as_slice());
        println!("update: {description}");
        self.updates.push(description);
        Status::ok()
    }

    fn on_message_ack(&mut self, _message_id: MessageId) {}

    fn on_message_result_ok(
        &mut self,
        _message_id: MessageId,
        packet: BufferSlice,
        _original_size: usize,
    ) -> Status {
        let data = packet.as_slice();
        let Some(ctor) = data.get(..4) else {
            return Status::ok();
        };
        let ctor = i32::from_le_bytes(ctor.try_into().unwrap());
        if ctor == telegram_api::auth_sentCode::ID {
            let msg = match telegram_api::auth_sentCode::parse(data) {
                Ok(msg) => msg,
                Err(error) => {
                    eprintln!("failed to parse auth.sentCode: {error}");
                    return Status::ok();
                }
            };
            self.phone_code_hash = msg.phone_code_hash;
            let default_ty = telegram_api::auth_SentCodeType::auth_sentCodeTypeSms(
                telegram_api::auth_sentCodeTypeSms { length: 5 },
            );
            let type_name = match msg.r#type.as_ref().unwrap_or(&default_ty) {
                telegram_api::auth_SentCodeType::auth_sentCodeTypeApp(t) => {
                    format!("app length={}", t.length)
                }
                telegram_api::auth_SentCodeType::auth_sentCodeTypeSms(t) => {
                    format!("sms length={}", t.length)
                }
                telegram_api::auth_SentCodeType::auth_sentCodeTypeCall(t) => {
                    format!("call length={}", t.length)
                }
                telegram_api::auth_SentCodeType::auth_sentCodeTypeFlashCall(t) => {
                    format!("flashcall pattern={}", t.pattern)
                }
                telegram_api::auth_SentCodeType::auth_sentCodeTypeMissedCall(t) => {
                    format!("missed_call length={} prefix={}", t.length, t.prefix)
                }
                telegram_api::auth_SentCodeType::auth_sentCodeTypeEmailCode(t) => {
                    format!("email length={}", t.length)
                }
                other => format!("other: {other:?}"),
            };
            eprintln!("sentCode type: {type_name}, hash={}", self.phone_code_hash);
            self.got_sent_code = true;
            if matches!(self.mode, Mode::Interactive) {
                print!("Enter the code from Telegram/SMS: ");
                std::io::stdout().flush().unwrap();
                let mut code = String::new();
                std::io::stdin().read_line(&mut code).unwrap();
                let code = code.trim().to_string();
                self.queue_sign_in(&code);
            }
            return Status::ok();
        }
        if ctor == telegram_api::auth_authorization::ID {
            let msg = match telegram_api::auth_authorization::parse(data) {
                Ok(msg) => msg,
                Err(error) => {
                    eprintln!("failed to parse auth.authorization: {error}");
                    return Status::ok();
                }
            };
            match msg.user.unwrap_or(telegram_api::User::userEmpty(
                telegram_api::userEmpty::default(),
            )) {
                telegram_api::User::user(u) => {
                    println!(
                        "AUTHORIZED user_id={} username={:?} name={:?} {:?} phone={:?}",
                        u.id, u.username, u.first_name, u.last_name, u.phone
                    );
                }
                other => println!("AUTHORIZED: {other:?}"),
            }
            self.logged_in = true;
            if matches!(self.mode, Mode::SignIn) {
                self.stream_deadline = Some(Instant::now() + STREAM_WINDOW);
                // Post-auth query round-trip + the update position to beat.
                let get_state =
                    telegram_api::Function::updates_getState(telegram_api::updates_getState);
                self.pending_query = Some(wrap_query(self.api_id, get_state));
            }
            return Status::ok();
        }
        if ctor == telegram_api::account_Password::ID {
            let msg = match telegram_api::account_Password::parse(data) {
                Ok(msg) => msg,
                Err(error) => {
                    eprintln!("failed to parse account.password: {error}");
                    return Status::ok();
                }
            };
            if !msg.hint.is_empty() {
                eprintln!("password hint: {}", msg.hint);
            }
            let check = match (&msg.current_algo, self.password.as_deref()) {
                (
                    Some(telegram_api::PasswordKdfAlgo::passwordKdfAlgoSHA256SHA256PBKDF2HMACSHA512iter100000SHA256ModPow(algo)),
                    Some(password),
                ) => calc_input_check_password_srp(
                    password.as_bytes(),
                    algo,
                    &msg.srp_B,
                    msg.srp_id,
                ),
                _ => None,
            };
            let check = match check {
                Some(check) => check,
                None => {
                    eprintln!("no usable SRP parameters (algo) or no --password given");
                    self.fatal = Some((401, "SESSION_PASSWORD_NEEDED".to_string(), 4));
                    return Status::ok();
                }
            };
            let query =
                telegram_api::Function::auth_checkPassword(telegram_api::auth_checkPassword {
                    password: Some(telegram_api::InputCheckPasswordSRP::inputCheckPasswordSRP(
                        check,
                    )),
                });
            self.pending_query = Some(wrap_query(self.api_id, query));
            return Status::ok();
        }
        if ctor == telegram_api::updates_State::ID {
            let msg = match telegram_api::updates_State::parse(data) {
                Ok(msg) => msg,
                Err(error) => {
                    eprintln!("failed to parse updates.state: {error}");
                    return Status::ok();
                }
            };
            println!(
                "updates.getState: pts={} qts={} seq={} date={} unread={}",
                msg.pts, msg.qts, msg.seq, msg.date, msg.unread_count
            );
            return Status::ok();
        }
        println!("result: ctor 0x{ctor:08x}, {} bytes", data.len());
        Status::ok()
    }

    fn on_message_result_error(&mut self, _message_id: MessageId, code: i32, message: String) {
        eprintln!("RPC error {code}: {message}");
        if message == "PHONE_NUMBER_UNOCCUPIED" && matches!(self.mode, Mode::SignIn) {
            eprintln!("number unoccupied on the test DC — registering (auth.signUp)");
            self.queue_sign_up();
            return;
        }
        if message.starts_with("PHONE_MIGRATE_") || message.starts_with("NETWORK_MIGRATE_") {
            let dc = message.rsplit('_').next().unwrap_or("?");
            eprintln!("the phone's home DC is {dc} — rerun with --dc {dc}");
            self.fatal = Some((code, message, 5));
            return;
        }
        if message == "SESSION_PASSWORD_NEEDED"
            && let Some(password) = self.password.clone()
        {
            eprintln!("cloud password required — running the SRP flow (account.getPassword)");
            self.password = Some(password);
            let query =
                telegram_api::Function::account_getPassword(telegram_api::account_getPassword);
            self.pending_query = Some(wrap_query(self.api_id, query));
            return;
        }
        self.fatal = Some(match message.as_str() {
            "PHONE_CODE_INVALID" | "PHONE_CODE_EXPIRED" => (code, message, 3),
            "SESSION_PASSWORD_NEEDED" | "PASSWORD_HASH_INVALID" => (code, message, 4),
            _ => (code, message, 1),
        });
    }

    fn on_message_failed(&mut self, _message_id: MessageId, status: Status) {
        eprintln!("message failed: {}", status.message());
    }

    fn on_message_info(
        &mut self,
        _message_id: MessageId,
        _state: i32,
        _answer_message_id: MessageId,
        _answer_size: i32,
        _source: i32,
    ) {
    }

    fn on_destroy_auth_key(&mut self) -> Status {
        Status::ok()
    }
}

const STATE_MAGIC: &[u8; 4] = b"MTA2";
struct SavedState {
    dc: u8,
    /// Wall-clock seconds minus `tdutils::time::now()` at save time:
    /// `time::now()` is process-anchored monotonic, so the persisted
    /// server-time difference must be re-based to the loading process.
    wall_anchor: f64,
    phone: String,
    api_id: i32,
    api_hash: String,
    phone_code_hash: String,
    blob: Vec<u8>,
}

fn save_state(path: &str, state: &SavedState) {
    fn put_str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut out = Vec::new();
    out.extend_from_slice(STATE_MAGIC);
    out.push(state.dc);
    out.extend_from_slice(&state.wall_anchor.to_le_bytes());
    put_str(&mut out, &state.phone);
    out.extend_from_slice(&state.api_id.to_le_bytes());
    put_str(&mut out, &state.api_hash);
    put_str(&mut out, &state.phone_code_hash);
    out.extend_from_slice(&(state.blob.len() as u32).to_le_bytes());
    out.extend_from_slice(&state.blob);
    if let Err(error) = std::fs::write(path, out) {
        eprintln!("failed to write {path}: {error}");
        std::process::exit(1);
    }
}

fn load_state(path: &str) -> SavedState {
    fn take_str(data: &mut &[u8]) -> String {
        let len = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
        let s = String::from_utf8_lossy(&data[4..4 + len]).into_owned();
        *data = &data[4 + len..];
        s
    }
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("failed to read {path}: {error}");
            std::process::exit(1);
        }
    };
    let mut data = data.as_slice();
    if data.get(..4) != Some(STATE_MAGIC) {
        eprintln!("{path}: not an mtproto_auth state file");
        std::process::exit(1);
    }
    data = &data[4..];
    let dc = data[0];
    data = &data[1..];
    let wall_anchor = f64::from_le_bytes(data[..8].try_into().unwrap());
    data = &data[8..];
    let phone = take_str(&mut data);
    let api_id = i32::from_le_bytes(data[..4].try_into().unwrap());
    data = &data[4..];
    let api_hash = take_str(&mut data);
    let phone_code_hash = take_str(&mut data);
    let blob_len = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
    let blob = data[4..4 + blob_len].to_vec();
    SavedState {
        dc,
        wall_anchor,
        phone,
        api_id,
        api_hash,
        phone_code_hash,
        blob,
    }
}

/// Wall-clock epoch seconds minus `time::now()` (the process-anchored
/// monotonic clock) — the re-basing anchor for persisted time diffs.
fn wall_minus_monotonic() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        - tdutils::time::now()
}

/// The current auth state of a live connection, in the S61 layout.
fn connection_blob(connection: &mut SessionConnection) -> Vec<u8> {
    let now = tdutils::time::now();
    let auth_data = connection.auth_data();
    let salt = ServerSalt {
        salt: auth_data.get_server_salt(now),
        valid_since: now,
        valid_until: now + 1.0e9,
    };
    save_auth_data(
        auth_data.get_main_auth_key(),
        None,
        &salt,
        auth_data.get_server_time_difference(),
        auth_data.get_session_id() as i64,
    )
}

enum Cli {
    Handshake,
    Interactive {
        phone: String,
        api_id: i32,
        api_hash: String,
        port: u16,
    },
    SendCode {
        phone: String,
        api_id: i32,
        api_hash: String,
        path: String,
        dc: u8,
    },
    SignIn {
        path: String,
        code: String,
        password: Option<String>,
    },
}

fn usage() -> ! {
    eprintln!(
        "usage: mtproto_auth --handshake\n\
         \x20      mtproto_auth <phone> <api_id> <api_hash> [port]\n\
         \x20      mtproto_auth --send-code [--dc 1..3] <phone> <api_id> <api_hash> <state-file>\n\
         \x20      mtproto_auth --sign-in <state-file> <code>"
    );
    std::process::exit(2);
}
fn parse_args() -> Cli {
    let mut args: Vec<String> = std::env::args().collect();
    // Optional `--password`: completes the 2FA SRP flow after signIn.
    let mut password: Option<String> = None;
    if let Some(pos) = args.iter().position(|a| a == "--password") {
        let value = args.get(pos + 1).cloned().unwrap_or_default();
        args.drain(pos..=pos + 1);
        password = Some(value);
    }
    let mut dc: u8 = 2;
    if let Some(pos) = args.iter().position(|a| a == "--dc") {
        let value = args.get(pos + 1).cloned().unwrap_or_default();
        args.drain(pos..=pos + 1);
        dc = value.parse().unwrap_or_else(|_| usage());
    }
    if !(1..=3).contains(&dc) {
        usage();
    }
    if args.len() == 2 && args[1] == "--handshake" {
        return Cli::Handshake;
    }
    if args.len() == 6 && args[1] == "--send-code" {
        return Cli::SendCode {
            phone: args[2].clone(),
            api_id: args[3].parse().unwrap_or_else(|_| usage()),
            api_hash: args[4].clone(),
            path: args[5].clone(),
            dc,
        };
    }
    if args.len() == 4 && args[1] == "--sign-in" {
        return Cli::SignIn {
            path: args[2].clone(),
            code: args[3].clone(),
            password,
        };
    }
    if (args.len() == 4 || args.len() == 5) && !args[1].starts_with("--") {
        let port = if args.len() == 5 {
            args[4].parse().unwrap_or_else(|_| usage())
        } else {
            443
        };
        return Cli::Interactive {
            phone: args[1].clone(),
            api_id: args[2].parse().unwrap_or_else(|_| usage()),
            api_hash: args[3].clone(),
            port,
        };
    }
    usage()
}

fn main() {
    let cli = parse_args();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        match cli {
            Cli::Handshake => run_handshake_leg().await,
            Cli::Interactive {
                phone,
                api_id,
                api_hash,
                port,
            } => {
                run_authorize_leg(Mode::Interactive, &phone, api_id, &api_hash, 2, port, None)
                    .await;
            }
            Cli::SendCode {
                phone,
                api_id,
                api_hash,
                path,
                dc,
            } => {
                run_authorize_leg(
                    Mode::SendCode,
                    &phone,
                    api_id,
                    &api_hash,
                    dc,
                    443,
                    Some(&path),
                )
                .await;
            }
            Cli::SignIn {
                path,
                code,
                password,
            } => run_sign_in_leg(&path, &code, password).await,
        }
    });
}

/// The `--handshake` leg: connect, handshake, print the key, stop.
async fn run_handshake_leg() {
    let (ip, hs_dc, tx_dc) = test_dc(2);
    let _ = establish(ip, hs_dc, tx_dc, 443).await;
    println!("mtproto_auth:handshake: OK");
}

/// Connects and handshakes on a test DC. The `p_q_inner_data_dc` `dc` field
/// must be 10000 + n (test DC n, main keys) — the convention of td's own
/// live test (`test/mtproto.cpp get_default_dc_id`); any other value (2,
/// -2) makes the server answer transport error -444. The transport-level dc
/// (obfuscation header) is unrelated to the rejection.
async fn establish(
    ip: &str,
    hs_dc: i32,
    tx_dc: i16,
    port: u16,
) -> (RawConnection, tdmtproto::auth_key::AuthKey, f64, u64) {
    let stream = TcpStream::connect((ip, port)).await.unwrap();
    let transport_type = TransportType::new(TransportTypeEnum::Tcp, tx_dc, ProxySecret::default());
    let mut context = Context {
        rsa: RsaInterface {
            rsa: Rsa::from_pem_public_key(TEST_DC_KEY).unwrap(),
        },
    };
    let mut handshake = AuthKeyHandshake::new(hs_dc, 0);
    let raw_connection =
        match run_handshake(stream, transport_type, &mut context, 30.0, &mut handshake).await {
            Result::Ok(connection) => connection,
            Result::Err(status) => {
                eprintln!("handshake failed: {}", status.message());
                std::process::exit(1);
            }
        };
    let auth_key = handshake.release_auth_key();
    let time_diff = handshake.get_server_time_diff();
    let salt = handshake.get_server_salt();
    println!(
        "auth key created: id={:016x}, time_diff={time_diff:.3}, salt={salt}",
        auth_key.id()
    );
    (raw_connection, auth_key, time_diff, salt)
}

/// The `--handshake`-then-authorize legs (interactive and `--send-code`):
/// handshake → AuthData/session → `auth.sendCode`. `state_path` set
/// (`--send-code`) persists the auth state plus the phone-code hash once
/// the code is out; the interactive form keeps streaming instead.
async fn run_authorize_leg(
    mode: Mode,
    phone: &str,
    api_id: i32,
    api_hash: &str,
    dc: u8,
    port: u16,
    state_path: Option<&str>,
) {
    // 1. Connect + handshake on the test DC.
    let (ip, hs_dc, tx_dc) = test_dc(dc);
    let (raw_connection, auth_key, time_diff, salt) = establish(ip, hs_dc, tx_dc, port).await;

    // 2. AuthData + session.
    let mut auth_data = AuthData::new();
    auth_data.set_main_auth_key(auth_key);
    auth_data.set_use_pfs(false);
    auth_data.reset_server_time_difference(time_diff);
    auth_data.set_server_salt(salt as i64, tdutils::time::now());
    let mut session_id = 0u64;
    while session_id == 0 {
        session_id = random::secure_uint64();
    }
    auth_data.set_session_id(session_id);
    auth_data.set_auth_flag(true);
    let mut connection = SessionConnection::new(SessionMode::Tcp, raw_connection, auth_data);

    // 3. auth.sendCode once the session is up (queued through the
    //    pending-query path of the callback).
    let mut callback = AuthCallback {
        mode,
        phone: phone.to_string(),
        api_id,
        phone_code_hash: String::new(),
        password: None,
        logged_in: false,
        closed: false,
        got_sent_code: false,
        updates: Vec::new(),
        fatal: None,
        stream_deadline: None,
        pending_query: None,
    };
    let send_code = telegram_api::Function::auth_sendCode(telegram_api::auth_sendCode {
        phone_number: phone.to_string(),
        api_id,
        api_hash: api_hash.to_string(),
        settings: Some(telegram_api::CodeSettings {
            flags: 0,
            allow_flashcall: false,
            current_number: false,
            allow_app_hash: false,
            allow_missed_call: false,
            allow_firebase: false,
            unknown_number: false,
            logout_tokens: Vec::new(),
            token: String::new(),
            app_sandbox: false,
        }),
    });
    callback.pending_query = Some(wrap_query(api_id, send_code));

    // Drive: SendCode stops at `auth.sentCode`; interactive streams forever.
    let interactive = matches!(callback.mode, Mode::Interactive);
    let result = drive_auth_session(&mut connection, &mut callback, |cb| {
        cb.got_sent_code && !interactive
    })
    .await;
    if result.is_error() {
        eprintln!("session error: {}", result.message());
        std::process::exit(1);
    }

    if let Some(path) = state_path {
        save_state(
            path,
            &SavedState {
                dc,
                wall_anchor: wall_minus_monotonic(),
                phone: phone.to_string(),
                api_id,
                api_hash: api_hash.to_string(),
                phone_code_hash: callback.phone_code_hash.clone(),
                blob: connection_blob(&mut connection),
            },
        );
        println!("mtproto_auth:send-code:ok state saved to {path}");
        return;
    }

    // Interactive: only reachable when the session closed.
    if !callback.logged_in {
        eprintln!("session closed before authorization");
        std::process::exit(1);
    }
    println!("mtproto_auth: OK");
}

/// The `--sign-in` leg: reload the persisted auth state, open an encrypted
/// session WITHOUT a new handshake (the persistence proof), sign in (or
/// sign up), then `updates.getState` + the streamed-update window.
async fn run_sign_in_leg(path: &str, code: &str, password: Option<String>) {
    let saved = load_state(path);
    let (key, _handshake, salt, time_diff, _session_id) = match load_auth_data(&saved.blob) {
        Result::Ok(loaded) => loaded,
        Result::Err(status) => {
            eprintln!("failed to load the auth state: {}", status.message());
            std::process::exit(1);
        }
    };
    println!(
        "mtproto_auth:reload:dc={}:key_id={:016x}:ok (encrypted session without a new handshake)",
        saved.dc,
        key.id()
    );

    let mut auth_data = AuthData::new();
    // The default is PFS (temp keys); this flow stores main keys.
    auth_data.set_use_pfs(false);
    auth_data.set_main_auth_key(key);
    auth_data.set_server_salt(salt.salt, tdutils::time::now());
    // Re-base the persisted diff onto THIS process's monotonic clock.
    let time_diff = time_diff + wall_minus_monotonic() - saved.wall_anchor;
    auth_data.reset_server_time_difference(time_diff);
    // A fresh session id per incarnation, exactly like the C++
    // Session::start_up (the id is never persisted; the auth KEY is).
    let mut session_id = 0u64;
    while session_id == 0 {
        session_id = random::secure_uint64();
    }
    auth_data.set_session_id(session_id);
    auth_data.set_auth_flag(true);

    let (ip, _hs_dc, tx_dc) = test_dc(saved.dc);
    let stream = TcpStream::connect((ip, 443)).await.unwrap();
    let raw_connection = RawConnection::new(
        stream,
        TransportType::new(TransportTypeEnum::Tcp, tx_dc, ProxySecret::default()),
        None,
    );
    let mut connection = SessionConnection::new(SessionMode::Tcp, raw_connection, auth_data);

    let mut callback = AuthCallback {
        mode: Mode::SignIn,
        phone: saved.phone.clone(),
        api_id: saved.api_id,
        phone_code_hash: saved.phone_code_hash.clone(),
        password,
        logged_in: false,
        closed: false,
        got_sent_code: false,
        updates: Vec::new(),
        fatal: None,
        stream_deadline: None,
        pending_query: None,
    };
    callback.queue_sign_in(code);
    connection.set_online(true, true);

    let drive = drive_auth_session(&mut connection, &mut callback, |cb| {
        cb.stream_deadline.is_some_and(|d| Instant::now() >= d)
    });
    let result = match tokio::time::timeout(LOGIN_BUDGET, drive).await {
        Ok(result) => result,
        Err(_elapsed) => {
            eprintln!("sign-in leg exceeded the {LOGIN_BUDGET:?} budget");
            std::process::exit(1);
        }
    };
    if result.is_error() {
        eprintln!("session error: {}", result.message());
        std::process::exit(1);
    }
    if !callback.logged_in {
        eprintln!("session closed before authorization");
        std::process::exit(1);
    }

    // Re-persist the (salt/time-refreshed) state through the S61 layout.
    save_state(
        path,
        &SavedState {
            dc: saved.dc,
            wall_anchor: wall_minus_monotonic(),
            phone: saved.phone,
            api_id: saved.api_id,
            api_hash: saved.api_hash,
            phone_code_hash: saved.phone_code_hash,
            blob: connection_blob(&mut connection),
        },
    );
    println!("mtproto_auth:persist:ok (auth state re-saved through the S61 layout)");
    println!("mtproto_auth:updates:received={}", callback.updates.len());
    println!("mtproto_auth: OK");
}

/// `PasswordManager::calc_password_hash` (td/telegram/PasswordManager.cpp):
/// sha256(S1 + password + S1) → sha256(S2 + · + S2) →
/// pbkdf2_sha512(·, S1, 100000, 64 bytes) → sha256(S2 + · + S2).
fn calc_password_hash(password: &[u8], client_salt: &[u8], server_salt: &[u8]) -> [u8; 32] {
    fn hash_sha256(salt: &[u8], data: &[u8]) -> [u8; 32] {
        tdutils::crypto::sha256(&[salt, data, salt].concat())
    }
    let buf = hash_sha256(client_salt, password);
    let buf = hash_sha256(server_salt, &buf);
    let mut pbkdf = [0u8; 64];
    tdutils::crypto::pbkdf2_sha512(&buf, client_salt, 100000, &mut pbkdf);
    hash_sha256(server_salt, &pbkdf)
}

/// `PasswordManager::get_input_check_password` (PasswordManager.cpp:73) —
/// the SRP-6a check for `auth.checkPassword`. `None` mirrors the C++
/// fallback to `inputCheckPasswordEmpty` on invalid parameters.
#[allow(clippy::too_many_lines)]
fn calc_input_check_password_srp(
    password: &[u8],
    algo: &telegram_api::passwordKdfAlgoSHA256SHA256PBKDF2HMACSHA512iter100000SHA256ModPow,
    srp_b: &[u8],
    srp_id: i64,
) -> Option<telegram_api::inputCheckPasswordSRP> {
    let (client_salt, server_salt, p_bytes) = (&algo.salt1, &algo.salt2, &algo.p);
    if DhHandshake::check_config(algo.g, p_bytes, &PrimeCallback).is_error() {
        eprintln!("invalid SRP config g={} p", algo.g);
        return None;
    }
    let p = BigNum::from_binary(p_bytes);
    let b = BigNum::from_binary(srp_b);
    let zero = BigNum::new();
    if BigNum::compare(&zero, &b) != -1
        || BigNum::compare(&b, &p) != -1
        || !(248..=256).contains(&srp_b.len())
    {
        eprintln!("invalid value of B ({} bytes)", srp_b.len());
        return None;
    }

    let mut ctx = BigNumContext::new();
    let mut g = BigNum::new();
    g.set_value(algo.g as u32);
    let g_padded = g.to_binary(256);

    let x = calc_password_hash(password, client_salt, server_salt);
    let x_bn = BigNum::from_binary(&x);

    let mut a_bytes = vec![0u8; 256];
    random::secure_bytes(&mut a_bytes);
    let a_bn = BigNum::from_binary(&a_bytes);

    let a_out = BigNum::mod_exp(&g, &a_bn, &p, &mut ctx).to_binary(256);

    let b_pad = vec![0u8; 256 - srp_b.len()];
    let u = BigNum::from_binary(&tdutils::crypto::sha256(&[&a_out, &b_pad, srp_b].concat()));
    let k = BigNum::from_binary(&tdutils::crypto::sha256(
        &[p_bytes.as_slice(), g_padded.as_slice()].concat(),
    ));

    let v = BigNum::mod_exp(&g, &x_bn, &p, &mut ctx);
    let kv = BigNum::mod_mul(&k, &v, &p, &mut ctx);
    let mut t = BigNum::sub(&b, &kv);
    if BigNum::compare(&t, &zero) == -1 {
        t = BigNum::add(&t, &p);
    }
    let ux = BigNum::mul(&u, &x_bn, &mut ctx);
    let exp = BigNum::add(&ux, &a_bn);

    let s = BigNum::mod_exp(&t, &exp, &p, &mut ctx).to_binary(256);
    let k_hash = tdutils::crypto::sha256(&s);
    let h1 = tdutils::crypto::sha256(p_bytes);
    let h2 = tdutils::crypto::sha256(&g_padded);
    let mut h1x = [0u8; 32];
    for i in 0..32 {
        h1x[i] = h1[i] ^ h2[i];
    }

    let m1 = tdutils::crypto::sha256(
        &[
            h1x.as_slice(),
            tdutils::crypto::sha256(client_salt).as_slice(),
            tdutils::crypto::sha256(server_salt).as_slice(),
            a_out.as_slice(),
            b_pad.as_slice(),
            srp_b,
            k_hash.as_slice(),
        ]
        .concat(),
    );
    Some(telegram_api::inputCheckPasswordSRP {
        srp_id,
        A: a_out,
        M1: m1.to_vec(),
    })
}

/// Drives the session and pumps the callback's pending query after each
/// flush (the C++ callback sends straight into the connection; the Rust
/// callback queues it instead). Returns when `stop` says so; fatal RPC
/// errors exit the process directly.
async fn drive_auth_session(
    connection: &mut SessionConnection,
    callback: &mut AuthCallback,
    mut stop: impl FnMut(&AuthCallback) -> bool,
) -> Status {
    loop {
        let _ = connection.flush(callback).await;
        if let Some(query) = callback.pending_query.take() {
            let _message_id = connection.send_query(
                BufferSlice::from_slice_bytes(&query),
                false,
                MessageId::default(),
                Vec::new(),
                false,
            );
            let _ = connection.flush(callback).await;
        }
        if let Some((_code, _message, exit)) = &callback.fatal {
            std::process::exit(*exit);
        }
        if callback.closed || stop(callback) {
            return Status::ok();
        }
        tokio::select! {
            _ = connection.raw_connection().stream().readable() => {}
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
}
