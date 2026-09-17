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

use crate::contact::Contact;
use crate::td::Td;
use crate::td_api::{self, Function, Object};
use crate::user_manager::{FileId, PendingUserRequest, SecretChatId, UserEnv, UserId};
use tdutils::promise::Promise;
use tdutils::status::Status;

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
                .check_is_saved_music(FileId::new(request.file_id), &mut promise);
        }
        Function::setProfileAudioPosition(request) => {
            // Requests.cpp:7149-7155: validation is local; the promise
            // parks at the messages.saveMusic send ([blocked: net])
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            td.user_manager.add_saved_music(
                FileId::new(request.file_id),
                FileId::new(request.after_file_id),
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
                .remove_saved_music(FileId::new(request.file_id), promise);
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
fn get_contacts(imported: &[td_api::ImportedContact]) -> Result<Vec<Contact>, Status> {
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
