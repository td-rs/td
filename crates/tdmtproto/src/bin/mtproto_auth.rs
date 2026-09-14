//! The Track G gate (S66): full authorization from Rust against a Telegram
//! test DC with updates streamed — handshake (S60) → session (S62) →
//! `auth.sendCode`/`auth.signIn` over the generated telegram_api types.
//!
//! Usage: mtproto_auth <phone> <api_id> <api_hash>
//! The authorization code is read from stdin when the DC asks for it.

use std::io::Write as _;

use tdmtproto::auth_data::AuthData;
use tdmtproto::dh_callback::DhCallback;
use tdmtproto::handshake::{AuthKeyHandshake, AuthKeyHandshakeContext};
use tdmtproto::handshake_actor::run_handshake;
use tdmtproto::message_id::MessageId;
use tdmtproto::proxy_secret::ProxySecret;
use tdmtproto::rsa::{PublicRsaKeyInterface, Rsa, RsaKey};
use tdmtproto::session_connection::{SessionConnection, SessionConnectionCallback, SessionMode};
use tdmtproto::telegram_api;
use tdmtproto::transport_type::{TransportType, TransportTypeEnum};
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

/// The session callback: performs authorization and streams updates.
struct AuthCallback {
    phone: String,
    phone_code_hash: String,
    logged_in: bool,
    /// The C++ callback sends straight into the connection it is flushing;
    /// the Rust callback queues instead and the driver pumps it after the
    /// flush returns.
    pending_query: Option<Vec<u8>>,
    /// Set when the DC answers RPC_ERROR 303 *_MIGRATE_X: the number lives
    /// on test DC X and the whole flow must be redone there.
    migrate_dc: Option<i32>,
}

impl SessionConnectionCallback for AuthCallback {
    fn on_connected(&mut self) {}

    fn on_closed(&mut self, status: Status) {
        eprintln!("session closed: {}", status.message());
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
        let data = packet.as_slice();
        if data.len() >= 4 {
            let ctor = i32::from_le_bytes(data[..4].try_into().unwrap());
            println!("update: ctor 0x{ctor:08x}, {} bytes", data.len());
        }
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
            let type_name = match &msg.r#type {
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
            print!("Enter the code from Telegram/SMS: ");
            std::io::stdout().flush().unwrap();
            let mut code = String::new();
            std::io::stdin().read_line(&mut code).unwrap();
            let code = code.trim().to_string();

            let mut query = Vec::new();
            telegram_api::auth_signIn {
                flags: 0,
                phone_number: self.phone.clone(),
                phone_code_hash: self.phone_code_hash.clone(),
                phone_code: code,
                email_verification: telegram_api::EmailVerification::emailVerificationCode(
                    telegram_api::emailVerificationCode::default(),
                ),
            }
            .store_boxed(&mut query);
            // The callback cannot access the connection (it is being
            // flushed); queue through a re-send after the flush returns.
            self.pending_query = Some(query);
            return Status::ok();
        }
        if ctor == telegram_api::auth_authorization::ID {
            println!("AUTHORIZED: got auth.authorization ({} bytes)", data.len());
            self.logged_in = true;
            return Status::ok();
        }
        println!("result: ctor 0x{ctor:08x}, {} bytes", data.len());
        Status::ok()
    }

    fn on_message_result_error(&mut self, _message_id: MessageId, code: i32, message: String) {
        eprintln!("RPC error {code}: {message}");
        // 303 = *_MIGRATE_X (PHONE_/NETWORK_/FILE_): the tail digit is the
        // DC the number belongs to.
        if code == 303 {
            if let Some(dc) = message
                .rsplit('_')
                .next()
                .and_then(|t| t.parse::<i32>().ok())
            {
                self.migrate_dc = Some(dc);
            }
        }
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let handshake_only = args.len() == 2 && args[1] == "--handshake";
    if !handshake_only && args.len() != 4 && args.len() != 5 && args.len() != 8 {
        eprintln!(
            "usage: mtproto_auth <phone> <api_id> <api_hash> [port]\n       mtproto_auth --handshake (live key creation only)"
        );
        std::process::exit(2);
    }
    let phone = if handshake_only {
        String::new()
    } else {
        args[1].clone()
    };
    let api_id: i32 = if handshake_only {
        0
    } else {
        args[2].parse().unwrap()
    };
    let api_hash = if handshake_only {
        String::new()
    } else {
        args[3].clone()
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        // 1. Connect + handshake on the test DC (149.154.167.40 = test DC 2).
        // The p_q_inner_data_dc `dc` field must be 10002 (10000 + 2: test
        // DC2, main keys) — the value td's own live test uses
        // (test/mtproto.cpp get_default_dc_id). Any other value (2, -2)
        // makes the server answer transport error -444; the transport-level
        // dc (obfuscation header) is unrelated to the rejection.
        // DC selection: <ip> <handshake-dc> <transport-dc> <port> as the
        // last four args, defaulting to test DC 2 (10002). Test DC list:
        // 1 = 149.154.175.10, 2 = 149.154.167.40, 3 = 149.154.175.117.
        // A PHONE_MIGRATE_X answer auto-reconnects to test DC X (fresh
        // handshake — auth keys do not cross DCs), like tdlib's dispatcher.
        let (mut dc_ip, mut hs_dc, mut tx_dc, port) = if args.len() >= 8 {
            (
                args[args.len() - 4].clone(),
                args[args.len() - 3].parse().unwrap(),
                args[args.len() - 2].parse().unwrap(),
                args[args.len() - 1].parse().unwrap(),
            )
        } else if let Some(p) = args.last().and_then(|a| a.parse::<u16>().ok()) {
            ("149.154.167.40".to_string(), 10002, 10002, p)
        } else {
            ("149.154.167.40".to_string(), 10002, 10002, 443)
        };
        loop {
            let stream = TcpStream::connect((dc_ip.as_str(), port)).await.unwrap();
            let transport_type =
                TransportType::new(TransportTypeEnum::Tcp, tx_dc, ProxySecret::default());
            let mut context = Context {
                rsa: RsaInterface {
                    rsa: Rsa::from_pem_public_key(TEST_DC_KEY).unwrap(),
                },
            };
            let mut handshake = AuthKeyHandshake::new(hs_dc, 0);
            let raw_connection =
                match run_handshake(stream, transport_type, &mut context, 30.0, &mut handshake)
                    .await
                {
                    Result::Ok(connection) => connection,
                    Result::Err(status) => {
                        eprintln!("handshake failed: {}", status.message());
                        std::process::exit(1);
                    }
                };
            let auth_key = handshake.release_auth_key();
            println!(
                "auth key created: id={:016x}, time_diff={:.3}, salt={}",
                auth_key.id(),
                handshake.get_server_time_diff(),
                handshake.get_server_salt()
            );

            // 2. AuthData + session.
            let mut auth_data = AuthData::new();
            auth_data.set_main_auth_key(auth_key);
            auth_data.set_use_pfs(false);
            auth_data.reset_server_time_difference(handshake.get_server_time_diff());
            auth_data.set_server_salt(handshake.get_server_salt() as i64, tdutils::time::now());
            let mut session_id = 0u64;
            while session_id == 0 {
                session_id = random::secure_uint64();
            }
            auth_data.set_session_id(session_id);

            if handshake_only {
                println!("gate S66 step 1 (live handshake against the test DC): OK");
                return;
            }

            let mut connection =
                SessionConnection::new(SessionMode::Tcp, raw_connection, auth_data);

            // 3. auth.sendCode once the session is up (queued through the
            //    pending-query path of the callback).
            let mut callback = AuthCallback {
                phone: phone.clone(),
                phone_code_hash: String::new(),
                logged_in: false,
                pending_query: None,
                migrate_dc: None,
            };
            {
                let mut query = Vec::new();
                // Every API query rides in invokeWithLayer{initConnection{...}}
                // — without it the DC answers RPC_ERROR CONNECTION_NOT_INITED.
                // Layer 229 = td 1.8.67 (td/telegram/Version.h MTPROTO_LAYER).
                telegram_api::invokeWithLayer {
                    layer: 229,
                    query: Box::new(telegram_api::Function::initConnection(
                        telegram_api::initConnection {
                            flags: 0,
                            api_id,
                            device_model: "td-rs".to_string(),
                            system_version: "linux".to_string(),
                            app_version: "0.1.0".to_string(),
                            system_lang_code: "en".to_string(),
                            lang_pack: String::new(),
                            lang_code: "en".to_string(),
                            proxy: telegram_api::InputClientProxy {
                                address: String::new(),
                                port: 0,
                            },
                            params: Box::new(telegram_api::JSONValue::jsonNull(
                                telegram_api::jsonNull {},
                            )),
                            query: Box::new(telegram_api::Function::auth_sendCode(
                                telegram_api::auth_sendCode {
                                    phone_number: phone.clone(),
                                    api_id,
                                    api_hash: api_hash.clone(),
                                    settings: telegram_api::CodeSettings {
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
                                    },
                                },
                            )),
                        },
                    )),
                }
                .store_boxed(&mut query);
                callback.pending_query = Some(query);
            }
            connection.set_online(true, true);

            // Drive the session; the callback re-sends its pending query after
            // each flush.
            let result = drive_auth_session(&mut connection, &mut callback).await;
            if result.is_error() {
                eprintln!("session error: {}", result.message());
                std::process::exit(1);
            }
            if callback.logged_in {
                println!("gate S66: OK");
                return;
            }
            if let Some(dc) = callback.migrate_dc.take() {
                eprintln!("auto-migrating to test DC {dc}");
                let (ip, hs, tx) = match dc {
                    1 => ("149.154.175.10", 10001, 10001),
                    2 => ("149.154.167.40", 10002, 10002),
                    3 => ("149.154.175.117", 10003, 10003),
                    other => {
                        eprintln!("no known test endpoint for DC {other}");
                        std::process::exit(1);
                    }
                };
                dc_ip = ip.to_string();
                hs_dc = hs;
                tx_dc = tx;
                continue;
            }
            eprintln!("session closed before authorization");
            std::process::exit(1);
        }
    });
}

/// Drives the session and pumps the callback's pending query after each
/// flush (the C++ callback sends straight into the connection; the Rust
/// callback queues it instead).
async fn drive_auth_session(
    connection: &mut SessionConnection,
    callback: &mut AuthCallback,
) -> Status {
    loop {
        let wakeup_at = connection.flush(callback).await;
        // The DC told us the number lives elsewhere — hand control back to
        // main for a full reconnect (fresh handshake: auth keys do not
        // cross DCs).
        if callback.migrate_dc.is_some() {
            return Status::ok();
        }
        if let Some(query) = callback.pending_query.take() {
            let message_id = connection.send_query(
                BufferSlice::from_slice_bytes(&query),
                false,
                MessageId::default(),
                Vec::new(),
                false,
            );
            eprintln!("sent query {message_id}");
            let wakeup_at = connection.flush(callback).await;
            if callback.migrate_dc.is_some() {
                return Status::ok();
            }
            if wakeup_at == 0.0 {
                return Status::ok();
            }
        } else if wakeup_at == 0.0 {
            return Status::ok();
        } else if callback.logged_in {
            // Keep streaming updates until the process is killed.
        }
        tokio::select! {
            _ = connection.raw_connection().stream().readable() => {}
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
        }
    }
}
