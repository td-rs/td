//! Port of `td/telegram/PhoneNumberManager.{h,cpp}` (the glue slice): the
//! change-phone / verify-phone / confirm-ownership code flows — the
//! `SendCodeHelper` state, the generation guard (the C++
//! `inc_generation` / `"Request was canceled"` band), the send/resend
//! answers (`auth.sentCode` → the `authenticationCodeInfo` object) and
//! the check-code queries (`account.changePhone`, `account.verifyPhone`,
//! `account.confirmPhone`). The queries ride the NetQueryDispatcher via
//! the `td.rs` walking-query sender (the auth-net convention); the wire
//! helpers live here:
//!
//! * `SendCodeHelper::get_input_code_settings` (SendCodeHelper.cpp:59-113)
//! * `SendCodeHelper::send_change_phone_code` / `send_verify_phone_code` /
//!   `send_confirm_phone_code` (SendCodeHelper.cpp:147-164)
//! * `SendCodeHelper::resend_code` (SendCodeHelper.cpp:48-67)
//! * `SendCodeHelper::request_firebase_sms` (SendCodeHelper.cpp:116-137)
//! * `SendCodeHelper::report_missing_code` (SendCodeHelper.cpp:139-142)
//! * the `SendCodeQuery` / `RequestFirebaseSmsQuery` /
//!   `ReportMissingCodeQuery` / `ChangePhoneQuery` / `VerifyPhoneQuery` /
//!   `ConfirmPhoneQuery` ResultHandlers (PhoneNumberManager.cpp:22-181)

use tdmtproto::telegram_api;
use tdutils::status::{Result, Status};

use crate::auth_manager::{AuthenticationCodeInfo, CodeType, SendCodeHelper};
use crate::td_api;

/// `PhoneNumberManager::Type` (PhoneNumberManager.h:41).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PhoneCodeFlow {
    #[default]
    None,
    ChangePhone,
    VerifyPhone,
    ConfirmPhone,
}

/// `PhoneNumberManager::State` (PhoneNumberManager.h:42).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhoneState {
    Ok,
    WaitCode,
}

/// The parked phone-plane request's kind (the C++ captured-generation
/// `PromiseCreator::lambda`s of `send_new_send_code_query` and
/// `check_code`), matched against the answer on the drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhonePendingKind {
    SendCode { generation: i64 },
    ResendCode { generation: i64 },
    CheckCode { check: PhoneCheckQuery, generation: i64 },
    FirebaseSms,
    ReportMissingCode,
}

impl PhonePendingKind {
    /// The captured generation (`None` — the queries without the guard).
    pub(crate) fn generation(&self) -> Option<i64> {
        match self {
            PhonePendingKind::SendCode { generation }
            | PhonePendingKind::ResendCode { generation }
            | PhonePendingKind::CheckCode { generation, .. } => Some(*generation),
            PhonePendingKind::FirebaseSms | PhonePendingKind::ReportMissingCode => None,
        }
    }
}

/// The check-code query family (the C++ `check_code` type switch — the
/// concrete query is fixed at dispatch, so the answer parse is too).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhoneCheckQuery {
    ChangePhone,
    VerifyPhone,
    ConfirmPhone,
}

/// The parsed phone-plane answer (the ResultHandler `on_result` shapes).
#[derive(Debug)]
pub(crate) enum PhoneAnswer {
    /// `auth.sentCode` — the send/resend answer; applied by the manager
    /// (the generation guard + the helper update).
    SentCode(telegram_api::auth_sentCode),
    /// The changed self user (`account.changePhone` — the C++
    /// `ChangePhoneQuery::on_result` feeds `user_manager_->on_get_user`).
    ChangedUser(telegram_api::user),
    /// The Unit answers (`Bool` — verify/confirm/firebase/report).
    Ok,
}

/// The prepared send-code dispatch (the C++ `send_new_send_code_query`
/// inputs): the telegram_api function plus the generation the answer
/// must match.
#[derive(Debug)]
pub(crate) struct PreparedSendCode {
    pub(crate) generation: i64,
    pub(crate) function: telegram_api::Function,
    /// The `SuggestedAction::Type::CheckPhoneNumber` hide that fires
    /// between the validation and the query in the ChangePhone arm
    /// (PhoneNumberManager.cpp:250-251); the suggested-action manager
    /// lives on `Td`, so the flag rides back to the caller.
    pub(crate) hide_check_phone_number: bool,
}

/// The prepared check-code dispatch (the `check_code` type switch).
#[derive(Debug)]
pub(crate) struct PreparedCheckCode {
    pub(crate) check: PhoneCheckQuery,
    pub(crate) generation: i64,
    pub(crate) function: telegram_api::Function,
}

/// `td::PhoneNumberManager` — the collapsed actor (the manager is a plain
/// `Td` member like the other collapsed managers).
#[derive(Debug)]
pub(crate) struct PhoneNumberManager {
    flow: PhoneCodeFlow,
    state: PhoneState,
    generation: i64,
    send_code_helper: SendCodeHelper,
}

impl Default for PhoneNumberManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PhoneNumberManager {
    pub(crate) fn new() -> Self {
        Self {
            flow: PhoneCodeFlow::None,
            state: PhoneState::Ok,
            generation: 0,
            send_code_helper: SendCodeHelper::default(),
        }
    }

    /// `PhoneNumberManager::inc_generation` (cpp:190-194): a new request
    /// invalidates every in-flight answer and resets the helper state.
    fn inc_generation(&mut self) {
        self.generation += 1;
        self.state = PhoneState::Ok;
        self.send_code_helper = SendCodeHelper::default();
    }

    /// `PhoneNumberManager::set_phone_number` (cpp:235-274), phase 1: the
    /// generation bump, the validation ladder and the query build. The
    /// `type == nullptr` band ("Type must be non-empty") is expressible —
    /// the generated model keeps the abstract types optional.
    pub(crate) fn prepare_set_phone_number(
        &mut self,
        phone_number: String,
        settings: Option<&td_api::PhoneNumberAuthenticationSettings>,
        ty: Option<&td_api::PhoneNumberCodeType>,
    ) -> std::result::Result<PreparedSendCode, Status> {
        self.inc_generation();
        if phone_number.is_empty() {
            return Err(Status::error(400, "Phone number must be non-empty"));
        }
        let Some(ty) = ty else {
            return Err(Status::error(400, "Type must be non-empty"));
        };
        let (flow, function, hide_check_phone_number) = match ty {
            td_api::PhoneNumberCodeType::phoneNumberCodeTypeChange(_) => {
                self.send_code_helper.phone_number = phone_number;
                (
                    PhoneCodeFlow::ChangePhone,
                    telegram_api::Function::account_sendChangePhoneCode(
                        telegram_api::account_sendChangePhoneCode {
                            phone_number: self.send_code_helper.phone_number.clone(),
                            settings: Some(get_input_code_settings(settings)),
                        },
                    ),
                    true,
                )
            }
            td_api::PhoneNumberCodeType::phoneNumberCodeTypeVerify(_) => {
                self.send_code_helper.phone_number = phone_number;
                (
                    PhoneCodeFlow::VerifyPhone,
                    telegram_api::Function::account_sendVerifyPhoneCode(
                        telegram_api::account_sendVerifyPhoneCode {
                            phone_number: self.send_code_helper.phone_number.clone(),
                            settings: Some(get_input_code_settings(settings)),
                        },
                    ),
                    false,
                )
            }
            td_api::PhoneNumberCodeType::phoneNumberCodeTypeConfirmOwnership(ownership) => {
                // The hash clean lives inside the manager in the C++
                // (PhoneNumberManager.cpp:259-265); the UTF-8 band is
                // inexpressible on the Rust `String` (the CLEAN_INPUT_STRING
                // collapse) — the cleanup side effect is kept.
                let hash = crate::misc::clean_input_string(&ownership.hash).unwrap_or_default();
                if hash.is_empty() {
                    return Err(Status::error(400, "Hash must be non-empty"));
                }
                self.send_code_helper.phone_number = phone_number;
                (
                    PhoneCodeFlow::ConfirmPhone,
                    telegram_api::Function::account_sendConfirmPhoneCode(
                        telegram_api::account_sendConfirmPhoneCode {
                            hash,
                            settings: Some(get_input_code_settings(settings)),
                        },
                    ),
                    false,
                )
            }
        };
        self.flow = flow;
        Ok(PreparedSendCode {
            generation: self.generation,
            function,
            hide_check_phone_number,
        })
    }

    /// `PhoneNumberManager::send_firebase_sms` (cpp:276-282), phase 1.
    pub(crate) fn prepare_send_firebase_sms(
        &self,
        token: &str,
    ) -> std::result::Result<telegram_api::Function, Status> {
        if self.state != PhoneState::WaitCode {
            return Err(Status::error(400, "Can't send Firebase SMS"));
        }
        Ok(telegram_api::Function::auth_requestFirebaseSms(
            request_firebase_sms_query(&self.send_code_helper, token),
        ))
    }

    /// `PhoneNumberManager::report_missing_code` (cpp:284-291), phase 1.
    pub(crate) fn prepare_report_missing_code(
        &self,
        mobile_network_code: &str,
    ) -> std::result::Result<telegram_api::Function, Status> {
        if self.state != PhoneState::WaitCode {
            return Err(Status::error(400, "Can't report missing code"));
        }
        Ok(telegram_api::Function::auth_reportMissingCode(
            telegram_api::auth_reportMissingCode {
                phone_number: self.send_code_helper.phone_number.clone(),
                phone_code_hash: self.send_code_helper.phone_code_hash.clone(),
                mnc: mobile_network_code.to_string(),
            },
        ))
    }

    /// `PhoneNumberManager::resend_authentication_code` (cpp:293-302),
    /// phase 1: no generation bump (the C++ reuses the captured
    /// generation), and the next-type band from
    /// `SendCodeHelper::resend_code` (SendCodeHelper.cpp:48-53).
    pub(crate) fn prepare_resend_code(
        &self,
        reason: Option<&td_api::ResendCodeReason>,
    ) -> std::result::Result<PreparedSendCode, Status> {
        if self.state != PhoneState::WaitCode {
            return Err(Status::error(400, "Can't resend code"));
        }
        let query = match resend_code_query(&self.send_code_helper, reason) {
            Result::Ok(query) => query,
            Result::Err(status) => return Err(status),
        };
        Ok(PreparedSendCode {
            generation: self.generation,
            function: telegram_api::Function::auth_resendCode(query),
            hide_check_phone_number: false,
        })
    }

    /// `PhoneNumberManager::check_code` (cpp:304-330), phase 1: the query
    /// family is fixed by the flow at dispatch time.
    pub(crate) fn prepare_check_code(
        &self,
        code: String,
    ) -> std::result::Result<PreparedCheckCode, Status> {
        if self.state != PhoneState::WaitCode {
            return Err(Status::error(400, "Can't check code"));
        }
        let (check, function) = match self.flow {
            PhoneCodeFlow::ChangePhone => (
                PhoneCheckQuery::ChangePhone,
                telegram_api::Function::account_changePhone(telegram_api::account_changePhone {
                    phone_number: self.send_code_helper.phone_number.clone(),
                    phone_code_hash: self.send_code_helper.phone_code_hash.clone(),
                    phone_code: code,
                }),
            ),
            PhoneCodeFlow::VerifyPhone => (
                PhoneCheckQuery::VerifyPhone,
                telegram_api::Function::account_verifyPhone(telegram_api::account_verifyPhone {
                    phone_number: self.send_code_helper.phone_number.clone(),
                    phone_code_hash: self.send_code_helper.phone_code_hash.clone(),
                    phone_code: code,
                }),
            ),
            PhoneCodeFlow::ConfirmPhone => (
                PhoneCheckQuery::ConfirmPhone,
                telegram_api::Function::account_confirmPhone(telegram_api::account_confirmPhone {
                    phone_code_hash: self.send_code_helper.phone_code_hash.clone(),
                    phone_code: code,
                }),
            ),
            // UNREACHABLE in the C++ (the state band above keeps the flow
            // at a live value); the collapse keeps the default answer.
            PhoneCodeFlow::None => {
                return Err(Status::error(400, "Can't check code"));
            }
        };
        Ok(PreparedCheckCode {
            check,
            generation: self.generation,
            function,
        })
    }

    /// `PhoneNumberManager::on_send_code_result` (cpp:207-233), phase 2:
    /// the generation guard, the email-type band and the helper update.
    /// The `auth_sentCodeSuccess`/`auth_sentCodePaymentRequired` band is
    /// the ResultHandler's (the parse layer, [`parse_phone_answer`]).
    pub(crate) fn on_send_code_result(
        &mut self,
        generation: i64,
        sent_code: telegram_api::auth_sentCode,
    ) -> Result<td_api::AuthenticationCodeInfo> {
        if generation != self.generation {
            return Result::Err(Status::error(500, "Request was canceled"));
        }
        if let Some(ty) = &sent_code.r#type {
            use telegram_api::auth_SentCodeType as T;
            if matches!(
                ty,
                T::auth_sentCodeTypeSetUpEmailRequired(_) | T::auth_sentCodeTypeEmailCode(_)
            ) {
                return Result::Err(Status::error(500, "Receive incorrect response"));
            }
        }
        self.phone_on_sent_code(sent_code);
        self.state = PhoneState::WaitCode;
        Result::Ok(self
            .send_code_helper
            .get_authentication_code_info_object(tdutils::time::now()))
    }

    /// `PhoneNumberManager::on_check_code_result` (cpp:332-344), phase 2:
    /// the generation guard, then the success resets the state (the
    /// `inc_generation` tail — the helper is dropped with the new
    /// generation).
    pub(crate) fn on_check_code_result(&mut self, generation: i64) -> Result<()> {
        if generation != self.generation {
            return Result::Err(Status::error(500, "Request was canceled"));
        }
        self.inc_generation();
        Result::Ok(())
    }

    /// `SendCodeHelper::on_sent_code` (SendCodeHelper.cpp:19-36) — the
    /// helper slots plus the Firebase→Sms next-type fallback.
    fn phone_on_sent_code(&mut self, sent_code: telegram_api::auth_sentCode) {
        self.send_code_helper.phone_code_hash = sent_code.phone_code_hash;
        self.send_code_helper.sent_code_info = match &sent_code.r#type {
            Some(ty) => get_sent_authentication_code_info(ty),
            // CHECK(sent_code_type_ptr != nullptr) in the C++; the
            // generated parse is total.
            None => AuthenticationCodeInfo::default(),
        };
        self.send_code_helper.next_code_info = get_authentication_code_info(sent_code.next_type.as_ref());
        self.send_code_helper.next_code_timestamp = tdutils::time::now() + sent_code.timeout as f64;

        if self.send_code_helper.next_code_info.ty == CodeType::None
            && matches!(
                self.send_code_helper.sent_code_info.ty,
                CodeType::FirebaseAndroidSafetyNet
                    | CodeType::FirebaseAndroidPlayIntegrity
                    | CodeType::FirebaseIos
            )
        {
            self.send_code_helper.next_code_info = AuthenticationCodeInfo {
                ty: CodeType::Sms,
                length: self.send_code_helper.sent_code_info.length,
                ..AuthenticationCodeInfo::default()
            };
        }
    }
}

/// `SendCodeHelper::get_authentication_code_info`
/// (SendCodeHelper.cpp:166-186): the `auth_CodeType` mapping; `None` is
/// the default (Type::None) info.
fn get_authentication_code_info(code_type: Option<&telegram_api::auth_CodeType>) -> AuthenticationCodeInfo {
    use telegram_api::auth_CodeType as T;
    let Some(ty) = code_type else {
        return AuthenticationCodeInfo::default();
    };
    let info = |code_type| AuthenticationCodeInfo {
        ty: code_type,
        length: 0,
        ..AuthenticationCodeInfo::default()
    };
    match ty {
        T::auth_codeTypeSms(_) => info(CodeType::Sms),
        T::auth_codeTypeCall(_) => info(CodeType::Call),
        T::auth_codeTypeFlashCall(_) => info(CodeType::FlashCall),
        T::auth_codeTypeMissedCall(_) => info(CodeType::MissedCall),
        T::auth_codeTypeFragmentSms(_) => info(CodeType::Fragment),
    }
}

/// `SendCodeHelper::get_sent_authentication_code_info`
/// (SendCodeHelper.cpp:189-256) — the platform-less build: neither
/// `TD_ANDROID` nor `TD_DARWIN` is defined for this build (the
/// notification_settings.rs convention), so `auth_sentCodeTypeFirebaseSms`
/// falls back to `Sms` (the C++ falls through both platform branches).
fn get_sent_authentication_code_info(
    sent_code_type: &telegram_api::auth_SentCodeType,
) -> AuthenticationCodeInfo {
    use telegram_api::auth_SentCodeType as T;
    let info = |ty, length: i32, pattern: &str| AuthenticationCodeInfo {
        ty,
        length,
        pattern: pattern.to_string(),
        ..AuthenticationCodeInfo::default()
    };
    match sent_code_type {
        T::auth_sentCodeTypeApp(t) => info(CodeType::Message, t.length, ""),
        T::auth_sentCodeTypeSms(t) => info(CodeType::Sms, t.length, ""),
        T::auth_sentCodeTypeCall(t) => info(CodeType::Call, t.length, ""),
        T::auth_sentCodeTypeFlashCall(t) => info(CodeType::FlashCall, 0, &t.pattern),
        T::auth_sentCodeTypeMissedCall(t) => info(CodeType::MissedCall, t.length, &t.prefix),
        T::auth_sentCodeTypeFragmentSms(t) => info(CodeType::Fragment, t.length, &t.url),
        T::auth_sentCodeTypeFirebaseSms(t) => info(CodeType::Sms, t.length, ""),
        T::auth_sentCodeTypeSmsWord(t) => {
            // The >1-letter guard (SendCodeHelper.cpp:241-245).
            let beginning = if tdutils::utf8::utf8_length(&t.beginning) > 1 {
                tracing::error!(
                    "Receive \"{}\" as word first letter",
                    t.beginning
                );
                String::new()
            } else {
                t.beginning.clone()
            };
            info(CodeType::SmsWord, 0, &beginning)
        }
        T::auth_sentCodeTypeSmsPhrase(t) => info(CodeType::SmsPhrase, 0, &t.beginning),
        // UNREACHABLE in the C++ (the manager band rejects the email
        // types before this); the parse is total here.
        T::auth_sentCodeTypeEmailCode(_) | T::auth_sentCodeTypeSetUpEmailRequired(_) => {
            AuthenticationCodeInfo::default()
        }
    }
}

/// `SendCodeHelper::get_input_code_settings` (SendCodeHelper.cpp:59-113).
/// The generated `codeSettings` store ORs the plain bools into the flags
/// word itself; the C++ `TOKEN_MASK` (flags.8) and `LOGOUT_TOKENS_MASK`
/// (flags.6) bits are set here like the C++ does. The `settings ==
/// nullptr` band is expressible: `None` takes the all-defaults path of
/// the C++ guard.
fn get_input_code_settings(
    settings: Option<&td_api::PhoneNumberAuthenticationSettings>,
) -> telegram_api::CodeSettings {
    let mut flags = 0;
    let mut logout_tokens: Vec<Vec<u8>> = Vec::new();
    let mut device_token = String::new();
    let mut is_app_sandbox = false;
    let mut allow_flashcall = false;
    let mut current_number = false;
    let mut allow_app_hash = false;
    let mut allow_missed_call = false;
    let mut unknown_number = false;
    let mut allow_firebase = false;

    if let Some(settings) = settings {
        allow_flashcall = settings.allow_flash_call;
        current_number = settings.is_current_phone_number;
        allow_app_hash = settings.allow_sms_retriever_api;
        allow_missed_call = settings.allow_missed_call;
        unknown_number = settings.has_unknown_phone_number;
        if let Some(firebase) = &settings.firebase_authentication_settings {
            allow_firebase = true;
            if let td_api::FirebaseAuthenticationSettings::firebaseAuthenticationSettingsIos(
                ios,
            ) = firebase
            {
                flags |= CODE_SETTINGS_TOKEN_MASK;
                device_token = ios.device_token.clone();
                is_app_sandbox = ios.is_app_sandbox;
            }
        }

        // The server-side logout-token limit (SendCodeHelper.cpp:100).
        const MAX_LOGOUT_TOKENS: usize = 20;
        for token in &settings.authentication_tokens {
            if let Ok(decoded) = base64url_decode(token) {
                logout_tokens.push(decoded);
                if logout_tokens.len() >= MAX_LOGOUT_TOKENS {
                    break;
                }
            }
        }
        if !logout_tokens.is_empty() {
            flags |= CODE_SETTINGS_LOGOUT_TOKENS_MASK;
        }
    }

    telegram_api::CodeSettings {
        flags,
        allow_flashcall,
        current_number,
        allow_app_hash,
        allow_missed_call,
        allow_firebase,
        unknown_number,
        logout_tokens,
        token: device_token,
        app_sandbox: is_app_sandbox,
    }
}

/// `telegram_api::codeSettings::TOKEN_MASK` — the `token:flags.8?string`
/// bit of the pinned scheme (the C++ generated constant).
const CODE_SETTINGS_TOKEN_MASK: i32 = 1 << 8;
/// `telegram_api::codeSettings::LOGOUT_TOKENS_MASK` — the
/// `logout_tokens:flags.6?Vector<bytes>` bit.
const CODE_SETTINGS_LOGOUT_TOKENS_MASK: i32 = 1 << 6;

/// `SendCodeHelper::resend_code` (SendCodeHelper.cpp:48-67). The
/// verification-failed reason rides the REASON_MASK bit
/// (`reason:flags.0?string` of the pinned scheme); the C++ puts the raw
/// string in the struct and gates only the flag — the generated store
/// drops the value with the flag cleared. The `reason == nullptr` band
/// is expressible (the generated model keeps the abstract type
/// optional).
fn resend_code_query(
    send_code_helper: &SendCodeHelper,
    reason: Option<&td_api::ResendCodeReason>,
) -> Result<telegram_api::auth_resendCode> {
    if send_code_helper.next_code_info.ty == CodeType::None {
        return Result::Err(Status::error(
            400,
            "Authentication code can't be resend",
        ));
    }
    let mut flags = 0;
    let reason_str = match reason {
        Some(td_api::ResendCodeReason::resendCodeReasonVerificationFailed(failed)) => {
            failed.error_message.clone()
        }
        _ => String::new(),
    };
    if !reason_str.is_empty() && crate::misc::clean_input_string(&reason_str).is_some() {
        flags |= AUTH_RESEND_CODE_REASON_MASK;
    }
    Result::Ok(telegram_api::auth_resendCode {
        flags,
        phone_number: send_code_helper.phone_number.clone(),
        phone_code_hash: send_code_helper.phone_code_hash.clone(),
        reason: reason_str,
    })
}

/// `telegram_api::auth_resendCode::REASON_MASK` — the `reason:flags.0`
/// bit of the pinned scheme.
const AUTH_RESEND_CODE_REASON_MASK: i32 = 1;

/// `SendCodeHelper::request_firebase_sms` (SendCodeHelper.cpp:116-137) —
/// the platform-less build: neither `TD_ANDROID` nor `TD_DARWIN` branch
/// fires, so the flags stay empty (the C++ no-define fall-through).
fn request_firebase_sms_query(
    send_code_helper: &SendCodeHelper,
    _token: &str,
) -> telegram_api::auth_requestFirebaseSms {
    telegram_api::auth_requestFirebaseSms {
        flags: 0,
        phone_number: send_code_helper.phone_number.clone(),
        phone_code_hash: send_code_helper.phone_code_hash.clone(),
        safety_net_token: String::new(),
        play_integrity_token: String::new(),
        ios_push_secret: String::new(),
    }
}

/// The phone-plane answer parse (the `SendCodeQuery` / `ChangePhoneQuery`
/// / `VerifyPhoneQuery` / `ConfirmPhoneQuery` / `RequestFirebaseSmsQuery`
/// / `ReportMissingCodeQuery` `on_result`s). Wire errors pass through
/// with their server code; the parse errors take the C++ local bands.
pub(crate) fn parse_phone_answer(
    kind: &PhonePendingKind,
    answer: Option<&[u8]>,
    error: Option<&Status>,
) -> Result<PhoneAnswer> {
    let Some(data) = answer else {
        return Result::Err(match error {
            Some(status) => Status::error(status.code(), status.message()),
            None => Status::error(500, "the answer is missing"),
        });
    };
    match kind {
        PhonePendingKind::SendCode { .. } | PhonePendingKind::ResendCode { .. } => {
            let msg = match telegram_api::auth_SentCode::parse(data) {
                Ok(msg) => msg,
                Err(_) => {
                    return Result::Err(Status::error(500, "Can't parse auth.sentCode"));
                }
            };
            match msg {
                telegram_api::auth_SentCode::auth_sentCode(sent_code) => {
                    Result::Ok(PhoneAnswer::SentCode(sent_code))
                }
                // SendCodeQuery::on_result (PhoneNumberManager.cpp:41-44).
                telegram_api::auth_SentCode::auth_sentCodeSuccess(_)
                | telegram_api::auth_SentCode::auth_sentCodePaymentRequired(_) => {
                    Result::Err(Status::error(500, "Receive invalid response"))
                }
            }
        }
        PhonePendingKind::CheckCode {
            check: PhoneCheckQuery::ChangePhone,
            ..
        } => match telegram_api::user::parse(data) {
            Ok(user) => Result::Ok(PhoneAnswer::ChangedUser(user)),
            Err(_) => Result::Err(Status::error(500, "Can't parse user")),
        },
        PhonePendingKind::CheckCode { .. }
        | PhonePendingKind::FirebaseSms
        | PhonePendingKind::ReportMissingCode => Result::Ok(PhoneAnswer::Ok),
    }
}

/// `td::base64url_decode` (tdutils/base64.cpp:156) — the bytes form the
/// logout tokens need (the RFC 4648 base64url alphabet, padding
/// optional).
fn base64url_decode(base64: &str) -> std::result::Result<Vec<u8>, ()> {
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
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tdutils `Result` carries no `unwrap_err`; the tests pull the
    /// status out through this helper.
    fn err_of<T: std::fmt::Debug>(result: Result<T>) -> Status {
        match result {
            Result::Ok(value) => panic!("expected an error, got {value:?}"),
            Result::Err(status) => status,
        }
    }

    /// The default settings object (the android Firebase variant).
    fn default_settings() -> td_api::PhoneNumberAuthenticationSettings {
        td_api::PhoneNumberAuthenticationSettings {
            allow_flash_call: false,
            allow_missed_call: false,
            is_current_phone_number: false,
            has_unknown_phone_number: false,
            allow_sms_retriever_api: false,
            firebase_authentication_settings: Some(
                td_api::FirebaseAuthenticationSettings::firebaseAuthenticationSettingsAndroid(
                    td_api::firebaseAuthenticationSettingsAndroid,
                ),
            ),
            authentication_tokens: Vec::new(),
        }
    }

    fn change_type() -> td_api::PhoneNumberCodeType {
        td_api::PhoneNumberCodeType::phoneNumberCodeTypeChange(td_api::phoneNumberCodeTypeChange)
    }

    fn confirm_type(hash: &str) -> td_api::PhoneNumberCodeType {
        td_api::PhoneNumberCodeType::phoneNumberCodeTypeConfirmOwnership(
            td_api::phoneNumberCodeTypeConfirmOwnership {
                hash: hash.to_string(),
            },
        )
    }

    /// The `auth.sentCode` wire answer (SMS type, call next type).
    fn sent_code_answer_bytes() -> Vec<u8> {
        let answer = telegram_api::auth_SentCode::auth_sentCode(telegram_api::auth_sentCode {
            flags: 0,
            r#type: Some(telegram_api::auth_SentCodeType::auth_sentCodeTypeSms(
                telegram_api::auth_sentCodeTypeSms { length: 5 },
            )),
            phone_code_hash: "hash1".to_string(),
            next_type: Some(telegram_api::auth_CodeType::auth_codeTypeCall(
                telegram_api::auth_codeTypeCall,
            )),
            timeout: 60,
        });
        let mut bytes = Vec::new();
        answer.store(&mut bytes);
        bytes
    }

    #[test]
    fn send_code_validation_ladder_matches_the_cpp() {
        let mut manager = PhoneNumberManager::new();
        let settings = default_settings();

        // The empty-number band fires after the generation bump.
        let error = manager
            .prepare_set_phone_number(String::new(), Some(&settings), Some(&change_type()))
            .unwrap_err();
        assert_eq!((error.code(), error.message()), (400, "Phone number must be non-empty"));

        // The `type == nullptr` band (PhoneNumberManager.cpp:243-245).
        let error = manager
            .prepare_set_phone_number("+42".to_string(), Some(&settings), None)
            .unwrap_err();
        assert_eq!((error.code(), error.message()), (400, "Type must be non-empty"));

        // The confirm arm cleans and empties the hash.
        let error = manager
            .prepare_set_phone_number("+42".to_string(), Some(&settings), Some(&confirm_type("")))
            .unwrap_err();
        assert_eq!((error.code(), error.message()), (400, "Hash must be non-empty"));

        // A control-only hash cleans to empty (the C++ checks the cleaned
        // hash) — the empty band still fires.
        let error = manager
            .prepare_set_phone_number("+42".to_string(), Some(&settings), Some(&confirm_type("\r")))
            .unwrap_err();
        assert_eq!((error.code(), error.message()), (400, "Hash must be non-empty"));
    }

    #[test]
    fn send_code_change_flow_hides_the_suggested_action_and_builds_the_query() {
        let mut manager = PhoneNumberManager::new();
        let settings = default_settings();
        let prepared = manager
            .prepare_set_phone_number("+15551234567".to_string(), Some(&settings), Some(&change_type()))
            .unwrap();
        assert!(prepared.hide_check_phone_number);
        assert_eq!(manager.flow, PhoneCodeFlow::ChangePhone);

        let telegram_api::Function::account_sendChangePhoneCode(query) = &prepared.function else {
            panic!("wrong query");
        };
        assert_eq!(query.phone_number, "+15551234567");
        let code_settings = query.settings.as_ref().unwrap();
        // The android Firebase variant sets allow_firebase, no TOKEN_MASK.
        assert_eq!(code_settings.flags & CODE_SETTINGS_TOKEN_MASK, 0);
        assert!(code_settings.allow_firebase);

        // The null-settings band takes the C++ all-defaults path.
        let prepared = manager
            .prepare_set_phone_number("+15551234567".to_string(), None, Some(&change_type()))
            .unwrap();
        let telegram_api::Function::account_sendChangePhoneCode(query) = &prepared.function
        else {
            panic!("wrong query");
        };
        let empty_settings = query.settings.as_ref().unwrap();
        assert_eq!(empty_settings.flags, 0);
        assert!(!empty_settings.allow_firebase);
        assert!(!empty_settings.allow_flashcall);

        // The verify flow does not hide the suggested action.
        let prepared = manager
            .prepare_set_phone_number(
                "+15551234567".to_string(),
                Some(&default_settings()),
                Some(&td_api::PhoneNumberCodeType::phoneNumberCodeTypeVerify(
                    td_api::phoneNumberCodeTypeVerify,
                )),
            )
            .unwrap();
        assert!(!prepared.hide_check_phone_number);
        assert!(matches!(
            &prepared.function,
            telegram_api::Function::account_sendVerifyPhoneCode(_)
        ));
    }

    #[test]
    fn sent_code_answer_waits_the_code_and_answers_the_code_info() {
        let mut manager = PhoneNumberManager::new();
        let settings = default_settings();
        let prepared = manager
            .prepare_set_phone_number("+42".to_string(), Some(&settings), Some(&change_type()))
            .unwrap();

        let answer = match parse_phone_answer(
            &PhonePendingKind::SendCode {
                generation: prepared.generation,
            },
            Some(&sent_code_answer_bytes()),
            None,
        ) {
            Result::Ok(PhoneAnswer::SentCode(sent_code)) => sent_code,
            other => panic!("wrong answer {other:?}"),
        };
        let info = manager
            .on_send_code_result(prepared.generation, answer)
            .unwrap();
        assert_eq!(manager.state, PhoneState::WaitCode);
        assert_eq!(info.phone_number, "+42");
        assert_eq!(info.timeout, 61);
        assert!(matches!(
            &info.r#type,
            Some(td_api::AuthenticationCodeType::authenticationCodeTypeSms(t)) if t.length == 5
        ));
        assert!(matches!(
            &info.next_type,
            Some(td_api::AuthenticationCodeType::authenticationCodeTypeCall(_))
        ));
    }

    #[test]
    fn stale_generation_and_email_types_take_the_cpp_bands() {
        let mut manager = PhoneNumberManager::new();
        let settings = default_settings();
        let prepared = manager
            .prepare_set_phone_number("+42".to_string(), Some(&settings), Some(&change_type()))
            .unwrap();

        // The stale answer (a newer request bumped the generation).
        let sent_code = telegram_api::auth_sentCode {
            flags: 0,
            r#type: Some(telegram_api::auth_SentCodeType::auth_sentCodeTypeSms(
                telegram_api::auth_sentCodeTypeSms { length: 5 },
            )),
            phone_code_hash: "hash".to_string(),
            next_type: None,
            timeout: 10,
        };
        let error = err_of(
            manager
                .on_send_code_result(prepared.generation + 1, sent_code.clone()),
        );
        assert_eq!((error.code(), error.message()), (500, "Request was canceled"));

        // The email types are rejected with the manager band.
        let mut email_code = sent_code.clone();
        email_code.r#type =
            Some(telegram_api::auth_SentCodeType::auth_sentCodeTypeEmailCode(
                telegram_api::auth_sentCodeTypeEmailCode::default(),
            ));
        let error = err_of(manager.on_send_code_result(prepared.generation, email_code));
        assert_eq!((error.code(), error.message()), (500, "Receive incorrect response"));

        // The ResultHandler band: success/payment answers never apply.
        let success =
            telegram_api::auth_SentCode::auth_sentCodeSuccess(telegram_api::auth_sentCodeSuccess {
                authorization: None,
            });
        let mut bytes = Vec::new();
        success.store(&mut bytes);
        let error = err_of(parse_phone_answer(
            &PhonePendingKind::SendCode {
                generation: prepared.generation,
            },
            Some(&bytes),
            None,
        ));
        assert_eq!((error.code(), error.message()), (500, "Receive invalid response"));
    }

    #[test]
    fn firebase_sent_type_falls_back_to_sms_without_platform_defines() {
        let mut manager = PhoneNumberManager::new();
        let settings = default_settings();
        let prepared = manager
            .prepare_set_phone_number("+42".to_string(), Some(&settings), Some(&change_type()))
            .unwrap();
        let sent_code = telegram_api::auth_sentCode {
            flags: 0,
            // The safety-net firebase type (the TD_ANDROID-less build).
            r#type: Some(telegram_api::auth_SentCodeType::auth_sentCodeTypeFirebaseSms(
                telegram_api::auth_sentCodeTypeFirebaseSms {
                    flags: 1,
                    nonce: b"nonce".to_vec(),
                    play_integrity_project_id: 7,
                    play_integrity_nonce: Vec::new(),
                    receipt: String::new(),
                    push_timeout: 0,
                    length: 6,
                },
            )),
            phone_code_hash: "hash".to_string(),
            next_type: None,
            timeout: 10,
        };
        manager
            .on_send_code_result(prepared.generation, sent_code)
            .unwrap();
        // The sent type maps to Sms (the C++ no-define fall-through).
        assert_eq!(manager.send_code_helper.sent_code_info.ty, CodeType::Sms);
        assert_eq!(manager.send_code_helper.sent_code_info.length, 6);
    }

    #[test]
    fn check_code_rides_the_flow_and_resets_the_state() {
        let mut manager = PhoneNumberManager::new();
        let settings = default_settings();
        let prepared = manager
            .prepare_set_phone_number("+42".to_string(), Some(&settings), Some(&change_type()))
            .unwrap();
        let sent_code = telegram_api::auth_sentCode {
            flags: 0,
            r#type: Some(telegram_api::auth_SentCodeType::auth_sentCodeTypeSms(
                telegram_api::auth_sentCodeTypeSms { length: 5 },
            )),
            phone_code_hash: "hash".to_string(),
            next_type: None,
            timeout: 10,
        };
        manager
            .on_send_code_result(prepared.generation, sent_code)
            .unwrap();

        let prepared = manager.prepare_check_code("12345".to_string()).unwrap();
        assert_eq!(prepared.check, PhoneCheckQuery::ChangePhone);
        let telegram_api::Function::account_changePhone(query) = &prepared.function else {
            panic!("wrong query");
        };
        assert_eq!(query.phone_code_hash, "hash");
        assert_eq!(query.phone_code, "12345");
        assert_eq!(prepared.generation, manager.generation);

        manager.on_check_code_result(prepared.generation).unwrap();
        assert_eq!(manager.state, PhoneState::Ok);
        // The generation moved — a stale check-code answer is canceled.
        let error = err_of(manager.on_check_code_result(prepared.generation));
        assert_eq!((error.code(), error.message()), (500, "Request was canceled"));
    }

    #[test]
    fn wait_code_bands_gate_firebase_report_and_resend() {
        let mut manager = PhoneNumberManager::new();
        let reason = td_api::ResendCodeReason::resendCodeReasonUserRequest(
            td_api::resendCodeReasonUserRequest,
        );

        let error = manager.prepare_send_firebase_sms("token").unwrap_err();
        assert_eq!((error.code(), error.message()), (400, "Can't send Firebase SMS"));
        let error = manager
            .prepare_report_missing_code("250")
            .unwrap_err();
        assert_eq!((error.code(), error.message()), (400, "Can't report missing code"));
        let error = manager.prepare_resend_code(Some(&reason)).unwrap_err();
        assert_eq!((error.code(), error.message()), (400, "Can't resend code"));
        let error = manager.prepare_check_code("12345".to_string()).unwrap_err();
        assert_eq!((error.code(), error.message()), (400, "Can't check code"));

        // Enter WaitCode.
        let settings = default_settings();
        let prepared = manager
            .prepare_set_phone_number("+42".to_string(), Some(&settings), Some(&change_type()))
            .unwrap();
        let sent_code = telegram_api::auth_sentCode {
            flags: 0,
            r#type: Some(telegram_api::auth_SentCodeType::auth_sentCodeTypeSms(
                telegram_api::auth_sentCodeTypeSms { length: 5 },
            )),
            phone_code_hash: "hash".to_string(),
            next_type: None,
            timeout: 10,
        };
        manager
            .on_send_code_result(prepared.generation, sent_code)
            .unwrap();

        // With no next type, the resend takes the helper band.
        let error = manager.prepare_resend_code(Some(&reason)).unwrap_err();
        assert_eq!(
            (error.code(), error.message()),
            (400, "Authentication code can't be resend")
        );

        // The firebase/report queries build with the helper state.
        let function = manager.prepare_send_firebase_sms("token").unwrap();
        let telegram_api::Function::auth_requestFirebaseSms(query) = function else {
            panic!("wrong query");
        };
        assert_eq!((query.flags, query.phone_number.as_str()), (0, "+42"));
        let function = manager.prepare_report_missing_code("250").unwrap();
        let telegram_api::Function::auth_reportMissingCode(query) = function else {
            panic!("wrong query");
        };
        assert_eq!(query.mnc, "250");
    }

    #[test]
    fn resend_code_reason_flag_matches_the_cpp() {
        let mut manager = PhoneNumberManager::new();
        let settings = default_settings();
        let prepared = manager
            .prepare_set_phone_number("+42".to_string(), Some(&settings), Some(&change_type()))
            .unwrap();
        let sent_code = telegram_api::auth_sentCode {
            flags: 0,
            r#type: Some(telegram_api::auth_SentCodeType::auth_sentCodeTypeSms(
                telegram_api::auth_sentCodeTypeSms { length: 5 },
            )),
            phone_code_hash: "hash".to_string(),
            next_type: Some(telegram_api::auth_CodeType::auth_codeTypeSms(
                telegram_api::auth_codeTypeSms,
            )),
            timeout: 10,
        };
        manager
            .on_send_code_result(prepared.generation, sent_code)
            .unwrap();

        // The verification-failed reason rides REASON_MASK; the user
        // request leaves the flags empty.
        let failed = td_api::ResendCodeReason::resendCodeReasonVerificationFailed(
            td_api::resendCodeReasonVerificationFailed {
                error_message: "FACE_NOT_RECOGNIZED".to_string(),
            },
        );
        let prepared = manager.prepare_resend_code(Some(&failed)).unwrap();
        let telegram_api::Function::auth_resendCode(query) = &prepared.function else {
            panic!("wrong query");
        };
        assert_eq!(query.flags & AUTH_RESEND_CODE_REASON_MASK, AUTH_RESEND_CODE_REASON_MASK);
        assert_eq!(query.reason, "FACE_NOT_RECOGNIZED");
        assert_eq!(query.phone_code_hash, "hash");
        assert_eq!(manager.state, PhoneState::WaitCode);

        let user_request = td_api::ResendCodeReason::resendCodeReasonUserRequest(
            td_api::resendCodeReasonUserRequest,
        );
        let prepared = manager.prepare_resend_code(Some(&user_request)).unwrap();
        let telegram_api::Function::auth_resendCode(query) = &prepared.function else {
            panic!("wrong query");
        };
        assert_eq!(query.flags, 0);
    }

    #[test]
    fn input_code_settings_maps_the_flags_and_logout_tokens() {
        let mut settings = default_settings();
        settings.allow_flash_call = true;
        settings.is_current_phone_number = true;
        settings.allow_missed_call = true;
        settings.has_unknown_phone_number = true;
        settings.allow_sms_retriever_api = true;
        settings.firebase_authentication_settings = Some(
            td_api::FirebaseAuthenticationSettings::firebaseAuthenticationSettingsIos(
                td_api::firebaseAuthenticationSettingsIos {
                    device_token: "dt".to_string(),
                    is_app_sandbox: true,
                },
            ),
        );
        settings.authentication_tokens = vec![
            // One valid token plus junk (the invalid entries are skipped).
            "YWJjZGVmZ2hpamtsbW5vcA".to_string(),
            "not base64!".to_string(),
        ];
        let code_settings = get_input_code_settings(Some(&settings));
        assert_eq!(
            code_settings.flags,
            CODE_SETTINGS_TOKEN_MASK | CODE_SETTINGS_LOGOUT_TOKENS_MASK
        );
        assert!(code_settings.allow_flashcall);
        assert!(code_settings.current_number);
        assert!(code_settings.allow_app_hash);
        assert!(code_settings.allow_missed_call);
        assert!(code_settings.allow_firebase);
        assert!(code_settings.unknown_number);
        assert_eq!(code_settings.token, "dt");
        assert!(code_settings.app_sandbox);
        assert_eq!(code_settings.logout_tokens, vec![b"abcdefghijklmnop".to_vec()]);
    }

    #[test]
    fn base64url_decode_rejects_the_bad_shapes() {
        assert_eq!(base64url_decode("").unwrap(), Vec::<u8>::new());
        assert_eq!(base64url_decode("YQ").unwrap(), b"a".to_vec());
        assert_eq!(base64url_decode("YWI=").unwrap(), b"ab".to_vec());
        assert_eq!(base64url_decode("YWJj").unwrap(), b"abc".to_vec());
        assert!(base64url_decode("Y").is_err());
        assert!(base64url_decode("YWJjZA==").is_err()); // padding >= 3 minus one: two pads is fine, three is not
        assert!(base64url_decode("A").is_err());
    }

    #[test]
    fn wire_errors_pass_through_with_the_server_code() {
        let error = err_of(parse_phone_answer(
            &PhonePendingKind::FirebaseSms,
            None,
            Some(&Status::error(429, "PHONE_NUMBER_BANNED")),
        ));
        assert_eq!((error.code(), error.message()), (429, "PHONE_NUMBER_BANNED"));

        let error = parse_phone_answer(&PhonePendingKind::ReportMissingCode, None, None)
            .unwrap_err();
        assert_eq!((error.code(), error.message()), (500, "the answer is missing"));
    }
}
