//! `WidgetManager` (td/telegram/WidgetManager.{h,cpp}) — the 1.8.70
//! widgets family (#119): the bot widget-query plane. The C++ manager is
//! stateless besides `td_` (every method immediately converts to a wire
//! query through a `Td::ResultHandler`); this port collapses the handlers
//! into the `WidgetNetKind` completed queue with per-family FIFO
//! promise lots on [`WidgetManager`]. The three inbound
//! `updateBot*WidgetQuery` updates route from `UpdatesManager` into the
//! [`WidgetManagerEnv::send_update`] outbox as the td_api
//! `updateNew*WidgetQuery` family. Recorded seams: the
//! `on_get_message_error` error hop (the walking-lane shape — raw
//! errors), the `wallet_card` conversion (`TonWalletCard` is the
//! wallet-cards family) and the `need_resend_on_503 = false` flag (the
//! port's NetQuery carries no per-query resend budget).

use crate::dialog_id::DialogId;
use crate::messages_manager::{MessageFullId, MessageId};
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The `G()`/`td_` surface of the manager.
pub trait WidgetManagerEnv {
    fn auth_is_bot(&self) -> bool;

    fn send_update(&mut self, update: td_api::Update);

    /// `DialogManager::check_dialog_access(dialog_id, false, Read, _)` —
    /// the full access-check plane is the M.6 continuation; the seam
    /// answers valid dialogs only.
    fn check_dialog_access(&mut self, dialog_id: DialogId) -> Result<(), Status>;

    /// `DialogManager::force_create_dialog` — the first query emitter's
    /// pre-create.
    fn force_create_dialog(&mut self, dialog_id: DialogId, source: &str) {
        let _ = (dialog_id, source);
    }

    /// `UserManager::have_min_user` — the unknown-sender LOG_IF guard.
    fn have_min_user(&mut self, user_id: UserId) -> bool {
        let _ = user_id;
        true
    }

    /// `UserManager::get_user_id_object` — the emitted update's sender.
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64 {
        let _ = source;
        user_id.get()
    }

    /// `DialogManager::get_chat_id_object`.
    fn get_chat_id_object(&mut self, dialog_id: DialogId) -> i64 {
        dialog_id.get()
    }

    /// `MessagesManager::have_message_force` — the message-availability
    /// gate of the user/bot message bands.
    fn have_message_force(&mut self, message_full_id: MessageFullId, source: &str) -> bool {
        let _ = (message_full_id, source);
        true
    }

    /// `MessagesManager::get_message_widget_public_data`
    /// (MessagesManager.cpp:5253-5261) — the local widget-data read.
    fn get_message_widget_public_data(
        &mut self,
        message_full_id: MessageFullId,
    ) -> Result<(String, i64), Status>;

    /// `MessagesManager::on_update_message_widget_data`
    /// (MessagesManager.cpp:5222-5251) — the widgetPublicData answer's
    /// in-place apply.
    fn on_update_message_widget_data(
        &mut self,
        message_full_id: MessageFullId,
        public_data: String,
        public_data_hash: i64,
        chat_data: String,
        update_chat_data: bool,
    );

    /// `InlineQueriesManager::get_inline_message_id` — the inline
    /// emitter's id stringifier.
    fn get_inline_message_id(
        &mut self,
        inline_message_id: telegram_api::InputBotInlineMessageID,
    ) -> String;

    // ---- The net query senders — the C++ ResultHandler send legs
    // (WidgetManager.cpp:125-313). The dispatch-dependency argument of
    // the C++ `net_query_creator().create` calls collapses (the port's
    // NetQuery carries no dependency set), and
    // `need_resend_on_503 = false` (GetBotWidgetQueryAnswerQuery) has no
    // per-query carrier. ----

    /// `GetBotWidgetQueryAnswerQuery::send` (cpp:135-147) — the
    /// read-access input peer of the message dialog resolves env-side.
    fn send_get_bot_widget_query_answer_query(
        &mut self,
        message_full_id: MessageFullId,
        query_type: telegram_api::WidgetQueryType,
        data: &[u8],
    ) {
        let _ = (message_full_id, query_type, data);
    }

    /// `SetBotWidgetQueryAnswerQuery::send` (cpp:175-178).
    fn send_set_bot_widget_query_answer_query(&mut self, widget_query_id: i64, data: &str) {
        let _ = (widget_query_id, data);
    }

    /// `EditMessageWidgetDataQuery::send` (cpp:207-227) — the answer is
    /// an Updates umbrella (routed through the updates pipe); the peer
    /// resolves env-side.
    fn send_edit_message_widget_data_query(
        &mut self,
        message_full_id: MessageFullId,
        edit_public_data: bool,
        public_data: &str,
        edit_chat_data: bool,
        chat_data: &str,
    ) {
        let _ = (
            message_full_id,
            edit_public_data,
            public_data,
            edit_chat_data,
            chat_data,
        );
    }

    /// `EditWidgetDataQuery::send` (cpp:253-260).
    fn send_edit_widget_data_query(&mut self, widget_id: i64, public_data: &str, cache_time: i32) {
        let _ = (widget_id, public_data, cache_time);
    }

    /// `GetWidgetPublicDataQuery::send` (cpp:287-296) — the peer
    /// resolves env-side.
    fn send_get_widget_public_data_query(&mut self, message_full_id: MessageFullId, hash: i64) {
        let _ = (message_full_id, hash);
    }
}

/// The per-family promise lots — the C++ `Promise` members of the five
/// `ResultHandler` classes (stateless besides these).
#[derive(Default)]
pub struct WidgetManager {
    /// `GetBotWidgetQueryAnswerQuery::promise_`.
    pub get_answer_promises: Vec<Promise<td_api::WidgetQueryAnswer>>,
    /// `SetBotWidgetQueryAnswerQuery::promise_`.
    pub set_answer_promises: Vec<Promise<()>>,
    /// `EditMessageWidgetDataQuery::promise_`.
    pub edit_message_data_promises: Vec<Promise<()>>,
    /// `EditWidgetDataQuery::promise_`.
    pub edit_widget_data_promises: Vec<Promise<()>>,
    /// `GetWidgetPublicDataQuery::promise_`.
    pub get_public_data_promises: Vec<Promise<td_api::WidgetPublicData>>,
}

/// `get_input_user_wallet` (WidgetManager.cpp:30-44).
fn get_input_user_wallet(
    wallet: Option<td_api::WidgetQueryWallet>,
) -> Result<telegram_api::UserWallet, Status> {
    let Some(wallet) = wallet else {
        return Err(Status::error(400, "Wallet must be non-empty"));
    };
    match crate::ton_wallet::check_ton_address_allow_raw(&wallet.address) {
        tdutils::status::Result::Ok(()) => {}
        tdutils::status::Result::Err(status) => return Err(status),
    }
    if wallet.gram_amount < 0 {
        return Err(Status::error(400, "Invalid wallet balance specified"));
    }
    if wallet.public_key.is_empty() != wallet.state_init.is_empty() {
        return Err(Status::error(
            400,
            "Wallet public key and state init must be specified together",
        ));
    }
    Ok(telegram_api::UserWallet {
        flags: 0,
        address: wallet.address,
        balance: wallet.gram_amount,
        public_key: wallet.public_key,
        state_init: wallet.state_init,
    })
}

/// `get_input_widget_query_type` (WidgetManager.cpp:46-78) — the null
/// query type is the "Query type must be non-empty" band.
pub(crate) fn get_input_widget_query_type(
    query_type: Option<td_api::WidgetQueryType>,
) -> Result<telegram_api::WidgetQueryType, Status> {
    let Some(query_type) = query_type else {
        return Err(Status::error(400, "Query type must be non-empty"));
    };
    Ok(match query_type {
        td_api::WidgetQueryType::widgetQueryTypeData(type_) => {
            telegram_api::WidgetQueryType::widgetQueryTypeData(telegram_api::widgetQueryTypeData {
                flags: 0,
                with_wallet: type_.with_wallet,
            })
        }
        td_api::WidgetQueryType::widgetQueryTypeSignData(type_) => {
            let wallet = get_input_user_wallet(type_.wallet)?;
            let Some(sign_data) = crate::misc::clean_input_string(&type_.sign_data) else {
                return Err(Status::error(400, "Strings must be encoded in UTF-8"));
            };
            telegram_api::WidgetQueryType::widgetQueryTypeSignData(
                telegram_api::widgetQueryTypeSignData {
                    wallet: Some(wallet),
                    sign_data: Some(telegram_api::DataJSON { data: sign_data }),
                },
            )
        }
        td_api::WidgetQueryType::widgetQueryTypeSentTransaction(type_) => {
            let wallet = get_input_user_wallet(type_.wallet)?;
            let Some(msg_hash) = crate::misc::clean_input_string(&type_.msg_hash) else {
                return Err(Status::error(400, "Strings must be encoded in UTF-8"));
            };
            telegram_api::WidgetQueryType::widgetQueryTypeSentTransaction(
                telegram_api::widgetQueryTypeSentTransaction {
                    wallet: Some(wallet),
                    msg_hash,
                },
            )
        }
    })
}

/// `get_widget_query_wallet_object` (WidgetManager.cpp:80-90) — the
/// negative wire balance clamps to zero behind an error log.
fn get_widget_query_wallet_object(wallet: telegram_api::UserWallet) -> td_api::WidgetQueryWallet {
    let mut balance = wallet.balance;
    if balance < 0 {
        tdutils::log_error!("Receive TON wallet balance {balance}");
        balance = 0;
    }
    td_api::WidgetQueryWallet {
        address: wallet.address,
        gram_amount: balance,
        public_key: wallet.public_key,
        state_init: wallet.state_init,
    }
}

/// `get_widget_query_type_object` (WidgetManager.cpp:92-114).
pub(crate) fn get_widget_query_type_object(
    query_type: telegram_api::WidgetQueryType,
) -> td_api::WidgetQueryType {
    match query_type {
        telegram_api::WidgetQueryType::widgetQueryTypeData(type_) => {
            td_api::WidgetQueryType::widgetQueryTypeData(td_api::widgetQueryTypeData {
                with_wallet: type_.with_wallet,
            })
        }
        telegram_api::WidgetQueryType::widgetQueryTypeSignData(type_) => {
            let wallet = type_
                .wallet
                .map(get_widget_query_wallet_object)
                .unwrap_or_default();
            td_api::WidgetQueryType::widgetQueryTypeSignData(td_api::widgetQueryTypeSignData {
                wallet: Some(wallet),
                sign_data: type_.sign_data.map(|data| data.data).unwrap_or_default(),
            })
        }
        telegram_api::WidgetQueryType::widgetQueryTypeSentTransaction(type_) => {
            let wallet = type_
                .wallet
                .map(get_widget_query_wallet_object)
                .unwrap_or_default();
            td_api::WidgetQueryType::widgetQueryTypeSentTransaction(
                td_api::widgetQueryTypeSentTransaction {
                    wallet: Some(wallet),
                    msg_hash: type_.msg_hash,
                },
            )
        }
    }
}

/// `get_widget_query_ton_wallet_card_object` (WidgetManager.cpp:116-123)
/// — the `TonWalletCard::get_ton_wallet_card` conversion is the
/// wallet-cards family; the emitter carries `None` until it lands.
fn get_widget_query_ton_wallet_card_object(
    wallet_card: Option<telegram_api::wallet_Card>,
) -> Option<td_api::TonWalletCard> {
    let _ = wallet_card;
    None
}

impl WidgetManager {
    /// `WidgetManager::send_widget_query` (WidgetManager.cpp:324-342) —
    /// the getWidgetQueryAnswer band.
    pub fn send_widget_query(
        &mut self,
        env: &mut impl WidgetManagerEnv,
        message_full_id: MessageFullId,
        query_type: Option<td_api::WidgetQueryType>,
        data: Vec<u8>,
        mut promise: Promise<td_api::WidgetQueryAnswer>,
    ) {
        let dialog_id = message_full_id.get_dialog_id();
        let message_id = message_full_id.get_message_id();
        if let Err(status) = env.check_dialog_access(dialog_id) {
            return promise.set_error(status);
        }
        if !env.have_message_force(message_full_id, "send_widget_query") {
            return promise.set_error(Status::error(400, "Message not found"));
        }
        if !message_id.is_server() {
            return promise.set_error(Status::error(400, "Wrong message identifier"));
        }
        let input_query_type = match get_input_widget_query_type(query_type) {
            Ok(query_type) => query_type,
            Err(status) => return promise.set_error(status),
        };
        env.send_get_bot_widget_query_answer_query(message_full_id, input_query_type, &data);
        self.get_answer_promises.push(promise);
    }

    /// `WidgetManager::on_new_widget_query` (WidgetManager.cpp:344-376) —
    /// the updateBotWidgetQuery intake.
    #[allow(clippy::too_many_arguments)] // the C++ signature has the same shape
    pub fn on_new_widget_query(
        &mut self,
        env: &mut impl WidgetManagerEnv,
        widget_query_id: i64,
        query_type: Option<telegram_api::WidgetQueryType>,
        sender_user_id: UserId,
        dialog_id: DialogId,
        message_id: MessageId,
        data: Vec<u8>,
        chat_instance: i64,
        chat_type: String,
        widget_id: i64,
        wallet_card: Option<telegram_api::wallet_Card>,
    ) {
        if !dialog_id.is_valid() {
            tdutils::log_error!("Receive new widget query in invalid dialog {dialog_id:?}");
            return;
        }
        if !sender_user_id.is_valid() {
            tdutils::log_error!(
                "Receive new widget query from invalid user {sender_user_id:?} in {dialog_id:?}"
            );
            return;
        }
        if !env.have_min_user(sender_user_id) {
            tdutils::log_error!("Receive unknown user {sender_user_id:?}");
        }
        if !env.auth_is_bot() {
            tdutils::log_error!("Receive new widget query");
            return;
        }
        if !message_id.is_valid() {
            tdutils::log_error!(
                "Receive new widget query from invalid message {message_id:?} in {dialog_id:?} sent by {sender_user_id:?}"
            );
            return;
        }
        env.force_create_dialog(dialog_id, "on_new_widget_query");
        let query_type = query_type.map(get_widget_query_type_object);
        let sender_user_id_object = env.get_user_id_object(sender_user_id, "updateNewWidgetQuery");
        let chat_id = env.get_chat_id_object(dialog_id);
        env.send_update(td_api::Update::updateNewWidgetQuery(
            td_api::updateNewWidgetQuery {
                id: widget_query_id,
                sender_user_id: sender_user_id_object,
                chat_id,
                message_id: message_id.get(),
                chat_instance,
                chat_type,
                query_type,
                data,
                widget_id,
                wallet_card: get_widget_query_ton_wallet_card_object(wallet_card),
            },
        ));
    }

    /// `WidgetManager::on_new_inline_widget_query`
    /// (WidgetManager.cpp:378-401) — the updateInlineBotWidgetQuery
    /// intake.
    #[allow(clippy::too_many_arguments)] // the C++ signature has the same shape
    pub fn on_new_inline_widget_query(
        &mut self,
        env: &mut impl WidgetManagerEnv,
        widget_query_id: i64,
        query_type: Option<telegram_api::WidgetQueryType>,
        sender_user_id: UserId,
        inline_message_id: telegram_api::InputBotInlineMessageID,
        data: Vec<u8>,
        chat_instance: i64,
        chat_type: String,
        widget_id: i64,
        wallet_card: Option<telegram_api::wallet_Card>,
    ) {
        if !sender_user_id.is_valid() {
            tdutils::log_error!(
                "Receive new inline widget query from invalid user {sender_user_id:?}"
            );
            return;
        }
        if !env.have_min_user(sender_user_id) {
            tdutils::log_error!("Receive unknown user {sender_user_id:?}");
        }
        if !env.auth_is_bot() {
            tdutils::log_error!("Receive new inline widget query");
            return;
        }
        let query_type = query_type.map(get_widget_query_type_object);
        let inline_message_id = env.get_inline_message_id(inline_message_id);
        let sender_user_id_object =
            env.get_user_id_object(sender_user_id, "updateNewInlineWidgetQuery");
        env.send_update(td_api::Update::updateNewInlineWidgetQuery(
            td_api::updateNewInlineWidgetQuery {
                id: widget_query_id,
                sender_user_id: sender_user_id_object,
                inline_message_id,
                chat_instance,
                chat_type,
                query_type,
                data,
                widget_id,
                wallet_card: get_widget_query_ton_wallet_card_object(wallet_card),
            },
        ));
    }

    /// `WidgetManager::on_new_widget_copy_query` (WidgetManager.cpp:403-422)
    /// — the updateBotWidgetCopyQuery intake.
    #[allow(clippy::too_many_arguments)] // the C++ signature has the same shape
    pub fn on_new_widget_copy_query(
        &mut self,
        env: &mut impl WidgetManagerEnv,
        widget_query_id: i64,
        query_type: Option<telegram_api::WidgetQueryType>,
        sender_user_id: UserId,
        data: Vec<u8>,
        widget_id: i64,
        wallet_card: Option<telegram_api::wallet_Card>,
    ) {
        if !sender_user_id.is_valid() {
            tdutils::log_error!(
                "Receive new widget copy query from invalid user {sender_user_id:?}"
            );
            return;
        }
        if !env.have_min_user(sender_user_id) {
            tdutils::log_error!("Receive unknown user {sender_user_id:?}");
        }
        if !env.auth_is_bot() {
            tdutils::log_error!("Receive new widget copy query");
            return;
        }
        let query_type = query_type.map(get_widget_query_type_object);
        let sender_user_id_object =
            env.get_user_id_object(sender_user_id, "updateNewWidgetCopyQuery");
        env.send_update(td_api::Update::updateNewWidgetCopyQuery(
            td_api::updateNewWidgetCopyQuery {
                id: widget_query_id,
                sender_user_id: sender_user_id_object,
                query_type,
                data,
                widget_id,
                wallet_card: get_widget_query_ton_wallet_card_object(wallet_card),
            },
        ));
    }

    /// `WidgetManager::answer_widget_query` (WidgetManager.cpp:424-426) —
    /// the answerWidgetQuery band; the id carries no validation.
    pub fn answer_widget_query(
        &mut self,
        env: &mut impl WidgetManagerEnv,
        widget_query_id: i64,
        data: &str,
        promise: Promise<()>,
    ) {
        env.send_set_bot_widget_query_answer_query(widget_query_id, data);
        self.set_answer_promises.push(promise);
    }

    /// `WidgetManager::edit_message_widget_data` (WidgetManager.cpp:428-449)
    /// — the editMessageWidgetData band; the answer is an Updates
    /// umbrella routed through the updates pipe.
    #[allow(clippy::too_many_arguments)] // the C++ signature has the same shape
    pub fn edit_message_widget_data(
        &mut self,
        env: &mut impl WidgetManagerEnv,
        message_full_id: MessageFullId,
        edit_public_data: bool,
        public_data: &str,
        edit_chat_data: bool,
        chat_data: &str,
        mut promise: Promise<()>,
    ) {
        let dialog_id = message_full_id.get_dialog_id();
        let message_id = message_full_id.get_message_id();
        if let Err(status) = env.check_dialog_access(dialog_id) {
            return promise.set_error(status);
        }
        if !env.have_message_force(message_full_id, "edit_message_widget_data") {
            return promise.set_error(Status::error(400, "Message not found"));
        }
        if !message_id.is_server() {
            return promise.set_error(Status::error(400, "Wrong message identifier"));
        }
        if !edit_public_data && !edit_chat_data {
            return promise.set_value(());
        }
        env.send_edit_message_widget_data_query(
            message_full_id,
            edit_public_data,
            public_data,
            edit_chat_data,
            chat_data,
        );
        self.edit_message_data_promises.push(promise);
    }

    /// `WidgetManager::edit_widget_data` (WidgetManager.cpp:451-461) —
    /// the editWidgetData band.
    pub fn edit_widget_data(
        &mut self,
        env: &mut impl WidgetManagerEnv,
        widget_id: i64,
        public_data: &str,
        cache_time: i32,
        mut promise: Promise<()>,
    ) {
        if widget_id == 0 {
            return promise.set_error(Status::error(400, "Invalid widget identifier specified"));
        }
        if cache_time != 0 && !(5..=3600).contains(&cache_time) {
            return promise.set_error(Status::error(400, "Invalid cache time specified"));
        }
        env.send_edit_widget_data_query(widget_id, public_data, cache_time);
        self.edit_widget_data_promises.push(promise);
    }

    /// `WidgetManager::get_widget_public_data` (WidgetManager.cpp:463-476)
    /// — the getWidgetPublicData band; the hash is the locally cached
    /// public-data hash.
    pub fn get_widget_public_data(
        &mut self,
        env: &mut impl WidgetManagerEnv,
        message_full_id: MessageFullId,
        mut promise: Promise<td_api::WidgetPublicData>,
    ) {
        let dialog_id = message_full_id.get_dialog_id();
        let message_id = message_full_id.get_message_id();
        if let Err(status) = env.check_dialog_access(dialog_id) {
            return promise.set_error(status);
        }
        let (_, public_data_hash) = match env.get_message_widget_public_data(message_full_id) {
            Ok(public_data) => public_data,
            Err(status) => return promise.set_error(status),
        };
        if !message_id.is_server() {
            return promise.set_error(Status::error(400, "Wrong message identifier"));
        }
        env.send_get_widget_public_data_query(message_full_id, public_data_hash);
        self.get_public_data_promises.push(promise);
    }

    /// `WidgetManager::on_get_widget_public_data`
    /// (WidgetManager.cpp:478-500) — the GetWidgetPublicDataQuery answer
    /// tail.
    pub fn on_get_widget_public_data(
        &mut self,
        env: &mut impl WidgetManagerEnv,
        message_full_id: MessageFullId,
        public_data: telegram_api::messages_WidgetPublicData,
        mut promise: Promise<td_api::WidgetPublicData>,
    ) {
        match public_data {
            telegram_api::messages_WidgetPublicData::messages_widgetPublicDataNotModified(data) => {
                let (current_public_data, _) =
                    match env.get_message_widget_public_data(message_full_id) {
                        Ok(public_data) => public_data,
                        Err(status) => return promise.set_error(status),
                    };
                promise.set_value(td_api::WidgetPublicData {
                    data: current_public_data,
                    poll_period: data.period.max(0),
                });
            }
            telegram_api::messages_WidgetPublicData::messages_widgetPublicData(data) => {
                env.on_update_message_widget_data(
                    message_full_id,
                    data.data.clone(),
                    data.hash,
                    String::new(),
                    false,
                );
                promise.set_value(td_api::WidgetPublicData {
                    data: data.data,
                    poll_period: data.period.max(0),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;

    struct TestEnv {
        is_bot: bool,
        known_users: Vec<UserId>,
        updates: Vec<td_api::Update>,
        inline_ids: Vec<String>,
    }

    impl TestEnv {
        fn new(is_bot: bool) -> TestEnv {
            TestEnv {
                is_bot,
                known_users: vec![UserId::new(9)],
                updates: Vec::new(),
                inline_ids: Vec::new(),
            }
        }
    }

    impl WidgetManagerEnv for TestEnv {
        fn auth_is_bot(&self) -> bool {
            self.is_bot
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn check_dialog_access(&mut self, dialog_id: DialogId) -> Result<(), Status> {
            if dialog_id.is_valid() {
                Ok(())
            } else {
                Err(Status::error(400, "Chat not found"))
            }
        }

        fn have_min_user(&mut self, user_id: UserId) -> bool {
            self.known_users.contains(&user_id)
        }

        fn get_message_widget_public_data(
            &mut self,
            _message_full_id: MessageFullId,
        ) -> Result<(String, i64), Status> {
            Ok(("cached".to_owned(), 1234))
        }

        fn on_update_message_widget_data(
            &mut self,
            _message_full_id: MessageFullId,
            _public_data: String,
            _public_data_hash: i64,
            _chat_data: String,
            _update_chat_data: bool,
        ) {
        }

        fn get_inline_message_id(
            &mut self,
            _inline_message_id: telegram_api::InputBotInlineMessageID,
        ) -> String {
            self.inline_ids.pop().unwrap_or_default()
        }
    }

    fn record<T: Send + Default + 'static>() -> (Promise<T>, Arc<Mutex<Option<T>>>) {
        let recorded = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&recorded);
        (
            tdutils::promise::PromiseCreator::lambda(move |value| {
                *sink.lock().unwrap() = Some(value);
            }),
            recorded,
        )
    }

    #[test]
    fn input_widget_query_type_builds_the_wire_shapes() {
        // the null wallet is the "Wallet must be non-empty" band
        let error = get_input_user_wallet(None).unwrap_err();
        assert_eq!(error.message(), "Wallet must be non-empty");

        // the sign-data arm wraps the data in a dataJSON and passes the
        // wallet through
        let wire = get_input_widget_query_type(Some(
            td_api::WidgetQueryType::widgetQueryTypeSignData(td_api::widgetQueryTypeSignData {
                wallet: Some(td_api::WidgetQueryWallet {
                    address: "0:0000000000000000000000000000000000000000000000000000000000000000"
                        .to_owned(),
                    gram_amount: 5,
                    public_key: Vec::new(),
                    state_init: Vec::new(),
                }),
                sign_data: "{\"a\":1}".to_owned(),
            }),
        ))
        .unwrap();
        let telegram_api::WidgetQueryType::widgetQueryTypeSignData(sign) = wire else {
            panic!("the sign-data arm must build")
        };
        assert_eq!(sign.sign_data.unwrap().data, "{\"a\":1}");
        assert_eq!(sign.wallet.unwrap().balance, 5);

        // the negative-balance and key/state-init bands
        let error = get_input_user_wallet(Some(td_api::WidgetQueryWallet {
            address: "0:0000000000000000000000000000000000000000000000000000000000000000"
                .to_owned(),
            gram_amount: -1,
            public_key: Vec::new(),
            state_init: Vec::new(),
        }))
        .unwrap_err();
        assert_eq!(error.message(), "Invalid wallet balance specified");
        let error = get_input_user_wallet(Some(td_api::WidgetQueryWallet {
            address: "0:0000000000000000000000000000000000000000000000000000000000000000"
                .to_owned(),
            gram_amount: 0,
            public_key: vec![1],
            state_init: Vec::new(),
        }))
        .unwrap_err();
        assert_eq!(
            error.message(),
            "Wallet public key and state init must be specified together"
        );
    }

    #[test]
    fn edit_widget_data_runs_the_cache_time_ladder() {
        let mut env = TestEnv::new(true);
        let mut manager = WidgetManager::default();

        // the zero id
        let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&errors);
        let promise = tdutils::promise::PromiseCreator::lambda_result(move |result| {
            if let tdutils::status::Result::Err(status) = result {
                sink.lock().unwrap().push(status.message().to_owned());
            }
        });
        manager.edit_widget_data(&mut env, 0, "p", 0, promise);
        assert_eq!(
            errors.lock().unwrap().as_slice(),
            ["Invalid widget identifier specified"]
        );

        // the 5..3600 window (0 = the server default) parks one promise
        // per dispatch (the senders are net-inert here)
        for cache_time in [0, 5, 3600] {
            let (promise, _) = record();
            manager.edit_widget_data(&mut env, 42, "p", cache_time, promise);
        }
        assert_eq!(manager.edit_widget_data_promises.len(), 3);

        // the out-of-window values are rejected without parking
        for cache_time in [-1, 3, 3601] {
            let (promise, _) = record();
            manager.edit_widget_data(&mut env, 42, "p", cache_time, promise);
        }
        assert_eq!(manager.edit_widget_data_promises.len(), 3);
    }

    #[test]
    fn edit_message_widget_data_noops_and_parks() {
        let mut env = TestEnv::new(true);
        let mut manager = WidgetManager::default();
        let dialog_id = DialogId::from_user_id(UserId::new(9));
        let full_id = MessageFullId::new(dialog_id, MessageId::new(1 << 20));

        // both flags false — the immediate ok (cpp:443-445)
        let settled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&settled);
        let promise = tdutils::promise::PromiseCreator::lambda_result(move |result| match result {
            tdutils::status::Result::Ok(()) => sink.lock().unwrap().push("ok".to_owned()),
            tdutils::status::Result::Err(status) => {
                sink.lock().unwrap().push(status.message().to_owned())
            }
        });
        manager.edit_message_widget_data(&mut env, full_id, false, "", false, "", promise);
        assert_eq!(settled.lock().unwrap().as_slice(), ["ok"]);
        assert!(manager.edit_message_data_promises.is_empty());

        // a dispatch parks the promise (the senders are net-inert here)
        let (promise, _) = record();
        manager.edit_message_widget_data(&mut env, full_id, true, "p", false, "", promise);
        assert_eq!(manager.edit_message_data_promises.len(), 1);

        // non-server ids are rejected
        let local = MessageFullId::new(dialog_id, MessageId::new(-3));
        let (promise, recorded) = record::<()>();
        manager.edit_message_widget_data(&mut env, local, true, "p", false, "", promise);
        assert_eq!(manager.edit_message_data_promises.len(), 1);
        let _ = recorded; // the promise stays unsettled until drop
    }

    #[test]
    fn new_widget_query_guards_and_emits() {
        let mut env = TestEnv::new(true);
        let mut manager = WidgetManager::default();
        let query_type = Some(telegram_api::WidgetQueryType::widgetQueryTypeData(
            telegram_api::widgetQueryTypeData {
                flags: 0,
                with_wallet: false,
            },
        ));

        // the unknown sender only logs; the non-bot band drops
        manager.on_new_widget_query(
            &mut env,
            1,
            query_type.clone(),
            UserId::new(9),
            DialogId::from_user_id(UserId::new(9)),
            MessageId::new(1 << 20),
            b"d".to_vec(),
            42,
            "c".to_owned(),
            7,
            None,
        );
        assert_eq!(env.updates.len(), 1);
        let td_api::Update::updateNewWidgetQuery(update) = &env.updates[0] else {
            panic!("the emitter must answer updateNewWidgetQuery")
        };
        assert_eq!(update.data, b"d");
        assert_eq!(update.widget_id, 7);
        assert!(update.query_type.is_some());

        env.is_bot = false;
        manager.on_new_widget_query(
            &mut env,
            2,
            query_type,
            UserId::new(9),
            DialogId::from_user_id(UserId::new(9)),
            MessageId::new(1 << 20),
            Vec::new(),
            42,
            String::new(),
            7,
            None,
        );
        assert_eq!(env.updates.len(), 1);
    }

    #[test]
    fn widget_query_type_object_inverts_the_wire() {
        let wallet = telegram_api::UserWallet {
            flags: 0,
            address: "a".to_owned(),
            balance: -3,
            public_key: vec![1],
            state_init: vec![2],
        };
        let object = get_widget_query_type_object(
            telegram_api::WidgetQueryType::widgetQueryTypeSentTransaction(
                telegram_api::widgetQueryTypeSentTransaction {
                    wallet: Some(wallet),
                    msg_hash: "h".to_owned(),
                },
            ),
        );
        let td_api::WidgetQueryType::widgetQueryTypeSentTransaction(sent) = object else {
            panic!()
        };
        // the negative wire balance clamps to zero behind the error log
        assert_eq!(sent.wallet.unwrap().gram_amount, 0);
        assert_eq!(sent.msg_hash, "h");
    }
}
