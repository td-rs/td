//! The auth-net dispatch layer — the C++ AuthManager's NetQueryCreator
//! usage (NetQueryCreator.cpp:161-201 `create_auth_query`): builds the
//! `auth.*` query bytes and parses the answers. The queries ride the
//! `NetQueryDispatcher` (the auth-net slice); this module is the wire
//! half, driven by the dispatcher-based sender in `td.rs`.
//!
//! The `CheckPassword` flow is the C++ two-step: `account.getPassword`
//! (fresh salts) → the SRP math → `auth.checkPassword`.

use tdmtproto::dh_callback::DhCallback;
use tdmtproto::dh_handshake::DhHandshake;
use tdmtproto::telegram_api;
use tdutils::bignum::{BigNum, BigNumContext};
use tdutils::random;
use tdutils::status::{Result, Status};

use crate::auth_manager::{
    AuthNetQuery, AuthNetResult, AuthenticationCodeInfo, CodeType, SentCodeKind, WaitPasswordState,
};

/// The in-flight auth step — the parked td_api request's kind, matched
/// against the answer on the callback drain. The `CheckPassword` flow is
/// the C++ two-step (`account.getPassword` → SRP → `auth.checkPassword`),
/// so it occupies two kinds.
#[derive(Debug, Clone)]
pub(crate) enum AuthPendingKind {
    SendCode,
    SignIn,
    BotToken,
    CheckPassword {
        password: String,
    },
    CheckPasswordStep2,
    /// The `account.getPassword` state fetch (the sign-in
    /// SESSION_PASSWORD_NEEDED turn and the explicit `getPassword`).
    GetPassword,
    LogOut,
}

/// The parsed answer (the `AuthNetResult` plus the wire-only payloads the
/// completion needs — the self `user` and the SRP pre-step input).
pub(crate) enum AuthAnswer {
    SentCode(AuthNetResult),
    Authorization {
        user_id: Option<i64>,
        user: Option<telegram_api::user>,
    },
    PasswordOk {
        user: Option<telegram_api::user>,
    },
    LoggedOut,
    /// The `account.getPassword` SRP pre-step answer.
    PasswordSrp {
        srp: telegram_api::inputCheckPasswordSRP,
    },
    PasswordState {
        wait_password_state: WaitPasswordState,
    },
}

/// The TL function for the prepared `AuthNetQuery` (the payload-carrying
/// single-query kinds; `CheckPassword` builds its two functions from the
/// two-step answers).
pub(crate) fn build_auth_net_function(
    query: &AuthNetQuery,
    api_id: i32,
    api_hash: &str,
) -> telegram_api::Function {
    match query {
        AuthNetQuery::SendCode { phone_number } => {
            telegram_api::Function::auth_sendCode(telegram_api::auth_sendCode {
                phone_number: phone_number.clone(),
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
            })
        }
        AuthNetQuery::SignIn {
            phone_number,
            phone_code_hash,
            code,
        } => telegram_api::Function::auth_signIn(telegram_api::auth_signIn {
            // flags bit 0: `phone_code` is a `flags.0?string` — the
            // generated store gates it on the flag (the S66 trap).
            flags: 1,
            phone_number: phone_number.clone(),
            phone_code_hash: phone_code_hash.clone(),
            phone_code: code.clone(),
            email_verification: Some(telegram_api::EmailVerification::emailVerificationCode(
                telegram_api::emailVerificationCode::default(),
            )),
        }),
        AuthNetQuery::BotToken { token } => telegram_api::Function::auth_importBotAuthorization(
            telegram_api::auth_importBotAuthorization {
                flags: 0,
                api_id,
                api_hash: api_hash.to_string(),
                bot_auth_token: token.clone(),
            },
        ),
        AuthNetQuery::LogOut => telegram_api::Function::auth_logOut(telegram_api::auth_logOut),
        AuthNetQuery::GetPassword => {
            telegram_api::Function::account_getPassword(telegram_api::account_getPassword)
        }
        AuthNetQuery::CheckPassword { .. } => {
            // The SRP pre-step answer builds the function (the two-step).
            telegram_api::Function::account_getPassword(telegram_api::account_getPassword)
        }
    }
}

/// The `auth.checkPassword` function from the SRP pre-step answer.
pub(crate) fn build_check_password_function(
    srp: telegram_api::inputCheckPasswordSRP,
) -> telegram_api::Function {
    telegram_api::Function::auth_checkPassword(telegram_api::auth_checkPassword {
        password: Some(telegram_api::InputCheckPasswordSRP::inputCheckPasswordSRP(
            srp,
        )),
    })
}

/// Parse the answer of a phase. `answer` is the dispatcher's `Ok(data)`;
/// the sign-in error surface (PHONE_NUMBER_UNOCCUPIED →
/// WaitRegistration, SESSION_PASSWORD_NEEDED → WaitPassword) arrives via
/// `error` — the C++ routes those to the same state machines.
pub(crate) fn parse_auth_answer(
    kind: &AuthPendingKind,
    answer: Option<&[u8]>,
    error: Option<&str>,
) -> Result<AuthAnswer> {
    // The parse ladder is string-error (the wire parsers are `Result<_,
    // &'static str>`); the wrapper lifts the message into a `Status` for
    // the caller's `send_error` surface.
    match parse_auth_answer_message(kind, answer, error) {
        Ok(answer) => Result::Ok(answer),
        Err(message) => Result::Err(Status::error_message(&message)),
    }
}

fn parse_auth_answer_message(
    kind: &AuthPendingKind,
    answer: Option<&[u8]>,
    error: Option<&str>,
) -> std::result::Result<AuthAnswer, String> {
    match kind {
        AuthPendingKind::SendCode => {
            let data = answer.ok_or_else(|| error_message(error))?;
            let msg = telegram_api::auth_SentCode::parse(data)
                .map_err(|e| format!("auth.sentCode: {e}"))?;
            match msg {
                telegram_api::auth_SentCode::auth_sentCode(msg) => {
                    let (kind, code_info, next_code_info) = sent_code_info(&msg);
                    Ok(AuthAnswer::SentCode(AuthNetResult::SentCode {
                        phone_code_hash: msg.phone_code_hash,
                        kind,
                        code_info,
                        next_code_info,
                        timeout: msg.timeout as f64,
                    }))
                }
                telegram_api::auth_SentCode::auth_sentCodeSuccess(_) => {
                    // `auth.sentCodeSuccess` embeds an authorization — the
                    // number is already logged in (the C++ routes it
                    // straight to on_get_authorization).
                    Err("already authorized via sentCodeSuccess".to_string())
                }
                telegram_api::auth_SentCode::auth_sentCodePaymentRequired(_) => {
                    Ok(AuthAnswer::SentCode(AuthNetResult::SentCode {
                        phone_code_hash: String::new(),
                        kind: SentCodeKind::PaymentRequired,
                        code_info: Default::default(),
                        next_code_info: Default::default(),
                        timeout: 0.0,
                    }))
                }
            }
        }
        AuthPendingKind::SignIn => match error {
            Some("PHONE_NUMBER_UNOCCUPIED") => Ok(AuthAnswer::Authorization {
                user_id: None,
                user: None,
            }),
            Some("SESSION_PASSWORD_NEEDED") => Err("SESSION_PASSWORD_NEEDED".to_string()),
            _ => parse_authorization(answer.ok_or_else(|| error_message(error))?),
        },
        AuthPendingKind::BotToken => {
            parse_authorization(answer.ok_or_else(|| error_message(error))?)
        }
        AuthPendingKind::CheckPassword { password } => {
            let data = answer.ok_or_else(|| error_message(error))?;
            let msg = telegram_api::account_Password::parse(data)
                .map_err(|e| format!("account.password: {e}"))?;
            let srp = password_srp(password.as_bytes(), &msg)
                .ok_or_else(|| "invalid SRP parameters".to_string())?;
            Ok(AuthAnswer::PasswordSrp { srp })
        }
        AuthPendingKind::CheckPasswordStep2 => {
            let data = answer.ok_or_else(|| error_message(error))?;
            let msg = telegram_api::auth_authorization::parse(data)
                .map_err(|e| format!("auth.authorization: {e}"))?;
            let user = authorization_user(&msg);
            Ok(AuthAnswer::PasswordOk { user })
        }
        AuthPendingKind::GetPassword => {
            let data = answer.ok_or_else(|| error_message(error))?;
            let msg = telegram_api::account_Password::parse(data)
                .map_err(|e| format!("account.password: {e}"))?;
            Ok(AuthAnswer::PasswordState {
                wait_password_state: parse_password_state(&msg),
            })
        }
        AuthPendingKind::LogOut => {
            if error.is_some() {
                return Err(error_message(error));
            }
            Ok(AuthAnswer::LoggedOut)
        }
    }
}

fn error_message(error: Option<&str>) -> String {
    match error {
        Some(message) => message.to_string(),
        None => "the answer is missing".to_string(),
    }
}

fn parse_authorization(data: &[u8]) -> std::result::Result<AuthAnswer, String> {
    let msg = telegram_api::auth_authorization::parse(data)
        .map_err(|e| format!("auth.authorization: {e}"))?;
    let (user_id, user) = match &msg.user {
        Some(telegram_api::User::user(user)) => (Some(user.id), Some(user.clone())),
        _ => (None, None),
    };
    Ok(AuthAnswer::Authorization { user_id, user })
}

fn authorization_user(msg: &telegram_api::auth_authorization) -> Option<telegram_api::user> {
    match &msg.user {
        Some(telegram_api::User::user(user)) => Some(user.clone()),
        _ => None,
    }
}

/// `account.getPassword` → the SRP input (the C++
/// `PasswordManager::get_input_check_password` step).
pub(crate) fn password_srp(
    password: &[u8],
    msg: &telegram_api::account_Password,
) -> Option<telegram_api::inputCheckPasswordSRP> {
    match &msg.current_algo {
        Some(telegram_api::PasswordKdfAlgo::passwordKdfAlgoSHA256SHA256PBKDF2HMACSHA512iter100000SHA256ModPow(algo)) => {
            calc_input_check_password_srp(password, algo, &msg.srp_B, msg.srp_id)
        }
        _ => None,
    }
}

/// `account.password` → the `WaitPasswordState` fields (the
/// `authorizationStateWaitPassword` payload).
pub(crate) fn parse_password_state(msg: &telegram_api::account_Password) -> WaitPasswordState {
    let (g, p, salt1, salt2) = match &msg.current_algo {
        Some(telegram_api::PasswordKdfAlgo::passwordKdfAlgoSHA256SHA256PBKDF2HMACSHA512iter100000SHA256ModPow(algo)) => {
            (algo.g, algo.p.clone(), algo.salt1.clone(), algo.salt2.clone())
        }
        _ => (0, Vec::new(), Vec::new(), Vec::new()),
    };
    let email_pattern = if msg.email_unconfirmed_pattern.is_empty() {
        msg.login_email_pattern.clone()
    } else {
        msg.email_unconfirmed_pattern.clone()
    };
    WaitPasswordState {
        current_client_salt: hex(&salt1),
        current_server_salt: hex(&salt2),
        srp_g: g,
        srp_p: hex(&p),
        srp_b: hex(&msg.srp_B),
        srp_id: msg.srp_id,
        hint: msg.hint.clone(),
        has_recovery: msg.has_recovery,
        email_address_pattern: email_pattern,
        has_secure_values: msg.has_secure_values,
    }
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

struct PrimeCallback;

impl DhCallback for PrimeCallback {
    fn is_good_prime(&self, _prime_str: &[u8]) -> i32 {
        -1 // run the full primality checks
    }
    fn add_good_prime(&self, _prime_str: &[u8]) {}
    fn add_bad_prime(&self, _prime_str: &[u8]) {}
}

/// `PasswordManager::get_input_check_password` (PasswordManager.cpp:73) —
/// the SRP-6a check for `auth.checkPassword` (the S66 verbatim port).
#[allow(clippy::too_many_lines)]
fn calc_input_check_password_srp(
    password: &[u8],
    algo: &telegram_api::passwordKdfAlgoSHA256SHA256PBKDF2HMACSHA512iter100000SHA256ModPow,
    srp_b: &[u8],
    srp_id: i64,
) -> Option<telegram_api::inputCheckPasswordSRP> {
    let (client_salt, server_salt, p_bytes) = (&algo.salt1, &algo.salt2, &algo.p);
    if DhHandshake::check_config(algo.g, p_bytes, &PrimeCallback).is_error() {
        return None;
    }
    let p = BigNum::from_binary(p_bytes);
    let b = BigNum::from_binary(srp_b);
    let zero = BigNum::new();
    if BigNum::compare(&zero, &b) != -1
        || BigNum::compare(&b, &p) != -1
        || !(248..=256).contains(&srp_b.len())
    {
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

/// `PasswordManager::calc_password_hash` (td/telegram/PasswordManager.cpp).
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

/// `SendCodeHelper::on_send_code` — the `auth.sentCode` mapping into the
/// helper's two `AuthenticationCodeInfo` slots.
fn sent_code_info(
    msg: &telegram_api::auth_sentCode,
) -> (SentCodeKind, AuthenticationCodeInfo, AuthenticationCodeInfo) {
    use telegram_api::auth_CodeType as Next;
    use telegram_api::auth_SentCodeType as T;

    let convert = |ty: CodeType, length: i32, pattern: &str| AuthenticationCodeInfo {
        ty,
        length,
        push_timeout: 0,
        cloud_project_number: 0,
        pattern: pattern.to_string(),
    };
    let default_ty =
        telegram_api::auth_SentCodeType::auth_sentCodeTypeSms(telegram_api::auth_sentCodeTypeSms {
            length: 5,
        });
    let ty = msg.r#type.as_ref().unwrap_or(&default_ty);
    let info = match ty {
        T::auth_sentCodeTypeApp(t) => convert(CodeType::Message, t.length, ""),
        T::auth_sentCodeTypeSms(t) => convert(CodeType::Sms, t.length, ""),
        T::auth_sentCodeTypeCall(t) => convert(CodeType::Call, t.length, ""),
        T::auth_sentCodeTypeFlashCall(t) => convert(CodeType::FlashCall, 0, &t.pattern),
        T::auth_sentCodeTypeMissedCall(t) => convert(CodeType::MissedCall, t.length, &t.prefix),
        T::auth_sentCodeTypeFirebaseSms(t) => {
            convert(CodeType::FirebaseAndroidSafetyNet, t.length, "")
        }
        T::auth_sentCodeTypeEmailCode(_) => convert(CodeType::None, 0, ""),
        T::auth_sentCodeTypeSetUpEmailRequired(_) => convert(CodeType::None, 0, ""),
        _ => convert(CodeType::None, 0, ""),
    };
    let default_next =
        telegram_api::auth_CodeType::auth_codeTypeSms(telegram_api::auth_codeTypeSms);
    let next = match msg.next_type.as_ref().unwrap_or(&default_next) {
        Next::auth_codeTypeSms(_) => convert(CodeType::Sms, 0, ""),
        Next::auth_codeTypeCall(_) => convert(CodeType::Call, 0, ""),
        Next::auth_codeTypeFlashCall(_) => convert(CodeType::FlashCall, 0, ""),
        Next::auth_codeTypeMissedCall(_) => convert(CodeType::MissedCall, 0, ""),
        Next::auth_codeTypeFragmentSms(_) => convert(CodeType::Fragment, 0, ""),
    };
    let kind = match ty {
        T::auth_sentCodeTypeSetUpEmailRequired(_) => SentCodeKind::EmailAddressRequired,
        T::auth_sentCodeTypeEmailCode(_) => SentCodeKind::EmailCode,
        _ => SentCodeKind::Code,
    };
    (kind, info, next)
}
