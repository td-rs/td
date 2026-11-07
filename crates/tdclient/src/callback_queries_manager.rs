//! Port of `td/telegram/CallbackQueriesManager.{h,cpp}` — the M.46
//! bots-slice manager over the callback-query updates (the four
//! `on_new_*` intakes) and the `getCallbackQueryAnswer`/`answerCallbackQuery`
//! request flips (Requests.cpp:8359-8374).
//!
//! Seams:
//! - The three `Td::ResultHandler` network tails
//!   (`messages.getBotCallbackAnswer`, `messages.setBotCallbackAnswer`,
//!   `ephemeral.getCallbackAnswer`) ride the `CallbackNetKind` plane
//!   (net-drain 49): the manager-held `CallbackNetHandle` dispatches
//!   the wire queries and the answers drain through
//!   `Td::on_callback_net_answer` into
//!   [`CallbackQueriesManager::on_get_bot_callback_answer`],
//!   [`CallbackQueriesManager::on_get_ephemeral_callback_answer`] and
//!   [`CallbackQueriesManager::on_set_bot_callback_answer`].
//! - `td_->password_manager_->get_input_check_password_srp`
//!   (CallbackQueriesManager.cpp:365-373) is the
//!   [`CallbackQueriesManagerEnv::get_input_check_password_srp`] seam —
//!   [blocked: PasswordManager]; the SRP promise parks with the seam.
//! - The MessagesManager/EphemeralMessage message plane
//!   (`on_edited_ephemeral_message`, `get_business_message_object`,
//!   `have_message_force`, `get_ephemeral_message_id_of_message_id`,
//!   `on_get_message_error`, `is_message_edited_recently`) is
//!   [blocked: M.10] through the env trait.
//! - `InlineQueriesManager::get_inline_message_id`
//!   (InlineQueriesManager.cpp:534-542) is ported here as a free
//!   function — it is a pure serializer with no manager state.

use crate::dialog_id::DialogId;
use crate::message_id::{MessageFullId, MessageId};
use crate::td_api;
use crate::user_manager::{AccessRights, UserId};
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The callback-query-plane in-flight query kinds — the C++
/// `Td::ResultHandler` query classes the manager's network tails create
/// (CallbackQueriesManager.cpp:31-179). The completions park by kind
/// behind the manager-held `crate::td::CallbackNetHandle`;
/// `Td::on_callback_net_answer` routes them. The kinds
/// carry the handler members the error heads read (the §5 identity — an
/// out-of-order answer pairs with its own query), so the family lots
/// park the promises alone.
// the C++ ResultHandler class names carry the shared
// CallbackAnswer postfix
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallbackNetKind {
    /// `GetBotCallbackAnswerQuery` (CallbackQueriesManager.cpp:31-100)
    /// — `messages.getBotCallbackAnswer`; the kind carries the
    /// handler's `dialog_id_`/`message_id_` for the
    /// `on_get_message_error` head the `on_error` tail runs (cpp:91).
    GetBotCallbackAnswer {
        dialog_id: DialogId,
        message_id: MessageId,
    },
    /// `SetBotCallbackAnswerQuery` (CallbackQueriesManager.cpp:144-179)
    /// — `messages.setBotCallbackAnswer`; the handler carries only the
    /// `promise_` member (cpp:146-149), so the lot is a plain
    /// positional FIFO.
    SetBotCallbackAnswer,
    /// `GetEphemeralCallbackAnswerQuery` (CallbackQueriesManager.cpp:
    /// 102-142) — `ephemeral.getCallbackAnswer`; the kind carries the
    /// handler's `dialog_id_` for the `on_get_dialog_error` head the
    /// `on_error` tail runs (cpp:136).
    GetEphemeralCallbackAnswer { dialog_id: DialogId },
}

/// The `messages.getBotCallbackAnswer` wire flags
/// (CallbackQueriesManager.cpp:49-70) — `DATA_MASK` carries the payload
/// bytes, `PASSWORD_MASK` the SRP envelope.
const CALLBACK_DATA_MASK: i32 = 1;
const CALLBACK_PASSWORD_MASK: i32 = 1 << 2;
/// The `messages.setBotCallbackAnswer` wire flags
/// (CallbackQueriesManager.cpp:152-158) — `MESSAGE_MASK`/`URL_MASK`
/// carry the non-empty strings.
const CALLBACK_MESSAGE_MASK: i32 = 1;
const CALLBACK_URL_MASK: i32 = 1 << 2;
/// The `ephemeral.getCallbackAnswer` wire flag
/// (CallbackQueriesManager.cpp:117) — the data bytes ride bit 1.
const EPHEMERAL_CALLBACK_DATA_MASK: i32 = 1 << 1;

/// `InlineQueriesManager::get_inline_message_id`
/// (InlineQueriesManager.cpp:534-542) — the base64url form of the bare
/// TL serialization (the C++ `serialize(*)` writes the fields without
/// the constructor id, exactly like the generated Rust `store`).
pub fn get_inline_message_id(
    input_bot_inline_message_id: telegram_api::InputBotInlineMessageID,
) -> String {
    let mut out = Vec::new();
    match input_bot_inline_message_id {
        telegram_api::InputBotInlineMessageID::inputBotInlineMessageID(value) => {
            value.store(&mut out)
        }
        telegram_api::InputBotInlineMessageID::inputBotInlineMessageID64(value) => {
            value.store(&mut out)
        }
    }
    tdutils::log_info!("Receive inline message identifier: {} bytes", out.len());
    base64url_encode_bytes(&out)
}

/// `td::base64url_encode` (base64.cpp:73-75) — the URL-safe alphabet,
/// no padding (the auth_manager stand-in, over bytes).
fn base64url_encode_bytes(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;
        result.push(ALPHABET[(n >> 18) as usize & 63] as char);
        result.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            result.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            result.push(ALPHABET[n as usize & 63] as char);
        }
    }
    result
}

/// The `G()`/manager slice the collapsed manager reads through.
pub trait CallbackQueriesManagerEnv {
    /// `td_->auth_manager_->is_bot()`.
    fn is_bot(&self) -> bool;

    /// `G()->close_flag()` — the `G()->close_status()` gate of
    /// `send_get_callback_answer_query` (CallbackQueriesManager.cpp:383).
    fn close_flag(&mut self) -> bool;

    /// `td_->user_manager_->have_min_user(sender_user_id)`.
    fn have_min_user(&mut self, sender_user_id: UserId) -> bool;

    /// `td_->user_manager_->get_user_id_object(user_id, source)`.
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64;

    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    /// `td_->dialog_manager_->force_create_dialog(dialog_id, source, true)`.
    fn force_create_dialog(&mut self, dialog_id: DialogId, source: &str);

    /// `td_->dialog_manager_->get_chat_id_object(dialog_id, source)`.
    fn get_chat_id_object(&mut self, dialog_id: DialogId, source: &str) -> i64;

    /// `td_->dialog_manager_->check_dialog_access(dialog_id, false,
    /// AccessRights::Read, source)`.
    fn check_dialog_access(&mut self, dialog_id: DialogId, source: &str) -> Result<(), Status>;

    /// `td_->dialog_manager_->check_dialog_access_in_memory(dialog_id,
    /// false, AccessRights::Read)`.
    fn check_dialog_access_in_memory(&mut self, dialog_id: DialogId) -> Result<(), Status>;

    /// `td_->dialog_manager_->on_get_dialog_error(dialog_id, status,
    /// source)` — `true` when the error was consumed.
    fn on_get_dialog_error(&mut self, dialog_id: DialogId, status: &Status, source: &str) -> bool;

    /// `td_->messages_manager_->have_message_force(message_full_id,
    /// source)` — [blocked: M.10].
    fn have_message_force(&mut self, message_full_id: MessageFullId, source: &str) -> bool;

    /// `td_->messages_manager_->on_get_message_error(dialog_id,
    /// message_id, status, source)` — [blocked: M.10].
    fn on_get_message_error(
        &mut self,
        dialog_id: DialogId,
        message_id: MessageId,
        status: &Status,
        source: &str,
    ) {
        let _ = (dialog_id, message_id, status, source);
        tdutils::log_info!("MessagesManager::on_get_message_error — [blocked: M.10 Messages]");
    }

    /// `td_->messages_manager_->is_message_edited_recently(
    /// {dialog_id, message_id}, seconds)` — [blocked: M.10].
    fn is_message_edited_recently(&mut self, message_full_id: MessageFullId, seconds: i32) -> bool {
        let _ = (message_full_id, seconds);
        tdutils::log_info!(
            "MessagesManager::is_message_edited_recently — [blocked: M.10 Messages]"
        );
        false
    }

    /// `td_->messages_manager_->get_ephemeral_message_id_of_message_id(
    /// message_full_id)` — [blocked: M.10]; `0` is the invalid id.
    fn get_ephemeral_message_id_of_message_id(&mut self, message_full_id: MessageFullId) -> i32 {
        let _ = message_full_id;
        tdutils::log_info!(
            "MessagesManager::get_ephemeral_message_id_of_message_id — [blocked: M.10 Messages]"
        );
        0
    }

    /// `td_->messages_manager_->on_edited_ephemeral_message(message,
    /// true)` — [blocked: M.10]; the default answers the "no message"
    /// pair, which the caller rejects exactly like the C++ invalid-id
    /// bail.
    fn on_edited_ephemeral_message(
        &mut self,
        message: Box<telegram_api::EphemeralMessage>,
    ) -> MessageFullId {
        let _ = message;
        tdutils::log_info!(
            "MessagesManager::on_edited_ephemeral_message — [blocked: M.10 Messages]"
        );
        MessageFullId::new(crate::message_id::DialogId::new(0), MessageId::new(0))
    }

    /// `td_->messages_manager_->get_business_message_object(message,
    /// reply_to_message)` — [blocked: M.10].
    fn get_business_message_object(
        &mut self,
        message: Option<Box<telegram_api::Message>>,
        reply_to_message: Option<Box<telegram_api::Message>>,
    ) -> Option<td_api::BusinessMessage> {
        let _ = (message, reply_to_message);
        tdutils::log_info!(
            "MessagesManager::get_business_message_object — [blocked: M.10 Messages]"
        );
        None
    }

    /// `td_->password_manager_->get_input_check_password_srp(password,
    /// promise)` (CallbackQueriesManager.cpp:365-368) —
    /// [blocked: PasswordManager]; the SRP promise parks with the seam.
    fn get_input_check_password_srp(
        &mut self,
        password: String,
        promise: Promise<telegram_api::InputCheckPasswordSRP>,
    ) {
        let _ = (password, promise);
        tdutils::log_info!(
            "PasswordManager::get_input_check_password_srp — [blocked: PasswordManager]"
        );
    }

    /// `td_->dialog_manager_->get_input_peer(dialog_id, rights)` — the
    /// net-drain 49 query send legs resolve the read peer (the
    /// `ForumTopicEnv` split shape; CallbackQueriesManager.cpp:46).
    fn get_input_peer(
        &mut self,
        dialog_id: DialogId,
        access_rights: AccessRights,
    ) -> Option<telegram_api::InputPeer>;
}

/// A parked `messages.getBotCallbackAnswer` query — the request
/// promise (the C++ query carries it on the handler's `promise_`
/// member, CallbackQueriesManager.cpp:32-35; the error head's
/// dialog/message identity rides the kind).
struct PendingBotCallbackAnswerQuery {
    promise: Promise<td_api::CallbackQueryAnswer>,
}

/// A parked `ephemeral.getCallbackAnswer` query — the request promise
/// (the C++ query carries it on the handler's `promise_` member,
/// CallbackQueriesManager.cpp:105-109; the error head's dialog identity
/// rides the kind).
struct PendingEphemeralCallbackAnswerQuery {
    promise: Promise<td_api::CallbackQueryAnswer>,
}

/// A parked `messages.setBotCallbackAnswer` query.
struct PendingSetBotCallbackAnswerQuery {
    promise: Promise<()>,
}

/// The port of `td::CallbackQueriesManager` (the C++ `td_` member
/// becomes the env-parameter methods).
#[derive(Default)]
pub struct CallbackQueriesManager {
    /// The manager-held dispatch surface of the net-drain 49 families —
    /// `None` keeps the net-inert posture (the pre-init windows and the
    /// net-inert tests; the `GroupCallManager::net` shape — the shared
    /// [`crate::td::BotEnv`] view resolves the peer at the send legs).
    pub(crate) net: Option<crate::td::CallbackNetHandle>,
    /// The parked `messages.getBotCallbackAnswer` queries — the collapsed
    /// `GetBotCallbackAnswerQuery` actor queue.
    pending_bot_callback_answer_queries: Vec<PendingBotCallbackAnswerQuery>,
    /// The parked `ephemeral.getCallbackAnswer` queries.
    pending_ephemeral_callback_answer_queries: Vec<PendingEphemeralCallbackAnswerQuery>,
    /// The parked `messages.setBotCallbackAnswer` queries.
    pending_set_bot_callback_answer_queries: Vec<PendingSetBotCallbackAnswerQuery>,
}

impl CallbackQueriesManager {
    /// `CallbackQueriesManager::CallbackQueriesManager(Td *)`
    /// (CallbackQueriesManager.cpp:181-182).
    pub fn new() -> CallbackQueriesManager {
        CallbackQueriesManager::default()
    }

    /// `answer_callback_query` (CallbackQueriesManager.cpp:184-188) —
    /// the env stays in the port shape though the send reads no plane
    /// through it (the manager-held handle replaces the C++
    /// `td_->create_handler` leg).
    // the C++ argument list is the port shape (env + 6 params)
    #[allow(clippy::too_many_arguments)]
    pub fn answer_callback_query(
        &mut self,
        _env: &mut impl CallbackQueriesManagerEnv,
        callback_query_id: i64,
        text: &str,
        show_alert: bool,
        url: &str,
        cache_time: i32,
        promise: Promise<()>,
    ) {
        self.send_set_bot_callback_answer_query(
            callback_query_id,
            text,
            url,
            show_alert,
            cache_time,
            promise,
        );
    }

    /// The `td_->create_handler<SetBotCallbackAnswerQuery>(promise)
    /// ->send(...)` collapse (CallbackQueriesManager.cpp:151-161): the
    /// request promise parks in the family lot (the C++ carries it on
    /// the query's `promise_` member, cpp:146-149; §5's FIFO shape),
    /// then the wire query dispatches — the `MESSAGE_MASK`/`URL_MASK`
    /// bits carry the non-empty strings (cpp:152-158) and the alert
    /// bit rides the bool field. `None` on the handle keeps the
    /// net-inert posture.
    #[allow(clippy::too_many_arguments)]
    fn send_set_bot_callback_answer_query(
        &mut self,
        callback_query_id: i64,
        text: &str,
        url: &str,
        show_alert: bool,
        cache_time: i32,
        promise: Promise<()>,
    ) {
        self.pending_set_bot_callback_answer_queries
            .push(PendingSetBotCallbackAnswerQuery { promise });
        match self.net.clone() {
            Some(net) => net.dispatch(
                CallbackNetKind::SetBotCallbackAnswer,
                telegram_api::Function::messages_setBotCallbackAnswer(
                    telegram_api::messages_setBotCallbackAnswer {
                        flags: (if text.is_empty() {
                            0
                        } else {
                            CALLBACK_MESSAGE_MASK
                        }) | (if url.is_empty() { 0 } else { CALLBACK_URL_MASK }),
                        alert: show_alert,
                        query_id: callback_query_id,
                        message: text.to_owned(),
                        url: url.to_owned(),
                        cache_time,
                    },
                ),
            ),
            None => {
                tdutils::log_info!("Send messages.setBotCallbackAnswer — [net-inert]");
            }
        }
    }

    /// `get_query_payload` (CallbackQueriesManager.cpp:190-207) —
    /// exactly one of data/game must be present.
    fn get_query_payload(
        data: &[u8],
        game_short_name: &str,
    ) -> Option<td_api::CallbackQueryPayload> {
        let has_data = !data.is_empty();
        let has_game = !game_short_name.is_empty();
        if has_data == has_game {
            tdutils::log_error!("Receive wrong callback query: {has_data} {has_game}");
            return None;
        }

        if has_data {
            return Some(td_api::CallbackQueryPayload::callbackQueryPayloadData(
                td_api::callbackQueryPayloadData {
                    data: data.to_vec(),
                },
            ));
        }
        Some(td_api::CallbackQueryPayload::callbackQueryPayloadGame(
            td_api::callbackQueryPayloadGame {
                game_short_name: game_short_name.to_owned(),
            },
        ))
    }

    /// `on_new_callback_query` (CallbackQueriesManager.cpp:209-242).
    #[allow(clippy::too_many_arguments)]
    pub fn on_new_callback_query(
        &mut self,
        env: &mut impl CallbackQueriesManagerEnv,
        callback_query_id: i64,
        sender_user_id: UserId,
        dialog_id: DialogId,
        message_id: MessageId,
        data: &[u8],
        chat_instance: i64,
        game_short_name: &str,
    ) {
        if !dialog_id.is_valid() {
            tdutils::log_error!("Receive new callback query in invalid {dialog_id:?}");
            return;
        }
        if !sender_user_id.is_valid() {
            tdutils::log_error!(
                "Receive new callback query from invalid {sender_user_id:?} in {dialog_id:?}"
            );
            return;
        }
        if !env.have_min_user(sender_user_id) {
            tdutils::log_error!("Receive unknown {sender_user_id:?}");
        }
        if !env.is_bot() {
            tdutils::log_error!("Receive new callback query");
            return;
        }
        if !message_id.is_valid() {
            tdutils::log_error!(
                "Receive new callback query from {message_id:?} in {dialog_id:?} sent by {sender_user_id:?}"
            );
            return;
        }

        let payload = Self::get_query_payload(data, game_short_name);
        let Some(payload) = payload else {
            return;
        };

        env.force_create_dialog(dialog_id, "on_new_callback_query");
        let update = td_api::Update::updateNewCallbackQuery(td_api::updateNewCallbackQuery {
            id: callback_query_id,
            sender_user_id: env.get_user_id_object(sender_user_id, "on_new_callback_query"),
            chat_id: env.get_chat_id_object(dialog_id, "updateNewCallbackQuery"),
            message_id: message_id.get(),
            chat_instance,
            payload: Some(payload),
        });
        env.send_update(update);
    }

    /// `on_new_ephemeral_callback_query`
    /// (CallbackQueriesManager.cpp:244-275).
    pub fn on_new_ephemeral_callback_query(
        &mut self,
        env: &mut impl CallbackQueriesManagerEnv,
        callback_query_id: i64,
        sender_user_id: UserId,
        data: &[u8],
        chat_instance: i64,
        message: Option<Box<telegram_api::EphemeralMessage>>,
    ) {
        if !sender_user_id.is_valid() {
            tdutils::log_error!(
                "Receive new ephemeral callback query from invalid {sender_user_id:?}"
            );
            return;
        }
        if !env.have_min_user(sender_user_id) {
            tdutils::log_error!("Receive unknown {sender_user_id:?}");
        }
        if !env.is_bot() {
            tdutils::log_error!("Receive new callback query");
            return;
        }

        let Some(payload) = Self::get_query_payload(data, "") else {
            return;
        };
        let Some(message) = message else {
            return;
        };
        if message.peer_id.is_none() {
            return;
        }

        let message_full_id = env.on_edited_ephemeral_message(message);
        let message_id = message_full_id.get_message_id();
        if !message_id.is_valid() || !message_id.is_local() {
            tdutils::log_error!("Receive ephemeral callback query from {message_full_id:?}");
            return;
        }

        let update = td_api::Update::updateNewCallbackQuery(td_api::updateNewCallbackQuery {
            id: callback_query_id,
            sender_user_id: env
                .get_user_id_object(sender_user_id, "on_new_ephemeral_callback_query"),
            chat_id: env.get_chat_id_object(
                DialogId::new(message_full_id.get_dialog_id().get()),
                "on_new_ephemeral_callback_query",
            ),
            message_id: message_id.get(),
            chat_instance,
            payload: Some(payload),
        });
        env.send_update(update);
    }

    /// `on_new_inline_callback_query`
    /// (CallbackQueriesManager.cpp:277-302).
    // the C++ argument list is the port shape
    #[allow(clippy::too_many_arguments)]
    pub fn on_new_inline_callback_query(
        &mut self,
        env: &mut impl CallbackQueriesManagerEnv,
        callback_query_id: i64,
        sender_user_id: UserId,
        inline_message_id: Option<telegram_api::InputBotInlineMessageID>,
        data: &[u8],
        chat_instance: i64,
        game_short_name: &str,
    ) {
        if !sender_user_id.is_valid() {
            tdutils::log_error!("Receive new callback query from invalid {sender_user_id:?}");
            return;
        }
        if !env.have_min_user(sender_user_id) {
            tdutils::log_error!("Receive unknown {sender_user_id:?}");
        }
        if !env.is_bot() {
            tdutils::log_error!("Receive new inline callback query");
            return;
        }
        let Some(inline_message_id) = inline_message_id else {
            // the C++ CHECK(inline_message_id != nullptr)
            panic!("Receive no inline message identifier");
        };

        let payload = Self::get_query_payload(data, game_short_name);
        let Some(payload) = payload else {
            return;
        };
        let update =
            td_api::Update::updateNewInlineCallbackQuery(td_api::updateNewInlineCallbackQuery {
                id: callback_query_id,
                sender_user_id: env
                    .get_user_id_object(sender_user_id, "on_new_inline_callback_query"),
                inline_message_id: get_inline_message_id(inline_message_id),
                chat_instance,
                payload: Some(payload),
            });
        env.send_update(update);
    }

    /// `on_new_business_callback_query`
    /// (CallbackQueriesManager.cpp:304-329).
    // the C++ argument list is the port shape
    #[allow(clippy::too_many_arguments)]
    pub fn on_new_business_callback_query(
        &mut self,
        env: &mut impl CallbackQueriesManagerEnv,
        callback_query_id: i64,
        sender_user_id: UserId,
        connection_id: String,
        message: Option<Box<telegram_api::Message>>,
        reply_to_message: Option<Box<telegram_api::Message>>,
        data: &[u8],
        chat_instance: i64,
    ) {
        if !sender_user_id.is_valid() {
            tdutils::log_error!("Receive new callback query from invalid {sender_user_id:?}");
            return;
        }
        if !env.have_min_user(sender_user_id) {
            tdutils::log_error!("Receive unknown {sender_user_id:?}");
        }
        if !env.is_bot() {
            tdutils::log_error!("Receive new business callback query");
            return;
        }
        let message_object = env.get_business_message_object(message, reply_to_message);
        let Some(message_object) = message_object else {
            return;
        };

        let payload = td_api::CallbackQueryPayload::callbackQueryPayloadData(
            td_api::callbackQueryPayloadData {
                data: data.to_vec(),
            },
        );
        let update = td_api::Update::updateNewBusinessCallbackQuery(
            td_api::updateNewBusinessCallbackQuery {
                id: callback_query_id,
                sender_user_id: env
                    .get_user_id_object(sender_user_id, "on_new_business_callback_query"),
                connection_id,
                message: Some(message_object),
                chat_instance,
                payload: Some(payload),
            },
        );
        env.send_update(update);
    }

    /// `send_callback_query` (CallbackQueriesManager.cpp:331-377).
    pub fn send_callback_query(
        &mut self,
        env: &mut impl CallbackQueriesManagerEnv,
        message_full_id: MessageFullId,
        payload: Option<td_api::CallbackQueryPayload>,
        mut promise: Promise<td_api::CallbackQueryAnswer>,
    ) {
        if env.is_bot() {
            return promise.set_error(Status::error(
                400,
                "Bot can't send callback queries to other bot",
            ));
        }

        let Some(payload) = payload else {
            return promise.set_error(Status::error(400, "Payload must be non-empty"));
        };

        let dialog_id = DialogId::new(message_full_id.get_dialog_id().get());
        if let Err(status) = env.check_dialog_access(dialog_id, "send_callback_query") {
            return promise.set_error(status);
        }

        if !env.have_message_force(message_full_id, "send_callback_query") {
            return promise.set_error(Status::error(400, "Message not found"));
        }
        let message_id = message_full_id.get_message_id();
        if message_id.is_valid_scheduled() {
            return promise.set_error(Status::error(
                400,
                "Can't send callback queries from scheduled messages",
            ));
        }
        let ephemeral_message_id = env.get_ephemeral_message_id_of_message_id(message_full_id);
        if ephemeral_message_id > 0
            && let td_api::CallbackQueryPayload::callbackQueryPayloadData(data_payload) = &payload
        {
            self.send_get_ephemeral_callback_answer_query(
                env,
                dialog_id,
                ephemeral_message_id,
                data_payload.data.clone(),
                promise,
            );
            return;
        }
        if !message_id.is_server() {
            return promise.set_error(Status::error(400, "Wrong message identifier"));
        }

        // the C++ splits the tail into `send_get_callback_answer_query`
        // (CallbackQueriesManager.cpp:379-394); the password leg routes
        // through the SRP seam first (CallbackQueriesManager.cpp:363-373,
        // [blocked: PasswordManager]) and the tail parks here
        let is_game = matches!(
            payload,
            td_api::CallbackQueryPayload::callbackQueryPayloadGame(_)
        );
        if let td_api::CallbackQueryPayload::callbackQueryPayloadDataWithPassword(data_payload) =
            &payload
        {
            env.get_input_check_password_srp(data_payload.password.clone(), Promise::default());
            self.send_get_callback_answer_query(
                env,
                message_full_id,
                false,
                data_payload.data.clone(),
                true,
                promise,
            );
        } else {
            let data = match &payload {
                td_api::CallbackQueryPayload::callbackQueryPayloadData(data_payload) => {
                    data_payload.data.clone()
                }
                _ => Vec::new(),
            };
            self.send_get_callback_answer_query(
                env,
                message_full_id,
                is_game,
                data,
                false,
                promise,
            );
        }
    }

    /// `send_get_callback_answer_query`
    /// (CallbackQueriesManager.cpp:379-394) — the close/in-memory
    /// guards run at the query send; the collapsed park keeps the
    /// per-query state for the answer handler.
    fn send_get_callback_answer_query(
        &mut self,
        env: &mut impl CallbackQueriesManagerEnv,
        message_full_id: MessageFullId,
        is_game: bool,
        data: Vec<u8>,
        has_password: bool,
        mut promise: Promise<td_api::CallbackQueryAnswer>,
    ) {
        // TRY_STATUS_PROMISE(promise, G()->close_status())
        if env.close_flag() {
            return promise.set_error(Status::error(500, "Request aborted"));
        }

        let dialog_id = DialogId::new(message_full_id.get_dialog_id().get());
        if let Err(status) = env.check_dialog_access_in_memory(dialog_id) {
            return promise.set_error(status);
        }
        if !env.have_message_force(message_full_id, "send_get_callback_answer_query") {
            return promise.set_error(Status::error(400, "Message not found"));
        }

        self.send_get_bot_callback_answer_query(
            env,
            dialog_id,
            message_full_id.get_message_id(),
            is_game,
            data,
            has_password,
            promise,
        );
    }

    /// The `td_->create_handler<GetBotCallbackAnswerQuery>(promise)
    /// ->send(...)` collapse (CallbackQueriesManager.cpp:41-77): the
    /// request promise parks in the family lot (the C++ carries it on
    /// the query, §5's FIFO shape), then the read peer resolves and the
    /// wire query dispatches — the `DATA_MASK`/`PASSWORD_MASK`/`GAME`
    /// bits carry the payload form (cpp:49-70) and the id is the server
    /// message id (cpp:73). The C++ `CHECK(input_peer != nullptr)` band
    /// (cpp:46-47) files through the completed queue so the answer half
    /// runs the full `on_error` tail (the message-query convention);
    /// `None` on the handle keeps the net-inert posture. The C++
    /// `need_resend_on_503_ = false` flag (cpp:75) rides the handle's
    /// 24-hour budget like every landed per-kind send (documented
    /// divergence).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_get_bot_callback_answer_query(
        &mut self,
        env: &mut impl CallbackQueriesManagerEnv,
        dialog_id: DialogId,
        message_id: MessageId,
        is_game: bool,
        data: Vec<u8>,
        has_password: bool,
        promise: Promise<td_api::CallbackQueryAnswer>,
    ) {
        self.pending_bot_callback_answer_queries
            .push(PendingBotCallbackAnswerQuery { promise });
        let kind = CallbackNetKind::GetBotCallbackAnswer {
            dialog_id,
            message_id,
        };
        match (
            self.net.clone(),
            env.get_input_peer(dialog_id, AccessRights::Read),
        ) {
            (Some(net), Some(peer)) => net.dispatch(
                kind,
                telegram_api::Function::messages_getBotCallbackAnswer(
                    telegram_api::messages_getBotCallbackAnswer {
                        flags: if is_game { 0 } else { CALLBACK_DATA_MASK }
                            | if has_password {
                                CALLBACK_PASSWORD_MASK
                            } else {
                                0
                            },
                        game: is_game,
                        peer: Some(Box::new(peer)),
                        msg_id: message_id.get_server_message_id().get(),
                        data,
                        // the SRP envelope rides the
                        // [blocked: PasswordManager] seam (the parked
                        // get_input_check_password_srp call above)
                        password: None,
                    },
                ),
            ),
            (Some(net), None) => {
                // the C++ CHECK(input_peer != nullptr) band (cpp:46-47)
                net.complete_error(kind, 400, "Can't access the chat");
            }
            (None, _) => {
                tdutils::log_info!("Send messages.getBotCallbackAnswer — [net-inert]");
            }
        }
    }

    /// The `td_->create_handler<GetEphemeralCallbackAnswerQuery>(promise)
    /// ->send(...)` collapse (CallbackQueriesManager.cpp:111-122, the
    /// routing at cpp:353-358): the request promise parks in the family
    /// lot (the C++ carries it on the query's `promise_` member,
    /// cpp:105-109; §5's FIFO shape — the kind carries the handler's
    /// `dialog_id_` for the error head), then the read peer resolves and
    /// the wire query dispatches — the data bytes ride the bit-1 mask
    /// (cpp:117) and the id is the raw ephemeral message id (cpp:119).
    /// The C++ `CHECK(input_peer != nullptr)` band (cpp:114-115) files
    /// through the completed queue so the answer half runs the full
    /// `on_error` tail (the message-query convention); `None` on the
    /// handle keeps the net-inert posture. The C++
    /// `need_resend_on_503_ = false` flag (cpp:120) rides the handle's
    /// 24-hour budget like every landed per-kind send (documented
    /// divergence).
    pub(crate) fn send_get_ephemeral_callback_answer_query(
        &mut self,
        env: &mut impl CallbackQueriesManagerEnv,
        dialog_id: DialogId,
        ephemeral_message_id: i32,
        data: Vec<u8>,
        promise: Promise<td_api::CallbackQueryAnswer>,
    ) {
        self.pending_ephemeral_callback_answer_queries
            .push(PendingEphemeralCallbackAnswerQuery { promise });
        let kind = CallbackNetKind::GetEphemeralCallbackAnswer { dialog_id };
        match (
            self.net.clone(),
            env.get_input_peer(dialog_id, AccessRights::Read),
        ) {
            (Some(net), Some(peer)) => net.dispatch(
                kind,
                telegram_api::Function::ephemeral_getCallbackAnswer(
                    telegram_api::ephemeral_getCallbackAnswer {
                        flags: EPHEMERAL_CALLBACK_DATA_MASK,
                        peer: Some(Box::new(peer)),
                        id: ephemeral_message_id,
                        data,
                    },
                ),
            ),
            (Some(net), None) => {
                // the C++ CHECK(input_peer != nullptr) band (cpp:114-115)
                net.complete_error(kind, 400, "Can't access the chat");
            }
            (None, _) => {
                tdutils::log_info!("Send ephemeral.getCallbackAnswer — [net-inert]");
            }
        }
    }

    /// `GetBotCallbackAnswerQuery::on_result` (on the parked query) +
    /// `on_error` (CallbackQueriesManager.cpp:79-99) — the caller passes
    /// the query answer or the fetch/network status; the
    /// dialog/message pair rides the wire kind (the C++ handler's
    /// `dialog_id_`/`message_id_` members, cpp:33-34).
    pub fn on_get_bot_callback_answer(
        &mut self,
        env: &mut impl CallbackQueriesManagerEnv,
        dialog_id: DialogId,
        message_id: MessageId,
        result: Result<telegram_api::messages_BotCallbackAnswer, Status>,
    ) {
        let Some(query) = self.pending_bot_callback_answer_queries.pop() else {
            tdutils::log_info!("Receive an unexpected messages.getBotCallbackAnswer answer");
            return;
        };
        let mut promise = query.promise;
        let answer = match result {
            Ok(answer) => answer,
            Err(mut status) => {
                env.on_get_message_error(
                    dialog_id,
                    message_id,
                    &status,
                    "GetBotCallbackAnswerQuery",
                );
                if status.message() == "BOT_RESPONSE_TIMEOUT" {
                    status = Status::error(502, "The bot is not responding");
                }
                if status.code() == 502
                    && env.is_message_edited_recently(
                        MessageFullId::new(
                            crate::message_id::DialogId::new(dialog_id.get()),
                            message_id,
                        ),
                        31,
                    )
                {
                    return promise.set_value(td_api::CallbackQueryAnswer::default());
                }
                return promise.set_error(status);
            }
        };
        promise.set_value(td_api::CallbackQueryAnswer {
            text: answer.message,
            show_alert: answer.alert,
            url: answer.url,
        });
    }

    /// `GetEphemeralCallbackAnswerQuery::on_result`/`on_error`
    /// (CallbackQueriesManager.cpp:124-142) — the dialog identity rides
    /// the wire kind (the C++ handler's `dialog_id_` member, cpp:103).
    pub fn on_get_ephemeral_callback_answer(
        &mut self,
        env: &mut impl CallbackQueriesManagerEnv,
        dialog_id: DialogId,
        result: Result<telegram_api::messages_BotCallbackAnswer, Status>,
    ) {
        let Some(query) = self.pending_ephemeral_callback_answer_queries.pop() else {
            tdutils::log_info!("Receive an unexpected ephemeral.getCallbackAnswer answer");
            return;
        };
        let mut promise = query.promise;
        let answer = match result {
            Ok(answer) => answer,
            Err(mut status) => {
                env.on_get_dialog_error(dialog_id, &status, "GetEphemeralCallbackAnswerQuery");
                if status.message() == "BOT_RESPONSE_TIMEOUT" {
                    status = Status::error(502, "The bot is not responding");
                }
                return promise.set_error(status);
            }
        };
        promise.set_value(td_api::CallbackQueryAnswer {
            text: answer.message,
            show_alert: answer.alert,
            url: answer.url,
        });
    }

    /// `SetBotCallbackAnswerQuery::on_result`/`on_error`
    /// (CallbackQueriesManager.cpp:163-178).
    pub fn on_set_bot_callback_answer(&mut self, result: Result<bool, Status>) {
        let Some(query) = self.pending_set_bot_callback_answer_queries.pop() else {
            tdutils::log_info!("Receive an unexpected messages.setBotCallbackAnswer answer");
            return;
        };
        let mut promise = query.promise;
        match result {
            // the C++ `result_ptr.ok()` is the Bool wire answer
            Ok(true) => promise.set_value(()),
            Ok(false) => {
                tdutils::log_info!("Sending answer to a callback query has failed");
                promise.set_value(());
            }
            Err(status) => promise.set_error(status),
        }
    }

    /// Test-only: the parked get-family promise pop — the netted
    /// net-inert asserts read the park (the `pop_rate_promise`
    /// convention).
    #[cfg(test)]
    pub(crate) fn pop_bot_callback_answer_promise_for_test(
        &mut self,
    ) -> Option<Promise<td_api::CallbackQueryAnswer>> {
        self.pending_bot_callback_answer_queries
            .pop()
            .map(|query| query.promise)
    }

    /// Test-only: the parked set-family promise pop (the same
    /// convention).
    #[cfg(test)]
    pub(crate) fn pop_set_bot_callback_answer_promise_for_test(&mut self) -> Option<Promise<()>> {
        self.pending_set_bot_callback_answer_queries
            .pop()
            .map(|query| query.promise)
    }

    /// Test-only: the parked ephemeral-family promise pop.
    #[cfg(test)]
    pub(crate) fn pop_ephemeral_callback_answer_promise_for_test(
        &mut self,
    ) -> Option<Promise<td_api::CallbackQueryAnswer>> {
        self.pending_ephemeral_callback_answer_queries
            .pop()
            .map(|query| query.promise)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdutils::promise::PromiseInterface;

    /// The recording env: the fired query seams, the sent updates and the
    /// [blocked: M.10] answer toggles.
    struct TestEnv {
        bot: bool,
        closing: bool,
        known_users: Vec<UserId>,
        have_message: bool,
        edited_recently: bool,
        ephemeral_message_id: i32,
        business_message: Option<td_api::BusinessMessage>,
        queries: Vec<String>,
        updates: Vec<td_api::Update>,
        /// `check_dialog_access_in_memory` denies (the C++ in-memory gate).
        deny_in_memory: bool,
        /// the `have_message_force` calls flip to `false` after the first
        /// one (the C++ re-check inside `send_get_callback_answer_query`).
        have_message_once: bool,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                bot: true,
                closing: false,
                known_users: vec![UserId::new(7)],
                have_message: true,
                edited_recently: false,
                ephemeral_message_id: 0,
                business_message: None,
                queries: Vec::new(),
                updates: Vec::new(),
                deny_in_memory: false,
                have_message_once: false,
            }
        }
    }

    impl CallbackQueriesManagerEnv for TestEnv {
        fn is_bot(&self) -> bool {
            self.bot
        }

        fn close_flag(&mut self) -> bool {
            self.closing
        }

        fn have_min_user(&mut self, sender_user_id: UserId) -> bool {
            self.known_users.contains(&sender_user_id)
        }

        fn get_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
            user_id.get()
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn force_create_dialog(&mut self, dialog_id: DialogId, _source: &str) {
            self.queries
                .push(format!("force_create:{}", dialog_id.get()));
        }

        fn get_chat_id_object(&mut self, dialog_id: DialogId, _source: &str) -> i64 {
            dialog_id.get()
        }

        fn check_dialog_access(
            &mut self,
            dialog_id: DialogId,
            _source: &str,
        ) -> Result<(), Status> {
            if dialog_id.is_valid() {
                Ok(())
            } else {
                Err(Status::error(400, "Chat not found"))
            }
        }

        fn check_dialog_access_in_memory(&mut self, dialog_id: DialogId) -> Result<(), Status> {
            if self.deny_in_memory {
                return Err(Status::error(400, "Chat is inaccessible"));
            }
            if dialog_id.is_valid() {
                Ok(())
            } else {
                Err(Status::error(400, "Chat not found"))
            }
        }

        fn on_get_dialog_error(
            &mut self,
            _dialog_id: DialogId,
            _status: &Status,
            _source: &str,
        ) -> bool {
            true
        }

        fn have_message_force(
            &mut self,
            message_full_id: crate::message_id::MessageFullId,
            _source: &str,
        ) -> bool {
            let answer = self.have_message
                && message_full_id.get_dialog_id().is_valid()
                && message_full_id.get_message_id().get() != -3;
            if self.have_message_once {
                // the one-shot: only the first call sees the message
                self.have_message = false;
                self.have_message_once = false;
            }
            answer
        }

        fn is_message_edited_recently(
            &mut self,
            _message_full_id: crate::message_id::MessageFullId,
            _seconds: i32,
        ) -> bool {
            self.edited_recently
        }

        fn get_ephemeral_message_id_of_message_id(
            &mut self,
            _message_full_id: crate::message_id::MessageFullId,
        ) -> i32 {
            self.ephemeral_message_id
        }

        fn on_edited_ephemeral_message(
            &mut self,
            _message: Box<telegram_api::EphemeralMessage>,
        ) -> crate::message_id::MessageFullId {
            crate::message_id::MessageFullId::new(
                crate::message_id::DialogId::new(7),
                // a local id: the low TYPE_LOCAL bits
                crate::message_id::MessageId::new((1 << 20) | 2),
            )
        }

        fn get_business_message_object(
            &mut self,
            _message: Option<Box<telegram_api::Message>>,
            _reply_to_message: Option<Box<telegram_api::Message>>,
        ) -> Option<td_api::BusinessMessage> {
            self.business_message.clone()
        }

        fn get_input_peer(
            &mut self,
            _dialog_id: DialogId,
            _access_rights: crate::user_manager::AccessRights,
        ) -> Option<telegram_api::InputPeer> {
            Some(telegram_api::InputPeer::inputPeerSelf(
                telegram_api::inputPeerSelf {},
            ))
        }
    }

    // --- the test promise helpers (the M.45 RecordingPromise convention) ---

    type Recorded<T> = std::sync::Arc<std::sync::Mutex<Vec<std::result::Result<T, String>>>>;

    struct RecordingPromise<T: Send + 'static> {
        outcomes: Recorded<T>,
    }

    impl<T: Send + 'static> PromiseInterface<T> for RecordingPromise<T> {
        fn set_value(&mut self, value: T) {
            self.outcomes.lock().unwrap().push(Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            self.outcomes.lock().unwrap().push(Err(format!(
                "{} {}",
                error.code(),
                error.message()
            )));
        }
    }

    fn recording_promise<T: Send + 'static>() -> (Promise<T>, Recorded<T>) {
        let outcomes: Recorded<T> = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Promise::from_interface(RecordingPromise {
                outcomes: outcomes.clone(),
            }),
            outcomes,
        )
    }

    fn message_full_id(dialog_id: i64, message_id: i64) -> crate::message_id::MessageFullId {
        crate::message_id::MessageFullId::new(
            crate::message_id::DialogId::new(dialog_id),
            MessageId::new(message_id),
        )
    }

    fn sample_answer() -> telegram_api::messages_BotCallbackAnswer {
        telegram_api::messages_BotCallbackAnswer {
            flags: 0,
            alert: true,
            has_url: true,
            native_ui: false,
            message: "answer".to_owned(),
            url: "https://t.me".to_owned(),
            cache_time: 5,
        }
    }

    #[test]
    fn get_inline_message_id_round_trips_the_bare_serialization() {
        let inline_message_id = telegram_api::InputBotInlineMessageID::inputBotInlineMessageID(
            telegram_api::inputBotInlineMessageID {
                dc_id: 2,
                id: 77,
                access_hash: 1234,
            },
        );
        let encoded = get_inline_message_id(inline_message_id.clone());
        // the URL-safe alphabet carries no padding
        assert!(!encoded.contains('='));
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));

        // decode and parse the bare form back
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&encoded)
            .expect("decodes");
        // the bare form has no constructor id; the concrete read matches
        // the C++ `inputBotInlineMessageID::fetch(parser)` shape
        let parsed = telegram_api::InputBotInlineMessageID::inputBotInlineMessageID(
            telegram_api::inputBotInlineMessageID::read_bare(
                &mut tdmtproto::telegram_api::tl::Reader::new(&bytes),
            )
            .expect("parses the bare form"),
        );
        assert_eq!(parsed, inline_message_id);
    }

    #[test]
    fn get_query_payload_requires_exactly_one_of_data_and_game() {
        // both present → the C++ logs and returns nullptr
        assert!(CallbackQueriesManager::get_query_payload(b"data", "game").is_none());
        // neither → the same log arm
        assert!(CallbackQueriesManager::get_query_payload(b"", "").is_none());
        let data = CallbackQueriesManager::get_query_payload(b"raw", "");
        assert!(matches!(
            data,
            Some(td_api::CallbackQueryPayload::callbackQueryPayloadData(_))
        ));
        let game = CallbackQueriesManager::get_query_payload(b"", "game");
        assert!(matches!(
            game,
            Some(td_api::CallbackQueryPayload::callbackQueryPayloadGame(_))
        ));
    }

    #[test]
    fn on_new_callback_query_guards_and_emits_the_update() {
        let mut env = TestEnv::default();
        let mut manager = CallbackQueriesManager::new();

        // a non-bot client never emits the update
        env.bot = false;
        manager.on_new_callback_query(
            &mut env,
            1,
            UserId::new(7),
            DialogId::new(-3),
            MessageId::new(1 << 20),
            b"d",
            5,
            "",
        );
        assert!(env.updates.is_empty());
        env.bot = true;

        // a payload with both data and game is dropped
        manager.on_new_callback_query(
            &mut env,
            1,
            UserId::new(7),
            DialogId::new(-3),
            MessageId::new(1 << 20),
            b"d",
            5,
            "g",
        );
        assert!(env.updates.is_empty());

        manager.on_new_callback_query(
            &mut env,
            42,
            UserId::new(7),
            DialogId::new(-3),
            MessageId::new(1 << 20),
            b"d",
            5,
            "",
        );
        assert_eq!(env.queries, vec!["force_create:-3"]);
        assert_eq!(env.updates.len(), 1);
        let td_api::Update::updateNewCallbackQuery(update) = &env.updates[0] else {
            panic!("wrong update");
        };
        assert_eq!(update.id, 42);
        assert_eq!(update.sender_user_id, 7);
        assert_eq!(update.chat_id, -3);
        assert_eq!(update.message_id, 1 << 20);
        assert_eq!(update.chat_instance, 5);
        assert!(matches!(
            update.payload,
            Some(td_api::CallbackQueryPayload::callbackQueryPayloadData(_))
        ));
    }

    #[test]
    fn on_new_ephemeral_callback_query_builds_the_local_message_update() {
        let mut env = TestEnv::default();
        let mut manager = CallbackQueriesManager::new();

        // no peer in the message → the C++ bail
        manager.on_new_ephemeral_callback_query(&mut env, 1, UserId::new(7), b"d", 5, None);
        assert!(env.updates.is_empty());

        let message = telegram_api::EphemeralMessage {
            peer_id: Some(telegram_api::Peer::peerUser(telegram_api::peerUser {
                user_id: 7,
            })),
            ..Default::default()
        };
        manager.on_new_ephemeral_callback_query(
            &mut env,
            43,
            UserId::new(7),
            b"d",
            5,
            Some(Box::new(message)),
        );
        assert_eq!(env.updates.len(), 1);
        let td_api::Update::updateNewCallbackQuery(update) = &env.updates[0] else {
            panic!("wrong update");
        };
        assert_eq!(update.id, 43);
        assert_eq!(update.chat_id, 7);
        // the local message id the env handed back
        assert_eq!(update.message_id, (1 << 20) | 2);
    }

    #[test]
    fn on_new_inline_callback_query_encodes_the_inline_id() {
        let mut env = TestEnv::default();
        let mut manager = CallbackQueriesManager::new();

        let inline_id = telegram_api::InputBotInlineMessageID::inputBotInlineMessageID(
            telegram_api::inputBotInlineMessageID {
                dc_id: 2,
                id: 1,
                access_hash: 2,
            },
        );
        manager.on_new_inline_callback_query(
            &mut env,
            44,
            UserId::new(7),
            Some(inline_id),
            b"",
            9,
            "game",
        );
        assert_eq!(env.updates.len(), 1);
        let td_api::Update::updateNewInlineCallbackQuery(update) = &env.updates[0] else {
            panic!("wrong update");
        };
        assert_eq!(update.id, 44);
        assert!(!update.inline_message_id.is_empty());
        assert!(matches!(
            update.payload,
            Some(td_api::CallbackQueryPayload::callbackQueryPayloadGame(_))
        ));

        // the C++ CHECK(inline_message_id != nullptr)
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            manager.on_new_inline_callback_query(&mut env, 45, UserId::new(7), None, b"", 9, "");
        }));
        assert!(result.is_err());
    }

    #[test]
    fn on_new_business_callback_query_requires_the_message_object() {
        let mut env = TestEnv::default();
        let mut manager = CallbackQueriesManager::new();

        // the message object seam answers None → no update
        manager.on_new_business_callback_query(
            &mut env,
            46,
            UserId::new(7),
            "conn".to_owned(),
            None,
            None,
            b"d",
            5,
        );
        assert!(env.updates.is_empty());

        env.business_message = Some(td_api::BusinessMessage::default());
        manager.on_new_business_callback_query(
            &mut env,
            46,
            UserId::new(7),
            "conn".to_owned(),
            None,
            None,
            b"d",
            5,
        );
        assert_eq!(env.updates.len(), 1);
        let td_api::Update::updateNewBusinessCallbackQuery(update) = &env.updates[0] else {
            panic!("wrong update");
        };
        assert_eq!(update.id, 46);
        assert_eq!(update.connection_id, "conn");
        assert!(update.message.is_some());
        assert!(matches!(
            update.payload,
            Some(td_api::CallbackQueryPayload::callbackQueryPayloadData(_))
        ));
    }

    #[test]
    fn send_callback_query_guards() {
        let mut env = TestEnv::default();
        let mut manager = CallbackQueriesManager::new();

        // bots cannot ask other bots
        env.bot = true;
        let (promise, outcomes) = recording_promise();
        manager.send_callback_query(
            &mut env,
            message_full_id(-3, 1 << 20),
            Some(td_api::CallbackQueryPayload::callbackQueryPayloadData(
                td_api::callbackQueryPayloadData {
                    data: b"d".to_vec(),
                },
            )),
            promise,
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 Bot can't send callback queries to other bot")
        );

        // an empty payload
        env.bot = false;
        let (promise, outcomes) = recording_promise();
        manager.send_callback_query(&mut env, message_full_id(-3, 1 << 20), None, promise);
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 Payload must be non-empty")
        );

        // an inaccessible chat
        let (promise, outcomes) = recording_promise();
        manager.send_callback_query(
            &mut env,
            message_full_id(0, 1 << 20),
            Some(payload_data()),
            promise,
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 Chat not found")
        );

        // an unknown message
        env.have_message = false;
        let (promise, outcomes) = recording_promise();
        manager.send_callback_query(
            &mut env,
            message_full_id(-3, 1 << 20),
            Some(payload_data()),
            promise,
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 Message not found")
        );
        env.have_message = true;

        // scheduled messages are rejected
        let (promise, outcomes) = recording_promise();
        manager.send_callback_query(
            &mut env,
            message_full_id(-3, (1 << 20) | 4),
            Some(payload_data()),
            promise,
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 Can't send callback queries from scheduled messages")
        );

        // non-server ids are rejected
        let (promise, outcomes) = recording_promise();
        manager.send_callback_query(
            &mut env,
            message_full_id(-3, (1 << 20) | 2),
            Some(payload_data()),
            promise,
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 Wrong message identifier")
        );
    }

    fn payload_data() -> td_api::CallbackQueryPayload {
        td_api::CallbackQueryPayload::callbackQueryPayloadData(td_api::callbackQueryPayloadData {
            data: b"d".to_vec(),
        })
    }

    #[test]
    fn send_callback_query_parks_and_answers() {
        let mut env = TestEnv {
            bot: false,
            ..TestEnv::default()
        };
        let mut manager = CallbackQueriesManager::new();

        let (promise, outcomes) = recording_promise();
        manager.send_callback_query(
            &mut env,
            message_full_id(-3, 1 << 20),
            Some(payload_data()),
            promise,
        );
        assert!(
            !manager.pending_bot_callback_answer_queries.is_empty(),
            "the promise parks like the C++ handler holds it"
        );
        manager.on_get_bot_callback_answer(
            &mut env,
            crate::dialog_id::DialogId::new(-3),
            crate::message_id::MessageId::new(1 << 20),
            Ok(sample_answer()),
        );
        let outcomes = outcomes.lock().unwrap();
        let answer = outcomes[0].as_ref().expect("answered");
        assert_eq!(answer.text, "answer");
        assert!(answer.show_alert);
        assert_eq!(answer.url, "https://t.me");

        // the BOT_RESPONSE_TIMEOUT remap
        let (promise, outcomes) = recording_promise();
        manager.send_callback_query(
            &mut env,
            message_full_id(-3, 1 << 20),
            Some(payload_data()),
            promise,
        );
        manager.on_get_bot_callback_answer(
            &mut env,
            crate::dialog_id::DialogId::new(-3),
            crate::message_id::MessageId::new(1 << 20),
            Err(Status::error(400, "BOT_RESPONSE_TIMEOUT")),
        );
        assert!(matches!(
            &outcomes.lock().unwrap()[0],
            Err(message) if message == "502 The bot is not responding"
        ));

        // a recently-edited message answers the empty object on a 502
        env.edited_recently = true;
        let (promise, outcomes) = recording_promise();
        manager.send_callback_query(
            &mut env,
            message_full_id(-3, 1 << 20),
            Some(payload_data()),
            promise,
        );
        manager.on_get_bot_callback_answer(
            &mut env,
            crate::dialog_id::DialogId::new(-3),
            crate::message_id::MessageId::new(1 << 20),
            Err(Status::error(502, "The bot is not responding")),
        );
        let outcomes = outcomes.lock().unwrap();
        let answer = outcomes[0].as_ref().expect("empty answer");
        assert_eq!(answer.text, "");
        assert!(!answer.show_alert);
    }

    #[test]
    fn send_callback_query_routes_the_ephemeral_and_password_forms() {
        let mut env = TestEnv {
            bot: false,
            ..TestEnv::default()
        };
        let mut manager = CallbackQueriesManager::new();

        // an ephemeral message id routes to the ephemeral query — the
        // promise parks in the ephemeral lot (the get lot stays empty)
        env.ephemeral_message_id = 9;
        let (promise, outcomes) = recording_promise();
        manager.send_callback_query(
            &mut env,
            message_full_id(-3, 1 << 20),
            Some(payload_data()),
            promise,
        );
        assert!(manager.pending_bot_callback_answer_queries.is_empty());
        assert!(!manager.pending_ephemeral_callback_answer_queries.is_empty());
        assert!(outcomes.lock().unwrap().is_empty());
        manager.on_get_ephemeral_callback_answer(
            &mut env,
            crate::dialog_id::DialogId::new(-3),
            Ok(sample_answer()),
        );
        assert_eq!(outcomes.lock().unwrap().len(), 1);

        // the BOT_RESPONSE_TIMEOUT remap on the ephemeral answer
        let (promise, outcomes) = recording_promise();
        manager.send_callback_query(
            &mut env,
            message_full_id(-3, 1 << 20),
            Some(payload_data()),
            promise,
        );
        manager.on_get_ephemeral_callback_answer(
            &mut env,
            crate::dialog_id::DialogId::new(-3),
            Err(Status::error(400, "BOT_RESPONSE_TIMEOUT")),
        );
        assert!(matches!(
            &outcomes.lock().unwrap()[0],
            Err(message) if message == "502 The bot is not responding"
        ));

        // the password payload routes through the SRP seam with the data
        // — the get-family promise parks (the PASSWORD_MASK band is the
        // netted test's wire assert)
        env.ephemeral_message_id = 0;
        env.queries.clear();
        let (promise, _outcomes) = recording_promise();
        manager.send_callback_query(
            &mut env,
            message_full_id(-3, 1 << 20),
            Some(
                td_api::CallbackQueryPayload::callbackQueryPayloadDataWithPassword(
                    td_api::callbackQueryPayloadDataWithPassword {
                        password: "pw".to_owned(),
                        data: b"d".to_vec(),
                    },
                ),
            ),
            promise,
        );
        assert!(
            !manager.pending_bot_callback_answer_queries.is_empty(),
            "the password-form promise parks like the data form"
        );
        assert!(env.queries.is_empty(), "no ephemeral query fired");
    }

    #[test]
    fn send_get_callback_answer_query_guards_run_at_the_send() {
        // CallbackQueriesManager.cpp:379-394: the close gate, the
        // in-memory access re-check and the `have_message_force` re-check
        // all fire on the collapsed send path
        let mut env = TestEnv {
            bot: false,
            ..TestEnv::default()
        };
        let mut manager = CallbackQueriesManager::new();

        // a closing Td fails before anything fires
        env.closing = true;
        let (promise, outcomes) = recording_promise();
        manager.send_callback_query(
            &mut env,
            message_full_id(-3, 1 << 20),
            Some(payload_data()),
            promise,
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "500 Request aborted")
        );
        assert!(env.queries.is_empty());
        assert!(manager.pending_bot_callback_answer_queries.is_empty());
        env.closing = false;

        // the in-memory gate
        env.deny_in_memory = true;
        let (promise, outcomes) = recording_promise();
        manager.send_callback_query(
            &mut env,
            message_full_id(-3, 1 << 20),
            Some(payload_data()),
            promise,
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 Chat is inaccessible")
        );
        assert!(env.queries.is_empty());
        env.deny_in_memory = false;

        // the message vanishes between the outer check and the send
        env.have_message_once = true;
        let (promise, outcomes) = recording_promise();
        manager.send_callback_query(
            &mut env,
            message_full_id(-3, 1 << 20),
            Some(payload_data()),
            promise,
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(message) if message == "400 Message not found")
        );
        assert!(env.queries.is_empty());
    }

    #[test]
    fn answer_callback_query_parks_and_completes() {
        let mut env = TestEnv::default();
        let mut manager = CallbackQueriesManager::new();

        let (promise, outcomes) = recording_promise();
        manager.answer_callback_query(&mut env, 11, "done", true, "https://t.me", 7, promise);
        assert!(
            !manager.pending_set_bot_callback_answer_queries.is_empty(),
            "the promise parks like the C++ handler holds it"
        );
        manager.on_set_bot_callback_answer(Ok(true));
        assert_eq!(*outcomes.lock().unwrap(), vec![Ok(())]);

        // the false Bool answer still completes the promise (the C++ logs)
        let (promise, outcomes) = recording_promise();
        manager.answer_callback_query(&mut env, 12, "", false, "", 0, promise);
        manager.on_set_bot_callback_answer(Ok(false));
        assert_eq!(*outcomes.lock().unwrap(), vec![Ok(())]);

        // a network error passes through
        let (promise, outcomes) = recording_promise();
        manager.answer_callback_query(&mut env, 13, "", false, "", 0, promise);
        manager.on_set_bot_callback_answer(Err(Status::error(400, "QUERY_ID_INVALID")));
        assert!(matches!(
            &outcomes.lock().unwrap()[0],
            Err(message) if message == "400 QUERY_ID_INVALID"
        ));
    }
}
