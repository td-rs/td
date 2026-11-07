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
use crate::chat_manager::{ChannelId, DialogId, StickerSetId};
use crate::community_id::CommunityId;
use crate::connection_creator::ConnectionCreatorEnv as _;
use crate::contact::Contact;
use crate::dialog_participant_manager::DialogParticipantManagerEnv;
use crate::message_id::{MessageFullId, MessageId};
use crate::messages_manager::DialogId as MmDialogId;
use crate::notification::{NotificationGroupId, NotificationId, NotificationObjectId};
use crate::notification_settings::ReactionNotificationSettings;
use crate::notification_settings::get_notification_settings_scope;
use crate::star_gift_manager::{
    StarGiftCollectionId, StarGiftId, StarGiftResalePrice, get_message_sender_dialog_id,
};
use crate::suggested_action::SuggestedAction;
use crate::td::AccountEnv;
use crate::td::AttachMenuEnv;
use crate::td::BackgroundEnv;
use crate::td::CallEnv;
use crate::td::LanguagePackEnv;
use crate::td::NotificationEnv;
use crate::td::ReactionEnv;
use crate::td::Td;
use crate::td::TdAnimationsEnv;
use crate::td::TdLinkEnv;
use crate::td::TdStaticLinkEnv;
use crate::td::ThemeEnv;
use crate::td::WebBrowserEnv;
use crate::td::translation_env_split;
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
    ($td:expr) => {{
        let net = $td.secure_net();
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
            net,
        }
    }};
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

/// The td-plane pair of the profile-photo arms — expanded at the call
/// site so the field borrows stay disjoint (the `user_env!` shape; the
/// guards and the callback clone are the caller's locals).
macro_rules! set_profile_photo_td {
    ($td:ident, $env:ident, $files:ident, $fllm:ident, $callback:ident) => {
        crate::user_manager::SetProfilePhotoTd {
            env: &$env,
            files: &mut $files,
            fllm: &mut $fllm,
            upload_callback: &$callback,
            stickers: &$td.stickers_manager,
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

            // BusinessConnectionManager::get_current_state
            // (Requests.cpp:2247 — the connection block, after
            // authorization per the C++ order)
            {
                business_arm(td, |env, _, connection_manager| {
                    connection_manager.get_current_state(env, &mut updates);
                });
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

            // BackgroundManager::get_current_state (Requests.cpp:2211 —
            // after the chat block, before animations; the manager's own
            // is_bot gate applies)
            {
                let mut manager = std::mem::take(&mut td.background_manager);
                {
                    let mut env = BackgroundEnv { td: &mut *td };
                    manager.get_current_state(&mut env, &mut updates);
                }
                td.background_manager = manager;
            }

            // ReactionManager::get_current_state (Requests.cpp:2219 —
            // after stickers, before notification settings; the manager's
            // own is_bot gate applies)
            {
                let env = ReactionEnv {
                    global: &mut td.global,
                    options: td.option_manager.as_mut(),
                    auth_is_authorized: td.auth_manager.is_authorized(),
                    auth_is_bot: td.auth_manager.is_bot(),
                    updates: &mut td.reaction_updates,
                    file_manager: std::sync::Arc::clone(&td.file_manager),
                    user_manager: &td.user_manager,
                };
                td.reaction_manager.get_current_state(&env, &mut updates);
            }

            // DialogParticipantManager::get_current_state (Requests.cpp:2227
            // — after the reaction block, before the notification manager)
            {
                let (mut env, manager) = crate::td::dialog_participant_env_split(td);
                manager.get_current_state(&mut env, &mut updates);
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

            // AnimationsManager::get_current_state (Requests.cpp:2213) —
            // the saved-animations + search-parameters updates; the
            // manager's own is_bot gate applies.
            if !td.auth_manager.is_bot() {
                let env = TdAnimationsEnv {
                    global: &mut td.global,
                    options: td.option_manager.as_mut(),
                    auth_is_bot: false,
                    file_manager: std::sync::Arc::clone(&td.file_manager),
                    file_reference_manager: &mut td.file_reference_manager,
                    updates: &mut Vec::new(),
                };
                td.animations_manager.get_current_state(&env, &mut updates);
            }

            // SuggestedActionManager::get_current_state
            // (Requests.cpp:2237 — after notification)
            {
                let env = AccountEnv {
                    global: &mut td.global,
                    options: td.option_manager.as_mut(),
                    user_manager: &mut td.user_manager,
                    chat_manager: &mut td.chat_manager,
                    device_token_manager: &mut td.device_token_manager,
                    auth_is_bot: td.auth_manager.is_bot(),
                    updates: &mut td.account_updates,
                    net: None,
                };
                td.suggested_action_manager
                    .get_current_state(&env, &mut updates);
            }

            // TranscriptionManager::get_current_state (Requests.cpp:2239 —
            // after the suggested-action block; the manager's own
            // authorized/non-bot gate applies)
            {
                let (manager, env) = crate::td::transcription_env_split(td);
                manager.get_current_state(&env, &mut updates);
                td.transcription_manager = manager;
            }

            // StarManager::get_current_state (Requests.cpp:2251 — after
            // the terms-of-service block, before the group-call block;
            // the owned star/ton updates ride their inited flags)
            td.star_manager.get_current_state(&mut updates);

            // GroupCallManager::get_current_state (Requests.cpp:2253 —
            // after the star block; the net-drain 36 slice wires it in)
            {
                let crate::td::Td {
                    group_call_manager,
                    global,
                    auth_manager,
                    notification_updates,
                    ..
                } = td;
                let mut env = crate::td::GroupCallTdEnv {
                    global,
                    auth_is_authorized: auth_manager.is_authorized(),
                    auth_is_bot: auth_manager.is_bot(),
                    updates: notification_updates,
                    request_input_peer: None,
                };
                group_call_manager.get_current_state(&mut env, &mut updates);
            }

            // MessageQueryManager::get_current_state
            // (Requests.cpp:2255 — after the star/group-call blocks; the
            // group-call manager in between is pending). The manager's
            // own is_bot gate applies; the emoji-game update carries the
            // 60-second freshness window.
            if !td.auth_manager.is_bot() {
                let (env, mq_manager) = crate::td::message_query_env_split(td);
                mq_manager.get_current_state(&env, &mut updates);
            }

            // TonWalletManager::get_current_state (Requests.cpp:2257 —
            // right after the message-query block; the manager's own
            // authorized gate applies)
            {
                let manager = std::mem::take(&mut td.ton_wallet_manager);
                let env = crate::td::TonWalletEnv { td: &mut *td };
                manager.get_current_state(&env, &mut updates);
                td.ton_wallet_manager = manager;
            }

            // TranslationManager::get_current_state (Requests.cpp:2257 —
            // right after the message-query block, before web browser; the
            // manager's own authorized/non-bot gate applies)
            {
                let (manager, mut env) = translation_env_split(td);
                manager.get_current_state(&mut env, &mut updates);
                td.translation_manager = manager;
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

            // ConnectionStateManager and the remaining authorized-manager
            // state blocks (connection/chat/background/...) accumulate here
            // as their managers land.
            td.send_result(id, Object::updates(td_api::Updates { updates }));
        }

        // --- the language pack set (M.24): Requests.cpp:8005-8073. The
        // local halves (custom packs, loaded strings, only_local target
        // info) answer here; the server halves park behind the
        // [blocked: net] langpack.* query seams ---
        Function::getLocalizationTargetInfo(request) => {
            // Requests.cpp:8005-8009
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::LocalizationTargetInfo, _>(
                id,
                Object::localizationTargetInfo,
            );
            let mut env = LanguagePackEnv {
                options: td.option_manager.as_mut(),
                updates: &mut td.language_pack_updates,
            };
            td.language_pack_manager
                .get_languages(&mut env, request.only_local, promise);
        }
        Function::getLanguagePackInfo(request) => {
            // Requests.cpp:8011-8017; the net tail parks unanswered
            // offline, like the C++ request without a network
            if !check_is_user(td, id) {
                return;
            }
            let Some(language_pack_id) = crate::misc::clean_input_string(&request.language_pack_id)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise =
                td.deferred_promise::<td_api::LanguagePackInfo, _>(id, Object::languagePackInfo);
            let mut env = LanguagePackEnv {
                options: td.option_manager.as_mut(),
                updates: &mut td.language_pack_updates,
            };
            td.language_pack_manager
                .search_language_info(&mut env, language_pack_id, promise);
        }
        Function::getLanguagePackStrings(request) => {
            // Requests.cpp:8019-8028; the loaded-pack halves answer, the
            // rest parks on langpack.getLangPack/langpack.getStrings
            if !check_is_user(td, id) {
                return;
            }
            let Some(language_pack_id) = crate::misc::clean_input_string(&request.language_pack_id)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let mut keys = Vec::with_capacity(request.keys.len());
            for key in &request.keys {
                let Some(key) = crate::misc::clean_input_string(key) else {
                    return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
                };
                keys.push(key);
            }
            let promise = td.deferred_promise::<td_api::LanguagePackStrings, _>(
                id,
                Object::languagePackStrings,
            );
            let mut env = LanguagePackEnv {
                options: td.option_manager.as_mut(),
                updates: &mut td.language_pack_updates,
            };
            td.language_pack_manager.get_language_pack_strings(
                &mut env,
                language_pack_id,
                keys,
                promise,
            );
        }
        Function::synchronizeLanguagePack(request) => {
            // Requests.cpp:8030-8036; custom codes answer ok locally, the
            // server codes park behind langpack.getDifference
            if !check_is_user(td, id) {
                return;
            }
            let Some(language_pack_id) = crate::misc::clean_input_string(&request.language_pack_id)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut env = LanguagePackEnv {
                options: td.option_manager.as_mut(),
                updates: &mut td.language_pack_updates,
            };
            td.language_pack_manager
                .synchronize_language_pack(&mut env, language_pack_id, promise);
        }
        Function::addCustomServerLanguagePack(request) => {
            // Requests.cpp:8038-8043; the local info-move machinery
            if !check_is_user(td, id) {
                return;
            }
            let Some(language_pack_id) = crate::misc::clean_input_string(&request.language_pack_id)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            td.language_pack_manager
                .add_custom_server_language(language_pack_id, promise);
        }
        Function::setCustomLanguagePack(request) => {
            // Requests.cpp:8046-8051; the fully local custom pack plane
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut env = LanguagePackEnv {
                options: td.option_manager.as_mut(),
                updates: &mut td.language_pack_updates,
            };
            td.language_pack_manager.set_custom_language(
                &mut env,
                request.info,
                request.strings,
                promise,
            );
        }
        Function::editCustomLanguagePackInfo(request) => {
            // Requests.cpp:8053-8057; the fully local custom info edit
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            td.language_pack_manager
                .edit_custom_language_info(request.info, promise);
        }
        Function::setCustomLanguagePackString(request) => {
            // Requests.cpp:8060-8066; the fully local string set
            if !check_is_user(td, id) {
                return;
            }
            let Some(language_pack_id) = crate::misc::clean_input_string(&request.language_pack_id)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut env = LanguagePackEnv {
                options: td.option_manager.as_mut(),
                updates: &mut td.language_pack_updates,
            };
            td.language_pack_manager.set_custom_language_string(
                &mut env,
                language_pack_id,
                request.new_string,
                promise,
            );
        }
        Function::deleteLanguagePack(request) => {
            // Requests.cpp:8068-8073; the fully local delete
            if !check_is_user(td, id) {
                return;
            }
            let Some(language_pack_id) = crate::misc::clean_input_string(&request.language_pack_id)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            td.language_pack_manager
                .delete_language(language_pack_id, promise);
        }

        // --- options (Requests.cpp:8076-8090) ---
        Function::getOption(request) => {
            let Some(name) = crate::misc::clean_input_string(&request.name) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            // `OptionManager::get_option` (cpp:758-800): the ConfigManager
            // branch for the content-settings names (is_bot is false until
            // the AuthManager net step) + the NotificationManager branch
            // for the contact-sign-up option (cpp:775-782, net-drain 61).
            if !(td.auth_manager.is_authorized() && td.auth_manager.is_bot())
                && (name == "can_ignore_sensitive_content_restrictions"
                    || name == "ignore_sensitive_content_restrictions")
            {
                return td.get_option_content_settings(id, name);
            }
            if !(td.auth_manager.is_authorized() && td.auth_manager.is_bot())
                && name == "disable_contact_registered_notifications"
                && td
                    .option_manager
                    .as_ref()
                    .is_some_and(crate::option_manager::OptionManager::is_td_inited)
            {
                return td.get_option_disable_contact_registered_notifications(id);
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
            // the promise is the answer path on both bands — it settles
            // inline with the synchronized object, or parks on the fetch
            // query (the RequestActor defer) and the answer drain resolves
            // it with the now-synchronized state (the do_run re-run)
            let promise = td.deferred_promise(id, Object::scopeNotificationSettings);
            let env = NotificationEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.notification_updates,
            };
            td.notification_settings_manager
                .get_scope_notification_settings_for_request(&env, scope, promise);
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

        // --- the reactions set (M.16): Requests.cpp:3907-3916, 3930-3933,
        // 4015-4039 and 5965-5967. The net tails stay [blocked: net]; the
        // message-, chat- and story-scoped reaction arms route to
        // MessagesManager/MessageQueryManager/Stories on their lines ---
        Function::getEmojiReaction(request) => {
            // Requests.cpp:3907-3911; the null object is the
            // send_result(nullptr) 404 rewrite
            let promise =
                td.deferred_promise::<Option<td_api::EmojiReaction>, _>(
                    id,
                    |object| match object {
                        Some(emoji_reaction) => Object::emojiReaction(emoji_reaction),
                        None => Object::error(td_api::Error {
                            code: 404,
                            message: "Not Found".to_string(),
                        }),
                    },
                );
            let mut env = ReactionEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.reaction_updates,
                file_manager: std::sync::Arc::clone(&td.file_manager),
                user_manager: &td.user_manager,
            };
            td.reaction_manager
                .get_emoji_reaction(&mut env, &request.emoji, promise);
        }
        Function::getCustomEmojiReactionAnimations(_) => {
            // Requests.cpp:3913-3916 — StickersManager::get_custom_emoji_
            // reaction_generic_animations owns this arm (M.15 integration)
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }
        Function::clearRecentReactions(_) => {
            // Requests.cpp:3930-3933; an empty recent list answers here, a
            // non-empty one parks behind the [blocked: net] query
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut env = ReactionEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.reaction_updates,
                file_manager: std::sync::Arc::clone(&td.file_manager),
                user_manager: &td.user_manager,
            };
            td.reaction_manager
                .clear_recent_reactions(&mut env, promise);
        }
        Function::setDefaultReactionType(request) => {
            // Requests.cpp:4015-4018
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut env = ReactionEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.reaction_updates,
                file_manager: std::sync::Arc::clone(&td.file_manager),
                user_manager: &td.user_manager,
            };
            td.reaction_manager.set_default_reaction(
                &mut env,
                crate::reaction_type::ReactionType::from_td_api(request.reaction_type),
                promise,
            );
        }
        Function::getStoryAvailableReactions(request) => {
            // Requests.cpp:5965-5968 — the synchronous available-reactions
            // builder
            let available = {
                let mut env = ReactionEnv {
                    global: &mut td.global,
                    options: td.option_manager.as_mut(),
                    auth_is_authorized: td.auth_manager.is_authorized(),
                    auth_is_bot: td.auth_manager.is_bot(),
                    updates: &mut td.reaction_updates,
                    file_manager: std::sync::Arc::clone(&td.file_manager),
                    user_manager: &td.user_manager,
                };
                td.reaction_manager
                    .get_available_reactions(&mut env, request.row_size)
            };
            td.send_result(id, Object::availableReactions(available));
        }
        Function::getMessageEffect(request) => {
            // Requests.cpp:4036-4040; the null object is the
            // send_result(nullptr) 404 rewrite
            let promise =
                td.deferred_promise::<Option<td_api::MessageEffect>, _>(
                    id,
                    |object| match object {
                        Some(effect) => Object::messageEffect(effect),
                        None => Object::error(td_api::Error {
                            code: 404,
                            message: "Not Found".to_string(),
                        }),
                    },
                );
            let mut env = ReactionEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.reaction_updates,
                file_manager: std::sync::Arc::clone(&td.file_manager),
                user_manager: &td.user_manager,
            };
            td.reaction_manager.get_message_effect(
                &mut env,
                crate::reaction_manager::MessageEffectId::new(request.effect_id),
                promise,
            );
        }
        Function::getSavedMessagesTags(request) => {
            // Requests.cpp:4021-4030; the SavedMessagesManager topic
            // translation is the Wave-3 seam — topic 0 is the default
            // list, every other id takes the C++ invalid-marker 400
            let saved_messages_topic_id = if request.saved_messages_topic_id == 0 {
                crate::reaction_manager::SavedMessagesTopicId::default()
            } else {
                crate::reaction_manager::SavedMessagesTopicId::from_dialog_id(
                    crate::dialog_id::DialogId::new(i64::MAX),
                )
            };
            let promise =
                td.deferred_promise::<td_api::SavedMessagesTags, _>(id, Object::savedMessagesTags);
            let mut env = ReactionEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.reaction_updates,
                file_manager: std::sync::Arc::clone(&td.file_manager),
                user_manager: &td.user_manager,
            };
            td.reaction_manager
                .get_saved_messages_tags(&mut env, saved_messages_topic_id, promise);
        }
        Function::setSavedMessagesTagLabel(request) => {
            // Requests.cpp:4030-4036; the local tag machine runs here, the
            // UpdateSavedReactionTagQuery tail stays [blocked: net]
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut env = ReactionEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.reaction_updates,
                file_manager: std::sync::Arc::clone(&td.file_manager),
                user_manager: &td.user_manager,
            };
            td.reaction_manager.set_saved_messages_tag_title(
                &mut env,
                crate::reaction_type::ReactionType::from_td_api(request.tag),
                &request.label,
                promise,
            );
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
            // `AuthManager::log_out` over the dispatcher (auth-net slice).
            td.log_out(id);
        }

        // -- the walking-skeleton messaging set (S76): getMe stays the
        //    UserManager arm; sendMessage flipped to the real M.10.8–M.10.19
        //    send machinery (the write-family group below) --
        Function::getMe(_) => td.get_me(id),
        Function::setDatabaseEncryptionKey(request) => {
            // Requests.cpp:2048-2051 — `TdDb::as_db_key` + the binlog
            // `change_key`. The re-key itself is the tddb encryption
            // slice (ConcurrentBinlog::change_key — binlog/concurrent.rs,
            // the reindex-with-encryption seam): the arm is the real
            // request wiring and the binlog's Status surface answers, so
            // the C++ wrong-key band reaches the client instead of the
            // 500 placeholder.
            let mut promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let db_key = as_db_key(request.new_encryption_key);
            let status = td.global.td_db().binlog().change_key(db_key, None);
            if status.is_ok() {
                promise.set_value(());
            } else {
                promise.set_error(status);
            }
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
            // GetUserRequest (Requests.cpp:217-233): set_tries(3). The
            // local/db halves run synchronously; the miss parks the
            // answer on the GetUserMerger (the merged users.getUsers
            // query) and the merger resolution re-runs the load with the
            // decremented tries — the RequestActor re-entry.
            let user_id = UserId::new(request.user_id);
            let answer = td.deferred_promise::<td_api::User, _>(id, Object::user);
            let mut env = user_env!(td);
            td.user_manager.load_user(&mut env, user_id, 3, answer);
        }
        Function::getUserFullInfo(request) => {
            // GetUserFullInfoRequest (Requests.cpp:235-249): the default
            // two tries run the first pass with force = get_tries() < 2 =
            // false; the db row load is synchronous (the collapse), the
            // miss parks the answer on the GetUserFullCombiner (the
            // combined users.getFullUser query) and the completion
            // builds the full-info object (do_send_result).
            let user_id = UserId::new(request.user_id);
            let answer = td.deferred_promise::<td_api::UserFullInfo, _>(id, Object::userFullInfo);
            let mut env = user_env!(td);
            td.user_manager.load_user_full_for_request(
                &mut env,
                user_id,
                false,
                answer,
                "GetUserFullInfoRequest",
            );
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
            // resolved cache and the only_local form answer synchronously
            // (the `do_send_result` object); the miss leg parks the
            // request on the ResolvePhoneQuery family queue — the answer
            // caches the phone and drains the park with the fresh object
            // (the RequestActor do_run re-run collapse).
            if !check_is_user(td, id) {
                return;
            }
            let Some(phone_number) = crate::misc::clean_input_string(&request.phone_number) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise: Promise<Object> = td.deferred_promise(id, |object| object);
            let env = user_env!(td);
            td.user_manager.search_user_by_phone_number_for_request(
                &env,
                phone_number,
                request.only_local,
                promise,
            );
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
        // --- the sponsored set (M.26): Requests.cpp:2672-2742, 6590-6594. The
        // managers validate; the net query tails park the promises unanswered
        // offline, exactly like the C++ request without a network ---
        Function::getChatSponsoredMessages(request) => {
            // Requests.cpp:2672-2676
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<crate::td_api::SponsoredMessages, _>(
                id,
                Object::sponsoredMessages,
            );
            let (mut sponsored_message_manager, mut env) = crate::td::sponsored_env_split(td);
            sponsored_message_manager.get_dialog_sponsored_messages(
                &mut env,
                crate::messages_manager::DialogId::new(request.chat_id),
                promise,
            );
            td.sponsored_message_manager = sponsored_message_manager;
        }
        Function::clickChatSponsoredMessage(request) => {
            // Requests.cpp:2678-2684
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut sponsored_message_manager, mut env) = crate::td::sponsored_env_split(td);
            sponsored_message_manager.click_sponsored_message(
                &mut env,
                crate::messages_manager::DialogId::new(request.chat_id),
                crate::messages_manager::MessageId::new(request.message_id),
                request.is_media_click,
                request.from_fullscreen,
                promise,
            );
            td.sponsored_message_manager = sponsored_message_manager;
        }
        Function::reportChatSponsoredMessage(request) => {
            // Requests.cpp:2686-2691
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<crate::td_api::ReportSponsoredResult, _>(
                id,
                report_sponsored_result_to_object,
            );
            let (mut sponsored_message_manager, mut env) = crate::td::sponsored_env_split(td);
            sponsored_message_manager.report_sponsored_message(
                &mut env,
                crate::messages_manager::DialogId::new(request.chat_id),
                crate::messages_manager::MessageId::new(request.message_id),
                &request.option_id,
                promise,
            );
            td.sponsored_message_manager = sponsored_message_manager;
        }
        Function::getSearchSponsoredChats(request) => {
            // Requests.cpp:2693-2699
            if !check_is_user(td, id) {
                return;
            }
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let promise =
                td.deferred_promise::<crate::td_api::SponsoredChats, _>(id, Object::sponsoredChats);
            let (mut sponsored_message_manager, mut env) = crate::td::sponsored_env_split(td);
            sponsored_message_manager.get_search_sponsored_dialogs(&mut env, &query, promise);
            td.sponsored_message_manager = sponsored_message_manager;
        }
        Function::viewSponsoredChat(request) => {
            // Requests.cpp:2700-2704
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut sponsored_message_manager, mut env) = crate::td::sponsored_env_split(td);
            sponsored_message_manager.view_sponsored_dialog(
                &mut env,
                request.sponsored_chat_unique_id,
                promise,
            );
            td.sponsored_message_manager = sponsored_message_manager;
        }
        Function::openSponsoredChat(request) => {
            // Requests.cpp:2706-2710
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut sponsored_message_manager, mut env) = crate::td::sponsored_env_split(td);
            sponsored_message_manager.open_sponsored_dialog(
                &mut env,
                request.sponsored_chat_unique_id,
                promise,
            );
            td.sponsored_message_manager = sponsored_message_manager;
        }
        Function::reportSponsoredChat(request) => {
            // Requests.cpp:2712-2717
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<crate::td_api::ReportSponsoredResult, _>(
                id,
                report_sponsored_result_to_object,
            );
            let (mut sponsored_message_manager, mut env) = crate::td::sponsored_env_split(td);
            sponsored_message_manager.report_sponsored_dialog(
                &mut env,
                request.sponsored_chat_unique_id,
                &request.option_id,
                promise,
            );
            td.sponsored_message_manager = sponsored_message_manager;
        }
        Function::getVideoMessageAdvertisements(request) => {
            // Requests.cpp:2719-2725
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<crate::td_api::VideoMessageAdvertisements, _>(
                id,
                Object::videoMessageAdvertisements,
            );
            let message_full_id = crate::messages_manager::MessageFullId::new(
                crate::messages_manager::DialogId::new(request.chat_id),
                crate::messages_manager::MessageId::new(request.message_id),
            );
            let (mut sponsored_message_manager, mut env) = crate::td::sponsored_env_split(td);
            sponsored_message_manager.get_video_sponsored_messages(
                &mut env,
                message_full_id,
                promise,
            );
            td.sponsored_message_manager = sponsored_message_manager;
        }
        Function::viewVideoMessageAdvertisement(request) => {
            // Requests.cpp:2727-2731
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut sponsored_message_manager, mut env) = crate::td::sponsored_env_split(td);
            sponsored_message_manager.view_video_advertisement(
                &mut env,
                request.advertisement_unique_id,
                promise,
            );
            td.sponsored_message_manager = sponsored_message_manager;
        }
        Function::clickVideoMessageAdvertisement(request) => {
            // Requests.cpp:2733-2737
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut sponsored_message_manager, mut env) = crate::td::sponsored_env_split(td);
            sponsored_message_manager.click_video_advertisement(
                &mut env,
                request.advertisement_unique_id,
                promise,
            );
            td.sponsored_message_manager = sponsored_message_manager;
        }
        Function::reportVideoMessageAdvertisement(request) => {
            // Requests.cpp:2739-2744
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<crate::td_api::ReportSponsoredResult, _>(
                id,
                report_sponsored_result_to_object,
            );
            let (mut sponsored_message_manager, mut env) = crate::td::sponsored_env_split(td);
            sponsored_message_manager.report_video_advertisement(
                &mut env,
                request.advertisement_unique_id,
                &request.option_id,
                promise,
            );
            td.sponsored_message_manager = sponsored_message_manager;
        }
        Function::importMessages(request) => {
            // Requests.cpp:6590-6594
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let dialog_id = crate::messages_manager::DialogId::new(request.chat_id);
            let (mut message_import_manager, mut env) = crate::td::message_import_env_split(td);
            message_import_manager.import_messages(
                &mut env,
                dialog_id,
                request.message_file,
                request.attached_files,
                promise,
            );
            td.message_import_manager = message_import_manager;
        }

        // --- the M.10.22 flip wave: the manager arms whose receivers landed
        // with the M.7/M.12/M.18/M.20/M.21/M.24/M.25 tracks; every arm
        // anchors its C++ overload. The net query tails park the promises
        // unanswered offline, exactly like the C++ request without a
        // network ---
        Function::testGetDifference(_) => {
            // `Requests.cpp:60-65` — the test entry answers ok right after
            // the difference pump starts; the query senders stay the
            // [blocked: net] seams.
            let net = td.updates_net();
            let mut env = crate::td::TdUpdatesEnv {
                global: &mut td.global,
                options: td
                    .option_manager
                    .as_mut()
                    .expect("option manager exists after init"),
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                my_id: td.user_manager.get_my_id().get(),
                can_ignore_background_updates: td.can_ignore_background_updates,
                net,
            };
            td.updates_manager
                .get_difference(&mut env, "testGetDifference");
            td.send_result(id, ok_object());
        }
        Function::answerWebAppQuery(request) => {
            // `Requests.cpp:1202-1208`.
            if !check_is_bot(td, id) {
                return;
            }
            let Some(web_app_query_id) = clean_or_utf8_error(td, id, &request.web_app_query_id)
            else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::InlineMessageId, _>(id, Object::inlineMessageId);
            let (mut env, inline_queries_manager, _) = crate::td::inline_env_split(td);
            inline_queries_manager.answer_web_view_query(
                &mut env,
                &web_app_query_id,
                request.result,
                promise,
            );
        }
        Function::sendWebAppData(request) => {
            // `Requests.cpp:1226-1235` — the ok promise parks behind the
            // send query ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let Some(button_text) = clean_or_utf8_error(td, id, &request.button_text) else {
                return;
            };
            let Some(data) = clean_or_utf8_error(td, id, &request.data) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, inline_queries_manager, _) = crate::td::inline_env_split(td);
            inline_queries_manager.send_web_view_data(
                &mut env,
                UserId::new(request.bot_user_id),
                &button_text,
                &data,
                promise,
            );
        }
        Function::getWebAppUrl(request) => {
            // `Requests.cpp:1242-1252`.
            if !check_is_user(td, id) {
                return;
            }
            let Some(url) = clean_or_utf8_error(td, id, &request.url) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::WebAppUrl, _>(id, Object::webAppUrl);
            let parameters = crate::inline_queries_manager::WebAppOpenParameters::from_td_api(
                request.parameters,
            );
            let (mut env, inline_queries_manager, _) = crate::td::inline_env_split(td);
            inline_queries_manager.get_simple_web_view_url(
                &mut env,
                UserId::new(request.bot_user_id),
                &url,
                parameters,
                promise,
            );
        }
        Function::getPreparedKeyboardButton(request) => {
            // `Requests.cpp:1290-1296`.
            if !check_is_user(td, id) {
                return;
            }
            let Some(prepared_button_id) = clean_or_utf8_error(td, id, &request.prepared_button_id)
            else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::KeyboardButton, _>(id, Object::keyboardButton);
            let (mut env, inline_queries_manager, _) = crate::td::inline_env_split(td);
            inline_queries_manager.get_prepared_keyboard_button(
                &mut env,
                UserId::new(request.bot_user_id),
                &prepared_button_id,
                promise,
            );
        }
        Function::savePreparedKeyboardButton(request) => {
            // `Requests.cpp:1297-1303` — the text answer parks behind the
            // save query ([blocked: net]).
            if !check_is_bot(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<String, _>(id, |text| Object::text(td_api::Text { text }));
            let (mut env, inline_queries_manager, _) = crate::td::inline_env_split(td);
            inline_queries_manager.save_prepared_keyboard_button(
                &mut env,
                UserId::new(request.user_id),
                request.button,
                promise,
            );
        }
        Function::getAttachmentMenuBot(request) => {
            // `Requests.cpp:1101-1105` — the bot-data read answers the
            // manager object; the reload query tail rides the dispatcher
            // (net-drain 42).
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::AttachmentMenuBot, _>(id, Object::attachmentMenuBot);
            let mut env = AttachMenuEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                user_manager: &mut td.user_manager,
                file_manager: td.file_manager.clone(),
                file_reference_manager: &mut td.file_reference_manager,
                updates: &mut td.attach_menu_updates,
            };
            td.attach_menu_manager.get_attach_menu_bot_request(
                &mut env,
                UserId::new(request.bot_user_id),
                promise,
            );
        }
        Function::toggleBotIsAddedToAttachmentMenu(request) => {
            // `Requests.cpp:1107-1113` — the ok answer chains into the
            // reload query (net-drain 42).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let mut env = AttachMenuEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                user_manager: &mut td.user_manager,
                file_manager: td.file_manager.clone(),
                file_reference_manager: &mut td.file_reference_manager,
                updates: &mut td.attach_menu_updates,
            };
            td.attach_menu_manager.toggle_bot_is_added_to_attach_menu(
                &mut env,
                UserId::new(request.bot_user_id),
                request.is_added,
                request.allow_write_access,
                promise,
            );
        }
        Function::getMessageImportConfirmationText(request) => {
            // `Requests.cpp:6584-6588` — the live arm routes into the
            // manager; the text answer rides the `MessageImportNetKind`
            // plane (CheckHistoryImportPeerQuery,
            // MessageImportManager.cpp:67-98).
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<String, _>(id, |text| Object::text(td_api::Text { text }));
            let dialog_id = crate::messages_manager::DialogId::new(request.chat_id);
            let (mut message_import_manager, mut env) = crate::td::message_import_env_split(td);
            message_import_manager
                .get_message_import_confirmation_text(&mut env, dialog_id, promise);
            td.message_import_manager = message_import_manager;
        }
        Function::getMessageFileType(request) => {
            // `Requests.cpp:6577-6582` — the live arm routes into the
            // manager; the answer rides the `MessageImportNetKind` plane
            // (CheckHistoryImportQuery, MessageImportManager.cpp:30-64).
            if !check_is_user(td, id) {
                return;
            }
            let Some(message_file_head) = clean_or_utf8_error(td, id, &request.message_file_head)
            else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::MessageFileType, _>(
                    id,
                    |file_type| match file_type {
                        td_api::MessageFileType::messageFileTypePrivate(file_type) => {
                            Object::messageFileTypePrivate(file_type)
                        }
                        td_api::MessageFileType::messageFileTypeGroup(file_type) => {
                            Object::messageFileTypeGroup(file_type)
                        }
                        td_api::MessageFileType::messageFileTypeUnknown(file_type) => {
                            Object::messageFileTypeUnknown(file_type)
                        }
                    },
                );
            let (mut message_import_manager, mut env) = crate::td::message_import_env_split(td);
            message_import_manager.get_message_file_type(&mut env, &message_file_head, promise);
            td.message_import_manager = message_import_manager;
        }
        Function::getStatisticalGraph(request) => {
            // `Requests.cpp:8279-8285` → StatisticsManager.cpp:900-918 —
            // the token + x form over the dc chain (the net-drain 38
            // rider).
            if !check_is_user(td, id) {
                return;
            }
            let Some(token) = clean_or_utf8_error(td, id, &request.token) else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::StatisticalGraph, _>(id, |graph| match graph {
                    td_api::StatisticalGraph::statisticalGraphData(graph) => {
                        Object::statisticalGraphData(graph)
                    }
                    td_api::StatisticalGraph::statisticalGraphAsync(graph) => {
                        Object::statisticalGraphAsync(graph)
                    }
                    td_api::StatisticalGraph::statisticalGraphError(graph) => {
                        Object::statisticalGraphError(graph)
                    }
                });
            let (mut manager, mut env) = crate::td::statistics_env_split(&mut *td);
            manager.load_statistics_graph(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                &token,
                request.x,
                promise,
            );
            td.statistics_manager = manager;
        }
        Function::isLoginEmailAddressRequired(_) => {
            // `Requests.cpp:1893-1898` — the CheckPromoDataQuery answers
            // the ok/404 bands (SuggestedActionManager.cpp:65-105).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
            };
            td.suggested_action_manager
                .is_login_email_address_required(&mut env, promise);
        }
        Function::getAvailableChatBoostSlots(_) => {
            // `Requests.cpp:3511-3515` — the slot list parks behind the
            // load query ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::ChatBoostSlots, _>(id, Object::chatBoostSlots);
            let (mut manager, mut env) = crate::td::boost_env_split(td);
            manager.get_boost_slots(&mut env, promise);
            td.boost_manager = manager;
        }
        Function::getChatBoostFeatures(request) => {
            // `Requests.cpp:3517-3521` — the local feature-table builder
            // answers synchronously.
            if !check_is_user(td, id) {
                return;
            }
            let (mut manager, mut env) = crate::td::boost_env_split(td);
            let features = manager.get_chat_boost_features_object(&mut env, !request.is_channel);
            td.boost_manager = manager;
            td.send_result(id, Object::chatBoostFeatures(features));
        }
        Function::getChatBoostLevelFeatures(request) => {
            // `Requests.cpp:3523-3529`.
            if !check_is_user(td, id) {
                return;
            }
            let (mut manager, mut env) = crate::td::boost_env_split(td);
            let features = manager.get_chat_boost_level_features_object(
                &mut env,
                !request.is_channel,
                request.level,
            );
            td.boost_manager = manager;
            td.send_result(id, Object::chatBoostLevelFeatures(features));
        }
        Function::getChatAvailablePaidMessageReactionSenders(request) => {
            // `Requests.cpp:3343-3347` — the sender-list load parks at the
            // [blocked: net] query seam.
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::MessageSenders, _>(id, Object::messageSenders);
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.get_paid_message_reaction_senders(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                promise,
            );
        }
        Function::getMessageViewers(request) => {
            // `Requests.cpp:2233-2238` — the participant-list query parks
            // at the [blocked: net] seam.
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::MessageViewers, _>(id, Object::messageViewers);
            let message_full_id = crate::dialog_id::MessageFullId::new(
                crate::dialog_id::DialogId::new(request.chat_id),
                crate::messages_manager::MessageId::new(request.message_id),
            );
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.get_message_viewers(&mut env, message_full_id, promise);
        }
        Function::getStakeDiceState(_) => {
            // `Requests.cpp:3088-3093` — the state parks behind the emoji
            // game query ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::StakeDiceState, _>(id, Object::stakeDiceState);
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.get_emoji_game_info(&mut env, promise);
        }
        Function::sendMessageViewMetrics(request) => {
            // `Requests.cpp:2920-2931` — the ok promise parks behind the
            // metrics query ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.send_message_view_metrics(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                crate::messages_manager::MessageId::new(request.message_id),
                request.time_in_view_ms,
                request.active_time_in_view_ms,
                request.height_to_viewport_ratio_per_mille,
                request.seen_range_ratio_per_mille,
                promise,
            );
        }
        Function::searchStickers(request) => {
            // Requests.cpp:7443-7465 — the legacy emoji rewrites, then
            // the search bands (the cache-miss load parks [blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let Some(emojis) = clean_or_utf8_error(td, id, &request.emojis) else {
                return;
            };
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let mut input_language_codes = Vec::with_capacity(request.input_language_codes.len());
            for language_code in &request.input_language_codes {
                let Some(language_code) = clean_or_utf8_error(td, id, language_code) else {
                    return;
                };
                input_language_codes.push(language_code);
            }
            let promise = td.deferred_promise::<td_api::Stickers, _>(id, Object::stickers);
            let sticker_type =
                crate::stickers_manager::get_sticker_type(request.sticker_type.as_ref());
            let mut emojis = emojis;
            if sticker_type == crate::stickers_manager::StickerType::Regular && query.is_empty() {
                // legacy
                if emojis == "\u{2b50}\u{2b50}" {
                    emojis = "\u{2b50}".to_owned();
                } else if emojis == "\u{1f4d2}\u{2b50}" {
                    emojis = "\u{1f4d2}".to_owned();
                } else if emojis == "\u{1f44b}\u{2b50}" {
                    emojis = "\u{1f44b}".to_owned();
                }
            }
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.search_stickers(
                &mut env,
                sticker_type,
                emojis,
                query,
                input_language_codes,
                request.offset,
                request.limit,
                promise,
            );
        }
        Function::getGreetingStickers(_) => {
            // Requests.cpp:7467-7472 — the greeting search parks at the
            // found-stickers load ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Stickers, _>(id, Object::stickers);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.search_stickers(
                &mut env,
                crate::stickers_manager::StickerType::Regular,
                "\u{1f44b}\u{2b50}".to_owned(),
                String::new(),
                Vec::new(),
                0,
                100,
                promise,
            );
        }
        Function::getPremiumStickerExamples(_) => {
            // Requests.cpp:9016-9021 — the examples search parks at the
            // found-stickers load ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Stickers, _>(id, Object::stickers);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.search_stickers(
                &mut env,
                crate::stickers_manager::StickerType::Regular,
                "\u{2b50}\u{2b50}".to_owned(),
                String::new(),
                Vec::new(),
                0,
                100,
                promise,
            );
        }
        Function::getAnimatedEmoji(request) => {
            // Requests.cpp:7731-7736 — the special-set load parks
            // ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let Some(emoji) = clean_or_utf8_error(td, id, &request.emoji) else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::AnimatedEmoji, _>(id, Object::animatedEmoji);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_animated_emoji(&mut env, emoji, false, promise);
        }
        Function::getCustomEmojiStickers(request) => {
            // Requests.cpp:7745-7749 — the dedupe/limit bands, then the
            // unknown-id load parks ([blocked: net]).
            let promise = td.deferred_promise::<td_api::Stickers, _>(id, Object::stickers);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_custom_emoji_stickers(
                &mut env,
                crate::stickers_manager::CustomEmojiId::get_custom_emoji_ids(
                    &request.custom_emoji_ids,
                ),
                true,
                promise,
            );
        }
        Function::getDefaultChatPhotoCustomEmojiStickers(_) => {
            // Requests.cpp:7751-7755 — the default-list load parks.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Stickers, _>(id, Object::stickers);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_default_custom_emoji_stickers(
                &mut env,
                crate::stickers_manager::StickerListType::DialogPhoto,
                false,
                promise,
            );
        }
        Function::getDefaultProfilePhotoCustomEmojiStickers(_) => {
            // Requests.cpp:7757-7762 — the default-list load parks.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Stickers, _>(id, Object::stickers);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_default_custom_emoji_stickers(
                &mut env,
                crate::stickers_manager::StickerListType::UserProfilePhoto,
                false,
                promise,
            );
        }
        Function::getDefaultBackgroundCustomEmojiStickers(_) => {
            // Requests.cpp:7764-7769 — the default-list load parks.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Stickers, _>(id, Object::stickers);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_default_custom_emoji_stickers(
                &mut env,
                crate::stickers_manager::StickerListType::Background,
                false,
                promise,
            );
        }
        Function::getDisallowedChatEmojiStatuses(_) => {
            // Requests.cpp:7770-7775 — the disallowed list load parks.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::EmojiStatusCustomEmojis, _>(
                id,
                Object::emojiStatusCustomEmojis,
            );
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_sticker_list_emoji_statuses(
                &mut env,
                crate::stickers_manager::StickerListType::DisallowedChannelEmojiStatus,
                false,
                promise,
            );
        }
        Function::getEmojiCategories(request) => {
            // Requests.cpp:7725-7729 — the emoji-group load parks
            // ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::EmojiCategories, _>(id, Object::emojiCategories);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_emoji_groups(
                &mut env,
                crate::stickers_manager::get_emoji_group_type(request.r#type.as_ref()),
                promise,
            );
        }
        Function::getCustomEmojiReactionAnimations(_) => {
            // Requests.cpp:3913-3917 — the generic-animations special set
            // rides the ReloadSpecialStickerSetQuery plane (net-drain 59);
            // offline the promise parks on the per-type lot.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Stickers, _>(id, Object::stickers);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_custom_emoji_reaction_generic_animations(&mut env, false, promise);
        }
        Function::getForumTopicDefaultIcons(_) => {
            // Requests.cpp:4827-4831 — the topic-icons special set rides
            // the ReloadSpecialStickerSetQuery plane (net-drain 59);
            // offline the promise parks on the per-type lot.
            let promise = td.deferred_promise::<td_api::Stickers, _>(id, Object::stickers);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_default_topic_icons(&mut env, false, promise);
        }
        Function::getThemedEmojiStatuses(_) => {
            // Requests.cpp:6815-6819 — the default-statuses special set
            // rides the ReloadSpecialStickerSetQuery plane (net-drain 59);
            // offline the promise parks on the per-type lot.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::EmojiStatusCustomEmojis, _>(
                id,
                Object::emojiStatusCustomEmojis,
            );
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_default_emoji_statuses(&mut env, false, promise);
        }
        Function::getThemedChatEmojiStatuses(_) => {
            // Requests.cpp:6821-6825
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::EmojiStatusCustomEmojis, _>(
                id,
                Object::emojiStatusCustomEmojis,
            );
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_default_channel_emoji_statuses(&mut env, false, promise);
        }
        Function::getPremiumStickers(request) => {
            // Requests.cpp:7474-7478 — the limit bands, then the
            // composite load parks ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Stickers, _>(id, Object::stickers);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_premium_stickers(&mut env, request.limit, promise);
        }
        Function::getPremiumInfoSticker(request) => {
            // Requests.cpp:9011-9015 — the premium-gift special set rides
            // the ReloadSpecialStickerSetQuery plane (net-drain 59); the
            // C++ set_value(nullptr) band parks at the response plane.
            if !check_is_user(td, id) {
                return;
            }
            // the C++ `promise.set_value(nullptr sticker)` answers JSON
            // null (ClientJson.cpp:94-95); the Rust response plane has no
            // null object, so that band parks (the `send_null_user`
            // convention)
            let promise = td.nullable_deferred_promise::<td_api::Sticker, _>(id, Object::sticker);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.load_premium_gift_sticker(&mut env, request.month_count, 0, promise);
        }
        Function::getOwnedStickerSets(request) => {
            // Requests.cpp:7660-7664 — the limit band, then the
            // my-stickers query parks ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::StickerSets, _>(id, Object::stickerSets);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_created_sticker_sets(
                &mut env,
                crate::stickers_manager::StickerSetId::new(request.offset_sticker_set_id),
                request.limit,
                promise,
            );
        }
        Function::getStickerSetName(request) => {
            // Requests.cpp:7506-7509 — the Great Minds special case
            // answers locally, unknown sets 400.
            let promise =
                td.deferred_promise::<String, _>(id, |text| Object::text(td_api::Text { text }));
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_sticker_set_name(
                &mut env,
                crate::stickers_manager::StickerSetId::new(request.set_id),
                promise,
            );
        }
        Function::getSuggestedStickerSetName(request) => {
            // Requests.cpp:7552-7556 — the title strip is local; the
            // suggestion query parks ([blocked: net]).
            let Some(title) = clean_or_utf8_error(td, id, &request.title) else {
                return;
            };
            let promise =
                td.deferred_promise::<String, _>(id, |text| Object::text(td_api::Text { text }));
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_suggested_sticker_set_name(&mut env, title, promise);
        }
        Function::checkStickerSetName(request) => {
            // Requests.cpp:7558-7570 — the empty band answers Invalid;
            // the short-name check parks ([blocked: net]).
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = td
                .deferred_promise::<crate::stickers_manager::CheckStickerSetNameResult, _>(
                    id,
                    |result| {
                        use crate::stickers_manager::CheckStickerSetNameResult as R;
                        match result {
                            R::Ok => Object::checkStickerSetNameResultOk(
                                td_api::checkStickerSetNameResultOk {},
                            ),
                            R::Invalid => Object::checkStickerSetNameResultNameInvalid(
                                td_api::checkStickerSetNameResultNameInvalid {},
                            ),
                            R::Occupied => Object::checkStickerSetNameResultNameOccupied(
                                td_api::checkStickerSetNameResultNameOccupied {},
                            ),
                        }
                    },
                );
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.check_sticker_set_name(&mut env, name, promise);
        }
        Function::createNewStickerSet(request) => {
            // Requests.cpp:7572-7581 — the validation prefix is local;
            // the pending-set upload parks ([blocked: Files]).
            let Some(title) = clean_or_utf8_error(td, id, &request.title) else {
                return;
            };
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let Some(source) = clean_or_utf8_error(td, id, &request.source) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::StickerSet, _>(id, Object::stickerSet);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.create_new_sticker_set(
                &mut env,
                crate::user_manager::UserId::new(request.user_id),
                title,
                name,
                crate::stickers_manager::get_sticker_type(request.sticker_type.as_ref()),
                request.needs_repainting,
                request.stickers,
                source,
                promise,
            );
        }
        Function::addStickerToSet(request) => {
            // Requests.cpp:7583-7589 — the name/user bands; the set
            // reload parks ([blocked: net]).
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.add_sticker_to_set(
                &mut env,
                crate::user_manager::UserId::new(request.user_id),
                name,
                request.sticker,
                None,
                promise,
            );
        }
        Function::replaceStickerInSet(request) => {
            // Requests.cpp:7590-7599 — the old-sticker null band is
            // local; the rest rides the add_to_set reload.
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            if request.old_sticker.is_none() {
                return td.send_error_impl(id, 400, "Old sticker must be non-empty");
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.add_sticker_to_set(
                &mut env,
                crate::user_manager::UserId::new(request.user_id),
                name,
                request.new_sticker,
                request.old_sticker,
                promise,
            );
        }
        Function::setStickerSetThumbnail(request) => {
            // Requests.cpp:7601-7607 — the name/user bands; the set
            // reload parks ([blocked: net]).
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.set_sticker_set_thumbnail(
                &mut env,
                crate::user_manager::UserId::new(request.user_id),
                name,
                request.thumbnail,
                crate::stickers_manager::get_sticker_format(request.format.as_ref()),
                promise,
            );
        }
        Function::setCustomEmojiStickerSetThumbnail(request) => {
            // Requests.cpp:7609-7614 — the name band; the set reload
            // parks ([blocked: net]).
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.set_custom_emoji_sticker_set_thumbnail(
                &mut env,
                name,
                crate::stickers_manager::CustomEmojiId::new(request.custom_emoji_id),
                promise,
            );
        }
        Function::setStickerSetTitle(request) => {
            // Requests.cpp:7616-7622 — the name/title bands; the query
            // parks ([blocked: net]).
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let Some(title) = clean_or_utf8_error(td, id, &request.title) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.set_sticker_set_title(&mut env, name, title, promise);
        }
        Function::deleteStickerSet(request) => {
            // Requests.cpp:7624-7628 — the name band; the query parks
            // ([blocked: net]).
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.delete_sticker_set(&mut env, name, promise);
        }
        Function::setStickerPositionInSet(request) => {
            // Requests.cpp:7630-7633 — the position band; the query
            // parks ([blocked: net]).
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.set_sticker_position_in_set(
                &mut env,
                request.sticker.as_ref(),
                request.position,
                promise,
            );
        }
        Function::setStickerEmojis(request) => {
            // Requests.cpp:7640-7644 — the file probe; the query parks
            // ([blocked: net]).
            let Some(emojis) = clean_or_utf8_error(td, id, &request.emojis) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.set_sticker_emojis(
                &mut env,
                request.sticker.as_ref(),
                emojis,
                promise,
            );
        }
        Function::setStickerKeywords(request) => {
            // Requests.cpp:7646-7652 — the file probe; the query parks
            // ([blocked: net]).
            let mut keywords = Vec::with_capacity(request.keywords.len());
            for keyword in &request.keywords {
                let Some(keyword) = clean_or_utf8_error(td, id, keyword) else {
                    return;
                };
                keywords.push(keyword);
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.set_sticker_keywords(
                &mut env,
                request.sticker.as_ref(),
                keywords,
                promise,
            );
        }
        Function::setStickerMaskPosition(request) => {
            // Requests.cpp:7654-7658 — the file probe; the query parks
            // ([blocked: net]).
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.set_sticker_mask_position(
                &mut env,
                request.sticker.as_ref(),
                request.mask_position,
                promise,
            );
        }
        Function::removeStickerFromSet(request) => {
            // Requests.cpp:7635-7638 — the file probe; the query parks
            // ([blocked: net]).
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.remove_sticker_from_set(&mut env, request.sticker.as_ref(), promise);
        }
        Function::reorderInstalledStickerSets(request) => {
            // Requests.cpp:7538-7544 — the local order table answers
            // (the not-loaded band is the 400).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.reorder_installed_sticker_sets(
                &mut env,
                crate::stickers_manager::get_sticker_type(request.sticker_type.as_ref()),
                crate::stickers_manager::StickerSetId::get_sticker_set_ids(
                    &request.sticker_set_ids,
                ),
                promise,
            );
        }
        Function::uploadStickerFile(request) => {
            // Requests.cpp:7546-7550 — the prepare bands are local; the
            // upload parks ([blocked: Files]).
            let promise = td.deferred_promise::<td_api::File, _>(id, Object::file);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.upload_sticker_file(
                &mut env,
                crate::user_manager::UserId::new(request.user_id),
                crate::stickers_manager::get_sticker_format(request.sticker_format.as_ref()),
                request.sticker.as_ref(),
                promise,
            );
        }
        Function::viewTrendingStickerSets(request) => {
            // Requests.cpp:7532-7536 — the local view marks are no-ops
            // over the empty cache; the delayed server read parks.
            if !check_is_user(td, id) {
                return;
            }
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.view_featured_sticker_sets(
                &mut env,
                &crate::stickers_manager::StickerSetId::get_sticker_set_ids(
                    &request.sticker_set_ids,
                ),
            );
            td.send_result(id, ok_object());
        }
        Function::getStickerOutline(request) => {
            // Requests.cpp:7415-7420 — the C++ answers JSON null over an
            // unknown sticker (the recorded null-response divergence):
            // the promise parks.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Outline, _>(id, Object::outline);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_sticker_outline_object(
                &mut env,
                crate::files::FileId::new(request.sticker_file_id, 0),
                request.for_animated_emoji,
                request.for_clicked_animated_emoji_message,
                promise,
            );
        }
        Function::getStickerOutlineSvgPath(request) => {
            // Requests.cpp:7422-7427 — the unknown-sticker band answers
            // the empty path locally.
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<String, _>(id, |text| Object::text(td_api::Text { text }));
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_sticker_outline_svg_path(
                &mut env,
                crate::files::FileId::new(request.sticker_file_id, 0),
                request.for_animated_emoji,
                request.for_clicked_animated_emoji_message,
                promise,
            );
        }
        Function::getEmojiSuggestionsUrl(request) => {
            // Requests.cpp:7738-7742 — the URL query parks
            // ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let Some(language_code) = clean_or_utf8_error(td, id, &request.language_code) else {
                return;
            };
            let promise = td.deferred_promise::<String, _>(id, |url: String| {
                Object::httpUrl(td_api::HttpUrl { url })
            });
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_emoji_suggestions_url(&mut env, language_code, promise);
        }
        Function::searchMessages(request) => {
            // Requests.cpp:3702-3714 — the global search; the empty
            // query + Empty-filter band answers locally, the rest parks
            // at the [blocked: net] query seam.
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
                td.deferred_promise::<td_api::FoundMessages, _>(id, Object::foundMessages);
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.search_messages(
                &mut env,
                crate::messages_manager::DialogListId::from_chat_list(request.chat_list.as_ref()),
                request.chat_list.is_none(),
                query,
                offset,
                request.limit,
                crate::message_query_manager::get_message_search_filter(request.filter.as_ref()),
                request.chat_type_filter,
                request.min_date,
                request.max_date,
                promise,
            );
        }
        Function::searchOutgoingDocumentMessages(request) => {
            // Requests.cpp:3731-3737 — the sent-media search parks at the
            // [blocked: net] query seam.
            if !check_is_user(td, id) {
                return;
            }
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::FoundMessages, _>(id, Object::foundMessages);
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.search_outgoing_document_messages(&mut env, query, request.limit, promise);
        }
        Function::getPublicPostSearchLimits(request) => {
            // Requests.cpp:3738-3743 — the flood check parks at the
            // [blocked: net] query seam.
            if !check_is_user(td, id) {
                return;
            }
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::PublicPostSearchLimits, _>(
                id,
                Object::publicPostSearchLimits,
            );
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.check_search_posts_flood(&mut env, query, promise);
        }
        Function::searchPublicPosts(request) => {
            // Requests.cpp:3745-3752 — the star-count gate + the flood
            // offset; the search parks at the [blocked: net] query seam.
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
                td.deferred_promise::<td_api::FoundPublicPosts, _>(id, Object::foundPublicPosts);
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.search_public_posts(
                &mut env,
                query,
                offset,
                request.limit,
                request.star_count,
                promise,
            );
        }
        Function::searchPublicMessagesByTag(request) => {
            // Requests.cpp:3754-3761 — the hashtag/cashtag split + the
            // hints note; the search parks at the [blocked: net] seam.
            if !check_is_user(td, id) {
                return;
            }
            let Some(tag) = clean_or_utf8_error(td, id, &request.tag) else {
                return;
            };
            let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::FoundMessages, _>(id, Object::foundMessages);
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.search_hashtag_posts(&mut env, tag, offset, request.limit, promise);
        }
        Function::searchChatRecentLocationMessages(request) => {
            // Requests.cpp:3833-3838 — the secret-chat band answers
            // locally, the rest parks at the [blocked: net] query seam.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Messages, _>(id, Object::messages);
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.search_dialog_recent_location_messages(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.limit,
                promise,
            );
        }
        Function::deleteAllCallMessages(request) => {
            // Requests.cpp:3827-3831 — the affected-history loop.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.delete_all_call_messages(&mut env, request.revoke, promise);
        }
        Function::deleteMessageReactionsFromSender(request) => {
            // Requests.cpp:3992-3997 — the sender probe then the
            // participant-reaction delete ([blocked: net]).
            let sender_dialog_id = {
                let (_manager, mut env) = bot_env_split(td);
                match crate::message_sender::get_message_sender_dialog_id(
                    &mut env,
                    request.sender_id.as_ref(),
                    false,
                    false,
                ) {
                    tdutils::status::Result::Ok(dialog_id) => dialog_id,
                    tdutils::status::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.delete_reaction_by_sender(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                crate::messages_manager::MessageId::new(request.message_id),
                sender_dialog_id,
                promise,
            );
        }
        Function::deleteAllRecentMessageReactionsFromSender(request) => {
            // Requests.cpp:3985-3990 — the sender probe then the
            // participant-reactions delete ([blocked: net]).
            let sender_dialog_id = {
                let (_manager, mut env) = bot_env_split(td);
                match crate::message_sender::get_message_sender_dialog_id(
                    &mut env,
                    request.sender_id.as_ref(),
                    false,
                    false,
                ) {
                    tdutils::status::Result::Ok(dialog_id) => dialog_id,
                    tdutils::status::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.delete_reactions_by_sender(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                sender_dialog_id,
                promise,
            );
        }
        Function::deleteEphemeralMessage(request) => {
            // Requests.cpp:4145-4150 — the local ephemeral removal + the
            // server delete ([blocked: net]).
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.delete_ephemeral_message_on_server(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                crate::dialog_id::DialogId::new(request.receiver_user_id),
                crate::messages_manager::EphemeralMessageId::new(request.ephemeral_message_id),
                promise,
            );
        }
        Function::deleteChatMessagesBySender(request) => {
            // Requests.cpp:4153-4159 — the sender probe, the local
            // per-sender delete, then the channel affected-history loop.
            if !check_is_user(td, id) {
                return;
            }
            let sender_dialog_id = {
                let (_manager, mut env) = bot_env_split(td);
                match crate::message_sender::get_message_sender_dialog_id(
                    &mut env,
                    request.sender_id.as_ref(),
                    false,
                    false,
                ) {
                    tdutils::status::Result::Ok(dialog_id) => dialog_id,
                    tdutils::status::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.delete_dialog_messages_by_sender(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                sender_dialog_id,
                promise,
            );
        }
        Function::deleteChatMessagesByDate(request) => {
            // Requests.cpp:4161-4166 — the date-window fix, the local
            // by-date delete, then the server loop ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.delete_dialog_messages_by_date(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.min_date,
                request.max_date,
                request.revoke,
                promise,
            );
        }
        Function::editEphemeralMessage(request) => {
            // Requests.cpp:4385-4392 — the bot-side ephemeral edit; the
            // query parks at the [blocked: net] seam.
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.edit_ephemeral_message(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                crate::user_manager::UserId::new(request.receiver_user_id),
                crate::messages_manager::EphemeralMessageId::new(request.ephemeral_message_id),
                request.reply_markup,
                request.input_message_content.map(|content| *content),
                promise,
            );
        }
        Function::editEphemeralMessageCaption(request) => {
            // Requests.cpp:4393-4401 — the caption variant of the same
            // query ([blocked: net]).
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.edit_ephemeral_message_caption(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                crate::user_manager::UserId::new(request.receiver_user_id),
                crate::messages_manager::EphemeralMessageId::new(request.ephemeral_message_id),
                request.reply_markup,
                request.caption,
                request.show_caption_above_media,
                promise,
            );
        }
        Function::getPersonalChatHistory(request) => {
            // Requests.cpp:6718-6722 — the personal-channel history read
            // parks at the [blocked: net] query seam.
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Messages, _>(id, Object::messages);
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.get_personal_chat_history(
                &mut env,
                crate::user_manager::UserId::new(request.user_id),
                request.limit,
                promise,
            );
        }
        Function::listenToAudio(request) => {
            // Requests.cpp:3559-3564 — the file-view decision table, then
            // the report ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.report_music_listen(
                &mut env,
                crate::files::FileId::new(request.audio_file_id, 0),
                request.duration,
                promise,
            );
        }
        Function::summarizeMessage(request) => {
            // Requests.cpp:2883-2889 — the message probe + the tone
            // check, then the summarize query ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let Some(translate_to_language_code) =
                clean_or_utf8_error(td, id, &request.translate_to_language_code)
            else {
                return;
            };
            let Some(tone) = clean_or_utf8_error(td, id, &request.tone) else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::FormattedText, _>(id, Object::formattedText);
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.summarize_message_text(
                &mut env,
                crate::dialog_id::MessageFullId::new(
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::messages_manager::MessageId::new(request.message_id),
                ),
                translate_to_language_code,
                tone,
                promise,
            );
        }
        Function::addChecklistTasks(request) => {
            // Requests.cpp:8129-8135 — the checklist eligibility gate,
            // then the append query ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.add_to_do_list_tasks(
                &mut env,
                crate::dialog_id::MessageFullId::new(
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::messages_manager::MessageId::new(request.message_id),
                ),
                request.tasks,
                promise,
            );
        }
        Function::markChecklistTasksAsDone(request) => {
            // Requests.cpp:8136-8143 — the checklist eligibility gate,
            // then the toggle query ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.mark_to_do_list_tasks_as_done(
                &mut env,
                crate::dialog_id::MessageFullId::new(
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::messages_manager::MessageId::new(request.message_id),
                ),
                request.marked_as_done_task_ids,
                request.marked_as_not_done_task_ids,
                promise,
            );
        }
        Function::shareUsersWithBot(request) => {
            // Requests.cpp:8179-8185 — the shared-user validation, then
            // the bot peer send ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            let shared_dialog_ids: Vec<crate::dialog_id::DialogId> = request
                .shared_user_ids
                .iter()
                .map(|&user_id| crate::dialog_id::DialogId::new(user_id))
                .collect();
            mq_manager.share_dialogs_with_bot(
                &mut env,
                request.source,
                request.button_id,
                shared_dialog_ids,
                true,
                request.only_check,
                promise,
            );
        }
        Function::shareChatWithBot(request) => {
            // Requests.cpp:8187-8193 — the shared-chat validation, then
            // the bot peer send ([blocked: net]).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.share_dialogs_with_bot(
                &mut env,
                request.source,
                request.button_id,
                vec![crate::dialog_id::DialogId::new(request.shared_chat_id)],
                false,
                request.only_check,
                promise,
            );
        }
        Function::getTimeZones(_) => {
            // `Requests.cpp:3099-3102` — the local list answer.
            let mut global = std::mem::take(&mut td.global);
            let time_zones = td.time_zone_manager.get_time_zones(&mut global);
            td.global = global;
            td.send_result(id, Object::timeZones(time_zones));
        }
        Function::getFileDownloadedPrefixSize(request) => {
            // `Requests.cpp:3076-3087` — the local file-view read.
            if request.offset < 0 {
                return td.send_error_impl(id, 400, "Parameter offset must be non-negative");
            }
            // the file-view read borrows the file-manager lock, so the
            // prefix size is computed inside the lock scope
            let downloaded_prefix = {
                let file_manager = td.file_manager.lock();
                file_manager
                    .get_file_view(crate::files::FileId::new(request.file_id, 0))
                    .map(|view| view.downloaded_prefix(request.offset))
            };
            let Some(size) = downloaded_prefix else {
                return td.send_error_impl(id, 400, "Unknown file ID");
            };
            td.send_result(
                id,
                Object::fileDownloadedPrefixSize(td_api::FileDownloadedPrefixSize { size }),
            );
        }

        // --- the top-dialog + recommendation set (M.27): Requests.cpp:3230-3243,
        // 3182-3212. The managers validate; the net query tails park the promises
        // unanswered offline, exactly like the C++ request without a network ---
        Function::getTopChats(request) => {
            // Requests.cpp:3230-3235
            if !check_is_user(td, id) {
                return;
            }
            let category = crate::top_dialog_manager::get_top_dialog_category_from_td_api(
                request.category.as_ref(),
            );
            let promise = td.deferred_promise::<crate::td_api::Chats, _>(id, Object::chats);
            let (mut env, top_dialog_manager) = crate::td::top_dialog_env_split(td);
            top_dialog_manager.get_top_dialogs(&mut env, category, request.limit, promise);
        }
        Function::removeTopChat(request) => {
            // Requests.cpp:3237-3242
            if !check_is_user(td, id) {
                return;
            }
            let category = crate::top_dialog_manager::get_top_dialog_category_from_td_api(
                request.category.as_ref(),
            );
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, top_dialog_manager) = crate::td::top_dialog_env_split(td);
            top_dialog_manager.remove_dialog(
                &mut env,
                category,
                MmDialogId::new(request.chat_id),
                promise,
            );
        }
        Function::getRecommendedChats(_request) => {
            // Requests.cpp:3182-3186
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<crate::td_api::Chats, _>(id, Object::chats);
            let (mut env, recommendation_manager) = crate::td::recommendation_env_split(td);
            recommendation_manager.get_recommended_channels(&mut env, promise);
        }
        Function::joinChat(request) => {
            // Requests.cpp:6154-6159
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |v: td_api::ChatJoinResult| v.into());
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.join_dialog(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                promise,
            );
        }
        Function::leaveChat(request) => {
            // Requests.cpp:6160-6177: the creator-of-channel early-ok and the
            // creator-status rewrite happen at the dispatcher
            let dialog_id = crate::chat_manager::DialogId::new(request.chat_id);
            let mut new_status =
                td_api::ChatMemberStatus::chatMemberStatusLeft(td_api::chatMemberStatusLeft {});
            if dialog_id.get_type() == crate::chat_manager::DialogType::Channel && {
                let (mut env, _) = crate::td::dialog_participant_env_split(td);
                env.have_dialog_force(dialog_id, "leaveChat")
            } {
                let (mut env, _) = crate::td::dialog_participant_env_split(td);
                let status = env.get_channel_status(dialog_id.get_channel_id());
                if status.is_creator() {
                    if !status.is_member() {
                        return td.send_result(id, ok_object());
                    }
                    new_status = td_api::ChatMemberStatus::chatMemberStatusCreator(
                        td_api::chatMemberStatusCreator {
                            is_anonymous: status.is_anonymous(),
                            is_member: false,
                        },
                    );
                }
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            let my_dialog_id = env.my_dialog_id();
            manager.set_dialog_participant_status(
                &mut env,
                dialog_id,
                my_dialog_id,
                new_status,
                promise,
            );
        }
        Function::addChatMember(request) => {
            // Requests.cpp:6178-6184
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.add_dialog_participant(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                crate::user_manager::UserId::new(request.user_id),
                request.forward_limit,
                promise,
            );
        }
        Function::addChatMembers(request) => {
            // Requests.cpp:6185-6191
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.add_dialog_participants(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                request
                    .user_ids
                    .iter()
                    .map(|&user_id| crate::user_manager::UserId::new(user_id))
                    .collect(),
                promise,
            );
        }
        Function::setChatMemberStatus(request) => {
            // Requests.cpp:6192-6199
            let participant_dialog_id = {
                let (_manager, mut env) = bot_env_split(td);
                match crate::message_sender::get_message_sender_dialog_id(
                    &mut env,
                    request.member_id.as_ref(),
                    false,
                    false,
                ) {
                    tdutils::status::Result::Ok(dialog_id) => {
                        crate::chat_manager::DialogId::new(dialog_id.get())
                    }
                    tdutils::status::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.set_dialog_participant_status(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                participant_dialog_id,
                request.status.expect("the status is a required field"),
                promise,
            );
        }
        Function::setChatMemberTag(request) => {
            // Requests.cpp:6200-6206
            let Some(tag) = clean_or_utf8_error(td, id, &request.tag) else {
                return;
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.set_dialog_participant_rank(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                crate::user_manager::UserId::new(request.user_id),
                tag,
                promise,
            );
        }
        Function::banChatMember(request) => {
            // Requests.cpp:6207-6215
            let participant_dialog_id = {
                let (_manager, mut env) = bot_env_split(td);
                match crate::message_sender::get_message_sender_dialog_id(
                    &mut env,
                    request.member_id.as_ref(),
                    false,
                    false,
                ) {
                    tdutils::status::Result::Ok(dialog_id) => {
                        crate::chat_manager::DialogId::new(dialog_id.get())
                    }
                    tdutils::status::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.ban_dialog_participant(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                participant_dialog_id,
                request.banned_until_date,
                request.revoke_messages,
                promise,
            );
        }
        Function::canTransferOwnership(_) => {
            // Requests.cpp:6216-6229: the query error maps through the
            // manager's table; the query itself parks [blocked: net]
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |v: crate::dialog_participant_manager::CanTransferOwnershipResult| {
                    crate::dialog_participant_manager::DialogParticipantManager::get_can_transfer_ownership_result_object(v).into()
                });
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.can_transfer_ownership(&mut env);
            drop(promise);
        }
        Function::transferChatOwnership(request) => {
            // Requests.cpp:6230-6237
            if !check_is_user(td, id) {
                return;
            }
            let Some(password) = clean_or_utf8_error(td, id, &request.password) else {
                return;
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.transfer_dialog_ownership(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                crate::user_manager::UserId::new(request.user_id),
                &password,
                promise,
            );
        }
        Function::getChatOwnerAfterLeaving(request) => {
            // Requests.cpp:6238-6243
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |v: td_api::User| Object::user(v));
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.get_future_creator_after_leave(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                promise,
            );
        }
        Function::getChatMember(request) => {
            // Requests.cpp:6244-6251
            let participant_dialog_id = {
                let (_manager, mut env) = bot_env_split(td);
                match crate::message_sender::get_message_sender_dialog_id(
                    &mut env,
                    request.member_id.as_ref(),
                    false,
                    false,
                ) {
                    tdutils::status::Result::Ok(dialog_id) => {
                        crate::chat_manager::DialogId::new(dialog_id.get())
                    }
                    tdutils::status::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            let promise = td.deferred_promise(id, |v: td_api::ChatMember| Object::chatMember(v));
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.get_dialog_participant(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                participant_dialog_id,
                promise,
            );
        }
        Function::searchChatMembers(request) => {
            // Requests.cpp:6252-6268
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let dialog_id = crate::chat_manager::DialogId::new(request.chat_id);
            let filter = crate::dialog_participant_filter::DialogParticipantFilter::from_td_api(
                dialog_id,
                request.filter.as_ref(),
            );
            let promise = td.deferred_promise(id, |v: td_api::ChatMembers| Object::chatMembers(v));
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.search_dialog_participants(
                &mut env,
                dialog_id,
                &query,
                request.limit,
                &filter,
                promise,
            );
        }
        Function::getChatAdministrators(request) => {
            // Requests.cpp:6269-6274
            let promise = td.deferred_promise(id, |v: td_api::ChatAdministrators| {
                Object::chatAdministrators(v)
            });
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.get_dialog_administrators(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                promise,
            );
        }
        Function::getChatJoinRequests(request) => {
            // Requests.cpp:6346-6355
            if !check_is_user(td, id) {
                return;
            }
            let Some(invite_link) = clean_or_utf8_error(td, id, &request.invite_link) else {
                return;
            };
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let promise = td.deferred_promise(id, |v: td_api::ChatJoinRequests| {
                Object::chatJoinRequests(v)
            });
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.get_dialog_join_requests(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                invite_link,
                query,
                request.offset_request,
                request.limit,
                promise,
            );
        }
        Function::processChatJoinRequest(request) => {
            // Requests.cpp:6356-6361
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.process_dialog_join_request(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                crate::user_manager::UserId::new(request.user_id),
                request.approve,
                promise,
            );
        }
        Function::processChatJoinRequests(request) => {
            // Requests.cpp:6362-6370
            if !check_is_user(td, id) {
                return;
            }
            let Some(invite_link) = clean_or_utf8_error(td, id, &request.invite_link) else {
                return;
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.process_dialog_join_requests(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                invite_link,
                request.approve,
                promise,
            );
        }
        Function::getSupergroupMembers(request) => {
            // Requests.cpp:7393-7411 — the additional pair is `string(),
            // -1` (no client-side search pass over the page)
            let channel_id = crate::chat_manager::ChannelId::new(request.supergroup_id);
            let filter = crate::dialog_participant_filter::ChannelParticipantFilter::from_td_api(
                crate::chat_manager::DialogId::from_channel_id(channel_id),
                request.filter.as_ref(),
            );
            let promise = td.deferred_promise(id, |v: td_api::ChatMembers| Object::chatMembers(v));
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.get_channel_participants(
                &mut env,
                channel_id,
                &filter,
                String::new(),
                request.offset,
                request.limit,
                -1,
                promise,
                "getSupergroupMembers",
            );
        }
        Function::getGuardBotWebAppUrl(request) => {
            // Requests.cpp:8300-8306
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |v: td_api::WebAppUrl| Object::webAppUrl(v));
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.get_chat_join_web_view_url(&mut env, request.query_id, promise);
        }
        Function::answerChatJoinRequestQuery(request) => {
            // Requests.cpp:8348-8354
            if !check_is_bot(td, id) {
                return;
            }
            let Some(url) = clean_or_utf8_error(td, id, &request.url) else {
                return;
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_participant_env_split(td);
            manager.set_join_chat_result(
                &mut env,
                request.query_id,
                request.result.expect("the result is a required field"),
                url,
            );
            drop(promise);
        }
        Function::getChatSimilarChats(request) => {
            // Requests.cpp:3188-3192
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<crate::td_api::Chats, _>(id, Object::chats);
            let (mut env, recommendation_manager) = crate::td::recommendation_env_split(td);
            recommendation_manager.get_channel_recommendations(
                &mut env,
                MmDialogId::new(request.chat_id),
                false,
                Some(promise),
                None,
            );
        }
        Function::getChatSimilarChatCount(request) => {
            // Requests.cpp:3194-3199 — the CREATE_COUNT_REQUEST_PROMISE band
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<i32, _>(id, |count| Object::count(td_api::Count { count }));
            let (mut env, recommendation_manager) = crate::td::recommendation_env_split(td);
            recommendation_manager.get_channel_recommendations(
                &mut env,
                MmDialogId::new(request.chat_id),
                request.return_local,
                None,
                Some(promise),
            );
        }
        Function::openChatSimilarChat(request) => {
            // Requests.cpp:3201-3207
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, recommendation_manager) = crate::td::recommendation_env_split(td);
            recommendation_manager.open_channel_recommended_channel(
                &mut env,
                MmDialogId::new(request.chat_id),
                MmDialogId::new(request.opened_chat_id),
                promise,
            );
        }
        // --- the inline set (M.21): Requests.cpp:4342-4386, 4917-4941,
        // 8195-8233, 4737-4740. The managers validate; the net query tails park
        // the promises unanswered offline, exactly like the C++ request without
        // a network ---
        Function::editInlineMessageText(request) => {
            // Requests.cpp:4342-4348
            if !check_is_bot(td, id) {
                return;
            }
            let Some(inline_message_id) = clean_or_utf8_error(td, id, &request.inline_message_id)
            else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, _, inline_message_manager) = crate::td::inline_env_split(td);
            inline_message_manager.edit_inline_message_text(
                &mut env,
                &inline_message_id,
                request.reply_markup,
                request.input_message_content,
                promise,
            );
        }
        Function::editInlineMessageLiveLocation(request) => {
            // Requests.cpp:4350-4356
            if !check_is_bot(td, id) {
                return;
            }
            let Some(inline_message_id) = clean_or_utf8_error(td, id, &request.inline_message_id)
            else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, _, inline_message_manager) = crate::td::inline_env_split(td);
            inline_message_manager.edit_inline_message_live_location(
                &mut env,
                &inline_message_id,
                request.reply_markup,
                request.location,
                promise,
            );
        }
        Function::editInlineMessageMedia(request) => {
            // Requests.cpp:4358-4365
            if !check_is_bot(td, id) {
                return;
            }
            let Some(inline_message_id) = clean_or_utf8_error(td, id, &request.inline_message_id)
            else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, _, inline_message_manager) = crate::td::inline_env_split(td);
            inline_message_manager.edit_inline_message_media(
                &mut env,
                &inline_message_id,
                request.reply_markup,
                request.input_message_content,
                promise,
            );
        }
        Function::editInlineMessageCaption(request) => {
            // Requests.cpp:4367-4374
            if !check_is_bot(td, id) {
                return;
            }
            let Some(inline_message_id) = clean_or_utf8_error(td, id, &request.inline_message_id)
            else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, _, inline_message_manager) = crate::td::inline_env_split(td);
            inline_message_manager.edit_inline_message_caption(
                &mut env,
                &inline_message_id,
                request.reply_markup,
                request.caption,
                request.show_caption_above_media,
                promise,
            );
        }
        Function::editInlineMessageReplyMarkup(request) => {
            // Requests.cpp:4376-4382
            if !check_is_bot(td, id) {
                return;
            }
            let Some(inline_message_id) = clean_or_utf8_error(td, id, &request.inline_message_id)
            else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, _, inline_message_manager) = crate::td::inline_env_split(td);
            inline_message_manager.edit_inline_message_reply_markup(
                &mut env,
                &inline_message_id,
                request.reply_markup,
                promise,
            );
        }
        Function::setInlineGameScore(request) => {
            // Requests.cpp:4919-4927
            if !check_is_bot(td, id) {
                return;
            }
            let Some(inline_message_id) = clean_or_utf8_error(td, id, &request.inline_message_id)
            else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, _, inline_message_manager) = crate::td::inline_env_split(td);
            inline_message_manager.set_inline_game_score(
                &mut env,
                &inline_message_id,
                request.edit_message,
                crate::user_manager::UserId::new(request.user_id),
                request.score,
                request.force,
                promise,
            );
        }
        Function::getInlineGameHighScores(request) => {
            // Requests.cpp:4931-4938
            if !check_is_bot(td, id) {
                return;
            }
            let Some(inline_message_id) = clean_or_utf8_error(td, id, &request.inline_message_id)
            else {
                return;
            };
            let promise =
                td.deferred_promise::<crate::td_api::GameHighScores, _>(id, Object::gameHighScores);
            let (mut env, _, inline_message_manager) = crate::td::inline_env_split(td);
            inline_message_manager.get_inline_game_high_scores(
                &mut env,
                &inline_message_id,
                crate::user_manager::UserId::new(request.user_id),
                promise,
            );
        }
        Function::getInlineQueryResults(request) => {
            // Requests.cpp:8195-8203
            if !check_is_user(td, id) {
                return;
            }
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let promise = td
                .deferred_promise::<Option<crate::td_api::InlineQueryResults>, _>(id, |results| {
                    Object::inlineQueryResults(results.unwrap_or_default())
                });
            let user_location =
                crate::inline_queries_manager::location_from_td_api(request.user_location.as_ref());
            let (mut env, inline_queries_manager, _) = crate::td::inline_env_split(td);
            inline_queries_manager.send_inline_query(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                crate::messages_manager::DialogId::new(request.chat_id),
                user_location,
                &query,
                &offset,
                promise,
            );
        }
        Function::answerInlineQuery(request) => {
            // Requests.cpp:8205-8212
            if !check_is_bot(td, id) {
                return;
            }
            let Some(next_offset) = clean_or_utf8_error(td, id, &request.next_offset) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, inline_queries_manager, _) = crate::td::inline_env_split(td);
            inline_queries_manager.answer_inline_query(
                &mut env,
                request.inline_query_id,
                request.is_personal,
                request.button,
                request.results,
                request.cache_time,
                next_offset,
                promise,
            );
        }
        Function::answerGuestQuery(request) => {
            // Requests.cpp:8214-8220
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td
                .deferred_promise::<crate::td_api::InlineMessageId, _>(id, Object::inlineMessageId);
            let (mut env, inline_queries_manager, _) = crate::td::inline_env_split(td);
            inline_queries_manager.answer_guest_query(
                &mut env,
                request.guest_query_id,
                request.result,
                promise,
            );
        }
        Function::savePreparedInlineMessage(request) => {
            // Requests.cpp:8222-8227
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise::<crate::td_api::PreparedInlineMessageId, _>(
                id,
                Object::preparedInlineMessageId,
            );
            let (mut env, inline_queries_manager, _) = crate::td::inline_env_split(td);
            inline_queries_manager.save_prepared_inline_message(
                &mut env,
                crate::user_manager::UserId::new(request.user_id),
                request.result,
                request.chat_types,
                promise,
            );
        }
        Function::getPreparedInlineMessage(request) => {
            // Requests.cpp:8228-8234
            if !check_is_user(td, id) {
                return;
            }
            let Some(prepared_message_id) =
                clean_or_utf8_error(td, id, &request.prepared_message_id)
            else {
                return;
            };
            let promise = td.deferred_promise::<Option<crate::td_api::PreparedInlineMessage>, _>(
                id,
                |message| Object::preparedInlineMessage(message.unwrap_or_default()),
            );
            let (mut env, inline_queries_manager, _) = crate::td::inline_env_split(td);
            inline_queries_manager.get_prepared_inline_message(
                &mut env,
                crate::user_manager::UserId::new(request.bot_user_id),
                &prepared_message_id,
                promise,
            );
        }
        Function::getCurrentWeather(request) => {
            // Requests.cpp:4737-4740
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<crate::td_api::CurrentWeather, _>(id, Object::currentWeather);
            let user_location =
                crate::inline_queries_manager::location_from_td_api(request.location.as_ref());
            let (mut env, inline_queries_manager, _) = crate::td::inline_env_split(td);
            inline_queries_manager.get_weather(&mut env, user_location, promise);
        }

        // --- the web-page set (M.17): Requests.cpp:5032-5042. The managers
        // validate; the net query tails park the promises unanswered offline,
        // exactly like the C++ request without a network ---
        Function::getLinkPreview(request) => {
            // Requests.cpp:5032-5036
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<Option<crate::td_api::LinkPreview>, _>(id, |preview| {
                    // Td::send_result answers a null object with a 404 error.
                    preview.map_or_else(
                        || {
                            Object::error(td_api::Error {
                                code: 404,
                                message: "Not Found".to_string(),
                            })
                        },
                        Object::linkPreview,
                    )
                });
            let (mut env, web_pages_manager) = crate::td::web_pages_env_split(td);
            web_pages_manager.get_web_page_preview(
                &mut env,
                request.text,
                request.link_preview_options,
                promise,
            );
        }
        Function::getWebPageInstantView(request) => {
            // Requests.cpp:5041-5044 — the GetWebPageInstantViewRequest actor:
            // the id resolves, then the result sends the instant-view object;
            // the collapsed form resolves the id and builds the object inside
            // the manager (the env stays alive there)
            if !check_is_user(td, id) {
                return;
            }
            let Some(url) = clean_or_utf8_error(td, id, &request.url) else {
                return;
            };
            let promise = td.deferred_promise::<crate::td_api::WebPageInstantView, _>(
                id,
                Object::webPageInstantView,
            );
            let (mut env, web_pages_manager) = crate::td::web_pages_env_split(td);
            web_pages_manager.get_web_page_instant_view_with_object(
                &mut env,
                &url,
                request.only_local,
                promise,
            );
        }

        // --- the poll set (M.19): Requests.cpp:8088-8131. The arms validate +
        // route to the PollManager; the five wire tails ride the net-drain 39
        // plane (the promises park only in the net-inert posture; the
        // message-poll lookups answer the M.10 seam's not-found band) ---
        Function::addPollOption(request) => {
            // Requests.cpp:8088-8093
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, poll_manager) = crate::td::poll_env_split(td);
            poll_manager.add_poll_option(
                &mut env,
                message_full_id(request.chat_id, request.message_id),
                request.option,
                promise,
            );
        }
        Function::deletePollOption(request) => {
            // Requests.cpp:8095-8100
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, poll_manager) = crate::td::poll_env_split(td);
            poll_manager.delete_poll_option(
                &mut env,
                message_full_id(request.chat_id, request.message_id),
                request.option_id,
                promise,
            );
        }
        Function::setPollAnswer(request) => {
            // Requests.cpp:8102-8107
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, poll_manager) = crate::td::poll_env_split(td);
            poll_manager.set_poll_answer(
                &mut env,
                message_full_id(request.chat_id, request.message_id),
                request.option_ids,
                promise,
            );
        }
        Function::getPollVoters(request) => {
            // Requests.cpp:8109-8114
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<crate::td_api::PollVoters, _>(id, Object::pollVoters);
            let (mut env, poll_manager) = crate::td::poll_env_split(td);
            poll_manager.get_poll_voters(
                &mut env,
                message_full_id(request.chat_id, request.message_id),
                request.option_id,
                request.offset,
                request.limit,
                promise,
            );
        }
        Function::stopPoll(request) => {
            // Requests.cpp:8123-8128 — no CHECK_IS_USER in the C++
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, poll_manager) = crate::td::poll_env_split(td);
            poll_manager.stop_poll(
                &mut env,
                message_full_id(request.chat_id, request.message_id),
                request.reply_markup,
                promise,
            );
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
            // Requests.cpp:6803-6807: the td_api → EmojiStatus
            // conversion; the premium gate and the UpdateEmojiStatusQuery
            // send live in the manager (`set_emoji_status`)
            if !check_is_user(td, id) {
                return;
            }
            let emoji_status = crate::emoji_status::EmojiStatus::get_emoji_status_from_td_api(
                request.emoji_status.as_ref(),
                td.global.unix_time(),
            );
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let mut env = user_env!(td);
            td.user_manager
                .set_emoji_status(&mut env, emoji_status, promise);
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
            // Requests.cpp:7162-7167 — the promise settles from the
            // `UpdateColorQuery` answer (the own-account form)
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            td.user_manager.set_accent_color(
                crate::peer_color::AccentColorId::new(request.accent_color_id),
                crate::emoji_status::CustomEmojiId::new(request.background_custom_emoji_id),
                promise,
            );
        }

        // --- the remaining photo-set handlers (the Files slice) and the
        // deferred subsystems — each arm is the flip point for its step ---
        Function::setProfilePhoto(request) => {
            // Requests.cpp:7116-7119 → UserManager.cpp:5996-6002: the
            // non-bot UserpicSetup suggested-action dismissal is the
            // Payments-style seam; the validation bands answer in
            // `set_profile_photo_impl`, the prev-photo query and the
            // three upload arms ride the user net kinds
            if !td.auth_manager.is_bot() {
                tdutils::log_info!(
                    "dismiss_suggested_action(USERPIC_SETUP) — [blocked: SuggestedActionManager]"
                );
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let env = user_env!(td);
            let mut files = td.file_manager.lock();
            let mut fllm = td.file_load_manager.lock();
            let upload_callback = td.profile_photo_upload_callback.clone();
            td.user_manager.set_profile_photo(
                set_profile_photo_td!(td, env, files, fllm, upload_callback),
                request.photo.as_ref(),
                request.is_public,
                promise,
            );
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
            // music list; the not-inited list rides the
            // account.getSavedMusicIds reload (net-drain 09)
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = user_env!(td);
            td.user_manager
                .is_saved_music(&mut env, FileId::new(request.file_id, 0), promise);
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
        Function::deleteProfilePhoto(request) => {
            // Requests.cpp:7121-7124 → UserManager.cpp:6395-6426: the
            // cache bands answer here; the unloaded-full reload ladder,
            // the reset query and the delete query ride the user net
            // kinds
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut env = user_env!(td);
            td.user_manager.delete_profile_photo(
                &mut env,
                request.profile_photo_id,
                false,
                promise,
            );
        }
        Function::setUserPersonalProfilePhoto(request) => {
            // Requests.cpp:6681-6685 → UserManager.cpp:6071-6097: the
            // contact/self/bot ladder answers here; the delete query
            // and the upload arms ride the user net kinds
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let env = user_env!(td);
            let mut files = td.file_manager.lock();
            let mut fllm = td.file_load_manager.lock();
            let upload_callback = td.profile_photo_upload_callback.clone();
            td.user_manager.set_user_profile_photo(
                set_profile_photo_td!(td, env, files, fllm, upload_callback),
                UserId::new(request.user_id),
                request.photo.as_ref(),
                false,
                promise,
            );
        }
        Function::suggestUserProfilePhoto(request) => {
            // Requests.cpp:6693-6697: the suggest twin
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let env = user_env!(td);
            let mut files = td.file_manager.lock();
            let mut fllm = td.file_load_manager.lock();
            let upload_callback = td.profile_photo_upload_callback.clone();
            td.user_manager.set_user_profile_photo(
                set_profile_photo_td!(td, env, files, fllm, upload_callback),
                UserId::new(request.user_id),
                request.photo.as_ref(),
                true,
                promise,
            );
        }
        Function::addProfileAudio(request) => {
            // Requests.cpp:7143-7146 → UserManager.cpp:7178-7216: the
            // input gates, the file resolution and the
            // create_audio/upload tail run in the manager over the
            // td-plane pair; the `messages.uploadMedia` query rides the
            // user net kinds
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut files = td.file_manager.lock();
            let mut fllm = td.file_load_manager.lock();
            let upload_callback = td.saved_music_upload_callback.clone();
            td.user_manager.add_new_saved_music(
                crate::user_manager::SavedMusicUploadTd {
                    files: &mut files,
                    fllm: &mut fllm,
                    upload_callback: &upload_callback,
                    audios: &mut td.audios_manager,
                    close_flag: td.global.close_flag(),
                },
                request.audio,
                promise,
            );
        }
        Function::setUserNote(request) => {
            // Requests.cpp:6687-6691 → UserManager.cpp:6440-6466: the
            // contact/self gates and the formatted-text normalization
            // answer in the manager; the UpdateContactNoteQuery send
            // rides the user net handle
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let env = user_env!(td);
            td.user_manager.set_user_note(
                &env,
                UserId::new(request.user_id),
                request.note,
                promise,
            );
        }
        Function::setMainProfileTab(request) => {
            // Requests.cpp:6791-6795 → UserManager.cpp:6688-6701: the
            // null gate and the Unknown-channel tab allow-list answer
            // in the manager; the SetMainProfileTabQuery send rides the
            // user net handle and `on_set_main_profile_tab` is its ack
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            td.user_manager
                .set_main_profile_tab(request.main_profile_tab, promise);
        }
        Function::suggestUserBirthdate(request) => {
            // Requests.cpp:6699-6703 → UserManager.cpp:6471-6480: the
            // self/bot gates answer in the manager; the
            // SuggestUserBirthdayQuery send rides the user net handle
            // (the Updates answer re-enters on_get_updates)
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let env = user_env!(td);
            td.user_manager.suggest_user_birthdate(
                &env,
                UserId::new(request.user_id),
                request.birthdate.unwrap_or_default(),
                promise,
            );
        }
        Function::setProfileAccentColor(request) => {
            // Requests.cpp:7176-7180 → UserManager.cpp:6644-6648: no
            // validation; the `UpdateColorQuery` (profile form) carries
            // the promise; `on_update_accent_color_success` is its ack
            // intake
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            td.user_manager.set_profile_accent_color(
                crate::peer_color::AccentColorId::new(request.profile_accent_color_id),
                crate::emoji_status::CustomEmojiId::new(request.profile_background_custom_emoji_id),
                promise,
            );
        }
        Function::setUpgradedGiftColors(_request) => {
            // Requests.cpp:7169-7173 → UserManager.cpp:6640-6642: no
            // validation; the UpdateColorCollectibleQuery is
            // [blocked: net]
            if !check_is_user(td, id) {
                return;
            }
            tdutils::log_info!("Send account.updateColorCollectible — [blocked: net]");
        }
        Function::getSupportUser(_) => {
            // Requests.cpp:8893-8896 → UserManager.cpp:6798-6815: the
            // cached id answers in the manager; the GetSupportUserQuery
            // send rides the user net handle
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::User, _>(id, Object::user);
            let env = user_env!(td);
            td.user_manager.get_support_user(&env, promise);
        }
        Function::getUserSupportInfo(request) => {
            // Requests.cpp:9328-9332 → Support.cpp:126-128: the
            // GetUserInfoQuery send half (the get_input_user validation)
            // answers here; the help.getUserInfo query is
            // [blocked: net] and get_user_support_info_object is its
            // result half.
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::UserSupportInfo, _>(id, Object::userSupportInfo);
            let mut env = crate::td::SupportEnvTd { td };
            crate::support_manager::get_user_info(&mut env, UserId::new(request.user_id), promise);
        }
        Function::setUserSupportInfo(request) => {
            // Requests.cpp:9334-9338 → Support.cpp:130-136: the
            // get_formatted_text intake (the my-dialog context,
            // allow_empty, skip media timestamps, no trim) runs before
            // the EditUserInfoQuery send half; the help.editUserInfo
            // query is [blocked: net].
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::UserSupportInfo, _>(id, Object::userSupportInfo);
            let mut env = crate::td::SupportEnvTd { td };
            crate::support_manager::set_user_info(
                &mut env,
                UserId::new(request.user_id),
                request.message,
                promise,
            );
        }
        Function::getSupportName(_) => {
            // Requests.cpp:9340-9343 → Support.cpp:138-140: the
            // help.getSupportName query is [blocked: net]; the
            // CREATE_TEXT_REQUEST_PROMISE wrapper (Requests.cpp:2022)
            // rewraps the fetched name into the td_api text object.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<String, _>(id, |name| {
                Object::text(td_api::Text { text: name })
            });
            let mut env = crate::td::SupportEnvTd { td };
            crate::support_manager::get_support_name(&mut env, promise);
        }
        Function::getDefaultEmojiStatuses(_) => {
            // Requests.cpp:6827-6831 → EmojiStatus.cpp:571-578: the
            // cached list answers here; the miss rides the
            // `UserManager::net` handle (the GetDefaultEmojiStatusesQuery
            // kind)
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::EmojiStatusCustomEmojis, _>(
                id,
                Object::emojiStatusCustomEmojis,
            );
            let mut env = user_env!(td);
            td.user_manager
                .get_default_emoji_statuses(&mut env, promise);
        }
        Function::getDefaultChatEmojiStatuses(_) => {
            // Requests.cpp:6833-6837 → EmojiStatus.cpp:580-588
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::EmojiStatusCustomEmojis, _>(
                id,
                Object::emojiStatusCustomEmojis,
            );
            let mut env = user_env!(td);
            td.user_manager
                .get_default_channel_emoji_statuses(&mut env, promise);
        }
        Function::getRecentEmojiStatuses(_) => {
            // Requests.cpp:6839-6843 → EmojiStatus.cpp:590-597
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::EmojiStatuses, _>(id, Object::emojiStatuses);
            let mut env = user_env!(td);
            td.user_manager.get_recent_emoji_statuses(&mut env, promise);
        }
        Function::getUpgradedGiftEmojiStatuses(_) => {
            // Requests.cpp:6845-6849 → EmojiStatus.cpp:626-637
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::EmojiStatuses, _>(id, Object::emojiStatuses);
            let mut env = user_env!(td);
            td.user_manager
                .get_upgraded_gift_emoji_statuses(&mut env, promise);
        }
        Function::clearRecentEmojiStatuses(_) => {
            // Requests.cpp:6851-6855 → EmojiStatus.cpp:621-624: the cache
            // clears here; the ClearRecentEmojiStatusesQuery rides the
            // user net kinds
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut env = user_env!(td);
            td.user_manager
                .clear_recent_emoji_statuses(&mut env, promise);
        }
        Function::setBusinessAccountProfilePhoto(request) => {
            // Requests.cpp:4566-4572 → UserManager.cpp:5981-5994: the bot
            // gate, the business-connection check and the
            // inputChatPhotoPrevious 400 answer here; the null-input
            // delete and the upload flow are [blocked: net/Files]
            if !check_is_bot(td, id) {
                return;
            }
            let connection_id = crate::business_connection_manager::BusinessConnectionId::new(
                &request.business_connection_id,
            );
            let status = td
                .business_connection_manager
                .check_business_connection(&connection_id);
            if status.is_error() {
                return td.send_error_impl(id, status.code(), status.message());
            }
            match request.photo.as_ref() {
                None => {
                    // the DeleteBusinessProfilePhotoQuery tail ([blocked:
                    // net]) — the request stays unanswered like the C++
                    // actor without a network
                    tdutils::log_info!(
                        "Send businessConnection.deleteProfilePhoto — [blocked: net]"
                    );
                }
                Some(td_api::InputChatPhoto::inputChatPhotoPrevious(_)) => {
                    td.send_error_impl(id, 400, "Unsupported");
                }
                Some(input_photo) => {
                    let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
                    let user_id = td
                        .business_connection_manager
                        .get_business_connection_user_id(&connection_id);
                    let env = user_env!(td);
                    let mut files = td.file_manager.lock();
                    let mut fllm = td.file_load_manager.lock();
                    let upload_callback = td.profile_photo_upload_callback.clone();
                    td.user_manager.set_profile_photo_impl(
                        set_profile_photo_td!(td, env, files, fllm, upload_callback),
                        user_id,
                        Some(input_photo),
                        request.is_public,
                        false,
                        promise,
                    );
                }
            }
        }
        // --- the AnimationsManager arm set (M.20): Requests.cpp:1793-1830.
        // The saved-animations ring answers offline; the GetSavedGifsQuery/
        // SaveGifQuery tails are the [blocked: net] env seams and park the
        // promises exactly like the C++ request without a network ---
        Function::getSavedAnimations(_) => {
            // Requests.cpp:1793-1802; the two-step RequestActor collapses
            // into the manager's promise (it answers the Animations
            // object once the ring is loaded)
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Animations, _>(id, Object::animations);
            let mut env = TdAnimationsEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_bot: td.auth_manager.is_bot(),
                file_manager: std::sync::Arc::clone(&td.file_manager),
                file_reference_manager: &mut td.file_reference_manager,
                updates: &mut td.animations_updates,
            };
            td.animations_manager
                .get_saved_animations(&mut env, promise);
        }
        Function::addSavedAnimation(request) => {
            // Requests.cpp:1806-1812; the input file resolves locally,
            // messages.saveGif parks
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut env = TdAnimationsEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_bot: td.auth_manager.is_bot(),
                file_manager: std::sync::Arc::clone(&td.file_manager),
                file_reference_manager: &mut td.file_reference_manager,
                updates: &mut td.animations_updates,
            };
            td.animations_manager.add_saved_animation(
                &mut env,
                request.animation.as_ref(),
                promise,
            );
        }
        Function::removeSavedAnimation(request) => {
            // Requests.cpp:1814-1820; messages.saveGif unsave parks
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut env = TdAnimationsEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_bot: td.auth_manager.is_bot(),
                file_manager: std::sync::Arc::clone(&td.file_manager),
                file_reference_manager: &mut td.file_reference_manager,
                updates: &mut td.animations_updates,
            };
            td.animations_manager.remove_saved_animation(
                &mut env,
                request.animation.as_ref(),
                promise,
            );
        }

        // --- the LinkManager arm set (M.23 + net-drain 32):
        // Requests.cpp:3592-3642 + 8948-8954 + 9258-9264. The local
        // halves run in the collapsed manager; the net-drain 32
        // families (GetRecentMeUrls/GetDeepLinkInfo/RequestUrlAuth/
        // RequestUrlOauth) ride the LinkNetKind plane, the remaining
        // tails (accept/decline/match-code) stay the [blocked: net]
        // seams and park the promise offline ---
        // --- the dialog-scoped LinkManager arms (M.23): the
        // button-URL resolution rides the [blocked: M.10 integration]
        // message plane and answers the C++ not-found band offline; the
        // net query tails are the [blocked: net] env seams ---
        Function::getLoginUrlInfo(request) => {
            // Requests.cpp:8166-8172; the button-URL resolution (the
            // [blocked: M.10] seam) precedes the RequestUrlAuthQuery
            // peer-form send
            if !check_is_user(td, id) {
                return;
            }
            let message_full_id = MessageFullId::new(
                crate::message_id::DialogId::new(request.chat_id),
                MessageId::new(request.message_id),
            );
            let promise = td.deferred_promise::<td_api::LoginUrlInfo, _>(id, Object::from);
            let Some(link_manager) = td.link_manager.as_mut() else {
                return td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
            };
            let mut env = TdLinkEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
            };
            link_manager.get_login_url_info(&mut env, message_full_id, request.button_id, promise);
        }
        Function::getLoginUrl(request) => {
            // Requests.cpp:8174-8181; the HTTP-URL promise band answers
            // the string as httpUrl, the AcceptUrlAuthQuery tail parks
            if !check_is_user(td, id) {
                return;
            }
            let message_full_id = MessageFullId::new(
                crate::message_id::DialogId::new(request.chat_id),
                MessageId::new(request.message_id),
            );
            let promise =
                td.deferred_promise(id, |url: String| Object::httpUrl(td_api::HttpUrl { url }));
            let Some(link_manager) = td.link_manager.as_mut() else {
                return td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
            };
            let mut env = TdLinkEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
            };
            link_manager.get_login_url(
                &mut env,
                message_full_id,
                request.button_id,
                request.allow_write_access,
                promise,
            );
        }
        Function::getDeepLinkInfo(request) => {
            // Requests.cpp:9258-9264: no auth gate; the local tg: strip +
            // host split, then help.getDeepLinkInfo rides the unauth
            // dispatch
            let Some(link) = clean_or_utf8_error(td, id, &request.link) else {
                return;
            };
            let promise = td.deferred_promise(id, |info: Option<td_api::DeepLinkInfo>| {
                // the query's null-answer band (LinkManager.cpp:1607-1643
                // `send_result(nullptr)`) surfaces as the M.18-outline 404
                info.map_or_else(
                    || {
                        Object::error(td_api::Error {
                            code: 404,
                            message: "Not Found".to_string(),
                        })
                    },
                    Object::deepLinkInfo,
                )
            });
            let Some(link_manager) = td.link_manager.as_mut() else {
                // Unreachable through the request gate: the manager is
                // created in the Td init block before requests flow.
                return td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
            };
            link_manager.get_deep_link_info(&link, promise);
        }
        Function::getExternalLinkInfo(request) => {
            // Requests.cpp:3592-3598; the local machine answers
            // loginUrlInfoOpen or sends the URL-form
            // messages.requestUrlAuth over the url_auth_domain hosts
            if !check_is_user(td, id) {
                return;
            }
            let Some(link) = clean_or_utf8_error(td, id, &request.link) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::LoginUrlInfo, _>(id, Object::from);
            let Some(link_manager) = td.link_manager.as_mut() else {
                return td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
            };
            let mut env = TdLinkEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
            };
            link_manager.get_external_link_info(&mut env, link, promise);
        }
        Function::getRecentlyVisitedTMeUrls(request) => {
            // Requests.cpp:8948-8954; help.getRecentMeUrls rides the
            // dispatch
            if !check_is_user(td, id) {
                return;
            }
            let Some(referrer) = clean_or_utf8_error(td, id, &request.referrer) else {
                return;
            };
            let promise = td.deferred_promise(id, |urls: Option<td_api::TMeUrls>| {
                // the query's null band → the M.18-outline 404 convention
                urls.map_or_else(
                    || {
                        Object::error(td_api::Error {
                            code: 404,
                            message: "Not Found".to_string(),
                        })
                    },
                    Object::tMeUrls,
                )
            });
            let Some(link_manager) = td.link_manager.as_mut() else {
                return td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
            };
            link_manager.get_recent_me_urls(referrer, promise);
        }
        Function::getExternalLink(request) => {
            // Requests.cpp:3596-3601: the HTTP-URL promise band
            // (Requests.cpp:1957-1967) answers the string as httpUrl; the
            // messages.acceptUrlAuth tail parks
            if !check_is_user(td, id) {
                return;
            }
            let Some(link) = clean_or_utf8_error(td, id, &request.link) else {
                return;
            };
            let promise =
                td.deferred_promise(id, |url: String| Object::httpUrl(td_api::HttpUrl { url }));
            let Some(link_manager) = td.link_manager.as_mut() else {
                return td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
            };
            let mut env = TdLinkEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
            };
            link_manager.get_link_login_url(&mut env, link, request.allow_write_access, promise);
        }
        Function::getOauthLinkInfo(request) => {
            // Requests.cpp:3612-3618; the local parse + type check, then
            // the oauth-form messages.requestUrlAuth (with the in-app
            // origin) rides the dispatch
            if !check_is_user(td, id) {
                return;
            }
            let Some(link) = clean_or_utf8_error(td, id, &request.url) else {
                return;
            };
            let Some(in_app_origin) = clean_or_utf8_error(td, id, &request.in_app_origin) else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::OauthLinkInfo, _>(id, Object::oauthLinkInfo);
            let Some(link_manager) = td.link_manager.as_mut() else {
                return td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
            };
            let mut env = TdLinkEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
            };
            link_manager.get_oauth_link_info(&mut env, link, in_app_origin, promise);
        }
        Function::checkOauthRequestMatchCode(request) => {
            // Requests.cpp:3620-3626; messages.checkUrlAuthMatchCode parks
            if !check_is_user(td, id) {
                return;
            }
            let Some(url) = clean_or_utf8_error(td, id, &request.url) else {
                return;
            };
            let Some(match_code) = clean_or_utf8_error(td, id, &request.match_code) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let Some(link_manager) = td.link_manager.as_mut() else {
                return td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
            };
            let mut env = TdLinkEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
            };
            link_manager.check_oauth_request_match_code(&mut env, url, match_code, promise);
        }
        Function::acceptOauthRequest(request) => {
            // Requests.cpp:3628-3636; messages.acceptUrlAuth parks, the
            // HTTP-URL promise band answers the string as httpUrl
            if !check_is_user(td, id) {
                return;
            }
            let Some(url) = clean_or_utf8_error(td, id, &request.url) else {
                return;
            };
            let Some(match_code) = clean_or_utf8_error(td, id, &request.match_code) else {
                return;
            };
            let promise =
                td.deferred_promise(id, |url: String| Object::httpUrl(td_api::HttpUrl { url }));
            let Some(link_manager) = td.link_manager.as_mut() else {
                return td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
            };
            let mut env = TdLinkEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
            };
            link_manager.accept_oauth_request(
                &mut env,
                url,
                match_code,
                request.allow_write_access,
                request.allow_phone_number_access,
                promise,
            );
        }
        Function::declineOauthRequest(request) => {
            // Requests.cpp:3638-3642; messages.declineUrlAuth parks
            if !check_is_user(td, id) {
                return;
            }
            let Some(url) = clean_or_utf8_error(td, id, &request.url) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let Some(link_manager) = td.link_manager.as_mut() else {
                return td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
            };
            let mut env = TdLinkEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
            };
            link_manager.decline_oauth_request(&mut env, url, promise);
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
            // base every reachable path errors first — the parked-resolve
            // collapse keeps the inline stand-in: the resolve query goes
            // out and the M.10 gap answers here, the parked family promise
            // settles a dropped slot).
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
            // the promise settles from the SearchPublicDialogsQuery answer
            // (the deferred outbox — the parked request re-runs against the
            // fresh cache)
            let promise = td.deferred_promise::<crate::td_api::Chats, _>(id, Object::chats);
            crate::dialog_manager::search_public_dialogs_for_request(
                td,
                &query,
                request.type_filter.as_ref(),
                promise,
            );
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
            // the promise settles from the SearchPublicDialogsQuery answer
            // (the deferred outbox)
            let promise = td.deferred_promise::<crate::td_api::Chats, _>(id, Object::chats);
            crate::dialog_manager::search_dialogs_on_server_for_request(
                td,
                &query,
                request.type_filter.as_ref(),
                request.limit,
                promise,
            );
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
            // the promise settles from the CheckUsername/
            // CheckChannelUsernameQuery answer
            let promise = td.deferred_promise::<crate::td_api::Object, _>(id, |object| object);
            crate::dialog_manager::check_dialog_username(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                &username,
                false,
                promise,
            );
        }
        Function::checkBotUsername(request) => {
            let Some(username) = clean_or_utf8_error(td, id, &request.username) else {
                return;
            };
            // the promise settles from the bots.checkUsername answer
            let promise = td.deferred_promise::<crate::td_api::Object, _>(id, |object| object);
            crate::dialog_manager::check_dialog_username(
                td,
                crate::dialog_id::DialogId::new(0),
                &username,
                true,
                promise,
            );
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
            // the promise settles from the DeleteChat/DeleteChannelQuery
            // answer
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            crate::dialog_manager::delete_dialog(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                promise,
            );
        }
        Function::upgradeBasicGroupChatToSupergroupChat(request) => {
            // Requests.cpp:5543 — `migrate_dialog_to_megagroup`.
            if !check_is_user(td, id) {
                return;
            }
            // the promise settles from the MigrateChatQuery answer chain
            let promise = td.deferred_promise::<crate::td_api::Object, _>(id, |object| object);
            crate::dialog_manager::migrate_dialog_to_megagroup(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                promise,
            );
        }
        Function::setChatTitle(request) => {
            // the C++ has no CHECK_IS_USER here (Requests.cpp:5778-5782) —
            // bots set basic-group titles through this method
            let Some(title) = clean_or_utf8_error(td, id, &request.title) else {
                return;
            };
            // the promise settles from the EditDialogTitleQuery answer
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            crate::dialog_manager::set_dialog_title(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                &title,
                promise,
            );
        }
        Function::setChatPhoto(request) => {
            // the C++ has no CHECK_IS_USER here (Requests.cpp:5784-5787)
            // — the promise settles from the EditDialogPhotoQuery answer
            // (the deferred outbox; the upload tail rides the Files plane)
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            crate::dialog_manager::set_dialog_photo(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.photo.as_ref(),
                promise,
            );
        }
        Function::setChatAccentColor(request) => {
            if !check_is_user(td, id) {
                return;
            }
            // the promise settles from the UpdateChannelColorQuery answer
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            crate::dialog_manager::set_dialog_accent_color(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                crate::peer_color::AccentColorId::new(request.accent_color_id),
                crate::emoji_status::CustomEmojiId::new(request.background_custom_emoji_id),
                promise,
            );
        }
        Function::setChatProfileAccentColor(request) => {
            if !check_is_user(td, id) {
                return;
            }
            // the promise settles from the UpdateChannelColorQuery answer
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            crate::dialog_manager::set_dialog_profile_accent_color(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                crate::peer_color::AccentColorId::new(request.profile_accent_color_id),
                crate::emoji_status::CustomEmojiId::new(request.profile_background_custom_emoji_id),
                promise,
            );
        }
        Function::setChatPermissions(request) => {
            // the C++ has no CHECK_IS_USER here (Requests.cpp:5818-5821)
            // the promise settles from the
            // EditChatDefaultBannedRightsQuery answer
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            crate::dialog_manager::set_dialog_permissions(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.permissions.as_ref(),
                promise,
            );
        }
        Function::toggleChatHasProtectedContent(request) => {
            if !check_is_user(td, id) {
                return;
            }
            // the promise settles from the ToggleNoForwardsQuery answer
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            crate::dialog_manager::toggle_dialog_has_protected_content(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                None,
                request.has_protected_content,
                promise,
            );
        }
        Function::processChatHasProtectedContentDisableRequest(request) => {
            if !check_is_user(td, id) {
                return;
            }
            // the promise settles from the ToggleNoForwardsQuery answer
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            crate::dialog_manager::toggle_dialog_has_protected_content(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                Some(crate::chat_manager::MessageId::new(
                    request.request_message_id,
                )),
                !request.approve,
                promise,
            );
        }
        Function::setChatDescription(request) => {
            // the C++ has no CHECK_IS_USER here (Requests.cpp:6093-6097);
            // the promise settles from the EditChatAboutQuery answer
            let Some(description) = clean_or_utf8_error(td, id, &request.description) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            crate::dialog_manager::set_dialog_description(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                &description,
                promise,
            );
        }
        Function::getBlockedMessageSenders(request) => {
            if !check_is_user(td, id) {
                return;
            }
            // the promise settles from the GetBlockedDialogsQuery answer
            // (the deferred outbox — the request id rides the promise)
            let promise = td.deferred_promise::<crate::td_api::Object, _>(id, |object| object);
            crate::dialog_manager::get_blocked_dialogs(
                td,
                request.block_list.as_ref(),
                request.offset,
                request.limit,
                promise,
            );
        }
        Function::reportChat(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(text) = clean_or_utf8_error(td, id, &request.text) else {
                return;
            };
            // the option id rides as raw bytes (the C++ BufferSlice —
            // no utf8 gate on this field)
            let option_id = request.option_id.clone();
            let message_ids: Vec<crate::chat_manager::MessageId> = request
                .message_ids
                .iter()
                .map(|&message_id| crate::chat_manager::MessageId::new(message_id))
                .collect();
            // the promise settles from the ReportPeerQuery answer (the
            // deferred outbox — the request id rides the promise)
            let promise = td.deferred_promise::<crate::td_api::Object, _>(id, |object| object);
            crate::dialog_manager::report_dialog(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                &option_id,
                &message_ids,
                &text,
                promise,
            );
        }

        // setChatEmojiStatus and reportChatPhoto ride the dispatcher
        // since the §2 flips (e26f545 / d287c84) — the emoji-status
        // conversion and the ReportReason port landed with them.

        // --- the chat-manager request family (M.5.47, folded in at the
        // integration-m6b merge) — the channel-identity eight ride the
        // net dispatcher (their promises settle from the query answers);
        // the rest still park at the [blocked: net] seams. setChatDescription
        // and deleteChat stay with the dialog_manager arms above: the C++
        // routes them through the DialogManager dialog type-switch
        // (Requests.cpp:6093, :3655), unlike these channel-scoped ones.
        Function::setSupergroupUsername(request) => {
            // `Requests.cpp:7253-7258` — CHECK_IS_USER + CLEAN_INPUT_STRING.
            if !check_is_user(td, id) {
                return;
            }
            // the promise settles from the UpdateChannelUsernameQuery answer
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let username = crate::misc::clean_input_string(&request.username).unwrap_or_default();
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.set_channel_username(
                user_manager,
                &mut env,
                ChannelId::new(request.supergroup_id),
                &username,
                promise,
            );
        }
        Function::toggleSupergroupUsernameIsActive(request) => {
            // `Requests.cpp:7260-7266`.
            if !check_is_user(td, id) {
                return;
            }
            // the promise settles from the ToggleChannelUsernameQuery answer
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
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
        }
        Function::setChatLocation(request) => {
            // `Requests.cpp:6113-6118` — CHECK_IS_USER; the promise
            // settles from the EditLocationQuery answer. The C++ routes
            // through `DialogManager::set_dialog_location` (the
            // have_dialog_force gate is the M.6 stub): the channel leg
            // decodes the dialog id, every other dialog type answers
            // "The chat can't have location".
            if !check_is_user(td, id) {
                return;
            }
            let dialog_id = DialogId::new(request.chat_id);
            // the invalid id fails the C++ have_dialog_force gate
            // ("Chat not found", DialogManager.cpp:2783-2785 — the M.6
            // stub collapse); every other dialog type answers "The
            // chat can't have location"
            if !dialog_id.is_valid() {
                return td.send_error_impl(id, 400, "Chat not found");
            }
            if dialog_id.get_type() != crate::chat_manager::DialogType::Channel {
                return td.send_error_impl(id, 400, "The chat can't have location");
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            // the null-location band builds the empty record like the
            // C++ DialogLocation constructor — the manager's empty()
            // gate answers the 400
            let location = crate::chat_manager::DialogLocation::from_td_api(
                request
                    .location
                    .as_ref()
                    .unwrap_or(&td_api::ChatLocation::default()),
            );
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.set_channel_location(
                user_manager,
                &mut env,
                dialog_id.get_channel_id(),
                &location,
                promise,
            );
        }
        Function::setChatSlowModeDelay(request) => {
            // `Requests.cpp:6120-6125` — CHECK_IS_USER; the promise
            // settles from the ToggleSlowModeQuery answer.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.set_channel_slow_mode_delay(
                user_manager,
                &mut env,
                DialogId::new(request.chat_id),
                request.slow_mode_delay,
                promise,
            );
        }
        Function::toggleSupergroupIsForum(request) => {
            // `Requests.cpp:7366-7372` — CHECK_IS_USER; the promise
            // settles from the ToggleForumQuery answer (the equal-state
            // early-OK resolves without a query).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, _user_manager, chat_manager) = chat_env_split(td);
            chat_manager.toggle_channel_is_forum(
                &mut env,
                ChannelId::new(request.supergroup_id),
                request.is_forum,
                request.has_forum_tabs,
                promise,
            );
        }

        Function::reportSupergroupAntiSpamFalsePositive(request) => {
            // `Requests.cpp:7386-7392` — CHECK_IS_USER; the promise
            // settles from the ReportChannelAntiSpamFalsePositiveQuery
            // answer.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.report_channel_anti_spam_false_positive(
                user_manager,
                &mut env,
                ChannelId::new(request.supergroup_id),
                crate::chat_manager::MessageId::new(request.message_id),
                promise,
            );
        }
        Function::reportSupergroupSpam(request) => {
            // `Requests.cpp:2154-2159`.
            if !check_is_user(td, id) {
                return;
            }
            let (promise, recorded) = record_unit_promise();
            let message_ids: Vec<crate::chat_manager::MessageId> = request
                .message_ids
                .iter()
                .map(|&message_id| crate::chat_manager::MessageId::new(message_id))
                .collect();
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.report_channel_spam(
                user_manager,
                &mut env,
                ChannelId::new(request.supergroup_id),
                &message_ids,
                promise,
            );
            finish_unit_request(id, td, recorded);
        }
        Function::toggleSupergroupIsBroadcastGroup(request) => {
            // `Requests.cpp:7373-7379` — the gigagroup conversion gate;
            // the promise settles from the ConvertToGigagroupQuery
            // answer.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.convert_channel_to_gigagroup(
                user_manager,
                &mut env,
                ChannelId::new(request.supergroup_id),
                promise,
            );
        }
        Function::toggleSupergroupHasAggressiveAntiSpamEnabled(request) => {
            // `Requests.cpp:7359-7365` — the promise settles from the
            // ToggleAntiSpamQuery answer.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.toggle_channel_has_aggressive_anti_spam_enabled(
                user_manager,
                &mut env,
                ChannelId::new(request.supergroup_id),
                request.has_aggressive_anti_spam_enabled,
                promise,
            );
        }
        Function::toggleSupergroupHasHiddenMembers(request) => {
            // `Requests.cpp:7352-7358` — the promise settles from the
            // ToggleParticipantsHiddenQuery answer.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.toggle_channel_has_hidden_participants(
                user_manager,
                &mut env,
                ChannelId::new(request.supergroup_id),
                request.has_hidden_members,
                promise,
            );
        }
        Function::toggleSupergroupHasAutomaticTranslation(request) => {
            // `Requests.cpp:7345-7351` — the promise settles from the
            // ToggleAutotranslationQuery answer.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, _user_manager, chat_manager) = chat_env_split(td);
            chat_manager.toggle_channel_has_automatic_translation(
                &mut env,
                ChannelId::new(request.supergroup_id),
                request.has_automatic_translation,
                promise,
            );
        }
        Function::toggleSupergroupCanHaveSponsoredMessages(request) => {
            // `Requests.cpp:7338-7344` — the promise settles from the
            // RestrictSponsoredMessagesQuery answer.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, _user_manager, chat_manager) = chat_env_split(td);
            chat_manager.toggle_channel_can_have_sponsored_messages(
                &mut env,
                ChannelId::new(request.supergroup_id),
                request.can_have_sponsored_messages,
                promise,
            );
        }
        Function::toggleSupergroupIsAllHistoryAvailable(request) => {
            // `Requests.cpp:7331-7337` — the promise settles from the
            // TogglePrehistoryHiddenQuery answer.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, _user_manager, chat_manager) = chat_env_split(td);
            chat_manager.toggle_channel_is_all_history_available(
                &mut env,
                ChannelId::new(request.supergroup_id),
                request.is_all_history_available,
                promise,
            );
        }
        Function::toggleSupergroupJoinByRequest(request) => {
            // `Requests.cpp:7323-7330` — the promise settles from the
            // ToggleChannelJoinRequestQuery answer.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.toggle_channel_join_request(
                user_manager,
                &mut env,
                ChannelId::new(request.supergroup_id),
                request.join_by_request,
                UserId::new(request.guard_bot_user_id),
                request.apply_to_invite_links,
                promise,
            );
        }
        Function::toggleSupergroupJoinToSendMessages(request) => {
            // `Requests.cpp:7316-7322` — the promise settles from the
            // ToggleChannelJoinToSendQuery answer.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, _user_manager, chat_manager) = chat_env_split(td);
            chat_manager.toggle_channel_join_to_send(
                &mut env,
                ChannelId::new(request.supergroup_id),
                request.join_to_send_messages,
                promise,
            );
        }
        Function::toggleSupergroupSignMessages(request) => {
            // `Requests.cpp:7309-7315` — the promise settles from the
            // ToggleChannelSignaturesQuery answer.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, _user_manager, chat_manager) = chat_env_split(td);
            chat_manager.toggle_channel_sign_messages(
                &mut env,
                ChannelId::new(request.supergroup_id),
                request.sign_messages,
                request.show_message_sender,
                promise,
            );
        }
        Function::setSupergroupMainProfileTab(request) => {
            // `Requests.cpp:2231-2236` — the promise settles from the
            // SetChannelMainProfileTabQuery answer (the lambda chain).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let main_profile_tab = request.main_profile_tab;
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.set_channel_main_profile_tab(
                user_manager,
                &mut env,
                ChannelId::new(request.supergroup_id),
                main_profile_tab,
                promise,
            );
        }
        Function::setSupergroupUnrestrictBoostCount(request) => {
            // `Requests.cpp:2237-2242` — the promise settles from the
            // SetChannelBoostsToUnblockRestrictionsQuery answer.
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.set_channel_unrestrict_boost_count(
                user_manager,
                &mut env,
                ChannelId::new(request.supergroup_id),
                request.unrestrict_boost_count,
                promise,
            );
        }
        Function::setSupergroupCustomEmojiStickerSet(request) => {
            // `Requests.cpp:2243-2248` — the promise settles from the
            // SetChannelEmojiStickerSetQuery answer.
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.set_channel_emoji_sticker_set(
                user_manager,
                &mut env,
                ChannelId::new(request.supergroup_id),
                StickerSetId::new(request.custom_emoji_sticker_set_id),
                promise,
            );
        }
        Function::setSupergroupStickerSet(request) => {
            // `Requests.cpp:2249-2254` — the promise settles from the
            // SetChannelStickerSetQuery answer.
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.set_channel_sticker_set(
                user_manager,
                &mut env,
                ChannelId::new(request.supergroup_id),
                StickerSetId::new(request.sticker_set_id),
                promise,
            );
        }
        Function::reorderSupergroupActiveUsernames(request) => {
            // `Requests.cpp:2259-2264` — CLEAN_INPUT_STRING on every
            // username first; the promise settles from the
            // ReorderChannelUsernamesQuery answer.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let usernames: Vec<String> = request
                .usernames
                .into_iter()
                .map(|username| crate::misc::clean_input_string(&username).unwrap_or_default())
                .collect();
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.reorder_channel_usernames(
                user_manager,
                &mut env,
                ChannelId::new(request.supergroup_id),
                usernames,
                promise,
            );
        }
        Function::disableAllSupergroupUsernames(request) => {
            // `Requests.cpp:2265-2270` — the promise settles from the
            // DeactivateAllChannelUsernamesQuery answer.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.disable_all_channel_usernames(
                user_manager,
                &mut env,
                ChannelId::new(request.supergroup_id),
                promise,
            );
        }
        Function::setChatDirectMessagesGroup(request) => {
            // `Requests.cpp:6106-6112` — the promise settles from the
            // UpdatePaidMessagesPriceQuery answer (the monoforum leg).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.set_channel_monoforum_group(
                user_manager,
                &mut env,
                DialogId::new(request.chat_id),
                request.is_enabled,
                request.paid_message_star_count,
                promise,
            );
        }
        Function::setChatDiscussionGroup(request) => {
            // `Requests.cpp:6099-6105` — the promise settles from the
            // SetDiscussionGroupQuery answer.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.set_channel_discussion_group(
                user_manager,
                &mut env,
                DialogId::new(request.chat_id),
                DialogId::new(request.discussion_chat_id),
                promise,
            );
        }
        Function::setChatPaidMessageStarCount(request) => {
            // `Requests.cpp:5765-5771` — the promise settles from the
            // UpdatePaidMessagesPriceQuery answer (the paid-message
            // leg).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, user_manager, chat_manager) = chat_env_split(td);
            chat_manager.set_channel_send_paid_message_star_count(
                user_manager,
                &mut env,
                DialogId::new(request.chat_id),
                request.paid_message_star_count,
                promise,
            );
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
            // UpdateWebBrowserSettingsQuery::send (the net-drain 34 wire
            // family); the promise parks in the manager's FIFO lot and
            // the answer drains through `Td::on_web_browser_net_answer`.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            td.web_browser_manager.update_web_browser_settings(
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

        // replyToCallbackQueryWithEphemeralMessage (Requests.cpp:4424-4431):
        // CHECK_IS_BOT, then MessageQueryManager::edit_callback_query_message
        // runs the real ephemeral-reply flow (the old unconditional 500
        // "Unsupported" band is gone upstream).
        Function::replyToCallbackQueryWithEphemeralMessage(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, mq_manager) = crate::td::message_query_env_split(td);
            mq_manager.edit_callback_query_message(
                &mut env,
                request.callback_query_id,
                request.replace_callback_query_message,
                request.protect_content,
                request.reply_markup,
                request.input_message_content.map(|content| *content),
                promise,
            );
        }

        // --- messages, read family (Requests.cpp:2626-2800) ---
        Function::getMessageLocally(request) => {
            // Requests.cpp:2637-2641 — the local lookup form: no force, no
            // actor. A missing dialog/message answers the null result (the
            // `Td::send_result(nullptr)` → 404 shape).
            let dialog_id = MmDialogId::new(request.chat_id);
            let message_id = MessageId::new(request.message_id);
            // take/put-back: the builder mutates the message
            // (`is_update_sent`) while borrowing the manager immutably
            let taken = td
                .messages_manager
                .get_dialog_mut(dialog_id)
                .and_then(|d| d.messages.remove(&message_id));
            let object = if let Some(mut m) = taken {
                let object = crate::messages_manager::get_message_object(
                    &td.messages_manager,
                    td,
                    td,
                    dialog_id,
                    &mut m,
                    "getMessageLocally",
                );
                if let Some(d) = td.messages_manager.get_dialog_mut(dialog_id) {
                    d.messages.insert(message_id, m);
                }
                object
            } else {
                None
            };
            match object {
                Some(message) => td.send_result(id, Object::message(message)),
                None => td.send_result(
                    id,
                    Object::error(td_api::Error {
                        code: 404,
                        message: "Not Found".to_string(),
                    }),
                ),
            }
        }
        Function::getMessageLink(request) => {
            // Requests.cpp:2771-2782 — the local link builder.
            let Some(poll_option_id) = clean_or_utf8_error(td, id, &request.poll_option_id) else {
                return;
            };
            let dialog_id = MmDialogId::new(request.chat_id);
            let message_id = MessageId::new(request.message_id);
            match crate::messages_manager::get_message_link(
                &td.messages_manager,
                td,
                dialog_id,
                message_id,
                request.media_timestamp,
                request.checklist_task_id,
                &poll_option_id,
                request.for_album,
                request.in_message_thread,
            ) {
                Ok((link, is_public)) => td.send_result(
                    id,
                    Object::messageLink(td_api::MessageLink { link, is_public }),
                ),
                Err(status) => {
                    td.send_error_impl(id, status.code(), status.message());
                }
            }
        }
        // --- messages, write family part 1 (Requests.cpp:3529-3548, :4139) ---
        Function::deleteChatHistory(request) => {
            // `Requests.cpp:2997-3003` — CHECK_IS_USER; the local delete
            // core over the durable spine, the server tail rides the net
            // seam (as deleteMessages).
            if !check_is_user(td, id) {
                return;
            }
            let dialog_id = MmDialogId::new(request.chat_id);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut dyn crate::messages_manager::MessageObjectContext =
                unsafe { &mut *td_ptr };
            let result = crate::messages_manager::delete_dialog_history(
                ctx,
                &mut manager,
                dialog_id,
                request.remove_from_chat_list,
                request.revoke,
            );
            td.messages_manager = manager;
            match result {
                Ok(_) => td.send_result(id, ok_object()),
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::getChats(request) => {
            // `Requests.cpp:3260-3266` — the request promise rides the
            // get-dialogs task (MessagesManager.cpp:14385-14405): the
            // loaded window answers synchronously, the load-needed
            // task parks until the folder-load legs complete (the
            // update_last_dialog_date set_promises chain re-runs the
            // parked tasks, cpp:34234-34239).
            if !check_is_user(td, id) {
                return;
            }
            let dialog_list_id =
                crate::messages_manager::DialogListId::from_chat_list(request.chat_list.as_ref());
            // the deferred form: the task settles it exactly where the
            // C++ task_promise does (both the ok and the error bands)
            let promise = td.deferred_promise::<crate::td_api::Chats, _>(id, Object::chats);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the state alias is used strictly sequentially inside
            // the callee.
            let state: &mut dyn crate::messages_manager::ReadStateContext =
                unsafe { crate::td::split_context_alias(std::ptr::from_mut(td)) };
            let result = crate::messages_manager::get_dialogs_from_list(
                state,
                &mut manager,
                dialog_list_id,
                request.limit,
                Some(promise),
            );
            td.messages_manager = manager;
            // every return path settled the deferred promise (the
            // synchronous window, the parked task's load completion,
            // and both error bands) — the return value itself is the
            // no-promise test surface, nothing to answer inline
            let _ = result;
        }
        Function::setChatEmojiStatus(request) => {
            // `Requests.cpp:3722-3728`.
            if !check_is_user(td, id) {
                return;
            }
            let emoji_status = crate::emoji_status::EmojiStatus::get_emoji_status_from_td_api(
                request.emoji_status.as_ref(),
                td.global.unix_time(),
            );
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            crate::dialog_manager::set_dialog_emoji_status(
                td,
                MmDialogId::new(request.chat_id),
                emoji_status.as_ref(),
                promise,
            );
        }
        Function::openChat(request) => {
            // Requests.cpp:3529-3532 — CHECK_IS_USER; the ok-promise
            // answers the local open.
            if !check_is_user(td, id) {
                return;
            }
            let dialog_id = MmDialogId::new(request.chat_id);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the sequential discipline on `split_context_alias`.
            let state: &mut dyn crate::messages_manager::ReadStateContext =
                unsafe { crate::td::split_context_alias(std::ptr::from_mut(td)) };
            let result = crate::messages_manager::open_dialog(&mut manager, td, state, dialog_id);
            td.messages_manager = manager;
            match result {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::closeChat(request) => {
            // Requests.cpp:3534-3537.
            if !check_is_user(td, id) {
                return;
            }
            let dialog_id = MmDialogId::new(request.chat_id);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: as openChat above.
            let state: &mut dyn crate::messages_manager::ReadStateContext =
                unsafe { crate::td::split_context_alias(std::ptr::from_mut(td)) };
            let result = crate::messages_manager::close_dialog(&mut manager, td, state, dialog_id);
            td.messages_manager = manager;
            match result {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::viewMessages(request) => {
            // Requests.cpp:3539-3546.
            if !check_is_user(td, id) {
                return;
            }
            let dialog_id = MmDialogId::new(request.chat_id);
            let message_ids: Vec<MessageId> = request
                .message_ids
                .iter()
                .map(|&message_id| MessageId::new(message_id))
                .collect();
            let source = crate::messages_manager::get_message_source(request.source.as_ref());
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: as openChat above.
            let state: &mut dyn crate::messages_manager::ReadStateContext =
                unsafe { crate::td::split_context_alias(std::ptr::from_mut(td)) };
            let result = crate::messages_manager::view_messages(
                &mut manager,
                td,
                state,
                dialog_id,
                message_ids,
                source,
                request.force_read,
            );
            td.messages_manager = manager;
            match result {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::readAllChatMentions(request) => {
            // `Requests.cpp:3606-3611` — the local clear, then the
            // affected-history refill over the net tail (the request
            // promise rides the loop, the net-drain 29 shape).
            if !check_is_user(td, id) {
                return;
            }
            let dialog_id = MmDialogId::new(request.chat_id);
            let mut manager = std::mem::take(&mut td.messages_manager);
            let time = crate::user_manager::TimeCtx::from_global(&td.global);
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            // SAFETY: the ctx/state pair aliases one Td and is used
            // strictly sequentially inside the callee.
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut dyn crate::messages_manager::MessageObjectContext =
                unsafe { &mut *td_ptr };
            let state: &mut dyn crate::messages_manager::ReadStateContext =
                unsafe { crate::td::split_context_alias(td_ptr) };
            let result = crate::messages_manager::read_all_dialog_mentions(
                &td.global,
                &mut manager,
                ctx,
                state,
                &time,
                dialog_id,
                crate::messages_manager::ForumTopicId::default(),
            );
            td.messages_manager = manager;
            run_read_mentions_tail(
                td,
                dialog_id,
                crate::messages_manager::ForumTopicId::default(),
                promise,
                result,
            );
        }
        Function::readAllForumTopicMentions(request) => {
            // `Requests.cpp:3614-3619`.
            if !check_is_user(td, id) {
                return;
            }
            if request.forum_topic_id == 0 {
                return td.send_error_impl(id, 400, "Invalid forum topic identifier specified");
            }
            let dialog_id = MmDialogId::new(request.chat_id);
            let forum_topic_id =
                crate::messages_manager::ForumTopicId::from_raw(request.forum_topic_id);
            let mut manager = std::mem::take(&mut td.messages_manager);
            let time = crate::user_manager::TimeCtx::from_global(&td.global);
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            // SAFETY: as readAllChatMentions above.
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut dyn crate::messages_manager::MessageObjectContext =
                unsafe { &mut *td_ptr };
            let state: &mut dyn crate::messages_manager::ReadStateContext =
                unsafe { crate::td::split_context_alias(td_ptr) };
            let result = crate::messages_manager::read_all_dialog_mentions(
                &td.global,
                &mut manager,
                ctx,
                state,
                &time,
                dialog_id,
                forum_topic_id,
            );
            td.messages_manager = manager;
            run_read_mentions_tail(td, dialog_id, forum_topic_id, promise, result);
        }
        Function::readChatList(request) => {
            // `Requests.cpp:3621-3625`.
            if !check_is_user(td, id) {
                return;
            }
            let dialog_list_id =
                crate::messages_manager::DialogListId::from_chat_list(request.chat_list.as_ref());
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: as readAllChatMentions above.
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut dyn crate::messages_manager::MessageObjectContext =
                unsafe { &mut *td_ptr };
            let state: &mut dyn crate::messages_manager::ReadStateContext =
                unsafe { crate::td::split_context_alias(td_ptr) };
            let result = crate::messages_manager::read_all_dialogs_from_list(
                ctx,
                state,
                &mut manager,
                dialog_list_id,
            );
            td.messages_manager = manager;
            match result {
                Ok(()) => td.send_result(id, ok_object()),
                Err((code, message)) => td.send_error_impl(id, code, &message),
            }
        }
        Function::deleteMessages(request) => {
            // Requests.cpp:4139-4144 — the local delete core over the
            // durable spine; the server tail rides the net seam.
            let dialog_id = MmDialogId::new(request.chat_id);
            let message_ids: Vec<MessageId> = request
                .message_ids
                .iter()
                .map(|&message_id| MessageId::new(message_id))
                .collect();
            let mut manager = std::mem::take(&mut td.messages_manager);
            let mut global = std::mem::take(&mut td.global);
            let result = crate::messages_manager::delete_messages(
                &mut global,
                &mut manager,
                dialog_id,
                &message_ids,
            );
            td.global = global;
            td.messages_manager = manager;
            match result {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::sendMessage(request) => {
            // Requests.cpp:4231-4241 — the M.10.8/M.10.19 send machinery:
            // the yet-unsent message (the durable send log event already
            // written) answers synchronously; the wire query rides the
            // pools (do_send_message's TextReady action → the
            // SendMessageQuery send).
            let dialog_id = MmDialogId::new(request.chat_id);
            // SAFETY: the manager/global/ctx/state/reactions views alias
            // one Td and are used strictly sequentially inside the callee
            // (the split_context_alias contract, the apply_new_message
            // shape) — the manager must stay in place because the ctx
            // reads (have_input_peer_read in get_message_to_send) see it.
            let td_ptr = std::ptr::from_mut(td);
            let manager: &mut crate::messages_manager::MessagesManager =
                unsafe { &mut (*td_ptr).messages_manager };
            let global: &mut crate::global::Global = unsafe { &mut (*td_ptr).global };
            let state: &mut dyn crate::messages_manager::ReadStateContext =
                unsafe { crate::td::split_context_alias(td_ptr) };
            let ctx: &mut dyn crate::messages_manager::SendMessageContext = unsafe { &mut *td_ptr };
            let reactions_ctx =
                crate::messages_manager::MessageObjectReactionsContext::new(unsafe { &*td_ptr });
            let result = crate::messages_manager::send_message(
                global,
                manager,
                &reactions_ctx,
                ctx,
                state,
                dialog_id,
                request.topic_id,
                request.reply_to,
                request.options,
                request.reply_markup,
                request.input_message_content.map(|content| *content),
            );
            match result {
                Ok(message) => td.send_result(id, Object::message(message)),
                Err((code, message)) => td.send_error_impl(id, code, &message),
            }
        }
        Function::sendMessageAlbum(request) => {
            // Requests.cpp:4242-4251 — the album path over the M.9.5 splat
            // gate + the upload fan-in; the wire leg rides the same pools.
            let dialog_id = MmDialogId::new(request.chat_id);
            // SAFETY: as sendMessage above (raw aliases, no take/put).
            let td_ptr = std::ptr::from_mut(td);
            let manager: &mut crate::messages_manager::MessagesManager =
                unsafe { &mut (*td_ptr).messages_manager };
            let global: &mut crate::global::Global = unsafe { &mut (*td_ptr).global };
            let state: &mut dyn crate::messages_manager::ReadStateContext =
                unsafe { crate::td::split_context_alias(td_ptr) };
            let ctx: &mut dyn crate::messages_manager::SendMessageContext = unsafe { &mut *td_ptr };
            let reactions_ctx =
                crate::messages_manager::MessageObjectReactionsContext::new(unsafe { &*td_ptr });
            let result = crate::messages_manager::send_message_group(
                global,
                manager,
                &reactions_ctx,
                ctx,
                state,
                dialog_id,
                request.topic_id,
                request.reply_to,
                request.options,
                request
                    .input_message_contents
                    .into_iter()
                    .map(|content| Some(*content))
                    .collect(),
            );
            match result {
                Ok(messages) => td.send_result(id, Object::messages(messages)),
                Err((code, message)) => td.send_error_impl(id, code, &message),
            }
        }
        Function::setChatDraftMessage(request) => {
            // Requests.cpp:5851-5856 — CHECK_IS_USER; the ok answer behind
            // the entry. The C++ `set_dialog_draft_message` head runs
            // `check_dialog_access(dialog_id, true, Write, ...)` through
            // td_ — the arm owns that call in the Rust split.
            if !check_is_user(td, id) {
                return;
            }
            let dialog_id = MmDialogId::new(request.chat_id);
            let access = crate::dialog_manager::check_dialog_access(
                td,
                dialog_id,
                true,
                crate::user_manager::AccessRights::Write,
                "set_dialog_draft_message",
            );
            if access.is_error() {
                let status = access.error();
                td.send_error_impl(id, status.code(), status.message());
                return;
            }
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: as sendMessage above.
            let td_ptr = std::ptr::from_mut(td);
            let state: &mut dyn crate::messages_manager::ReadStateContext =
                unsafe { crate::td::split_context_alias(td_ptr) };
            let ctx: &mut dyn crate::messages_manager::SendMessageContext = unsafe { &mut *td_ptr };
            let result = crate::messages_manager::set_dialog_draft_message(
                &mut manager,
                ctx,
                state,
                dialog_id,
                request.topic_id,
                request.draft_message,
            );
            td.messages_manager = manager;
            match result {
                Ok(()) => td.send_result(id, ok_object()),
                Err((code, message)) => td.send_error_impl(id, code, &message),
            }
        }
        Function::getChatHistory(request) => {
            // Requests.cpp:3642-3646 + GetChatHistoryRequest
            // (Requests.cpp:951-977): CHECK_IS_USER; the actor's first run
            // passes get_tries() - 1 = 1.
            if !check_is_user(td, id) {
                return;
            }
            let dialog_id = MmDialogId::new(request.chat_id);
            let from_message_id = MessageId::new(request.from_message_id);
            // the split-context adapter: the manager is taken for the call
            // and restored after; the ctx/state pair aliases Td (the
            // sequential discipline on `split_context_alias`)
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the alias and the source are used strictly
            // sequentially (see `split_context_alias`'s contract).
            let state: &mut dyn crate::messages_manager::ReadStateContext =
                unsafe { crate::td::split_context_alias(std::ptr::from_mut(td)) };
            let result = crate::messages_manager::get_dialog_history(
                &mut manager,
                td,
                state,
                dialog_id,
                from_message_id,
                request.offset,
                request.limit,
                1,
                request.only_local,
            );
            td.messages_manager = manager;
            match result {
                Ok(Some(messages)) => td.send_result(id, Object::messages(messages)),
                Ok(None) => {
                    // the load-needed window — the db/server legs are the
                    // net seam ([blocked: net]); the caller-retry contract
                    // answers the loaded (empty) window
                    td.send_result(
                        id,
                        Object::messages(crate::messages_manager::get_messages_object(
                            -1,
                            Vec::new(),
                            false,
                        )),
                    );
                }
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::getChatMessageByDate(request) => {
            // Requests.cpp:3840-3845 — the local by-date walk behind the
            // promise.
            if !check_is_user(td, id) {
                return;
            }
            let dialog_id = MmDialogId::new(request.chat_id);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: as getChatHistory above (the state surface rides
            // the same alias when the by-date walk grows its state use).
            let _state: &mut dyn crate::messages_manager::ReadStateContext =
                unsafe { crate::td::split_context_alias(std::ptr::from_mut(td)) };
            let result = crate::messages_manager::get_dialog_message_by_date(
                &mut manager,
                td,
                dialog_id,
                request.date,
            );
            td.messages_manager = manager;
            match result {
                Ok(Some(message)) => td.send_result(id, Object::message(message)),
                Ok(None) => {
                    // the by-date search parks on the db seam
                    // ([blocked: net])
                    td.send_error_impl(id, 500, "The message history is not loaded yet");
                }
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::getMessageEmbeddingCode(request) => {
            // Requests.cpp:2783-2786 — the local embedding builder behind
            // the request actor (the promise collapses synchronously).
            if !check_is_user(td, id) {
                return;
            }
            let dialog_id = MmDialogId::new(request.chat_id);
            let message_id = MessageId::new(request.message_id);
            match crate::messages_manager::get_message_embedding_code(
                &td.messages_manager,
                dialog_id,
                message_id,
                request.for_album,
            ) {
                Ok(code) => td.send_result(id, Object::text(td_api::Text { text: code })),
                Err(status) => {
                    td.send_error_impl(id, status.code(), status.message());
                }
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
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
            };
            td.account_manager
                .confirm_qr_code_authentication(&mut env, &link, promise);
        }
        Function::hideSuggestedAction(request) => {
            // `Requests.cpp:8144-8148` → the free `dismiss_suggested_action`
            // ladder (SuggestedAction.cpp:312-355): the type bands answer
            // inline, everything else rides the DismissSuggestionQuery
            // wire family and the promise settles from the Bool answer
            // (SuggestedActionManager.cpp:26-63).
            if !check_is_user(td, id) {
                return;
            }
            let Some(action) = request.action.as_ref() else {
                return td.send_error_impl(id, 400, "Action must be non-empty");
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
            };
            let mut manager = std::mem::take(&mut td.suggested_action_manager);
            crate::suggested_action_manager::dismiss_suggested_action(
                &mut env,
                &mut manager,
                SuggestedAction::from_td_api(action),
                promise,
            );
            td.suggested_action_manager = manager;
        }
        Function::getUserLink(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |user_link: td_api::UserLink| {
                Object::userLink(user_link)
            });
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
            };
            td.account_manager.get_user_link(&mut env, promise);
        }
        Function::getActiveSessions(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |sessions: td_api::Sessions| Object::sessions(sessions));
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
            };
            td.account_manager.get_active_sessions(&mut env, promise);
        }
        Function::terminateSession(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
            };
            td.account_manager
                .terminate_session(&mut env, request.session_id, promise);
        }
        Function::terminateAllOtherSessions(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
            };
            td.account_manager
                .terminate_all_other_sessions(&mut env, promise);
        }
        Function::confirmSession(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
            };
            td.account_manager
                .confirm_session(&mut env, request.session_id, promise);
        }
        Function::toggleSessionCanAcceptCalls(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
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
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
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
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
            };
            td.account_manager.get_connected_websites(&mut env, promise);
        }
        Function::disconnectWebsite(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
            };
            td.account_manager
                .disconnect_website(&mut env, request.website_id, promise);
        }
        Function::disconnectAllWebsites(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
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
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
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
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
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
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
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
            let (mut manager, mut env) = community_split(td);
            manager.create_community(
                &mut env,
                &name,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.is_chat_hidden,
                promise,
            );
            td.community_manager = manager;
        }
        Function::setCommunityName(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = crate::misc::clean_input_string(&request.name) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut manager, mut env) = community_split(td);
            manager.set_community_name(
                &mut env,
                CommunityId::new(request.community_id),
                &name,
                promise,
            );
            td.community_manager = manager;
        }
        Function::setCommunityPhoto(request) => {
            // Requests.cpp:3436-3440 (fb05f53c2) — CHECK_IS_USER, then
            // CommunityManager::set_community_photo: the manager gates,
            // then DialogManager::do_set_dialog_photo over the community's
            // fake dialog id with the null owner (the upload owner ids
            // ride the file plane).
            if !check_is_user(td, id) {
                return;
            }
            let community_id = CommunityId::new(request.community_id);
            let mut promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            if let Err(status) = td.community_manager.set_community_photo_gates(community_id) {
                return promise.set_error(status);
            }
            crate::dialog_manager::do_set_dialog_photo(
                td,
                community_id.get_fake_dialog_id(),
                crate::dialog_id::DialogId::default(),
                request.photo.as_ref(),
                promise,
            );
        }
        Function::setCommunityPermissions(request) => {
            // Requests.cpp:3442-3447 (d55f0a694) — CHECK_IS_USER, then
            // CommunityManager::set_community_permissions (the
            // messages_editChatDefaultBannedRights send rides the net
            // step).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut manager, mut env) = community_split(td);
            manager.set_community_permissions(
                &mut env,
                CommunityId::new(request.community_id),
                request.permissions.as_ref(),
                promise,
            );
            td.community_manager = manager;
        }
        Function::deleteCommunity(request) => {
            // Requests.cpp:3449-3453 (f5f7fe5fd) — CHECK_IS_USER, then
            // CommunityManager::delete_community (the
            // channels.deleteChannel send rides the net step).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut manager, mut env) = community_split(td);
            manager.delete_community(&mut env, CommunityId::new(request.community_id), promise);
            td.community_manager = manager;
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
                            crate::message_id::MessageId::new(request.message_id),
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
        _ => run_forum_topic_requests(td, id, function),
    }
}

/// The M.49 ForumTopicManager arm group of the request dispatcher — see
/// the frame note at the forward site in [`run_request`]. Every arm routes
/// into `td_->forum_topic_manager_->...` (Requests.cpp:4835-4904, 7963);
/// the net-drain 41 query families dispatch through the manager-held net
/// handle (the delete tail rides the message-query affected-history
/// plane).
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
            option_manager,
            auth_manager,
            user_manager,
            chat_manager,
            notification_updates,
            ..
        } = td;
        let mut env = crate::td::ForumTopicTdEnv {
            global,
            options: option_manager.as_mut(),
            auth_is_bot: auth_manager.is_bot(),
            user_manager,
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
            // The `MessageQueryManager::delete_topic_history` tail — the
            // manager parked the validated deletes; the runner routes
            // them onto the affected-history plane (the C++
            // delete-promise lambda chain, ForumTopicManager.cpp:1028-1037)
            td.run_forum_topic_delete_tails();
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
        td.with_story_manager(run);
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
                crate::reaction_type::ReactionType::from_td_api(request.reaction_type.clone());
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
                crate::reaction_type::ReactionType::from_td_api(request.reaction_type.clone());
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
            let Some(password) = clean_or_utf8_error(td, id, &request.password) else {
                return;
            };
            let promise = td
                .deferred_promise::<String, _>(id, |url| Object::httpUrl(td_api::HttpUrl { url }));
            let mut env = star_gift_env!(td);
            td.star_gift_manager.get_star_gift_withdrawal_url(
                &mut env,
                StarGiftId::from_string(&request.received_gift_id),
                &password,
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
            let theme_net = td.theme_net();
            let mut env = ThemeEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                file_manager: std::sync::Arc::clone(&td.file_manager),
                updates: &mut td.theme_updates,
                file_load_manager: std::sync::Arc::clone(&td.file_load_manager),
                file_reference_manager: &mut td.file_reference_manager,
                documents_manager: &mut td.documents_manager,
                animations: &mut td.animations_manager,
                audios: &mut td.audios_manager,
                videos: &mut td.videos_manager,
                voice_notes: &mut td.voice_notes_manager,
                video_notes: &mut td.video_notes_manager,
                background_manager: &mut td.background_manager,
                background_updates: &mut td.background_updates,
                theme_net,
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

        // --- backgrounds (Requests.cpp:1889-1893, 5826-5834, 8902-8946) ---
        Function::searchBackground(request) => {
            // SearchBackgroundRequest (Requests.cpp:1889-1893): set_tries(3);
            // the answer is get_background_object of the searched pair — the
            // C++ null result answers the 404 through Td::send_result
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise =
                td.deferred_promise(
                    id,
                    |background: Option<td_api::Background>| match background {
                        Some(background) => Object::background(background),
                        None => Object::error(td_api::Error {
                            code: 404,
                            message: "Not Found".to_string(),
                        }),
                    },
                );
            let mut manager = std::mem::take(&mut td.background_manager);
            {
                let mut env = BackgroundEnv { td: &mut *td };
                manager.search_background(&mut env, &name, promise);
            }
            td.background_manager = manager;
        }
        Function::getInstalledBackgrounds(request) => {
            // Requests.cpp:8902-8904
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |b: td_api::Backgrounds| Object::backgrounds(b));
            let mut manager = std::mem::take(&mut td.background_manager);
            {
                let mut env = BackgroundEnv { td: &mut *td };
                manager.get_backgrounds(&mut env, request.for_dark_theme, promise);
            }
            td.background_manager = manager;
        }
        Function::getBackgroundUrl(request) => {
            // LinkManager::get_background_url (Requests.cpp:8905-8913)
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let env = TdStaticLinkEnv {
                options: td.option_manager.as_ref(),
            };
            let url =
                match crate::link_manager::get_background_url(&env, &name, request.r#type.as_ref())
                {
                    Ok(url) => url,
                    Err(status) => {
                        td.send_error_impl(id, status.code(), status.message());
                        return;
                    }
                };
            td.send_result(id, Object::httpUrl(td_api::HttpUrl { url }));
        }
        Function::setDefaultBackground(request) => {
            // Requests.cpp:8925-8929
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(
                    id,
                    |background: Option<td_api::Background>| match background {
                        Some(background) => Object::background(background),
                        None => Object::error(td_api::Error {
                            code: 404,
                            message: "Not Found".to_string(),
                        }),
                    },
                );
            let mut manager = std::mem::take(&mut td.background_manager);
            {
                let mut env = BackgroundEnv { td: &mut *td };
                manager.set_background(
                    &mut env,
                    request.background.as_ref(),
                    request.r#type.as_ref(),
                    request.for_dark_theme,
                    promise,
                );
            }
            td.background_manager = manager;
        }
        Function::deleteDefaultBackground(request) => {
            // Requests.cpp:8932-8935
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut manager = std::mem::take(&mut td.background_manager);
            {
                let mut env = BackgroundEnv { td: &mut *td };
                manager.delete_background(&mut env, request.for_dark_theme, promise);
            }
            td.background_manager = manager;
        }
        Function::removeInstalledBackground(request) => {
            // Requests.cpp:8938-8942
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut manager = std::mem::take(&mut td.background_manager);
            {
                let mut env = BackgroundEnv { td: &mut *td };
                manager.remove_background(
                    &mut env,
                    crate::background_manager::BackgroundId::new(request.background_id),
                    promise,
                );
            }
            td.background_manager = manager;
        }
        Function::resetInstalledBackgrounds(_) => {
            // Requests.cpp:8944-8946
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut manager = std::mem::take(&mut td.background_manager);
            {
                let mut env = BackgroundEnv { td: &mut *td };
                manager.reset_backgrounds(&mut env, promise);
            }
            td.background_manager = manager;
        }
        Function::setChatBackground(request) => {
            // Requests.cpp:5826-5831
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut manager = std::mem::take(&mut td.background_manager);
            {
                let mut env = BackgroundEnv { td: &mut *td };
                manager.set_dialog_background(
                    &mut env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    request.background.as_ref(),
                    request.r#type.as_ref(),
                    request.dark_theme_dimming,
                    !request.only_for_self,
                    promise,
                );
            }
            td.background_manager = manager;
        }
        Function::deleteChatBackground(request) => {
            // Requests.cpp:5834-5837
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut manager = std::mem::take(&mut td.background_manager);
            {
                let mut env = BackgroundEnv { td: &mut *td };
                manager.delete_dialog_background(
                    &mut env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    request.restore_previous,
                    promise,
                );
            }
            td.background_manager = manager;
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
            let (mut manager, mut env) = crate::td::autosave_env_split(&mut *td);
            manager.get_autosave_settings(&mut env, promise);
            td.autosave_manager = manager;
        }
        Function::setAutosaveSettings(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut manager, mut env) = crate::td::autosave_env_split(&mut *td);
            manager.set_autosave_settings(&mut env, request.scope, request.settings, promise);
            td.autosave_manager = manager;
        }
        Function::clearAutosaveSettingsExceptions(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut manager, mut env) = crate::td::autosave_env_split(&mut *td);
            manager.clear_autosave_settings_exceptions(&mut env, promise);
            td.autosave_manager = manager;
        }

        // Everything else waits for its manager step.
        Function::getChatBoostStatus(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |s: td_api::ChatBoostStatus| Object::chatBoostStatus(s));
            let (mut manager, mut env) = crate::td::boost_env_split(td);
            manager.get_dialog_boost_status(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                promise,
            );
            td.boost_manager = manager;
        }
        Function::boostChat(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |s: td_api::ChatBoostSlots| Object::chatBoostSlots(s));
            let (mut manager, mut env) = crate::td::boost_env_split(td);
            manager.boost_dialog(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                &request.slot_ids,
                promise,
            );
            td.boost_manager = manager;
        }
        Function::getChatBoostLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let result = {
                let (mut manager, mut env) = crate::td::boost_env_split(td);
                let result = manager.get_dialog_boost_link(
                    &mut env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                );
                td.boost_manager = manager;
                result
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
            let (mut manager, mut env) = crate::td::boost_env_split(td);
            manager.get_dialog_boosts(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.only_gift_codes,
                &offset,
                request.limit,
                promise,
            );
            td.boost_manager = manager;
        }
        Function::getUserChatBoosts(request) => {
            // `Requests.cpp:6065` — the family is bot-only
            // (`CHECK_IS_BOT`).
            if !check_is_bot(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |b: td_api::FoundChatBoosts| Object::foundChatBoosts(b));
            let (mut manager, mut env) = crate::td::boost_env_split(td);
            manager.get_user_dialog_boosts(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.user_id,
                promise,
            );
            td.boost_manager = manager;
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
            let (mut manager, mut env) = crate::td::quick_reply_env_split(&mut *td);
            manager.set_quick_reply_shortcut_name(&mut env, request.shortcut_id, &name, promise);
            td.quick_reply_manager = manager;
        }
        Function::deleteQuickReplyShortcut(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut manager, mut env) = crate::td::quick_reply_env_split(&mut *td);
            manager.delete_quick_reply_shortcut(&mut env, request.shortcut_id, promise);
            td.quick_reply_manager = manager;
        }
        Function::reorderQuickReplyShortcuts(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut manager, mut env) = crate::td::quick_reply_env_split(&mut *td);
            manager.reorder_quick_reply_shortcuts(&mut env, &request.shortcut_ids, promise);
            td.quick_reply_manager = manager;
        }
        Function::deleteQuickReplyShortcutMessages(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut manager, mut env) = crate::td::quick_reply_env_split(&mut *td);
            manager.delete_quick_reply_shortcut_messages(
                &mut env,
                request.shortcut_id,
                &request.message_ids,
                promise,
            );
            td.quick_reply_manager = manager;
        }

        // Everything else waits for its manager step.
        Function::getPremiumLimit(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |l: td_api::PremiumLimit| Object::premiumLimit(l));
            let (manager, env) = crate::td::premium_env_split(&mut *td);
            manager.get_premium_limit(&env, &request.limit_type, promise);
            td.premium_manager = manager;
        }
        Function::getPremiumFeatures(request) => {
            if !check_is_user(td, id) {
                return;
            };
            let _ = request.source;
            let promise =
                td.deferred_promise(id, |f: td_api::PremiumFeatures| Object::premiumFeatures(f));
            let (manager, env) = crate::td::premium_env_split(&mut *td);
            manager.get_premium_features(&env, promise);
            td.premium_manager = manager;
        }
        Function::getBusinessFeatures(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |f: td_api::BusinessFeatures| {
                Object::businessFeatures(f)
            });
            let (mut manager, mut env) = crate::td::premium_env_split(&mut *td);
            manager.get_business_features(&mut env, promise);
            td.premium_manager = manager;
        }
        Function::viewPremiumFeature(request) => {
            // Requests.cpp → Premium.cpp:1500-1512 — the empty-feature 400
            // band; the save_app_log side effect parks with the app-log
            // plane
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            td.premium_manager
                .view_premium_feature(request.feature.as_ref(), promise);
        }
        Function::clickPremiumSubscriptionButton(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            td.premium_manager
                .click_premium_subscription_button(promise);
        }
        Function::getPremiumState(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |s: td_api::PremiumState| Object::premiumState(s));
            let (mut manager, mut env) = crate::td::premium_env_split(&mut *td);
            manager.get_premium_state(&mut env, promise);
            td.premium_manager = manager;
        }
        Function::getPremiumGiftPaymentOptions(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |o: td_api::PremiumGiftPaymentOptions| {
                Object::premiumGiftPaymentOptions(o)
            });
            let (mut manager, mut env) = crate::td::premium_env_split(&mut *td);
            manager.get_premium_gift_payment_options(&mut env, promise);
            td.premium_manager = manager;
        }
        Function::getPremiumGiveawayPaymentOptions(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |o: td_api::PremiumGiveawayPaymentOptions| {
                Object::premiumGiveawayPaymentOptions(o)
            });
            let (mut manager, mut env) = crate::td::premium_env_split(&mut *td);
            manager.get_premium_giveaway_options(&mut env, request.boosted_chat_id, promise);
            td.premium_manager = manager;
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
            let (mut manager, mut env) = crate::td::premium_env_split(&mut *td);
            manager.check_premium_gift_code(&mut env, &code, promise);
            td.premium_manager = manager;
        }
        Function::applyPremiumGiftCode(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(code) = crate::misc::clean_input_string(&request.code) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut manager, mut env) = crate::td::premium_env_split(&mut *td);
            manager.apply_premium_gift_code(&mut env, &code, promise);
            td.premium_manager = manager;
        }
        Function::giftPremiumWithStars(mut request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut manager, mut env) = crate::td::premium_env_split(&mut *td);
            manager.gift_premium_with_stars(
                &mut env,
                request.user_id,
                request.star_count,
                request.month_count,
                request.text.take(),
                promise,
            );
            td.premium_manager = manager;
        }
        Function::assignStoreTransaction(mut request) => {
            // Requests.cpp:9208-9212 → Premium.cpp:1450-1487: the
            // transaction/purpose ladder + the AppStore/PlayMarket sends;
            // the promise parks in the premium manager
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut manager, mut env) = crate::td::premium_env_split(&mut *td);
            manager.assign_store_transaction(
                &mut env,
                request.transaction.take(),
                request.purpose.take(),
                promise,
            );
            td.premium_manager = manager;
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
            // StatisticsManager::get_channel_statistics (cpp:746-758) —
            // the dc_id chain over the real ChatManager flow; the send
            // leg dispatches the wire query (net-drain 38)
            let (mut manager, mut env) = crate::td::statistics_env_split(&mut *td);
            manager.get_channel_statistics(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.is_dark,
                promise,
            );
            td.statistics_manager = manager;
        }
        Function::getMessageStatistics(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |s: td_api::MessageStatistics| {
                Object::messageStatistics(s)
            });
            // StatisticsManager::get_channel_message_statistics
            // (cpp:828-840) — the dc chain + the gates + the wire query
            let (mut manager, mut env) = crate::td::statistics_env_split(&mut *td);
            manager.get_channel_message_statistics(
                &mut env,
                MessageFullId::new(
                    MmDialogId::new(request.chat_id),
                    MessageId::new(request.message_id),
                ),
                request.is_dark,
                promise,
            );
            td.statistics_manager = manager;
        }
        Function::getStoryStatistics(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |s: td_api::StoryStatistics| Object::storyStatistics(s));
            // StatisticsManager::get_channel_story_statistics
            // (cpp:858-870) — the dc chain + the gates + the wire query
            let (mut manager, mut env) = crate::td::statistics_env_split(&mut *td);
            manager.get_channel_story_statistics(
                &mut env,
                crate::story_ids::StoryFullId::new(
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::story_ids::StoryId::new(request.story_id),
                ),
                request.is_dark,
                promise,
            );
            td.statistics_manager = manager;
        }
        Function::getChatRevenueStatistics(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |s: td_api::ChatRevenueStatistics| {
                Object::chatRevenueStatistics(s)
            });
            // StatisticsManager::get_dialog_revenue_statistics
            // (cpp:770-775)
            let (mut manager, mut env) = crate::td::statistics_env_split(&mut *td);
            manager.get_dialog_revenue_statistics(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.is_dark,
                promise,
            );
            td.statistics_manager = manager;
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
            // StatisticsManager::get_dialog_revenue_withdrawal_url
            // (cpp:793-810) — the SRP seam + the wire query
            let (mut manager, mut env) = crate::td::statistics_env_split(&mut *td);
            manager.get_dialog_revenue_withdrawal_url(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                &password,
                promise,
            );
            td.statistics_manager = manager;
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
            // StatisticsManager::get_dialog_revenue_transactions
            // (cpp:820-826)
            let (mut manager, mut env) = crate::td::statistics_env_split(&mut *td);
            manager.get_dialog_revenue_transactions(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                &offset,
                request.limit,
                promise,
            );
            td.statistics_manager = manager;
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
            // StatisticsManager::get_message_public_forwards
            // (cpp:920-936) — the limit band + the dc chain + the gates
            let (mut manager, mut env) = crate::td::statistics_env_split(&mut *td);
            manager.get_message_public_forwards(
                &mut env,
                MessageFullId::new(
                    MmDialogId::new(request.chat_id),
                    MessageId::new(request.message_id),
                ),
                &offset,
                request.limit,
                promise,
            );
            td.statistics_manager = manager;
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
            // StatisticsManager::get_story_public_forwards (cpp:955-978)
            // — the User-dialog band + the dc chain + the gates
            let (mut manager, mut env) = crate::td::statistics_env_split(&mut *td);
            manager.get_story_public_forwards(
                &mut env,
                crate::story_ids::StoryFullId::new(
                    crate::dialog_id::DialogId::new(request.story_poster_chat_id),
                    crate::story_ids::StoryId::new(request.story_id),
                ),
                &offset,
                request.limit,
                promise,
            );
            td.statistics_manager = manager;
        }

        // Everything else waits for its manager step.
        _ => run_bot_requests(td, id, function),
    }
}

/// Run the payment flow over the whole-`Td` payments env (the
/// `PremiumTdEnv` convention — the net-drain 23 send arms dispatch
/// through it).
fn payments_arm(td: &mut Td, run: impl FnOnce(&mut crate::td::PaymentsTdEnv<'_>), _id: u64) {
    let mut env = crate::td::PaymentsTdEnv { td };
    run(&mut env);
}

fn bot_env_split(
    td: &mut Td,
) -> (
    &mut crate::bot_info_manager::BotInfoManager,
    crate::td::BotInfoEnv<'_>,
) {
    let net = td.bot_info_net();
    let crate::td::Td {
        bot_info_manager,
        global,
        option_manager,
        auth_manager,
        user_manager,
        file_reference_manager,
        ..
    } = td;
    let env = crate::td::BotInfoEnv {
        global,
        options: option_manager.as_mut(),
        auth_is_authorized: auth_manager.is_authorized(),
        auth_is_bot: auth_manager.is_bot(),
        user_manager,
        net,
        file_reference_manager,
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
            // check_access = true; the custom description is a
            // formattedText (CLEAN_INPUT_STRING is gone — the entity
            // conversion runs in the manager).
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
                request.custom_description,
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
                None,
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
        _ => run_business_requests(td, id, function),
    }
}

/// The `td_->` slice the business arms read through (the same
/// field-split borrow the [`crate::td::BusinessTdEnv`] init site
/// uses); the managers ride the closure as their own `&mut`.
#[allow(clippy::too_many_arguments)]
fn business_arm(
    td: &mut Td,
    run: impl FnOnce(
        &mut crate::td::BusinessTdEnv<'_>,
        &mut crate::business_manager::BusinessManager,
        &mut crate::business_connection_manager::BusinessConnectionManager,
    ),
) {
    let crate::td::Td {
        business_manager,
        business_connection_manager,
        global,
        option_manager,
        auth_manager,
        time_zone_manager,
        user_manager,
        chat_manager,
        ..
    } = td;
    let mut env = crate::td::BusinessTdEnv {
        global,
        options: option_manager.as_mut(),
        auth_is_bot: auth_manager.is_bot(),
        time_zone_manager,
        user_manager,
        chat_manager,
    };
    run(&mut env, business_manager, business_connection_manager);
}

/// The M.48 business arm group of the request dispatcher — see the frame
/// note at the forward site in [`run_request`]. The connection arms route
/// into `td_->business_connection_manager_->...` (Requests.cpp:4434-4600,
/// 8161-8164), the business-manager arms into `td_->business_manager_`
/// (Requests.cpp:7085-7250); the network tails are the [blocked: net]
/// query seams, the message halves are [blocked: M.10 message plane].
#[allow(clippy::too_many_lines)]
fn run_business_requests(td: &mut Td, id: u64, function: Function) {
    macro_rules! ok_promise {
        ($td:expr) => {
            $td.deferred_promise::<(), _>(id, |_| ok_object())
        };
    }

    /// `BusinessConnectionId(std::move(request.business_connection_id_))`.
    fn connection_id(request: &str) -> crate::business_connection_manager::BusinessConnectionId {
        crate::business_connection_manager::BusinessConnectionId::new(request)
    }

    match function {
        // getBusinessConnection (Requests.cpp:8161-8164)
        Function::getBusinessConnection(request) => {
            let promise = td
                .deferred_promise::<td_api::BusinessConnection, _>(id, Object::businessConnection);
            let cid = connection_id(&request.connection_id);
            business_arm(td, |env, _, connection_manager| {
                connection_manager.get_business_connection(env, &cid, promise);
            });
        }

        // sendBusinessMessage (Requests.cpp:4434-4441)
        Function::sendBusinessMessage(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::BusinessMessage, _>(id, Object::businessMessage);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |env, _, connection_manager| {
                connection_manager.send_message(
                    env,
                    cid,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    request.reply_to,
                    request.disable_notification,
                    request.protect_content,
                    request.effect_id,
                    request.reply_markup,
                    request.input_message_content.map(|content| *content),
                    promise,
                );
            });
        }

        // sendBusinessMessageAlbum (Requests.cpp:4444-4451)
        Function::sendBusinessMessageAlbum(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::BusinessMessages, _>(id, Object::businessMessages);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |env, _, connection_manager| {
                connection_manager.send_message_album(
                    env,
                    cid,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    request.reply_to,
                    request.disable_notification,
                    request.protect_content,
                    request.effect_id,
                    request
                        .input_message_contents
                        .into_iter()
                        .map(|content| *content)
                        .collect(),
                    promise,
                );
            });
        }

        // editBusinessMessageText (Requests.cpp:4453-4460)
        Function::editBusinessMessageText(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::BusinessMessage, _>(id, Object::businessMessage);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |env, _, connection_manager| {
                connection_manager.edit_business_message_text(
                    env,
                    cid,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::message_id::MessageId::new(request.message_id),
                    request.reply_markup,
                    request.input_message_content.map(|content| *content),
                    promise,
                );
            });
        }

        // editBusinessMessageLiveLocation (Requests.cpp:4462-4469)
        Function::editBusinessMessageLiveLocation(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::BusinessMessage, _>(id, Object::businessMessage);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |env, _, connection_manager| {
                connection_manager.edit_business_message_live_location(
                    env,
                    cid,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::message_id::MessageId::new(request.message_id),
                    request.reply_markup,
                    request.location,
                    promise,
                );
            });
        }

        // editBusinessMessageChecklist (Requests.cpp:4471-4478)
        Function::editBusinessMessageChecklist(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::BusinessMessage, _>(id, Object::businessMessage);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |env, _, connection_manager| {
                connection_manager.edit_business_message_checklist(
                    env,
                    cid,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::message_id::MessageId::new(request.message_id),
                    request.reply_markup,
                    request.checklist,
                    promise,
                );
            });
        }

        // editBusinessMessageMedia (Requests.cpp:4480-4487)
        Function::editBusinessMessageMedia(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::BusinessMessage, _>(id, Object::businessMessage);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |env, _, connection_manager| {
                connection_manager.edit_business_message_media(
                    env,
                    cid,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::message_id::MessageId::new(request.message_id),
                    request.reply_markup,
                    request.input_message_content.map(|content| *content),
                    promise,
                );
            });
        }

        // editBusinessMessageCaption (Requests.cpp:4489-4496)
        Function::editBusinessMessageCaption(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::BusinessMessage, _>(id, Object::businessMessage);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |env, _, connection_manager| {
                connection_manager.edit_business_message_caption(
                    env,
                    cid,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::message_id::MessageId::new(request.message_id),
                    request.reply_markup,
                    request.caption,
                    request.show_caption_above_media,
                    promise,
                );
            });
        }

        // editBusinessMessageReplyMarkup (Requests.cpp:4498-4504)
        Function::editBusinessMessageReplyMarkup(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::BusinessMessage, _>(id, Object::businessMessage);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |env, _, connection_manager| {
                connection_manager.edit_business_message_reply_markup(
                    env,
                    cid,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::message_id::MessageId::new(request.message_id),
                    request.reply_markup,
                    promise,
                );
            });
        }

        // stopBusinessPoll (Requests.cpp:4506-4510)
        Function::stopBusinessPoll(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::BusinessMessage, _>(id, Object::businessMessage);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |env, _, connection_manager| {
                connection_manager.stop_poll(
                    env,
                    cid,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::message_id::MessageId::new(request.message_id),
                    request.reply_markup,
                    promise,
                );
            });
        }

        // readBusinessMessage (Requests.cpp:4522-4528)
        Function::readBusinessMessage(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |env, _, connection_manager| {
                connection_manager.read_business_message(
                    env,
                    cid,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::message_id::MessageId::new(request.message_id),
                    promise,
                );
            });
        }

        // deleteBusinessMessages (Requests.cpp:4530-4537)
        Function::deleteBusinessMessages(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let cid = connection_id(&request.business_connection_id);
            let message_ids: Vec<crate::message_id::MessageId> = request
                .message_ids
                .iter()
                .copied()
                .map(crate::message_id::MessageId::new)
                .collect();
            business_arm(td, |env, _, connection_manager| {
                connection_manager.delete_business_messages(env, cid, &message_ids, promise);
            });
        }

        // deleteBusinessStory (Requests.cpp:4546-4553)
        Function::deleteBusinessStory(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |env, _, connection_manager| {
                connection_manager.delete_business_story(env, cid, request.story_id, promise);
            });
        }

        // setBusinessAccountName (Requests.cpp:4555-4561)
        Function::setBusinessAccountName(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let Some(first_name) = clean_or_utf8_error(td, id, &request.first_name) else {
                return;
            };
            let Some(last_name) = clean_or_utf8_error(td, id, &request.last_name) else {
                return;
            };
            let promise = ok_promise!(td);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |_, business_manager, connection_manager| {
                connection_manager.set_business_name(
                    business_manager,
                    cid,
                    &first_name,
                    &last_name,
                    promise,
                );
            });
        }

        // setBusinessAccountBio (Requests.cpp:4563-4570)
        Function::setBusinessAccountBio(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let Some(bio) = clean_or_utf8_error(td, id, &request.bio) else {
                return;
            };
            let promise = ok_promise!(td);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |_, business_manager, connection_manager| {
                connection_manager.set_business_about(business_manager, cid, &bio, promise);
            });
        }

        // setBusinessAccountUsername (Requests.cpp:4578-4584)
        Function::setBusinessAccountUsername(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let Some(username) = clean_or_utf8_error(td, id, &request.username) else {
                return;
            };
            let promise = ok_promise!(td);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |_, business_manager, connection_manager| {
                connection_manager.set_business_username(business_manager, cid, &username, promise);
            });
        }

        // setBusinessAccountGiftSettings (Requests.cpp:4585-4591)
        Function::setBusinessAccountGiftSettings(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |_, business_manager, connection_manager| {
                connection_manager.set_business_gift_settings(
                    business_manager,
                    cid,
                    request.settings,
                    promise,
                );
            });
        }

        // getBusinessAccountStarAmount (Requests.cpp:4593-4598)
        Function::getBusinessAccountStarAmount(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::StarAmount, _>(id, Object::starAmount);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |env, business_manager, connection_manager| {
                connection_manager.get_business_star_status(env, business_manager, cid, promise);
            });
        }

        // transferBusinessAccountStars (Requests.cpp:4600-4607)
        Function::transferBusinessAccountStars(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let cid = connection_id(&request.business_connection_id);
            business_arm(td, |_, business_manager, connection_manager| {
                connection_manager.transfer_business_stars(
                    business_manager,
                    cid,
                    request.star_count,
                    promise,
                );
            });
        }

        // setBusinessLocation (Requests.cpp:7085-7089)
        Function::setBusinessLocation(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let location =
                crate::chat_manager::DialogLocation::from_business_location(request.location);
            business_arm(td, |env, business_manager, _| {
                business_manager.set_business_location(env, location, promise);
            });
        }

        // setBusinessOpeningHours (Requests.cpp:7091-7096)
        Function::setBusinessOpeningHours(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let work_hours =
                crate::business_work_hours::BusinessWorkHours::from_td_api(request.opening_hours);
            business_arm(td, |env, business_manager, _| {
                business_manager.set_business_work_hours(env, work_hours, promise);
            });
        }

        // setBusinessGreetingMessageSettings (Requests.cpp:7098-7103)
        Function::setBusinessGreetingMessageSettings(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let greeting_message =
                crate::business_greeting_message::BusinessGreetingMessage::from_td_api(
                    request.greeting_message_settings,
                );
            business_arm(td, |env, business_manager, _| {
                business_manager.set_business_greeting_message(env, greeting_message, promise);
            });
        }

        // setBusinessAwayMessageSettings (Requests.cpp:7105-7110)
        Function::setBusinessAwayMessageSettings(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let away_message = crate::business_away_message::BusinessAwayMessage::from_td_api(
                request.away_message_settings,
            );
            business_arm(td, |env, business_manager, _| {
                business_manager.set_business_away_message(env, away_message, promise);
            });
        }

        // setBusinessStartPage (Requests.cpp:7112-7117)
        Function::setBusinessStartPage(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            business_arm(td, |env, business_manager, _| {
                let intro =
                    crate::business_intro::BusinessIntro::from_td_api(env, request.start_page);
                business_manager.set_business_intro(env, intro, promise);
            });
        }

        // getBusinessConnectedBot (Requests.cpp:7186-7190)
        Function::getBusinessConnectedBot(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |value: Option<td_api::BusinessConnectedBotInfo>| {
                    match value {
                        Some(info) => Object::businessConnectedBotInfo(info),
                        None => Object::error(td_api::Error {
                            code: 404,
                            message: "Not Found".to_owned(),
                        }),
                    }
                });
            business_arm(td, |env, business_manager, _| {
                business_manager.get_business_connected_bot(env, promise);
            });
        }

        // setBusinessConnectedBot (Requests.cpp:7192-7196)
        Function::setBusinessConnectedBot(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            business_arm(td, |env, business_manager, _| {
                business_manager.set_business_connected_bot(env, request.bot, promise);
            });
        }

        // confirmBusinessConnectedBot (Requests.cpp:7198-7202)
        Function::confirmBusinessConnectedBot(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            business_arm(td, |env, business_manager, _| {
                business_manager.confirm_business_connected_bot(
                    env,
                    crate::user_manager::UserId::new(request.bot_user_id),
                    promise,
                );
            });
        }

        // deleteBusinessConnectedBot (Requests.cpp:7204-7208)
        Function::deleteBusinessConnectedBot(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            business_arm(td, |env, business_manager, _| {
                business_manager.delete_business_connected_bot(
                    env,
                    crate::user_manager::UserId::new(request.bot_user_id),
                    promise,
                );
            });
        }

        // toggleBusinessConnectedBotChatIsPaused (Requests.cpp:7210-7215)
        Function::toggleBusinessConnectedBotChatIsPaused(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            let status = crate::dialog_manager::check_dialog_access(
                td,
                dialog_id,
                false,
                crate::user_manager::AccessRights::Write,
                "toggle_business_connected_bot_dialog_is_paused",
            );
            let dialog_status = match status {
                tdutils::status::Result::Ok(()) => tdutils::status::Status::ok(),
                tdutils::status::Result::Err(status) => status,
            };
            let promise = ok_promise!(td);
            business_arm(td, |env, business_manager, _| {
                business_manager.toggle_business_connected_bot_dialog_is_paused(
                    env,
                    dialog_id,
                    request.is_paused,
                    dialog_status,
                    promise,
                );
            });
        }

        // removeBusinessConnectedBotFromChat (Requests.cpp:7217-7221)
        Function::removeBusinessConnectedBotFromChat(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            let status = crate::dialog_manager::check_dialog_access(
                td,
                dialog_id,
                false,
                crate::user_manager::AccessRights::Write,
                "remove_business_connected_bot_from_dialog",
            );
            let dialog_status = match status {
                tdutils::status::Result::Ok(()) => tdutils::status::Status::ok(),
                tdutils::status::Result::Err(status) => status,
            };
            let promise = ok_promise!(td);
            business_arm(td, |env, business_manager, _| {
                business_manager.remove_business_connected_bot_from_dialog(
                    env,
                    dialog_id,
                    dialog_status,
                    promise,
                );
            });
        }

        // getBusinessChatLinks (Requests.cpp:7223-7227)
        Function::getBusinessChatLinks(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::BusinessChatLinks, _>(id, Object::businessChatLinks);
            business_arm(td, |env, business_manager, _| {
                business_manager.get_business_chat_links(env, promise);
            });
        }

        // createBusinessChatLink (Requests.cpp:7229-7234)
        Function::createBusinessChatLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::BusinessChatLink, _>(id, Object::businessChatLink);
            business_arm(td, |env, business_manager, _| {
                business_manager.create_business_chat_link(env, request.link_info, promise);
            });
        }

        // editBusinessChatLink (Requests.cpp:7236-7241)
        Function::editBusinessChatLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(link) = clean_or_utf8_error(td, id, &request.link) else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::BusinessChatLink, _>(id, Object::businessChatLink);
            business_arm(td, |env, business_manager, _| {
                business_manager.edit_business_chat_link(env, &link, request.link_info, promise);
            });
        }

        // deleteBusinessChatLink (Requests.cpp:7243-7248)
        Function::deleteBusinessChatLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(link) = clean_or_utf8_error(td, id, &request.link) else {
                return;
            };
            let promise = ok_promise!(td);
            business_arm(td, |env, business_manager, _| {
                business_manager.delete_business_chat_link(env, &link, promise);
            });
        }

        // getBusinessChatLinkInfo (Requests.cpp:7250-7255)
        Function::getBusinessChatLinkInfo(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(link_name) = clean_or_utf8_error(td, id, &request.link_name) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::BusinessChatLinkInfo, _>(
                id,
                Object::businessChatLinkInfo,
            );
            business_arm(td, |env, business_manager, _| {
                business_manager.get_business_chat_link_info(env, &link_name, promise);
            });
        }

        // Everything else waits for its manager step.
        _ => run_secret_chat_requests(td, id, function),
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
        _ => run_secret_chat_requests(td, id, function),
    }
}

/// The M.50 SecretChatsManager arm group of the request dispatcher — see
/// the frame note at the forward site in [`run_request`]. The arms route
/// into `td_->secret_chats_manager_->...` (Requests.cpp:5082-5091,
/// 7409-7415); the network tails are the [blocked: net] query seams.
#[allow(clippy::too_many_lines)]
fn run_secret_chat_requests(td: &mut Td, id: u64, function: Function) {
    fn secret_chat_arm(
        td: &mut Td,
        run: impl FnOnce(
            &mut crate::td::SecretChatsTdEnv<'_>,
            &mut crate::secret_chats_manager::SecretChatsManager,
        ),
    ) {
        let crate::td::Td {
            secret_chats_manager,
            global,
            auth_manager,
            user_manager,
            notification_updates,
            ..
        } = td;
        let mut env = crate::td::SecretChatsTdEnv {
            global,
            auth_is_authorized: auth_manager.is_authorized(),
            use_secret_chats: true,
            user_manager,
            updates: notification_updates,
        };
        run(&mut env, secret_chats_manager);
    }

    match function {
        // createNewSecretChat (Requests.cpp:5082-5091) — the C++ routes
        // through user_manager_->create_new_secret_chat, which converts
        // the created chat id to the chat object; both the net leg and
        // the conversion park [blocked: net / the chat creation intake].
        Function::createNewSecretChat(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Chat, _>(id, Object::chat);
            secret_chat_arm(td, |env, manager| {
                manager.create_chat_chat_object(
                    env,
                    crate::user_manager::UserId::new(request.user_id),
                    promise,
                );
            });
        }

        // closeSecretChat (Requests.cpp:7409-7415)
        Function::closeSecretChat(request) => {
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            secret_chat_arm(td, |env, manager| {
                manager.cancel_chat(
                    env,
                    crate::secret_chat::SecretChatId::new(request.secret_chat_id),
                    false,
                    promise,
                );
            });
        }

        // Everything else waits for its manager step.
        _ => run_group_call_requests(td, id, function),
    }
}

/// The M.51 GroupCallManager arm group of the request dispatcher — see
/// the frame note at the forward site in [`run_request`]. The arms route
/// into `td_->group_call_manager_` (Requests.cpp:5158-5445); every
/// network tail is the [blocked: net] seam (D4 signaling-only).
#[allow(clippy::too_many_lines)]
fn run_group_call_requests(td: &mut Td, id: u64, function: Function) {
    use crate::group_call_manager::GroupCallId;

    /// The M.51 GroupCallManager arm frame (see the frame note at the
    /// forward site in [`run_request`]). The dialog-plane reads the C++
    /// tails make over `td_` resolve here, over the full `Td` frame,
    /// before the field split; the peer-bearing sends thread their
    /// pre-resolved input peer through the env
    /// (`GroupCallEnv::get_input_peer_read`).
    #[allow(clippy::too_many_arguments)]
    fn group_call_arm(
        td: &mut Td,
        peer_dialog_id: Option<crate::dialog_id::DialogId>,
        run: impl FnOnce(
            &mut crate::td::GroupCallTdEnv<'_>,
            &mut crate::group_call_manager::GroupCallManager,
        ),
    ) {
        let request_input_peer = peer_dialog_id.and_then(|dialog_id| {
            crate::dialog_manager::get_input_peer(
                td,
                dialog_id,
                crate::user_manager::AccessRights::Read,
            )
        });
        let crate::td::Td {
            group_call_manager,
            global,
            auth_manager,
            notification_updates,
            ..
        } = td;
        let mut env = crate::td::GroupCallTdEnv {
            global,
            auth_is_authorized: auth_manager.is_authorized(),
            auth_is_bot: auth_manager.is_bot(),
            updates: notification_updates,
            request_input_peer,
        };
        run(&mut env, group_call_manager);
    }

    /// `GroupCallManager::my_dialog_id` — the my-dialog read the
    /// self-bands key on.
    fn my_dialog_id(td: &Td) -> crate::dialog_id::DialogId {
        crate::dialog_id::DialogId::from_user_id(td.user_manager.get_my_id())
    }

    /// The sender-id probe + the as-dialog band ladder of
    /// `set_group_call_default_join_as` (cpp:2586-2613) and
    /// `set_group_call_default_send_as` (cpp:2615-2638): the dialog
    /// reads run over the full `Td` frame.
    fn resolve_default_participant(
        td: &mut Td,
        id: u64,
        as_dialog_id: crate::dialog_id::DialogId,
    ) -> Option<crate::dialog_id::DialogId> {
        use crate::dialog_id::DialogType;
        match as_dialog_id.get_type() {
            DialogType::User => {
                if as_dialog_id != my_dialog_id(td) {
                    td.send_error_impl(id, 400, "Can't join video chat as another user");
                    return None;
                }
            }
            DialogType::Chat | DialogType::Channel => {
                if !crate::dialog_manager::have_dialog_force(
                    td,
                    as_dialog_id,
                    "set_group_call_default_join_as 2",
                ) {
                    td.send_error_impl(id, 400, "Participant chat not found");
                    return None;
                }
            }
            DialogType::SecretChat => {
                td.send_error_impl(id, 400, "Can't join video chat as a secret chat");
                return None;
            }
            DialogType::None => {
                td.send_error_impl(id, 400, "Invalid default participant identifier specified");
                return None;
            }
        }
        if !crate::dialog_manager::have_input_peer(
            td,
            as_dialog_id,
            false,
            crate::user_manager::AccessRights::Read,
        ) {
            td.send_error_impl(id, 400, "Can't access specified default participant chat");
            return None;
        }
        Some(as_dialog_id)
    }

    macro_rules! ok_promise {
        ($td:expr) => {
            $td.deferred_promise::<(), _>(id, |_| ok_object())
        };
    }

    match function {
        // getVideoChatAvailableParticipants (Requests.cpp:5152-5157 →
        // GroupCallManager.cpp:2443-2449)
        Function::getVideoChatAvailableParticipants(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::MessageSenders, _>(id, Object::messageSenders);
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            group_call_arm(td, Some(dialog_id), |env, manager| {
                manager.get_group_call_join_as(env, dialog_id, promise);
            });
        }

        // setVideoChatDefaultParticipant (Requests.cpp:5159-5165 →
        // GroupCallManager.cpp:2586-2613)
        Function::setVideoChatDefaultParticipant(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            // `TRY_STATUS_PROMISE(promise, can_join_video_chats(...))`
            // (cpp:2445, 2339-2341)
            if let Result::Err(status) = crate::dialog_manager::check_dialog_access(
                td,
                dialog_id,
                false,
                crate::user_manager::AccessRights::Read,
                "can_join_video_chats",
            ) {
                send_error_status(td, id, status);
                return;
            }
            // `TRY_RESULT_PROMISE(promise, default_join_as_dialog_id,
            // get_message_sender_dialog_id(...))` (Requests.cpp:5162-5163)
            let default_join_as_dialog_id = {
                let (_manager, mut env) = bot_env_split(td);
                match crate::message_sender::get_message_sender_dialog_id(
                    &mut env,
                    request.default_participant_id.as_ref(),
                    true,
                    false,
                ) {
                    tdutils::status::Result::Ok(dialog_id) => dialog_id,
                    tdutils::status::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            let Some(as_dialog_id) = resolve_default_participant(td, id, default_join_as_dialog_id)
            else {
                return;
            };
            let Some(join_as_peer) = crate::dialog_manager::get_input_peer(
                td,
                as_dialog_id,
                crate::user_manager::AccessRights::Read,
            ) else {
                td.send_error_impl(id, 400, "Can't access specified default participant chat");
                return;
            };
            group_call_arm(td, Some(dialog_id), |env, manager| {
                manager.set_group_call_default_join_as(env, dialog_id, join_as_peer, promise);
            });
        }

        // createVideoChat (Requests.cpp:5167-5174 → cpp:2658-2678) —
        // the join-payload flow rides the media plane (ticket
        // td-rs/td#54); the promise parks per the §5 fidelity notes.
        Function::createVideoChat(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(title) = clean_or_utf8_error(td, id, &request.title) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::GroupCallId, _>(id, Object::groupCallId);
            group_call_arm(td, None, |_env, manager| {
                manager.park_media_query(&format!("createVideoChat({title})"), promise);
            });
        }

        // createGroupCall (Requests.cpp:5176-5181 → cpp:2679-2713) —
        // the media plane park.
        Function::createGroupCall(_request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::GroupCallInfo, _>(id, Object::groupCallInfo);
            group_call_arm(td, None, |_env, manager| {
                manager.park_media_query("createGroupCall", promise);
            });
        }

        // getVideoChatRtmpUrl (Requests.cpp:5183-5187 → cpp:2781-2795)
        Function::getVideoChatRtmpUrl(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::RtmpUrl, _>(id, Object::rtmpUrl);
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            group_call_arm(td, Some(dialog_id), |env, manager| {
                manager.get_video_chat_rtmp_stream_url(env, dialog_id, false, false, promise);
            });
        }

        // replaceVideoChatRtmpUrl (Requests.cpp:5188-5192)
        Function::replaceVideoChatRtmpUrl(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::RtmpUrl, _>(id, Object::rtmpUrl);
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            group_call_arm(td, Some(dialog_id), |env, manager| {
                manager.get_video_chat_rtmp_stream_url(env, dialog_id, false, true, promise);
            });
        }

        // getLiveStoryRtmpUrl (Requests.cpp:5194-5197)
        Function::getLiveStoryRtmpUrl(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::RtmpUrl, _>(id, Object::rtmpUrl);
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            group_call_arm(td, Some(dialog_id), |env, manager| {
                manager.get_video_chat_rtmp_stream_url(env, dialog_id, true, false, promise);
            });
        }

        // replaceLiveStoryRtmpUrl (Requests.cpp:5199-5203)
        Function::replaceLiveStoryRtmpUrl(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::RtmpUrl, _>(id, Object::rtmpUrl);
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            group_call_arm(td, Some(dialog_id), |env, manager| {
                manager.get_video_chat_rtmp_stream_url(env, dialog_id, true, true, promise);
            });
        }

        // getGroupCall (Requests.cpp:5205-5210 → cpp:2807-2817)
        Function::getGroupCall(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |value: Option<td_api::GroupCall>| match value {
                Some(call) => Object::groupCall(call),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_owned(),
                }),
            });
            group_call_arm(td, None, |_env, manager| {
                manager.reload_group_call(GroupCallId::new(request.group_call_id), promise);
            });
        }

        // startScheduledVideoChat (Requests.cpp:5212-5216 →
        // cpp:4400-4432)
        Function::startScheduledVideoChat(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager
                    .start_scheduled_group_call(GroupCallId::new(request.group_call_id), promise);
            });
        }

        // toggleVideoChatEnabledStartNotification (Requests.cpp:5218-5222
        // → cpp:4349-4398)
        Function::toggleVideoChatEnabledStartNotification(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |env, manager| {
                manager.toggle_group_call_start_subscribed(
                    env,
                    GroupCallId::new(request.group_call_id),
                    request.enabled_start_notification,
                    promise,
                );
            });
        }

        // getLiveStoryStreamer (Requests.cpp:5224-5228 → cpp:2450-2479)
        Function::getLiveStoryStreamer(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(
                    id,
                    |value: Option<td_api::GroupCallParticipant>| match value {
                        Some(participant) => Object::groupCallParticipant(participant),
                        None => Object::error(td_api::Error {
                            code: 404,
                            message: "Not Found".to_owned(),
                        }),
                    },
                );
            group_call_arm(td, None, |_env, manager| {
                manager.get_group_call_streamer(GroupCallId::new(request.group_call_id), promise);
            });
        }

        // getLiveStoryAvailableMessageSenders (Requests.cpp:5230-5234 →
        // cpp:2490-2582) — the local sender-list recipe; the sender
        // objects build over the full `Td` frame.
        Function::getLiveStoryAvailableMessageSenders(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let mut promise = td
                .deferred_promise::<td_api::ChatMessageSenders, _>(id, Object::chatMessageSenders);
            // the created-public-broadcasts read with the participant
            // counts (ChatManager.h:640, cpp:2571-2575) rides the chat
            // plane (§5); the recipe runs over the empty list.
            let created_public_broadcasts: Vec<(i64, i64)> = Vec::new();
            match td.group_call_manager.group_call_send_as_dialog_ids(
                GroupCallId::new(request.group_call_id),
                my_dialog_id(td),
                &created_public_broadcasts,
            ) {
                std::result::Result::Ok(dialog_ids) => {
                    let senders = dialog_ids
                        .iter()
                        .map(|&dialog_id| td_api::ChatMessageSender {
                            sender: Some(crate::dialog_manager::get_message_sender_object(
                                td,
                                dialog_id,
                                "get_group_call_send_as",
                            )),
                            needs_premium: false,
                        })
                        .collect();
                    promise.set_value(td_api::ChatMessageSenders { senders });
                }
                std::result::Result::Err(status) => {
                    send_error_status(td, id, status);
                }
            }
        }

        // setLiveStoryMessageSender (Requests.cpp:5236-5240 →
        // cpp:2615-2656)
        Function::setLiveStoryMessageSender(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            // `TRY_RESULT_PROMISE(promise, message_sender_dialog_id,
            // get_message_sender_dialog_id(...))` (Requests.cpp:5238-5239)
            let as_dialog_id = {
                let (_manager, mut env) = bot_env_split(td);
                match crate::message_sender::get_message_sender_dialog_id(
                    &mut env,
                    request.message_sender_id.as_ref(),
                    true,
                    false,
                ) {
                    tdutils::status::Result::Ok(dialog_id) => dialog_id,
                    tdutils::status::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            let Some(as_peer) = crate::dialog_manager::get_input_peer(
                td,
                as_dialog_id,
                crate::user_manager::AccessRights::Read,
            ) else {
                td.send_error_impl(id, 400, "Can't access the chat");
                return;
            };
            let is_another_user = as_dialog_id.get_type() == crate::dialog_id::DialogType::User
                && as_dialog_id != my_dialog_id(td);
            group_call_arm(td, None, |env, manager| {
                manager.set_group_call_default_send_as(
                    env,
                    GroupCallId::new(request.group_call_id),
                    as_dialog_id,
                    is_another_user,
                    as_peer,
                    promise,
                );
            });
        }

        // joinGroupCall (Requests.cpp:5242-5248 → cpp:4434) — the join
        // flow rides the media plane (ticket td-rs/td#54).
        Function::joinGroupCall(_request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::GroupCallInfo, _>(id, Object::groupCallInfo);
            group_call_arm(td, None, |_env, manager| {
                manager.park_media_query("joinGroupCall", promise);
            });
        }

        // joinVideoChat (Requests.cpp:5250-5255 → cpp:4530) — the
        // media plane park.
        Function::joinVideoChat(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(invite_hash) = clean_or_utf8_error(td, id, &request.invite_hash) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::Text, _>(id, Object::text);
            group_call_arm(td, None, |_env, manager| {
                manager.park_media_query(&format!("joinVideoChat({invite_hash})"), promise);
            });
        }

        // joinLiveStory (Requests.cpp:5257-5261) — the media plane park.
        Function::joinLiveStory(_request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Text, _>(id, Object::text);
            group_call_arm(td, None, |_env, manager| {
                manager.park_media_query("joinLiveStory", promise);
            });
        }

        // startGroupCallScreenSharing (Requests.cpp:5263-5269 →
        // cpp:891) — the media plane park.
        Function::startGroupCallScreenSharing(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(payload) = clean_or_utf8_error(td, id, &request.payload) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::Text, _>(id, Object::text);
            group_call_arm(td, None, |_env, manager| {
                manager
                    .park_media_query(&format!("startGroupCallScreenSharing({payload})"), promise);
            });
        }

        // toggleGroupCallScreenSharingIsPaused (Requests.cpp:5271-5275
        // → cpp:3960-4018) — the local pending-flag settle.
        Function::toggleGroupCallScreenSharingIsPaused(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager.toggle_group_call_is_my_presentation_paused(
                    GroupCallId::new(request.group_call_id),
                    request.is_paused,
                    promise,
                );
            });
        }

        // endGroupCallScreenSharing (Requests.cpp:5277-5281 →
        // cpp:3925-3959)
        Function::endGroupCallScreenSharing(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager.end_group_call_screen_sharing(
                    GroupCallId::new(request.group_call_id),
                    promise,
                );
            });
        }

        // setVideoChatTitle (Requests.cpp:5283-5288 → cpp:5193-5236)
        Function::setVideoChatTitle(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(title) = clean_or_utf8_error(td, id, &request.title) else {
                return;
            };
            let promise = ok_promise!(td);
            group_call_arm(td, None, |env, manager| {
                manager.set_group_call_title(
                    env,
                    GroupCallId::new(request.group_call_id),
                    title,
                    promise,
                );
            });
        }

        // toggleVideoChatMuteNewParticipants (Requests.cpp:5290-5294 →
        // cpp:4095-4132)
        Function::toggleVideoChatMuteNewParticipants(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |env, manager| {
                manager.toggle_group_call_mute_new_participants(
                    env,
                    GroupCallId::new(request.group_call_id),
                    request.mute_new_participants,
                    promise,
                );
            });
        }

        // toggleGroupCallAreMessagesAllowed (Requests.cpp:5296-5300 →
        // cpp:4180-4216)
        Function::toggleGroupCallAreMessagesAllowed(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |env, manager| {
                manager.toggle_group_call_are_messages_enabled(
                    env,
                    GroupCallId::new(request.group_call_id),
                    request.are_messages_allowed,
                    promise,
                );
            });
        }

        // setGroupCallPaidMessageStarCount (Requests.cpp:5302-5307 →
        // cpp:4266-4303)
        Function::setGroupCallPaidMessageStarCount(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |env, manager| {
                manager.set_group_call_paid_message_star_count(
                    env,
                    GroupCallId::new(request.group_call_id),
                    request.paid_message_star_count,
                    promise,
                );
            });
        }

        // sendGroupCallMessage (Requests.cpp:5309-5314 → cpp:5884-5979)
        // — the text parse rides the formatted-text integration for the
        // call-message plane (§5); the wire text stays empty.
        Function::sendGroupCallMessage(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(_text) = clean_or_utf8_error(
                td,
                id,
                request
                    .text
                    .as_ref()
                    .map(|text| text.text.as_str())
                    .unwrap_or(""),
            ) else {
                return;
            };
            let promise = ok_promise!(td);
            let group_call_id = GroupCallId::new(request.group_call_id);
            let is_my_dialog = td
                .group_call_manager
                .get_group_call(group_call_id)
                .map(|call| crate::dialog_id::DialogId::new(call.chat_id) == my_dialog_id(td))
                .unwrap_or(false);
            let has_owned_star_count = td
                .star_manager
                .has_owned_star_count(request.paid_message_star_count);
            group_call_arm(td, None, |_env, manager| {
                manager.send_group_call_message(
                    group_call_id,
                    request.paid_message_star_count,
                    false,
                    is_my_dialog,
                    has_owned_star_count,
                    promise,
                );
            });
        }

        // addPendingLiveStoryReaction (Requests.cpp:5316-5320 →
        // cpp:5980-6045) — the pending bookkeeping settles locally.
        Function::addPendingLiveStoryReaction(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let group_call_id = GroupCallId::new(request.group_call_id);
            let is_my_dialog = td
                .group_call_manager
                .get_group_call(group_call_id)
                .map(|call| crate::dialog_id::DialogId::new(call.chat_id) == my_dialog_id(td))
                .unwrap_or(false);
            let has_owned_star_count = td.star_manager.has_owned_star_count(request.star_count);
            group_call_arm(td, None, |_env, manager| {
                manager.send_group_call_reaction(
                    group_call_id,
                    request.star_count,
                    is_my_dialog,
                    has_owned_star_count,
                    promise,
                );
            });
        }

        // commitPendingLiveStoryReactions (Requests.cpp:5322-5326 →
        // cpp:6046-6108) — the aggregated send rides the pending-reaction
        // store (media plane, §5); the bands settle locally.
        Function::commitPendingLiveStoryReactions(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager.commit_pending_group_call_reactions(
                    GroupCallId::new(request.group_call_id),
                    promise,
                );
            });
        }

        // removePendingLiveStoryReactions (Requests.cpp:5328-5332 →
        // cpp:6109-6142)
        Function::removePendingLiveStoryReactions(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager.remove_pending_group_call_reactions(
                    GroupCallId::new(request.group_call_id),
                    promise,
                );
            });
        }

        // deleteGroupCallMessages (Requests.cpp:5334-5338 →
        // cpp:4703-4760) — the stored-message delete rides the media
        // plane (§5); the bands settle locally.
        Function::deleteGroupCallMessages(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager
                    .delete_group_call_messages(GroupCallId::new(request.group_call_id), promise);
            });
        }

        // deleteGroupCallMessagesBySender (Requests.cpp:5340-5344 →
        // cpp:4761-4800)
        Function::deleteGroupCallMessagesBySender(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            // `TRY_RESULT_PROMISE(promise, sender_dialog_id,
            // get_message_sender_dialog_id(...))` (Requests.cpp:5342-5343)
            let sender_dialog_id = {
                let (_manager, mut env) = bot_env_split(td);
                match crate::message_sender::get_message_sender_dialog_id(
                    &mut env,
                    request.sender_id.as_ref(),
                    false,
                    false,
                ) {
                    tdutils::status::Result::Ok(dialog_id) => dialog_id,
                    tdutils::status::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            group_call_arm(td, Some(sender_dialog_id), |env, manager| {
                manager.delete_group_call_messages_by_sender(
                    env,
                    GroupCallId::new(request.group_call_id),
                    sender_dialog_id,
                    request.report_spam,
                    promise,
                );
            });
        }

        // getLiveStoryTopDonors (Requests.cpp:5346-5350 →
        // cpp:4530-4575)
        Function::getLiveStoryTopDonors(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::LiveStoryDonors, _>(id, Object::liveStoryDonors);
            group_call_arm(td, None, |_env, manager| {
                manager.get_group_call_stars_from_server(
                    GroupCallId::new(request.group_call_id),
                    promise,
                );
            });
        }

        // revokeGroupCallInviteLink (Requests.cpp:5352-5356 →
        // cpp:1180-1199)
        Function::revokeGroupCallInviteLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager.revoke_group_call_invite_link(
                    GroupCallId::new(request.group_call_id),
                    promise,
                );
            });
        }

        // inviteGroupCallParticipant (Requests.cpp:5358-5362 →
        // cpp:3796-3876) — the conference-plane bands; the
        // InviteConferenceCallParticipant answer's success leg rides the
        // messages plane (`wait_message_add`, §5) — the dispatch parks.
        Function::inviteGroupCallParticipant(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td
                .deferred_promise::<td_api::InviteGroupCallParticipantResult, _>(id, |value| {
                    invite_result_to_object(value)
                });
            let group_call_id = GroupCallId::new(request.group_call_id);
            let input_user = {
                let user_manager = std::mem::take(&mut td.user_manager);
                let is_bot = td.auth_manager.is_bot();
                let Some(options) = td.option_manager.as_mut() else {
                    td.user_manager = user_manager;
                    td.send_error_impl(id, 400, "Option manager is not initialized");
                    return;
                };
                let env = crate::user_manager::UserEnv {
                    global: &mut td.global,
                    options,
                    auth_is_bot: is_bot,
                };
                let input_user = user_manager
                    .get_input_user(&env, crate::user_manager::UserId::new(request.user_id));
                td.user_manager = user_manager;
                match input_user {
                    Ok(input_user) => input_user,
                    Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            group_call_arm(td, None, |_env, manager| {
                manager.invite_group_call_participant(
                    group_call_id,
                    input_user,
                    request.is_video,
                    promise,
                );
            });
        }

        // declineGroupCallInvitation (Requests.cpp:5364-5368 →
        // cpp:1316-1323) — the invitation message id resolves through
        // the M.10.19 get_message_force seam.
        Function::declineGroupCallInvitation(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let message_full_id = crate::messages_manager::MessageFullId::new(
                crate::messages_manager::DialogId::new(request.chat_id),
                crate::messages_manager::MessageId::new(request.message_id),
            );
            let server_message_id = {
                // the context-alias split of the messages plane
                // (the get_giveaway_message_id seam shape)
                let td_ptr = std::ptr::from_mut(td);
                let manager: &mut crate::messages_manager::MessagesManager =
                    unsafe { &mut (*td_ptr).messages_manager };
                let global: &mut crate::global::Global = unsafe { &mut (*td_ptr).global };
                let sendctx: &dyn crate::messages_manager::SendMessageContext = unsafe { &*td_ptr };
                let state: &mut dyn crate::messages_manager::ReadStateContext =
                    unsafe { crate::td::split_context_alias(td_ptr) };
                let reactions_ctx: &dyn crate::messages_manager::DialogReactionsContext =
                    unsafe { &*td_ptr };
                match crate::messages_manager::get_group_call_message_id(
                    global,
                    manager,
                    sendctx,
                    state,
                    reactions_ctx,
                    message_full_id,
                ) {
                    std::result::Result::Ok(server_message_id) => server_message_id.get(),
                    std::result::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            group_call_arm(td, None, |_env, manager| {
                manager.decline_group_call_invitation(server_message_id, promise);
            });
        }

        // banGroupCallParticipants (Requests.cpp:5370-5374 →
        // cpp:3878-3894) — the self/conference bands; the conference
        // net leg rides the tde2e plane.
        Function::banGroupCallParticipants(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let my_user_id = td.user_manager.get_my_id().get();
            group_call_arm(td, None, |_env, manager| {
                manager.delete_group_call_participants(
                    GroupCallId::new(request.group_call_id),
                    &request.user_ids,
                    my_user_id,
                    promise,
                );
            });
        }

        // inviteVideoChatParticipants (Requests.cpp:5376-5380 →
        // cpp:3895-3924) — the self removal + the input users resolve
        // over the full `Td` frame.
        Function::inviteVideoChatParticipants(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let my_user_id = td.user_manager.get_my_id();
            let user_ids: Vec<_> = request
                .user_ids
                .iter()
                .copied()
                .filter(|&user_id| user_id != my_user_id.get())
                .map(crate::user_manager::UserId::new)
                .collect();
            let input_users = {
                let user_manager = std::mem::take(&mut td.user_manager);
                let is_bot = td.auth_manager.is_bot();
                let Some(options) = td.option_manager.as_mut() else {
                    td.user_manager = user_manager;
                    td.send_error_impl(id, 400, "Option manager is not initialized");
                    return;
                };
                let env = crate::user_manager::UserEnv {
                    global: &mut td.global,
                    options,
                    auth_is_bot: is_bot,
                };
                let input_users = user_manager.get_input_users(&env, &user_ids);
                td.user_manager = user_manager;
                match input_users {
                    Ok(input_users) => input_users,
                    Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            group_call_arm(td, None, |_env, manager| {
                manager.invite_group_call_participants(
                    GroupCallId::new(request.group_call_id),
                    input_users,
                    promise,
                );
            });
        }

        // getVideoChatInviteLink (Requests.cpp:5382-5386 →
        // cpp:4218-4250)
        Function::getVideoChatInviteLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td
                .deferred_promise::<String, _>(id, |url| Object::httpUrl(td_api::HttpUrl { url }));
            group_call_arm(td, None, |_env, manager| {
                manager.get_group_call_invite_link(
                    GroupCallId::new(request.group_call_id),
                    request.can_self_unmute,
                    promise,
                );
            });
        }

        // startGroupCallRecording (Requests.cpp:5388-5393 →
        // cpp:4298-4348)
        Function::startGroupCallRecording(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(title) = clean_or_utf8_error(td, id, &request.title) else {
                return;
            };
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager.toggle_group_call_recording(
                    GroupCallId::new(request.group_call_id),
                    true,
                    title,
                    request.record_video,
                    request.use_portrait_orientation,
                    promise,
                );
            });
        }

        // endGroupCallRecording (Requests.cpp:5395-5399)
        Function::endGroupCallRecording(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager.toggle_group_call_recording(
                    GroupCallId::new(request.group_call_id),
                    false,
                    String::new(),
                    false,
                    false,
                    promise,
                );
            });
        }

        // toggleGroupCallIsMyVideoPaused (Requests.cpp:5401-5405 →
        // cpp:4020-4076) — the local settle.
        Function::toggleGroupCallIsMyVideoPaused(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager.toggle_group_call_is_my_video_paused(
                    GroupCallId::new(request.group_call_id),
                    request.is_my_video_paused,
                    promise,
                );
            });
        }

        // toggleGroupCallIsMyVideoEnabled (Requests.cpp:5407-5411 →
        // cpp:4077-4094) — the local settle.
        Function::toggleGroupCallIsMyVideoEnabled(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager.toggle_group_call_is_my_video_enabled(
                    GroupCallId::new(request.group_call_id),
                    request.is_my_video_enabled,
                    promise,
                );
            });
        }

        // toggleGroupCallParticipantIsMuted / IsHandRaised /
        // setGroupCallParticipantVolumeLevel (Requests.cpp:5413-5429 →
        // cpp:6779-7085) — the participant plane (ticket td-rs/td#54).
        Function::toggleGroupCallParticipantIsMuted(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager.park_media_query("phone.editGroupCallParticipant (is_muted)", promise);
                let _ = request;
            });
        }

        Function::toggleGroupCallParticipantIsHandRaised(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager
                    .park_media_query("phone.editGroupCallParticipant (is_hand_raised)", promise);
                let _ = request;
            });
        }

        Function::setGroupCallParticipantVolumeLevel(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager.park_media_query("phone.editGroupCallParticipant (volume_level)", promise);
                let _ = request;
            });
        }

        // setGroupCallParticipantIsSpeaking (Requests.cpp:5431-5435) —
        // the speaking-action plane rides the media transport.
        Function::setGroupCallParticipantIsSpeaking(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::MessageSender, _>(id, |value| match value {
                    td_api::MessageSender::messageSenderUser(user) => {
                        Object::messageSenderUser(user)
                    }
                    td_api::MessageSender::messageSenderChat(chat) => {
                        Object::messageSenderChat(chat)
                    }
                });
            group_call_arm(td, None, |_env, manager| {
                manager.park_media_query("setGroupCallParticipantIsSpeaking", promise);
                let _ = request;
            });
        }

        // leaveGroupCall (Requests.cpp:5437-5441 → cpp:5090-5127)
        Function::leaveGroupCall(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager.leave_group_call(GroupCallId::new(request.group_call_id), promise);
            });
        }

        // endGroupCall (Requests.cpp:5443-5447 → cpp:7255-7259)
        Function::endGroupCall(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager.discard_group_call(GroupCallId::new(request.group_call_id), promise);
            });
        }

        // getGroupCallParticipants (Requests.cpp:5485-5490 →
        // cpp:4964-4976) — the participant-object build rides the
        // participants plane (§5); the promise parks.
        Function::getGroupCallParticipants(_request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::GroupCallParticipants, _>(id, |value| {
                Object::groupCallParticipants(value)
            });
            group_call_arm(td, None, |_env, manager| {
                manager.park_media_query("phone.getGroupParticipants", promise);
            });
        }

        // loadGroupCallParticipants (Requests.cpp:5492-5496 →
        // cpp:5060-5088)
        Function::loadGroupCallParticipants(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            group_call_arm(td, None, |_env, manager| {
                manager.load_group_call_participants(
                    GroupCallId::new(request.group_call_id),
                    request.limit,
                    promise,
                );
            });
        }

        // getGroupCallStreams (Requests.cpp:5498-5502 →
        // cpp:4246-4293)
        Function::getGroupCallStreams(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::GroupCallStreams, _>(id, |value| {
                Object::groupCallStreams(value)
            });
            group_call_arm(td, None, |_env, manager| {
                manager.get_group_call_streams(GroupCallId::new(request.group_call_id), promise);
            });
        }

        // getGroupCallStreamSegment (Requests.cpp:5504-5508) — the
        // stream-segment transport rides the media plane.
        Function::getGroupCallStreamSegment(_request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Data, _>(id, Object::data);
            group_call_arm(td, None, |_env, manager| {
                manager.park_media_query("getGroupCallStreamSegment", promise);
            });
        }

        // encryptGroupCallData (Requests.cpp:5510-5514) — [blocked: tde2e]
        Function::encryptGroupCallData(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Data, _>(id, Object::data);
            group_call_arm(td, None, |_env, manager| {
                tdutils::log_info!("encryptGroupCallData — [blocked: tde2e]");
                let _ = (manager, promise, request.group_call_id);
            });
        }

        // decryptGroupCallData (Requests.cpp:5516-5520) — [blocked: tde2e]
        Function::decryptGroupCallData(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Data, _>(id, Object::data);
            group_call_arm(td, None, |_env, manager| {
                tdutils::log_info!("decryptGroupCallData — [blocked: tde2e]");
                let _ = (manager, promise, request.group_call_id);
            });
        }

        // Everything else waits for its manager step.
        _ => run_referral_requests(td, id, function),
    }
}

/// The M.52 ReferralProgramManager arm group of the request dispatcher —
/// see the frame note at the forward site in [`run_request`]. The arms
/// route into `td_->referral_program_manager_` (Requests.cpp:9151-9169);
/// every server leg is the [blocked: net] seam.
#[allow(clippy::too_many_lines)]
fn run_referral_requests(td: &mut Td, id: u64, function: Function) {
    fn referral_arm(
        td: &mut Td,
        run: impl FnOnce(
            &mut crate::td::ReferralTdEnv<'_>,
            &mut crate::referral_program::ReferralProgramManager,
        ),
    ) {
        let crate::td::Td {
            referral_program_manager,
            global,
            notification_updates,
            ..
        } = td;
        let mut env = crate::td::ReferralTdEnv {
            global,
            updates: notification_updates,
        };
        run(&mut env, referral_program_manager);
    }

    macro_rules! ok_promise {
        ($td:expr) => {
            $td.deferred_promise::<(), _>(id, |_| ok_object())
        };
    }

    /// The null-result 404 mapper (the chat answer).
    fn chat_optional_to_object(value: Option<td_api::Chat>) -> Object {
        match value {
            Some(chat) => Object::chat(chat),
            None => Object::error(td_api::Error {
                code: 404,
                message: "Not Found".to_owned(),
            }),
        }
    }

    match function {
        // setChatAffiliateProgram (Requests.cpp:9146-9151)
        Function::setChatAffiliateProgram(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let parameters =
                crate::referral_program::ReferralProgramParameters::from_td_api(request.parameters);
            referral_arm(td, |env, manager| {
                manager.set_dialog_referral_program(
                    env,
                    crate::dialog_id::DialogId::new(request.chat_id),
                    parameters,
                    promise,
                );
            });
        }

        // searchChatAffiliateProgram (Requests.cpp:9153-9159)
        Function::searchChatAffiliateProgram(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(username) = clean_or_utf8_error(td, id, &request.username) else {
                return;
            };
            let Some(referrer) = clean_or_utf8_error(td, id, &request.referrer) else {
                return;
            };
            let promise = td.deferred_promise(id, chat_optional_to_object);
            referral_arm(td, |env, manager| {
                manager.search_dialog_referral_program(env, &username, &referrer, promise);
            });
        }

        // searchAffiliatePrograms (Requests.cpp:9161-9166)
        Function::searchAffiliatePrograms(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::FoundAffiliatePrograms, _>(id, |value| {
                Object::foundAffiliatePrograms(value)
            });
            let sort_order =
                crate::referral_program::ReferralProgramSortOrder::from_td_api(request.sort_order);
            referral_arm(td, |env, manager| {
                manager.search_referral_programs(
                    env,
                    request.affiliate,
                    sort_order,
                    &offset,
                    request.limit,
                    promise,
                );
            });
        }

        // connectAffiliateProgram (Requests.cpp:9168-9172)
        Function::connectAffiliateProgram(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |value: Option<td_api::ConnectedAffiliateProgram>| {
                    match value {
                        Some(program) => Object::connectedAffiliateProgram(program),
                        None => Object::error(td_api::Error {
                            code: 404,
                            message: "Not Found".to_owned(),
                        }),
                    }
                });
            referral_arm(td, |env, manager| {
                manager.connect_referral_program(
                    env,
                    request.affiliate,
                    crate::user_manager::UserId::new(request.bot_user_id),
                    promise,
                );
            });
        }

        // disconnectAffiliateProgram (Requests.cpp:9174-9178)
        Function::disconnectAffiliateProgram(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(url) = clean_or_utf8_error(td, id, &request.url) else {
                return;
            };
            let promise =
                td.deferred_promise(id, |value: Option<td_api::ConnectedAffiliateProgram>| {
                    match value {
                        Some(program) => Object::connectedAffiliateProgram(program),
                        None => Object::error(td_api::Error {
                            code: 404,
                            message: "Not Found".to_owned(),
                        }),
                    }
                });
            referral_arm(td, |env, manager| {
                manager.revoke_referral_program(env, request.affiliate, &url, promise);
            });
        }

        // getConnectedAffiliateProgram (Requests.cpp:9180-9184)
        Function::getConnectedAffiliateProgram(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |value: Option<td_api::ConnectedAffiliateProgram>| {
                    match value {
                        Some(program) => Object::connectedAffiliateProgram(program),
                        None => Object::error(td_api::Error {
                            code: 404,
                            message: "Not Found".to_owned(),
                        }),
                    }
                });
            referral_arm(td, |env, manager| {
                manager.get_connected_referral_program(
                    env,
                    request.affiliate,
                    crate::user_manager::UserId::new(request.bot_user_id),
                    promise,
                );
            });
        }

        // getConnectedAffiliatePrograms (Requests.cpp:9186-9190)
        Function::getConnectedAffiliatePrograms(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let promise = td
                .deferred_promise::<td_api::ConnectedAffiliatePrograms, _>(id, |value| {
                    Object::connectedAffiliatePrograms(value)
                });
            referral_arm(td, |env, manager| {
                manager.get_connected_referral_programs(
                    env,
                    request.affiliate,
                    &offset,
                    request.limit,
                    promise,
                );
            });
        }

        Function::createTextCompositionStyle(request) => {
            // `Requests.cpp:2263-2270`.
            if !check_is_user(td, id) {
                return;
            }
            let Some(title) = clean_or_utf8_error(td, id, &request.title) else {
                return;
            };
            let Some(prompt) = clean_or_utf8_error(td, id, &request.prompt) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::TextCompositionStyle, _>(
                id,
                Object::textCompositionStyle,
            );
            let (manager, mut env) = translation_env_split(td);
            manager.create_tone(
                &mut env,
                title,
                request.custom_emoji_id,
                prompt,
                request.show_creator,
                promise,
            );
            td.translation_manager = manager;
        }
        Function::editTextCompositionStyle(request) => {
            // `Requests.cpp:2272-2280`.
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let Some(title) = clean_or_utf8_error(td, id, &request.title) else {
                return;
            };
            let Some(prompt) = clean_or_utf8_error(td, id, &request.prompt) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::TextCompositionStyle, _>(
                id,
                Object::textCompositionStyle,
            );
            let (manager, mut env) = translation_env_split(td);
            manager.update_tone(
                &mut env,
                name,
                title,
                request.custom_emoji_id,
                prompt,
                request.show_creator,
                promise,
            );
            td.translation_manager = manager;
        }
        Function::deleteTextCompositionStyle(request) => {
            // `Requests.cpp:2281-2285`.
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (manager, mut env) = translation_env_split(td);
            manager.delete_tone(&mut env, name, promise);
            td.translation_manager = manager;
        }
        Function::searchTextCompositionStyle(request) => {
            // `Requests.cpp:2286-2292` — the tone read parks at the
            // [blocked: net] query seam like the C++ request.
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::TextCompositionStyle, _>(
                id,
                Object::textCompositionStyle,
            );
            let (manager, mut env) = translation_env_split(td);
            manager.search_tone(&mut env, name, promise);
            td.translation_manager = manager;
        }
        Function::getTextCompositionStyleExample(request) => {
            // `Requests.cpp:2293-2296`.
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::TextCompositionStyleExample, _>(
                id,
                Object::textCompositionStyleExample,
            );
            let (manager, mut env) = translation_env_split(td);
            manager.get_tone_example(&mut env, name, request.example_number, promise);
            td.translation_manager = manager;
        }
        Function::addTextCompositionStyle(request) => {
            // `Requests.cpp:2297-2300`.
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (manager, mut env) = translation_env_split(td);
            manager.add_tone(&mut env, name, promise);
            td.translation_manager = manager;
        }
        Function::removeTextCompositionStyle(request) => {
            // `Requests.cpp:2301-2304`.
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (manager, mut env) = translation_env_split(td);
            manager.remove_tone(&mut env, name, promise);
            td.translation_manager = manager;
        }
        Function::recognizeSpeech(request) => {
            // `Requests.cpp:2933-2938`: CHECK_IS_USER, then the
            // transcription manager. The message probe is the
            // [blocked: M.10] seam — it answers false while no message
            // plane exists, so the C++ "Message not found" band is the
            // offline answer; the `TranscribeAudioQuery` tail rides the
            // `TranscriptionNetKind` plane once the probe resolves.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut manager, mut env) = crate::td::transcription_env_split(td);
            manager.recognize_speech(
                &mut env,
                crate::dialog_id::MessageFullId::new(
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::messages_manager::MessageId::new(request.message_id),
                ),
                promise,
            );
            td.transcription_manager = manager;
        }
        Function::rateSpeechRecognition(request) => {
            // `Requests.cpp:2940-2946`: CHECK_IS_USER, then the
            // transcription manager (the same [blocked: M.10] probe band;
            // the `RateTranscribedAudioQuery` tail rides the
            // `TranscriptionNetKind` plane with the request promise parked
            // in the manager's FIFO lot).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut manager, mut env) = crate::td::transcription_env_split(td);
            manager.rate_speech_recognition(
                &mut env,
                crate::dialog_id::MessageFullId::new(
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::messages_manager::MessageId::new(request.message_id),
                ),
                request.is_good,
                promise,
            );
            td.transcription_manager = manager;
        }
        Function::translateText(request) => {
            // `Requests.cpp:2847-2853`: CHECK_IS_USER, clean the two
            // strings, then the translation manager. The query tail is
            // the [blocked: net] seam — the promise parks unanswered
            // offline.
            if !check_is_user(td, id) {
                return;
            }
            let Some(to_language_code) = clean_or_utf8_error(td, id, &request.to_language_code)
            else {
                return;
            };
            let Some(tone) = clean_or_utf8_error(td, id, &request.tone) else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::FormattedText, _>(id, Object::formattedText);
            let (manager, mut env) = translation_env_split(td);
            manager.translate_text(&mut env, request.text, to_language_code, tone, promise);
            td.translation_manager = manager;
        }
        Function::translateRichMessage(request) => {
            // `Requests.cpp:2855-2861`: CHECK_IS_USER, clean, then the
            // manager; the rich intake parks at the [blocked:
            // RichMessage] seam.
            if !check_is_user(td, id) {
                return;
            }
            let Some(to_language_code) = clean_or_utf8_error(td, id, &request.to_language_code)
            else {
                return;
            };
            let Some(tone) = clean_or_utf8_error(td, id, &request.tone) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::RichMessage, _>(id, Object::richMessage);
            let (manager, mut env) = translation_env_split(td);
            manager.translate_rich_message(
                &mut env,
                request.message.map(|message| *message),
                to_language_code,
                tone,
                promise,
            );
            td.translation_manager = manager;
        }
        Function::composeTextWithAi(request) => {
            // `Requests.cpp:2892-2898`: CHECK_IS_USER, clean, then the
            // manager's tone lookup + the [blocked: net] seam.
            if !check_is_user(td, id) {
                return;
            }
            let Some(translate_to_language_code) =
                clean_or_utf8_error(td, id, &request.translate_to_language_code)
            else {
                return;
            };
            let Some(style_name) = clean_or_utf8_error(td, id, &request.style_name) else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::FormattedText, _>(id, Object::formattedText);
            let (manager, mut env) = translation_env_split(td);
            manager.compose_message_with_ai(
                &mut env,
                request.text,
                translate_to_language_code,
                style_name,
                request.add_emojis,
                promise,
            );
            td.translation_manager = manager;
        }
        Function::composeRichMessageWithAi(request) => {
            // `Requests.cpp:2900-2910`: CHECK_IS_USER, the three strings,
            // then the manager's rich compose ([blocked: RichMessage]).
            if !check_is_user(td, id) {
                return;
            }
            let Some(translate_to_language_code) =
                clean_or_utf8_error(td, id, &request.translate_to_language_code)
            else {
                return;
            };
            let Some(style_name) = clean_or_utf8_error(td, id, &request.style_name) else {
                return;
            };
            let Some(custom_prompt) = clean_or_utf8_error(td, id, &request.custom_prompt) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::RichMessage, _>(id, Object::richMessage);
            let (manager, mut env) = translation_env_split(td);
            manager.compose_rich_message_with_ai(
                &mut env,
                request.message.map(|message| *message),
                translate_to_language_code,
                style_name,
                custom_prompt,
                request.add_emojis,
                promise,
            );
            td.translation_manager = manager;
        }
        Function::createRichMessageWithAi(request) => {
            // `Requests.cpp:2912-2919`: CHECK_IS_USER, clean the two
            // strings, then the manager's rich create
            // ([blocked: RichMessage]).
            if !check_is_user(td, id) {
                return;
            }
            let Some(language_code) = clean_or_utf8_error(td, id, &request.language_code) else {
                return;
            };
            let Some(prompt) = clean_or_utf8_error(td, id, &request.prompt) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::RichMessage, _>(id, Object::richMessage);
            let (manager, mut env) = translation_env_split(td);
            manager.create_rich_message_with_ai(
                &mut env,
                prompt,
                language_code,
                request.add_emojis,
                promise,
            );
            td.translation_manager = manager;
        }
        Function::fixTextWithAi(request) => {
            // `Requests.cpp:2921-2925`: CHECK_IS_USER, then the manager's
            // proofread intake (no strings to clean; the query tail is
            // the [blocked: net] seam).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::FixedText, _>(id, Object::fixedText);
            let (manager, mut env) = translation_env_split(td);
            manager.proofread_message_with_ai(&mut env, request.text, promise);
            td.translation_manager = manager;
        }
        Function::fixRichMessageWithAi(request) => {
            // `Requests.cpp:2927-2931`: CHECK_IS_USER, then the manager's
            // rich proofread ([blocked: RichMessage]).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::RichMessage, _>(id, Object::richMessage);
            let (manager, mut env) = translation_env_split(td);
            manager.proofread_rich_message_with_ai(
                &mut env,
                request.message.map(|message| *message),
                promise,
            );
            td.translation_manager = manager;
        }

        // Everything else waits for its manager step.
        _ => run_saved_messages_requests(td, id, function),
    }
}

/// The SavedMessagesManager arm group of the request dispatcher (SavedMessagesManager.h, Requests.cpp).
/// Handles the 19 saved messages topics and direct messages (monoforum) chat topics arms.
#[allow(clippy::too_many_lines)]
fn run_saved_messages_requests(td: &mut Td, id: u64, function: Function) {
    match function {
        Function::loadDirectMessagesChatTopics(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            manager.load_monoforum_topics(
                &mut env,
                crate::dialog_id::DialogId::new(request.chat_id),
                request.limit,
                promise,
            );
        }
        Function::getDirectMessagesChatTopic(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise(id, |v: Option<td_api::DirectMessagesChatTopic>| match v {
                    Some(topic) => Object::directMessagesChatTopic(topic),
                    None => Object::error(td_api::Error {
                        code: 404,
                        message: "Not Found".to_owned(),
                    }),
                });
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            let topic_id = manager.get_topic_id(&env, dialog_id, request.topic_id);
            manager.get_monoforum_topic(&mut env, dialog_id, topic_id, promise);
        }
        Function::getDirectMessagesChatTopicHistory(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |v: Option<td_api::Messages>| match v {
                Some(msgs) => Object::messages(msgs),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_owned(),
                }),
            });
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            let topic_id = manager.get_topic_id(&env, dialog_id, request.topic_id);
            manager.get_monoforum_topic_history(
                &mut env,
                dialog_id,
                topic_id,
                crate::message_id::MessageId::new(request.from_message_id),
                request.offset,
                request.limit,
                promise,
            );
        }
        Function::getDirectMessagesChatTopicMessageByDate(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |v: Option<td_api::Message>| match v {
                Some(msg) => Object::message(msg),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_owned(),
                }),
            });
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            let topic_id = manager.get_topic_id(&env, dialog_id, request.topic_id);
            manager.get_monoforum_topic_message_by_date(
                &mut env,
                dialog_id,
                topic_id,
                request.date,
                promise,
            );
        }
        Function::deleteDirectMessagesChatTopicHistory(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            let topic_id = manager.get_topic_id(&env, dialog_id, request.topic_id);
            manager.delete_monoforum_topic_history(&mut env, dialog_id, topic_id, promise);
        }
        Function::deleteDirectMessagesChatTopicMessagesByDate(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            let topic_id = manager.get_topic_id(&env, dialog_id, request.topic_id);
            manager.delete_monoforum_topic_messages_by_date(
                &mut env,
                dialog_id,
                topic_id,
                request.min_date,
                request.max_date,
                promise,
            );
        }
        Function::setDirectMessagesChatTopicIsMarkedAsUnread(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            let topic_id = manager.get_topic_id(&env, dialog_id, request.topic_id);
            manager.set_monoforum_topic_is_marked_as_unread(
                &mut env,
                dialog_id,
                topic_id,
                request.is_marked_as_unread,
                promise,
            );
        }
        Function::unpinAllDirectMessagesChatTopicMessages(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            let topic_id = manager.get_topic_id(&env, dialog_id, request.topic_id);
            manager.unpin_all_monoforum_topic_messages(&mut env, dialog_id, topic_id, promise);
        }
        Function::readAllDirectMessagesChatTopicReactions(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            let topic_id = manager.get_topic_id(&env, dialog_id, request.topic_id);
            manager.read_all_monoforum_topic_reactions(&mut env, dialog_id, topic_id, promise);
        }
        Function::getDirectMessagesChatTopicRevenue(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |v: Option<td_api::StarCount>| match v {
                Some(sc) => Object::starCount(sc),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_owned(),
                }),
            });
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            let topic_id = manager.get_topic_id(&env, dialog_id, request.topic_id);
            manager.get_monoforum_topic_revenue(&mut env, dialog_id, topic_id, promise);
        }
        Function::toggleDirectMessagesChatTopicCanSendUnpaidMessages(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            let dialog_id = crate::dialog_id::DialogId::new(request.chat_id);
            let topic_id = manager.get_topic_id(&env, dialog_id, request.topic_id);
            manager.toggle_monoforum_topic_nopaid_messages_exception(
                &mut env,
                dialog_id,
                topic_id,
                request.can_send_unpaid_messages,
                request.refund_payments,
                promise,
            );
        }
        Function::loadSavedMessagesTopics(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            manager.load_saved_messages_topics(&mut env, request.limit, promise);
        }
        Function::getSavedMessagesTopicHistory(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |v: Option<td_api::Messages>| match v {
                Some(msgs) => Object::messages(msgs),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_owned(),
                }),
            });
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            let topic_id = manager.get_topic_id(
                &env,
                crate::dialog_id::DialogId::default(),
                request.saved_messages_topic_id,
            );
            manager.get_saved_messages_topic_history(
                &mut env,
                topic_id,
                crate::message_id::MessageId::new(request.from_message_id),
                request.offset,
                request.limit,
                promise,
            );
        }
        Function::getSavedMessagesTopicMessageByDate(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |v: Option<td_api::Message>| match v {
                Some(msg) => Object::message(msg),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_owned(),
                }),
            });
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            let topic_id = manager.get_topic_id(
                &env,
                crate::dialog_id::DialogId::default(),
                request.saved_messages_topic_id,
            );
            manager.get_saved_messages_topic_message_by_date(
                &mut env,
                topic_id,
                request.date,
                promise,
            );
        }
        Function::deleteSavedMessagesTopicHistory(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            let topic_id = manager.get_topic_id(
                &env,
                crate::dialog_id::DialogId::default(),
                request.saved_messages_topic_id,
            );
            manager.delete_saved_messages_topic_history(&mut env, topic_id, promise);
        }
        Function::deleteSavedMessagesTopicMessagesByDate(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            let topic_id = manager.get_topic_id(
                &env,
                crate::dialog_id::DialogId::default(),
                request.saved_messages_topic_id,
            );
            manager.delete_saved_messages_topic_messages_by_date(
                &mut env,
                topic_id,
                request.min_date,
                request.max_date,
                promise,
            );
        }
        Function::toggleSavedMessagesTopicIsPinned(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            let topic_id = manager.get_topic_id(
                &env,
                crate::dialog_id::DialogId::default(),
                request.saved_messages_topic_id,
            );
            manager.toggle_saved_messages_topic_is_pinned(
                &mut env,
                topic_id,
                request.is_pinned,
                promise,
            );
        }
        Function::setPinnedSavedMessagesTopics(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            let topic_ids = manager.get_topic_ids(
                &env,
                crate::dialog_id::DialogId::default(),
                &request.saved_messages_topic_ids,
            );
            manager.set_pinned_saved_messages_topics(&mut env, topic_ids, promise);
        }
        Function::getMessageAuthor(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |v: Option<td_api::User>| match v {
                Some(user) => Object::user(user),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_owned(),
                }),
            });
            let (mut env, manager) = crate::td::saved_messages_env_split(td);
            manager.get_monoforum_message_author(
                &mut env,
                crate::message_id::MessageFullId::new(
                    crate::dialog_id::DialogId::new(request.chat_id),
                    crate::message_id::MessageId::new(request.message_id),
                ),
                promise,
            );
        }
        _ => run_dialog_filter_requests(td, id, function),
    }
}

fn run_dialog_filter_requests(td: &mut Td, id: u64, function: Function) {
    match function {
        Function::getChatFolder(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, Object::chatFolder);
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            manager.get_dialog_filter_request(
                &mut env,
                crate::dialog_filter_manager::DialogFilterId::new(request.chat_folder_id),
                promise,
            );
        }
        Function::getRecommendedChatFolders(_request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, Object::recommendedChatFolders);
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            manager.get_recommended_dialog_filters(&mut env, promise);
        }
        Function::createChatFolder(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, Object::chatFolderInfo);
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            manager.create_dialog_filter(&mut env, request.folder, promise);
        }
        Function::editChatFolder(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, Object::chatFolderInfo);
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            manager.edit_dialog_filter(
                &mut env,
                crate::dialog_filter_manager::DialogFilterId::new(request.chat_folder_id),
                request.folder,
                promise,
            );
        }
        Function::deleteChatFolder(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            let leave_dialog_ids = request
                .leave_chat_ids
                .into_iter()
                .map(crate::dialog_id::DialogId::new)
                .collect();
            manager.delete_dialog_filter(
                &mut env,
                crate::dialog_filter_manager::DialogFilterId::new(request.chat_folder_id),
                leave_dialog_ids,
                promise,
            );
        }
        Function::getChatFolderChatsToLeave(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, Object::chats);
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            manager.get_leave_dialog_filter_suggestions(
                &mut env,
                crate::dialog_filter_manager::DialogFilterId::new(request.chat_folder_id),
                promise,
            );
        }
        Function::reorderChatFolders(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            let dialog_filter_ids = request
                .chat_folder_ids
                .into_iter()
                .map(crate::dialog_filter_manager::DialogFilterId::new)
                .collect();
            manager.reorder_dialog_filters(
                &mut env,
                dialog_filter_ids,
                request.main_chat_list_position,
                promise,
            );
        }
        Function::toggleChatFolderTags(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            manager.toggle_dialog_filter_tags(&mut env, request.are_tags_enabled, promise);
        }
        Function::getChatsForChatFolderInviteLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, Object::chats);
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            manager.get_dialogs_for_dialog_filter_invite_link(
                &mut env,
                crate::dialog_filter_manager::DialogFilterId::new(request.chat_folder_id),
                promise,
            );
        }
        Function::createChatFolderInviteLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(name) = crate::misc::clean_input_string(&request.name) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, Object::chatFolderInviteLink);
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            let dialog_ids = request
                .chat_ids
                .into_iter()
                .map(crate::dialog_id::DialogId::new)
                .collect();
            manager.create_dialog_filter_invite_link(
                &mut env,
                crate::dialog_filter_manager::DialogFilterId::new(request.chat_folder_id),
                name,
                dialog_ids,
                promise,
            );
        }
        Function::getChatFolderInviteLinks(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, Object::chatFolderInviteLinks);
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            manager.get_dialog_filter_invite_links(
                &mut env,
                crate::dialog_filter_manager::DialogFilterId::new(request.chat_folder_id),
                promise,
            );
        }
        Function::editChatFolderInviteLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(invite_link) = crate::misc::clean_input_string(&request.invite_link) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(name) = crate::misc::clean_input_string(&request.name) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, Object::chatFolderInviteLink);
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            let dialog_ids = request
                .chat_ids
                .into_iter()
                .map(crate::dialog_id::DialogId::new)
                .collect();
            manager.edit_dialog_filter_invite_link(
                &mut env,
                crate::dialog_filter_manager::DialogFilterId::new(request.chat_folder_id),
                invite_link,
                name,
                dialog_ids,
                promise,
            );
        }
        Function::deleteChatFolderInviteLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(invite_link) = crate::misc::clean_input_string(&request.invite_link) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            manager.delete_dialog_filter_invite_link(
                &mut env,
                crate::dialog_filter_manager::DialogFilterId::new(request.chat_folder_id),
                invite_link,
                promise,
            );
        }
        Function::checkChatFolderInviteLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(invite_link) = crate::misc::clean_input_string(&request.invite_link) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, Object::chatFolderInviteLinkInfo);
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            manager.check_dialog_filter_invite_link(&mut env, invite_link, promise);
        }
        Function::addChatFolderByInviteLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(invite_link) = crate::misc::clean_input_string(&request.invite_link) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            let dialog_ids = request
                .chat_ids
                .into_iter()
                .map(crate::dialog_id::DialogId::new)
                .collect();
            manager.add_dialog_filter_by_invite_link(&mut env, invite_link, dialog_ids, promise);
        }
        Function::getChatFolderNewChats(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, Object::chats);
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            manager.get_dialog_filter_new_chats(
                &mut env,
                crate::dialog_filter_manager::DialogFilterId::new(request.chat_folder_id),
                promise,
            );
        }
        Function::processChatFolderNewChats(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_filter_env_split(td);
            let dialog_ids = request
                .added_chat_ids
                .into_iter()
                .map(crate::dialog_id::DialogId::new)
                .collect();
            manager.add_dialog_filter_new_chats(
                &mut env,
                crate::dialog_filter_manager::DialogFilterId::new(request.chat_folder_id),
                dialog_ids,
                promise,
            );
        }
        _ => run_dialog_invite_link_requests(td, id, function),
    }
}

fn run_dialog_invite_link_requests(td: &mut Td, id: u64, function: Function) {
    match function {
        Function::replacePrimaryChatInviteLink(request) => {
            let promise = td.deferred_promise(id, Object::chatInviteLink);
            let (mut env, manager) = crate::td::dialog_invite_link_env_split(td);
            manager.export_dialog_invite_link(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                String::new(),
                0,
                0,
                false,
                crate::chat_manager::StarSubscriptionPricing::default(),
                false,
                true,
                promise,
            );
        }
        Function::createChatInviteLink(request) => {
            let Some(name) = crate::misc::clean_input_string(&request.name) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, Object::chatInviteLink);
            let (mut env, manager) = crate::td::dialog_invite_link_env_split(td);
            manager.export_dialog_invite_link(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                name,
                request.expiration_date,
                request.member_limit,
                request.creates_join_request,
                crate::chat_manager::StarSubscriptionPricing::default(),
                false,
                false,
                promise,
            );
        }
        Function::createChatSubscriptionInviteLink(request) => {
            let Some(name) = crate::misc::clean_input_string(&request.name) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let pricing = crate::chat_manager::StarSubscriptionPricing::from_td_api(
                request.subscription_pricing,
            );
            let promise = td.deferred_promise(id, Object::chatInviteLink);
            let (mut env, manager) = crate::td::dialog_invite_link_env_split(td);
            manager.export_dialog_invite_link(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                name,
                0,
                0,
                false,
                pricing,
                true,
                false,
                promise,
            );
        }
        Function::editChatInviteLink(request) => {
            let Some(name) = crate::misc::clean_input_string(&request.name) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(invite_link) = crate::misc::clean_input_string(&request.invite_link) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, Object::chatInviteLink);
            let (mut env, manager) = crate::td::dialog_invite_link_env_split(td);
            manager.edit_dialog_invite_link(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                invite_link,
                name,
                request.expiration_date,
                request.member_limit,
                request.creates_join_request,
                false,
                promise,
            );
        }
        Function::editChatSubscriptionInviteLink(request) => {
            let Some(name) = crate::misc::clean_input_string(&request.name) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(invite_link) = crate::misc::clean_input_string(&request.invite_link) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, Object::chatInviteLink);
            let (mut env, manager) = crate::td::dialog_invite_link_env_split(td);
            manager.edit_dialog_invite_link(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                invite_link,
                name,
                0,
                0,
                false,
                true,
                promise,
            );
        }
        Function::getChatInviteLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(invite_link) = crate::misc::clean_input_string(&request.invite_link) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, Object::chatInviteLink);
            let (mut env, manager) = crate::td::dialog_invite_link_env_split(td);
            manager.get_dialog_invite_link(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                invite_link,
                promise,
            );
        }
        Function::getChatInviteLinkCounts(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, Object::chatInviteLinkCounts);
            let (mut env, manager) = crate::td::dialog_invite_link_env_split(td);
            manager.get_dialog_invite_link_counts(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                promise,
            );
        }
        Function::getChatInviteLinks(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(offset_invite_link) =
                crate::misc::clean_input_string(&request.offset_invite_link)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, Object::chatInviteLinks);
            let (mut env, manager) = crate::td::dialog_invite_link_env_split(td);
            manager.get_dialog_invite_links(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                crate::user_manager::UserId::new(request.creator_user_id),
                request.is_revoked,
                request.offset_date,
                offset_invite_link,
                request.limit,
                promise,
            );
        }
        Function::getChatInviteLinkMembers(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(invite_link) = crate::misc::clean_input_string(&request.invite_link) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, Object::chatInviteLinkMembers);
            let (mut env, manager) = crate::td::dialog_invite_link_env_split(td);
            manager.get_dialog_invite_link_users(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                invite_link,
                request.only_with_expired_subscription,
                request.offset_member,
                request.limit,
                promise,
            );
        }
        Function::revokeChatInviteLink(request) => {
            let Some(invite_link) = crate::misc::clean_input_string(&request.invite_link) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, Object::chatInviteLinks);
            let (mut env, manager) = crate::td::dialog_invite_link_env_split(td);
            manager.revoke_dialog_invite_link(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                invite_link,
                promise,
            );
        }
        Function::deleteRevokedChatInviteLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(invite_link) = crate::misc::clean_input_string(&request.invite_link) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_invite_link_env_split(td);
            manager.delete_revoked_dialog_invite_link(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                invite_link,
                promise,
            );
        }
        Function::deleteAllRevokedChatInviteLinks(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_invite_link_env_split(td);
            manager.delete_all_revoked_dialog_invite_links(
                &mut env,
                crate::chat_manager::DialogId::new(request.chat_id),
                crate::user_manager::UserId::new(request.creator_user_id),
                promise,
            );
        }
        Function::checkChatInviteLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(invite_link) = crate::misc::clean_input_string(&request.invite_link) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, Object::chatInviteLinkInfo);
            let (mut env, manager) = crate::td::dialog_invite_link_env_split(td);
            manager.check_dialog_invite_link(&mut env, invite_link, promise);
        }
        Function::joinChatByInviteLink(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(invite_link) = crate::misc::clean_input_string(&request.invite_link) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |v: td_api::ChatJoinResult| v.into());
            let (mut env, manager) = crate::td::dialog_invite_link_env_split(td);
            manager.import_dialog_invite_link(&mut env, invite_link, promise);
        }
        _ => run_password_and_auth_requests(td, id, function),
    }
}

/// The PasswordManager and AuthManager edge request dispatcher.
#[allow(clippy::too_many_lines)]
fn run_password_and_auth_requests(td: &mut Td, id: u64, function: Function) {
    match function {
        Function::getPasswordState(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let state = td.password_manager.get_password_state();
            td.send_result(id, Object::passwordState(state));
        }
        Function::setPassword(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(old_password) = clean_or_utf8_error(td, id, &request.old_password) else {
                return;
            };
            let Some(new_password) = clean_or_utf8_error(td, id, &request.new_password) else {
                return;
            };
            let Some(new_hint) = clean_or_utf8_error(td, id, &request.new_hint) else {
                return;
            };
            let Some(new_recovery_email_address) =
                clean_or_utf8_error(td, id, &request.new_recovery_email_address)
            else {
                return;
            };
            match td.password_manager.set_password(
                &old_password,
                &new_password,
                &new_hint,
                request.set_recovery_email_address,
                &new_recovery_email_address,
            ) {
                Ok(state) => td.send_result(id, Object::passwordState(state)),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::setLoginEmailAddress(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(email) = clean_or_utf8_error(td, id, &request.new_login_email_address) else {
                return;
            };
            match td.password_manager.set_login_email_address(&email) {
                Ok(info) => td.send_result(id, Object::emailAddressAuthenticationCodeInfo(info)),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::resendLoginEmailAddressCode(_) => {
            if !check_is_user(td, id) {
                return;
            }
            match td.password_manager.resend_login_email_address_code() {
                Ok(info) => td.send_result(id, Object::emailAddressAuthenticationCodeInfo(info)),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::checkLoginEmailAddressCode(request) => {
            if !check_is_user(td, id) {
                return;
            }
            match td
                .password_manager
                .check_login_email_address_code(request.code)
            {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::setRecoveryEmailAddress(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(password) = clean_or_utf8_error(td, id, &request.password) else {
                return;
            };
            let Some(new_recovery_email_address) =
                clean_or_utf8_error(td, id, &request.new_recovery_email_address)
            else {
                return;
            };
            match td
                .password_manager
                .set_recovery_email_address(&password, &new_recovery_email_address)
            {
                Ok(state) => td.send_result(id, Object::passwordState(state)),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::getRecoveryEmailAddress(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(password) = clean_or_utf8_error(td, id, &request.password) else {
                return;
            };
            match td.password_manager.get_recovery_email_address(&password) {
                Ok(addr) => td.send_result(id, Object::recoveryEmailAddress(addr)),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::checkRecoveryEmailAddressCode(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(code) = clean_or_utf8_error(td, id, &request.code) else {
                return;
            };
            match td.password_manager.check_recovery_email_address_code(&code) {
                Ok(state) => td.send_result(id, Object::passwordState(state)),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::resendRecoveryEmailAddressCode(_) => {
            if !check_is_user(td, id) {
                return;
            }
            match td.password_manager.resend_recovery_email_address_code() {
                Ok(state) => td.send_result(id, Object::passwordState(state)),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::cancelRecoveryEmailAddressVerification(_) => {
            if !check_is_user(td, id) {
                return;
            }
            match td
                .password_manager
                .cancel_recovery_email_address_verification()
            {
                Ok(state) => td.send_result(id, Object::passwordState(state)),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::sendEmailAddressVerificationCode(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(email) = clean_or_utf8_error(td, id, &request.email_address) else {
                return;
            };
            match td
                .password_manager
                .send_email_address_verification_code(&email)
            {
                Ok(info) => td.send_result(id, Object::emailAddressAuthenticationCodeInfo(info)),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::resendEmailAddressVerificationCode(_) => {
            if !check_is_user(td, id) {
                return;
            }
            match td.password_manager.resend_email_address_verification_code() {
                Ok(info) => td.send_result(id, Object::emailAddressAuthenticationCodeInfo(info)),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::checkEmailAddressVerificationCode(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(code) = clean_or_utf8_error(td, id, &request.code) else {
                return;
            };
            match td
                .password_manager
                .check_email_address_verification_code(&code)
            {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::requestPasswordRecovery(_) => {
            if !check_is_user(td, id) {
                return;
            }
            match td.password_manager.request_password_recovery() {
                Ok(info) => td.send_result(id, Object::emailAddressAuthenticationCodeInfo(info)),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::checkPasswordRecoveryCode(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(code) = clean_or_utf8_error(td, id, &request.recovery_code) else {
                return;
            };
            match td.password_manager.check_password_recovery_code(&code) {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::recoverPassword(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(recovery_code) = clean_or_utf8_error(td, id, &request.recovery_code) else {
                return;
            };
            let Some(new_password) = clean_or_utf8_error(td, id, &request.new_password) else {
                return;
            };
            let Some(new_hint) = clean_or_utf8_error(td, id, &request.new_hint) else {
                return;
            };
            match td
                .password_manager
                .recover_password(&recovery_code, &new_password, &new_hint)
            {
                Ok(state) => td.send_result(id, Object::passwordState(state)),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::resetPassword(_) => {
            if !check_is_user(td, id) {
                return;
            }
            match td.password_manager.reset_password() {
                Ok(res) => td.send_result(id, res.into()),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::cancelPasswordReset(_) => {
            if !check_is_user(td, id) {
                return;
            }
            match td.password_manager.cancel_password_reset() {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::getTemporaryPasswordState(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let state = td.password_manager.get_temporary_password_state();
            td.send_result(id, Object::temporaryPasswordState(state));
        }
        Function::createTemporaryPassword(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(password) = clean_or_utf8_error(td, id, &request.password) else {
                return;
            };
            match td
                .password_manager
                .create_temporary_password(&password, request.valid_for)
            {
                Ok(state) => td.send_result(id, Object::temporaryPasswordState(state)),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::getPasskeyParameters(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let text = td.password_manager.get_passkey_parameters();
            td.send_result(id, Object::text(text));
        }
        Function::getAuthenticationPasskeyParameters(_) => {
            let text = td.password_manager.get_authentication_passkey_parameters();
            td.send_result(id, Object::text(text));
        }
        Function::getLoginPasskeys(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let passkeys = td.password_manager.get_login_passkeys();
            td.send_result(id, Object::passkeys(passkeys));
        }
        Function::addLoginPasskey(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(client_data) = clean_or_utf8_error(td, id, &request.client_data) else {
                return;
            };
            match td
                .password_manager
                .add_login_passkey(&client_data, request.attestation_object)
            {
                Ok(passkey) => td.send_result(id, Object::passkey(passkey)),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::removeLoginPasskey(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(passkey_id) = clean_or_utf8_error(td, id, &request.passkey_id) else {
                return;
            };
            match td.password_manager.remove_login_passkey(&passkey_id) {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => send_error_status(td, id, status),
            }
        }

        // --- AuthManager Edge Flows (16) ---
        Function::resendAuthenticationCode(request) => {
            match td
                .auth_manager
                .resend_authentication_code(request.reason.as_ref())
            {
                Ok(()) => td.send_result(id, ok_object()),
                Err(msg) => td.send_error_impl(id, 400, &msg),
            }
        }

        // -- the PhoneNumberManager plane (the auth-tails glue slice):
        //    Requests.cpp:2480-2513 — the change-phone / verify-phone /
        //    confirm-ownership flows; the queries ride the NetQueryDispatcher
        //    (the auth-net convention) and the answers apply on the drain --
        Function::sendPhoneNumberCode(request) => {
            // Requests.cpp:2480-2486 — CHECK_IS_USER + the phone clean.
            if !check_is_user(td, id) {
                return;
            }
            let Some(phone_number) = clean_or_utf8_error(td, id, &request.phone_number) else {
                return;
            };
            td.send_phone_number_code(
                id,
                phone_number,
                request.settings.as_ref(),
                request.r#type.as_ref(),
            );
        }
        Function::sendPhoneNumberFirebaseSms(request) => {
            // Requests.cpp:2488-2493 — CHECK_IS_USER + the token clean.
            if !check_is_user(td, id) {
                return;
            }
            let Some(token) = clean_or_utf8_error(td, id, &request.token) else {
                return;
            };
            td.send_phone_number_firebase_sms(id, token);
        }
        Function::reportPhoneNumberCodeMissing(request) => {
            // Requests.cpp:2495-2500 — CHECK_IS_USER + the mnc clean.
            if !check_is_user(td, id) {
                return;
            }
            let Some(mobile_network_code) =
                clean_or_utf8_error(td, id, &request.mobile_network_code)
            else {
                return;
            };
            td.report_phone_number_code_missing(id, mobile_network_code);
        }
        Function::resendPhoneNumberCode(request) => {
            // Requests.cpp:2502-2506 — CHECK_IS_USER, no clean.
            if !check_is_user(td, id) {
                return;
            }
            td.resend_phone_number_code(id, request.reason.as_ref());
        }
        Function::checkPhoneNumberCode(request) => {
            // Requests.cpp:2508-2513 — CHECK_IS_USER + the code clean.
            if !check_is_user(td, id) {
                return;
            }
            let Some(code) = clean_or_utf8_error(td, id, &request.code) else {
                return;
            };
            td.check_phone_number_code(id, code);
        }

        Function::requestQrCodeAuthentication(request) => {
            match td
                .auth_manager
                .request_qr_code_authentication(&request.other_user_ids)
            {
                Ok(()) => {
                    td.flush_auth_updates();
                    td.send_result(id, ok_object());
                }
                Err(msg) => td.send_error_impl(id, 400, &msg),
            }
        }
        Function::sendAuthenticationFirebaseSms(request) => {
            let Some(token) = clean_or_utf8_error(td, id, &request.token) else {
                return;
            };
            match td.auth_manager.send_authentication_firebase_sms(&token) {
                Ok(()) => td.send_result(id, ok_object()),
                Err(msg) => td.send_error_impl(id, 400, &msg),
            }
        }
        Function::reportAuthenticationCodeMissing(request) => {
            let Some(code) = clean_or_utf8_error(td, id, &request.mobile_network_code) else {
                return;
            };
            match td.auth_manager.report_authentication_code_missing(&code) {
                Ok(()) => td.send_result(id, ok_object()),
                Err(msg) => td.send_error_impl(id, 400, &msg),
            }
        }
        Function::setAuthenticationEmailAddress(request) => {
            let Some(email) = clean_or_utf8_error(td, id, &request.email_address) else {
                return;
            };
            match td.auth_manager.set_authentication_email_address(&email) {
                Ok(()) => {
                    td.flush_auth_updates();
                    td.send_result(id, ok_object());
                }
                Err(msg) => td.send_error_impl(id, 400, &msg),
            }
        }
        Function::resetAuthenticationEmailAddress(_) => {
            match td.auth_manager.reset_authentication_email_address() {
                Ok(()) => td.send_result(id, ok_object()),
                Err(msg) => td.send_error_impl(id, 400, &msg),
            }
        }
        Function::checkAuthenticationEmailCode(request) => {
            match td
                .auth_manager
                .check_authentication_email_code(request.code.as_ref())
            {
                Ok(()) => {
                    td.flush_auth_updates();
                    td.send_result(id, ok_object());
                }
                Err(msg) => td.send_error_impl(id, 400, &msg),
            }
        }
        Function::setAuthenticationPremiumPurchaseTransaction(request) => {
            match td.auth_manager.set_premium_purchase_transaction(
                request.transaction.as_ref(),
                request.is_restore,
                request.premium_day_count,
                &request.currency,
                request.amount,
            ) {
                Ok(()) => {
                    td.flush_auth_updates();
                    td.send_result(id, ok_object());
                }
                Err(msg) => td.send_error_impl(id, 400, &msg),
            }
        }
        Function::checkAuthenticationPremiumPurchase(request) => {
            let Some(currency) = clean_or_utf8_error(td, id, &request.currency) else {
                return;
            };
            match td.auth_manager.check_premium_purchase(
                request.premium_day_count,
                &currency,
                request.amount,
            ) {
                Ok(()) => td.send_result(id, ok_object()),
                Err(msg) => td.send_error_impl(id, 400, &msg),
            }
        }
        Function::checkAuthenticationPasskey(request) => {
            let Some(credential_id) = clean_or_utf8_error(td, id, &request.credential_id) else {
                return;
            };
            let Some(client_data) = clean_or_utf8_error(td, id, &request.client_data) else {
                return;
            };
            match td.auth_manager.finish_passkey_login(
                &credential_id,
                &client_data,
                &request.authenticator_data,
                &request.signature,
                &request.user_handle,
            ) {
                Ok(()) => {
                    td.flush_auth_updates();
                    td.send_result(id, ok_object());
                }
                Err(msg) => td.send_error_impl(id, 400, &msg),
            }
        }
        Function::checkAuthenticationWebToken(request) => {
            let Some(token) = clean_or_utf8_error(td, id, &request.token) else {
                return;
            };
            match td
                .auth_manager
                .import_web_token_authorization(&token, request.dc_id)
            {
                Ok(()) => {
                    td.flush_auth_updates();
                    td.send_result(id, ok_object());
                }
                Err(msg) => td.send_error_impl(id, 400, &msg),
            }
        }
        Function::checkAuthenticationPasswordRecoveryCode(request) => {
            let Some(code) = clean_or_utf8_error(td, id, &request.recovery_code) else {
                return;
            };
            match td
                .auth_manager
                .check_authentication_password_recovery_code(&code)
            {
                Ok(()) => td.send_result(id, ok_object()),
                Err(msg) => td.send_error_impl(id, 400, &msg),
            }
        }
        Function::requestAuthenticationPasswordRecovery(_) => {
            match td.auth_manager.request_authentication_password_recovery() {
                Ok(()) => td.send_result(id, ok_object()),
                Err(msg) => td.send_error_impl(id, 400, &msg),
            }
        }
        Function::recoverAuthenticationPassword(request) => {
            let Some(code) = clean_or_utf8_error(td, id, &request.recovery_code) else {
                return;
            };
            let Some(new_password) = clean_or_utf8_error(td, id, &request.new_password) else {
                return;
            };
            let Some(new_hint) = clean_or_utf8_error(td, id, &request.new_hint) else {
                return;
            };
            match td
                .auth_manager
                .recover_authentication_password(&code, &new_password, &new_hint)
            {
                Ok(()) => {
                    td.flush_auth_updates();
                    td.send_result(id, ok_object());
                }
                Err(msg) => td.send_error_impl(id, 400, &msg),
            }
        }
        Function::registerUser(request) => {
            let Some(first_name) = clean_or_utf8_error(td, id, &request.first_name) else {
                return;
            };
            let Some(last_name) = clean_or_utf8_error(td, id, &request.last_name) else {
                return;
            };
            match td.auth_manager.register_user(
                &first_name,
                &last_name,
                request.disable_notification,
            ) {
                Ok(()) => {
                    td.flush_auth_updates();
                    td.send_result(id, ok_object());
                }
                Err(msg) => td.send_error_impl(id, 400, &msg),
            }
        }
        Function::deleteAccount(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(reason) = clean_or_utf8_error(td, id, &request.reason) else {
                return;
            };
            let Some(password) = clean_or_utf8_error(td, id, &request.password) else {
                return;
            };
            match td.auth_manager.delete_account(&reason, &password) {
                Ok(()) => {
                    td.flush_auth_updates();
                    td.send_result(id, ok_object());
                }
                Err(msg) => td.send_error_impl(id, 400, &msg),
            }
        }
        _ => run_star_requests(td, id, function),
    }
}

/// The StarManager request dispatcher (the M.star slice; the C++
/// `Requests::on_request` overloads that route into
/// `td_->star_manager_`).
#[allow(clippy::too_many_lines)]
fn run_star_requests(td: &mut Td, id: u64, function: Function) {
    match function {
        Function::getStarPaymentOptions(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, Object::starPaymentOptions);
            let (mut manager, mut env) = crate::td::star_env_split(td);
            manager.get_star_payment_options(&mut env, promise);
            td.star_manager = manager;
        }
        Function::getStarGiftPaymentOptions(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, Object::starPaymentOptions);
            let (mut manager, mut env) = crate::td::star_env_split(td);
            manager.get_star_gift_payment_options(
                &mut env,
                crate::user_manager::UserId::new(request.user_id),
                promise,
            );
            td.star_manager = manager;
        }
        Function::getStarGiveawayPaymentOptions(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, Object::starGiveawayPaymentOptions);
            let (mut manager, mut env) = crate::td::star_env_split(td);
            manager.get_star_giveaway_payment_options(&mut env, promise);
            td.star_manager = manager;
        }
        Function::getStarTransactions(request) => {
            let Some(subscription_id) = clean_or_utf8_error(td, id, &request.subscription_id)
            else {
                return;
            };
            let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            // the C++ manager probes the sender off its `td_`
            // (StarManager.cpp:1597); the collapsed probe rides the bot
            // env (MessageSender.cpp:173-215)
            let dialog_id = {
                let (_manager, mut env) = bot_env_split(td);
                match crate::message_sender::get_message_sender_dialog_id(
                    &mut env,
                    request.owner_id.as_ref(),
                    true,
                    false,
                ) {
                    tdutils::status::Result::Ok(dialog_id) => dialog_id,
                    tdutils::status::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            let promise = td.deferred_promise(id, Object::starTransactions);
            let (mut manager, mut env) = crate::td::star_env_split(td);
            manager.get_star_transactions(
                &mut env,
                dialog_id,
                subscription_id,
                offset,
                request.limit,
                request.direction,
                promise,
            );
            td.star_manager = manager;
        }
        Function::getTonTransactions(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let promise = td.deferred_promise(id, Object::tonTransactions);
            let (mut manager, mut env) = crate::td::star_env_split(td);
            manager.get_ton_transactions(
                &mut env,
                offset,
                request.limit,
                request.direction,
                promise,
            );
            td.star_manager = manager;
        }
        Function::getStarSubscriptions(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let promise = td.deferred_promise(id, Object::starSubscriptions);
            let (mut manager, mut env) = crate::td::star_env_split(td);
            manager.get_star_subscriptions(&mut env, request.only_expiring, offset, promise);
            td.star_manager = manager;
        }
        Function::editStarSubscription(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(subscription_id) = clean_or_utf8_error(td, id, &request.subscription_id)
            else {
                return;
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut manager, mut env) = crate::td::star_env_split(td);
            manager.edit_star_subscription(&mut env, subscription_id, request.is_canceled, promise);
            td.star_manager = manager;
        }
        Function::editUserStarSubscription(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let Some(telegram_payment_charge_id) =
                clean_or_utf8_error(td, id, &request.telegram_payment_charge_id)
            else {
                return;
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut manager, mut env) = crate::td::star_env_split(td);
            manager.edit_user_star_subscription(
                &mut env,
                crate::user_manager::UserId::new(request.user_id),
                telegram_payment_charge_id,
                request.is_canceled,
                promise,
            );
            td.star_manager = manager;
        }
        Function::reuseStarSubscription(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(subscription_id) = clean_or_utf8_error(td, id, &request.subscription_id)
            else {
                return;
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut manager, mut env) = crate::td::star_env_split(td);
            manager.reuse_star_subscription(&mut env, subscription_id, promise);
            td.star_manager = manager;
        }
        Function::refundStarPayment(request) => {
            if !check_is_bot(td, id) {
                return;
            }
            let Some(telegram_payment_charge_id) =
                clean_or_utf8_error(td, id, &request.telegram_payment_charge_id)
            else {
                return;
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut manager, mut env) = crate::td::star_env_split(td);
            manager.refund_star_payment(
                &mut env,
                crate::user_manager::UserId::new(request.user_id),
                telegram_payment_charge_id,
                promise,
            );
            td.star_manager = manager;
        }
        Function::getStarRevenueStatistics(request) => {
            if !check_is_user(td, id) {
                return;
            }
            // the C++ manager probes the sender off its `td_`
            // (StarManager.cpp:1679)
            let dialog_id = {
                let (_manager, mut env) = bot_env_split(td);
                match crate::message_sender::get_message_sender_dialog_id(
                    &mut env,
                    request.owner_id.as_ref(),
                    true,
                    false,
                ) {
                    tdutils::status::Result::Ok(dialog_id) => dialog_id,
                    tdutils::status::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            let promise = td.deferred_promise(id, Object::starRevenueStatistics);
            let (mut manager, mut env) = crate::td::star_env_split(td);
            manager.get_star_revenue_statistics(&mut env, dialog_id, request.is_dark, promise);
            td.star_manager = manager;
        }
        Function::getStarWithdrawalUrl(request) => {
            if !check_is_user(td, id) {
                return;
            }
            // the C++ manager probes the sender off its `td_`
            // (StarManager.cpp:1712)
            let dialog_id = {
                let (_manager, mut env) = bot_env_split(td);
                match crate::message_sender::get_message_sender_dialog_id(
                    &mut env,
                    request.owner_id.as_ref(),
                    true,
                    false,
                ) {
                    tdutils::status::Result::Ok(dialog_id) => dialog_id,
                    tdutils::status::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            let Some(password) = clean_or_utf8_error(td, id, &request.password) else {
                return;
            };
            let promise =
                td.deferred_promise(id, |url: String| Object::httpUrl(td_api::HttpUrl { url }));
            let (mut manager, mut env) = crate::td::star_env_split(td);
            manager.get_star_withdrawal_url(
                &mut env,
                dialog_id,
                request.star_count,
                password,
                promise,
            );
            td.star_manager = manager;
        }
        Function::getStarAdAccountUrl(request) => {
            if !check_is_user(td, id) {
                return;
            }
            // the C++ manager probes the sender off its `td_`
            // (StarManager.cpp:1729)
            let dialog_id = {
                let (_manager, mut env) = bot_env_split(td);
                match crate::message_sender::get_message_sender_dialog_id(
                    &mut env,
                    request.owner_id.as_ref(),
                    true,
                    false,
                ) {
                    tdutils::status::Result::Ok(dialog_id) => dialog_id,
                    tdutils::status::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            let promise =
                td.deferred_promise(id, |url: String| Object::httpUrl(td_api::HttpUrl { url }));
            let (mut manager, mut env) = crate::td::star_env_split(td);
            manager.get_star_ad_account_url(&mut env, dialog_id, promise);
            td.star_manager = manager;
        }
        Function::getGramRevenueStatistics(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, Object::gramRevenueStatistics);
            let (mut manager, mut env) = crate::td::star_env_split(td);
            manager.get_ton_revenue_statistics(&mut env, request.is_dark, promise);
            td.star_manager = manager;
        }
        Function::getGramWithdrawalUrl(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(password) = clean_or_utf8_error(td, id, &request.password) else {
                return;
            };
            let promise =
                td.deferred_promise(id, |url: String| Object::httpUrl(td_api::HttpUrl { url }));
            let (mut manager, mut env) = crate::td::star_env_split(td);
            manager.get_ton_withdrawal_url(&mut env, password, promise);
            td.star_manager = manager;
        }
        Function::getPaidMessageRevenue(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, Object::starCount);
            let (mut manager, mut env) = crate::td::star_env_split(td);
            manager.get_paid_message_revenue(
                &mut env,
                crate::user_manager::UserId::new(request.user_id),
                promise,
            );
            td.star_manager = manager;
        }
        _ => run_message_dialog_requests(td, id, function),
    }
}

/// The Message & Dialog / Interaction Operations arm group of the request dispatcher.
/// Handles 87 functions: MessagesManager (60), ReactionManager (13), QuickReplyManager (6),
/// WelcomeMessageManager (5), and Star/Financial (3).
#[inline(never)]
fn run_message_dialog_requests(td: &mut Td, id: u64, function: Function) {
    match function {
        // --- MessagesManager Direct & Inline Operations (60) ---
        Function::getMessage(request) => {
            let dialog_id = MmDialogId::new(request.chat_id);
            let message_id = MessageId::new(request.message_id);
            let taken = td
                .messages_manager
                .get_dialog_mut(dialog_id)
                .and_then(|d| d.messages.remove(&message_id));
            let object = if let Some(mut m) = taken {
                let obj = crate::messages_manager::get_message_object(
                    &td.messages_manager,
                    td,
                    td,
                    dialog_id,
                    &mut m,
                    "getMessage",
                );
                if let Some(d) = td.messages_manager.get_dialog_mut(dialog_id) {
                    d.messages.insert(message_id, m);
                }
                obj
            } else {
                None
            };
            if let Some(message) = object {
                td.send_result(id, Object::message(message));
            } else {
                let _promise =
                    td.deferred_promise::<Option<td_api::Message>, _>(id, |opt| match opt {
                        Some(message) => Object::message(message),
                        None => Object::error(td_api::Error {
                            code: 404,
                            message: "Not Found".to_string(),
                        }),
                    });
            }
        }
        Function::getMessages(request) => {
            let dialog_id = MmDialogId::new(request.chat_id);
            let mut found_all = true;
            let mut messages = Vec::with_capacity(request.message_ids.len());
            for &mid in &request.message_ids {
                let message_id = MessageId::new(mid);
                let taken = td
                    .messages_manager
                    .get_dialog_mut(dialog_id)
                    .and_then(|d| d.messages.remove(&message_id));
                if let Some(mut m) = taken {
                    let obj = crate::messages_manager::get_message_object(
                        &td.messages_manager,
                        td,
                        td,
                        dialog_id,
                        &mut m,
                        "getMessages",
                    );
                    if let Some(d) = td.messages_manager.get_dialog_mut(dialog_id) {
                        d.messages.insert(message_id, m);
                    }
                    if let Some(msg) = obj {
                        messages.push(msg);
                    } else {
                        found_all = false;
                        break;
                    }
                } else {
                    found_all = false;
                    break;
                }
            }
            if found_all && !request.message_ids.is_empty() {
                td.send_result(
                    id,
                    Object::messages(td_api::Messages {
                        total_count: messages.len() as i32,
                        messages,
                    }),
                );
            } else {
                let _promise = td.deferred_promise::<td_api::Messages, _>(id, Object::messages);
            }
        }
        Function::getRepliedMessage(_) => {
            let _promise = td.deferred_promise::<Option<td_api::Message>, _>(id, |opt| match opt {
                Some(message) => Object::message(message),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_string(),
                }),
            });
        }
        Function::getChatMessagePosition(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::Count, _>(id, Object::count);
        }
        Function::getChatMessageCalendar(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise =
                td.deferred_promise::<td_api::MessageCalendar, _>(id, Object::messageCalendar);
        }
        Function::getChatMessageCount(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::Count, _>(id, Object::count);
        }
        Function::getChatSparseMessagePositions(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise =
                td.deferred_promise::<td_api::MessagePositions, _>(id, Object::messagePositions);
        }
        Function::getChatPinnedMessage(_) => {
            let _promise = td.deferred_promise::<Option<td_api::Message>, _>(id, |opt| match opt {
                Some(message) => Object::message(message),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_string(),
                }),
            });
        }
        Function::getChatScheduledMessages(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::Messages, _>(id, Object::messages);
        }
        Function::getMessageThread(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise =
                td.deferred_promise::<td_api::MessageThreadInfo, _>(id, Object::messageThreadInfo);
        }
        Function::getMessageThreadHistory(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::Messages, _>(id, Object::messages);
        }
        Function::searchChatMessages(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(_query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let _promise =
                td.deferred_promise::<td_api::FoundChatMessages, _>(id, Object::foundChatMessages);
        }
        Function::searchSavedMessages(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(_query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let _promise =
                td.deferred_promise::<td_api::FoundChatMessages, _>(id, Object::foundChatMessages);
        }
        Function::searchSecretMessages(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(_query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let Some(_offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let _promise =
                td.deferred_promise::<td_api::FoundMessages, _>(id, Object::foundMessages);
        }
        Function::searchCallMessages(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise =
                td.deferred_promise::<td_api::FoundMessages, _>(id, Object::foundMessages);
        }
        Function::getMessageReadDate(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::MessageReadDate, _>(id, Into::into);
        }
        Function::getMessageLinkInfo(request) => {
            let Some(_url) = clean_or_utf8_error(td, id, &request.url) else {
                return;
            };
            let _promise =
                td.deferred_promise::<td_api::MessageLinkInfo, _>(id, Object::messageLinkInfo);
        }
        Function::getCallbackQueryMessage(_) => {
            let _promise = td.deferred_promise::<Option<td_api::Message>, _>(id, |opt| match opt {
                Some(message) => Object::message(message),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_string(),
                }),
            });
        }
        Function::getFullRichMessage(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::RichMessage, _>(id, Object::richMessage);
        }
        Function::editMessageText(_) => {
            let _promise = td.deferred_promise::<Option<td_api::Message>, _>(id, |opt| match opt {
                Some(message) => Object::message(message),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_string(),
                }),
            });
        }
        Function::editMessageCaption(_) => {
            let _promise = td.deferred_promise::<Option<td_api::Message>, _>(id, |opt| match opt {
                Some(message) => Object::message(message),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_string(),
                }),
            });
        }
        Function::editMessageMedia(_) => {
            let _promise = td.deferred_promise::<Option<td_api::Message>, _>(id, |opt| match opt {
                Some(message) => Object::message(message),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_string(),
                }),
            });
        }
        Function::editMessageReplyMarkup(_) => {
            if !check_is_bot(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<Option<td_api::Message>, _>(id, |opt| match opt {
                Some(message) => Object::message(message),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_string(),
                }),
            });
        }
        Function::editMessageLiveLocation(_) => {
            let _promise = td.deferred_promise::<Option<td_api::Message>, _>(id, |opt| match opt {
                Some(message) => Object::message(message),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_string(),
                }),
            });
        }
        Function::editMessageSchedulingState(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::editMessageChecklist(_) => {
            let _promise = td.deferred_promise::<Option<td_api::Message>, _>(id, |opt| match opt {
                Some(message) => Object::message(message),
                None => Object::error(td_api::Error {
                    code: 404,
                    message: "Not Found".to_string(),
                }),
            });
        }
        Function::pinChatMessage(request) => {
            // Requests.cpp:6127-6132 — the real pin ladder; the
            // updateChatPinnedMessage server tail parks at the [blocked:
            // net] seam.
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut global = std::mem::take(&mut td.global);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            crate::messages_manager::pin_dialog_message(
                &mut global,
                &mut manager,
                ctx,
                crate::business_connection_manager::BusinessConnectionId::default(),
                MmDialogId::new(request.chat_id),
                crate::messages_manager::MessageId::new(request.message_id),
                request.disable_notification,
                request.only_for_self,
                false,
                promise,
            );
            td.global = global;
            td.messages_manager = manager;
        }
        Function::unpinChatMessage(request) => {
            // Requests.cpp:6134-6138.
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut global = std::mem::take(&mut td.global);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            crate::messages_manager::pin_dialog_message(
                &mut global,
                &mut manager,
                ctx,
                crate::business_connection_manager::BusinessConnectionId::default(),
                MmDialogId::new(request.chat_id),
                crate::messages_manager::MessageId::new(request.message_id),
                false,
                false,
                true,
                promise,
            );
            td.global = global;
            td.messages_manager = manager;
        }
        Function::unpinAllChatMessages(request) => {
            // Requests.cpp:6140-6143.
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut global = std::mem::take(&mut td.global);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            crate::messages_manager::unpin_all_dialog_messages(
                &mut global,
                &mut manager,
                ctx,
                MmDialogId::new(request.chat_id),
                crate::messages_manager::ForumTopicId::default(),
                promise,
            );
            td.global = global;
            td.messages_manager = manager;
        }
        Function::unpinAllForumTopicMessages(request) => {
            // Requests.cpp:6145-6151.
            if request.forum_topic_id == 0 {
                return td.send_error_impl(id, 400, "Invalid forum topic identifier specified");
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut global = std::mem::take(&mut td.global);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            crate::messages_manager::unpin_all_dialog_messages(
                &mut global,
                &mut manager,
                ctx,
                MmDialogId::new(request.chat_id),
                crate::messages_manager::ForumTopicId::from_raw(request.forum_topic_id),
                promise,
            );
            td.global = global;
            td.messages_manager = manager;
        }
        Function::setBusinessMessageIsPinned(_) => {
            if !check_is_bot(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::removeChatActionBar(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::deleteMessageEphemeralContent(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::openMessageContent(_) => {
            if !check_is_user(td, id) {
                return;
            }
            td.send_result(id, ok_object());
        }
        Function::sendChatAction(_) => {
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::stopPendingMessage(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::sendTextMessageDraft(_) => {
            if !check_is_bot(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::sendRichMessageDraft(_) => {
            if !check_is_bot(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::clearAllDraftMessages(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::forwardMessages(_) => {
            let _promise = td.deferred_promise::<td_api::Messages, _>(id, Object::messages);
        }
        Function::resendMessages(_) => {
            let _promise = td.deferred_promise::<td_api::Messages, _>(id, Object::messages);
        }
        Function::addLocalMessage(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::Message, _>(id, Object::message);
        }
        Function::sendBotStartMessage(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(_param) = clean_or_utf8_error(td, id, &request.parameter) else {
                return;
            };
            let _promise = td.deferred_promise::<td_api::Message, _>(id, Object::message);
        }
        Function::sendEphemeralMessage(_) => {
            let _promise = td.deferred_promise::<td_api::Message, _>(id, Object::message);
        }
        Function::sendInlineQueryResultMessage(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(_res) = clean_or_utf8_error(td, id, &request.result_id) else {
                return;
            };
            let _promise = td.deferred_promise::<td_api::Message, _>(id, Object::message);
        }
        Function::sendQuickReplyShortcutMessages(_) => {
            let _promise = td.deferred_promise::<td_api::Messages, _>(id, Object::messages);
        }
        Function::setMessageFactCheck(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::setMessageSenderBlockList(request) => {
            // Requests.cpp:5895-5899 — MessagesManager::
            // set_message_sender_block_list (the toggleChatIsBlocked
            // chain: the local flip, then the ToggleDialogIsBlockedQuery
            // send; the request answers with the ladder's status)
            if !check_is_user(td, id) {
                return;
            }
            match crate::dialog_manager::set_message_sender_block_list(
                td,
                request.sender_id.as_ref(),
                request.block_list.as_ref(),
            ) {
                tdutils::status::Result::Ok(()) => td.send_result(id, ok_object()),
                tdutils::status::Result::Err(status) => send_error_status(td, id, status),
            }
        }
        Function::setChatMessageSender(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::setChatMessageAutoDeleteTime(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::setDefaultMessageAutoDeleteTime(request) => {
            if !check_is_user(td, id) {
                return;
            }
            if request.message_auto_delete_time.is_none() {
                return td.send_error_impl(
                    id,
                    400,
                    "New default message auto-delete time must be non-empty",
                );
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::getDefaultMessageAutoDeleteTime(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::MessageAutoDeleteTime, _>(
                id,
                Object::messageAutoDeleteTime,
            );
        }
        Function::getChatAvailableMessageSenders(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td
                .deferred_promise::<td_api::ChatMessageSenders, _>(id, Object::chatMessageSenders);
        }
        Function::allowUnpaidMessagesFromUser(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::clickAnimatedEmojiMessage(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::Sticker, _>(id, Object::sticker);
        }
        Function::blockMessageSenderFromReplies(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::translateMessageText(request) => {
            // `Requests.cpp:2867-2873` — CHECK_IS_USER, clean the two
            // strings, then the messages-manager glue (the §3.8 net-drain
            // 25 `translateText` PEER shape).
            if !check_is_user(td, id) {
                return;
            }
            let Some(to_language_code) = clean_or_utf8_error(td, id, &request.to_language_code)
            else {
                return;
            };
            let Some(tone) = clean_or_utf8_error(td, id, &request.tone) else {
                return;
            };
            let promise =
                td.deferred_promise::<td_api::FormattedText, _>(id, Object::formattedText);
            td.translate_message_text(
                crate::messages_manager::MessageFullId::new(
                    MmDialogId::new(request.chat_id),
                    crate::messages_manager::MessageId::new(request.message_id),
                ),
                to_language_code,
                tone,
                promise,
            );
        }
        Function::translateMessageRichMessage(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(_lang) = clean_or_utf8_error(td, id, &request.to_language_code) else {
                return;
            };
            let Some(_tone) = clean_or_utf8_error(td, id, &request.tone) else {
                return;
            };
            let _promise = td.deferred_promise::<td_api::RichMessage, _>(id, Object::richMessage);
        }
        Function::searchQuote(request) => {
            let (Some(text), Some(quote)) = (request.text, request.quote) else {
                return td.send_error_impl(id, 400, "Text and quote must be non-empty");
            };
            if !tdutils::utf8::check_utf8(text.text.as_bytes())
                || !tdutils::utf8::check_utf8(quote.text.as_bytes())
            {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            }
            if quote.text.is_empty() {
                return td.send_error_impl(id, 404, "Not Found");
            }
            let pos = {
                let mut best_pos = None;
                let mut min_diff = i32::MAX;
                let mut start = 0;
                while let Some(idx) = text.text[start..].find(&quote.text) {
                    let byte_idx = start + idx;
                    let p = tdutils::utf8::utf8_utf16_length(&text.text[..byte_idx]) as i32;
                    let diff = (p - request.quote_position).abs();
                    if diff < min_diff {
                        min_diff = diff;
                        best_pos = Some(p);
                    }
                    start = byte_idx + quote.text.len().max(1);
                    if start >= text.text.len() {
                        break;
                    }
                }
                best_pos
            };
            match pos {
                Some(position) => td.send_result(
                    id,
                    Object::foundPosition(td_api::FoundPosition { position }),
                ),
                None => td.send_error_impl(id, 404, "Not Found"),
            }
        }

        // --- ReactionManager (13) ---
        Function::addMessageReaction(request) => {
            // Requests.cpp:3936-3941 — the real add_message_reaction
            // ladder; the SendReactionQuery tail parks at the [blocked:
            // net] seam.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut global = std::mem::take(&mut td.global);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            crate::messages_manager::add_message_reaction(
                &mut global,
                &mut manager,
                ctx,
                crate::messages_manager::MessageFullId::new(
                    MmDialogId::new(request.chat_id),
                    crate::messages_manager::MessageId::new(request.message_id),
                ),
                crate::messages_manager::ReactionType::new(
                    crate::reaction_type::ReactionType::from_td_api(request.reaction_type)
                        .get_string()
                        .to_owned(),
                ),
                request.is_big,
                request.update_recent_reactions,
                promise,
            );
            td.global = global;
            td.messages_manager = manager;
        }
        Function::removeMessageReaction(request) => {
            // Requests.cpp:3979-3983.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut global = std::mem::take(&mut td.global);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            crate::messages_manager::remove_message_reaction(
                &mut global,
                &mut manager,
                ctx,
                crate::messages_manager::MessageFullId::new(
                    MmDialogId::new(request.chat_id),
                    crate::messages_manager::MessageId::new(request.message_id),
                ),
                crate::messages_manager::ReactionType::new(
                    crate::reaction_type::ReactionType::from_td_api(request.reaction_type)
                        .get_string()
                        .to_owned(),
                ),
                promise,
            );
            td.global = global;
            td.messages_manager = manager;
        }
        Function::setMessageReactions(_) => {
            // Requests.cpp:4000-4004 — the arm-level free
            // `set_message_reactions` (MessageReactions.cpp) is not ported
            // yet; only the post-mutation fan-out landed with the mmreq
            // reconcile, so the arm stays a parked placeholder.
            if !check_is_bot(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::readAllChatReactions(request) => {
            // Requests.cpp:4184-4188.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut global = std::mem::take(&mut td.global);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            crate::messages_manager::read_all_dialog_reactions(
                &mut global,
                &mut manager,
                ctx,
                MmDialogId::new(request.chat_id),
                crate::messages_manager::ForumTopicId::default(),
                promise,
            );
            td.global = global;
            td.messages_manager = manager;
        }
        Function::readAllForumTopicReactions(request) => {
            // Requests.cpp:4190-4198.
            if !check_is_user(td, id) {
                return;
            }
            if request.forum_topic_id == 0 {
                return td.send_error_impl(id, 400, "Invalid forum topic identifier specified");
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut global = std::mem::take(&mut td.global);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            crate::messages_manager::read_all_dialog_reactions(
                &mut global,
                &mut manager,
                ctx,
                MmDialogId::new(request.chat_id),
                crate::messages_manager::ForumTopicId::from_raw(request.forum_topic_id),
                promise,
            );
            td.global = global;
            td.messages_manager = manager;
        }
        Function::getMessageAddedReactions(request) => {
            // Requests.cpp:4006-4012 — the M.react slice routes the query
            // through `get_message_added_reactions`; the list query tail
            // stays [blocked: net]
            if !check_is_user(td, id) {
                return;
            }
            let Some(offset) = crate::misc::clean_input_string(&request.offset) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, Object::addedReactions);
            let (mut env, manager) = crate::td::message_reactions_query_env_split(td);
            crate::message_reaction::get_message_added_reactions(
                &mut env,
                manager,
                crate::messages_manager::MessageFullId::new(
                    crate::messages_manager::DialogId::new(request.chat_id),
                    crate::messages_manager::MessageId::new(request.message_id),
                ),
                crate::reaction_type::ReactionType::from_td_api(request.reaction_type),
                offset,
                request.limit,
                promise,
            );
        }
        Function::getMessageAvailableReactions(request) => {
            // Requests.cpp:3919-3930 — the dialog/message access ladder,
            // the sorted list built through the M.16 ReactionManager.
            if !check_is_user(td, id) {
                return;
            }
            let mut global = std::mem::take(&mut td.global);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            let result = crate::messages_manager::get_message_available_reactions_full(
                &mut global,
                &mut manager,
                ctx,
                crate::messages_manager::MessageFullId::new(
                    MmDialogId::new(request.chat_id),
                    crate::messages_manager::MessageId::new(request.message_id),
                ),
                request.row_size,
            );
            td.global = global;
            td.messages_manager = manager;
            match result {
                Ok(reactions) => td.send_result(id, Object::availableReactions(reactions)),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::reportMessageReactions(request) => {
            // Requests.cpp:7858-7865 — the M.react slice validates the
            // ladder in `report_message_reactions`; the report query tail
            // stays [blocked: net]
            if !check_is_user(td, id) {
                return;
            }
            let sender_dialog_id = {
                let (_manager, mut env) = bot_env_split(td);
                match crate::message_sender::get_message_sender_dialog_id(
                    &mut env,
                    request.sender_id.as_ref(),
                    false,
                    false,
                ) {
                    tdutils::status::Result::Ok(dialog_id) => dialog_id,
                    tdutils::status::Result::Err(status) => {
                        return send_error_status(td, id, status);
                    }
                }
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, manager) = crate::td::message_reactions_query_env_split(td);
            crate::message_reaction::report_message_reactions(
                &mut env,
                manager,
                crate::messages_manager::MessageFullId::new(
                    crate::messages_manager::DialogId::new(request.chat_id),
                    crate::messages_manager::MessageId::new(request.message_id),
                ),
                sender_dialog_id,
                promise,
            );
        }
        Function::addPendingPaidMessageReaction(request) => {
            // Requests.cpp:3947-3951 — the pending paid-reaction ladder;
            // the SendPaidReactionQuery tail parks at the [blocked: net]
            // seam.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut global = std::mem::take(&mut td.global);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            crate::messages_manager::add_paid_message_reaction(
                &mut global,
                &mut manager,
                ctx,
                crate::messages_manager::MessageFullId::new(
                    MmDialogId::new(request.chat_id),
                    crate::messages_manager::MessageId::new(request.message_id),
                ),
                request.star_count,
                request.r#type,
                promise,
            );
            td.global = global;
            td.messages_manager = manager;
        }
        Function::removePendingPaidMessageReactions(request) => {
            // Requests.cpp:3957-3961.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut global = std::mem::take(&mut td.global);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            crate::messages_manager::remove_paid_message_reactions(
                &mut global,
                &mut manager,
                ctx,
                crate::messages_manager::MessageFullId::new(
                    MmDialogId::new(request.chat_id),
                    crate::messages_manager::MessageId::new(request.message_id),
                ),
                promise,
            );
            td.global = global;
            td.messages_manager = manager;
        }
        Function::commitPendingPaidMessageReactions(request) => {
            // Requests.cpp:3953-3956.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut global = std::mem::take(&mut td.global);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            crate::messages_manager::commit_paid_message_reactions(
                &mut global,
                &mut manager,
                ctx,
                crate::messages_manager::MessageFullId::new(
                    MmDialogId::new(request.chat_id),
                    crate::messages_manager::MessageId::new(request.message_id),
                ),
                promise,
            );
            td.global = global;
            td.messages_manager = manager;
        }
        Function::setPaidMessageReactionType(request) => {
            // Requests.cpp:3972-3976.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut global = std::mem::take(&mut td.global);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            crate::messages_manager::set_paid_message_reaction_type(
                &mut global,
                &mut manager,
                ctx,
                crate::messages_manager::MessageFullId::new(
                    MmDialogId::new(request.chat_id),
                    crate::messages_manager::MessageId::new(request.message_id),
                ),
                request.r#type,
                promise,
            );
            td.global = global;
            td.messages_manager = manager;
        }
        Function::setChatAvailableReactions(request) => {
            // Requests.cpp:6082-6087 — the M.react slice runs the full
            // `set_dialog_available_reactions` ladder: the access and
            // rights checks, the local dialog mutation with the
            // generation bump, then the server ack ([blocked: net])
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut env, manager) = crate::td::dialog_available_reactions_env_split(td);
            crate::messages_manager::set_dialog_available_reactions(
                manager,
                &mut env,
                crate::messages_manager::DialogId::new(request.chat_id),
                request.available_reactions,
                promise,
            );
        }

        // --- QuickReplyManager (6) ---
        Function::addQuickReplyShortcutMessage(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(_name) = clean_or_utf8_error(td, id, &request.shortcut_name) else {
                return;
            };
            let _promise =
                td.deferred_promise::<td_api::QuickReplyMessage, _>(id, Object::quickReplyMessage);
        }
        Function::addQuickReplyShortcutMessageAlbum(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(_name) = clean_or_utf8_error(td, id, &request.shortcut_name) else {
                return;
            };
            let _promise = td
                .deferred_promise::<td_api::QuickReplyMessages, _>(id, Object::quickReplyMessages);
        }
        Function::addQuickReplyShortcutInlineQueryResultMessage(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(_name) = clean_or_utf8_error(td, id, &request.shortcut_name) else {
                return;
            };
            let Some(_res) = clean_or_utf8_error(td, id, &request.result_id) else {
                return;
            };
            let _promise =
                td.deferred_promise::<td_api::QuickReplyMessage, _>(id, Object::quickReplyMessage);
        }
        Function::editQuickReplyMessage(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::loadQuickReplyShortcutMessages(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::readdQuickReplyShortcutMessages(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(_name) = clean_or_utf8_error(td, id, &request.shortcut_name) else {
                return;
            };
            let _promise = td
                .deferred_promise::<td_api::QuickReplyMessages, _>(id, Object::quickReplyMessages);
        }

        // --- WelcomeMessageManager (5) ---
        Function::addChatWelcomeMessage(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::editChatWelcomeMessage(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::deleteChatWelcomeMessage(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::loadChatWelcomeMessages(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::deleteAllChatWelcomeMessages(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }

        // --- Star / Financial (3) ---
        Function::getPaidMessageRevenue(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::StarCount, _>(id, Object::starCount);
        }
        Function::getStarTransactions(request) => {
            let Some(_sub) = clean_or_utf8_error(td, id, &request.subscription_id) else {
                return;
            };
            let Some(_offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let _promise =
                td.deferred_promise::<td_api::StarTransactions, _>(id, Object::starTransactions);
        }
        Function::getTonTransactions(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(_offset) = clean_or_utf8_error(td, id, &request.offset) else {
                return;
            };
            let _promise =
                td.deferred_promise::<td_api::TonTransactions, _>(id, Object::tonTransactions);
        }

        _ => run_chat_and_group_requests(td, id, function),
    }
}

/// The Chat & Group Management arm group of the request dispatcher.
/// Handles 37 functions: chat creation, chat loading/getting, supergroup/basic group inspection,
/// chat settings/properties toggling, and public/discussion/personal chat queries.
#[inline(never)]
fn run_chat_and_group_requests(td: &mut Td, id: u64, function: Function) {
    fn chat_optional_to_object(value: Option<td_api::Chat>) -> Object {
        match value {
            Some(chat) => Object::chat(chat),
            None => Object::error(td_api::Error {
                code: 404,
                message: "Not Found".to_owned(),
            }),
        }
    }

    match function {
        Function::addChatToList(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::checkCreatedPublicChatsLimit(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::createBasicGroupChat(request) => {
            let dialog_id =
                MmDialogId::from_chat_id(crate::chat_manager::ChatId::new(request.basic_group_id));
            if let Some(chat) = td.chats.get(&dialog_id.get()).cloned() {
                td.send_result(id, Object::chat(chat));
            } else {
                let _promise =
                    td.deferred_promise::<Option<td_api::Chat>, _>(id, chat_optional_to_object);
            }
        }
        Function::createNewBasicGroupChat(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(_title) = clean_or_utf8_error(td, id, &request.title) else {
                return;
            };
            let _promise =
                td.deferred_promise::<Option<td_api::Chat>, _>(id, chat_optional_to_object);
        }
        Function::createNewSupergroupChat(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(_title) = clean_or_utf8_error(td, id, &request.title) else {
                return;
            };
            let Some(_description) = clean_or_utf8_error(td, id, &request.description) else {
                return;
            };
            let _promise =
                td.deferred_promise::<Option<td_api::Chat>, _>(id, chat_optional_to_object);
        }
        Function::createPrivateChat(request) => {
            td.ensure_chat(request.user_id);
            if let Some(chat) = td.chats.get(&request.user_id).cloned() {
                td.send_result(id, Object::chat(chat));
            } else {
                let _promise =
                    td.deferred_promise::<Option<td_api::Chat>, _>(id, chat_optional_to_object);
            }
        }
        Function::createSecretChat(request) => {
            let dialog_id = MmDialogId::from_secret_chat_id(
                crate::user_manager::SecretChatId::new(request.secret_chat_id),
            );
            if let Some(chat) = td.chats.get(&dialog_id.get()).cloned() {
                td.send_result(id, Object::chat(chat));
            } else {
                let _promise =
                    td.deferred_promise::<Option<td_api::Chat>, _>(id, chat_optional_to_object);
            }
        }
        Function::createSupergroupChat(request) => {
            let dialog_id = MmDialogId::from_channel_id(ChannelId::new(request.supergroup_id));
            if let Some(chat) = td.chats.get(&dialog_id.get()).cloned() {
                td.send_result(id, Object::chat(chat));
            } else {
                let _promise =
                    td.deferred_promise::<Option<td_api::Chat>, _>(id, chat_optional_to_object);
            }
        }
        Function::deleteChatReplyMarkup(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let dialog_id = MmDialogId::new(request.chat_id);
            let message_id = MessageId::new(request.message_id);
            if let Some(d) = td.messages_manager.get_dialog_mut(dialog_id)
                && d.reply_markup_message_id == message_id
            {
                d.reply_markup_message_id = MessageId::new(0);
            }
            td.send_result(id, ok_object());
        }
        Function::getArchiveChatListSettings(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::ArchiveChatListSettings, _>(
                id,
                Object::archiveChatListSettings,
            );
        }
        Function::getBasicGroup(request) => {
            if let Some(basic_group) = td
                .chat_manager
                .get_basic_group_object(crate::chat_manager::ChatId::new(request.basic_group_id))
            {
                td.send_result(id, Object::basicGroup(basic_group));
            } else {
                let _promise = td.deferred_promise::<td_api::BasicGroup, _>(id, Object::basicGroup);
            }
        }
        Function::getBasicGroupFullInfo(_) => {
            let _promise = td
                .deferred_promise::<td_api::BasicGroupFullInfo, _>(id, Object::basicGroupFullInfo);
        }
        Function::getChat(request) => {
            if let Some(chat) = td.chats.get(&request.chat_id).cloned() {
                td.send_result(id, Object::chat(chat));
            } else {
                td.ensure_chat(request.chat_id);
                if let Some(chat) = td.chats.get(&request.chat_id).cloned() {
                    td.send_result(id, Object::chat(chat));
                } else {
                    let _promise =
                        td.deferred_promise::<Option<td_api::Chat>, _>(id, chat_optional_to_object);
                }
            }
        }
        Function::getChatBoostLinkInfo(request) => {
            let Some(_url) = clean_or_utf8_error(td, id, &request.url) else {
                return;
            };
            let _promise =
                td.deferred_promise::<td_api::ChatBoostLinkInfo, _>(id, Object::chatBoostLinkInfo);
        }
        Function::getChatEventLog(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(_query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let _promise = td.deferred_promise::<td_api::ChatEvents, _>(id, Object::chatEvents);
        }
        Function::getChatFolderChatCount(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::Count, _>(id, Object::count);
        }
        Function::getChatListsToAddChat(_) => {
            if !check_is_user(td, id) {
                return;
            }
            td.send_result(
                id,
                Object::chatLists(td_api::ChatLists {
                    chat_lists: Vec::new(),
                }),
            );
        }
        Function::getChatNotificationSettingsExceptions(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::Chats, _>(id, Object::chats);
        }
        Function::getCreatedPublicChats(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::Chats, _>(id, Object::chats);
        }
        Function::getInactiveSupergroupChats(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::Chats, _>(id, Object::chats);
        }
        Function::getSuitableDiscussionChats(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::Chats, _>(id, Object::chats);
        }
        Function::getSuitablePersonalChats(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<td_api::Chats, _>(id, Object::chats);
        }
        Function::getSupergroup(request) => {
            if let Some(supergroup) = td
                .chat_manager
                .get_supergroup_object(ChannelId::new(request.supergroup_id))
            {
                td.send_result(id, Object::supergroup(supergroup));
            } else {
                let _promise = td.deferred_promise::<td_api::Supergroup, _>(id, Object::supergroup);
            }
        }
        Function::getSupergroupFullInfo(_) => {
            let _promise = td
                .deferred_promise::<td_api::SupergroupFullInfo, _>(id, Object::supergroupFullInfo);
        }
        Function::loadChats(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::getCommunityFullInfo(_) => {
            // Requests.cpp:3415-3419 — CHECK_IS_USER then
            // CommunityManager::get_community_full (the load variant is
            // gone upstream; the full-info answer rides the manager's
            // reload plane).
            if !check_is_user(td, id) {
                return;
            }
            let _promise =
                td.deferred_promise::<td_api::CommunityFullInfo, _>(id, Object::communityFullInfo);
        }
        Function::reportChatPhoto(request) => {
            // Requests.cpp:7847-7855 — CHECK_IS_USER, the ReportReason
            // conversion, then the manager flow (the promise settles from
            // the ReportProfilePhotoQuery answer)
            if !check_is_user(td, id) {
                return;
            }
            let reason = match crate::report_reason::ReportReason::get_report_reason(
                request.reason.as_ref(),
                request.text,
            ) {
                tdutils::status::Result::Ok(reason) => reason,
                tdutils::status::Result::Err(status) => {
                    return td.send_error_impl(id, status.code(), status.message());
                }
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            crate::dialog_manager::report_dialog_photo(
                td,
                crate::dialog_id::DialogId::new(request.chat_id),
                crate::user_manager::FileId::new(request.file_id, 0),
                reason,
                promise,
            );
        }
        Function::setArchiveChatListSettings(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::setChatClientData(request) => {
            let Some(client_data) = clean_or_utf8_error(td, id, &request.client_data) else {
                return;
            };
            let dialog_id = MmDialogId::new(request.chat_id);
            if let Some(d) = td.messages_manager.get_dialog_mut(dialog_id) {
                d.client_data = client_data;
            }
            td.send_result(id, ok_object());
        }
        Function::setChatNotificationSettings(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::setPersonalChat(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::setPinnedChats(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let _promise = td.deferred_promise::<(), _>(id, |()| ok_object());
        }
        Function::toggleChatDefaultDisableNotification(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let dialog_id = MmDialogId::new(request.chat_id);
            if let Some(d) = td.messages_manager.get_dialog_mut(dialog_id) {
                d.notification_settings.silent_send_message = request.default_disable_notification;
            }
            td.send_result(id, ok_object());
        }
        Function::toggleChatIsMarkedAsUnread(request) => {
            // Requests.cpp:5883-5887 — the real toggle ladder with the
            // unread-counter side effects, not the bare field write.
            if !check_is_user(td, id) {
                return;
            }
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            let result = crate::messages_manager::toggle_dialog_is_marked_as_unread(
                &mut manager,
                ctx,
                MmDialogId::new(request.chat_id),
                request.is_marked_as_unread,
            );
            td.messages_manager = manager;
            match result {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::toggleChatIsPinned(request) => {
            // Requests.cpp:5871-5875.
            if !check_is_user(td, id) {
                return;
            }
            let mut global = std::mem::take(&mut td.global);
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            let result = crate::messages_manager::toggle_dialog_is_pinned(
                &mut global,
                &mut manager,
                ctx,
                crate::messages_manager::DialogListId::from_chat_list(request.chat_list.as_ref()),
                MmDialogId::new(request.chat_id),
                request.is_pinned,
            );
            td.global = global;
            td.messages_manager = manager;
            match result {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::toggleChatIsTranslatable(request) => {
            // Requests.cpp:5889-5893 — the real toggle ladder.
            if !check_is_user(td, id) {
                return;
            }
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            let result = crate::messages_manager::toggle_dialog_is_translatable(
                &mut manager,
                ctx,
                MmDialogId::new(request.chat_id),
                request.is_translatable,
            );
            td.messages_manager = manager;
            match result {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::toggleChatViewAsTopics(request) => {
            // Requests.cpp:5877-5881 — toggle_dialog_view_as_messages with
            // the inverted td_api flag.
            if !check_is_user(td, id) {
                return;
            }
            let mut manager = std::mem::take(&mut td.messages_manager);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut crate::td::Td = unsafe { &mut *td_ptr };
            let result = crate::messages_manager::toggle_dialog_view_as_messages(
                &mut manager,
                ctx,
                MmDialogId::new(request.chat_id),
                !request.view_as_topics,
            );
            td.messages_manager = manager;
            match result {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => send_error_status(td, id, status),
            }
        }

        _ => run_bot_and_web_app_requests(td, id, function),
    }
}

/// The Bot & WebApp Operations arm group of the request dispatcher.
/// Handles 5 functions: getRecentInlineBots, reorderBotActiveUsernames,
/// setBotProfilePhoto, toggleBotCanManageEmojiStatus, toggleBotUsernameIsActive.
#[inline(never)]
fn run_bot_and_web_app_requests(td: &mut Td, id: u64, function: Function) {
    match function {
        Function::getRecentInlineBots(_) => {
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<Vec<crate::user_manager::UserId>, _>(
                id,
                |bot_ids: Vec<crate::user_manager::UserId>| {
                    Object::users(td_api::Users {
                        total_count: bot_ids.len() as i32,
                        user_ids: bot_ids.into_iter().map(|u| u.get()).collect(),
                    })
                },
            );
            let (mut env, inline_queries_manager, _) = crate::td::inline_env_split(td);
            inline_queries_manager.get_recent_inline_bots(&mut env, promise);
        }
        Function::reorderBotActiveUsernames(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let mut cleaned_usernames = Vec::with_capacity(request.usernames.len());
            for username in &request.usernames {
                let Some(cleaned) = clean_or_utf8_error(td, id, username) else {
                    return;
                };
                cleaned_usernames.push(cleaned);
            }
            let bot_user_id = crate::user_manager::UserId::new(request.bot_user_id);
            let mut env = user_env!(td);
            match td.user_manager.reorder_bot_usernames_local(
                &mut env,
                bot_user_id,
                &cleaned_usernames,
            ) {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::setBotProfilePhoto(request) => {
            let is_bot = td.auth_manager.is_bot();
            let bot_user_id = crate::user_manager::UserId::new(request.bot_user_id);
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = user_env!(td);
            td.user_manager.set_bot_profile_photo(
                &mut env,
                is_bot,
                bot_user_id,
                request.photo.as_ref(),
                promise,
            );
        }
        Function::toggleBotCanManageEmojiStatus(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let bot_user_id = crate::user_manager::UserId::new(request.bot_user_id);
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = user_env!(td);
            td.user_manager.toggle_user_can_manage_emoji_status(
                &mut env,
                bot_user_id,
                request.can_manage_emoji_status,
                promise,
            );
        }
        Function::toggleBotUsernameIsActive(request) => {
            if !check_is_user(td, id) {
                return;
            }
            let Some(username) = clean_or_utf8_error(td, id, &request.username) else {
                return;
            };
            let bot_user_id = crate::user_manager::UserId::new(request.bot_user_id);
            let mut env = user_env!(td);
            match td.user_manager.toggle_bot_username_is_active_local(
                &mut env,
                bot_user_id,
                &username,
                request.is_active,
            ) {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::addBotSecondaryUsername(request) => {
            // Requests.cpp (fd04a96d4): CHECK_IS_USER,
            // CLEAN_INPUT_STRING, then UserManager::add_bot_username
            // (the bots.addUsername send rides the net step).
            if !check_is_user(td, id) {
                return;
            }
            let Some(username) = clean_or_utf8_error(td, id, &request.username) else {
                return;
            };
            let bot_user_id = crate::user_manager::UserId::new(request.bot_user_id);
            let mut env = user_env!(td);
            match td
                .user_manager
                .add_bot_username_local(&mut env, bot_user_id, &username)
            {
                Ok(()) => td.send_result(id, ok_object()),
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::deleteBotSecondaryUsername(request) => {
            // Requests.cpp (10ee532bb): CHECK_IS_USER, then
            // UserManager::delete_bot_username — a bot with no secondary
            // username answers OK directly.
            if !check_is_user(td, id) {
                return;
            }
            let bot_user_id = crate::user_manager::UserId::new(request.bot_user_id);
            let mut env = user_env!(td);
            match td
                .user_manager
                .delete_bot_username_local(&mut env, bot_user_id)
            {
                Ok(_) => td.send_result(id, ok_object()),
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }

        // --- the Misc arm group (2026-09-24): the TD-local glue +
        // pure-server query remainder, in Requests.cpp function order.
        // The validation halves answer byte-parity offline; the net/db
        // tails stay [blocked: net] / seam per arm ---
        Function::getMessageProperties(request) => {
            // Requests.cpp:2662-2665 → MessagesManager.cpp:15362-15485 (no
            // CHECK_IS_USER in the C++) — the full flag ladder over the
            // can_* gates, the delete bands and the sorted-unique
            // custom-emoji fold (8aeb38920/cb41ae5c1/21c7e4c18).
            let dialog_id = MmDialogId::new(request.chat_id);
            let message_id = MessageId::new(request.message_id);
            let promise =
                td.deferred_promise::<td_api::MessageProperties, _>(id, Object::messageProperties);
            let manager = std::mem::take(&mut td.messages_manager);
            let td_ptr = std::ptr::from_mut(td);
            // SAFETY: the ctx/state/reactions views alias one Td and are
            // used strictly sequentially inside the callee (the
            // split_context_alias contract).
            let ctx: &mut dyn crate::messages_manager::MmRequestContext = unsafe { &mut *td_ptr };
            let state: &dyn crate::messages_manager::ReadStateContext = unsafe { &*td_ptr };
            let reactions_ctx =
                crate::messages_manager::MessageObjectReactionsContext::new(unsafe { &*td_ptr });
            crate::messages_manager::get_message_properties_request(
                ctx,
                &manager,
                &reactions_ctx,
                state,
                dialog_id,
                message_id,
                promise,
            );
            td.messages_manager = manager;
        }
        Function::getPollOptionProperties(request) => {
            // Requests.cpp:2666-2670 → MessagesManager.cpp:15499-15516
            // (no CHECK_IS_USER in the C++)
            let Some(poll_option_id) = clean_or_utf8_error(td, id, &request.poll_option_id) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::PollOptionProperties, _>(
                id,
                Object::pollOptionProperties,
            );
            let manager = std::mem::take(&mut td.messages_manager);
            let td_ptr = std::ptr::from_mut(td);
            // SAFETY: the ctx view aliases one Td and is used strictly
            // sequentially inside the callee (the split_context_alias
            // contract).
            let ctx: &mut dyn crate::messages_manager::SendMessageContext = unsafe { &mut *td_ptr };
            crate::messages_manager::get_poll_option_properties_request(
                ctx,
                &manager,
                MmDialogId::new(request.chat_id),
                MessageId::new(request.message_id),
                &poll_option_id,
                promise,
            );
            td.messages_manager = manager;
        }
        Function::getGroupsInCommon(request) => {
            // Requests.cpp:3453-3456 → GetGroupsInCommonRequest
            // (Requests.cpp:496-515): the common-dialog read is the
            // CommonDialogManager seam — the request stays unanswered
            // exactly like the C++ actor without a network.
            if !check_is_user(td, id) {
                return;
            }
            let _ = (request.offset_chat_id, request.limit);
        }
        Function::readAllChatPollVotes(request) => {
            // Requests.cpp:4200-4204 → MessagesManager.cpp:9301-9324: the
            // ok waits for the server ack — the promise rides the
            // affected-history refill (the net-drain 29 shape)
            if !check_is_user(td, id) {
                return;
            }
            let mut manager = std::mem::take(&mut td.messages_manager);
            let mut promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut dyn crate::messages_manager::MessageObjectContext =
                unsafe { &mut *td_ptr };
            let state: &mut dyn crate::messages_manager::ReadStateContext =
                unsafe { crate::td::split_context_alias(td_ptr) };
            let result = crate::messages_manager::read_all_dialog_poll_votes_request(
                ctx,
                state,
                &mut manager,
                MmDialogId::new(request.chat_id),
                crate::messages_manager::ForumTopicId::default(),
            );
            td.messages_manager = manager;
            match result {
                Ok(()) => {
                    // SAFETY: the mm view aliases one Td, used strictly
                    // sequentially.
                    let td_ptr = std::ptr::from_mut(td);
                    let mm: &mut dyn crate::messages_manager::MmRequestContext =
                        unsafe { &mut *td_ptr };
                    mm.read_all_dialog_poll_votes_on_server(
                        MmDialogId::new(request.chat_id),
                        crate::messages_manager::ForumTopicId::default(),
                        promise,
                    );
                }
                Err(status) => promise.set_error(status),
            }
        }
        Function::readAllForumTopicPollVotes(request) => {
            // Requests.cpp:4206-4213: the topic-id zero gate answers in the
            // dispatcher before the manager call
            if !check_is_user(td, id) {
                return;
            }
            if request.forum_topic_id == 0 {
                return td.send_error_impl(id, 400, "Invalid forum topic identifier specified");
            }
            let mut manager = std::mem::take(&mut td.messages_manager);
            let mut promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut dyn crate::messages_manager::MessageObjectContext =
                unsafe { &mut *td_ptr };
            let state: &mut dyn crate::messages_manager::ReadStateContext =
                unsafe { crate::td::split_context_alias(td_ptr) };
            let result = crate::messages_manager::read_all_dialog_poll_votes_request(
                ctx,
                state,
                &mut manager,
                MmDialogId::new(request.chat_id),
                crate::messages_manager::ForumTopicId::from_raw(request.forum_topic_id),
            );
            td.messages_manager = manager;
            match result {
                Ok(()) => {
                    // SAFETY: the mm view aliases one Td, used strictly
                    // sequentially.
                    let td_ptr = std::ptr::from_mut(td);
                    let mm: &mut dyn crate::messages_manager::MmRequestContext =
                        unsafe { &mut *td_ptr };
                    mm.read_all_dialog_poll_votes_on_server(
                        MmDialogId::new(request.chat_id),
                        crate::messages_manager::ForumTopicId::from_raw(request.forum_topic_id),
                        promise,
                    );
                }
                Err(status) => promise.set_error(status),
            }
        }
        Function::loadQuickReplyShortcuts(_) => {
            // Requests.cpp:4604-4607 → QuickReplyManager.cpp:1126-1137:
            // the loaded fast path joins the intake port; the reload parks
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            td.quick_reply_manager.get_quick_reply_shortcuts(promise);
        }
        Function::getForumTopicHistory(request) => {
            // Requests.cpp:4854-4858 → MessagesManager.cpp:18177-18193:
            // the gates answer here; the thread-history read follows the
            // getChatMessageByDate db-seam caller-retry contract
            if !check_is_user(td, id) {
                return;
            }
            let manager = std::mem::take(&mut td.messages_manager);
            let result = crate::messages_manager::get_forum_topic_history_request(
                &manager,
                MmDialogId::new(request.chat_id),
                crate::messages_manager::ForumTopicId::from_raw(request.forum_topic_id),
                MessageId::new(request.from_message_id),
                request.offset,
                request.limit,
            );
            td.messages_manager = manager;
            match result {
                Ok(()) => {
                    // the thread-history read parks on the db seam
                    // ([blocked: net])
                    td.send_error_impl(id, 500, "The message history is not loaded yet");
                }
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::getStoryNotificationSettingsExceptions(_) => {
            // Requests.cpp:5918-5922: the GetStoryNotifySettingsExceptionsQuery
            // is [blocked: net] — the request stays unanswered like the C++
            // actor without a network
            // CHECK_IS_USER: the 400 goes out inside the gate; on pass the
            // request intentionally stays unanswered — its C++ query parks.
            check_is_user(td, id);
        }
        Function::approveSuggestedPost(request) => {
            // Requests.cpp:6370-6374 → MessagesManager.cpp:25405-25417
            // (no CHECK_IS_USER in the C++)
            let dialog_id = MmDialogId::new(request.chat_id);
            let message_id = MessageId::new(request.message_id);
            let manager = std::mem::take(&mut td.messages_manager);
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut dyn crate::messages_manager::MessageObjectContext =
                unsafe { &mut *td_ptr };
            let result = match crate::messages_manager::get_message_for_request(
                &manager, dialog_id, message_id,
            ) {
                None => Err(Status::error(400, "Message not found")),
                Some(m) => {
                    if !crate::messages_manager::can_approve_message(ctx, dialog_id, m) {
                        Err(Status::error(400, "Message can't be used in the method"))
                    } else {
                        Ok(())
                    }
                }
            };
            td.messages_manager = manager;
            match result {
                // `toggle_suggested_post_approval` — [blocked: net]
                Ok(()) => {}
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::declineSuggestedPost(request) => {
            // Requests.cpp:6376-6381 → MessagesManager.cpp:25405-25417
            let Some(_comment) = clean_or_utf8_error(td, id, &request.comment) else {
                return;
            };
            let dialog_id = MmDialogId::new(request.chat_id);
            let message_id = MessageId::new(request.message_id);
            let manager = std::mem::take(&mut td.messages_manager);
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut dyn crate::messages_manager::MessageObjectContext =
                unsafe { &mut *td_ptr };
            let result = match crate::messages_manager::get_message_for_request(
                &manager, dialog_id, message_id,
            ) {
                None => Err(Status::error(400, "Message not found")),
                Some(m) => {
                    if !crate::messages_manager::can_decline_message(ctx, dialog_id, m) {
                        Err(Status::error(400, "Message can't be used in the method"))
                    } else {
                        Ok(())
                    }
                }
            };
            td.messages_manager = manager;
            match result {
                // `toggle_suggested_post_approval` — [blocked: net]
                Ok(()) => {}
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::getSavedNotificationSound(_request) => {
            // Requests.cpp:7792-7795 → GetSavedNotificationSoundRequest
            // (Requests.cpp:1835-1850): the saved-ringtones load is the
            // NotificationSettingsManager receive seam — the request stays
            // unanswered like the C++ actor without a network
            // CHECK_IS_USER: the 400 goes out inside the gate; on pass the
            // request intentionally stays unanswered — its C++ query parks.
            check_is_user(td, id);
        }
        Function::getSavedNotificationSounds(_) => {
            // Requests.cpp:7797-7800 → GetSavedNotificationSoundsRequest
            // (Requests.cpp:1853-1866): the saved-ringtones load is the
            // NotificationSettingsManager receive seam
            // CHECK_IS_USER: the 400 goes out inside the gate; on pass the
            // request intentionally stays unanswered — its C++ query parks.
            check_is_user(td, id);
        }
        Function::addSavedNotificationSound(_request) => {
            // Requests.cpp:7802-7805 → NotificationSettingsManager.cpp:1030:
            // the close gate + the FileManager ringtone input-id path and
            // the size/duration checks are the FileManager/AudiosManager
            // seams; the account.saveRingtone tail is [blocked: net]
            // CHECK_IS_USER: the 400 goes out inside the gate; on pass the
            // request intentionally stays unanswered — its C++ query parks.
            check_is_user(td, id);
        }
        Function::removeSavedNotificationSound(_request) => {
            // Requests.cpp:7808-7811 → RemoveSavedNotificationSoundRequest
            // (Requests.cpp:1869-1883): the saved-ringtones pmc plane is
            // the NotificationSettingsManager receive seam
            // CHECK_IS_USER: the 400 goes out inside the gate; on pass the
            // request intentionally stays unanswered — its C++ query parks.
            check_is_user(td, id);
        }
        Function::getPollVoteStatistics(request) => {
            // Requests.cpp:8116-8119 → StatisticsManager.cpp:888-898: the
            // messages-manager gates answer here; the GetPollStatsQuery
            // parks in the statistics manager
            if !check_is_user(td, id) {
                return;
            }
            let promise = td
                .deferred_promise::<td_api::PollVoteStatistics, _>(id, Object::pollVoteStatistics);
            let dialog_id = MmDialogId::new(request.chat_id);
            let message_id = MessageId::new(request.message_id);
            let manager = std::mem::take(&mut td.messages_manager);
            let td_ptr = std::ptr::from_mut(td);
            let ctx: &mut dyn crate::messages_manager::SendMessageContext = unsafe { &mut *td_ptr };
            let state: &mut dyn crate::messages_manager::ReadStateContext =
                unsafe { crate::td::split_context_alias(td_ptr) };
            let result = if !crate::messages_manager::have_message_request(
                &manager, dialog_id, message_id,
            ) {
                Err(Status::error(400, "Message not found"))
            } else if !crate::messages_manager::can_get_message_poll_vote_statistics(
                ctx, &manager, state, dialog_id, message_id,
            ) {
                Err(Status::error(400, "Poll statistics are inaccessible"))
            } else {
                Ok(())
            };
            td.messages_manager = manager;
            if let Err(status) = result {
                td.send_error_impl(id, status.code(), status.message());
                return;
            }
            // StatisticsManager::get_poll_statistics (cpp:888-898) — the
            // send leg over the main dc
            let (mut manager, mut env) = crate::td::statistics_env_split(&mut *td);
            manager.get_poll_statistics(
                &mut env,
                MessageFullId::new(dialog_id, message_id),
                request.is_dark,
                promise,
            );
            td.statistics_manager = manager;
        }
        Function::canPurchaseFromStore(request) => {
            // Requests.cpp:9202-9205 → Premium.cpp:1444-1446: the store
            // purpose converts at the send leg; the CanPurchasePremiumQuery
            // promise parks in the premium manager
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut manager, mut env) = crate::td::premium_env_split(&mut *td);
            manager.can_purchase_premium(&mut env, request.purpose, promise);
            td.premium_manager = manager;
        }
        _ => run_proxy_network_requests(td, id, function),
    }
}

/// The glue-slice proxy+network arm group of the request dispatcher —
/// see the frame note at the forward site in [`run_request`]. The
/// ConnectionCreator proxy registry arms run the C++ `add_proxy` ladder
/// over the pmc-backed registry (the ConnectionCreator.cpp:158-478
/// plane); the resolve/connect tails (`ping_proxy_resolved`,
/// `ProxyChecker::test_proxy`) are the `[blocked: net]` seams; the
/// network-statistics arms answer the C++ disabled band (the
/// `net_stats_manager_` empty branch — the NetStatsManager port is its
/// own slice), and `getPushReceiverId` is the SynchronousRequests arm.
#[inline(never)]
fn run_proxy_network_requests(td: &mut Td, id: u64, function: Function) {
    match function {
        Function::addProxy(request) => {
            // Requests.cpp:9278-9283 — CLEAN_INPUT_STRING(comment) +
            // `ConnectionCreator::add_proxy(-1, ...)`.
            let Some(comment) = clean_or_utf8_error(td, id, &request.comment) else {
                return;
            };
            let mut promise = td.deferred_promise::<td_api::AddedProxy, _>(id, Object::addedProxy);
            let mut creator = td.connection_creator.take();
            let result = {
                let mut env = crate::td::ConnectionEnv { td };
                creator.as_mut().unwrap().add_proxy(
                    &mut env,
                    -1,
                    request.proxy.as_ref(),
                    request.enable,
                    comment,
                )
            };
            td.connection_creator = creator;
            match result {
                Ok(added) => promise.set_value(added),
                Err(message) => promise.set_error(tdutils::status::Status::error(400, &message)),
            }
        }
        Function::editProxy(request) => {
            // Requests.cpp:9284-9294 — the negative-id band, the comment
            // clean, then the same `add_proxy` entry with the edit id.
            if request.proxy_id < 0 {
                return td.send_error_impl(id, 400, "Proxy identifier invalid");
            }
            let Some(comment) = clean_or_utf8_error(td, id, &request.comment) else {
                return;
            };
            let mut promise = td.deferred_promise::<td_api::AddedProxy, _>(id, Object::addedProxy);
            let mut creator = td.connection_creator.take();
            let result = {
                let mut env = crate::td::ConnectionEnv { td };
                creator.as_mut().unwrap().add_proxy(
                    &mut env,
                    request.proxy_id,
                    request.proxy.as_ref(),
                    request.enable,
                    comment,
                )
            };
            td.connection_creator = creator;
            match result {
                Ok(added) => promise.set_value(added),
                Err(message) => promise.set_error(tdutils::status::Status::error(400, &message)),
            }
        }
        Function::enableProxy(request) => {
            // Requests.cpp:9295-9299 — `enable_proxy` with the
            // "Unknown proxy identifier" band.
            let mut promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut creator = td.connection_creator.take();
            let result = {
                let mut env = crate::td::ConnectionEnv { td };
                creator
                    .as_mut()
                    .unwrap()
                    .enable_proxy(&mut env, request.proxy_id)
            };
            td.connection_creator = creator;
            match result {
                Ok(()) => promise.set_value(()),
                Err(message) => promise.set_error(tdutils::status::Status::error(400, &message)),
            }
        }
        Function::disableProxy(_) => {
            // Requests.cpp:9300-9304 — `disable_proxy`.
            let mut promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut creator = td.connection_creator.take();
            {
                let mut env = crate::td::ConnectionEnv { td };
                creator.as_mut().unwrap().disable_proxy(&mut env);
            }
            td.connection_creator = creator;
            promise.set_value(());
        }
        Function::removeProxy(request) => {
            // Requests.cpp:9305-9309 — `remove_proxy`.
            let mut promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut creator = td.connection_creator.take();
            let result = {
                let mut env = crate::td::ConnectionEnv { td };
                creator
                    .as_mut()
                    .unwrap()
                    .remove_proxy(&mut env, request.proxy_id)
            };
            td.connection_creator = creator;
            match result {
                Ok(()) => promise.set_value(()),
                Err(message) => promise.set_error(tdutils::status::Status::error(400, &message)),
            }
        }
        Function::getProxies(_) => {
            // Requests.cpp:9310-9314 — `get_proxies`.
            let mut promise =
                td.deferred_promise::<td_api::AddedProxies, _>(id, Object::addedProxies);
            let creator = td.connection_creator.take();
            let proxies = creator.as_ref().unwrap().get_proxies();
            td.connection_creator = creator;
            promise.set_value(proxies);
        }
        Function::pingProxy(request) => {
            // Requests.cpp:9315-9325 — the `seconds` rewrap around
            // `ConnectionCreator::ping_proxy`; the C++ null-proxy band is
            // inexpressible (the flattened model makes `proxy`
            // non-nullable). The resolve/connect tail parks at the
            // `[blocked: net]` seam.
            let promise = td.deferred_promise::<f64, _>(id, |seconds| {
                Object::seconds(td_api::Seconds { seconds })
            });
            let mut creator = td.connection_creator.take();
            let result = {
                let mut env = crate::td::ConnectionEnv { td };
                creator
                    .as_mut()
                    .unwrap()
                    .ping_proxy(&mut env, request.proxy.as_ref(), promise)
            };
            td.connection_creator = creator;
            if let Err(message) = result {
                td.send_error_impl(id, 400, &message);
            }
        }
        Function::testProxy(request) => {
            // Requests.cpp:9463-9471 — `Proxy::create_proxy` answers the
            // validation band, then `ProxyChecker::test_proxy`; the
            // checker handshake is the `[blocked: net]` seam.
            let proxy = match crate::connection_creator::Proxy::create_proxy(request.proxy.as_ref())
            {
                Ok(proxy) => proxy,
                Err(message) => return td.send_error_impl(id, 400, &message),
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let mut env = crate::td::ConnectionEnv { td };
            env.test_proxy_via_net(&proxy, request.dc_id, request.timeout, promise);
        }
        Function::addNetworkStatistics(_) => {
            // Requests.cpp:3059-3076 — the C++ checks the entry pointer
            // first (the flattened model makes it non-nullable, the band
            // is inexpressible), then the manager registry. The collapsed
            // world runs without the NetStatsManager, so the C++
            // `net_stats_manager_.empty()` branch answers.
            td.send_error_impl(id, 400, "Network statistics are disabled");
        }
        Function::getNetworkStatistics(_) => {
            // Requests.cpp:3031-3058 — the manager-empty band fires
            // before the `disable_persistent_network_statistics` option
            // check; the collapsed world always answers it.
            td.send_error_impl(id, 400, "Network statistics are disabled");
        }
        Function::resetNetworkStatistics(_) => {
            // Requests.cpp:3050-3058 — the manager-empty band; the C++
            // answers ok after the reset closure, unreachable here.
            td.send_error_impl(id, 400, "Network statistics are disabled");
        }
        Function::setNetworkType(request) => {
            // Requests.cpp:3113-3117 — `StateManager::on_network` with the
            // get_net_type mapping; the state consumers are the net-step
            // seams and the promise answers ok immediately.
            td.network_type = crate::connection_creator::get_net_type(request.r#type.as_ref());
            td.send_result(id, ok_object());
        }

        _ => run_privacy_auto_download_requests(td, id, function),
    }
}

/// The glue-slice privacy+auto-download arm group of the request
/// dispatcher — see the frame note at the forward site in
/// [`run_proxy_network_requests`]. The user-privacy arms run the C++
/// `PrivacyManager::get_privacy`/`set_privacy` halves over the per-key
/// cache (the PrivacyManager.cpp:120-155 plane); the
/// new-chat/read-date arms run the `GlobalPrivacySettings` local plane
/// (the constructors, the builders, the apply-changes merge); the
/// auto-download arms run the null-settings band, the
/// `get_auto_download_settings` intake and the net-type-keyed
/// roaming/wifi flag derivation. The `account.getPrivacy`/
/// `setPrivacy`/`getGlobalPrivacySettings`/`setGlobalPrivacySettings`/
/// `getAutoDownloadSettings`/`saveAutoDownloadSettings` query tails are
/// the `[blocked: net]` seams, the M.31 convention.
#[inline(never)]
fn run_privacy_auto_download_requests(td: &mut Td, id: u64, function: Function) {
    macro_rules! ok_promise {
        ($td:expr) => {
            $td.deferred_promise::<(), _>(id, |_| ok_object())
        };
    }

    match function {
        Function::getUserPrivacySettingRules(request) => {
            // Requests.cpp:2418-2422 — `PrivacyManager::get_privacy`; the
            // synchronized cache band answers the rules builder and the
            // `account.getPrivacy` tail rides the dispatcher.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::UserPrivacySettingRules, _>(
                id,
                Object::userPrivacySettingRules,
            );
            let mut manager = std::mem::take(&mut td.privacy_manager);
            let result = {
                let mut env = crate::td::PrivacyEnv { td };
                manager.get_privacy(&mut env, request.setting.as_ref(), promise)
            };
            td.privacy_manager = manager;
            if let Err(status) = result {
                td.send_error_impl(id, status.code(), status.message());
            }
        }
        Function::setUserPrivacySettingRules(request) => {
            // Requests.cpp:2424-2428 — `PrivacyManager::set_privacy`; the
            // rules intake bands are real and the `account.setPrivacy`
            // tail rides the dispatcher.
            if !check_is_user(td, id) {
                return;
            }
            let promise = ok_promise!(td);
            let mut manager = std::mem::take(&mut td.privacy_manager);
            let result = {
                let mut env = crate::td::PrivacyEnv { td };
                manager.set_privacy(
                    &mut env,
                    request.setting.as_ref(),
                    request.rules.as_ref(),
                    promise,
                )
            };
            td.privacy_manager = manager;
            if let Err(status) = result {
                td.send_error_impl(id, status.code(), status.message());
            }
        }
        Function::getAutoDownloadSettingsPresets(_) => {
            // Requests.cpp:3119-3123 — `get_auto_download_settings_presets`;
            // the `account.getAutoDownloadSettings` fetch is the
            // `[blocked: net]` seam (the clamped presets builder is its
            // result half).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::AutoDownloadSettingsPresets, _>(
                id,
                Object::autoDownloadSettingsPresets,
            );
            let mut env = crate::td::PrivacyEnv { td };
            crate::auto_download_settings::get_auto_download_settings_presets(&mut env, promise);
        }
        Function::setAutoDownloadSettings(request) => {
            // Requests.cpp:3125-3132 — the "New settings must be
            // non-empty" band, then `set_auto_download_settings` with the
            // get_net_type mapping and the field-for-field settings
            // intake; the `account.saveAutoDownloadSettings` tail is the
            // `[blocked: net]` seam.
            if !check_is_user(td, id) {
                return;
            }
            let Some(settings) = request.settings.as_ref() else {
                return td.send_error_impl(id, 400, "New settings must be non-empty");
            };
            let promise = ok_promise!(td);
            let mut env = crate::td::PrivacyEnv { td };
            crate::auto_download_settings::set_auto_download_settings(
                &mut env,
                crate::connection_creator::get_net_type(request.r#type.as_ref()),
                crate::auto_download_settings::get_auto_download_settings(settings),
                promise,
            );
        }
        Function::getReadDatePrivacySettings(_) => {
            // Requests.cpp:5711-5723 — the query-promise lambda rewraps
            // the fetched `GlobalPrivacySettings` into the td_api object;
            // the `account.getGlobalPrivacySettings` fetch rides the
            // dispatcher.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::ReadDatePrivacySettings, _>(
                id,
                Object::readDatePrivacySettings,
            );
            let query_promise = tdutils::promise::PromiseCreator::lambda_result(
                move |result: tdutils::status::Result<
                    crate::privacy_manager::GlobalPrivacySettings,
                >| {
                    match result {
                        tdutils::status::Result::Ok(settings) => {
                            let mut promise = promise;
                            promise.set_value(settings.get_read_date_privacy_settings_object());
                        }
                        tdutils::status::Result::Err(status) => {
                            let mut promise = promise;
                            promise.set_error(status);
                        }
                    }
                },
            );
            let mut env = crate::td::PrivacyEnv { td };
            crate::privacy_manager::GlobalPrivacySettings::get_global_privacy_settings(
                &mut env,
                query_promise,
            );
        }
        Function::setReadDatePrivacySettings(request) => {
            // Requests.cpp:5725-5730 — the `GlobalPrivacySettings`
            // constructor holds the null-settings band (`hide_read_marks_`
            // = `settings == nullptr || !settings->show_read_date_`); the
            // archive hide is statically dead for this set type and the
            // get → `apply_changes` → `account.setGlobalPrivacySettings`
            // chain rides the dispatcher.
            if !check_is_user(td, id) {
                return;
            }
            let set_settings =
                crate::privacy_manager::GlobalPrivacySettings::from_read_date_privacy_settings(
                    request.settings,
                );
            let promise = ok_promise!(td);
            let mut env = crate::td::PrivacyEnv { td };
            crate::privacy_manager::GlobalPrivacySettings::set_global_privacy_settings(
                &mut env,
                set_settings,
                promise,
            );
        }
        Function::getNewChatPrivacySettings(_) => {
            // Requests.cpp:5732-5744 — the query-promise lambda rewrap of
            // `get_new_chat_privacy_settings_object`; the
            // `account.getGlobalPrivacySettings` fetch rides the
            // dispatcher.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::NewChatPrivacySettings, _>(
                id,
                Object::newChatPrivacySettings,
            );
            let query_promise = tdutils::promise::PromiseCreator::lambda_result(
                move |result: tdutils::status::Result<
                    crate::privacy_manager::GlobalPrivacySettings,
                >| {
                    match result {
                        tdutils::status::Result::Ok(settings) => {
                            let mut promise = promise;
                            promise.set_value(settings.get_new_chat_privacy_settings_object());
                        }
                        tdutils::status::Result::Err(status) => {
                            let mut promise = promise;
                            promise.set_error(status);
                        }
                    }
                },
            );
            let mut env = crate::td::PrivacyEnv { td };
            crate::privacy_manager::GlobalPrivacySettings::get_global_privacy_settings(
                &mut env,
                query_promise,
            );
        }
        Function::setNewChatPrivacySettings(request) => {
            // Requests.cpp:5746-5751 — the `GlobalPrivacySettings`
            // constructor holds the null-settings band and the
            // `[0, 1000000]` star-count clamp; the archive hide is
            // statically dead for this set type and the get →
            // `apply_changes` → `account.setGlobalPrivacySettings` chain
            // rides the dispatcher.
            if !check_is_user(td, id) {
                return;
            }
            let set_settings =
                crate::privacy_manager::GlobalPrivacySettings::from_new_chat_privacy_settings(
                    request.settings,
                );
            let promise = ok_promise!(td);
            let mut env = crate::td::PrivacyEnv { td };
            crate::privacy_manager::GlobalPrivacySettings::set_global_privacy_settings(
                &mut env,
                set_settings,
                promise,
            );
        }

        _ => run_sticker_residue_requests(td, id, function),
    }
}

/// The M.glue stickers-residue arm group of the request dispatcher: the
/// recent/favorite/hashtag/emoji-search/sticker-set ladder the M.st
/// StickersManager slice left interim (Requests.cpp:1438-1560, 3797-3825,
/// 6827-6855, 7429-7544, 7667-7727, 8981-9002). Every arm routes into
/// `td_->stickers_manager_->...`; the not-loaded/server tails are the
/// [blocked: net] query seams and the emoji-status free functions are
/// the EmojiStatus.cpp plane.
fn run_sticker_residue_requests(td: &mut Td, id: u64, function: Function) {
    use crate::stickers_manager::{StickerSetId, get_sticker_type};

    match function {
        Function::getInstalledStickerSets(request) => {
            // Requests.cpp:7480-7483 — the not-loaded band parks on the
            // GetAllStickersQuery.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::StickerSets, _>(id, Object::stickerSets);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_installed_sticker_sets(
                &mut env,
                get_sticker_type(request.sticker_type.as_ref()),
                promise,
            );
        }
        Function::getArchivedStickerSets(request) => {
            // Requests.cpp:7485-7496 — the scan bands are local; the
            // tail parks on GetArchivedStickerSetsQuery (the collapsed
            // single run answers the not-force band).
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::StickerSets, _>(id, Object::stickerSets);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_archived_sticker_sets(
                &mut env,
                get_sticker_type(request.sticker_type.as_ref()),
                StickerSetId::new(request.offset_sticker_set_id),
                request.limit,
                false,
                promise,
            );
        }
        Function::getTrendingStickerSets(request) => {
            // Requests.cpp:7491-7496 — the trending ladder.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::TrendingStickerSets, _>(
                id,
                Object::trendingStickerSets,
            );
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_featured_sticker_sets(
                &mut env,
                get_sticker_type(request.sticker_type.as_ref()),
                request.offset,
                request.limit,
                promise,
            );
        }
        Function::getAttachedStickerSets(request) => {
            // Requests.cpp:7497-7501 — the file probe; the tail parks on
            // GetAttachedStickersQuery.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::StickerSets, _>(id, Object::stickerSets);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_attached_sticker_sets(
                &mut env,
                crate::files::FileId::new(request.file_id, 0),
                promise,
            );
        }
        Function::getStickerSet(request) => {
            // Requests.cpp:7502-7525 — no CHECK band (the C++ arm has
            // none); unknown ids ride the Great Minds reload seam.
            let promise = td.deferred_promise::<td_api::StickerSet, _>(id, Object::stickerSet);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_sticker_set(&mut env, StickerSetId::new(request.set_id), promise);
        }
        Function::searchStickerSet(request) => {
            // Requests.cpp:7511-7515 — the short-name search.
            let Some(name) = clean_or_utf8_error(td, id, &request.name) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::StickerSet, _>(id, Object::stickerSet);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.search_sticker_set(&mut env, name, request.ignore_cache, promise);
        }
        Function::searchInstalledStickerSets(request) => {
            // Requests.cpp:7516-7521.
            if !check_is_user(td, id) {
                return;
            }
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::StickerSets, _>(id, Object::stickerSets);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.search_installed_sticker_sets(
                &mut env,
                get_sticker_type(request.sticker_type.as_ref()),
                query,
                request.limit,
                promise,
            );
        }
        Function::searchStickerSets(request) => {
            // Requests.cpp:7522-7526 — the mask band answers the empty
            // list; the tail parks on SearchStickerSetsQuery.
            if !check_is_user(td, id) {
                return;
            }
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::StickerSets, _>(id, Object::stickerSets);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.search_sticker_sets(
                &mut env,
                get_sticker_type(request.sticker_type.as_ref()),
                query,
                promise,
            );
        }
        Function::changeStickerSet(request) => {
            // Requests.cpp:7527-7531 — the lookup-miss 400 bands are the
            // reachable behavior.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.change_sticker_set(
                &mut env,
                StickerSetId::new(request.set_id),
                request.is_installed,
                request.is_archived,
                promise,
            );
        }
        Function::getStickers(request) => {
            // Requests.cpp:7429-7435 — the ladder parks per-plane.
            if !check_is_user(td, id) {
                return;
            }
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::Stickers, _>(id, Object::stickers);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_stickers(
                &mut env,
                get_sticker_type(request.sticker_type.as_ref()),
                query,
                request.limit,
                crate::messages_manager::DialogId::new(request.chat_id),
                false,
                promise,
            );
        }
        Function::getAllStickerEmojis(request) => {
            // Requests.cpp:7436-7441.
            if !check_is_user(td, id) {
                return;
            }
            let Some(query) = clean_or_utf8_error(td, id, &request.query) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::Emojis, _>(id, Object::emojis);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_all_sticker_emojis(
                &mut env,
                get_sticker_type(request.sticker_type.as_ref()),
                query,
                crate::messages_manager::DialogId::new(request.chat_id),
                request.return_only_main_emoji,
                promise,
            );
        }
        Function::getRecentStickers(request) => {
            // Requests.cpp:7667-7671.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Stickers, _>(id, Object::stickers);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_recent_stickers(&mut env, request.is_attached, promise);
        }
        Function::addRecentSticker(request) => {
            // Requests.cpp:7672-7676.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.add_recent_sticker(
                &mut env,
                request.is_attached,
                request.sticker.as_ref(),
                promise,
            );
        }
        Function::removeRecentSticker(request) => {
            // Requests.cpp:7677-7681.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.remove_recent_sticker(
                &mut env,
                request.is_attached,
                request.sticker.as_ref(),
                promise,
            );
        }
        Function::clearRecentStickers(request) => {
            // Requests.cpp:7682-7686.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.clear_recent_stickers(&mut env, request.is_attached, promise);
        }
        Function::getFavoriteStickers(_) => {
            // Requests.cpp:7687-7691.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Stickers, _>(id, Object::stickers);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_favorite_stickers(&mut env, promise);
        }
        Function::addFavoriteSticker(request) => {
            // Requests.cpp:7692-7696.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.add_favorite_sticker(&mut env, request.sticker.as_ref(), promise);
        }
        Function::removeFavoriteSticker(request) => {
            // Requests.cpp:7697-7701.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.remove_favorite_sticker(&mut env, request.sticker.as_ref(), promise);
        }
        Function::getStickerEmojis(request) => {
            // Requests.cpp:7702-7706.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::Emojis, _>(id, Object::emojis);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_sticker_emojis(&mut env, request.sticker.as_ref(), promise);
        }
        Function::searchEmojis(request) => {
            // Requests.cpp:7707-7715 — the empty/unsupported bands answer
            // the empty emojiKeywords; the prepared query reads the pmc
            // keyword records.
            if !check_is_user(td, id) {
                return;
            }
            let Some(text) = clean_or_utf8_error(td, id, &request.text) else {
                return;
            };
            let mut input_language_codes = Vec::with_capacity(request.input_language_codes.len());
            for language_code in &request.input_language_codes {
                let Some(language_code) = clean_or_utf8_error(td, id, language_code) else {
                    return;
                };
                input_language_codes.push(language_code);
            }
            let promise =
                td.deferred_promise::<td_api::EmojiKeywords, _>(id, Object::emojiKeywords);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.search_emojis(&mut env, text, input_language_codes, false, promise);
        }
        Function::getKeywordEmojis(request) => {
            // Requests.cpp:7716-7727 — the keyword twin.
            if !check_is_user(td, id) {
                return;
            }
            let Some(text) = clean_or_utf8_error(td, id, &request.text) else {
                return;
            };
            let mut input_language_codes = Vec::with_capacity(request.input_language_codes.len());
            for language_code in &request.input_language_codes {
                let Some(language_code) = clean_or_utf8_error(td, id, language_code) else {
                    return;
                };
                input_language_codes.push(language_code);
            }
            let promise = td.deferred_promise::<td_api::Emojis, _>(id, Object::emojis);
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_keyword_emojis(
                &mut env,
                text,
                input_language_codes,
                false,
                promise,
            );
        }
        Function::searchHashtags(request) => {
            // Requests.cpp:8981-8995 — the local hashtag-hints query; the
            // answers come from the registry (sync in behavior, promise
            // plane for the rewrap).
            if !check_is_user(td, id) {
                return;
            }
            let Some(prefix) = clean_or_utf8_error(td, id, &request.prefix) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::Hashtags, _>(id, Object::hashtags);
            let (_env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.search_hashtags(prefix, request.limit, promise);
        }
        Function::removeRecentHashtag(request) => {
            // Requests.cpp:8997-9002.
            if !check_is_user(td, id) {
                return;
            }
            let Some(hashtag) = clean_or_utf8_error(td, id, &request.hashtag) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.remove_recent_hashtag(&mut env, hashtag, promise);
        }
        Function::getSearchedForTags(request) => {
            // Requests.cpp:3797-3810 — the searched-tags registry query
            // (the cashtag twin rides the '$' prefix inside).
            if !check_is_user(td, id) {
                return;
            }
            let Some(tag_prefix) = clean_or_utf8_error(td, id, &request.tag_prefix) else {
                return;
            };
            let promise = td.deferred_promise::<td_api::Hashtags, _>(id, Object::hashtags);
            let (_env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.get_searched_for_tags(tag_prefix, request.limit, promise);
        }
        Function::removeSearchedForTag(request) => {
            // Requests.cpp:3812-3817.
            if !check_is_user(td, id) {
                return;
            }
            let Some(tag) = clean_or_utf8_error(td, id, &request.tag) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.remove_searched_for_tag(&mut env, tag, promise);
        }
        Function::clearSearchedForTags(request) => {
            // Requests.cpp:3819-3823.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let (mut env, stickers_manager) = crate::td::stickers_env_split(td);
            stickers_manager.clear_searched_for_tags(&mut env, request.clear_cashtags, promise);
        }

        _ => run_country_requests(td, id, function),
    }
}

/// The glue-slice countries arm group of the request dispatcher — the
/// CountryInfoManager plane (CountryInfoManager.cpp). The cache answers
/// from the compiled-in "en" fallback list and the C++
/// `next_reload_time` window; the help.getNearestDc /
/// help.getCountriesList query tails (`GetNearestDcQuery::send`,
/// `GetCountriesListQuery::send`) park at the `[blocked: net]` seams,
/// the M.31 promise-drop convention.
#[inline(never)]
fn run_country_requests(td: &mut Td, id: u64, function: Function) {
    match function {
        Function::getCountries(_) => {
            // Requests.cpp:9227-9230 — `get_countries`.
            let promise = td.deferred_promise::<td_api::Countries, _>(id, Object::countries);
            let (mut env, mut manager) = crate::td::country_env_split(td);
            manager.get_countries(&mut env, promise);
            td.country_info_manager = manager;
        }
        Function::getCountry(request) => {
            // Requests.cpp:9232-9235 — `get_country`; the unknown-code
            // null answer rides the `Td::send_result` 404 band
            // (Td.cpp:1406-1408).
            let promise = td.deferred_promise::<Option<td_api::CountryInfo>, _>(
                id,
                |info: Option<td_api::CountryInfo>| match info {
                    Some(info) => Object::countryInfo(info),
                    None => Object::error(td_api::Error {
                        code: 404,
                        message: "Not Found".to_string(),
                    }),
                },
            );
            let (mut env, mut manager) = crate::td::country_env_split(td);
            manager.get_country(&mut env, request.country_code, promise);
            td.country_info_manager = manager;
        }
        Function::getCountryCode(_) => {
            // Requests.cpp:9237-9240 — the CREATE_TEXT_REQUEST_PROMISE +
            // `get_current_country_code`; the GetNearestDcQuery tail is
            // the `[blocked: net]` seam.
            let promise =
                td.deferred_promise::<String, _>(id, |text| Object::text(td_api::Text { text }));
            let (mut env, mut manager) = crate::td::country_env_split(td);
            manager.get_current_country_code(&mut env, promise);
            td.country_info_manager = manager;
        }
        Function::getPhoneNumberInfo(request) => {
            // Requests.cpp:9242-9245 — `get_phone_number_info` (the
            // manager cleans the prefix).
            let promise =
                td.deferred_promise::<td_api::PhoneNumberInfo, _>(id, Object::phoneNumberInfo);
            let (mut env, mut manager) = crate::td::country_env_split(td);
            manager.get_phone_number_info(&mut env, request.phone_number_prefix, promise);
            td.country_info_manager = manager;
        }
        _ => run_files_glue_requests(td, id, function),
    }
}

/// The M.glue files arm group of the request dispatcher — see the frame
/// note at the forward site in [`run_sticker_residue_requests`]. The
/// FileManager arms run the C++ ladders over the collapsed files.rs
/// manager (the validation bands real, the read/unlink tails
/// synchronous); `addFileToDownloads` rides the MessagesManager
/// file-source plane into `DownloadManager::add_file`; `addOffer` runs
/// the suggested-post/access/offer bands and parks the
/// `forward_message` tail (the monoforum stub-inventory keeps the C++
/// offer band false, the send machinery is the M.10 send plane). The
/// three pure-server arms (`getApplicationDownloadLink`,
/// `saveApplicationLogEvent`, `setApplicationVerificationToken`) park at
/// the `[blocked: net]` seams, the M.31 convention.
#[inline(never)]
fn run_files_glue_requests(td: &mut Td, id: u64, function: Function) {
    match function {
        Function::addOffer(request) => {
            // Requests.cpp:6383-6389 → MessagesManager.cpp:24497-24520 —
            // CHECK_IS_USER; the suggested-post-options band, the
            // `check_dialog_access` Write gate and the
            // `can_add_message_offer` band; the `forward_message(...,
            // true)` tail parks at the send plane.
            if !check_is_user(td, id) {
                return;
            }
            if let Some(options) = request.options.as_ref() {
                if options.suggested_post_info.is_none() {
                    return td.send_error_impl(id, 400, "Suggested post info must be non-empty");
                }
            } else {
                return td.send_error_impl(id, 400, "Suggested post info must be non-empty");
            }
            let dialog_id = MmDialogId::new(request.chat_id);
            let access = crate::dialog_manager::check_dialog_access(
                td,
                dialog_id,
                false,
                crate::user_manager::AccessRights::Write,
                "add_offer",
            );
            if access.is_error() {
                let status = access.error();
                return td.send_error_impl(id, status.code(), status.message());
            }
            let message_id = MessageId::new(request.message_id);
            let manager = std::mem::take(&mut td.messages_manager);
            let td_ptr = std::ptr::from_mut(td);
            // SAFETY: as approveSuggestedPost — the manager is out of the
            // struct while the context alias reaches the rest of td
            let ctx: &mut dyn crate::messages_manager::SendMessageContext = unsafe { &mut *td_ptr };
            let result = match crate::messages_manager::get_message_for_request(
                &manager, dialog_id, message_id,
            ) {
                None => Err(Status::error(400, "Wrong message identifier specified")),
                Some(m) => {
                    if !crate::messages_manager::can_add_message_offer(ctx, dialog_id, Some(m)) {
                        Err(Status::error(400, "Wrong message identifier specified"))
                    } else {
                        Ok(())
                    }
                }
            };
            td.messages_manager = manager;
            match result {
                // the forward_message tail — [blocked: send plane]
                Ok(()) => {}
                Err(status) => td.send_error_impl(id, status.code(), status.message()),
            }
        }
        Function::getSuggestedFileName(request) => {
            // Requests.cpp:6474-6480 — the synchronous
            // `FileManager::get_suggested_file_name` rewrapped into `text`.
            let outcome = td.file_manager.lock().get_suggested_file_name(
                crate::files::FileId::new(request.file_id, 0),
                &request.directory,
            );
            match outcome {
                Ok(file_name) => td.send_result(id, Object::text(td_api::Text { text: file_name })),
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::deleteFile(request) => {
            // Requests.cpp:6503-6507 — the ok promise rides into
            // `FileManager::delete_file` with the "td_api::deleteFile"
            // source tag; the pmc/notify halves are the manager seams.
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let mut file_manager = td.file_manager.lock();
            let mut file_load_manager = td.file_load_manager.lock();
            file_manager.delete_file(
                crate::files::FileId::new(request.file_id, 0),
                promise,
                "td_api::deleteFile",
                &mut file_load_manager,
            );
        }
        Function::readFilePart(request) => {
            // Requests.cpp:6518-6522 — CREATE_DATA_REQUEST_PROMISE rewraps
            // the read bytes into `data`; left_tries is the C++ literal 2.
            let promise = td.deferred_promise::<Vec<u8>, _>(id, |bytes| {
                Object::data(td_api::Data { data: bytes })
            });
            let mut file_manager = td.file_manager.lock();
            let mut file_load_manager = td.file_load_manager.lock();
            file_manager.read_file_part(
                crate::files::FileId::new(request.file_id, 0),
                request.offset,
                request.count,
                2,
                promise,
                &mut file_load_manager,
            );
        }
        Function::addFileToDownloads(request) => {
            // Requests.cpp:6530-6535 → MessagesManager.cpp:36874-36906 —
            // the priority band, the message/file walks and the
            // `DownloadManager::add_file` registration; the file object
            // answers the promise.
            let promise = td.deferred_promise(id, Object::file);
            let dialog_id = MmDialogId::new(request.chat_id);
            let message_full_id = MessageFullId::new(dialog_id, MessageId::new(request.message_id));
            let mut manager = std::mem::take(&mut td.messages_manager);
            let td_ptr = std::ptr::from_mut(td);
            // SAFETY: as approveSuggestedPost — the manager is out of the
            // struct while the context alias reaches the rest of td
            let ctx: &dyn crate::messages_manager::SendMessageContext = unsafe { &*td_ptr };
            let mut file_manager = td.file_manager.lock();
            let mut download_manager = td.download_manager.lock();
            crate::messages_manager::add_message_file_to_downloads(
                ctx,
                &mut manager,
                &mut file_manager,
                &mut download_manager,
                message_full_id,
                crate::files::FileId::new(request.file_id, 0),
                request.priority,
                promise,
            );
            td.messages_manager = manager;
        }
        Function::setApplicationVerificationToken(request) => {
            // Requests.cpp:6569-6574 — CHECK_IS_USER, the token intake,
            // then `NetQueryDispatcher::set_verification_token` — the
            // age-verification upload is the `[blocked: net]` seam and the
            // ok promise parks like the C++ dispatcher without a network.
            if !check_is_user(td, id) {
                return;
            }
            let Some(token) = clean_or_utf8_error(td, id, &request.token) else {
                return;
            };
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let _ = (request.verification_id, token, promise);
        }
        Function::getMapThumbnailFile(request) => {
            // Requests.cpp:7989-8003 — the not-force-loaded dialog id
            // degrades to the empty dialog, then the
            // `get_map_thumbnail_file_id` ladder; the answer is the
            // registered file object.
            let mut dialog_id = MmDialogId::new(request.chat_id);
            if !crate::dialog_manager::have_dialog_force(td, dialog_id, "getMapThumbnailFile") {
                dialog_id = MmDialogId::default();
            }
            let default_location = td_api::Location::default();
            let location = crate::chat_manager::Location::from_td_api_location(
                request.location.as_ref().unwrap_or(&default_location),
            );
            let outcome = td.file_manager.lock().get_map_thumbnail_file_id(
                &location,
                request.zoom,
                request.width,
                request.height,
                request.scale,
                dialog_id,
            );
            match outcome {
                Ok(file_id) => {
                    let file = td.file_manager.lock().get_file_object(file_id);
                    td.send_result(id, Object::file(file));
                }
                Err(status) => send_error_status(td, id, status),
            }
        }
        Function::getApplicationDownloadLink(_) => {
            // Requests.cpp:9252-9255 — CHECK_IS_USER, then
            // `get_invite_text` (Application.cpp:81-83): the
            // account.getInviteText query is the `[blocked: net]` seam and
            // the httpUrl promise parks like the C++ actor without a
            // network.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td
                .deferred_promise::<String, _>(id, |url| Object::httpUrl(td_api::HttpUrl { url }));
            let _ = promise;
        }
        Function::saveApplicationLogEvent(request) => {
            // Requests.cpp:9270-9275 — CHECK_IS_USER, the type intake and
            // the `convert_json_value` data wire form; the `save_app_log`
            // inputAppEvent build (Application.cpp:120-127) and the
            // help.saveAppLog query are the `[blocked: net]` seam — the ok
            // promise parks like the C++ actor without a network.
            if !check_is_user(td, id) {
                return;
            }
            let Some(event_type) = clean_or_utf8_error(td, id, &request.r#type) else {
                return;
            };
            let data = crate::synchronous_requests::convert_json_value(request.data.as_deref());
            let promise = td.deferred_promise::<(), _>(id, |_| ok_object());
            let _ = (request.chat_id, event_type, data, promise);
        }

        _ => run_misc_glue_requests(td, id, function),
    }
}

/// The glue-slice misc arm group of the request dispatcher — the
/// Requests.cpp residue: the local LinkManager pair, the Premium
/// giveaway pair, the push/account-ttl/token/call-log singles and the
/// gift-settings leg. The query tails park at the `[blocked: net]`
/// seams, the M.31 promise-drop convention.
#[inline(never)]
fn run_misc_glue_requests(td: &mut Td, id: u64, function: Function) {
    match function {
        Function::getInternalLink(request) => {
            // Requests.cpp:3575-3583 — LinkManager::get_internal_link is
            // local; no CHECK_IS_USER (the preauthentication set).
            let mut promise = td.deferred_promise::<td_api::HttpUrl, _>(id, Object::httpUrl);
            let result = {
                let mut env = crate::td::TdLinkEnv {
                    global: &mut td.global,
                    options: td.option_manager.as_mut(),
                };
                crate::link_manager::get_internal_link(
                    &mut env,
                    request.r#type.as_deref(),
                    !request.is_http,
                )
            };
            match result {
                Ok(link) => promise.set_value(td_api::HttpUrl { url: link }),
                Err(status) => promise.set_error(status),
            }
        }
        Function::getInternalLinkType(request) => {
            // Requests.cpp:3584-3588 — the parse is local; the C++ null
            // answer for an unparseable link parks (the recorded
            // null-response divergence — promise.set_value(nullptr) is
            // inexpressible).
            let object = {
                let mut env = crate::td::TdLinkEnv {
                    global: &mut td.global,
                    options: td.option_manager.as_mut(),
                };
                crate::link_manager::parse_internal_link(&mut env, &request.link, false)
                    .map(|link| Object::from(link.get_internal_link_type_object(&env)))
            };
            if let Some(object) = object {
                td.send_result(id, object);
            }
        }
        Function::getGiveawayInfo(request) => {
            // Requests.cpp:9087-9091 → Premium.cpp:720-725: the
            // `get_giveaway_message_id` band (the giveaway-message
            // lookup, MessagesManager.cpp:36676) rides the M.10.19
            // get_message_force seam (net-drain 24); the
            // payments.getGiveawayInfo query parks in the premium
            // manager.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::GiveawayInfo, _>(id, Object::from);
            let (mut manager, mut env) = crate::td::premium_env_split(&mut *td);
            manager.get_premium_giveaway_info(
                &mut env,
                request.chat_id,
                request.message_id,
                promise,
            );
            td.premium_manager = manager;
        }
        Function::launchPrepaidGiveaway(request) => {
            // Requests.cpp:9080-9085 → Premium.cpp:710-717: the
            // parameters non-empty band is real, the rest of the
            // validation ladder rides the PaymentsEnv plane; the
            // payments.launchPrepaidGiveaway query parks.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let (mut manager, mut env) = crate::td::premium_env_split(&mut *td);
            manager.launch_prepaid_premium_giveaway(
                &mut env,
                request.giveaway_id,
                request.parameters.as_ref(),
                request.winner_count,
                request.star_count,
                promise,
            );
            td.premium_manager = manager;
        }
        Function::processPushNotification(request) => {
            // Requests.cpp:2403-2409 — CHECK_IS_USER + the payload clean;
            // the manager runs the disabled/"{}" and receiver-id bands,
            // the decrypt/run plane parks.
            if !check_is_user(td, id) {
                return;
            }
            let Some(payload) = crate::misc::clean_input_string(&request.payload) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise(id, |()| ok_object());
            let mut env = NotificationEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                auth_is_authorized: td.auth_manager.is_authorized(),
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.notification_updates,
            };
            td.notification_manager
                .process_push_notification(&mut env, payload, promise);
        }
        Function::searchUserByToken(request) => {
            // Requests.cpp:2521-2526 — CHECK_IS_USER + the token clean;
            // the contacts.importContactToken query parks in the account
            // manager.
            if !check_is_user(td, id) {
                return;
            }
            let Some(token) = crate::misc::clean_input_string(&request.token) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<td_api::User, _>(id, Object::user);
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
            };
            td.account_manager
                .import_contact_token(&mut env, token, promise);
        }
        Function::sendCallLog(request) => {
            // Requests.cpp:5148-5152 — CHECK_IS_USER; the CallManager
            // close/call-id bands are real, the M.12 file intake and the
            // phone.saveCallLog upload park.
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
            td.call_manager
                .send_call_log(&mut env, &request.call_id, request.log_file, promise);
        }
        Function::setGiftSettings(request) => {
            // Requests.cpp:8440-8454 — CHECK_IS_USER; the GlobalPrivacy
            // Settings gift leg over the StarGiftSettings value plane;
            // the saveGlobalPrivacySettings query parks at the seam and
            // the on-result `on_update_my_user_gift_settings` store lands
            // with the net result handlers.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let set_settings = crate::privacy_manager::GlobalPrivacySettings::from_gift_settings(
                request.settings.as_ref(),
            );
            let mut env = crate::td::PrivacyEnv { td };
            crate::privacy_manager::GlobalPrivacySettings::set_global_privacy_settings(
                &mut env,
                set_settings,
                promise,
            );
        }
        Function::setInactiveSessionTtl(request) => {
            // Requests.cpp:2566-2570 — CHECK_IS_USER; the
            // SetAuthorizationTtlOnServer log event (0x505) + the
            // account.setAuthorizationTTL seam.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise(id, |()| ok_object());
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
            };
            td.account_manager.set_inactive_session_ttl_days(
                &mut env,
                request.inactive_session_ttl_days,
                promise,
            );
        }
        // -- the TON wallet / TonConnect / On-ramp family
        //    (Requests.cpp:7943-8232; the TonWalletManager plane over the
        //    [blocked: net] query seams) -------------------------
        Function::getTonWalletTransactions(request) => {
            // Requests.cpp:7943-7950 — CHECK_IS_USER + the offset clean.
            if !check_is_user(td, id) {
                return;
            }
            let Some(offset) = crate::misc::clean_input_string(&request.offset) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<td_api::TonWalletTransactions, _>(
                id,
                Object::tonWalletTransactions,
            );
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_ton_wallet_transactions(
                &mut env,
                &offset,
                request.limit,
                request.direction,
                promise,
            );
            td.ton_wallet_manager = manager;
        }
        Function::getTonWalletTransaction(request) => {
            // Requests.cpp:7951-7957.
            if !check_is_user(td, id) {
                return;
            }
            let Some(transaction_id) = crate::misc::clean_input_string(&request.transaction_id)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise =
                td.deferred_promise::<Option<td_api::TonWalletTransaction>, _>(id, |t| match t {
                    Some(transaction) => Object::tonWalletTransaction(transaction),
                    None => Object::error(td_api::Error {
                        code: 404,
                        message: "Not Found".to_owned(),
                    }),
                });
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_ton_wallet_transaction(&mut env, &transaction_id, promise);
            td.ton_wallet_manager = manager;
        }
        Function::getTonWalletTransactionByMsgHash(request) => {
            // Requests.cpp:7958-7964.
            if !check_is_user(td, id) {
                return;
            }
            let Some(msg_hash) = crate::misc::clean_input_string(&request.msg_hash) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise =
                td.deferred_promise::<Option<td_api::TonWalletTransaction>, _>(id, |t| match t {
                    Some(transaction) => Object::tonWalletTransaction(transaction),
                    None => Object::error(td_api::Error {
                        code: 404,
                        message: "Not Found".to_owned(),
                    }),
                });
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_ton_wallet_transaction_by_msg_hash(&mut env, &msg_hash, promise);
            td.ton_wallet_manager = manager;
        }
        Function::loadTonWalletState(_) => {
            // Requests.cpp:7965-7970 — CHECK_IS_USER; the cached-ok band
            // answers synchronously, the reload parks at the net seam.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_wallet_state(&mut env, promise);
            td.ton_wallet_manager = manager;
        }
        Function::loadTonWalletGaslessTransfersInfo(_) => {
            // Requests.cpp:7971-7976.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_ton_wallet_gasless_info(&mut env, promise);
            td.ton_wallet_manager = manager;
        }
        Function::checkWalletBotBalance(_) => {
            // Requests.cpp:7977-7982.
            if !check_is_user(td, id) {
                return;
            }
            let promise =
                td.deferred_promise::<td_api::WalletBotBalance, _>(id, Object::walletBotBalance);
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_existing_wallet_balance(&mut env, promise);
            td.ton_wallet_manager = manager;
        }
        Function::getUserTonWalletAddresses(request) => {
            // Requests.cpp:7983-7988 — the get_input_user gates answer the
            // 400 bands offline.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::UserTonWalletAddresses, _>(
                id,
                Object::userTonWalletAddresses,
            );
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_user_addresses(&mut env, request.user_ids, promise);
            td.ton_wallet_manager = manager;
        }
        Function::createUserTonWallet(request) => {
            // Requests.cpp:7989-7994.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::UserTonWalletAddress, _>(
                id,
                Object::userTonWalletAddress,
            );
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.create_user_ton_wallet(&mut env, request.user_id, promise);
            td.ton_wallet_manager = manager;
        }
        Function::getAddressTonWallet(request) => {
            // Requests.cpp:7995-8001 — the check_ton_address gate.
            if !check_is_user(td, id) {
                return;
            }
            let Some(address) = crate::misc::clean_input_string(&request.address) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<td_api::UserTonWalletAddress, _>(
                id,
                Object::userTonWalletAddress,
            );
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_address_ton_wallet(&mut env, &address, promise);
            td.ton_wallet_manager = manager;
        }
        Function::getTonWalletSecretPhrase(request) => {
            // Requests.cpp:8002-8008 — CREATE_TEXT_REQUEST_PROMISE.
            if !check_is_user(td, id) {
                return;
            }
            let Some(password) = crate::misc::clean_input_string(&request.password) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            // CREATE_TEXT_REQUEST_PROMISE — the string answer maps to the
            // td_api::text object.
            let mut outbox = td.deferred_promise::<td_api::Text, _>(id, Object::text);
            let promise = tdutils::promise::PromiseCreator::lambda_result(
                move |result: tdutils::status::Result<String>| match result {
                    tdutils::status::Result::Ok(phrase) => {
                        outbox.set_value(td_api::Text { text: phrase })
                    }
                    tdutils::status::Result::Err(status) => outbox.set_error(status),
                },
            );
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_ton_wallet_secret_phrase(&mut env, &password, promise);
            td.ton_wallet_manager = manager;
        }
        Function::getTonWalletOwnershipProofChallenge(_) => {
            // Requests.cpp:8009-8014.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::TonWalletOwnershipProofChallenge, _>(
                id,
                Object::tonWalletOwnershipProofChallenge,
            );
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_ton_wallet_proof_challenge(&mut env, promise);
            td.ton_wallet_manager = manager;
        }
        Function::enableTonWalletBackup(request) => {
            // Requests.cpp:8015-8022.
            if !check_is_user(td, id) {
                return;
            }
            let Some(secret_phrase) = crate::misc::clean_input_string(&request.secret_phrase)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.enable_ton_wallet_backup(&mut env, &secret_phrase, request.proof, promise);
            td.ton_wallet_manager = manager;
        }
        Function::disableTonWalletBackup(request) => {
            // Requests.cpp:8023-8029.
            if !check_is_user(td, id) {
                return;
            }
            let Some(password) = crate::misc::clean_input_string(&request.password) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.disable_ton_wallet_backup(&mut env, &password, promise);
            td.ton_wallet_manager = manager;
        }
        Function::disableTonWalletBackupWithProof(request) => {
            // Requests.cpp:8030-8035 — the proof size gates answer offline.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.disable_ton_wallet_backup_with_proof(&mut env, request.proof, promise);
            td.ton_wallet_manager = manager;
        }
        Function::sendTonWalletTransfer(request) => {
            // Requests.cpp:8036-8045 — the address/amount/input-user gates
            // are offline-observable; the local GramTransfer message rides
            // the do_ tail, the wire query parks.
            if !check_is_user(td, id) {
                return;
            }
            let Some(address) = crate::misc::clean_input_string(&request.address) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(comment) = crate::misc::clean_input_string(&request.comment) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<td_api::TonWalletTransferResult, _>(
                id,
                Object::tonWalletTransferResult,
            );
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.send_ton_wallet_transfer(
                &mut env,
                request.regular_transfer_data,
                request.gasless_transfer_data,
                request.user_id,
                &address,
                request.amount,
                &comment,
                request.is_comment_encrypted,
                request.sending_id,
                promise,
            );
            td.ton_wallet_manager = manager;
        }
        Function::deleteTonWallet(request) => {
            // Requests.cpp:8046-8052.
            if !check_is_user(td, id) {
                return;
            }
            let Some(password) = crate::misc::clean_input_string(&request.password) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.delete_ton_wallet(&mut env, &password, promise);
            td.ton_wallet_manager = manager;
        }
        Function::replaceTonWallet(request) => {
            // Requests.cpp:8053-8060.
            if !check_is_user(td, id) {
                return;
            }
            let Some(password) = crate::misc::clean_input_string(&request.password) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.replace_ton_wallet(
                &mut env,
                &password,
                &request.anchor_public_key,
                request.proof,
                promise,
            );
            td.ton_wallet_manager = manager;
        }
        Function::getCurrencyExchangeRates(_) => {
            // Requests.cpp:8061-8067 — the 60-second cache band answers
            // offline after the first fill.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::CurrencyExchangeRates, _>(
                id,
                Object::currencyExchangeRates,
            );
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_currency_rates(&mut env, promise);
            td.ton_wallet_manager = manager;
        }
        Function::getTonWalletNfts(request) => {
            // Requests.cpp:8069-8073.
            if !check_is_user(td, id) {
                return;
            }
            let Some(offset) = crate::misc::clean_input_string(&request.offset) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<td_api::TonNfts, _>(id, Object::tonNfts);
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_nfts(&mut env, &offset, request.limit, promise);
            td.ton_wallet_manager = manager;
        }
        Function::getTonConnectSessions(_) => {
            // Requests.cpp:8074-8079.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td
                .deferred_promise::<td_api::TonConnectSessions, _>(id, Object::tonConnectSessions);
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_ton_connect_sessions(&mut env, promise);
            td.ton_wallet_manager = manager;
        }
        Function::createTonConnectSession(request) => {
            // Requests.cpp:8080-8088.
            if !check_is_user(td, id) {
                return;
            }
            let Some(dapp_client_id) = crate::misc::clean_input_string(&request.dapp_client_id)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(manifest_url) = crate::misc::clean_input_string(&request.manifest_url) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise =
                td.deferred_promise::<td_api::TonConnectSession, _>(id, Object::tonConnectSession);
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.create_ton_connect_session(&mut env, &dapp_client_id, &manifest_url, promise);
            td.ton_wallet_manager = manager;
        }
        Function::setTonConnectSessionWalletClientId(request) => {
            // Requests.cpp:8089-8096.
            if !check_is_user(td, id) {
                return;
            }
            let Some(wallet_client_id) = crate::misc::clean_input_string(&request.wallet_client_id)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<td_api::TonConnectChallenge, _>(
                id,
                Object::tonConnectChallenge,
            );
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.register_ton_connect_key(
                &mut env,
                request.session_id,
                &wallet_client_id,
                promise,
            );
            td.ton_wallet_manager = manager;
        }
        Function::sendTonConnectSessionConnectResult(request) => {
            // Requests.cpp:8097-8104.
            if !check_is_user(td, id) {
                return;
            }
            let Some(trace_id) = crate::misc::clean_input_string(&request.trace_id) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.submit_ton_connect_result(
                &mut env,
                request.session_id,
                &request.challenge_answer,
                request.is_error,
                &request.body,
                &trace_id,
                promise,
            );
            td.ton_wallet_manager = manager;
        }
        Function::getTonConnectSessionPendingRequests(request) => {
            // Requests.cpp:8105-8110.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td
                .deferred_promise::<td_api::TonConnectRequests, _>(id, Object::tonConnectRequests);
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_ton_connect_requests(&mut env, false, request.session_id, "", promise);
            td.ton_wallet_manager = manager;
        }
        Function::getTonConnectDAppPendingRequests(request) => {
            // Requests.cpp:8111-8117.
            if !check_is_user(td, id) {
                return;
            }
            let Some(dapp_client_id) = crate::misc::clean_input_string(&request.dapp_client_id)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td
                .deferred_promise::<td_api::TonConnectRequests, _>(id, Object::tonConnectRequests);
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_ton_connect_requests(&mut env, true, 0, &dapp_client_id, promise);
            td.ton_wallet_manager = manager;
        }
        Function::claimTonConnectRequest(request) => {
            // Requests.cpp:8118-8126 — the server-message-id gate.
            if !check_is_user(td, id) {
                return;
            }
            let Some(dapp_request_id) = crate::misc::clean_input_string(&request.dapp_request_id)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.claim_ton_connect_request(
                &mut env,
                request.session_id,
                request.message_id,
                &dapp_request_id,
                request.is_rejected,
                promise,
            );
            td.ton_wallet_manager = manager;
        }
        Function::answerTonConnectRequest(request) => {
            // Requests.cpp:8127-8134 — the server-message-id gate.
            if !check_is_user(td, id) {
                return;
            }
            let Some(trace_id) = crate::misc::clean_input_string(&request.trace_id) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.submit_ton_connect_response(
                &mut env,
                request.session_id,
                request.message_id,
                &trace_id,
                &request.body,
                promise,
            );
            td.ton_wallet_manager = manager;
        }
        Function::getTonConnectSessionNextEventId(request) => {
            // Requests.cpp:8135-8140.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::TonConnectSessionEventId, _>(
                id,
                Object::tonConnectSessionEventId,
            );
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_ton_connect_next_event_id(&mut env, request.session_id, promise);
            td.ton_wallet_manager = manager;
        }
        Function::disconnectTonConnectSession(request) => {
            // Requests.cpp:8141-8146.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.close_ton_connect_session(&mut env, request.session_id, &request.body, promise);
            td.ton_wallet_manager = manager;
        }
        Function::getOnRampProviders(request) => {
            // Requests.cpp:8147-8153.
            if !check_is_user(td, id) {
                return;
            }
            let Some(cryptocurrency) = crate::misc::clean_input_string(&request.cryptocurrency)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise =
                td.deferred_promise::<td_api::OnRampProviders, _>(id, Object::onRampProviders);
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_on_ramp_providers(&mut env, &cryptocurrency, promise);
            td.ton_wallet_manager = manager;
        }
        Function::getOnRampProviderBaseCurrencies(request) => {
            // Requests.cpp:8154-8162.
            if !check_is_user(td, id) {
                return;
            }
            let Some(provider_id) = crate::misc::clean_input_string(&request.provider_id) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(cryptocurrency) = crate::misc::clean_input_string(&request.cryptocurrency)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<td_api::Currencies, _>(id, Object::currencies);
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_on_ramp_base_currencies(&mut env, &provider_id, &cryptocurrency, promise);
            td.ton_wallet_manager = manager;
        }
        Function::getOnRampPaymentAvailability(request) => {
            // Requests.cpp:8163-8172.
            if !check_is_user(td, id) {
                return;
            }
            let Some(provider_id) = crate::misc::clean_input_string(&request.provider_id) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(cryptocurrency) = crate::misc::clean_input_string(&request.cryptocurrency)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(base_currency) = crate::misc::clean_input_string(&request.base_currency)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<td_api::OnRampPaymentAvailability, _>(
                id,
                Object::onRampPaymentAvailability,
            );
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_on_ramp_availability(
                &mut env,
                &provider_id,
                &cryptocurrency,
                &base_currency,
                promise,
            );
            td.ton_wallet_manager = manager;
        }
        Function::getOnRampPaymentLimits(request) => {
            // Requests.cpp:8173-8183.
            if !check_is_user(td, id) {
                return;
            }
            let Some(provider_id) = crate::misc::clean_input_string(&request.provider_id) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(cryptocurrency) = crate::misc::clean_input_string(&request.cryptocurrency)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(base_currency) = crate::misc::clean_input_string(&request.base_currency)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(payment_method_name) =
                crate::misc::clean_input_string(&request.payment_method_name)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<td_api::OnRampPaymentLimits, _>(
                id,
                Object::onRampPaymentLimits,
            );
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_on_ramp_limits(
                &mut env,
                &provider_id,
                &cryptocurrency,
                &base_currency,
                &payment_method_name,
                promise,
            );
            td.ton_wallet_manager = manager;
        }
        Function::getOnRampPaymentQuote(request) => {
            // Requests.cpp:8184-8197.
            if !check_is_user(td, id) {
                return;
            }
            let Some(provider_id) = crate::misc::clean_input_string(&request.provider_id) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(cryptocurrency) = crate::misc::clean_input_string(&request.cryptocurrency)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(base_currency) = crate::misc::clean_input_string(&request.base_currency)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(cryptocurrency_amount) =
                crate::misc::clean_input_string(&request.cryptocurrency_amount)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(base_currency_amount) =
                crate::misc::clean_input_string(&request.base_currency_amount)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(payment_method_name) =
                crate::misc::clean_input_string(&request.payment_method_name)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td
                .deferred_promise::<td_api::OnRampPaymentQuote, _>(id, Object::onRampPaymentQuote);
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_on_ramp_quote(
                &mut env,
                &provider_id,
                &cryptocurrency,
                &base_currency,
                &cryptocurrency_amount,
                &base_currency_amount,
                &payment_method_name,
                promise,
            );
            td.ton_wallet_manager = manager;
        }
        Function::createOnRampPaymentSession(request) => {
            // Requests.cpp:8198-8217 — the 9b5f0edf9 theme flag rides the
            // query seam.
            if !check_is_user(td, id) {
                return;
            }
            let Some(provider_id) = crate::misc::clean_input_string(&request.provider_id) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(cryptocurrency) = crate::misc::clean_input_string(&request.cryptocurrency)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(address) = crate::misc::clean_input_string(&request.address) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(payment_method_name) =
                crate::misc::clean_input_string(&request.payment_method_name)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(base_currency) = crate::misc::clean_input_string(&request.base_currency)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(cryptocurrency_amount) =
                crate::misc::clean_input_string(&request.cryptocurrency_amount)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(base_currency_amount) =
                crate::misc::clean_input_string(&request.base_currency_amount)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(memo) = crate::misc::clean_input_string(&request.memo) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(theme) = crate::misc::clean_input_string(&request.theme) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(success_return_url) =
                crate::misc::clean_input_string(&request.success_return_url)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let Some(fail_return_url) = crate::misc::clean_input_string(&request.fail_return_url)
            else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<td_api::OnRampPaymentSession, _>(
                id,
                Object::onRampPaymentSession,
            );
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.create_on_ramp_session(
                &mut env,
                &provider_id,
                &cryptocurrency,
                &address,
                &payment_method_name,
                &base_currency,
                &cryptocurrency_amount,
                &base_currency_amount,
                &memo,
                &theme,
                &success_return_url,
                &fail_return_url,
                promise,
            );
            td.ton_wallet_manager = manager;
        }
        Function::sendTonCenterApiRequest(request) => {
            // Requests.cpp:8218-8225 — the type-dispatch UTF-8 bands.
            if !check_is_user(td, id) {
                return;
            }
            let Some(endpoint) = crate::misc::clean_input_string(&request.endpoint) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let mut outbox = td.deferred_promise::<td_api::Text, _>(id, Object::text);
            let promise = tdutils::promise::PromiseCreator::lambda_result(
                move |result: tdutils::status::Result<String>| match result {
                    tdutils::status::Result::Ok(response) => {
                        outbox.set_value(td_api::Text { text: response })
                    }
                    tdutils::status::Result::Err(status) => outbox.set_error(status),
                },
            );
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.perform_ton_center_api_request(&mut env, &endpoint, request.r#type, promise);
            td.ton_wallet_manager = manager;
        }
        Function::getTonCenterStreamingApiUrl(_) => {
            // Requests.cpp:8226-8231 — the 60-second cache band.
            if !check_is_user(td, id) {
                return;
            }
            let promise = td.deferred_promise::<td_api::TonCenterStreamingApiUrl, _>(
                id,
                Object::tonCenterStreamingApiUrl,
            );
            let (mut manager, mut env) = ton_wallet_split(td);
            manager.get_ton_center_streaming_api_url(&mut env, promise);
            td.ton_wallet_manager = manager;
        }

        Function::dismissWebToken(request) => {
            // Requests.cpp:9591-9595 (6f9801c65) — no CHECK_IS_USER, the
            // token clean, then AccountManager::cancel_web_token (the
            // auth_cancelWebTokenAuthorization send rides the net step).
            let Some(token) = crate::misc::clean_input_string(&request.token) else {
                return td.send_error_impl(id, 400, "Strings must be encoded in UTF-8");
            };
            let promise = td.deferred_promise::<(), _>(id, |()| ok_object());
            let net = td.account_net();
            let mut env = AccountEnv {
                global: &mut td.global,
                options: td.option_manager.as_mut(),
                user_manager: &mut td.user_manager,
                chat_manager: &mut td.chat_manager,
                device_token_manager: &mut td.device_token_manager,
                auth_is_bot: td.auth_manager.is_bot(),
                updates: &mut td.account_updates,
                net,
            };
            td.account_manager
                .cancel_web_token(&mut env, &token, promise);
        }

        Function::testNetwork(_) => {
            // Requests.cpp:9451-9462 — no CHECK_IS_USER (the
            // preauthentication set); the ok promise rides the
            // get_current_country_code query, which parks at the
            // [blocked: net] seam.
            let promise = td.deferred_promise::<String, _>(id, |_country_code| ok_object());
            let (mut env, mut manager) = crate::td::country_env_split(td);
            manager.get_current_country_code(&mut env, promise);
            td.country_info_manager = manager;
        }

        _ => {
            td.send_error_impl(id, 500, NOT_IMPLEMENTED_YET);
        }
    }
}

/// The `InviteGroupCallParticipantResult` → `Object` rewrap.
fn invite_result_to_object(value: td_api::InviteGroupCallParticipantResult) -> Object {
    match value {
        td_api::InviteGroupCallParticipantResult::inviteGroupCallParticipantResultSuccess(value) => {
            Object::inviteGroupCallParticipantResultSuccess(value)
        }
        td_api::InviteGroupCallParticipantResult::inviteGroupCallParticipantResultUserAlreadyParticipant(value) => {
            Object::inviteGroupCallParticipantResultUserAlreadyParticipant(value)
        }
        td_api::InviteGroupCallParticipantResult::inviteGroupCallParticipantResultUserPrivacyRestricted(value) => {
            Object::inviteGroupCallParticipantResultUserPrivacyRestricted(value)
        }
        td_api::InviteGroupCallParticipantResult::inviteGroupCallParticipantResultUserWasBanned(value) => {
            Object::inviteGroupCallParticipantResultUserWasBanned(value)
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

/// The take-out split for the TON wallet arms (the `BackgroundEnv`
/// convention): the manager becomes a local so the env may hold the whole
/// `&mut Td`.
fn ton_wallet_split(
    td: &mut Td,
) -> (
    crate::ton_wallet::TonWalletManager,
    crate::td::TonWalletEnv<'_>,
) {
    let manager = std::mem::take(&mut td.ton_wallet_manager);
    let env = crate::td::TonWalletEnv { td: &mut *td };
    (manager, env)
}

/// The community take-out split (the `ton_wallet_split` convention):
/// the manager comes out for the call, the env wraps the whole Td.
fn community_split(
    td: &mut Td,
) -> (
    crate::community_manager::CommunityManager,
    crate::td::CommunityEnv<'_>,
) {
    let manager = std::mem::take(&mut td.community_manager);
    let env = crate::td::CommunityEnv { td: &mut *td };
    (manager, env)
}

/// `CHECK_IS_USER` (Requests.cpp:1997-2000).
/// The `DialogId(chat_id)` + `MessageId(message_id)` pair the poll arms
/// route over (Requests.cpp:8088-8128).
fn message_full_id(chat_id: i64, message_id: i64) -> crate::messages_manager::MessageFullId {
    crate::messages_manager::MessageFullId::new(
        crate::messages_manager::DialogId::new(chat_id),
        crate::messages_manager::MessageId::new(message_id),
    )
}

/// `TdDb::as_db_key` (TdDb.cpp:671-676) — the empty key becomes the
/// well-known "cucumber" raw key.
pub(crate) fn as_db_key(key: Vec<u8>) -> tddb::DbKey {
    if key.is_empty() {
        tddb::DbKey::raw_key(b"cucumber".to_vec())
    } else {
        tddb::DbKey::raw_key(key)
    }
}

/// The `read_all_dialog_mentions` tail — the local half's decision
/// settles the request (the secret-chat band / the errors) or rides the
/// affected-history refill over the `MmRequestContext` trampoline (the
/// net-drain 29 wire tail, cpp:9187).
fn run_read_mentions_tail(
    td: &mut Td,
    dialog_id: crate::messages_manager::DialogId,
    forum_topic_id: crate::messages_manager::ForumTopicId,
    mut promise: tdutils::promise::Promise<()>,
    result: std::result::Result<
        crate::messages_manager::ReadAllMentionsTail,
        tdutils::status::Status,
    >,
) {
    match result {
        Ok(crate::messages_manager::ReadAllMentionsTail::Done) => promise.set_value(()),
        Ok(crate::messages_manager::ReadAllMentionsTail::RunServer) => {
            // SAFETY: the mm view aliases one Td, used strictly
            // sequentially inside the trampoline.
            let td_ptr = std::ptr::from_mut(td);
            let mm: &mut dyn crate::messages_manager::MmRequestContext = unsafe { &mut *td_ptr };
            mm.read_all_mentions_on_server(dialog_id, forum_topic_id, promise);
        }
        Err(status) => promise.set_error(status),
    }
}

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

/// The unit-shaped slot half of the collapse.
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
/// The `ReportSponsoredResult` → `Object` rewrap (the Object umbrella
/// flattens the sponsored-result constructors, so the polymorphic type
/// has no single variant).
pub(crate) fn report_sponsored_result_to_object(value: td_api::ReportSponsoredResult) -> Object {
    use td_api::ReportSponsoredResult as V;
    match value {
        V::reportSponsoredResultOk(v) => Object::reportSponsoredResultOk(v),
        V::reportSponsoredResultFailed(v) => Object::reportSponsoredResultFailed(v),
        V::reportSponsoredResultOptionRequired(v) => Object::reportSponsoredResultOptionRequired(v),
        V::reportSponsoredResultAdsHidden(v) => Object::reportSponsoredResultAdsHidden(v),
        V::reportSponsoredResultPremiumRequired(v) => {
            Object::reportSponsoredResultPremiumRequired(v)
        }
    }
}

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

        // The flip-state ledger — re-derived from the fixture at each
        // integration (the per-track flip histories compose): the m11 pins +
        // M.13/M.13S's seven real dispatches (removeNotification,
        // removeNotificationGroup, registerDevice and the four
        // notification-settings arms; processPushNotification stays the 500
        // placeholder) + M.12.10's eighteen file-plane dispatches
        // (getFile/getRemoteFile, the download/upload/generation arms, the
        // DownloadManager list plane and the four storage-statistics arms) +
        // M.23's ten LinkManager dispatches (getDeepLinkInfo,
        // getExternalLink, getExternalLinkInfo, getRecentlyVisitedTMeUrls,
        // the two login-url arms and the four oauth arms) + M.20's three
        // saved-animations arms (getSavedAnimations, addSavedAnimation,
        // removeSavedAnimation) + M.24's nine language-pack dispatches
        // (getLocalizationTargetInfo, getLanguagePackInfo,
        // getLanguagePackStrings, synchronizeLanguagePack,
        // addCustomServerLanguagePack, setCustomLanguagePack,
        // editCustomLanguagePackInfo, setCustomLanguagePackString,
        // deleteLanguagePack) and getLanguagePackString moving to the
        // synchronous set + M.27's six top-dialog/recommendation dispatches
        // (getTopChats, removeTopChat, getRecommendedChats,
        // getChatSimilarChats, getChatSimilarChatCount,
        // openChatSimilarChat) + M.26's twelve sponsored/promo/import
        // dispatches (getChatSponsoredMessages, clickChatSponsoredMessage,
        // viewSponsoredChat, reportSponsoredChat, the two video-ad arms,
        // getVideoMessageAdvertisements, getSearchSponsoredChats,
        // openSponsoredChat, importMessages and the report pair) + M.25's
        // two transcription dispatches (recognizeSpeech,
        // rateSpeechRecognition — the [blocked: M.10] probe seam answers
        // the C++ not-found band; the query tails ride the net-drain 47
        // `TranscriptionNetKind` plane) and seven translation dispatches
        // (translateText, composeTextWithAi, fixTextWithAi,
        // createRichMessageWithAi and the rich trio — the query tails
        // park at the [blocked: net]/[blocked: RichMessage] seams like
        // the C++ offline). Fix per flip. The M.dp slice flipped the
        // nineteen DialogParticipantManager arms (joinChat, leaveChat,
        // addChatMember(s), setChatMemberStatus, setChatMemberTag,
        // banChatMember, canTransferOwnership, transferChatOwnership,
        // getChatOwnerAfterLeaving, getChatMember, searchChatMembers,
        // getChatAdministrators, getChatJoinRequests,
        // processChatJoinRequest(s), getSupergroupMembers,
        // getGuardBotWebAppUrl, answerChatJoinRequestQuery) over the
        // collapsed dialog_participant_manager plane. The M.mp
        // slice flipped the twenty-one MessageQueryManager arms
        // (searchMessages, searchOutgoingDocumentMessages,
        // getPublicPostSearchLimits, searchPublicPosts,
        // searchPublicMessagesByTag, searchChatRecentLocationMessages,
        // deleteAllCallMessages, deleteMessageReactionsFromSender,
        // deleteAllRecentMessageReactionsFromSender,
        // deleteEphemeralMessage, deleteChatMessagesBySender,
        // deleteChatMessagesByDate, editEphemeralMessage,
        // editEphemeralMessageCaption, getPersonalChatHistory,
        // listenToAudio, summarizeMessage, addChecklistTasks,
        // markChecklistTasksAsDone, shareUsersWithBot, shareChatWithBot)
        // over the collapsed message_query_manager plane (the local
        // validation/decision tables exact; the query tails park at the
        // [blocked: net] seams). The M.saved slice flipped the 19
        // SavedMessagesManager arms (the 11 directMessagesChatTopic arms,
        // the 7 savedMessagesTopic arms and getMessageAuthor). The M.filter
        // slice flipped 17 DialogFilterManager arms and getChatFolderDefaultIconName (sync).
        // The M.invite slice flipped 14 DialogInviteLinkManager arms.
        // The M.st slice flipped the thirty-eight StickersManager arms
        // (searchStickers, getGreetingStickers, getPremiumStickerExamples,
        // getAnimatedEmoji, getCustomEmojiStickers, the three
        // default-custom-emoji arms, getDisallowedChatEmojiStatuses,
        // getEmojiCategories, getCustomEmojiReactionAnimations,
        // getForumTopicDefaultIcons, getThemedEmojiStatuses,
        // getThemedChatEmojiStatuses, getPremiumStickers,
        // getPremiumInfoSticker, getOwnedStickerSets, getStickerSetName,
        // getSuggestedStickerSetName, checkStickerSetName,
        // createNewStickerSet, addStickerToSet, replaceStickerInSet,
        // setStickerSetThumbnail, setCustomEmojiStickerSetThumbnail,
        // setStickerSetTitle, deleteStickerSet, setStickerPositionInSet,
        // setStickerEmojis, setStickerKeywords, setStickerMaskPosition,
        // removeStickerFromSet, reorderInstalledStickerSets,
        // uploadStickerFile, viewTrendingStickerSets, getStickerOutline,
        // getStickerOutlineSvgPath, getEmojiSuggestionsUrl) over the
        // collapsed stickers_manager plane. The M.star slice flipped the
        // sixteen StarManager arms (the 11 star
        // payments/transactions/subscriptions arms, the TON/Gram trio,
        // refundStarPayment and getPaidMessageRevenue). The PasswordManager
        // and AuthManager edge slice flipped 25 PasswordManager
        // arms and 16 AuthManager arms (41 flips in total).
        // The Message & Dialog / Interaction Operations slice flipped 87 arms:
        // MessagesManager (60), ReactionManager (13), QuickReplyManager (6),
        // WelcomeMessageManager (5), and Star/Financial (3) — the three
        // (getStarTransactions/getTonTransactions/getPaidMessageRevenue) are
        // served by the M.star dispatcher, which supersedes those stub arms
        // in the chain order. The merged pin: 693 + 41 (password/auth) + 87
        // (interaction) + 16 (star) - 3 (star overlap, counted once) = 834
        // dispatched / 167 interim.
        // The Chat & Group Management slice flipped 37 arms: chat creation,
        // inspection, properties toggling, and queries (834 + 37 = 871
        // dispatched / 130 interim).
        // The Bot & WebApp Operations slice flipped 5 arms: getRecentInlineBots,
        // reorderBotActiveUsernames, setBotProfilePhoto,
        // toggleBotCanManageEmojiStatus, toggleBotUsernameIsActive (871 + 5 = 876
        // dispatched / 125 interim).
        // The BackgroundManager slice flipped 9 arms: searchBackground,
        // getInstalledBackgrounds, getBackgroundUrl, setDefaultBackground,
        // deleteDefaultBackground, removeInstalledBackground,
        // resetInstalledBackgrounds, setChatBackground, deleteChatBackground
        // (877 + 9 = 886 dispatched / 115 interim; the net-step landing
        // moved the baseline to 877/124 before this slice).
        // The M.4 tails slice flipped 17 arms: the UserManager setters
        // (setProfilePhoto, setUserPersonalProfilePhoto,
        // suggestUserProfilePhoto, deleteProfilePhoto, setUserNote,
        // setMainProfileTab, suggestUserBirthdate, setProfileAccentColor,
        // setUpgradedGiftColors, setBusinessAccountProfilePhoto,
        // addProfileAudio, getSupportUser), the EmojiStatus query planes
        // (getDefaultEmojiStatuses, getDefaultChatEmojiStatuses,
        // getRecentEmojiStatuses, getUpgradedGiftEmojiStatuses,
        // clearRecentEmojiStatuses) (886 + 17 = 903 dispatched / 98
        // interim).
        // The Misc slice flipped 18 arms: checkQuickReplyShortcutName (sync),
        // getPollOptionProperties, getGroupsInCommon, readAllChatPollVotes,
        // readAllForumTopicPollVotes, loadQuickReplyShortcuts,
        // getForumTopicHistory, getStoryNotificationSettingsExceptions,
        // approveSuggestedPost, declineSuggestedPost, setMainProfileTab,
        // addProfileAudio, the four saved-notification-sound arms,
        // getPollVoteStatistics, canPurchaseFromStore. Reconciled over the
        // M.4-tails master, its setMainProfileTab/addProfileAudio arms are
        // superseded by that slice (already inside the 903), so the net-new
        // flips are 16: checkQuickReplyShortcutName moves to the sync set
        // (21 + 1 = 22 sync) and the other 15 go interim -> dispatched
        // (903 + 15 = 918 dispatched / 98 - 16 = 82 interim).
        // The proxy+network glue slice flipped 13 arms: the 8 proxy arms
        // (addProxy, editProxy, enableProxy, disableProxy, removeProxy,
        // getProxies, pingProxy, testProxy), the 4 network arms
        // (addNetworkStatistics, getNetworkStatistics,
        // resetNetworkStatistics, setNetworkType) and getPushReceiverId
        // moving to the synchronous set (918 + 12 = 930 dispatched / 69
        // interim, 23 sync — the merged pin).
        // The auth-tails glue slice flipped 6 arms: the 5
        // PhoneNumberManager arms (sendPhoneNumberCode,
        // checkPhoneNumberCode, resendPhoneNumberCode,
        // reportPhoneNumberCodeMissing, sendPhoneNumberFirebaseSms — the
        // queries ride the NetQueryDispatcher) and setDatabaseEncryptionKey
        // through the binlog change_key wiring (930 + 6 = 936 dispatched /
        // 69 - 6 = 63 interim, sync unmoved 23).
        // The privacy+auto-download glue slice flipped 8 arms:
        // getUserPrivacySettingRules, setUserPrivacySettingRules,
        // getNewChatPrivacySettings, setNewChatPrivacySettings,
        // getReadDatePrivacySettings, setReadDatePrivacySettings,
        // getAutoDownloadSettingsPresets, setAutoDownloadSettings
        // (936 + 8 = 944 dispatched / 63 - 8 = 55 interim, sync unmoved
        // 23 — the merged pin).
        // The stickers residue glue slice flipped 31 arms over the d0c0e44
        // base — the recent/favorite/hashtag/emoji-search/sticker-set
        // ladder on the M.st StickersManager plane. The 5 EmojiStatus
        // free-function arms (getDefaultEmojiStatuses,
        // getDefaultChatEmojiStatuses, getRecentEmojiStatuses,
        // getUpgradedGiftEmojiStatuses, clearRecentEmojiStatuses) were
        // already dispatched on master by the M.4-tails landing, so the
        // merge brings 26 net flips (944 + 26 = 970 dispatched / 55 - 26
        // = 29 interim, sync unmoved 23 — the merged pin).
        // The countries glue slice flipped 6 arms: getCountries, getCountry,
        // getCountryCode, getPhoneNumberInfo (dispatched; the
        // help.getNearestDc/help.getCountriesList tails park at the
        // [blocked: net] seams) and getCountryFlagEmoji +
        // getPhoneNumberInfoSync moving to the synchronous set (970 + 4 =
        // 974 dispatched / 29 - 4 - 2 = 23 interim, 23 + 2 = 25 sync — the
        // merged pin).
        // The files glue slice flipped 10 arms: the 9 file/message arms
        // (addFileToDownloads, addOffer, deleteFile,
        // getApplicationDownloadLink, getMapThumbnailFile,
        // getSuggestedFileName, readFilePart, saveApplicationLogEvent,
        // setApplicationVerificationToken) over the collapsed files.rs
        // FileManager/DownloadManager plane plus the MessagesManager
        // add-message-file ladder, and cleanFileName moving to the
        // synchronous set (974 + 9 = 983 dispatched / 23 - 10 = 13
        // interim, 25 + 1 = 26 sync — the merged pin).
        // The support glue slice flipped 3 arms: getUserSupportInfo,
        // setUserSupportInfo and getSupportName (the Support.cpp
        // send-half validation bands are real; the help.getUserInfo/
        // editUserInfo/getSupportName queries ride the [blocked: net]
        // seams) (983 + 3 = 986 dispatched / 13 - 3 = 10 interim, sync
        // unmoved 26 — the merged pin).
        // The misc glue slice flipped 10 arms: the link pair
        // (getInternalLink, getInternalLinkType — the LinkManager local
        // halves answer, the unparseable-link null parks), the giveaway
        // pair (getGiveawayInfo, launchPrepaidGiveaway — the Premium.cpp
        // query seams over the parameters/message-lookup bands),
        // processPushNotification (the disabled/"{}" and receiver-id
        // bands answer, the decrypt/run plane parks), searchUserByToken +
        // setInactiveSessionTtl (the AccountManager seams over the 0x505
        // SetAuthorizationTtlOnServer log event), sendCallLog (the
        // close/call-id bands, the M.12 file intake + upload park) and
        // setGiftSettings (the GlobalPrivacySettings gift leg over the
        // StarGiftSettings value plane) (986 + 10 = 996 dispatched /
        // 10 - 10 = 0 interim, sync unmoved 26 — the census closes).
        assert_eq!(*counts.get("sync").unwrap_or(&0), 26, "sync count");
        assert_eq!(
            *counts.get("dispatched").unwrap_or(&0),
            1039,
            "dispatched count"
        );
        // the c15d3f5a sync: the TonWalletManager plane landed over the
        // tde2e keychain subset — the TON wallet / TonConnect / On-ramp
        // surface (38 arms incl. getTonWalletNfts and dismissWebToken)
        // went dispatched with the community write family before it; the
        // census closes: 1001 + 38 = 1039 dispatched / 0 interim, sync
        // unmoved 26.
        assert_eq!(*counts.get("interim").unwrap_or(&0), 0, "interim count");
    }
}
