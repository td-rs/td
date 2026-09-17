//! Port of `td/telegram/Requests.{h,cpp}` (Track I, Td M.1 subset): the
//! `td_api` request dispatch. The C++ file is a ~9.5K-line per-function
//! `on_request` overload set that hands each request to its manager through
//! a `RequestActor`; the Rust dispatcher lives in one `run_request` match
//! and grows with every manager step.
//!
//! M.1 carries the handlers that need no network or manager actors: the
//! `test*` echo set, `getCurrentState`, `getAuthorizationState`, the
//! `close`/`destroy` pair, and the `Run`-state `setTdlibParameters`
//! rejection. Everything not yet ported answers the interim 500
//! "TDLib function is not implemented yet"; the interim handlers below the
//! `-- interim --` marker get their real bodies as their managers land.

use crate::chat_manager::{ChannelId, ChatId, DialogId};
use crate::td::Td;
use crate::td_api::{self, Function, Object};

/// The interim message of the S70 skeleton, kept for the not-yet-ported
/// handlers until the Waves complete.
const NOT_IMPLEMENTED_YET: &str = "TDLib function is not implemented yet";

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
            let mut updates = Vec::new();
            let unix_time = td.global.unix_time();
            if let Some(option_manager) = td.option_manager.as_mut() {
                option_manager.get_current_state(&mut updates, unix_time);
            }

            // the ChatManager state block (M.5.47): unknown placeholders,
            // then channels before chats, then fulls — see
            // ChatManager::get_current_state
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
                updates.extend(chat_updates.into_iter().map(Object::from));
            }

            if let Some(authorization_state) =
                td.auth_manager.get_current_authorization_state_object()
            {
                updates.push(Object::updateAuthorizationState(
                    td_api::updateAuthorizationState {
                        authorization_state,
                    },
                ));
            }

            // ConnectionStateManager and the authorized-manager state blocks
            // (community/user/chat/background/...) accumulate here as their
            // managers land.
            let updates: Vec<td_api::Update> = updates
                .into_iter()
                .map(|update| {
                    td_api::Update::try_from_object(update).expect("getCurrentState emits updates")
                })
                .collect();
            td.send_result(id, Object::updates(td_api::Updates { updates }));
        }

        // --- options (Requests.cpp:8076-8090) ---
        Function::getOption(request) => {
            let Some(name) = crate::misc::clean_input_string(&request.name) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
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
            match option_manager.set_option_from_api(&name, &request.value) {
                Ok(()) => {
                    td.flush_option_updates();
                    td.send_result(id, ok_object());
                }
                Err(error) => td.send_error_impl(id, 400, &error),
            }
        }

        // -- interim: the auth set is validated and routed in the C++ shape,
        //    but the network query layer lands with the ConfigManager step
        //    (every one of these needs auth.* NetQueries, logOut's local
        //    guards included). --
        Function::setAuthenticationPhoneNumber(request) => {
            // CLEAN_INPUT_STRING, then `AuthManager::set_phone_number`.
            let _ = crate::misc::clean_input_string(&request.phone_number);
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }
        Function::checkAuthenticationCode(request) => {
            let _ = crate::misc::clean_input_string(&request.code);
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }
        Function::checkAuthenticationPassword(request) => {
            let _ = crate::misc::clean_input_string(&request.password);
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }
        Function::logOut(_) => {
            // `AuthManager::log_out` — the local guards need the sender for
            // the query part; both arrive with the net step.
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }
        Function::setDatabaseEncryptionKey(_) => {
            // `G()->td_db()->get_binlog()->change_key(...)` — the binlog
            // re-key lands with the TdDb binlog-close wiring.
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }

        // --- the chat-manager request family (M.5.47) ---
        // Each local-validating entry settles its promise synchronously;
        // the net queries terminate in the blocked-net 500 until the net
        // step lands.
        Function::setChatDescription(request) => {
            let (promise, recorded) = record_unit_promise();
            let description =
                crate::misc::clean_input_string(&request.description).unwrap_or_default();
            let chat_id = ChatId::new(request.chat_id);
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.set_chat_description(
                user_manager,
                &mut env,
                chat_id,
                &description,
                promise,
            );
            finish_unit_request(id, td, recorded);
        }
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
            let location = crate::chat_manager::DialogLocation::from_td_api(&request.location);
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
        Function::deleteChat(request) => {
            let (promise, recorded) = record_unit_promise();
            let (mut env, _user_manager, chat_manager) = chat_env_split(td);
            chat_manager.delete_chat(&mut env, ChatId::new(request.chat_id), promise);
            finish_unit_request(id, td, recorded);
        }
        // Everything else waits for its manager step.
        _ => {
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }
    }
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
