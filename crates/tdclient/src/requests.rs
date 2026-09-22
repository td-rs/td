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

use crate::chat_manager::{ChannelId, DialogId};
use crate::contact::Contact;
use crate::notification::{NotificationGroupId, NotificationId, NotificationObjectId};
use crate::notification_settings::{
    ReactionNotificationSettings, get_notification_settings_scope,
    get_scope_notification_settings_object,
};
use crate::notification_settings_manager::ScopeSettingsAnswer;
use crate::td::NotificationEnv;
use crate::td::Td;
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
        | Function::getSupportUser(_) => {
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

        // Everything else waits for its manager step.
        _ => {
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }
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

        // The flip-state ledger at the m13s↔master (4f716e8: M.7+M.11)
        // merge: the m11 pins + M.13/M.13S's seven real dispatches
        // (removeNotification, removeNotificationGroup, registerDevice and
        // the four notification-settings arms; processPushNotification
        // stays the 500 placeholder) + M.12.10's eighteen file-plane
        // dispatches (getFile/getRemoteFile, the download/upload/
        // generation arms, the DownloadManager list plane and the four
        // storage-statistics arms) + M.44's nine passport dispatches
        // (getPassportElement/getAllPassportElements/setPassportElement/
        // deletePassportElement/setPassportElementErrors/
        // getPreferredCountryLanguage and the three authorization-form
        // arms — the [blocked: net] query tails park). Fix per flip.
        assert_eq!(*counts.get("sync").unwrap_or(&0), 18, "sync count");
        assert_eq!(
            *counts.get("dispatched").unwrap_or(&0),
            119,
            "dispatched count"
        );
        assert_eq!(*counts.get("interim").unwrap_or(&0), 885, "interim count");
    }
}
