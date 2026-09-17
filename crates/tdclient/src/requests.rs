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

use crate::td::Td;
use crate::td_api::{self, Function, Object};
use tdutils::status::Result;

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
            let unix_time = td.get_unix_time();
            if let Some(option_manager) = td.option_manager.as_mut() {
                option_manager.get_current_state(&mut updates, unix_time);
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
            match option_manager.set_option_from_api(&name, &request.value) {
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

        // -- interim: the auth set is validated and routed in the C++ shape,
        //    but the network query layer lands with the net step (every one
        //    of these needs auth.* NetQueries, logOut's local guards
        //    included; the ConfigManager's own queries are held by the Td
        //    from this step on). --
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

        // --- M.6.20: the DialogManager-served requests
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
                Some(&request.type_filter),
                promise,
            );
            if fail_from_slot(td, id, &slot) {
                return;
            }
            let chats = crate::dialog_manager::get_chats_object(-1, &dialog_ids, "searchPublicChats");
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
                Some(&request.type_filter),
                request.limit,
                promise,
            );
            if fail_from_slot(td, id, &slot) {
                return;
            }
            let chats = crate::dialog_manager::get_chats_object(
                total_count,
                &dialog_ids,
                "searchChats",
            );
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
                Some(&request.type_filter),
                request.limit,
                promise,
            );
            if fail_from_slot(td, id, &slot) {
                return;
            }
            let chats = crate::dialog_manager::get_chats_object(
                -1,
                &dialog_ids,
                "searchChatsOnServer",
            );
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
                Some(&request.type_filter),
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
                Some(&request.photo),
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
                Some(&request.permissions),
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
                Some(crate::chat_manager::MessageId::new(request.request_message_id)),
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
                Some(&request.block_list),
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

/// The synchronous collapse of `CREATE_REQUEST_PROMISE` for `Promise<Unit>`
/// requests: the promise error surface lands in the slot; successes are
/// signaled by an empty slot and the caller sends the result itself.
fn unit_request_promise() -> (
    tdutils::promise::Promise<()>,
    std::sync::Arc<std::sync::Mutex<Option<Result<()>>>>,
) {
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let capture = std::sync::Arc::clone(&slot);
    let promise =
        tdutils::promise::PromiseCreator::lambda_result(move |result: Result<()>| {
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
fn fail_from_slot(
    td: &mut Td,
    id: u64,
    slot: &std::sync::Mutex<Option<Result<()>>>,
) -> bool {
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
fn finish_unit_slot(td: &mut Td, id: u64, slot: std::sync::Arc<std::sync::Mutex<Option<Result<()>>>>) {
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
