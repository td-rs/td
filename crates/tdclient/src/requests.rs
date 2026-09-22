//! Port of `td/telegram/Requests.{h,cpp}` (Track I): the `td_api` request
//! dispatch. The C++ file is a ~9.5K-line per-function `on_request` overload
//! set that hands each request to its manager through a `RequestActor`; the
//! Rust dispatcher lives in one `run_request` match and grows with every
//! manager step (the M.11 flip protocol,
//! `RUST_REWRITE_PLAN_REQUESTS_M11.md`: arms + census fixture + corpus
//! coverage land per slice, never the dispatcher internals).
//!
//! M.1 carried the handlers that need no network or manager actors: the
//! `test*` echo set, `getCurrentState`, `getAuthorizationState`, the
//! `close`/`destroy` pair, and the `Run`-state `setTdlibParameters`
//! rejection. Everything not yet ported answers the interim 500
//! "TDLib function is not implemented yet".
//!
//! M.11.1 dispatcher audit (2026-09-17, vs the local C++ 1.8.67+ tree):
//! byte-parity re-verified for the pre-existing core arms — the `test*`
//! family (Requests.cpp:9483-9517; `testUseUpdate`'s `send_result(nullptr)`
//! is the 404 "Not Found" `Td::send_result` rewrite, Td.cpp:1407-1409),
//! `setTdlibParameters` (2044-2048), `close`/`destroy` (2160-2175, the ok
//! answers before the close hop), `getAuthorizationState` (2051-2053 vs
//! `AuthManager::get_state`, AuthManager.cpp:450-456 — the `State::None`
//! park is unobservable: the fake-state path answers before `Run`, and the
//! init pmc restore pins a concrete state in `Run`), `getCurrentState`
//! (2192-2209: options → authorization → user → chat; the connection and
//! authorized-manager blocks accumulate with their managers),
//! `getOption`/`setOption` (8076-8086: `CLEAN_INPUT_STRING` first, then the
//! OptionManager contract ported with M.3), the `CHECK_IS_USER`/
//! `CHECK_IS_BOT` texts (1993-2000), and the classification lists
//! `is_authentication_request`/`is_preinitialization_request`/
//! `is_preauthentication_request` (Td.cpp:154-249, 26/12/37 entries) behind
//! the `Td::run_request` state gate (Td.cpp:299-374).

use crate::call_id::{CallId, InputCallId};
use crate::call_protocol::CallProtocol;
use crate::chat_manager::{ChannelId, DialogId};
use crate::community_id::CommunityId;
use crate::contact::Contact;
use crate::notification::{NotificationGroupId, NotificationId, NotificationObjectId};
use crate::notification_settings::{
    ReactionNotificationSettings, get_notification_settings_scope,
    get_scope_notification_settings_object,
};
use crate::notification_settings_manager::ScopeSettingsAnswer;
use crate::star_gift_manager::{
    StarGiftCollectionId, StarGiftId, StarGiftResalePrice, get_message_sender_dialog_id,
};
use crate::suggested_action::SuggestedAction;
use crate::td::AccountEnv;
use crate::td::AutosaveEnv;
use crate::td::BoostEnv;
use crate::td::CallEnv;
use crate::td::CommunityEnv;
use crate::td::NotificationEnv;
use crate::td::PremiumTdEnv;
use crate::td::QuickReplyEnv;
use crate::td::StatisticsEnv;
use crate::td::Td;
use crate::td::ThemeEnv;
use crate::td::WebBrowserEnv;
use crate::td_api::{self, Function, Object};
use crate::user_manager::{FileId, PendingUserRequest, SecretChatId, UserEnv, UserId};
use tdutils::promise::Promise;
use tdutils::status::{Result, Status};

/// The interim message of the S70 skeleton, kept for the not-yet-ported
/// handlers until the Waves complete.
const NOT_IMPLEMENTED_YET: &str = "TDLib function is not implemented yet";

/// The `UserEnv` for the manager calls. Expanded inline (a helper would
/// freeze all of `td` behind one opaque borrow, breaking the field-disjoint
/// `td.user_manager.x(&mut env)` call shape); every user request runs after
/// `init`, so the option manager exists.
/// The `SecureEnv` for the SecureManager calls — expanded inline like
/// `user_env!` to keep the field-disjoint borrows (the M.12 file plane
/// rides its `Arc` clone).
macro_rules! secure_env {
    ($td:expr) => {
        crate::td::SecureEnv {
            global: &mut $td.global,
            options: $td
                .option_manager
                .as_mut()
                .expect("passport requests run with the initialized option manager"),
            auth_is_bot: $td.auth_manager.is_bot(),
            user_manager: &mut $td.user_manager,
            file_manager: std::sync::Arc::clone(&$td.file_manager),
            file_load_manager: std::sync::Arc::clone(&$td.file_load_manager),
            close_flag: $td.close_flag != 0,
        }
    };
}

macro_rules! user_env {
    ($td:expr) => {
        UserEnv {
            global: &mut $td.global,
            options: $td
                .option_manager
                .as_mut()
                .expect("user requests run with the initialized option manager"),
            auth_is_bot: $td.auth_manager.is_bot(),
        }
    };
}

/// `Requests::run_request` — the C++ `downcast_call` dispatch.
pub(crate) fn run_request(td: &mut Td, id: u64, function: Function) {
    match function {
        // --- test (Requests.cpp:9484-9521) ---
        Function::testUseUpdate(_) => {
            // `send_result(id, nullptr)` — the C++ null result becomes the
            // 404 in `Td::send_result`.
            td.send_result(
                id,
                Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_string(),
                }),
            );
        }
        Function::testCallEmpty(_) => td.send_result(id, ok_object()),
        Function::testSquareInt(request) => td.send_result(
            id,
            Object::testInt(td_api::TestInt {
                value: request.x * request.x,
            }),
        ),
        Function::testCallString(request) => td.send_result(
            id,
            Object::testString(td_api::TestString { value: request.x }),
        ),
        Function::testCallBytes(request) => td.send_result(
            id,
            Object::testBytes(td_api::TestBytes { value: request.x }),
        ),
        Function::testCallVectorInt(request) => td.send_result(
            id,
            Object::testVectorInt(td_api::TestVectorInt { value: request.x }),
        ),
        Function::testCallVectorIntObject(request) => td.send_result(
            id,
            Object::testVectorIntObject(td_api::TestVectorIntObject { value: request.x }),
        ),
        Function::testCallVectorString(request) => td.send_result(
            id,
            Object::testVectorString(td_api::TestVectorString { value: request.x }),
        ),
        Function::testCallVectorStringObject(request) => td.send_result(
            id,
            Object::testVectorStringObject(td_api::TestVectorStringObject { value: request.x }),
        ),

        // --- the instance controls (Requests.cpp:2044-2190) ---
        Function::setTdlibParameters(_) => {
            td.send_error_impl(id, 400, "Unexpected setTdlibParameters");
        }
        Function::close(_) => {
            // send response before actually closing
            td.send_result(id, ok_object());
            td.close_impl(false);
        }
        Function::destroy(_) => {
            // send response before actually destroying
            td.send_result(id, ok_object());
            td.close_impl(true);
        }

        // --- state (Requests.cpp:2192-) ---
        Function::getAuthorizationState(_) => {
            // `AuthManager::get_state` — the bare state; `State::None`
            // queues the query in the C++, but the init pmc restore pins a
            // concrete state before Run, so None cannot be observed here.
            if let Some(authorization_state) =
                td.auth_manager.get_current_authorization_state_object()
            {
                td.send_result(
                    id,
                    crate::auth_manager::authorization_state_to_object(authorization_state),
                );
            } else {
                td.send_result(id, ok_object());
            }
        }
        Function::getCurrentState(_) => {
            // The blocks accumulate `Update`s directly: routing them through
            // `Object` and the generated `Update::try_from_object` match
            // moves the 32 KB `Object` enum per item and exhausts the
            // thread stack in debug builds.
            let mut updates: Vec<td_api::Update> = Vec::new();
            let unix_time = td.get_unix_time();
            if let Some(option_manager) = td.option_manager.as_mut() {
                let mut option_updates = Vec::new();
                option_manager.get_current_state(&mut option_updates, unix_time);
                for update in option_updates {
                    match update {
                        Object::updateOption(u) => updates.push(td_api::Update::updateOption(u)),
                        _ => unreachable!("the option manager emits updateOption only"),
                    }
                }
            }

            if let Some(authorization_state) =
                td.auth_manager.get_current_authorization_state_object()
            {
                updates.push(td_api::Update::updateAuthorizationState(
                    td_api::updateAuthorizationState {
                        authorization_state: Some(authorization_state),
                    },
                ));
            }

            // UserManager::get_current_state (UserManager.cpp:10533-10568;
            // the Requests.cpp order is options → authorization →
            // connection → community → user → chat → …)
            {
                let mut env = user_env!(td);
                td.user_manager.get_current_state(&mut env, &mut updates);
            }

            // the ChatManager state block (M.5.47): unknown placeholders,
            // then channels before chats, then fulls — see
            // ChatManager::get_current_state (Requests.cpp:2209, after the
            // user block)
            if let Some(options) = td.option_manager.as_mut() {
                let mut env = crate::user_manager::UserEnv {
                    global: &mut td.global,
                    options,
                    auth_is_bot: td.auth_manager.is_bot(),
                };
                let mut chat_updates = Vec::new();
                td.chat_manager.get_current_state(
                    &mut td.user_manager,
                    &mut env,
                    &mut chat_updates,
                );
                updates.extend(chat_updates);
            }

            // NotificationSettingsManager::get_current_state
            // (Requests.cpp:2221 — before the notification block; the
            // manager's own is_bot gate applies)
            if td.auth_manager.is_authorized() {
                let env = NotificationEnv {
                    global: &mut td.global,
                    options: td.option_manager.as_mut(),
                    auth_is_authorized: true,
                    auth_is_bot: td.auth_manager.is_bot(),
                    updates: &mut td.notification_updates,
                };
                td.notification_settings_manager
                    .get_current_state(&env, &mut updates);
            }

            // NotificationManager::get_current_state (Requests.cpp:2229 —
            // deep in the authorized block, after user/chat)
            if td.auth_manager.is_authorized() && !td.auth_manager.is_bot() {
                let env = NotificationEnv {
                    global: &mut td.global,
                    options: td.option_manager.as_mut(),
                    auth_is_authorized: true,
                    auth_is_bot: false,
                    updates: &mut td.notification_updates,
                };
                td.notification_manager
                    .get_current_state(&env, &mut updates);
            }

            // WebBrowserManager::get_current_state (Requests.cpp:2259; the
            // manager's own is_authorized/is_bot gate applies)
            {
                let env = WebBrowserEnv {
                    global: &mut td.global,
                    auth_is_authorized: td.auth_manager.is_authorized(),
                    auth_is_bot: td.auth_manager.is_bot(),
                    updates: &mut td.web_browser_updates,
                };
                td.web_browser_manager.get_current_state(&env, &mut updates);
            }

            // SuggestedActionManager::get_current_state
            // (Requests.cpp:2237 — after notification)
            {
                let env = AccountEnv {
                    global: &mut td.global,
                    options: td.option_manager.as_mut(),
                    user_manager: &mut td.user_manager,
                    device_token_manager: &mut td.device_token_manager,
                    auth_is_bot: td.auth_manager.is_bot(),
                    updates: &mut td.account_updates,
                };
                td.suggested_action_manager
                    .get_current_state(&env, &mut updates);
            }

            // ConnectionStateManager and the remaining authorized-manager
            // state blocks (connection/chat/background/...) accumulate here
            // as their managers land.
            td.send_result(id, Object::updates(td_api::Updates { updates }));
        }

        // --- options (Requests.cpp:8076-8090) ---
        Function::getOption(request) => {
            let Some(name) = crate::misc::clean_input_string(&request.name) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            // `OptionManager::get_option` (cpp:758-800): the ConfigManager
            // branch for the content-settings names (is_bot is false until
            // the AuthManager net step).
            if !(td.auth_manager.is_authorized() && td.auth_manager.is_bot())
                && (name == "can_ignore_sensitive_content_restrictions"
                    || name == "ignore_sensitive_content_restrictions")
            {
                return td.get_option_content_settings(id, name);
            }
            #[allow(clippy::large_enum_variant)] // function-local control flow
            enum Action {
                Queued,
                Answer(Object),
            }
            let Some(option_manager) = td.option_manager.as_mut() else {
                // Unreachable through the request gate: getOption is a
                // preauthentication request and queues until init creates
                // the manager.
                return td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
            };
            let unix_time = td.global.unix_time();
            let action = if option_manager.is_td_inited() {
                Action::Answer(option_value_to_object(
                    option_manager.get_option_value(&name, unix_time),
                ))
            } else {
                option_manager.queue_get_option(id, name);
                Action::Queued
            };
            if let Action::Answer(value) = action {
                td.send_result(id, value);
            }
        }
        Function::setOption(request) => {
            let Some(name) = crate::misc::clean_input_string(&request.name) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(option_manager) = td.option_manager.as_mut() else {
                return td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
            };
            let Some(value) = request.value.as_ref() else {
                return td.send_error_impl(id, 400, "Expected to receive \"value\"");
            };
            match option_manager.set_option_from_api(&name, value) {
                Ok(crate::option_manager::SetOptionAction::Done) => {
                    td.flush_option_updates();
                    td.send_result(id, ok_object());
                }
                // `OptionManager::set_option` (cpp:969-983): the setOption
                // answers when the account.setContentSettings query
                // completes.
                Ok(crate::option_manager::SetOptionAction::SetContentSettings { value }) => {
                    td.set_content_settings_option(id, value);
                }
                Err(error) => td.send_error_impl(id, 400, &error),
            }
        }
        Function::getApplicationConfig(_) => {
            // `CHECK_IS_USER` — bots are rejected; is_bot is false until
            // the AuthManager net step, so the request reaches the manager.
            td.get_application_config(id);
        }

        // -- the notification set (M.13.8): the manager-backed request
        //    family (Requests.cpp:4123/4131/2403) --
        Function::removeNotification(request) => {
            // `Requests.cpp:4123`: is_permanent = false, force_update =
            // true
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let mut env = NotificationEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.notification_updates,
            };
            td.notification_manager.remove_notification(
                &mut env,
                NotificationGroupId::new(request.notification_group_id),
                NotificationId::new(request.notification_id),
                false,
                true,
                promise,
                "td_api::removeNotification",
            );
        }
        Function::removeNotificationGroup(request) => {
            // `Requests.cpp:4131`: the request exposes no max object id —
            // the C++ passes an invalid one; force_update = true
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let mut env = NotificationEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.notification_updates,
            };
            td.notification_manager.remove_notification_group(
                &mut env,
                NotificationGroupId::new(request.notification_group_id),
                NotificationId::new(request.max_notification_id),
                NotificationObjectId::default(),
                -1,
                true,
                promise,
            );
        }
        Function::processPushNotification(request) => {
            // `Requests.cpp:2403`: CHECK_IS_USER, the payload string is
            // cleaned, then NotificationManager::process_push_notification
            // parses the push payload and synthesizes the message — the
            // pipeline needs MessagesManager/MessageDb and the secret-chat
            // decryption ([blocked: M.13.6b, M.6/M.10]); the C++ answers ok
            // only after processing.
            if !check_is_user(td, id) {
                return;
            }
            let _ = crate::misc::clean_input_string(&request.payload);
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }

        // -- the notification settings set (M.13S.4): Requests.cpp:7824/
        //    7968/7977/7983 + the DeviceTokenManager arm at 2411 --
        Function::getScopeNotificationSettings(request) => {
            // `Requests.cpp:7824-7830`: CHECK_IS_USER + the null-scope gate
            if !check_is_user(td, id) {
                return;
            }
            let Some(scope_object) = request.scope else {
                return td.send_error_impl(id, 400, "Scope must be non-empty");
            };
            let scope = get_notification_settings_scope(&scope_object);
            // `GetScopeNotificationSettingsRequest` (Requests.cpp:1340-1364):
            // the synchronized settings answer the object, an unsynchronized
            // scope defers into the (net-blocked) fetch and answers nothing
            let unix_time = td.get_unix_time();
            let answer = {
                let env = NotificationEnv {
                    global: &mut td.global,
                    options: td.option_manager.as_mut(),
                    auth_is_authorized: td.auth_manager.is_authorized(),
                    auth_is_bot: td.auth_manager.is_bot(),
                    updates: &mut td.notification_updates,
                };
                match td
                    .notification_settings_manager
                    .get_scope_notification_settings_for_request(&env, scope)
                {
                    ScopeSettingsAnswer::Ready(settings) => {
                        Some(get_scope_notification_settings_object(settings, unix_time))
                    }
                    ScopeSettingsAnswer::Deferred => None,
                }
            };
            if let Some(object) = answer {
                td.send_result(id, Object::scopeNotificationSettings(object));
            }
        }
        Function::setScopeNotificationSettings(request) => {
            // `Requests.cpp:7968-7975`: CHECK_IS_USER + the null-scope gate;
            // the answer is ok even before the server acks (the C++ answers
            // via answer_ok_query with the Status converted by the macro)
            if !check_is_user(td, id) {
                return;
            }
            let Some(scope_object) = request.scope else {
                return td.send_error_impl(id, 400, "Scope must be non-empty");
            };
            let scope = get_notification_settings_scope(&scope_object);
            let result = {
                let mut env = NotificationEnv {
                    global: &mut td.global,
                    options: td.option_manager.as_mut(),
                    auth_is_authorized: td.auth_manager.is_authorized(),
                    auth_is_bot: td.auth_manager.is_bot(),
                    updates: &mut td.notification_updates,
                };
                td.notification_settings_manager
                    .set_scope_notification_settings(&mut env, scope, request.notification_settings)
            };
            match result {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::setReactionNotificationSettings(request) => {
            // `Requests.cpp:7977-7981`
            if !check_is_user(td, id) {
                return;
            }
            let settings = ReactionNotificationSettings::from_td_api(request.notification_settings);
            let result = {
                let mut env = NotificationEnv {
                    global: &mut td.global,
                    options: td.option_manager.as_mut(),
                    auth_is_authorized: td.auth_manager.is_authorized(),
                    auth_is_bot: td.auth_manager.is_bot(),
                    updates: &mut td.notification_updates,
                };
                td.notification_settings_manager
                    .set_reaction_notification_settings(&mut env, settings)
            };
            match result {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::resetAllNotificationSettings(_) => {
            // `Requests.cpp:7983-7987`: the local reset answers ok without
            // waiting for the (net-blocked) ResetNotifySettingsQuery
            if !check_is_user(td, id) {
                return;
            }
            {
                let mut env = NotificationEnv {
                    global: &mut td.global,
                    options: td.option_manager.as_mut(),
                    auth_is_authorized: td.auth_manager.is_authorized(),
                    auth_is_bot: td.auth_manager.is_bot(),
                    updates: &mut td.notification_updates,
                };
                td.notification_settings_manager
                    .reset_all_notification_settings(&mut env);
            }
            td.send_result(id, ok_object());
        }
        Function::registerDevice(request) => {
            // `Requests.cpp:2411-2416`: CHECK_IS_USER, then the
            // DeviceTokenManager takes the promise; the register ack is
            // the [blocked: net] tail and the request stays unanswered
            if !check_is_user(td, id) {
                return;
            }
            let promise = td
                .deferred_promise::<td_api::PushReceiverId, _>(id, |push_receiver_id| {
                    Object::pushReceiverId(push_receiver_id)
                });
            {
                let mut env = NotificationEnv {
                    global: &mut td.global,
                    options: td.option_manager.as_mut(),
                    auth_is_authorized: td.auth_manager.is_authorized(),
                    auth_is_bot: td.auth_manager.is_bot(),
                    updates: &mut td.notification_updates,
                };
                td.device_token_manager.register_device(
                    &mut env,
                    request.device_token,
                    &request.other_user_ids,
                    promise,
                );
            }
        }

        // -- the auth set (S76): the real `AuthManager` handlers over the
        //    walking-skeleton bridge. logOut and the password-recovery
        //    helpers keep their interim bodies until their managers land. --
        Function::setAuthenticationPhoneNumber(request) => {
            td.set_authentication_phone_number(id, request.phone_number);
        }
        Function::checkAuthenticationCode(request) => {
            td.check_authentication_code(id, request.code);
        }
        Function::checkAuthenticationPassword(request) => {
            td.check_authentication_password(id, request.password);
        }
        Function::checkAuthenticationBotToken(request) => {
            td.check_authentication_bot_token(id, request.token);
        }
        Function::logOut(_) => {
            // `AuthManager::log_out` — the local guards need the sender for
            // the query part; both arrive with the net step.
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }

        // -- the walking-skeleton messaging set (S76) --
        Function::getMe(_) => td.get_me(id),
        Function::sendMessage(request) => td.on_send_message(id, request),
        Function::setDatabaseEncryptionKey(_) => {
            // `G()->td_db()->get_binlog()->change_key(...)` — the binlog
            // re-key lands with the TdDb binlog-close wiring.
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }

        // --- the M.11.4 generic (manager-free) family ---
        Function::setAlarm(request) => {
            // Requests.cpp:8976-8979 → `AlarmManager::set_alarm`
            // (AlarmManager.cpp:52-60): the ok answer parks until the
            // deadline fires from the shared timeout slot.
            td.set_alarm(id, request.seconds);
        }

        // The M.4.16/M.6.20 manager-arm groups live in dedicated
        // `#[inline(never)]` functions: the merged dispatcher's single
        // match frame pushed the scheduler-thread call chain over its
        // 2 MiB stack in debug builds (the M.6 integration gate); the
        // group frames unwind between groups instead of nesting.
        function => run_user_requests(td, id, function),
    }
}

/// The M.4.16 UserManager arm group of the request dispatcher — see the
/// frame note at the forward site in [`run_request`]. Every arm fully
/// answers the request; `return;` exits after answering.
#[inline(never)]
fn run_user_requests(td: &mut Td, id: u64, function: Function) {
    match function {
        // ------------------------------------------------------------------
        // M.4.16: the UserManager slice (the `Requests.cpp` on_request
        // overloads routing to `user_manager_`). The C++ request actors'
        // local halves run here; every network tail stays [blocked: net]
        // and answers nothing until the net step lands (the M.4 line
        // predates the M.3.8 NetQueryDispatcher).
        // ------------------------------------------------------------------

        // --- the read surface ---
        Function::getUser(request) => {
            // GetUserRequest (Requests.cpp:217-233): set_tries(3)
            let user_id = UserId::new(request.user_id);
            let loaded = {
                let mut env = user_env!(td);
                td.user_manager.load_user(&mut env, user_id, 3)
            };
            match loaded {
                Ok(true) => {
                    let user = {
                        let env = user_env!(td);
                        td.user_manager.get_user_object_of(&env, user_id)
                    };
                    td.send_result(id, Object::user(user));
                }
                Ok(false) => {} // the merged users.getUsers query — [blocked: net]
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::getUserFullInfo(request) => {
            // GetUserFullInfoRequest (Requests.cpp:235-249): the default
            // two tries run the first pass with force = get_tries() < 2 =
            // false; the db row load is synchronous (the collapse)
            let user_id = UserId::new(request.user_id);
            let loaded = {
                let mut env = user_env!(td);
                td.user_manager
                    .load_user_full(&mut env, user_id, false, "GetUserFullInfoRequest")
            };
            match loaded {
                Ok(true) => {
                    let user_full_info = {
                        let env = user_env!(td);
                        td.user_manager.get_user_full_info_object_of(&env, user_id)
                    };
                    td.send_result(id, Object::userFullInfo(user_full_info));
                }
                Ok(false) => {} // the users.getFullUser query — [blocked: net]
                Err(message) => td.send_error_impl(id, 400, message),
            }
        }
        Function::getSecretChat(request) => {
            // GetSecretChatRequest (Requests.cpp:323-338): the default two
            // tries run get_secret_chat(force = false) first; the db load
            // is synchronous here, so the collapse force-loads once and
            // answers the C++ "Secret chat not found" on the miss
            let secret_chat_id = SecretChatId::new(request.secret_chat_id);
            if !secret_chat_id.is_valid() {
                return td.send_error_impl(id, 400, "Invalid secret chat identifier");
            }
            let found = {
                let mut env = user_env!(td);
                td.user_manager.get_secret_chat_force(
                    &mut env,
                    secret_chat_id,
                    "GetSecretChatRequest",
                )
            };
            if !found {
                return td.send_error_impl(id, 400, "Secret chat not found");
            }
            let object = {
                let mut env = user_env!(td);
                td.user_manager
                    .get_secret_chat_object_for_request(&mut env, secret_chat_id)
            };
            match object {
                Some(secret_chat) => td.send_result(id, Object::secretChat(secret_chat)),
                None => td.send_result(
                    id,
                    Object::error(td_api::Error {
                        code: 404,
                        message: "Not Found".to_string(),
                    }),
                ),
            }
        }
        Function::searchUserByPhoneNumber(request) => {
            // SearchUserByPhoneNumberRequest (Requests.cpp:364-390): the
            // resolved cache answers locally; the ResolvePhoneQuery tail is
            // [blocked: net]. The completed future with no user record
            // answers the `send_result(nullptr)` 404.
            if !check_is_user(td, id) {
                return;
            }
            let Some(phone_number) = crate::misc::clean_input_string(&request.phone_number) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let resolved = td
                .user_manager
                .search_user_by_phone_number_for_request(phone_number, request.only_local);
            match resolved {
                Err(status) => send_error_status(td, id, status),
                Ok(None) => {} // the resolve query — [blocked: net]
                Ok(Some(user_id)) => {
                    let have = td.user_manager.get_user(user_id).is_some();
                    if have {
                        let user = {
                            let env = user_env!(td);
                            td.user_manager.get_user_object_of(&env, user_id)
                        };
                        td.send_result(id, Object::user(user));
                    } else {
                        // `get_user_object(nullptr)` → the 404 in `Td::send_result`
                        td.send_result(
                            id,
                            Object::error(td_api::Error {
                                code: 404,
                                message: "Not Found".to_string(),
                            }),
                        );
                    }
                }
            }
        }

        // --- the contacts family ---
        Function::getContacts(_) => {
            // Requests.cpp:6631-6634: `SearchContactsRequest(string(), 1000000)`
            search_contacts_request(td, id, String::new(), 1_000_000);
        }
        Function::searchContacts(request) => {
            // Requests.cpp:6636-6641
            if !check_is_user(td, id) {
                return;
            }
            let Some(query) = crate::misc::clean_input_string(&request.query) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            search_contacts_request(td, id, query, request.limit);
        }
        Function::addContact(request) => {
            // Requests.cpp:6611-6620 + `get_contact` (Contact.cpp:142-156):
            // the importedContact variant has no vcard and carries the
            // MessageEntity note (the M.4.14 never-stored divergence)
            if !check_is_user(td, id) {
                return;
            }
            let user_id = UserId::new(request.user_id);
            let Some(imported_contact) = request.contact else {
                return td.send_error_impl(id, 400, "Imported contact is required");
            };
            let mut contact = Contact::new(
                imported_contact.phone_number,
                imported_contact.first_name,
                imported_contact.last_name,
                String::new(),
                UserId::default(),
            );
            if let Err(error) = contact.validate() {
                return send_error_status(td, id, error);
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            {
                let mut env = user_env!(td);
                td.user_manager.add_contact(
                    &mut env,
                    user_id,
                    contact,
                    request.share_phone_number,
                    promise,
                );
            }
        }
        Function::importContacts(request) => {
            // ImportContactsRequest (Requests.cpp:1192-1215, tries 3 =
            // load_contacts + import_contacts)
            if !check_is_user(td, id) {
                return;
            }
            let contacts = match get_contacts(&request.contacts) {
                Ok(contacts) => contacts,
                Err(error) => return send_error_status(td, id, error),
            };
            let promise: Promise<Object> = td.deferred_promise(id, |object| object);
            {
                let mut env = user_env!(td);
                td.user_manager
                    .park_pending_user_request(PendingUserRequest::ImportContacts {
                        contacts,
                        random_id: 0,
                        promise,
                    });
                if !td.user_manager.are_contacts_loaded() {
                    td.user_manager.load_contacts(&mut env, Promise::default());
                } else {
                    td.user_manager.drain_pending_user_requests(&mut env);
                }
            }
        }
        Function::changeImportedContacts(request) => {
            // ChangeImportedContactsRequest (Requests.cpp:1251-1268, tries 4)
            if !check_is_user(td, id) {
                return;
            }
            let contacts = match get_contacts(&request.contacts) {
                Ok(contacts) => contacts,
                Err(error) => return send_error_status(td, id, error),
            };
            let promise: Promise<Object> = td.deferred_promise(id, |object| object);
            {
                let mut env = user_env!(td);
                td.user_manager.park_pending_user_request(
                    PendingUserRequest::ChangeImportedContacts {
                        contacts,
                        random_id: 0,
                        promise,
                    },
                );
                if !td.user_manager.are_contacts_loaded() {
                    td.user_manager.load_contacts(&mut env, Promise::default());
                } else {
                    td.user_manager.drain_pending_user_requests(&mut env);
                }
            }
        }
        Function::removeContacts(request) => {
            // RemoveContactsRequest (Requests.cpp:1237-1249, tries 3)
            if !check_is_user(td, id) {
                return;
            }
            let user_ids: Vec<UserId> =
                request.user_ids.iter().map(|&id| UserId::new(id)).collect();
            let promise = td.deferred_promise(id, |()| ok_object());
            {
                let mut env = user_env!(td);
                td.user_manager.remove_contacts(&mut env, user_ids, promise);
            }
        }
        Function::getImportedContactCount(_) => {
            // Requests.cpp:6647-6651: the count rides the load-gate promise
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |count: i32| Object::count(td_api::Count { count }));
            {
                let mut env = user_env!(td);
                td.user_manager
                    .get_imported_contact_count(&mut env, promise);
            }
        }
        Function::clearImportedContacts(_) => {
            // Requests.cpp:6653-6658: the ResetContactsQuery tail is
            // [blocked: net]; the empty-store short-circuit answers here
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            {
                let mut env = user_env!(td);
                td.user_manager.clear_imported_contacts(&mut env, promise);
            }
        }
        Function::getCloseFriends(_) => {
            // GetCloseFriendsRequest (Requests.cpp:1271-1286)
            if !check_is_user(td, id) {
                return;
            }
            let promise: Promise<Object> = td.deferred_promise(id, |object| object);
            {
                let mut env = user_env!(td);
                td.user_manager
                    .park_pending_user_request(PendingUserRequest::GetCloseFriends { promise });
                if !td.user_manager.are_contacts_loaded() {
                    td.user_manager.load_contacts(&mut env, Promise::default());
                } else {
                    td.user_manager.drain_pending_user_requests(&mut env);
                }
            }
        }
        Function::setCloseFriends(request) => {
            // Requests.cpp:8143-8149: the EditCloseFriendsQuery is
            // [blocked: net]; the "User not found" gates answer here
            if !check_is_user(td, id) {
                return;
            }
            let user_ids: Vec<UserId> =
                request.user_ids.iter().map(|&id| UserId::new(id)).collect();
            let promise = td.deferred_promise(id, |()| ok_object());
            td.user_manager.set_close_friends(user_ids, promise);
        }
        Function::hideContactCloseBirthdays(_) => {
            // Requests.cpp:8150-8156: the option flip is local; the
            // contacts.getBirthdays dismissal query is [blocked: net]
            if !check_is_user(td, id) {
                return;
            }
            if let Some(option_manager) = td.option_manager.as_mut() {
                option_manager.set_option_boolean("dismiss_birthday_contact_today", true);
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            td.user_manager.hide_contact_birthdays(promise);
        }

        // --- the profile setters (the query promise completes on the
        // server ack; the [blocked: net] tails answer nothing) ---
        Function::setName(request) => {
            // Requests.cpp:6747-6753 + `set_name`: flags == 0 (the no-op
            // rename) answers immediately
            if !check_is_user(td, id) {
                return;
            }
            let Some(first_name) = crate::misc::clean_input_string(&request.first_name) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(last_name) = crate::misc::clean_input_string(&request.last_name) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let flags = td.user_manager.set_name_local(&first_name, &last_name);
            match flags {
                Ok(0) => td.send_result(id, ok_object()),
                Ok(_) => tdutils::log_info!("Update profile query — [blocked: net]"),
                Err(error) => send_error_status(td, id, error),
            }
        }
        Function::setBio(request) => {
            // Requests.cpp:6755-6760 + `set_bio`: the sanitized no-op bio
            // answers immediately
            if !check_is_user(td, id) {
                return;
            }
            let Some(bio) = crate::misc::clean_input_string(&request.bio) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let new_bio = {
                let env = user_env!(td);
                td.user_manager.set_bio_local(&env, &bio)
            };
            match new_bio {
                Ok(None) => td.send_result(id, ok_object()),
                Ok(Some(_)) => tdutils::log_info!("Update profile query — [blocked: net]"),
                Err(error) => send_error_status(td, id, error),
            }
        }
        Function::setUsername(request) => {
            // Requests.cpp:6762-6767: the UpdateUsernameQuery always sends
            if !check_is_user(td, id) {
                return;
            }
            let Some(username) = crate::misc::clean_input_string(&request.username) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            if let Err(error) = td.user_manager.set_username_local(&username) {
                return send_error_status(td, id, error);
            }
            tdutils::log_info!("Update username query — [blocked: net]");
        }
        Function::toggleUsernameIsActive(request) => {
            // Requests.cpp:6769-6775: the optimistic local apply runs
            // before the (blocked) ToggleUsernameQuery
            if !check_is_user(td, id) {
                return;
            }
            let Some(username) = crate::misc::clean_input_string(&request.username) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let toggled = {
                let mut env = user_env!(td);
                td.user_manager.toggle_username_is_active_local(
                    &mut env,
                    &username,
                    request.is_active,
                )
            };
            match toggled {
                Ok(()) => tdutils::log_info!("Toggle username query — [blocked: net]"),
                Err(error) => send_error_status(td, id, error),
            }
        }
        Function::reorderActiveUsernames(request) => {
            // Requests.cpp:6776-6783: size <= 1 answers after the local
            // gate; the ReorderUsernamesQuery is blocked
            if !check_is_user(td, id) {
                return;
            }
            let mut usernames = Vec::with_capacity(request.usernames.len());
            for username in &request.usernames {
                let Some(username) = crate::misc::clean_input_string(username) else {
                    return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
                };
                usernames.push(username);
            }
            let reordered = {
                let mut env = user_env!(td);
                td.user_manager
                    .reorder_usernames_local(&mut env, &usernames)
            };
            match reordered {
                Ok(()) if usernames.len() <= 1 => td.send_result(id, ok_object()),
                Ok(()) => tdutils::log_info!("Reorder usernames query — [blocked: net]"),
                Err(error) => send_error_status(td, id, error),
            }
        }
        Function::setBirthdate(request) => {
            // Requests.cpp:6785-6789: the UpdateBirthdayQuery carries the
            // promise; the local `on_set_birthdate` apply is its ack intake
            let _ = request;
            if !check_is_user(td, id) {
                return;
            }
            tdutils::log_info!("Update birthday query — [blocked: net]");
        }
        Function::setEmojiStatus(request) => {
            // Requests.cpp:6711-6716: the premium gate answers here; the
            // UpdateEmojiStatusQuery is [blocked: net]
            let _ = request;
            if !check_is_user(td, id) {
                return;
            }
            let premium = {
                let env = user_env!(td);
                td.user_manager.set_emoji_status_local(&env)
            };
            match premium {
                Ok(()) => tdutils::log_info!("Update emoji status query — [blocked: net]"),
                Err(error) => send_error_status(td, id, error),
            }
        }
        Function::setUserEmojiStatus(request) => {
            // Requests.cpp:6711-6716 (the bot variant): the
            // UpdateUserEmojiStatusQuery is [blocked: net]; the
            // get_input_user gate answers here
            let _ = request;
            if !check_is_bot(td, id) {
                return;
            }
            let user_id = UserId::new(request.user_id);
            let input_user = {
                let env = user_env!(td);
                td.user_manager.get_input_user(&env, user_id)
            };
            match input_user {
                Ok(_) => tdutils::log_info!("Update user emoji status query — [blocked: net]"),
                Err(error) => send_error_status(td, id, error),
            }
        }
        Function::toggleHasSponsoredMessagesEnabled(request) => {
            // Requests.cpp:6809-6813: the ToggleSponsoredMessagesQuery is
            // [blocked: net]; `on_toggle_sponsored_messages` is its ack
            let _ = request;
            if !check_is_user(td, id) {
                return;
            }
            tdutils::log_info!("Toggle sponsored messages query — [blocked: net]");
        }
        Function::setAccentColor(request) => {
            // Requests.cpp:7157-7167: the invalid-id 400 answers here; the
            // UpdateAccentColorQuery is [blocked: net]
            if !check_is_user(td, id) {
                return;
            }
            let accent_color = crate::peer_color::AccentColorId::new(request.accent_color_id);
            let custom_emoji =
                crate::emoji_status::CustomEmojiId::new(request.background_custom_emoji_id);
            let validated = td
                .user_manager
                .set_accent_color_local(accent_color, custom_emoji);
            match validated {
                Ok(_) => tdutils::log_info!("Update accent color query — [blocked: net]"),
                Err(error) => send_error_status(td, id, error),
            }
        }

        // --- the remaining photo-set handlers (the Files slice) and the
        // deferred subsystems — each arm is the flip point for its step ---
        Function::setProfilePhoto(_) => {
            // the upload path — the Files slice (M.4.12's stub keeps the
            // C++ error surface)
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }
        Function::getUserProfilePhotos(request) => {
            // Requests.cpp:7125-7130: no CHECK gate; the cache window
            // answers locally, a cache miss parks in the photos queue
            // (the GetUserQuery refetch is [blocked: net])
            let promise = td.deferred_promise(id, Object::chatPhotos);
            let mut env = user_env!(td);
            td.user_manager.get_user_profile_photos(
                &mut env,
                UserId::new(request.user_id),
                request.offset,
                request.limit,
                promise,
            );
        }
        Function::getUserProfileAudios(request) => {
            // Requests.cpp:7131-7136: the saved-music twin of
            // getUserProfilePhotos
            let promise = td.deferred_promise(id, Object::audios);
            let mut env = user_env!(td);
            td.user_manager.get_user_saved_music(
                &mut env,
                UserId::new(request.user_id),
                request.offset,
                request.limit,
                promise,
            );
        }
        Function::isProfileAudio(request) => {
            // Requests.cpp:7137-7142: answers over the loaded my-saved-
            // music list
            if !check_is_user(td, id) {
                return;
            }
            let mut promise = td.deferred_promise(id, |()| ok_object());
            td.user_manager
                .check_is_saved_music(FileId::new(request.file_id, 0), &mut promise);
        }
        Function::setProfileAudioPosition(request) => {
            // Requests.cpp:7149-7155: validation is local; the promise
            // parks at the messages.saveMusic send ([blocked: net])
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            td.user_manager.add_saved_music(
                FileId::new(request.file_id, 0),
                FileId::new(request.after_file_id, 0),
                promise,
            );
        }
        Function::removeProfileAudio(request) => {
            // Requests.cpp:7156-7162: the messages.unsaveMusic twin
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            td.user_manager
                .remove_saved_music(FileId::new(request.file_id, 0), promise);
        }
        Function::canSendMessageToUser(request) => {
            // Requests.cpp:5772-5776: the local matrix answers; the
            // premium-required re-dispatch parks in the QueryMerger
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, Object::from);
            let mut env = user_env!(td);
            td.user_manager.can_send_message_to_user(
                &mut env,
                UserId::new(request.user_id),
                request.only_local,
                promise,
            );
        }
        Function::sharePhoneNumber(request) => {
            // Requests.cpp:6730-6734: parks until the contacts load, then
            // the AcceptContactQuery ack ([blocked: net])
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = user_env!(td);
            td.user_manager
                .share_phone_number(&mut env, UserId::new(request.user_id), promise);
        }
        Function::getWebAppPlaceholder(request) => {
            // Requests.cpp:8265-8269: the missing bot info answers the
            // C++ `send_result(nullptr)` 404
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |outline: Option<td_api::Outline>| {
                outline.map_or_else(
                    || {
                        Object::error(td_api::Error {
                            code: 404,
                            message: "Not Found".to_string(),
                        })
                    },
                    Object::outline,
                )
            });
            let mut env = user_env!(td);
            td.user_manager.get_web_app_placeholder(
                &mut env,
                UserId::new(request.bot_user_id),
                promise,
            );
        }
        Function::deleteProfilePhoto(_)
        | Function::setUserPersonalProfilePhoto(_)
        | Function::suggestUserProfilePhoto(_)
        | Function::setBotProfilePhoto(_) => {
            // the photo-set family: the non-null input path rides
            // FileManager (`set_profile_photo_impl`, the Files slice) and
            // the null-input halves are net actors
            // (DeleteContactProfilePhotoQuery/UpdateProfilePhotoQuery)
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }
        Function::addProfileAudio(_) => {
            // the input-audio path needs FileManager's get_input_file_id
            // (the Files slice)
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }
        Function::setUserNote(_)
        | Function::setMainProfileTab(_)
        | Function::setPersonalChat(_)
        | Function::createNewSecretChat(_)
        | Function::suggestUserBirthdate(_)
        | Function::toggleBotCanManageEmojiStatus(_)
        | Function::setProfileAccentColor(_)
        | Function::setUpgradedGiftColors(_)
        | Function::getSupportUser(_)
        | Function::setPassportElementErrors(_) => {
            // MessageEntity / UserFull ProfileTab / DialogManager /
            // passport / the missing setter halves (M.4.12 deferred them
            // with their types)
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }

        function => run_dialog_requests(td, id, function),
    }
}

/// The M.6.20 DialogManager arm group of the request dispatcher — see the
/// frame note at the forward site in [`run_request`].
#[inline(never)]
fn run_dialog_requests(td: &mut Td, id: u64, function: Function) {
    match function {
        // ------------------------------------------------------------------
        // M.6.20: the DialogManager-served requests
        // (Requests.cpp:3425-3530, :3667, :5781-5873, :6096, :6608, :7842).
        Function::searchPublicChat(request) => {
            // the C++ has no CHECK_IS_USER here (Requests.cpp:3429-3432) —
            // bots may resolve public chats
            let Some(username) = clean_or_utf8_error(td, id, &request.username) else {
                return;
            };
            // SearchPublicChatRequest: set_tries(4); get_tries() < 3 →
            // false on the first try. The result object is the
            // MessagesManager chat (the builder lands with M.10; on this
            // base every reachable path errors first — stub inventory).
            let (promise, slot) = unit_request_promise();
            let _dialog_id =
                crate::dialog_manager::search_public_dialog(td, &username, false, promise);
            if fail_from_slot(td, id, &slot) {
                return;
            }
            td.send_error_impl(id, 500, "Chat info not found");
        }
        Function::searchPublicChats(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let (promise, slot) = unit_request_promise();
            let dialog_ids = crate::dialog_manager::search_public_dialogs(
                td,
                &query,
                request.type_filter.as_ref(),
                promise,
            );
            if fail_from_slot(td, id, &slot) {
                return;
            }
            let chats =
                crate::dialog_manager::get_chats_object(-1, &dialog_ids, "searchPublicChats");
            td.send_result(id, Object::chats(chats));
        }
        Function::searchChats(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let (promise, slot) = unit_request_promise();
            let (total_count, dialog_ids) = crate::dialog_manager::search_dialogs(
                td,
                &query,
                request.type_filter.as_ref(),
                request.limit,
                promise,
            );
            if fail_from_slot(td, id, &slot) {
                return;
            }
            let chats =
                crate::dialog_manager::get_chats_object(total_count, &dialog_ids, "searchChats");
            td.send_result(id, Object::chats(chats));
        }
        Function::searchChatsOnServer(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let (promise, slot) = unit_request_promise();
            let dialog_ids = crate::dialog_manager::search_dialogs_on_server(
                td,
                &query,
                request.type_filter.as_ref(),
                request.limit,
                promise,
            );
            if fail_from_slot(td, id, &slot) {
                return;
            }
            let chats =
                crate::dialog_manager::get_chats_object(-1, &dialog_ids, "searchChatsOnServer");
            td.send_result(id, Object::chats(chats));
        }
        Function::searchRecentlyFoundChats(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let (promise, slot) = unit_request_promise();
            let (total_count, dialog_ids) = crate::dialog_manager::search_recently_found_dialogs(
                td,
                &query,
                request.type_filter.as_ref(),
                request.limit,
                promise,
            );
            if fail_from_slot(td, id, &slot) {
                return;
            }
            let chats = crate::dialog_manager::get_chats_object(
                total_count,
                &dialog_ids,
                "searchRecentlyFoundChats",
            );
            td.send_result(id, Object::chats(chats));
        }
        Function::getRecentlyOpenedChats(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let (promise, slot) = unit_request_promise();
            let (total_count, dialog_ids) =
                crate::dialog_manager::get_recently_opened_dialogs(td, request.limit, promise);
            if fail_from_slot(td, id, &slot) {
                return;
            }
            let chats = crate::dialog_manager::get_chats_object(
                total_count,
                &dialog_ids,
                "getRecentlyOpenedChats",
            );
            td.send_result(id, Object::chats(chats));
        }
        Function::checkChatUsername(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(username) = clean_or_utf8_error(td, id, &request.username) else {
                return;
            };
            let (promise, slot) = object_request_promise();
            crate::dialog_manager::check_dialog_username(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                &username,
                false,
                promise,
            );
            finish_object_slot(td, id, slot);
        }
        Function::checkBotUsername(request) => {
            let Some(username) = clean_or_utf8_error(td, id, &request.username) else {
                return;
            };
            let (promise, slot) = object_request_promise();
            crate::dialog_manager::check_dialog_username(
                td,
                crate::dialog_id::DialogId::new(0),
                &username,
                true,
                promise,
            );
            finish_object_slot(td, id, slot);
        }
        Function::addRecentlyFoundChat(request) => {
            if !check_is_user(td, id) {
                return;
            }
            match crate::dialog_manager::add_recently_found_dialog(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
            ) {
                Result::Ok(()) => td.send_result(id, ok_object()),
                Result::Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::removeRecentlyFoundChat(request) => {
            if !check_is_user(td, id) {
                return;
            }
            match crate::dialog_manager::remove_recently_found_dialog(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
            ) {
                Result::Ok(()) => td.send_result(id, ok_object()),
                Result::Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::clearRecentlyFoundChats(_) => {
            if !check_is_user(td, id) {
                return;
            }
            crate::dialog_manager::clear_recently_found_dialogs(td);
            td.send_result(id, ok_object());
        }
        Function::deleteChat(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let (promise, slot) = unit_request_promise();
            crate::dialog_manager::delete_dialog(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                promise,
            );
            finish_unit_slot(td, id, slot);
        }
        Function::upgradeBasicGroupChatToSupergroupChat(request) => {
            // Requests.cpp:5543 — `migrate_dialog_to_megagroup`.
            if !check_is_user(td, id) {
                return;
            }
            let (promise, slot) = object_request_promise();
            crate::dialog_manager::migrate_dialog_to_megagroup(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                promise,
            );
            finish_object_slot(td, id, slot);
        }
        Function::setChatTitle(request) => {
            // the C++ has no CHECK_IS_USER here (Requests.cpp:5778-5782) —
            // bots set basic-group titles through this method
            let Some(title) = clean_or_utf8_error(td, id, &request.title) else {
                return;
            };
            let (promise, slot) = unit_request_promise();
            crate::dialog_manager::set_dialog_title(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                &title,
                promise,
            );
            finish_unit_slot(td, id, slot);
        }
        Function::setChatPhoto(request) => {
            // the C++ has no CHECK_IS_USER here (Requests.cpp:5784-5787)
            let (promise, slot) = unit_request_promise();
            crate::dialog_manager::set_dialog_photo(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.photo.as_ref(),
                promise,
            );
            finish_unit_slot(td, id, slot);
        }
        Function::setChatAccentColor(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let (promise, slot) = unit_request_promise();
            crate::dialog_manager::set_dialog_accent_color(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                crate::peer_color::AccentColorId::new(request.accent_color_id),
                crate::emoji_status::CustomEmojiId::new(request.background_custom_emoji_id),
                promise,
            );
            finish_unit_slot(td, id, slot);
        }
        Function::setChatProfileAccentColor(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let (promise, slot) = unit_request_promise();
            crate::dialog_manager::set_dialog_profile_accent_color(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                crate::peer_color::AccentColorId::new(request.profile_accent_color_id),
                crate::emoji_status::CustomEmojiId::new(request.profile_background_custom_emoji_id),
                promise,
            );
            finish_unit_slot(td, id, slot);
        }
        Function::setChatPermissions(request) => {
            // the C++ has no CHECK_IS_USER here (Requests.cpp:5818-5821)
            let (promise, slot) = unit_request_promise();
            crate::dialog_manager::set_dialog_permissions(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.permissions.as_ref(),
                promise,
            );
            finish_unit_slot(td, id, slot);
        }
        Function::toggleChatHasProtectedContent(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let (promise, slot) = unit_request_promise();
            crate::dialog_manager::toggle_dialog_has_protected_content(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                None,
                request.has_protected_content,
                promise,
            );
            finish_unit_slot(td, id, slot);
        }
        Function::processChatHasProtectedContentDisableRequest(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let (promise, slot) = unit_request_promise();
            crate::dialog_manager::toggle_dialog_has_protected_content(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                Some(crate::chat_manager::MessageId::new(
                    request.request_message_id,
                )),
                !request.approve,
                promise,
            );
            finish_unit_slot(td, id, slot);
        }
        Function::setChatDescription(request) => {
            // the C++ has no CHECK_IS_USER here (Requests.cpp:6093-6097)
            let Some(description) = clean_or_utf8_error(td, id, &request.description) else {
                return;
            };
            let (promise, slot) = unit_request_promise();
            crate::dialog_manager::set_dialog_description(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                &description,
                promise,
            );
            finish_unit_slot(td, id, slot);
        }
        Function::getBlockedMessageSenders(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let (promise, slot) = object_request_promise();
            crate::dialog_manager::get_blocked_dialogs(
                td,
                request.block_list.as_ref(),
                request.offset,
                request.limit,
                promise,
            );
            finish_object_slot(td, id, slot);
        }
        Function::reportChat(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(text) = clean_or_utf8_error(td, id, &request.text) else {
                return;
            };
            let option_id = String::from_utf8_lossy(&request.option_id).into_owned();
            let message_ids: Vec<crate::chat_manager::MessageId> = request
                .message_ids
                .iter()
                .map(|&message_id| crate::chat_manager::MessageId::new(message_id))
                .collect();
            let (promise, slot) = object_request_promise();
            crate::dialog_manager::report_dialog(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                &option_id,
                &message_ids,
                &text,
                promise,
            );
            finish_object_slot(td, id, slot);
        }

        // setChatEmojiStatus needs the td_api emojiStatus → EmojiStatus
        // conversion (the collectible variants arrive with the emoji-status
        // read surface), and reportChatPhoto needs ReportReason — both stay
        // interim with those gaps.

        // --- the chat-manager request family (M.5.47, folded in at the
        // integration-m6b merge) — each local-validating entry settles its
        // promise synchronously; the net queries terminate in the
        // blocked-net 500 until the net step lands. setChatDescription and
        // deleteChat stay with the dialog_manager arms above: the C++
        // routes them through the DialogManager dialog type-switch
        // (Requests.cpp:6093, :3655), unlike these channel-scoped ones.
        Function::setSupergroupUsername(request) => {
            let (promise, recorded) = record_unit_promise();
            let username = crate::misc::clean_input_string(&request.username).unwrap_or_default();
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.set_channel_username(
                user_manager,
                &mut env,
                ChannelId::new(request.supergroup_id),
                &username,
                promise,
            );
            finish_unit_request(id, td, recorded);
        }
        Function::toggleSupergroupUsernameIsActive(request) => {
            let (promise, recorded) = record_unit_promise();
            let username = crate::misc::clean_input_string(&request.username).unwrap_or_default();
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.toggle_channel_username_is_active(
                user_manager,
                &mut env,
                ChannelId::new(request.supergroup_id),
                username,
                request.is_active,
                promise,
            );
            finish_unit_request(id, td, recorded);
        }
        Function::setChatLocation(request) => {
            let (promise, recorded) = record_unit_promise();
            let location = crate::chat_manager::DialogLocation::from_td_api(
                request.location.as_ref().unwrap(),
            );
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.set_channel_location(
                user_manager,
                &mut env,
                ChannelId::new(request.chat_id),
                &location,
                promise,
            );
            finish_unit_request(id, td, recorded);
        }
        Function::setChatSlowModeDelay(request) => {
            let (promise, recorded) = record_unit_promise();
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.set_channel_slow_mode_delay(
                user_manager,
                &mut env,
                DialogId::from_channel_id(ChannelId::new(request.chat_id)),
                request.slow_mode_delay,
                promise,
            );
            finish_unit_request(id, td, recorded);
        }
        Function::toggleSupergroupIsForum(request) => {
            let (promise, recorded) = record_unit_promise();
            let (mut env, _user_manager, chat_manager) = chat_env_split(td);
            chat_manager.toggle_channel_is_forum(
                &mut env,
                ChannelId::new(request.supergroup_id),
                request.is_forum,
                request.has_forum_tabs,
                promise,
            );
            finish_unit_request(id, td, recorded);
        }

        // --- web browser settings (Requests.cpp:3154-3180, 3603-3608) ---
        Function::getLinkWebBrowserType(request) => {
            // `Requests.cpp:3603-3608` — the classification is local, no
            // net query; CHECK_IS_USER + CLEAN_INPUT_STRING.
            if !check_is_user(td, id) {
                return;
            }
            let Some(url) = crate::misc::clean_input_string(&request.link) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            match td.web_browser_manager.get_web_browser_type(&url) {
                Ok(td_api::WebBrowserType::webBrowserTypeExternal(object)) => {
                    td.send_result(id, Object::webBrowserTypeExternal(object))
                }
                Ok(td_api::WebBrowserType::webBrowserTypeInApp(object)) => {
                    td.send_result(id, Object::webBrowserTypeInApp(object))
                }
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::changeWebBrowserSettings(request) => {
            // `Requests.cpp:3154-3160` — CHECK_IS_USER + the
            // UpdateWebBrowserSettingsQuery ([blocked: net]); the promise
            // parks until the query answer arrives.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = WebBrowserEnv {
                global: &mut td.global,
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.web_browser_updates,
            };
            td.web_browser_manager.update_web_browser_settings(
                &mut env,
                request.open_external_browser,
                request.display_close_button,
                promise,
            );
        }
        Function::addWebBrowserSettingsException(request) => {
            // `Requests.cpp:3161-3168` — CLEAN_INPUT_STRING + the
            // ToggleWebBrowserSettingsExceptionQuery ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let Some(url) = crate::misc::clean_input_string(&request.url) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = WebBrowserEnv {
                global: &mut td.global,
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.web_browser_updates,
            };
            td.web_browser_manager.add_web_browser_settings_exception(
                &mut env,
                request.open_external_browser,
                &url,
                promise,
            );
        }
        Function::removeWebBrowserSettingsException(request) => {
            // `Requests.cpp:3169-3174`.
            if !check_is_user(td, id) {
                return;
            }
            let Some(url) = crate::misc::clean_input_string(&request.url) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = WebBrowserEnv {
                global: &mut td.global,
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.web_browser_updates,
            };
            td.web_browser_manager
                .remove_web_browser_settings_exception(&mut env, &url, promise);
        }
        Function::removeAllWebBrowserSettingsExceptions(_) => {
            // `Requests.cpp:3176-3180`.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = WebBrowserEnv {
                global: &mut td.global,
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.web_browser_updates,
            };
            td.web_browser_manager
                .remove_all_web_browser_settings_exceptions(&mut env, promise);
        }
        // ------------------------------------------------------------------
        // The M.12.10 Files arm group (Requests.cpp:2947-3080, 6451-6572):
        // the td_api file/download/upload/storage plane over the
        // FileManager, DownloadManager and StorageManager. Inlined into
        // the dialog match on purpose — a fifth by-value `Function` chain
        // level would stack another ~360 KB debug frame and cross the
        // 2 MB scheduler-thread guard (the stack-probe SIGSEGV the live
        // gate caught).
        // ------------------------------------------------------------------
        // getFile (Requests.cpp:2947-2955)
        Function::getFile(request) => {
            let mut file = td
                .file_manager
                .lock()
                .get_file_object(crate::files::FileId::new(request.file_id, 0));
            if file.id == 0 {
                // `send_result(id, nullptr)` — the null result is the 404
                // of `Td::send_result`
                td.send_result(
                    id,
                    Object::error(td_api::Error {
                        code: 404,
                        message: "Not Found".to_string(),
                    }),
                );
            } else {
                file.id = request.file_id;
                td.send_result(id, Object::file(file));
            }
        }

        // getRemoteFile (Requests.cpp:2957-2966)
        Function::getRemoteFile(request) => {
            let Some(remote_file_id) = clean_or_utf8_error(td, id, &request.remote_file_id) else {
                return;
            };
            let file_type = match request.file_type.as_ref() {
                Some(file_type) => crate::files::get_file_type(file_type),
                None => crate::files::FileType::Temp,
            };
            let outcome = {
                let mut file_manager = td.file_manager.lock();
                match file_manager.from_persistent_id(&remote_file_id, file_type) {
                    Ok(file_id) => Ok(file_manager.get_file_object(file_id)),
                    Err(status) => Err(status),
                }
            };
            match outcome {
                Ok(file) => td.send_result(id, Object::file(file)),
                Err(status) => send_error_status(td, id, status),
            }
        }

        // getStorageStatistics (Requests.cpp:2968-2979)
        Function::getStorageStatistics(request) => {
            let stats = td.get_storage_stats_object(false, request.chat_limit);
            td.send_result(id, Object::storageStatistics(stats));
        }

        // getStorageStatisticsFast (Requests.cpp:2981-2992)
        Function::getStorageStatisticsFast(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let stats = td.get_storage_stats_fast_object();
            td.send_result(id, Object::storageStatisticsFast(stats));
        }

        // getDatabaseStatistics (Requests.cpp:2994-3004)
        Function::getDatabaseStatistics(_) => match td.get_database_statistics_object() {
            Ok(stats) => td.send_result(id, Object::databaseStatistics(stats)),
            Err(status) => send_error_status(td, id, status),
        },

        // optimizeStorage (Requests.cpp:3006-3033)
        Function::optimizeStorage(request) => match td.optimize_storage(request) {
            Ok(stats) => td.send_result(id, Object::storageStatistics(stats)),
            Err(status) => send_error_status(td, id, status),
        },

        // downloadFile (Requests.cpp:6451-6456)
        Function::downloadFile(request) => {
            let promise = td.deferred_promise(id, Object::file);
            let mut file_manager = td.file_manager.lock();
            let mut file_load_manager = td.file_load_manager.lock();
            file_manager.download_file(
                crate::files::FileId::new(request.file_id, 0),
                request.priority,
                request.offset,
                request.limit,
                request.synchronous,
                promise,
                &mut file_load_manager,
            );
        }

        // cancelDownloadFile (Requests.cpp:6469-6481)
        Function::cancelDownloadFile(request) => {
            {
                let mut file_manager = td.file_manager.lock();
                let mut file_load_manager = td.file_load_manager.lock();
                file_manager.cancel_download(
                    crate::files::FileId::new(request.file_id, 0),
                    0,
                    request.only_if_pending,
                    &mut file_load_manager,
                );
            }
            td.send_result(id, ok_object());
        }

        // preliminaryUploadFile (Requests.cpp:6483-6487)
        Function::preliminaryUploadFile(request) => {
            let file_type = match request.file_type.as_ref() {
                Some(file_type) => crate::files::get_file_type(file_type),
                None => crate::files::FileType::Temp,
            };
            let outcome = {
                let mut file_manager = td.file_manager.lock();
                let mut file_load_manager = td.file_load_manager.lock();
                file_manager.preliminary_upload_file(
                    request.file.as_ref(),
                    file_type,
                    request.priority,
                    &mut file_load_manager,
                )
            };
            match outcome {
                Ok(file) => td.send_result(id, Object::file(file)),
                Err(status) => send_error_status(td, id, status),
            }
        }

        // cancelPreliminaryUploadFile (Requests.cpp:6489-6493)
        Function::cancelPreliminaryUploadFile(request) => {
            {
                let mut file_manager = td.file_manager.lock();
                let mut file_load_manager = td.file_load_manager.lock();
                file_manager.cancel_upload(
                    crate::files::FileUploadId::new(
                        crate::files::FileId::new(request.file_id, 0),
                        0,
                    ),
                    &mut file_load_manager,
                );
            }
            td.send_result(id, ok_object());
        }

        // writeGeneratedFilePart (Requests.cpp:6495-6499)
        Function::writeGeneratedFilePart(request) => {
            let result = {
                let mut file_manager = td.file_manager.lock();
                let mut file_load_manager = td.file_load_manager.lock();
                file_manager.external_file_generate_write_part(
                    request.generation_id,
                    request.offset,
                    request.data,
                    &mut file_load_manager,
                )
            };
            finish_unit_result(td, id, result);
        }

        // setFileGenerationProgress (Requests.cpp:6501-6505)
        Function::setFileGenerationProgress(request) => {
            let result = td.file_manager.lock().external_file_generate_progress(
                request.generation_id,
                request.expected_size,
                request.local_prefix_size,
            );
            finish_unit_result(td, id, result);
        }

        // finishFileGeneration (Requests.cpp:6507-6516)
        Function::finishFileGeneration(request) => {
            let error = request.error.as_ref().map(|error| {
                // CLEAN_INPUT_STRING(request.error_->message_)
                let message = crate::misc::clean_input_string(&error.message)
                    .unwrap_or_else(|| error.message.clone());
                Status::error(error.code, &message)
            });
            let result = {
                let mut file_manager = td.file_manager.lock();
                let mut file_load_manager = td.file_load_manager.lock();
                file_manager.external_file_generate_finish(
                    request.generation_id,
                    error,
                    &mut file_load_manager,
                )
            };
            finish_unit_result(td, id, result);
        }

        // toggleDownloadIsPaused (Requests.cpp:6531-6534)
        Function::toggleDownloadIsPaused(request) => {
            let result = td.download_manager.lock().toggle_is_paused(
                crate::files::FileId::new(request.file_id, 0),
                request.is_paused,
            );
            finish_unit_result(td, id, result);
        }

        // toggleAllDownloadsArePaused (Requests.cpp:6536-6539)
        Function::toggleAllDownloadsArePaused(request) => {
            let result = td
                .download_manager
                .lock()
                .toggle_all_is_paused(request.are_paused);
            finish_unit_result(td, id, result);
        }

        // removeFileFromDownloads (Requests.cpp:6549-6553)
        Function::removeFileFromDownloads(request) => {
            let result = td.download_manager.lock().remove_file(
                crate::files::FileId::new(request.file_id, 0),
                crate::files::FileSourceId::default(),
                request.delete_from_cache,
            );
            finish_unit_result(td, id, result);
        }

        // removeAllFilesFromDownloads (Requests.cpp:6555-6559)
        Function::removeAllFilesFromDownloads(request) => {
            let result = td.download_manager.lock().remove_all_files(
                request.only_active,
                request.only_completed,
                request.delete_from_cache,
            );
            finish_unit_result(td, id, result);
        }

        // searchFileDownloads (Requests.cpp:6561-6569)
        Function::searchFileDownloads(request) => {
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let outcome = td.download_manager.lock().search(
                &query,
                request.only_active,
                request.only_completed,
                &offset,
                request.limit,
            );
            match outcome {
                Ok(found) => td.send_result(id, Object::foundFileDownloads(found)),
                Err(status) => send_error_status(td, id, status),
            }
        }

        // --- calls (Requests.cpp:5088-5135) ---
        Function::createCall(request) => {
            // `Requests.cpp:5088-5100`: CHECK_IS_USER + the protocol gate;
            // the manager settles the promise with the local CallId.
            if !check_is_user(td, id) {
                return;
            }
            let Some(protocol) = request.protocol.as_ref() else {
                return td.send_error_impl(id, 400, "Call protocol must be non-empty");
            };
            let promise =
                td.deferred_promise(id, |call_id: td_api::CallId| Object::callId(call_id));
            let mut env = CallEnv {
                user_env: user_env!(td),
                user_manager: &mut td.user_manager,
                notification_manager: &mut td.notification_manager,
                updates: &mut td.call_updates,
                dh_config: &mut td.call_dh_config,
            };
            td.call_manager.create_call(
                &mut env,
                UserId::new(request.user_id),
                CallProtocol::from_td_api(protocol),
                request.is_video,
                promise,
            );
        }
        Function::acceptCall(request) => {
            // `Requests.cpp:5107-5115`.
            if !check_is_user(td, id) {
                return;
            }
            let Some(protocol) = request.protocol.as_ref() else {
                return td.send_error_impl(id, 400, "Call protocol must be non-empty");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = CallEnv {
                user_env: user_env!(td),
                user_manager: &mut td.user_manager,
                notification_manager: &mut td.notification_manager,
                updates: &mut td.call_updates,
                dh_config: &mut td.call_dh_config,
            };
            td.call_manager.accept_call(
                &mut env,
                CallId::new(request.call_id),
                CallProtocol::from_td_api(protocol),
                promise,
            );
        }
        Function::sendCallSignalingData(request) => {
            // `Requests.cpp:5117-5122`.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = CallEnv {
                user_env: user_env!(td),
                user_manager: &mut td.user_manager,
                notification_manager: &mut td.notification_manager,
                updates: &mut td.call_updates,
                dh_config: &mut td.call_dh_config,
            };
            td.call_manager.send_call_signaling_data(
                &mut env,
                CallId::new(request.call_id),
                request.data,
                promise,
            );
        }
        Function::discardCall(request) => {
            // `Requests.cpp:5124-5130`: CLEAN_INPUT_STRING on the invite
            // link.
            if !check_is_user(td, id) {
                return;
            }
            let Some(invite_link) = crate::misc::clean_input_string(&request.invite_link) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = CallEnv {
                user_env: user_env!(td),
                user_manager: &mut td.user_manager,
                notification_manager: &mut td.notification_manager,
                updates: &mut td.call_updates,
                dh_config: &mut td.call_dh_config,
            };
            td.call_manager.discard_call(
                &mut env,
                CallId::new(request.call_id),
                request.is_disconnected,
                &invite_link,
                request.duration,
                request.is_video,
                request.connection_id,
                promise,
            );
        }
        Function::sendCallRating(request) => {
            // `Requests.cpp:5132-5138`.
            if !check_is_user(td, id) {
                return;
            }
            let Some(comment) = crate::misc::clean_input_string(&request.comment) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = CallEnv {
                user_env: user_env!(td),
                user_manager: &mut td.user_manager,
                notification_manager: &mut td.notification_manager,
                updates: &mut td.call_updates,
                dh_config: &mut td.call_dh_config,
            };
            td.call_manager.rate_call(
                &mut env,
                &request.call_id,
                request.rating,
                comment,
                request.problems,
                promise,
            );
        }
        Function::sendCallDebugInformation(request) => {
            // `Requests.cpp:5140-5146`.
            if !check_is_user(td, id) {
                return;
            }
            let Some(debug_information) =
                crate::misc::clean_input_string(&request.debug_information)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let call_id = match InputCallId::get_input_call_id(&request.call_id) {
                Ok(call_id) => call_id,
                Err(status) => {
                    return td.send_error_impl(id, status.code(), status.message());
                }
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = CallEnv {
                user_env: user_env!(td),
                user_manager: &mut td.user_manager,
                notification_manager: &mut td.notification_manager,
                updates: &mut td.call_updates,
                dh_config: &mut td.call_dh_config,
            };
            td.call_manager.send_call_debug_information(
                &mut env,
                call_id,
                debug_information,
                promise,
            );
        }
        Function::sendCallLog(_) => {
            // `Requests.cpp:5148-5153` — the FileManager call-log upload
            // surface (FileType::CallLog + upload callback) queues on the
            // M.12 upload continuation; the interim 500 stays.
            if !check_is_user(td, id) {
                return;
            }
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }

        // --- account / sessions (Requests.cpp:2452-2478, 2515, …) ---
        Function::confirmQrCodeAuthentication(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(link) = crate::misc::clean_input_string(&request.link) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise =
                td.deferred_promise(id, |session: td_api::Session| Object::session(session));
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
            };
            td.account_manager
                .confirm_qr_code_authentication(&mut env, &link, promise);
        }
        Function::hideSuggestedAction(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(action) = request.action.as_ref() else {
                return td.send_error_impl(id, 400, "Action must be non-empty");
            };
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
            };
            td.suggested_action_manager
                .hide_suggested_action(&mut env, SuggestedAction::from_td_api(action));
            td.send_result(id, ok_object());
        }
        Function::getUserLink(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |user_link: td_api::UserLink| {
                Object::userLink(user_link)
            });
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
            };
            td.account_manager.get_user_link(&mut env, promise);
        }
        Function::getActiveSessions(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |sessions: td_api::Sessions| Object::sessions(sessions));
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
            };
            td.account_manager.get_active_sessions(&mut env, promise);
        }
        Function::terminateSession(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
            };
            td.account_manager
                .terminate_session(&mut env, request.session_id, promise);
        }
        Function::terminateAllOtherSessions(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
            };
            td.account_manager
                .terminate_all_other_sessions(&mut env, promise);
        }
        Function::confirmSession(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
            };
            td.account_manager
                .confirm_session(&mut env, request.session_id, promise);
        }
        Function::toggleSessionCanAcceptCalls(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
            };
            td.account_manager.toggle_session_can_accept_calls(
                &mut env,
                request.session_id,
                request.can_accept_calls,
                promise,
            );
        }
        Function::toggleSessionCanAcceptSecretChats(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
            };
            td.account_manager.toggle_session_can_accept_secret_chats(
                &mut env,
                request.session_id,
                request.can_accept_secret_chats,
                promise,
            );
        }
        Function::getConnectedWebsites(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |websites: td_api::ConnectedWebsites| {
                Object::connectedWebsites(websites)
            });
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
            };
            td.account_manager.get_connected_websites(&mut env, promise);
        }
        Function::disconnectWebsite(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
            };
            td.account_manager
                .disconnect_website(&mut env, request.website_id, promise);
        }
        Function::disconnectAllWebsites(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
            };
            td.account_manager
                .disconnect_all_websites(&mut env, promise);
        }
        Function::setAccountTtl(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(ttl) = request.ttl.as_ref() else {
                return td.send_error_impl(id, 400, "New account TTL must be non-empty");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
            };
            td.account_manager
                .set_account_ttl(&mut env, ttl.days, promise);
        }
        Function::getAccountTtl(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |ttl: td_api::AccountTtl| Object::accountTtl(ttl));
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
            };
            td.account_manager.get_account_ttl(&mut env, promise);
        }
        Function::acceptTermsOfService(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(terms_of_service_id) =
                crate::misc::clean_input_string(&request.terms_of_service_id)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
            };
            td.terms_of_service_manager.accept_terms_of_service(
                &mut env,
                terms_of_service_id,
                promise,
            );
        }

        // --- community (Requests.cpp:3415-3432) ---
        Function::createCommunity(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = crate::misc::clean_input_string(&request.name) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |community_id: td_api::CommunityId| {
                Object::communityId(community_id)
            });
            let mut env = CommunityEnv {
                global: &mut td.global,
                dialog_manager: &mut td.dialog_manager,
                updates: &mut td.community_updates,
            };
            td.community_manager.create_community(
                &mut env,
                &name,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.is_chat_hidden,
                promise,
            );
        }
        Function::setCommunityName(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = crate::misc::clean_input_string(&request.name) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = CommunityEnv {
                global: &mut td.global,
                dialog_manager: &mut td.dialog_manager,
                updates: &mut td.community_updates,
            };
            td.community_manager.set_community_name(
                &mut env,
                CommunityId::new(request.community_id),
                &name,
                promise,
            );
        }

        // -- the payments set (M.43): Requests.cpp:8372-8436, 8757-8771,
        //    9247-9251; the CHECK_IS_USER/CHECK_IS_BOT gates match the
        //    C++ arms, the flows park their net tails through the env --
        Function::answerShippingQuery(request) => {
            // Requests.cpp:8372-8378: CHECK_IS_BOT + CLEAN_INPUT_STRING +
            // CREATE_OK_REQUEST_PROMISE
            if !check_is_bot(td, id) {
                return;
            }
            let Some(error_message) = clean_or_utf8_error(td, id, &request.error_message) else {
                return;
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            payments_arm(
                td,
                |env| {
                    crate::payments::answer_shipping_query(
                        env,
                        request.shipping_query_id,
                        request.shipping_options,
                        &error_message,
                        promise,
                    )
                },
                id,
            );
        }
        Function::answerPreCheckoutQuery(request) => {
            // Requests.cpp:8380-8386
            if !check_is_bot(td, id) {
                return;
            }
            let Some(error_message) = clean_or_utf8_error(td, id, &request.error_message) else {
                return;
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            payments_arm(
                td,
                |env| {
                    crate::payments::answer_pre_checkout_query(
                        env,
                        request.pre_checkout_query_id,
                        &error_message,
                        promise,
                    )
                },
                id,
            );
        }
        Function::getBankCardInfo(request) => {
            // Requests.cpp:8387-8392
            if !check_is_user(td, id) {
                return;
            }
            let Some(bank_card_number) = clean_or_utf8_error(td, id, &request.bank_card_number)
            else {
                return;
            };
            let promise =
                td.deferred_promise(id, |b: td_api::BankCardInfo| Object::bankCardInfo(b));
            payments_arm(
                td,
                |env| crate::payments::get_bank_card_info(env, &bank_card_number, promise),
                id,
            );
        }
        Function::getPaymentForm(request) => {
            // Requests.cpp:8394-8398
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |b: td_api::PaymentForm| Object::paymentForm(b));
            payments_arm(
                td,
                |env| {
                    crate::payments::get_payment_form(
                        env,
                        request.input_invoice,
                        request.theme.as_ref(),
                        promise,
                    )
                },
                id,
            );
        }
        Function::validateOrderInfo(request) => {
            // Requests.cpp:8400-8405
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |b: td_api::ValidatedOrderInfo| {
                Object::validatedOrderInfo(b)
            });
            payments_arm(
                td,
                |env| {
                    crate::payments::validate_order_info(
                        env,
                        request.input_invoice,
                        request.order_info,
                        request.allow_save,
                        promise,
                    )
                },
                id,
            );
        }
        Function::sendPaymentForm(request) => {
            // Requests.cpp:8407-8414: the order/shipping identifiers are
            // cleaned before the flow
            if !check_is_user(td, id) {
                return;
            }
            let Some(order_info_id) = clean_or_utf8_error(td, id, &request.order_info_id) else {
                return;
            };
            let Some(shipping_option_id) = clean_or_utf8_error(td, id, &request.shipping_option_id)
            else {
                return;
            };
            let promise =
                td.deferred_promise(id, |b: td_api::PaymentResult| Object::paymentResult(b));
            payments_arm(
                td,
                |env| {
                    crate::payments::send_payment_form(
                        env,
                        request.input_invoice,
                        request.payment_form_id,
                        &order_info_id,
                        &shipping_option_id,
                        request.credentials,
                        request.tip_amount,
                        promise,
                    )
                },
                id,
            );
        }
        Function::getPaymentReceipt(request) => {
            // Requests.cpp:8416-8420: the MessageFullId comes from the
            // (chat_id, message_id) pair
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |b: td_api::PaymentReceipt| Object::paymentReceipt(b));
            payments_arm(
                td,
                |env| {
                    crate::payments::get_payment_receipt(
                        env,
                        crate::dialog_id::MessageFullId::new(
                            crate::dialog_id::DialogId::new(request.chat_id),
                            crate::chat_manager::MessageId::new(request.message_id),
                        ),
                        promise,
                    )
                },
                id,
            );
        }
        Function::getSavedOrderInfo(_) => {
            // Requests.cpp:8422-8426
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |b: td_api::OrderInfo| Object::orderInfo(b));
            payments_arm(
                td,
                |env| crate::payments::get_saved_order_info(env, promise),
                id,
            );
        }
        Function::deleteSavedOrderInfo(_) => {
            // Requests.cpp:8428-8432: CREATE_OK_REQUEST_PROMISE
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            payments_arm(
                td,
                |env| crate::payments::delete_saved_order_info(env, promise),
                id,
            );
        }
        Function::deleteSavedCredentials(_) => {
            // Requests.cpp:8434-8438
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            payments_arm(
                td,
                |env| crate::payments::delete_saved_credentials(env, promise),
                id,
            );
        }
        Function::createInvoiceLink(request) => {
            // Requests.cpp:8757-8762: CHECK_IS_BOT +
            // CREATE_HTTP_URL_REQUEST_PROMISE — the exported invoice URL
            // answers the httpUrl object
            if !check_is_bot(td, id) {
                return;
            }
            let Some(invoice) = request.invoice else {
                return td.send_error_impl(id, 400, "Invoice must be non-empty");
            };
            let promise =
                td.deferred_promise(id, |url: String| Object::httpUrl(td_api::HttpUrl { url }));
            payments_arm(
                td,
                |env| {
                    crate::payments::export_invoice(
                        env,
                        crate::payments::BusinessConnectionId::new(request.business_connection_id),
                        *invoice,
                        promise,
                    )
                },
                id,
            );
        }
        Function::getCollectibleItemInfo(request) => {
            // Requests.cpp:9247-9251: no CHECK gate
            let promise = td.deferred_promise(id, |b: td_api::CollectibleItemInfo| {
                Object::collectibleItemInfo(b)
            });
            payments_arm(
                td,
                |env| crate::payments::get_collectible_info(env, request.r#type, promise),
                id,
            );
        }

        // Everything else waits for its manager step.
        // Everything else waits for its manager step.
        _ => run_forum_topic_requests(td, id, function),
    }
}

/// The M.49 ForumTopicManager arm group of the request dispatcher — see
/// the frame note at the forward site in [`run_request`]. Every arm routes
/// into `td_->forum_topic_manager_->...` (Requests.cpp:4835-4904, 7963);
/// the network tails are the [blocked: net] query seams.
#[allow(clippy::too_many_lines)]
fn run_forum_topic_requests(td: &mut Td, id: u64, function: Function) {
    fn forum_arm(
        td: &mut Td,
        run: impl FnOnce(
            &mut crate::td::ForumTopicTdEnv<'_>,
            &mut crate::forum_topic_manager::ForumTopicManager,
        ),
    ) {
        let crate::td::Td {
            forum_topic_manager,
            global,
            auth_manager,
            chat_manager,
            notification_updates,
            ..
        } = td;
        let mut env = crate::td::ForumTopicTdEnv {
            global,
            auth_is_bot: auth_manager.is_bot(),
            chat_manager,
            updates: notification_updates,
        };
        run(&mut env, forum_topic_manager);
    }

    macro_rules! ok_promise {
        ($td:expr) => {
            $td.deferred_promise::<(), _>(id, |_| ok_object())
        };
    }

    /// The null-result 404 mapper (the forumTopic answer).
    fn forum_topic_optional_to_object(value: Option<td_api::ForumTopic>) -> Object {
        match value {
            Some(topic) => Object::forumTopic(topic),
            None => Object::error(td_api::Error {
                code: 404,
                message: "Not Found".to_owned(),
            }),
        }
    }

    match function {
        // createForumTopic (Requests.cpp:4832-4838)
        Function::createForumTopic(request) => {
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise =
                td.deferred_promise(id, |value: Option<td_api::ForumTopicInfo>| match value {
                    Some(info) => Object::forumTopicInfo(info),
                    None => Object::error(td_api::Error {
                        code: 404,
                        message: "Not Found".to_owned(),
                    }),
                });
            forum_arm(td, |env, manager| {
                manager.create_forum_topic(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    &name,
                    request.is_name_implicit,
                    request.icon,
                    promise,
                );
            });
        }

        // editForumTopic (Requests.cpp:4840-4847)
        Function::editForumTopic(request) => {
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = ok_promise!(td);
            forum_arm(td, |env, manager| {
                manager.edit_forum_topic(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::forum_topic::ForumTopicId::new(request.forum_topic_id),
                    &name,
                    request.edit_icon_custom_emoji,
                    crate::emoji_status::CustomEmojiId::new(request.icon_custom_emoji_id),
                    promise,
                );
            });
        }

        // getForumTopic (Requests.cpp:4849-4854)
        Function::getForumTopic(request) => {
            let promise = td.deferred_promise(id, forum_topic_optional_to_object);
            forum_arm(td, |env, manager| {
                manager.get_forum_topic(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::forum_topic::ForumTopicId::new(request.forum_topic_id),
                    promise,
                );
            });
        }

        // getForumTopicLink (Requests.cpp:4860-4867)
        Function::getForumTopicLink(request) => {
            let promise = td.deferred_promise::<td_api::MessageLink, _>(id, Object::messageLink);
            forum_arm(td, |env, manager| {
                manager.get_forum_topic_link(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::forum_topic::ForumTopicId::new(request.forum_topic_id),
                    promise,
                );
            });
        }

        // getForumTopics (Requests.cpp:4869-4877)
        Function::getForumTopics(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::ForumTopics, _>(id, Object::forumTopics);
            forum_arm(td, |env, manager| {
                manager.get_forum_topics(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    query,
                    request.offset_date,
                    crate::message_id::MessageId::new(request.offset_message_id),
                    crate::forum_topic::ForumTopicId::new(request.offset_forum_topic_id),
                    request.limit,
                    promise,
                );
            });
        }

        // toggleForumTopicIsClosed (Requests.cpp:4879-4885)
        Function::toggleForumTopicIsClosed(request) => {
            let promise = ok_promise!(td);
            forum_arm(td, |env, manager| {
                manager.toggle_forum_topic_is_closed(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::forum_topic::ForumTopicId::new(request.forum_topic_id),
                    request.is_closed,
                    promise,
                );
            });
        }

        // toggleGeneralForumTopicIsHidden (Requests.cpp:4887-4892)
        Function::toggleGeneralForumTopicIsHidden(request) => {
            let promise = ok_promise!(td);
            forum_arm(td, |env, manager| {
                manager.toggle_forum_topic_is_hidden(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    request.is_hidden,
                    promise,
                );
            });
        }

        // toggleForumTopicIsPinned (Requests.cpp:4894-4899)
        Function::toggleForumTopicIsPinned(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            forum_arm(td, |env, manager| {
                manager.toggle_forum_topic_is_pinned(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::forum_topic::ForumTopicId::new(request.forum_topic_id),
                    request.is_pinned,
                    promise,
                );
            });
        }

        // setPinnedForumTopics (Requests.cpp:4901-4906)
        Function::setPinnedForumTopics(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let forum_topic_ids =
                crate::forum_topic::ForumTopicId::get_forum_topic_ids(&request.forum_topic_ids);
            forum_arm(td, |env, manager| {
                manager.set_pinned_forum_topics(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    forum_topic_ids,
                    promise,
                );
            });
        }

        // deleteForumTopic (Requests.cpp:4908-4913)
        Function::deleteForumTopic(request) => {
            let promise = ok_promise!(td);
            forum_arm(td, |env, manager| {
                manager.delete_forum_topic(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::forum_topic::ForumTopicId::new(request.forum_topic_id),
                    promise,
                );
            });
        }

        // setForumTopicNotificationSettings (Requests.cpp:7963-7967) —
        // [blocked: the notification-settings plane].
        Function::setForumTopicNotificationSettings(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let mut promise = ok_promise!(td);
            tdutils::log_info!(
                "setForumTopicNotificationSettings({}, {}) — [blocked: the notification-settings plane]",
                request.chat_id,
                request.forum_topic_id
            );
            promise.set_value(());
        }


        // Everything else waits for its manager step.
        // Everything else waits for its manager step.
        _ => run_story_requests(td, id, function),
    }
}

/// The M.47 StoryManager arm group of the request dispatcher — see the
/// frame note at the forward site in [`run_request`]. Every arm routes
/// into `td_->story_manager_->...` (Requests.cpp:3763-3797, 4059-4119,
/// 4538-4544, 4745-4830, 5927-6010); the network tails are the
/// [blocked: net] query seams of the manager, the local validation halves
/// answer inline.
#[allow(clippy::too_many_lines)]
fn run_story_requests(td: &mut Td, id: u64, function: Function) {
    /// The `td_->` slice the story arms read through (the same field-split
    /// borrow the [`crate::td::StoryEnv`] init site uses); the manager
    /// field rides the closure as its own `&mut`.
    fn story_arm(
        td: &mut Td,
        run: impl FnOnce(&mut crate::td::StoryEnv<'_>, &mut crate::story_manager::StoryManager),
    ) {
        let crate::td::Td {
            story_manager,
            global,
            option_manager,
            auth_manager,
            user_manager,
            chat_manager,
            file_manager,
            notification_updates,
            ..
        } = td;
        let mut env = crate::td::StoryEnv {
            global,
            options: option_manager.as_mut(),
            auth_is_authorized: auth_manager.is_authorized(),
            auth_is_bot: auth_manager.is_bot(),
            user_manager,
            chat_manager,
            file_manager: file_manager.clone(),
            updates: notification_updates,
        };
        run(&mut env, story_manager);
    }

    macro_rules! ok_promise {
        ($td:expr) => {
            $td.deferred_promise::<(), _>(id, |_| ok_object())
        };
    }

    match function {
        // searchPublicStoriesByTag (Requests.cpp:3763-3770)
        Function::searchPublicStoriesByTag(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(tag) = clean_or_utf8_error(td, id, &request.tag) else {
                return;
            };
            let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::FoundStories, _>(id, Object::foundStories);
            story_arm(td, |env, manager| {
                manager.search_hashtag_posts(
                    env,
                    crate::dialog_id::DialogId::new(request.story_poster_chat_id),
                    tag,
                    offset,
                    request.limit,
                    promise,
                );
            });
        }

        // searchPublicStoriesByLocation (Requests.cpp:3772-3785)
        Function::searchPublicStoriesByLocation(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(address) = request.address else {
                return td.send_error_impl(id, 400, "Address must be non-empty");
            };
            let Some(country_code) = clean_or_utf8_error(td, id, &address.country_code) else {
                return;
            };
            let Some(state) = clean_or_utf8_error(td, id, &address.state) else {
                return;
            };
            let Some(city) = clean_or_utf8_error(td, id, &address.city) else {
                return;
            };
            let Some(street) = clean_or_utf8_error(td, id, &address.street) else {
                return;
            };
            let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::FoundStories, _>(id, Object::foundStories);
            story_arm(td, |env, manager| {
                manager.search_location_posts(
                    env,
                    Some(td_api::LocationAddress {
                        country_code,
                        state,
                        city,
                        street,
                    }),
                    offset,
                    request.limit,
                    promise,
                );
            });
        }

        // searchPublicStoriesByVenue (Requests.cpp:3787-3796)
        Function::searchPublicStoriesByVenue(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(venue_provider) = clean_or_utf8_error(td, id, &request.venue_provider) else {
                return;
            };
            let Some(venue_id) = clean_or_utf8_error(td, id, &request.venue_id) else {
                return;
            };
            let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::FoundStories, _>(id, Object::foundStories);
            story_arm(td, |env, manager| {
                manager.search_venue_posts(
                    env,
                    venue_provider,
                    venue_id,
                    offset,
                    request.limit,
                    promise,
                );
            });
        }

        // getChatStoryAlbums (Requests.cpp:4059-4063)
        Function::getChatStoryAlbums(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::StoryAlbums, _>(id, Object::storyAlbums);
            story_arm(td, |env, manager| {
                manager.get_story_albums(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    promise,
                );
            });
        }

        // getStoryAlbumStories (Requests.cpp:4065-4070)
        Function::getStoryAlbumStories(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Stories, _>(id, Object::stories);
            story_arm(td, |env, manager| {
                manager.get_story_album_stories(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::story_ids::StoryAlbumId::new(request.story_album_id),
                    request.offset,
                    request.limit,
                    promise,
                );
            });
        }

        // createStoryAlbum (Requests.cpp:4072-4078)
        Function::createStoryAlbum(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::StoryAlbum, _>(id, Object::storyAlbum);
            let story_ids = crate::story_ids::StoryId::get_story_ids(&request.story_ids);
            story_arm(td, |env, manager| {
                manager.create_story_album(
                    env,
                    crate::dialog_id::DialogId::new(request.story_poster_chat_id),
                    &name,
                    &story_ids,
                    promise,
                );
            });
        }

        // reorderStoryAlbums (Requests.cpp:4080-4085)
        Function::reorderStoryAlbums(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let story_album_ids =
                crate::story_ids::StoryAlbumId::get_story_album_ids(&request.story_album_ids);
            story_arm(td, |env, manager| {
                manager.reorder_story_albums(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    &story_album_ids,
                    promise,
                );
            });
        }

        // deleteStoryAlbum (Requests.cpp:4087-4092)
        Function::deleteStoryAlbum(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            story_arm(td, |env, manager| {
                manager.delete_story_album(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::story_ids::StoryAlbumId::new(request.story_album_id),
                    promise,
                );
            });
        }

        // setStoryAlbumName (Requests.cpp:4094-4100)
        Function::setStoryAlbumName(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::StoryAlbum, _>(id, Object::storyAlbum);
            story_arm(td, |env, manager| {
                manager.set_story_album_title(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::story_ids::StoryAlbumId::new(request.story_album_id),
                    &name,
                    promise,
                );
            });
        }

        // addStoryAlbumStories (Requests.cpp:4102-4108)
        Function::addStoryAlbumStories(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::StoryAlbum, _>(id, Object::storyAlbum);
            let story_ids = crate::story_ids::StoryId::get_story_ids(&request.story_ids);
            story_arm(td, |env, manager| {
                manager.add_story_album_stories(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::story_ids::StoryAlbumId::new(request.story_album_id),
                    &story_ids,
                    promise,
                );
            });
        }

        // removeStoryAlbumStories (Requests.cpp:4110-4116)
        Function::removeStoryAlbumStories(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::StoryAlbum, _>(id, Object::storyAlbum);
            let story_ids = crate::story_ids::StoryId::get_story_ids(&request.story_ids);
            story_arm(td, |env, manager| {
                manager.remove_story_album_stories(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::story_ids::StoryAlbumId::new(request.story_album_id),
                    &story_ids,
                    promise,
                );
            });
        }

        // reorderStoryAlbumStories (Requests.cpp:4118-4120)
        Function::reorderStoryAlbumStories(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::StoryAlbum, _>(id, Object::storyAlbum);
            let story_ids = crate::story_ids::StoryId::get_story_ids(&request.story_ids);
            story_arm(td, |env, manager| {
                manager.reorder_story_album_stories(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::story_ids::StoryAlbumId::new(request.story_album_id),
                    &story_ids,
                    promise,
                );
            });
        }

        // editBusinessStory (Requests.cpp:4538-4544)
        Function::editBusinessStory(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, story_optional_to_object);
            story_arm(td, |env, manager| {
                manager.edit_business_story(
                    env,
                    crate::dialog_id::DialogId::new(request.story_poster_chat_id),
                    crate::story_ids::StoryId::new(request.story_id),
                    request.content,
                    request.areas,
                    request.caption,
                    request.privacy_settings,
                    promise,
                );
            });
        }

        // getStory (Requests.cpp:4745-4750)
        Function::getStory(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, story_optional_to_object);
            story_arm(td, |env, manager| {
                manager.get_story_request(
                    env,
                    crate::dialog_id::DialogId::new(request.story_poster_chat_id),
                    crate::story_ids::StoryId::new(request.story_id),
                    request.only_local,
                    promise,
                );
            });
        }

        // getChatsToPostStories (Requests.cpp:4752-4756)
        Function::getChatsToPostStories(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Chats, _>(id, Object::chats);
            story_arm(td, |env, manager| {
                manager.get_dialogs_to_send_stories(env, promise);
            });
        }

        // canPostStory (Requests.cpp:4758-4762)
        Function::canPostStory(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, can_post_story_result_to_object);
            story_arm(td, |env, manager| {
                manager.can_send_story(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    promise,
                );
            });
        }

        // postStory (Requests.cpp:4763-4771)
        Function::postStory(request) => {
            let promise = td.deferred_promise(id, story_optional_to_object);
            let story_album_ids =
                crate::story_ids::StoryAlbumId::get_story_album_ids(&request.album_ids);
            story_arm(td, |env, manager| {
                manager.send_story(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    request.content,
                    request.areas,
                    request.caption,
                    request.privacy_settings,
                    story_album_ids,
                    request.active_period,
                    request.from_story_full_id,
                    request.is_posted_to_chat_page,
                    request.protect_content,
                    promise,
                );
            });
        }

        // startLiveStory (Requests.cpp:4773-4779)
        Function::startLiveStory(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, start_live_story_result_to_object);
            story_arm(td, |env, manager| {
                manager.start_live_story(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    request.privacy_settings,
                    false,
                    request.protect_content,
                    request.is_rtmp_stream,
                    request.enable_messages,
                    request.paid_message_star_count,
                    promise,
                );
            });
        }

        // editStory (Requests.cpp:4781-4787)
        Function::editStory(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            story_arm(td, |env, manager| {
                manager.edit_story(
                    env,
                    crate::dialog_id::DialogId::new(request.story_poster_chat_id),
                    crate::story_ids::StoryId::new(request.story_id),
                    request.content,
                    request.areas,
                    request.caption,
                    promise,
                );
            });
        }

        // editStoryCover (Requests.cpp:4789-4794)
        Function::editStoryCover(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            story_arm(td, |env, manager| {
                manager.edit_story_cover(
                    env,
                    crate::dialog_id::DialogId::new(request.story_poster_chat_id),
                    crate::story_ids::StoryId::new(request.story_id),
                    request.cover_frame_timestamp,
                    promise,
                );
            });
        }

        // setStoryPrivacySettings (Requests.cpp:4796-4801)
        Function::setStoryPrivacySettings(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            story_arm(td, |env, manager| {
                manager.set_story_privacy_settings(
                    env,
                    crate::story_ids::StoryId::new(request.story_id),
                    request.privacy_settings,
                    promise,
                );
            });
        }

        // toggleStoryIsPostedToChatPage (Requests.cpp:4803-4808)
        Function::toggleStoryIsPostedToChatPage(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            story_arm(td, |env, manager| {
                manager.toggle_story_is_pinned(
                    env,
                    crate::dialog_id::DialogId::new(request.story_poster_chat_id),
                    crate::story_ids::StoryId::new(request.story_id),
                    request.is_posted_to_chat_page,
                    promise,
                );
            });
        }

        // deleteStory (Requests.cpp:4810-4815)
        Function::deleteStory(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            story_arm(td, |env, manager| {
                manager.delete_story(
                    env,
                    crate::dialog_id::DialogId::new(request.story_poster_chat_id),
                    crate::story_ids::StoryId::new(request.story_id),
                    promise,
                );
            });
        }

        // loadActiveStories (Requests.cpp:4817-4821)
        Function::loadActiveStories(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let story_list_id =
                crate::story_ids::StoryListId::from_story_list_object(request.story_list.as_ref())
                    .unwrap_or(crate::story_ids::StoryListId::None);
            story_arm(td, |env, manager| {
                manager.load_active_stories(env, story_list_id, promise);
            });
        }

        // setChatActiveStoriesList (Requests.cpp:4823-4827)
        Function::setChatActiveStoriesList(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let story_list_id =
                crate::story_ids::StoryListId::from_story_list_object(request.story_list.as_ref())
                    .unwrap_or(crate::story_ids::StoryListId::None);
            story_arm(td, |env, manager| {
                manager.toggle_dialog_stories_hidden(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    story_list_id,
                    promise,
                );
            });
        }

        // getChatActiveStories (Requests.cpp:5927-5931)
        Function::getChatActiveStories(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::ChatActiveStories, _>(id, Object::chatActiveStories);
            story_arm(td, |env, manager| {
                manager.get_dialog_expiring_stories(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    promise,
                );
            });
        }

        // getChatPostedToChatPageStories (Requests.cpp:5933-5938)
        Function::getChatPostedToChatPageStories(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Stories, _>(id, Object::stories);
            story_arm(td, |env, manager| {
                manager.get_dialog_pinned_stories(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::story_ids::StoryId::new(request.from_story_id),
                    request.limit,
                    promise,
                );
            });
        }

        // getChatArchivedStories (Requests.cpp:5940-5945)
        Function::getChatArchivedStories(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Stories, _>(id, Object::stories);
            story_arm(td, |env, manager| {
                manager.get_story_archive(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::story_ids::StoryId::new(request.from_story_id),
                    request.limit,
                    promise,
                );
            });
        }

        // setChatPinnedStories (Requests.cpp:5947-5952)
        Function::setChatPinnedStories(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let story_ids = crate::story_ids::StoryId::get_story_ids(&request.story_ids);
            story_arm(td, |env, manager| {
                manager.set_pinned_stories(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    story_ids,
                    promise,
                );
            });
        }

        // openStory (Requests.cpp:5954-5958)
        Function::openStory(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            story_arm(td, |env, manager| {
                manager.open_story(
                    env,
                    crate::dialog_id::DialogId::new(request.story_poster_chat_id),
                    crate::story_ids::StoryId::new(request.story_id),
                    promise,
                );
            });
        }

        // closeStory (Requests.cpp:5960-5964)
        Function::closeStory(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            story_arm(td, |env, manager| {
                manager.close_story(
                    env,
                    crate::dialog_id::DialogId::new(request.story_poster_chat_id),
                    crate::story_ids::StoryId::new(request.story_id),
                    promise,
                );
            });
        }

        // setStoryReaction (Requests.cpp:5973-5979)
        Function::setStoryReaction(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let reaction_type =
                crate::reaction_type::ReactionType::from_td_api(request.reaction_type.as_ref());
            story_arm(td, |env, manager| {
                manager.set_story_reaction(
                    env,
                    crate::story_ids::StoryFullId::new(
                        crate::dialog_id::DialogId::new(request.story_poster_chat_id),
                        crate::story_ids::StoryId::new(request.story_id),
                    ),
                    reaction_type,
                    request.update_recent_reactions,
                    promise,
                );
            });
        }

        // getStoryInteractions (Requests.cpp:5983-5990)
        Function::getStoryInteractions(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::StoryInteractions, _>(id, Object::storyInteractions);
            story_arm(td, |env, manager| {
                manager.get_story_interactions(
                    env,
                    crate::story_ids::StoryId::new(request.story_id),
                    &query,
                    request.only_contacts,
                    request.prefer_forwards,
                    request.prefer_with_reaction,
                    &offset,
                    request.limit,
                    promise,
                );
            });
        }

        // getChatStoryInteractions (Requests.cpp:5992-5999)
        Function::getChatStoryInteractions(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::StoryInteractions, _>(id, Object::storyInteractions);
            let reaction_type =
                crate::reaction_type::ReactionType::from_td_api(request.reaction_type.as_ref());
            story_arm(td, |env, manager| {
                manager.get_dialog_story_interactions(
                    env,
                    crate::story_ids::StoryFullId::new(
                        crate::dialog_id::DialogId::new(request.story_poster_chat_id),
                        crate::story_ids::StoryId::new(request.story_id),
                    ),
                    reaction_type,
                    request.prefer_forwards,
                    &offset,
                    request.limit,
                    promise,
                );
            });
        }

        // reportStory (Requests.cpp:6001-6006)
        Function::reportStory(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(text) = clean_or_utf8_error(td, id, &request.text) else {
                return;
            };
            let promise = td.deferred_promise(id, report_story_result_to_object);
            let option_id = String::from_utf8_lossy(&request.option_id).into_owned();
            story_arm(td, |env, manager| {
                manager.report_story(
                    env,
                    crate::story_ids::StoryFullId::new(
                        crate::dialog_id::DialogId::new(request.story_poster_chat_id),
                        crate::story_ids::StoryId::new(request.story_id),
                    ),
                    &option_id,
                    &text,
                    promise,
                );
            });
        }

        // activateStoryStealthMode (Requests.cpp:6007-6009)
        Function::activateStoryStealthMode(_) => {
            let promise = ok_promise!(td);
            story_arm(td, |env, manager| {
                manager.activate_stealth_mode(env, promise);
            });
        }

        // Everything else waits for its manager step.
        Function::getSupportUser(_) => {
            // MessageEntity / UserFull ProfileTab / DialogManager / the
            // missing setter halves (M.4.12 deferred them with their types)
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }

        // --------------------------------------------------------------
        // M.44: the SecureManager-served passport requests
        // (Requests.cpp:8772-8830, :8854-8891).
        Function::getPassportElement(request) => {
            // Requests.cpp:8772-8781
            if !check_is_user(td, id) {
                return;
            }
            let Some(password) = clean_or_utf8_error(td, id, &request.password) else {
                return;
            };
            let Some(request_type) = &request.r#type else {
                return td.send_error_impl(id, 400, "Type must be non-empty");
            };
            let promise =
                td.deferred_promise(id, |element: crate::secure_manager::TdApiSecureValue| {
                    match element {
                        // the PassportElement umbrella flattens into the
                        // per-ctor Object variants (the `From` impl)
                        Some(element) => Object::from(element),
                        // the C++ nullptr result rides `Td::send_result`'s
                        // 404 "Not Found" rewrite (Td.cpp:1407-1409)
                        None => Object::error(td_api::Error {
                            code: 404,
                            message: "Not Found".to_string(),
                        }),
                    }
                });
            let mut env = secure_env!(td);
            td.secure_manager.get_secure_value(
                &mut env,
                &password,
                crate::secure_value::get_secure_value_type_td_api(request_type),
                promise,
            );
        }
        Function::getAllPassportElements(request) => {
            // Requests.cpp:8783-8789
            if !check_is_user(td, id) {
                return;
            }
            let Some(password) = clean_or_utf8_error(td, id, &request.password) else {
                return;
            };
            let promise =
                td.deferred_promise(id, |elements: crate::secure_manager::TdApiSecureValues| {
                    Object::passportElements(elements)
                });
            let mut env = secure_env!(td);
            td.secure_manager
                .get_all_secure_values(&mut env, &password, promise);
        }
        Function::setPassportElement(request) => {
            // Requests.cpp:8791-8801: the inline `get_secure_value` parse
            // answers 400 before the promise is created
            if !check_is_user(td, id) {
                return;
            }
            let Some(password) = clean_or_utf8_error(td, id, &request.password) else {
                return;
            };
            let r_secure_value = {
                let mut env = secure_env!(td);
                crate::secure_value::get_secure_value(&mut env, request.element)
            };
            let secure_value = match r_secure_value {
                Result::Ok(secure_value) => secure_value,
                Result::Err(error) => {
                    return td.send_error_impl(id, 400, error.message());
                }
            };
            let promise =
                td.deferred_promise(id, |element: crate::secure_manager::TdApiSecureValue| {
                    match element {
                        Some(element) => Object::from(element),
                        None => Object::error(td_api::Error {
                            code: 404,
                            message: "Not Found".to_string(),
                        }),
                    }
                });
            let mut env = secure_env!(td);
            td.secure_manager
                .set_secure_value(&mut env, &password, secure_value, promise);
        }
        Function::deletePassportElement(request) => {
            // Requests.cpp:8803-8811
            if !check_is_user(td, id) {
                return;
            }
            let Some(request_type) = &request.r#type else {
                return td.send_error_impl(id, 400, "Type must be non-empty");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = secure_env!(td);
            td.secure_manager.delete_secure_value(
                &mut env,
                crate::secure_value::get_secure_value_type_td_api(request_type),
                promise,
            );
        }
        Function::setPassportElementErrors(request) => {
            // Requests.cpp:8813-8822
            if !check_is_bot(td, id) {
                return;
            }
            let r_input_user = {
                let env = user_env!(td);
                td.user_manager
                    .get_input_user(&env, UserId::new(request.user_id))
            };
            if let Err(error) = r_input_user {
                return td.send_error_impl(id, error.code(), error.message());
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = secure_env!(td);
            td.secure_manager.set_secure_value_errors(
                &mut env,
                r_input_user.unwrap(),
                request.errors,
                promise,
            );
        }
        Function::getPreferredCountryLanguage(request) => {
            // Requests.cpp:8824-8830
            if !check_is_user(td, id) {
                return;
            }
            let Some(country_code) = clean_or_utf8_error(td, id, &request.country_code) else {
                return;
            };
            let promise = td.deferred_promise(id, |text: Option<td_api::Text>| match text {
                Some(text) => Object::text(text),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_string(),
                }),
            });
            let mut env = secure_env!(td);
            td.secure_manager
                .get_preferred_country_language(&mut env, &country_code, promise);
        }
        Function::getPassportAuthorizationForm(request) => {
            // Requests.cpp:8854-8870
            if !check_is_user(td, id) {
                return;
            }
            let Some(public_key) = clean_or_utf8_error(td, id, &request.public_key) else {
                return;
            };
            let Some(scope) = clean_or_utf8_error(td, id, &request.scope) else {
                return;
            };
            let Some(nonce) = clean_or_utf8_error(td, id, &request.nonce) else {
                return;
            };
            let bot_user_id = UserId::new(request.bot_user_id);
            if !bot_user_id.is_valid() {
                return td.send_error_impl(id, 400, "Bot user identifier invalid");
            }
            if nonce.is_empty() {
                return td.send_error_impl(id, 400, "Nonce must be non-empty");
            }
            let promise =
                td.deferred_promise(id, |form: crate::secure_manager::TdApiAuthorizationForm| {
                    Object::passportAuthorizationForm(form)
                });
            let mut env = secure_env!(td);
            td.secure_manager.get_passport_authorization_form(
                &mut env,
                bot_user_id,
                &scope,
                &public_key,
                &nonce,
                promise,
            );
        }
        Function::getPassportAuthorizationFormAvailableElements(request) => {
            // Requests.cpp:8872-8878
            if !check_is_user(td, id) {
                return;
            }
            let Some(password) = clean_or_utf8_error(td, id, &request.password) else {
                return;
            };
            let promise = td.deferred_promise(
                id,
                |elements: crate::secure_manager::TdApiSecureValuesWithErrors| {
                    Object::passportElementsWithErrors(elements)
                },
            );
            let mut env = secure_env!(td);
            td.secure_manager
                .get_passport_authorization_form_available_elements(
                    &mut env,
                    request.authorization_form_id,
                    &password,
                    promise,
                );
        }
        Function::sendPassportAuthorizationForm(request) => {
            // Requests.cpp:8880-8891
            if !check_is_user(td, id) {
                return;
            }
            // the C++ null-element walk (Requests.cpp:8882-8886) cannot
            // fire: the generated vector holds plain `PassportElementType`
            let types = crate::secure_value::get_secure_value_types_td_api(&request.types);
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = secure_env!(td);
            td.secure_manager.send_passport_authorization_form(
                &mut env,
                request.authorization_form_id,
                types,
                promise,
            );
        }

        // Everything else waits for its manager step.
        // Everything else waits for its manager step.
        // Everything else waits for its manager step; the M.42 star gift
        // group follows in its own frame (see the frame note above).
        _ => run_star_gift_requests(td, id, function),
    }
}

/// The `StarGiftTdEnv` for the star gift manager calls. Expanded inline
/// (the same field-disjoint shape as `user_env!`); `get_my_dialog_id`
/// reads through a shared reborrow before the mutable fields split off.
macro_rules! star_gift_env {
    ($td:expr) => {{
        let my_dialog_id = crate::dialog_manager::get_my_dialog_id($td);
        crate::td::StarGiftTdEnv {
            global: &mut $td.global,
            options: $td.option_manager.as_mut(),
            auth_is_authorized: $td.auth_manager.is_authorized(),
            auth_is_bot: $td.auth_manager.is_bot(),
            user_manager: &mut $td.user_manager,
            chat_manager: &mut $td.chat_manager,
            my_dialog_id,
            updates: &mut $td.notification_updates,
        }
    }};
}

/// The `CanSendGiftResult` → `Object` rewrap (the Object umbrella has no
/// single variant for the two-ctor type).
fn can_send_gift_result_to_object(value: td_api::CanSendGiftResult) -> Object {
    match value {
        td_api::CanSendGiftResult::canSendGiftResultOk(value) => Object::canSendGiftResultOk(value),
        td_api::CanSendGiftResult::canSendGiftResultFail(value) => {
            Object::canSendGiftResultFail(value)
        }
    }
}

/// The `CraftGiftResult` → `Object` rewrap.
fn craft_gift_result_to_object(value: td_api::CraftGiftResult) -> Object {
    match value {
        td_api::CraftGiftResult::craftGiftResultSuccess(value) => {
            Object::craftGiftResultSuccess(value)
        }
        td_api::CraftGiftResult::craftGiftResultTooEarly(value) => {
            Object::craftGiftResultTooEarly(value)
        }
        td_api::CraftGiftResult::craftGiftResultInvalidGift(value) => {
            Object::craftGiftResultInvalidGift(value)
        }
        td_api::CraftGiftResult::craftGiftResultFail(value) => Object::craftGiftResultFail(value),
    }
}

/// The `GiftResaleResult` → `Object` rewrap.
fn gift_resale_result_to_object(value: td_api::GiftResaleResult) -> Object {
    match value {
        td_api::GiftResaleResult::giftResaleResultOk(value) => Object::giftResaleResultOk(value),
        td_api::GiftResaleResult::giftResaleResultPriceIncreased(value) => {
            Object::giftResaleResultPriceIncreased(value)
        }
    }
}

/// The M.42 StarGiftManager arm group of the request dispatcher — see the
/// frame note at the forward site in [`run_request`]. Every arm routes
/// into `td_->star_gift_manager_->...` (Requests.cpp:8455-8756); the
/// network tails are the [blocked: net] query seams of the manager, the
/// local validation halves answer inline.
#[allow(clippy::too_many_lines)]
fn run_star_gift_requests(td: &mut Td, id: u64, function: Function) {
    // `get_message_sender_dialog_id(td_, sender, true, false)` for the
    // owner arms: the owner is parsed in one env, the manager runs in the
    // next (the two-phase borrow of the collapsed dispatcher)
    macro_rules! owner_dialog_id {
        ($td:expr, $sender:expr) => {
            get_message_sender_dialog_id(&mut star_gift_env!($td), $sender.as_ref())
        };
    }

    macro_rules! ok_promise {
        ($td:expr) => {
            $td.deferred_promise::<(), _>(id, |_| ok_object())
        };
    }

    match function {
        // getAvailableGifts (Requests.cpp:8455-8458)
        Function::getAvailableGifts(_) => {
            let promise =
                td.deferred_promise::<td_api::AvailableGifts, _>(id, Object::availableGifts);
            let mut env = star_gift_env!(td);
            td.star_gift_manager
                .get_gift_payment_options(&mut env, promise);
        }

        // canSendGift (Requests.cpp:8460-8463)
        Function::canSendGift(request) => {
            let promise = td.deferred_promise::<td_api::CanSendGiftResult, _>(
                id,
                can_send_gift_result_to_object,
            );
            let mut env = star_gift_env!(td);
            td.star_gift_manager
                .can_send_gift(&mut env, request.gift_id, promise);
        }

        // sendGift (Requests.cpp:8465-8470)
        Function::sendGift(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let owner_dialog_id = match owner_dialog_id!(td, request.owner_id) {
                std::result::Result::Ok(dialog_id) => dialog_id,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.send_gift(
                &mut env,
                request.gift_id,
                owner_dialog_id,
                request.text,
                request.is_private,
                request.pay_for_upgrade,
                promise,
            );
        }

        // getGiftAuctionState (Requests.cpp:8472-8477)
        Function::getGiftAuctionState(request) => {
            let promise =
                td.deferred_promise::<td_api::GiftAuctionState, _>(id, Object::giftAuctionState);
            let mut env = star_gift_env!(td);
            td.star_gift_manager
                .get_gift_auction_state(&mut env, &request.auction_id, promise);
        }

        // getGiftAuctionAcquiredGifts (Requests.cpp:8479-8483)
        Function::getGiftAuctionAcquiredGifts(request) => {
            let promise = td.deferred_promise::<td_api::GiftAuctionAcquiredGifts, _>(
                id,
                Object::giftAuctionAcquiredGifts,
            );
            let mut env = star_gift_env!(td);
            td.star_gift_manager.get_gift_auction_acquired_gifts(
                &mut env,
                request.gift_id,
                promise,
            );
        }

        // openGiftAuction (Requests.cpp:8485-8489)
        Function::openGiftAuction(request) => {
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            td.star_gift_manager
                .open_gift_auction(&mut env, request.gift_id, false, promise);
        }

        // closeGiftAuction (Requests.cpp:8491-8495)
        Function::closeGiftAuction(request) => {
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            {
                let _ = &mut env;
                td.star_gift_manager
                    .close_gift_auction(request.gift_id, promise);
            }
        }

        // placeGiftAuctionBid (Requests.cpp:8497-8502)
        Function::placeGiftAuctionBid(request) => {
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.place_gift_auction_bid(
                &mut env,
                request.gift_id,
                request.star_count,
                UserId::new(request.user_id),
                request.text,
                request.is_private,
                promise,
            );
        }

        // increaseGiftAuctionBid (Requests.cpp:8504-8508)
        Function::increaseGiftAuctionBid(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.update_gift_auction_bid(
                &mut env,
                request.gift_id,
                request.star_count,
                promise,
            );
        }

        // sellGift (Requests.cpp:8510-8515)
        Function::sellGift(request) => {
            if td.auth_manager.is_bot() && request.business_connection_id.is_empty() {
                return td.send_error_impl(id, 400, "The method is not available to bots");
            }
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.convert_gift(
                &mut env,
                crate::star_gift_manager::BusinessConnectionId::new(
                    &request.business_connection_id,
                ),
                StarGiftId::from_string(&request.received_gift_id),
                promise,
            );
        }

        // toggleGiftIsSaved (Requests.cpp:8517-8521)
        Function::toggleGiftIsSaved(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.save_gift(
                &mut env,
                StarGiftId::from_string(&request.received_gift_id),
                request.is_saved,
                promise,
            );
        }

        // setPinnedGifts (Requests.cpp:8523-8529)
        Function::setPinnedGifts(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let owner_dialog_id = match owner_dialog_id!(td, request.owner_id) {
                std::result::Result::Ok(dialog_id) => dialog_id,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.set_dialog_pinned_gifts(
                &mut env,
                owner_dialog_id,
                StarGiftId::get_star_gift_ids(&request.received_gift_ids),
                promise,
            );
        }

        // toggleChatGiftNotifications (Requests.cpp:8531-8536)
        Function::toggleChatGiftNotifications(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.toggle_chat_star_gift_notifications(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.are_enabled,
                promise,
            );
        }

        // getGiftUpgradePreview (Requests.cpp:8538-8542)
        Function::getGiftUpgradePreview(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td
                .deferred_promise::<td_api::GiftUpgradePreview, _>(id, Object::giftUpgradePreview);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.get_gift_upgrade_preview(
                &mut env,
                request.regular_gift_id,
                promise,
            );
        }

        // getUpgradedGiftVariants (Requests.cpp:8544-8549)
        Function::getUpgradedGiftVariants(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::GiftUpgradeVariants, _>(
                id,
                Object::giftUpgradeVariants,
            );
            let mut env = star_gift_env!(td);
            td.star_gift_manager.get_gift_upgrade_variants(
                &mut env,
                request.regular_gift_id,
                request.return_upgrade_models,
                request.return_craft_models,
                promise,
            );
        }

        // upgradeGift (Requests.cpp:8551-8557)
        Function::upgradeGift(request) => {
            if td.auth_manager.is_bot() && request.business_connection_id.is_empty() {
                return td.send_error_impl(id, 400, "The method is not available to bots");
            }
            let promise =
                td.deferred_promise::<td_api::UpgradeGiftResult, _>(id, Object::upgradeGiftResult);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.upgrade_gift(
                &mut env,
                crate::star_gift_manager::BusinessConnectionId::new(
                    &request.business_connection_id,
                ),
                StarGiftId::from_string(&request.received_gift_id),
                request.keep_original_details,
                request.star_count,
                promise,
            );
        }

        // buyGiftUpgrade (Requests.cpp:8559-8566)
        Function::buyGiftUpgrade(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(prepaid_upgrade_hash) =
                clean_or_utf8_error(td, id, &request.prepaid_upgrade_hash)
            else {
                return;
            };
            let owner_dialog_id = match owner_dialog_id!(td, request.owner_id) {
                std::result::Result::Ok(dialog_id) => dialog_id,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.buy_gift_upgrade(
                &mut env,
                owner_dialog_id,
                &prepaid_upgrade_hash,
                request.star_count,
                promise,
            );
        }

        // craftGift (Requests.cpp:8568-8572)
        Function::craftGift(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::CraftGiftResult, _>(id, craft_gift_result_to_object);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.craft_gift(
                &mut env,
                StarGiftId::get_star_gift_ids(&request.received_gift_ids),
                promise,
            );
        }

        // transferGift (Requests.cpp:8574-8581)
        Function::transferGift(request) => {
            if td.auth_manager.is_bot() && request.business_connection_id.is_empty() {
                return td.send_error_impl(id, 400, "The method is not available to bots");
            }
            let new_owner_dialog_id = match owner_dialog_id!(td, request.new_owner_id) {
                std::result::Result::Ok(dialog_id) => dialog_id,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.transfer_gift(
                &mut env,
                crate::star_gift_manager::BusinessConnectionId::new(
                    &request.business_connection_id,
                ),
                StarGiftId::from_string(&request.received_gift_id),
                new_owner_dialog_id,
                request.star_count,
                promise,
            );
        }

        // dropGiftOriginalDetails (Requests.cpp:8583-8587)
        Function::dropGiftOriginalDetails(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.drop_gift_original_details(
                &mut env,
                StarGiftId::from_string(&request.received_gift_id),
                request.star_count,
                promise,
            );
        }

        // sendResoldGift (Requests.cpp:8589-8599)
        Function::sendResoldGift(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(gift_name) = clean_or_utf8_error(td, id, &request.gift_name) else {
                return;
            };
            let owner_dialog_id = match owner_dialog_id!(td, request.owner_id) {
                std::result::Result::Ok(dialog_id) => dialog_id,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let price = {
                let mut env = star_gift_env!(td);
                StarGiftResalePrice::get_star_gift_resale_price(&mut env, request.price, true)
            };
            let price = match price {
                std::result::Result::Ok(price) => price,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let promise = td
                .deferred_promise::<td_api::GiftResaleResult, _>(id, gift_resale_result_to_object);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.send_resold_gift(
                &mut env,
                &gift_name,
                owner_dialog_id,
                price,
                request.text,
                request.is_private,
                promise,
            );
        }

        // sendGiftPurchaseOffer (Requests.cpp:8601-8609)
        Function::sendGiftPurchaseOffer(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(gift_name) = clean_or_utf8_error(td, id, &request.gift_name) else {
                return;
            };
            let owner_dialog_id = match owner_dialog_id!(td, request.owner_id) {
                std::result::Result::Ok(dialog_id) => dialog_id,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let price = {
                let mut env = star_gift_env!(td);
                StarGiftResalePrice::get_star_gift_resale_price(&mut env, request.price, false)
            };
            let price = match price {
                std::result::Result::Ok(price) => price,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.send_gift_offer(
                &mut env,
                owner_dialog_id,
                &gift_name,
                price,
                request.duration,
                request.paid_message_star_count,
                promise,
            );
        }

        // processGiftPurchaseOffer (Requests.cpp:8611-8616)
        Function::processGiftPurchaseOffer(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.process_gift_offer(
                &mut env,
                request.message_id,
                !request.accept,
                promise,
            );
        }

        // getReceivedGifts (Requests.cpp:8618-8629)
        Function::getReceivedGifts(request) => {
            let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let owner_dialog_id = match owner_dialog_id!(td, request.owner_id) {
                std::result::Result::Ok(dialog_id) => dialog_id,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let promise =
                td.deferred_promise::<td_api::ReceivedGifts, _>(id, Object::receivedGifts);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.get_saved_star_gifts(
                &mut env,
                crate::star_gift_manager::BusinessConnectionId::new(
                    &request.business_connection_id,
                ),
                owner_dialog_id,
                StarGiftCollectionId::new(request.collection_id),
                request.exclude_unsaved,
                request.exclude_saved,
                request.exclude_unlimited,
                request.exclude_upgradable,
                request.exclude_non_upgradable,
                request.exclude_upgraded,
                request.exclude_without_colors,
                request.exclude_hosted,
                request.sort_by_price,
                &offset,
                request.limit,
                promise,
            );
        }

        // getReceivedGift (Requests.cpp:8631-8634)
        Function::getReceivedGift(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::ReceivedGift, _>(id, Object::receivedGift);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.get_saved_star_gift(
                &mut env,
                StarGiftId::from_string(&request.received_gift_id),
                promise,
            );
        }

        // getGiftsForCrafting (Requests.cpp:8636-8642)
        Function::getGiftsForCrafting(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::GiftsForCrafting, _>(id, Object::giftsForCrafting);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.get_craft_star_gifts(
                &mut env,
                request.regular_gift_id,
                &offset,
                request.limit,
                promise,
            );
        }

        // getUpgradedGift (Requests.cpp:8644-8649)
        Function::getUpgradedGift(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::UpgradedGift, _>(id, Object::upgradedGift);
            let mut env = star_gift_env!(td);
            td.star_gift_manager
                .get_upgraded_gift(&mut env, &name, promise);
        }

        // getUpgradedGiftValueInfo (Requests.cpp:8651-8656)
        Function::getUpgradedGiftValueInfo(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::UpgradedGiftValueInfo, _>(
                id,
                Object::upgradedGiftValueInfo,
            );
            let mut env = star_gift_env!(td);
            td.star_gift_manager
                .get_upgraded_gift_value_info(&mut env, &name, promise);
        }

        // getUpgradedGiftWithdrawalUrl (Requests.cpp:8658-8662)
        Function::getUpgradedGiftWithdrawalUrl(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td
                .deferred_promise::<String, _>(id, |url| Object::httpUrl(td_api::HttpUrl { url }));
            let mut env = star_gift_env!(td);
            td.star_gift_manager.get_star_gift_withdrawal_url(
                &mut env,
                StarGiftId::from_string(&request.received_gift_id),
                &request.password,
                promise,
            );
        }

        // getUpgradedGiftsPromotionalAnimation (Requests.cpp:8664-8668)
        Function::getUpgradedGiftsPromotionalAnimation(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Animation, _>(id, Object::animation);
            let mut env = star_gift_env!(td);
            td.star_gift_manager
                .get_star_gift_promo_animation(&mut env, promise);
        }

        // setGiftResalePrice (Requests.cpp:8670-8678)
        Function::setGiftResalePrice(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let price = {
                let mut env = star_gift_env!(td);
                StarGiftResalePrice::get_star_gift_resale_price(&mut env, request.price, false)
            };
            let price = match price {
                std::result::Result::Ok(price) => price,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.set_star_gift_price(
                &mut env,
                StarGiftId::from_string(&request.received_gift_id),
                price,
                promise,
            );
        }

        // searchGiftsForResale (Requests.cpp:8680-8687)
        Function::searchGiftsForResale(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::GiftsForResale, _>(id, Object::giftsForResale);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.get_resale_star_gifts(
                &mut env,
                request.gift_id,
                request.order.as_ref(),
                request.for_crafting,
                request.for_stars,
                &request.attributes,
                &offset,
                request.limit,
                promise,
            );
        }

        // getGiftCollections (Requests.cpp:8689-8694)
        Function::getGiftCollections(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let owner_dialog_id = match owner_dialog_id!(td, request.owner_id) {
                std::result::Result::Ok(dialog_id) => dialog_id,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let promise =
                td.deferred_promise::<td_api::GiftCollections, _>(id, Object::giftCollections);
            let mut env = star_gift_env!(td);
            td.star_gift_manager
                .get_gift_collections(&mut env, owner_dialog_id, promise);
        }

        // createGiftCollection (Requests.cpp:8696-8703)
        Function::createGiftCollection(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let owner_dialog_id = match owner_dialog_id!(td, request.owner_id) {
                std::result::Result::Ok(dialog_id) => dialog_id,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let promise =
                td.deferred_promise::<td_api::GiftCollection, _>(id, Object::giftCollection);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.create_gift_collection(
                &mut env,
                owner_dialog_id,
                &name,
                StarGiftId::get_star_gift_ids(&request.received_gift_ids),
                promise,
            );
        }

        // reorderGiftCollections (Requests.cpp:8705-8711)
        Function::reorderGiftCollections(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let owner_dialog_id = match owner_dialog_id!(td, request.owner_id) {
                std::result::Result::Ok(dialog_id) => dialog_id,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.reorder_gift_collections(
                &mut env,
                owner_dialog_id,
                StarGiftCollectionId::get_star_gift_collection_ids(&request.collection_ids),
                promise,
            );
        }

        // deleteGiftCollection (Requests.cpp:8713-8719)
        Function::deleteGiftCollection(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let owner_dialog_id = match owner_dialog_id!(td, request.owner_id) {
                std::result::Result::Ok(dialog_id) => dialog_id,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let promise = ok_promise!(td);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.delete_gift_collection(
                &mut env,
                owner_dialog_id,
                StarGiftCollectionId::new(request.collection_id),
                promise,
            );
        }

        // setGiftCollectionName (Requests.cpp:8721-8728)
        Function::setGiftCollectionName(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let owner_dialog_id = match owner_dialog_id!(td, request.owner_id) {
                std::result::Result::Ok(dialog_id) => dialog_id,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let promise =
                td.deferred_promise::<td_api::GiftCollection, _>(id, Object::giftCollection);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.set_gift_collection_title(
                &mut env,
                owner_dialog_id,
                StarGiftCollectionId::new(request.collection_id),
                &name,
                promise,
            );
        }

        // addGiftCollectionGifts (Requests.cpp:8730-8737)
        Function::addGiftCollectionGifts(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let owner_dialog_id = match owner_dialog_id!(td, request.owner_id) {
                std::result::Result::Ok(dialog_id) => dialog_id,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let promise =
                td.deferred_promise::<td_api::GiftCollection, _>(id, Object::giftCollection);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.add_gift_collection_gifts(
                &mut env,
                owner_dialog_id,
                StarGiftCollectionId::new(request.collection_id),
                StarGiftId::get_star_gift_ids(&request.received_gift_ids),
                promise,
            );
        }

        // removeGiftCollectionGifts (Requests.cpp:8739-8746)
        Function::removeGiftCollectionGifts(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let owner_dialog_id = match owner_dialog_id!(td, request.owner_id) {
                std::result::Result::Ok(dialog_id) => dialog_id,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let promise =
                td.deferred_promise::<td_api::GiftCollection, _>(id, Object::giftCollection);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.remove_gift_collection_gifts(
                &mut env,
                owner_dialog_id,
                StarGiftCollectionId::new(request.collection_id),
                StarGiftId::get_star_gift_ids(&request.received_gift_ids),
                promise,
            );
        }

        // reorderGiftCollectionGifts (Requests.cpp:8748-8756)
        Function::reorderGiftCollectionGifts(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let owner_dialog_id = match owner_dialog_id!(td, request.owner_id) {
                std::result::Result::Ok(dialog_id) => dialog_id,
                std::result::Result::Err(status) => return send_error_status(td, id, status),
            };
            let promise =
                td.deferred_promise::<td_api::GiftCollection, _>(id, Object::giftCollection);
            let mut env = star_gift_env!(td);
            td.star_gift_manager.reorder_gift_collection_gifts(
                &mut env,
                owner_dialog_id,
                StarGiftCollectionId::new(request.collection_id),
                StarGiftId::get_star_gift_ids(&request.received_gift_ids),
                promise,
            );
        }

        // the group only carries the star gift arms

        // Everything else waits for its manager step.
        // --- themes (Requests.cpp:5838-5850) ---
        Function::getGiftChatThemes(request) => {
            if !check_is_user(td, id) {
                return;
            }
            // CLEAN_INPUT_STRING(request.offset_) (Requests.cpp:5841)
            let Some(offset) = crate::misc::clean_input_string(&request.offset) else {
                td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
                return;
            };
            let promise =
                td.deferred_promise(id, |b: td_api::GiftChatThemes| Object::giftChatThemes(b));
            let mut env = ThemeEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                file_manager: std::sync::Arc::clone(&td.file_manager),
                updates: &mut td.theme_updates,
            };
            td.theme_manager
                .get_unique_gift_chat_themes(&mut env, &offset, request.limit, promise);
        }
        Function::setChatTheme(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let mut promise = td.deferred_promise(id, |()| ok_object());
            // MessagesManager::set_dialog_theme (MessagesManager.cpp:30479-30526)
            let mut dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            if let Result::Err(status) = crate::dialog_manager::check_dialog_access(
                td,
                dialog_id,
                true,
                crate::user_manager::AccessRights::Write,
                "set_dialog_theme",
            ) {
                promise.set_error(status);
                return;
            }
            let mut is_gift = false;
            let mut theme_name = String::new();
            if let Some(theme) = request.theme.as_ref() {
                match theme {
                    crate::td_api::InputChatTheme::inputChatThemeEmoji(theme) => {
                        theme_name = theme.name.clone();
                    }
                    crate::td_api::InputChatTheme::inputChatThemeGift(theme) => {
                        theme_name = theme.name.clone();
                        is_gift = true;
                    }
                }
                if theme_name.is_empty() {
                    promise.set_error(tdutils::status::Status::error(
                        400,
                        "Invalid theme name specified",
                    ));
                    return;
                }
            }
            let Some(theme_name) = crate::misc::clean_input_string(&theme_name) else {
                promise.set_error(tdutils::status::Status::error(
                    400,
                    "Strings must be encoded in UTF-8",
                ));
                return;
            };
            match dialog_id.get_type() {
                crate::dialog_id::DialogType::User => {}
                crate::dialog_id::DialogType::Chat | crate::dialog_id::DialogType::Channel => {
                    promise.set_error(tdutils::status::Status::error(
                        400,
                        "Can't change theme in the chat",
                    ));
                    return;
                }
                crate::dialog_id::DialogType::SecretChat => {
                    let user_id = td
                        .user_manager
                        .get_secret_chat_user_id(dialog_id.get_secret_chat_id());
                    if !user_id.is_valid() {
                        promise.set_error(tdutils::status::Status::error(
                            400,
                            "Can't access the user",
                        ));
                        return;
                    }
                    dialog_id = crate::dialog_id::DialogId::from_user_id(user_id);
                }
                // check_dialog_access rejects invalid dialog ids first (the
                // C++ UNREACHABLE default is unreachable for the same reason)
                crate::dialog_id::DialogType::None => {
                    unreachable!("check_dialog_access rejects invalid dialogs")
                }
            }
            crate::dialog_manager::set_dialog_theme_on_server(
                td,
                dialog_id,
                &theme_name,
                is_gift,
                promise,
            );
        }

        // Everything else waits for its manager step.
        // --- autosave (Requests.cpp:3136-3152) ---
        Function::getAutosaveSettings(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |s: td_api::AutosaveSettings| {
                Object::autosaveSettings(s)
            });
            let mut env = AutosaveEnv {
                global: &mut td.global,
                updates: &mut td.autosave_updates,
            };
            td.autosave_manager.get_autosave_settings(&mut env, promise);
        }
        Function::setAutosaveSettings(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = AutosaveEnv {
                global: &mut td.global,
                updates: &mut td.autosave_updates,
            };
            td.autosave_manager.set_autosave_settings(
                &mut env,
                request.scope,
                request.settings,
                promise,
            );
        }
        Function::clearAutosaveSettingsExceptions(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = AutosaveEnv {
                global: &mut td.global,
                updates: &mut td.autosave_updates,
            };
            td.autosave_manager
                .clear_autosave_settings_exceptions(&mut env, promise);
        }

        // Everything else waits for its manager step.
        Function::getChatBoostStatus(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |s: td_api::ChatBoostStatus| Object::chatBoostStatus(s));
            let mut env = BoostEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                chat_manager: &mut td.chat_manager,
            };
            td.boost_manager.get_dialog_boost_status(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                promise,
            );
        }
        Function::boostChat(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |s: td_api::ChatBoostSlots| Object::chatBoostSlots(s));
            let mut env = BoostEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                chat_manager: &mut td.chat_manager,
            };
            td.boost_manager.boost_dialog(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                &request.slot_ids,
                promise,
            );
        }
        Function::getChatBoostLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let result = {
                let mut env = BoostEnv {
                    global: &mut td.global,
                    options: td.option_manager.as_mut(),
                    chat_manager: &mut td.chat_manager,
                };
                td.boost_manager.get_dialog_boost_link(
                    &mut env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                )
            };
            match result {
                Ok((url, is_public)) => td.send_result(
                    id,
                    Object::chatBoostLink(td_api::ChatBoostLink {
                        link: url,
                        is_public,
                    }),
                ),
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::getChatBoosts(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(offset) = crate::misc::clean_input_string(&request.offset) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise =
                td.deferred_promise(id, |b: td_api::FoundChatBoosts| Object::foundChatBoosts(b));
            let mut env = BoostEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                chat_manager: &mut td.chat_manager,
            };
            td.boost_manager.get_dialog_boosts(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.only_gift_codes,
                &offset,
                request.limit,
                promise,
            );
        }
        Function::getUserChatBoosts(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |b: td_api::FoundChatBoosts| Object::foundChatBoosts(b));
            let mut env = BoostEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                chat_manager: &mut td.chat_manager,
            };
            td.boost_manager.get_user_dialog_boosts(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.user_id,
                promise,
            );
        }

        // Everything else waits for its manager step.
        // --- quick replies (Requests.cpp) ---
        Function::setQuickReplyShortcutName(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = crate::misc::clean_input_string(&request.name) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = QuickReplyEnv {
                global: &mut td.global,
            };
            td.quick_reply_manager.set_quick_reply_shortcut_name(
                &mut env,
                request.shortcut_id,
                &name,
                promise,
            );
        }
        Function::deleteQuickReplyShortcut(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = QuickReplyEnv {
                global: &mut td.global,
            };
            td.quick_reply_manager.delete_quick_reply_shortcut(
                &mut env,
                request.shortcut_id,
                promise,
            );
        }
        Function::reorderQuickReplyShortcuts(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = QuickReplyEnv {
                global: &mut td.global,
            };
            td.quick_reply_manager.reorder_quick_reply_shortcuts(
                &mut env,
                &request.shortcut_ids,
                promise,
            );
        }
        Function::deleteQuickReplyShortcutMessages(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = QuickReplyEnv {
                global: &mut td.global,
            };
            td.quick_reply_manager.delete_quick_reply_shortcut_messages(
                &mut env,
                request.shortcut_id,
                &request.message_ids,
                promise,
            );
        }

        // Everything else waits for its manager step.
        Function::getPremiumLimit(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |l: td_api::PremiumLimit| Object::premiumLimit(l));
            let env = PremiumTdEnv {
                options: td.option_manager.as_mut(),
            };
            td.premium_manager
                .get_premium_limit(&env, &request.limit_type, promise);
        }
        Function::getPremiumFeatures(request) => {
            if !check_is_user(td, id) {
                return;
            };
            let _ = request.source;
            let promise =
                td.deferred_promise(id, |f: td_api::PremiumFeatures| Object::premiumFeatures(f));
            let env = PremiumTdEnv {
                options: td.option_manager.as_mut(),
            };
            td.premium_manager.get_premium_features(&env, promise);
        }
        Function::getBusinessFeatures(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |f: td_api::BusinessFeatures| {
                Object::businessFeatures(f)
            });
            let mut env = PremiumTdEnv {
                options: td.option_manager.as_mut(),
            };
            td.premium_manager.get_business_features(&mut env, promise);
        }
        Function::viewPremiumFeature(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            // the view/click side effects join the premium promo surface;
            // the promise parks with the seam
            td.premium_manager.net_blocked_promises.push(promise);
        }
        Function::clickPremiumSubscriptionButton(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            // the view/click side effects join the premium promo surface;
            // the promise parks with the seam
            td.premium_manager.net_blocked_promises.push(promise);
        }
        Function::getPremiumState(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |s: td_api::PremiumState| Object::premiumState(s));
            let mut env = PremiumTdEnv {
                options: td.option_manager.as_mut(),
            };
            td.premium_manager.get_premium_state(&mut env, promise);
        }
        Function::getPremiumGiftPaymentOptions(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |o: td_api::PremiumGiftPaymentOptions| {
                Object::premiumGiftPaymentOptions(o)
            });
            let mut env = PremiumTdEnv {
                options: td.option_manager.as_mut(),
            };
            td.premium_manager
                .get_premium_gift_payment_options(&mut env, promise);
        }
        Function::getPremiumGiveawayPaymentOptions(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |o: td_api::PremiumGiveawayPaymentOptions| {
                Object::premiumGiveawayPaymentOptions(o)
            });
            let mut env = PremiumTdEnv {
                options: td.option_manager.as_mut(),
            };
            td.premium_manager.get_premium_giveaway_options(
                &mut env,
                request.boosted_chat_id,
                promise,
            );
        }
        Function::checkPremiumGiftCode(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(code) = crate::misc::clean_input_string(&request.code) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |i: td_api::PremiumGiftCodeInfo| {
                Object::premiumGiftCodeInfo(i)
            });
            let mut env = PremiumTdEnv {
                options: td.option_manager.as_mut(),
            };
            td.premium_manager
                .check_premium_gift_code(&mut env, &code, promise);
        }
        Function::applyPremiumGiftCode(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(code) = crate::misc::clean_input_string(&request.code) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = PremiumTdEnv {
                options: td.option_manager.as_mut(),
            };
            td.premium_manager
                .apply_premium_gift_code(&mut env, &code, promise);
        }
        Function::giftPremiumWithStars(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = PremiumTdEnv {
                options: td.option_manager.as_mut(),
            };
            td.premium_manager.gift_premium_with_stars(
                &mut env,
                request.user_id,
                request.star_count,
                request.month_count,
                promise,
            );
        }
        Function::assignStoreTransaction(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = PremiumTdEnv {
                options: td.option_manager.as_mut(),
            };
            td.premium_manager
                .assign_store_transaction(&mut env, promise);
        }

        // Everything else waits for its manager step.
        Function::getChatStatistics(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |s: td_api::ChatStatistics| match s {
                td_api::ChatStatistics::chatStatisticsSupergroup(v) => {
                    Object::chatStatisticsSupergroup(v)
                }
                td_api::ChatStatistics::chatStatisticsChannel(v) => {
                    Object::chatStatisticsChannel(v)
                }
            });
            let mut env = StatisticsEnv {
                global: &mut td.global,
                updates: &mut td.statistics_updates,
            };
            td.statistics_manager.get_channel_statistics(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.is_dark,
                promise,
            );
        }
        Function::getMessageStatistics(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |s: td_api::MessageStatistics| {
                Object::messageStatistics(s)
            });
            let mut env = StatisticsEnv {
                global: &mut td.global,
                updates: &mut td.statistics_updates,
            };
            td.statistics_manager.get_channel_message_statistics(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.is_dark,
                promise,
            );
        }
        Function::getStoryStatistics(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |s: td_api::StoryStatistics| Object::storyStatistics(s));
            let mut env = StatisticsEnv {
                global: &mut td.global,
                updates: &mut td.statistics_updates,
            };
            td.statistics_manager.get_channel_story_statistics(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.is_dark,
                promise,
            );
        }
        Function::getChatRevenueStatistics(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |s: td_api::ChatRevenueStatistics| {
                Object::chatRevenueStatistics(s)
            });
            let mut env = StatisticsEnv {
                global: &mut td.global,
                updates: &mut td.statistics_updates,
            };
            td.statistics_manager.get_dialog_revenue_statistics(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.is_dark,
                promise,
            );
        }
        Function::getChatRevenueWithdrawalUrl(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(password) = crate::misc::clean_input_string(&request.password) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise =
                td.deferred_promise(id, |url: String| Object::httpUrl(td_api::HttpUrl { url }));
            let mut env = StatisticsEnv {
                global: &mut td.global,
                updates: &mut td.statistics_updates,
            };
            td.statistics_manager.get_dialog_revenue_withdrawal_url(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                &password,
                promise,
            );
        }
        Function::getChatRevenueTransactions(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(offset) = crate::misc::clean_input_string(&request.offset) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |s: td_api::ChatRevenueTransactions| {
                Object::chatRevenueTransactions(s)
            });
            let mut env = StatisticsEnv {
                global: &mut td.global,
                updates: &mut td.statistics_updates,
            };
            td.statistics_manager.get_dialog_revenue_transactions(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                &offset,
                request.limit,
                promise,
            );
        }
        Function::getMessagePublicForwards(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(offset) = crate::misc::clean_input_string(&request.offset) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise =
                td.deferred_promise(id, |s: td_api::PublicForwards| Object::publicForwards(s));
            let mut env = StatisticsEnv {
                global: &mut td.global,
                updates: &mut td.statistics_updates,
            };
            td.statistics_manager.get_message_public_forwards(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                &offset,
                request.limit,
                promise,
            );
        }
        Function::getStoryPublicForwards(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(offset) = crate::misc::clean_input_string(&request.offset) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise =
                td.deferred_promise(id, |s: td_api::PublicForwards| Object::publicForwards(s));
            let mut env = StatisticsEnv {
                global: &mut td.global,
                updates: &mut td.statistics_updates,
            };
            td.statistics_manager.get_story_public_forwards(
                &mut env,
                crate::dialog_id::DialogId::new(request.story_poster_chat_id),
                &offset,
                request.limit,
                promise,
            );
        }

        // Everything else waits for its manager step.
        _ => run_bot_requests(td, id, function),
    }
}

/// Build the [`crate::td::PaymentsEnv`] over the disjoint `Td` fields and
/// run the payment flow. The env is per-call (the `ThemeEnv` convention);
/// `my_dialog_id` is read before the split.
fn payments_arm(td: &mut Td, run: impl FnOnce(&mut crate::td::PaymentsEnv<'_>), _id: u64) {
    let my_dialog_id = crate::dialog_manager::get_my_dialog_id(td);
    let crate::td::Td {
        global,
        option_manager,
        auth_manager,
        user_manager,
        chat_manager,
        ..
    } = td;
    let mut env = crate::td::PaymentsEnv {
        global,
        options: Some(
            option_manager
                .as_mut()
                .expect("payment requests run with the initialized option manager"),
        ),
        auth_is_bot: auth_manager.is_bot(),
        my_dialog_id,
        user_manager,
        chat_manager,
    };
    run(&mut env);
}

fn bot_env_split(
    td: &mut Td,
) -> (
    &mut crate::bot_info_manager::BotInfoManager,
    crate::td::BotInfoEnv<'_>,
) {
    let crate::td::Td {
        bot_info_manager,
        global,
        option_manager,
        auth_manager,
        user_manager,
        ..
    } = td;
    let env = crate::td::BotInfoEnv {
        global,
        options: option_manager.as_mut(),
        auth_is_authorized: auth_manager.is_authorized(),
        auth_is_bot: auth_manager.is_bot(),
        user_manager,
    };
    (bot_info_manager, env)
}

fn run_bot_requests(td: &mut Td, id: u64, function: Function) {
    match function {
        // ------------------------------------------------------------------
        // M.45: the bot-served requests. The C++ anchors name the
        // `Requests::on_request` overloads; the flow bodies live in
        // `bot_command.rs`/`bot_menu_button.rs`/`bot_info_manager.rs`/
        // `bot_queries.rs`/`message_sender.rs`.
        // ------------------------------------------------------------------

        // --- the bot commands (BotCommand.cpp) ---
        Function::setCommands(request) => {
            // Requests.cpp:6857-6862: CHECK_IS_BOT + set_commands
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (manager, mut env) = bot_env_split(td);
            crate::bot_command::set_commands(
                manager,
                &mut env,
                request.scope,
                request.language_code,
                request.commands,
                promise,
            );
        }
        Function::deleteCommands(request) => {
            // Requests.cpp:6864-6869
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (manager, mut env) = bot_env_split(td);
            crate::bot_command::delete_commands(
                manager,
                &mut env,
                request.scope,
                request.language_code,
                promise,
            );
        }
        Function::getCommands(request) => {
            // Requests.cpp:6870-6875
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |commands: td_api::BotCommands| {
                Object::botCommands(commands)
            });
            let (manager, mut env) = bot_env_split(td);
            crate::bot_command::get_commands(
                manager,
                &mut env,
                request.scope,
                request.language_code,
                promise,
            );
        }

        // --- the bot menu button (BotMenuButton.cpp) ---
        Function::setMenuButton(request) => {
            // Requests.cpp:6876-6881
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (manager, mut env) = bot_env_split(td);
            crate::bot_menu_button::set_menu_button(
                manager,
                &mut env,
                crate::user_manager::UserId::new(request.user_id),
                request.menu_button,
                promise,
            );
        }
        Function::getMenuButton(request) => {
            // Requests.cpp:6882-6887
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |button: td_api::BotMenuButton| {
                Object::botMenuButton(button)
            });
            let (manager, mut env) = bot_env_split(td);
            crate::bot_menu_button::get_menu_button(
                manager,
                &mut env,
                crate::user_manager::UserId::new(request.user_id),
                promise,
            );
        }

        // --- the default administrator rights ---
        Function::setDefaultGroupAdministratorRights(request) => {
            // Requests.cpp:6888-6894
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (manager, mut env) = bot_env_split(td);
            manager.set_default_group_administrator_rights(
                &mut env,
                crate::chat_manager::AdministratorRights::from_chat_administrator_rights(
                    request.default_group_administrator_rights.as_ref(),
                    crate::chat_manager::ChannelType::Megagroup,
                ),
                promise,
            );
        }
        Function::setDefaultChannelAdministratorRights(request) => {
            // Requests.cpp:6895-6901
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (manager, mut env) = bot_env_split(td);
            manager.set_default_channel_administrator_rights(
                &mut env,
                crate::chat_manager::AdministratorRights::from_chat_administrator_rights(
                    request.default_channel_administrator_rights.as_ref(),
                    crate::chat_manager::ChannelType::Broadcast,
                ),
                promise,
            );
        }

        // --- the bot administration surface (BotInfoManager.cpp) ---
        Function::getOwnedBots(request) => {
            // Requests.cpp:6741-6745: CHECK_IS_USER + get_owned_bots
            if !check_is_user(td, id) {
                return;
            }
            let _ = request;
            let promise = td.deferred_promise(id, |users: td_api::Users| Object::users(users));
            let (manager, mut env) = bot_env_split(td);
            manager.get_owned_bots(&mut env, promise);
        }
        Function::canBotSendMessages(request) => {
            // Requests.cpp:6902-6907: CHECK_IS_USER
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (manager, mut env) = bot_env_split(td);
            manager.can_bot_send_messages(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                promise,
            );
        }
        Function::allowBotToSendMessages(request) => {
            // Requests.cpp:6908-6912: CHECK_IS_USER
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (manager, mut env) = bot_env_split(td);
            manager.allow_bot_to_send_messages(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                promise,
            );
        }
        Function::getBotMediaPreviews(request) => {
            // Requests.cpp:6923-6928: CHECK_IS_USER
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |previews: td_api::BotMediaPreviews| {
                Object::botMediaPreviews(previews)
            });
            let (manager, mut env) = bot_env_split(td);
            manager.get_bot_media_previews(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                promise,
            );
        }
        Function::getBotMediaPreviewInfo(request) => {
            // Requests.cpp:6929-6935
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |info: td_api::BotMediaPreviewInfo| {
                Object::botMediaPreviewInfo(info)
            });
            let (manager, mut env) = bot_env_split(td);
            manager.get_bot_media_preview_info(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                &request.language_code,
                promise,
            );
        }
        Function::addBotMediaPreview(request) => {
            // Requests.cpp:6936-6942
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |preview: td_api::BotMediaPreview| {
                Object::botMediaPreview(preview)
            });
            let (manager, mut env) = bot_env_split(td);
            manager.add_bot_media_preview(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                &request.language_code,
                request.content,
                promise,
            );
        }
        Function::editBotMediaPreview(request) => {
            // Requests.cpp:6943-6950: FileId(request.file_id_, 0)
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |preview: td_api::BotMediaPreview| {
                Object::botMediaPreview(preview)
            });
            let (manager, mut env) = bot_env_split(td);
            manager.edit_bot_media_preview(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                &request.language_code,
                crate::files::FileId::new(request.file_id, 0),
                request.content,
                promise,
            );
        }
        Function::reorderBotMediaPreviews(request) => {
            // Requests.cpp:6951-6957
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (manager, mut env) = bot_env_split(td);
            manager.reorder_bot_media_previews(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                &request.language_code,
                &request.file_ids,
                promise,
            );
        }
        Function::deleteBotMediaPreviews(request) => {
            // Requests.cpp:6958-6964
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (manager, mut env) = bot_env_split(td);
            manager.delete_bot_media_previews(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                &request.language_code,
                &request.file_ids,
                promise,
            );
        }
        Function::createBot(request) => {
            // Requests.cpp:6979-6987: CHECK_IS_USER + the cleaned strings
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let Some(username) = clean_or_utf8_error(td, id, &request.username) else {
                return;
            };
            let promise = td.deferred_promise(id, |user: td_api::User| Object::user(user));
            let (manager, mut env) = bot_env_split(td);
            manager.create_bot(
                &mut env,
                crate::user_manager::UserId::new(request.manager_bot_user_id),
                &name,
                &username,
                request.via_link,
                promise,
            );
        }
        Function::getManagedBotToken(request) => {
            // Requests.cpp:6988-6993: CHECK_IS_BOT
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |token: String| {
                Object::text(td_api::Text { text: token })
            });
            let (manager, mut env) = bot_env_split(td);
            manager.get_bot_token(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                request.revoke,
                promise,
            );
        }
        Function::getManagedBotAccessSettings(request) => {
            // Requests.cpp:6994-6999: CHECK_IS_BOT
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |settings: td_api::BotAccessSettings| {
                Object::botAccessSettings(settings)
            });
            let (manager, mut env) = bot_env_split(td);
            manager.get_bot_access_settings(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                promise,
            );
        }
        Function::setManagedBotAccessSettings(request) => {
            // Requests.cpp:7000-7006: CHECK_IS_BOT
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (manager, mut env) = bot_env_split(td);
            manager.set_bot_access_settings(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                request.settings,
                promise,
            );
        }
        Function::setBotName(request) => {
            // Requests.cpp:7007-7013
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (manager, mut env) = bot_env_split(td);
            manager.set_bot_name(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                &request.language_code,
                &name,
                promise,
            );
        }
        Function::getBotName(request) => {
            // Requests.cpp:7014-7019: CREATE_TEXT_REQUEST_PROMISE
            let promise =
                td.deferred_promise(id, |name: String| Object::text(td_api::Text { text: name }));
            let (manager, mut env) = bot_env_split(td);
            manager.get_bot_name(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                &request.language_code,
                promise,
            );
        }
        Function::setBotInfoDescription(request) => {
            // Requests.cpp:7042-7048
            let Some(description) = clean_or_utf8_error(td, id, &request.description) else {
                return;
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (manager, mut env) = bot_env_split(td);
            manager.set_bot_info_description(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                &request.language_code,
                &description,
                promise,
            );
        }
        Function::getBotInfoDescription(request) => {
            // Requests.cpp:7049-7054: CREATE_TEXT_REQUEST_PROMISE
            let promise = td.deferred_promise(id, |description: String| {
                Object::text(td_api::Text { text: description })
            });
            let (manager, mut env) = bot_env_split(td);
            manager.get_bot_info_description(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                &request.language_code,
                promise,
            );
        }
        Function::setBotInfoShortDescription(request) => {
            // Requests.cpp:7055-7061
            let Some(short_description) = clean_or_utf8_error(td, id, &request.short_description)
            else {
                return;
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (manager, mut env) = bot_env_split(td);
            manager.set_bot_info_about(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                &request.language_code,
                &short_description,
                promise,
            );
        }
        Function::getBotInfoShortDescription(request) => {
            // Requests.cpp:7062-7066: CREATE_TEXT_REQUEST_PROMISE
            let promise = td.deferred_promise(id, |about: String| {
                Object::text(td_api::Text { text: about })
            });
            let (manager, mut env) = bot_env_split(td);
            manager.get_bot_info_about(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                &request.language_code,
                promise,
            );
        }
        Function::setMessageSenderBotVerification(request) => {
            // Requests.cpp:7067-7074: get_message_sender_dialog_id with
            // check_access = true
            let Some(custom_description) = clean_or_utf8_error(td, id, &request.custom_description)
            else {
                return;
            };
            let dialog_id = {
                let (_manager, mut env) = bot_env_split(td);
                match crate::message_sender::get_message_sender_dialog_id(
                    &mut env,
                    request.verified_id.as_ref(),
                    true,
                    false,
                ) {
                    tdutils::status::Result::Ok(dialog_id) => dialog_id,
                    tdutils::status::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (manager, mut env) = bot_env_split(td);
            manager.set_custom_bot_verification(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                dialog_id,
                true,
                &custom_description,
                promise,
            );
        }
        Function::removeMessageSenderBotVerification(request) => {
            // Requests.cpp:7075-7081: check_access = false, empty description
            let dialog_id = {
                let (_manager, mut env) = bot_env_split(td);
                match crate::message_sender::get_message_sender_dialog_id(
                    &mut env,
                    request.verified_id.as_ref(),
                    false,
                    false,
                ) {
                    tdutils::status::Result::Ok(dialog_id) => dialog_id,
                    tdutils::status::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (manager, mut env) = bot_env_split(td);
            manager.set_custom_bot_verification(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                dialog_id,
                false,
                "",
                promise,
            );
        }

        // --- the bot custom queries (BotQueries.cpp) ---
        Function::setBotUpdatesStatus(request) => {
            // Requests.cpp:8954-8960: CHECK_IS_BOT; the fire-and-forget
            // send answers ok immediately
            if !check_is_bot(td, id) {
                return;
            }
            let Some(error_message) = clean_or_utf8_error(td, id, &request.error_message) else {
                return;
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (_manager, mut env) = bot_env_split(td);
            crate::bot_queries::set_bot_updates_status(
                &mut env,
                request.pending_update_count,
                error_message,
                promise,
            );
        }
        Function::sendCustomRequest(request) => {
            // Requests.cpp:8961-8968: CHECK_IS_BOT
            if !check_is_bot(td, id) {
                return;
            }
            let Some(method) = clean_or_utf8_error(td, id, &request.method) else {
                return;
            };
            let Some(parameters) = clean_or_utf8_error(td, id, &request.parameters) else {
                return;
            };
            let promise = td.deferred_promise(id, |result: td_api::CustomRequestResult| {
                Object::customRequestResult(result)
            });
            let (manager, mut env) = bot_env_split(td);
            crate::bot_queries::send_bot_custom_query(
                manager, &mut env, method, parameters, promise,
            );
        }
        Function::answerCustomQuery(request) => {
            // Requests.cpp:8969-8975: CHECK_IS_BOT
            if !check_is_bot(td, id) {
                return;
            }
            let Some(data) = clean_or_utf8_error(td, id, &request.data) else {
                return;
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (manager, mut env) = bot_env_split(td);
            crate::bot_queries::answer_bot_custom_query(
                manager,
                &mut env,
                request.custom_query_id,
                data,
                promise,
            );
        }

        // The M.46 bots arm group rides the same depth rule: a cheap
        // predicate forwards it at this level instead of nesting it under
        // the older groups.
        function if is_bot_request(&function) => run_bot_requests_46(td, id, function),
        // Everything else waits for its manager step.
        _ => {
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }
    }
}
/// Whether the function is one of the M.46 bots arm group (the cheap
/// reference match keeps the dispatcher frame small).
fn is_bot_request(function: &Function) -> bool {
    matches!(
        function,
        Function::getBotSimilarBots(_)
            | Function::getBotSimilarBotCount(_)
            | Function::openBotSimilarBot(_)
            | Function::setGameScore(_)
            | Function::getGameHighScores(_)
            | Function::sendWebAppCustomRequest(_)
            | Function::getGrossingWebAppBots(_)
            | Function::searchWebApp(_)
            | Function::getWebAppLinkUrl(_)
            | Function::getMainWebApp(_)
            | Function::openWebApp(_)
            | Function::closeWebApp(_)
            | Function::checkWebAppFileDownload(_)
            | Function::getCallbackQueryAnswer(_)
            | Function::answerCallbackQuery(_)
    )
}

/// The M.46 bots arm group of the request dispatcher — see the
/// frame note at the forward site in [`run_request`]. The manager state
/// is `std::mem::take`-n out of the `Td` for the call because the
/// [`crate::td::BotEnv`] holds the whole `Td` (the dialog/message free
/// functions keep their `&mut Td` signatures); it is put back after the
/// env drops. Every arm body is an `#[inline(never)]` helper so this
/// frame stays small on the 2 MiB test-thread stack (the M.6 gate).
#[inline(never)]
fn run_bot_requests_46(td: &mut Td, id: u64, function: Function) {
    match function {
        // getBotSimilarBots (Requests.cpp:3210-3215)
        Function::getBotSimilarBots(request) => arm_get_bot_similar_bots(td, id, request),
        // getBotSimilarBotCount (Requests.cpp:3217-3222)
        Function::getBotSimilarBotCount(request) => arm_get_bot_similar_bot_count(td, id, request),
        // openBotSimilarBot (Requests.cpp:3224-3229)
        Function::openBotSimilarBot(request) => arm_open_bot_similar_bot(td, id, request),
        // setGameScore (Requests.cpp:4911-4918)
        Function::setGameScore(request) => arm_set_game_score(td, id, request),
        // getGameHighScores (Requests.cpp:4926-4931)
        Function::getGameHighScores(request) => arm_get_game_high_scores(td, id, request),
        // sendWebAppCustomRequest (Requests.cpp:6916-6922)
        Function::sendWebAppCustomRequest(request) => {
            arm_send_web_app_custom_request(td, id, request)
        }
        // getGrossingWebAppBots (Requests.cpp:8253-8257)
        Function::getGrossingWebAppBots(request) => arm_get_grossing_web_app_bots(td, id, request),
        // searchWebApp (Requests.cpp:8259-8264)
        Function::searchWebApp(request) => arm_search_web_app(td, id, request),
        // getWebAppLinkUrl (Requests.cpp:8274-8281)
        Function::getWebAppLinkUrl(request) => arm_get_web_app_link_url(td, id, request),
        // getMainWebApp (Requests.cpp:8283-8289)
        Function::getMainWebApp(request) => arm_get_main_web_app(td, id, request),
        // openWebApp (Requests.cpp:8318-8325)
        Function::openWebApp(request) => arm_open_web_app(td, id, request),
        // closeWebApp (Requests.cpp:8327-8330)
        Function::closeWebApp(request) => arm_close_web_app(td, id, request),
        // checkWebAppFileDownload (Requests.cpp:8342-8347)
        Function::checkWebAppFileDownload(request) => {
            arm_check_web_app_file_download(td, id, request)
        }
        // getCallbackQueryAnswer (Requests.cpp:8357-8361)
        Function::getCallbackQueryAnswer(request) => arm_get_callback_query_answer(td, id, request),
        // answerCallbackQuery (Requests.cpp:8363-8369)
        Function::answerCallbackQuery(request) => arm_answer_callback_query(td, id, request),

        // Everything else waits for its manager step.
        _ => {
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }
    }
}

#[inline(never)]
fn arm_get_bot_similar_bots(td: &mut Td, id: u64, request: td_api::getBotSimilarBots) {
    if !check_is_user(td, id) {
        return;
    }
    let users_promise = td.deferred_promise::<td_api::Users, _>(id, Object::users);
    let mut manager = std::mem::take(&mut td.bot_recommendation_manager);
    {
        let mut env = crate::td::BotEnv { td };
        manager.get_bot_recommendations(
            &mut env,
            UserId::new(request.bot_user_id),
            false,
            users_promise,
            Promise::default(),
        );
    }
    td.bot_recommendation_manager = manager;
}

#[inline(never)]
fn arm_get_bot_similar_bot_count(td: &mut Td, id: u64, request: td_api::getBotSimilarBotCount) {
    if !check_is_user(td, id) {
        return;
    }
    let count_promise =
        td.deferred_promise::<i32, _>(id, |count| Object::count(td_api::Count { count }));
    let mut manager = std::mem::take(&mut td.bot_recommendation_manager);
    {
        let mut env = crate::td::BotEnv { td };
        manager.get_bot_recommendations(
            &mut env,
            UserId::new(request.bot_user_id),
            request.return_local,
            Promise::default(),
            count_promise,
        );
    }
    td.bot_recommendation_manager = manager;
}

#[inline(never)]
fn arm_open_bot_similar_bot(td: &mut Td, id: u64, request: td_api::openBotSimilarBot) {
    if !check_is_user(td, id) {
        return;
    }
    let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
    let mut manager = std::mem::take(&mut td.bot_recommendation_manager);
    {
        let mut env = crate::td::BotEnv { td };
        manager.open_bot_recommended_bot(
            &mut env,
            UserId::new(request.bot_user_id),
            UserId::new(request.opened_bot_user_id),
            promise,
        );
    }
    td.bot_recommendation_manager = manager;
}

#[inline(never)]
fn arm_set_game_score(td: &mut Td, id: u64, request: td_api::setGameScore) {
    if !check_is_bot(td, id) {
        return;
    }
    let promise = td.deferred_promise::<td_api::Message, _>(id, Object::message);
    let mut manager = std::mem::take(&mut td.game_manager);
    {
        let mut env = crate::td::BotEnv { td };
        manager.set_game_score(
            &mut env,
            crate::message_id::MessageFullId::new(
                crate::message_id::DialogId::new(request.chat_id),
                crate::message_id::MessageId::new(request.message_id),
            ),
            request.edit_message,
            UserId::new(request.user_id),
            request.score,
            request.force,
            promise,
        );
    }
    td.game_manager = manager;
}

#[inline(never)]
fn arm_get_game_high_scores(td: &mut Td, id: u64, request: td_api::getGameHighScores) {
    if !check_is_bot(td, id) {
        return;
    }
    let promise = td.deferred_promise::<td_api::GameHighScores, _>(id, Object::gameHighScores);
    let mut manager = std::mem::take(&mut td.game_manager);
    {
        let mut env = crate::td::BotEnv { td };
        manager.get_game_high_scores(
            &mut env,
            crate::message_id::MessageFullId::new(
                crate::message_id::DialogId::new(request.chat_id),
                crate::message_id::MessageId::new(request.message_id),
            ),
            UserId::new(request.user_id),
            promise,
        );
    }
    td.game_manager = manager;
}

#[inline(never)]
fn arm_send_web_app_custom_request(td: &mut Td, id: u64, request: td_api::sendWebAppCustomRequest) {
    if !check_is_user(td, id) {
        return;
    }
    let Some(method) = clean_or_utf8_error(td, id, &request.method) else {
        return;
    };
    let Some(parameters) = clean_or_utf8_error(td, id, &request.parameters) else {
        return;
    };
    let promise =
        td.deferred_promise::<td_api::CustomRequestResult, _>(id, Object::customRequestResult);
    let mut manager = std::mem::take(&mut td.web_app_manager);
    {
        let mut env = crate::td::BotEnv { td };
        manager.invoke_web_view_custom_method(
            &mut env,
            UserId::new(request.bot_user_id),
            &method,
            &parameters,
            promise,
        );
    }
    td.web_app_manager = manager;
}

#[inline(never)]
fn arm_get_grossing_web_app_bots(td: &mut Td, id: u64, request: td_api::getGrossingWebAppBots) {
    if !check_is_user(td, id) {
        return;
    }
    let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
        return;
    };
    let promise = td.deferred_promise::<td_api::FoundUsers, _>(id, Object::foundUsers);
    let mut manager = std::mem::take(&mut td.web_app_manager);
    {
        let mut env = crate::td::BotEnv { td };
        manager.get_popular_app_bots(&mut env, &offset, request.limit, promise);
    }
    td.web_app_manager = manager;
}

#[inline(never)]
fn arm_search_web_app(td: &mut Td, id: u64, request: td_api::searchWebApp) {
    if !check_is_user(td, id) {
        return;
    }
    let Some(web_app_short_name) = clean_or_utf8_error(td, id, &request.web_app_short_name) else {
        return;
    };
    // the C++ null foundWebApp (the BOT_APP_INVALID answer) becomes the
    // 404 in send_result
    let promise = td.deferred_promise::<Option<td_api::FoundWebApp>, _>(id, |found| match found {
        Some(found) => Object::foundWebApp(found),
        None => Object::error(td_api::Error {
            code: 404,
            message: "Not Found".to_owned(),
        }),
    });
    let mut manager = std::mem::take(&mut td.web_app_manager);
    {
        let mut env = crate::td::BotEnv { td };
        manager.get_web_app(
            &mut env,
            UserId::new(request.bot_user_id),
            &web_app_short_name,
            promise,
        );
    }
    td.web_app_manager = manager;
}

#[inline(never)]
fn arm_get_web_app_link_url(td: &mut Td, id: u64, request: td_api::getWebAppLinkUrl) {
    if !check_is_user(td, id) {
        return;
    }
    let Some(web_app_short_name) = clean_or_utf8_error(td, id, &request.web_app_short_name) else {
        return;
    };
    let Some(start_parameter) = clean_or_utf8_error(td, id, &request.start_parameter) else {
        return;
    };
    let parameters = crate::web_app::WebAppOpenParameters::from_td_api(request.parameters);
    let promise = td.deferred_promise::<td_api::WebAppUrl, _>(id, Object::webAppUrl);
    let mut manager = std::mem::take(&mut td.web_app_manager);
    {
        let mut env = crate::td::BotEnv { td };
        manager.request_app_web_view(
            &mut env,
            crate::dialog_id::DialogId::new(request.chat_id),
            UserId::new(request.bot_user_id),
            &web_app_short_name,
            &start_parameter,
            &parameters,
            request.allow_write_access,
            promise,
        );
    }
    td.web_app_manager = manager;
}

#[inline(never)]
fn arm_get_main_web_app(td: &mut Td, id: u64, request: td_api::getMainWebApp) {
    if !check_is_user(td, id) {
        return;
    }
    let Some(start_parameter) = clean_or_utf8_error(td, id, &request.start_parameter) else {
        return;
    };
    let parameters = crate::web_app::WebAppOpenParameters::from_td_api(request.parameters);
    let promise = td.deferred_promise::<td_api::MainWebApp, _>(id, Object::mainWebApp);
    let mut manager = std::mem::take(&mut td.web_app_manager);
    {
        let mut env = crate::td::BotEnv { td };
        manager.request_main_web_view(
            &mut env,
            crate::dialog_id::DialogId::new(request.chat_id),
            UserId::new(request.bot_user_id),
            &start_parameter,
            &parameters,
            promise,
        );
    }
    td.web_app_manager = manager;
}

#[inline(never)]
fn arm_open_web_app(td: &mut Td, id: u64, request: td_api::openWebApp) {
    if !check_is_user(td, id) {
        return;
    }
    let Some(url) = clean_or_utf8_error(td, id, &request.url) else {
        return;
    };
    let parameters = crate::web_app::WebAppOpenParameters::from_td_api(request.parameters);
    let promise = td.deferred_promise::<td_api::WebAppInfo, _>(id, Object::webAppInfo);
    let mut manager = std::mem::take(&mut td.web_app_manager);
    {
        let mut env = crate::td::BotEnv { td };
        manager.request_web_view(
            &mut env,
            crate::dialog_id::DialogId::new(request.chat_id),
            UserId::new(request.bot_user_id),
            request.topic_id.as_ref(),
            &url,
            &parameters,
            promise,
        );
    }
    td.web_app_manager = manager;
}

#[inline(never)]
fn arm_close_web_app(td: &mut Td, id: u64, request: td_api::closeWebApp) {
    if !check_is_user(td, id) {
        return;
    }
    // the promise completes synchronously
    let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
    let mut manager = std::mem::take(&mut td.web_app_manager);
    manager.close_web_view(request.web_app_launch_id, promise);
    td.web_app_manager = manager;
}

#[inline(never)]
fn arm_check_web_app_file_download(td: &mut Td, id: u64, request: td_api::checkWebAppFileDownload) {
    if !check_is_user(td, id) {
        return;
    }
    let Some(file_name) = clean_or_utf8_error(td, id, &request.file_name) else {
        return;
    };
    let Some(url) = clean_or_utf8_error(td, id, &request.url) else {
        return;
    };
    let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
    let mut manager = std::mem::take(&mut td.web_app_manager);
    {
        let mut env = crate::td::BotEnv { td };
        manager.check_download_file_params(
            &mut env,
            UserId::new(request.bot_user_id),
            &file_name,
            &url,
            promise,
        );
    }
    td.web_app_manager = manager;
}

#[inline(never)]
fn arm_get_callback_query_answer(td: &mut Td, id: u64, request: td_api::getCallbackQueryAnswer) {
    if !check_is_user(td, id) {
        return;
    }
    let promise =
        td.deferred_promise::<td_api::CallbackQueryAnswer, _>(id, Object::callbackQueryAnswer);
    let mut manager = std::mem::take(&mut td.callback_queries_manager);
    {
        let mut env = crate::td::BotEnv { td };
        manager.send_callback_query(
            &mut env,
            crate::message_id::MessageFullId::new(
                crate::message_id::DialogId::new(request.chat_id),
                crate::message_id::MessageId::new(request.message_id),
            ),
            request.payload,
            promise,
        );
    }
    td.callback_queries_manager = manager;
}

#[inline(never)]
fn arm_answer_callback_query(td: &mut Td, id: u64, request: td_api::answerCallbackQuery) {
    if !check_is_bot(td, id) {
        return;
    }
    let Some(text) = clean_or_utf8_error(td, id, &request.text) else {
        return;
    };
    let Some(url) = clean_or_utf8_error(td, id, &request.url) else {
        return;
    };
    let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
    let mut manager = std::mem::take(&mut td.callback_queries_manager);
    {
        let mut env = crate::td::BotEnv { td };
        manager.answer_callback_query(
            &mut env,
            request.callback_query_id,
            &text,
            request.show_alert,
            &url,
            request.cache_time,
            promise,
        );
    }
    td.callback_queries_manager = manager;
}

/// The `CanPostStoryResult` → `Object` rewrap (the Object umbrella has no
/// single variant for the six-ctor type).
fn can_post_story_result_to_object(value: td_api::CanPostStoryResult) -> Object {
    match value {
        td_api::CanPostStoryResult::canPostStoryResultOk(value) => {
            Object::canPostStoryResultOk(value)
        }
        td_api::CanPostStoryResult::canPostStoryResultPremiumNeeded(value) => {
            Object::canPostStoryResultPremiumNeeded(value)
        }
        td_api::CanPostStoryResult::canPostStoryResultBoostNeeded(value) => {
            Object::canPostStoryResultBoostNeeded(value)
        }
        td_api::CanPostStoryResult::canPostStoryResultWeeklyLimitExceeded(value) => {
            Object::canPostStoryResultWeeklyLimitExceeded(value)
        }
        td_api::CanPostStoryResult::canPostStoryResultMonthlyLimitExceeded(value) => {
            Object::canPostStoryResultMonthlyLimitExceeded(value)
        }
        td_api::CanPostStoryResult::canPostStoryResultActiveStoryLimitExceeded(value) => {
            Object::canPostStoryResultActiveStoryLimitExceeded(value)
        }
        td_api::CanPostStoryResult::canPostStoryResultLiveStoryIsActive(value) => {
            Object::canPostStoryResultLiveStoryIsActive(value)
        }
    }
}

/// The `StartLiveStoryResult` → `Object` rewrap.
fn start_live_story_result_to_object(value: td_api::StartLiveStoryResult) -> Object {
    match value {
        td_api::StartLiveStoryResult::startLiveStoryResultOk(value) => {
            Object::startLiveStoryResultOk(value)
        }
        td_api::StartLiveStoryResult::startLiveStoryResultFail(value) => {
            Object::startLiveStoryResultFail(value)
        }
    }
}

/// The `ReportStoryResult` → `Object` rewrap.
fn report_story_result_to_object(value: td_api::ReportStoryResult) -> Object {
    match value {
        td_api::ReportStoryResult::reportStoryResultOk(value) => Object::reportStoryResultOk(value),
        td_api::ReportStoryResult::reportStoryResultOptionRequired(value) => {
            Object::reportStoryResultOptionRequired(value)
        }
        td_api::ReportStoryResult::reportStoryResultTextRequired(value) => {
            Object::reportStoryResultTextRequired(value)
        }
    }
}

/// The `Option<Story>` → `Object` mapper for the null-result 404 (the
/// C++ `send_result(nullptr)` convention, td.rs testUseUpdate precedent).
fn story_optional_to_object(value: Option<td_api::Story>) -> Object {
    match value {
        Some(story) => Object::story(story),
        None => Object::error(td_api::Error {
            code: 404,
            message: "Not Found".to_owned(),
        }),
    }
}

/// `CHECK_IS_USER` (Requests.cpp:1997-2000).
fn check_is_user(td: &mut Td, id: u64) -> bool {
    if td.auth_manager.is_bot() {
        td.send_error_impl(id, 400, "The method is not available to bots");
        return false;
    }
    true
}

/// `CHECK_IS_BOT` (Requests.cpp:1993-1996).
fn check_is_bot(td: &mut Td, id: u64) -> bool {
    if !td.auth_manager.is_bot() {
        td.send_error_impl(id, 400, "Only bots can use the method");
        return false;
    }
    true
}

/// `Td::send_error(id, Status)` (Td.cpp:1429-1432).
fn send_error_status(td: &mut Td, id: u64, status: Status) {
    td.send_error_impl(id, status.code(), status.message());
}

/// `SearchContactsRequest` (Requests.cpp:1217-1234): parked against the
/// contacts load, answered from the drain's `do_send_result` shape.
fn search_contacts_request(td: &mut Td, id: u64, query: String, limit: i32) {
    let promise: Promise<Object> = td.deferred_promise(id, |object| object);
    let mut env = user_env!(td);
    td.user_manager
        .park_pending_user_request(PendingUserRequest::SearchContacts {
            query,
            limit,
            promise,
        });
    if !td.user_manager.are_contacts_loaded() {
        td.user_manager.load_contacts(&mut env, Promise::default());
    } else {
        td.user_manager.drain_pending_user_requests(&mut env);
    }
}

/// `get_contacts` (Contact.cpp:159-170) — the request-side validation of
/// the imported contacts. The `note` field is the MessageEntity step (the
/// M.4.14 divergence keeps it unparsed and unstored); the vCard-less
/// `importedContact` constructor builds an empty vCard.
fn get_contacts(imported: &[td_api::ImportedContact]) -> std::result::Result<Vec<Contact>, Status> {
    let mut contacts = Vec::with_capacity(imported.len());
    for imported_contact in imported {
        let mut contact = Contact::new(
            imported_contact.phone_number.clone(),
            imported_contact.first_name.clone(),
            imported_contact.last_name.clone(),
            String::new(),
            UserId::default(),
        );
        contact.validate()?;
        contacts.push(contact);
    }
    Ok(contacts)
}

type RecordedUnitResult = std::sync::Arc<parking_lot::Mutex<Option<tdutils::status::Result<()>>>>;

/// The recording-promise bridge for synchronously-settling manager
/// requests (Requests.cpp's `PromiseActor` hops collapse here): the
/// manager errors or resolves the promise before this returns, and the
/// recorded result forwards to `send_result`/`send_error`.
fn record_unit_promise() -> (tdutils::promise::Promise<()>, RecordedUnitResult) {
    let recorded = std::sync::Arc::new(parking_lot::Mutex::new(None));
    let sink = recorded.clone();
    let promise = tdutils::promise::PromiseCreator::lambda_result(
        move |result: tdutils::status::Result<()>| {
            *sink.lock() = Some(result);
        },
    );
    (promise, recorded)
}

fn finish_unit_request(
    id: u64,
    td: &mut Td,
    recorded: std::sync::Arc<parking_lot::Mutex<Option<tdutils::status::Result<()>>>>,
) {
    match &*recorded.lock() {
        Some(tdutils::status::Result::Ok(())) => td.send_result(id, ok_object()),
        Some(tdutils::status::Result::Err(status)) => {
            td.send_error_impl(id, status.code(), status.message());
        }
        other => panic!("request {id} never settled: {other:?}"),
    }
}

/// The synchronous `CREATE_OK_REQUEST_PROMISE` collapse for the manager
/// calls that answer `Result<(), Status>` inline.
fn finish_unit_result(td: &mut Td, id: u64, result: std::result::Result<(), Status>) {
    match result {
        Ok(()) => td.send_result(id, ok_object()),
        Err(status) => td.send_error_impl(id, status.code(), status.message()),
    }
}

/// Field-split the `Td` for the manager calls that take `user_manager`
/// plus an `env` borrowing `global`/`option_manager`.
fn chat_env_split(
    td: &mut Td,
) -> (
    crate::user_manager::UserEnv<'_>,
    &mut crate::user_manager::UserManager,
    &mut crate::chat_manager::ChatManager,
) {
    let crate::td::Td {
        global,
        option_manager,
        auth_manager,
        user_manager,
        chat_manager,
        ..
    } = td;
    let env = crate::user_manager::UserEnv {
        global,
        options: option_manager
            .as_mut()
            .expect("option manager exists after init"),
        auth_is_bot: auth_manager.is_bot(),
    };
    (env, user_manager, chat_manager)
}

/// `CLEAN_INPUT_STRING` (Requests.cpp:1989-1992) — `None` means the error
/// was sent.
fn clean_or_utf8_error(td: &mut Td, id: u64, s: &str) -> Option<String> {
    match crate::misc::clean_input_string(s) {
        Some(cleaned) => Some(cleaned),
        None => {
            td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            None
        }
    }
}

/// The unit-shaped slot half of the collapse, aliased to match `ObjectSlot`.
type UnitSlot = std::sync::Arc<std::sync::Mutex<Option<Result<()>>>>;

/// The synchronous collapse of `CREATE_REQUEST_PROMISE` for `Promise<Unit>`
/// requests: the promise error surface lands in the slot; successes are
/// signaled by an empty slot and the caller sends the result itself.
fn unit_request_promise() -> (tdutils::promise::Promise<()>, UnitSlot) {
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let capture = std::sync::Arc::clone(&slot);
    let promise = tdutils::promise::PromiseCreator::lambda_result(move |result: Result<()>| {
        *capture.lock().unwrap() = Some(result);
    });
    (promise, slot)
}

/// The same collapse for object-returning requests: the value is the result.
type ObjectSlot = std::sync::Arc<std::sync::Mutex<Option<Result<crate::td_api::Object>>>>;

fn object_request_promise() -> (tdutils::promise::Promise<crate::td_api::Object>, ObjectSlot) {
    let slot: ObjectSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let capture = std::sync::Arc::clone(&slot);
    let promise = tdutils::promise::PromiseCreator::lambda_result(
        move |result: Result<crate::td_api::Object>| {
            *capture.lock().unwrap() = Some(result);
        },
    );
    (promise, slot)
}

/// Send the slot's error, if any; `true` means the error was sent.
fn fail_from_slot(td: &mut Td, id: u64, slot: &std::sync::Mutex<Option<Result<()>>>) -> bool {
    let outcome = slot.lock().unwrap().take();
    match outcome {
        Some(Result::Err(status)) => {
            td.send_error_impl(id, status.code(), status.message());
            true
        }
        _ => false,
    }
}

/// The synchronous collapse of `CREATE_OK_REQUEST_PROMISE`
/// (Requests.cpp:1923-1930): the error is sent as-is, and a success sends
/// `ok` — the C++ `create_ok_request_promise` answers `send_result(id, ok)`
/// on `set_value`.
fn finish_unit_slot(
    td: &mut Td,
    id: u64,
    slot: std::sync::Arc<std::sync::Mutex<Option<Result<()>>>>,
) {
    match slot.lock().unwrap().take() {
        Some(Result::Ok(())) => td.send_result(id, ok_object()),
        Some(Result::Err(status)) => td.send_error_impl(id, status.code(), status.message()),
        None => {}
    }
}

/// Send the object slot's result or error.
fn finish_object_slot(td: &mut Td, id: u64, slot: ObjectSlot) {
    match slot.lock().unwrap().take() {
        Some(Result::Ok(object)) => td.send_result(id, object),
        Some(Result::Err(status)) => td.send_error_impl(id, status.code(), status.message()),
        None => {}
    }
}

fn ok_object() -> Object {
    Object::ok(td_api::Ok)
}

/// The `OptionValue` → `Object` rewrap (same payload structs under the same
/// variant names).
pub(crate) fn option_value_to_object(value: td_api::OptionValue) -> Object {
    use td_api::OptionValue as V;
    match value {
        V::optionValueEmpty(v) => Object::optionValueEmpty(v),
        V::optionValueBoolean(v) => Object::optionValueBoolean(v),
        V::optionValueInteger(v) => Object::optionValueInteger(v),
        V::optionValueString(v) => Object::optionValueString(v),
    }
}

/// The `JsonValue` → `Object` rewrap (the Object umbrella flattens the
/// JSON constructors, so the polymorphic type has no single variant).
pub(crate) fn json_value_to_object(value: td_api::JsonValue) -> Object {
    use td_api::JsonValue as V;
    match value {
        V::jsonValueNull(v) => Object::jsonValueNull(v),
        V::jsonValueBoolean(v) => Object::jsonValueBoolean(v),
        V::jsonValueNumber(v) => Object::jsonValueNumber(v),
        V::jsonValueString(v) => Object::jsonValueString(v),
        V::jsonValueArray(v) => Object::jsonValueArray(v),
        V::jsonValueObject(v) => Object::jsonValueObject(v),
    }
}

#[cfg(test)]
mod request_census_tests {
    /// M.11.3 — the dispatcher census. The fixture
    /// (`testdata/request_census.txt`, one `name<TAB>state` line per td_api
    /// function) must cover the pinned scheme exactly: every generated
    /// function appears once with a known state.
    ///
    /// States: `sync` — answered by `synchronous_requests` (native), while
    /// `getOption` stays `dispatched` (its synchronous half is gated by
    /// `is_synchronous_option`); `dispatched` — a real `run_request` arm
    /// (the parked-net tails keep the C++ local behavior); `interim` — the
    /// 500 "TDLib function is not implemented yet" placeholder. Every
    /// manager flip moves its functions `interim → dispatched` (and bumps
    /// the counts below); the S74 corpus records the flipped functions'
    /// coverage.
    #[test]
    fn census_fixture_covers_every_scheme_function() {
        let raw = include_str!("testdata/request_census.txt");
        let mut names = std::collections::BTreeSet::new();
        let mut counts = std::collections::BTreeMap::new();
        for line in raw.lines() {
            let Some((name, state)) = line.split_once('\t') else {
                panic!("census line is not `name<TAB>state`: {line:?}");
            };
            assert!(
                matches!(state, "sync" | "dispatched" | "interim"),
                "unknown census state {state:?} for {name}"
            );
            assert!(names.insert(name), "duplicate census entry {name}");
            *counts.entry(state).or_insert(0usize) += 1;
        }

        // The fixture set equals the generated scheme set exactly.
        let generated: std::collections::BTreeSet<&str> =
            crate::td_api::ALL_FUNCTION_NAMES.iter().copied().collect();
        let fixture: std::collections::BTreeSet<&str> = names.into_iter().collect();
        let missing: Vec<&str> = generated.difference(&fixture).copied().collect();
        let stale: Vec<&str> = fixture.difference(&generated).copied().collect();
        assert!(
            missing.is_empty(),
            "census is missing functions: {missing:?}"
        );
        assert!(
            stale.is_empty(),
            "census carries stale functions: {stale:?}"
        );

        // The scheme size from the S07 differential dump.
        assert_eq!(generated.len(), crate::td_api::SCHEME_FUNCTIONS);

        // The flip-state ledger — re-derived from the fixture at the wave-3
// integration (the per-track flip histories compose).
assert_eq!(*counts.get("sync").unwrap_or(&0), 19, "sync count");
assert_eq!(
    *counts.get("dispatched").unwrap_or(&0),
    363,
    "dispatched count"
);
assert_eq!(
    *counts.get("interim").unwrap_or(&0),
    640,
    "interim count"
);
    }
}






