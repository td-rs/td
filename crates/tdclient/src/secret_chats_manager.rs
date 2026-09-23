//! `td::SecretChatsManager` (SecretChatsManager.h/.cpp, M.50).
//!
//! The chat registry and the create/cancel/send flows with the C++
//! local validation; the actor plane (the DH exchange and the message
//! sequence machines of SecretChatActor) and the encrypted-message
//! network legs are [blocked: tde2e]/[blocked: net] seams with parked
//! promises. `SecretChatDb` is the pmc-keyed per-chat store.

use crate::secret_chat::{SecretChatId, SecretChatState};
use crate::user_manager::UserId;
use std::collections::HashMap;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `td::SecretChatDb` (SecretChatDb.h/.cpp) — the per-chat pmc store:
/// keys `secret<chat_id><key>`.
pub struct SecretChatDb {
    chat_id: i32,
    values: HashMap<String, Vec<u8>>,
}

impl SecretChatDb {
    /// `SecretChatDb(pmc, chat_id)` (SecretChatDb.cpp:10-12).
    pub fn new(chat_id: i32) -> SecretChatDb {
        SecretChatDb {
            chat_id,
            values: HashMap::new(),
        }
    }

    fn key(&self, value_key: &str) -> String {
        format!("secret{}{}", self.chat_id, value_key)
    }

    /// `SecretChatDb::set_value` (SecretChatDb.h:23-27).
    pub fn set_value(&mut self, value_key: &str, data: &[u8]) {
        self.values.insert(self.key(value_key), data.to_vec());
    }

    /// `SecretChatDb::erase_value` (SecretChatDb.h:28-32).
    pub fn erase_value(&mut self, value_key: &str) {
        self.values.remove(&self.key(value_key));
    }

    /// `SecretChatDb::get_value` (SecretChatDb.h:33-39).
    pub fn get_value(&self, value_key: &str) -> Option<Vec<u8>> {
        self.values.get(&self.key(value_key)).cloned()
    }
}

/// `td::SecretChat` — the manager's chat record.
#[derive(Clone, Debug, Default)]
pub struct SecretChat {
    pub secret_chat_id: SecretChatId,
    pub user_id: UserId,
    pub state: SecretChatState,
    pub is_outbound: bool,
    pub ttl: i32,
}

impl SecretChat {
    /// `SecretChat::is_valid` — the id carries the validity.
    pub fn is_valid(&self) -> bool {
        self.secret_chat_id.is_valid()
    }
}

/// The `G()`/`td_` surface the secret-chats plane reads through. The
/// [blocked: net]/[blocked: tde2e] tails park their promises.
pub trait SecretChatsEnv {
    fn is_authorized(&self) -> bool;
    fn use_secret_chats(&self) -> bool;

    /// `SecretChatsManager::create_chat` — the actor creation and the
    /// sendEncryptedService accept tail are [blocked: tde2e/net].
    fn send_create_secret_chat_query(
        &mut self,
        _user_id: UserId,
        _user_access_hash: i64,
        _promise: Promise<SecretChatId>,
    ) {
        tdutils::log_info!("Send messages.acceptEncryption (create) — [blocked: net]");
    }

    /// The createNewSecretChat chat-object answer — [blocked: net / the
    /// chat creation intake].
    fn send_create_secret_chat_chat_query(
        &mut self,
        _user_id: UserId,
        _promise: Promise<crate::td_api::Chat>,
    ) {
        tdutils::log_info!("createNewSecretChat — [blocked: net]");
    }

    /// `cancel_chat` — the sendEncryptedService discard tail.
    fn send_cancel_secret_chat_query(
        &mut self,
        _secret_chat_id: SecretChatId,
        _delete_history: bool,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send messages.discardEncryption (cancel) — [blocked: net]");
    }

    /// The send-message-family tails.
    fn send_encrypted_message_query(
        &mut self,
        _secret_chat_id: SecretChatId,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send messages.sendEncrypted — [blocked: net]");
    }
    fn send_encrypted_action_query(&mut self, _secret_chat_id: SecretChatId) {
        tdutils::log_info!("Send messages.sendEncryptedService (action) — [blocked: net]");
    }
    fn send_read_history_query(
        &mut self,
        _secret_chat_id: SecretChatId,
        _date: i32,
        _promise: Promise<()>,
    ) {
        tdutils::log_info!("Send messages.readEncryptedHistory — [blocked: net]");
    }

    /// The update outbox.
    fn send_update(&mut self, update: crate::td_api::Update) {
        tdutils::log_info!("send_update({update:?}) — seam default");
    }

    /// `get_secret_chat_object` — the td_api conversion of the record.
    fn get_secret_chat_object(
        &mut self,
        secret_chat: &SecretChat,
    ) -> Option<crate::td_api::SecretChat> {
        let _ = secret_chat;
        None
    }
}

/// `td::SecretChatsManager` (SecretChatsManager.h:19-72).
#[derive(Default)]
pub struct SecretChatsManager {
    chats: HashMap<SecretChatId, SecretChat>,
    /// The C++ `use_secret_chats_` flag.
    pub use_secret_chats: bool,
}

impl SecretChatsManager {
    /// `SecretChatsManager(parent, use_secret_chats)`
    /// (SecretChatsManager.cpp:23-25).
    pub fn new(use_secret_chats: bool) -> SecretChatsManager {
        SecretChatsManager {
            use_secret_chats,
            ..SecretChatsManager::default()
        }
    }

    /// `get_chat` — the registry read.
    pub fn get_chat(&self, secret_chat_id: SecretChatId) -> Option<&SecretChat> {
        self.chats.get(&secret_chat_id)
    }

    /// `on_update_chat` (SecretChatsManager.cpp:37-58) — the
    /// updateEncryption intake: pending/active/closed states fold into
    /// the registry.
    pub fn on_update_chat(
        &mut self,
        env: &mut impl SecretChatsEnv,
        secret_chat_id: SecretChatId,
        user_id: UserId,
        state: SecretChatState,
    ) {
        let chat = self
            .chats
            .entry(secret_chat_id)
            .or_insert_with(|| SecretChat {
                secret_chat_id,
                user_id,
                state: SecretChatState::Unknown,
                ..SecretChat::default()
            });
        chat.user_id = user_id;
        chat.state = state;
        env.send_update(crate::td_api::Update::updateSecretChat(
            crate::td_api::updateSecretChat { secret_chat: None },
        ));
    }

    /// `create_chat_chat_object` — the createNewSecretChat arm's
    /// chat-object answer: the C++ user_manager conversion parks
    /// [blocked: net / the chat creation intake].
    pub fn create_chat_chat_object(
        &mut self,
        env: &mut impl SecretChatsEnv,
        user_id: UserId,
        mut promise: Promise<crate::td_api::Chat>,
    ) {
        if !env.use_secret_chats() {
            promise.set_error(Status::error(400, "Secret chats are not supported"));
            return;
        }
        if !env.is_authorized() {
            promise.set_error(Status::error(400, "Not authorized"));
            return;
        }
        env.send_create_secret_chat_chat_query(user_id, promise);
    }

    /// `create_chat` (SecretChatsManager.cpp:112-121).
    pub fn create_chat(
        &mut self,
        env: &mut impl SecretChatsEnv,
        user_id: UserId,
        user_access_hash: i64,
        mut promise: Promise<SecretChatId>,
    ) {
        if !env.use_secret_chats() {
            promise.set_error(Status::error(400, "Secret chats are not supported"));
            return;
        }
        if !env.is_authorized() {
            promise.set_error(Status::error(400, "Not authorized"));
            return;
        }
        env.send_create_secret_chat_query(user_id, user_access_hash, promise);
    }

    /// `cancel_chat` (SecretChatsManager.cpp:122-130) — the close flow.
    pub fn cancel_chat(
        &mut self,
        env: &mut impl SecretChatsEnv,
        secret_chat_id: SecretChatId,
        delete_history: bool,
        mut promise: Promise<()>,
    ) {
        if !secret_chat_id.is_valid() {
            promise.set_error(Status::error(
                400,
                "Invalid secret chat identifier specified",
            ));
            return;
        }
        if let Some(chat) = self.chats.get_mut(&secret_chat_id) {
            chat.state = SecretChatState::Closed;
        }
        env.send_cancel_secret_chat_query(secret_chat_id, delete_history, promise);
    }

    /// `send_message` (SecretChatsManager.cpp:132-136) — [blocked: tde2e]
    /// for the layer conversion.
    pub fn send_message(
        &mut self,
        env: &mut impl SecretChatsEnv,
        secret_chat_id: SecretChatId,
        mut promise: Promise<()>,
    ) {
        if !secret_chat_id.is_valid() {
            promise.set_error(Status::error(
                400,
                "Invalid secret chat identifier specified",
            ));
            return;
        }
        env.send_encrypted_message_query(secret_chat_id, promise);
    }

    /// `send_message_action` (SecretChatsManager.cpp:138-140).
    pub fn send_message_action(
        &mut self,
        env: &mut impl SecretChatsEnv,
        secret_chat_id: SecretChatId,
    ) {
        if secret_chat_id.is_valid() {
            env.send_encrypted_action_query(secret_chat_id);
        }
    }

    /// `send_read_history` (SecretChatsManager.cpp:142-144).
    pub fn send_read_history(
        &mut self,
        env: &mut impl SecretChatsEnv,
        secret_chat_id: SecretChatId,
        date: i32,
        promise: Promise<()>,
    ) {
        if secret_chat_id.is_valid() {
            env.send_read_history_query(secret_chat_id, date, promise);
        }
    }

    /// `get_current_state` (SecretChatsManager.cpp:196-201).
    pub fn get_current_state(
        &mut self,
        env: &mut impl SecretChatsEnv,
        updates: &mut Vec<crate::td_api::Update>,
    ) {
        for chat in self.chats.values() {
            if let Some(object) = env.get_secret_chat_object(chat) {
                updates.push(crate::td_api::Update::updateSecretChat(
                    crate::td_api::updateSecretChat {
                        secret_chat: Some(object),
                    },
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdutils::promise::PromiseInterface;

    #[derive(Clone, Copy)]
    struct Absorbing;

    impl<T: Send + 'static> PromiseInterface<T> for Absorbing {
        fn set_value(&mut self, _value: T) {}
        fn set_error(&mut self, _error: Status) {}
    }

    struct TestEnv {
        authorized: bool,
        use_secret_chats: bool,
        updates: Vec<crate::td_api::Update>,
        queries: Vec<String>,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                authorized: true,
                use_secret_chats: true,
                updates: Vec::new(),
                queries: Vec::new(),
            }
        }
    }

    impl SecretChatsEnv for TestEnv {
        fn is_authorized(&self) -> bool {
            self.authorized
        }

        fn use_secret_chats(&self) -> bool {
            self.use_secret_chats
        }

        fn send_create_secret_chat_query(
            &mut self,
            user_id: UserId,
            _user_access_hash: i64,
            mut promise: Promise<SecretChatId>,
        ) {
            self.queries.push(format!("create {}", user_id.get()));
            promise.set_value(SecretChatId::new(1));
        }

        fn send_cancel_secret_chat_query(
            &mut self,
            secret_chat_id: SecretChatId,
            _delete_history: bool,
            mut promise: Promise<()>,
        ) {
            self.queries
                .push(format!("cancel {}", secret_chat_id.get()));
            promise.set_value(());
        }

        fn send_update(&mut self, update: crate::td_api::Update) {
            self.updates.push(update);
        }
    }

    #[test]
    fn create_chat_rejects_unsupported_secret_chats() {
        let mut manager = SecretChatsManager::new(true);
        let mut env = TestEnv {
            use_secret_chats: false,
            ..TestEnv::default()
        };
        let promise = Promise::from_interface(Absorbing);
        manager.create_chat(&mut env, UserId::new(5), 0, promise);
        assert!(env.queries.is_empty());
    }

    #[test]
    fn create_chat_queries_when_enabled() {
        let mut manager = SecretChatsManager::new(true);
        let mut env = TestEnv::default();
        let promise = Promise::from_interface(Absorbing);
        manager.create_chat(&mut env, UserId::new(5), 42, promise);
        assert_eq!(env.queries, vec!["create 5".to_owned()]);
    }

    #[test]
    fn cancel_chat_closes_the_record() {
        let mut manager = SecretChatsManager::new(true);
        let mut env = TestEnv::default();
        manager.on_update_chat(
            &mut env,
            SecretChatId::new(9),
            UserId::new(5),
            SecretChatState::Active,
        );
        let promise = Promise::from_interface(Absorbing);
        manager.cancel_chat(&mut env, SecretChatId::new(9), false, promise);
        assert_eq!(
            manager.get_chat(SecretChatId::new(9)).unwrap().state,
            SecretChatState::Closed
        );
    }

    #[test]
    fn secret_chat_db_round_trips() {
        let mut db = SecretChatDb::new(7);
        assert_eq!(db.get_value("key"), None);
        db.set_value("key", b"value");
        assert_eq!(db.get_value("key").unwrap(), b"value");
        db.erase_value("key");
        assert_eq!(db.get_value("key"), None);
    }
}
