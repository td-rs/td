//! `td::SecretChatsManager` (SecretChatsManager.h/.cpp, M.50).
//!
//! The chat registry and the create/cancel/send flows with the C++
//! local validation. The five wire families — `messages.requestEncryption`
//! (the create tail), `messages.acceptEncryption`, `messages.discardEncryption`,
//! `messages.readEncryptedHistory` and `messages.sendEncrypted`/`Service` —
//! ride the `SecretNetKind` dispatcher plane over the ported
//! `DhHandshake` machinery (tdmtproto): the queries are plain
//! telegram_api functions on the main DC with auth on
//! (SecretChatActor.cpp:68-71) and the answers drain through
//! `Td::on_secret_net_answer`. The actor plane (the SecretChatActor
//! binlog/PFS/seq state machines) and the encrypted-message payload
//! codec stay [blocked: tde2e] seams. `SecretChatDb` is the pmc-keyed
//! per-chat store.

use crate::dh_cache::{DhCache, DhConfig};
use crate::secret_chat::{SecretChatId, SecretChatState};
use crate::user_manager::UserId;
use std::collections::HashMap;
use tdmtproto::dh_handshake::DhHandshake;
use tdmtproto::telegram_api;
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

/// `td::SecretChat` — the manager's chat record with the collapsed
/// actor auth-state (`access_hash` is the C++ `auth_state_.access_hash`
/// the `save_common_info` folds keep, cpp:1732-1741;
/// `last_read_history_date_` rides the actor, cpp:331).
#[derive(Clone, Debug, Default)]
pub struct SecretChat {
    pub secret_chat_id: SecretChatId,
    pub user_id: UserId,
    pub state: SecretChatState,
    pub is_outbound: bool,
    pub ttl: i32,
    pub access_hash: i64,
    pub last_read_history_date: i32,
}

impl SecretChat {
    /// `SecretChat::is_valid` — the id carries the validity.
    pub fn is_valid(&self) -> bool {
        self.secret_chat_id.is_valid()
    }
}

/// The SecretChatsManager/SecretChatActor query families riding the
/// dispatcher — the C++ `QueryType` enum (SecretChatActor.h:466) riding
/// the `create_net_query` shape (SecretChatActor.cpp:68-71): plain
/// telegram_api functions, main DC, auth on. The answers drain through
/// `Td::on_secret_net_answer`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SecretNetKind {
    /// The create tail: `messages.requestEncryption` (SendRequest,
    /// SecretChatActor.cpp:507-516). The answer is the `EncryptedChat`
    /// union (on_update_chat, cpp:1805-1815); the create entry it
    /// resolves parks in [`PendingSecretQueries::creates`].
    RequestEncryption,
    /// `messages.acceptEncryption` (SendAccept, SecretChatActor.cpp:517-529);
    /// the same union answer. The id rides for the fatal-error band
    /// (the C++ answers route through the chat's actor, cpp:626-640).
    AcceptEncryption { secret_chat_id: i32 },
    /// `messages.discardEncryption` (do_close_chat_impl,
    /// SecretChatActor.cpp:707-711). The C++ sends it ordered over the
    /// chat's SequenceDispatcher with a one-year timeout budget; the
    /// Rust net plane has no SequenceDispatcher (net_query.rs:18) —
    /// this dispatch is unordered (the documented divergence).
    DiscardEncryption,
    /// `messages.readEncryptedHistory` (send_read_history,
    /// SecretChatActor.cpp:307-338). The generation pairs the answer
    /// with its parked receipt the way the C++ `read_history_query_`
    /// generation check does (cpp:1823-1828).
    ReadHistory { generation: u64 },
    /// `messages.sendEncrypted`/`messages.sendEncryptedService`
    /// (create_net_query(OutboundSecretMessage), SecretChatActor.cpp:1386-1404).
    SendEncrypted { service: bool },
}

/// One in-flight `messages.requestEncryption` create — the collapsed
/// actor auth-state the answer fold needs (the C++ `auth_state_` peer
/// fields, SecretChatActor.cpp:752-757, plus the request handshake).
pub(crate) struct PendingSecretChatCreate {
    /// The create receipt id — the C++ `SecretChatId(random_id)`
    /// (SecretChatsManager.cpp:99-104).
    pub(crate) random_id: i32,
    pub(crate) user_id: UserId,
    /// The request handshake: `set_config` at the send, `set_g_a` +
    /// `gen_key` at the answer (cpp:1769-1798).
    pub(crate) handshake: DhHandshake,
}

/// The parked query promises — the C++ actor members the `Copy`
/// [`SecretNetKind`]s can't carry. FIFO-paired per family (the §5
/// fidelity debt; the C++ carries the promise on the query itself).
#[derive(Default)]
pub(crate) struct PendingSecretQueries {
    /// The in-flight creates — one per dispatched
    /// `messages.requestEncryption`. The create receipt promise settles
    /// at the dispatch (cpp:124), so these carry only the answer fold
    /// state.
    pub(crate) creates: Vec<PendingSecretChatCreate>,
    /// `discard_encryption_promise_` (cpp:712-715) — the close
    /// receipts, FIFO-paired with the discard queries (the C++
    /// absorbs every outcome into `Unit`, cpp:113-117).
    pub(crate) discards: Vec<Promise<()>>,
    /// The `read_history_promise_` receipts keyed by their query
    /// generation (cpp:322-332).
    pub(crate) read_history: Vec<(u64, Promise<()>)>,
    /// The outbound send receipts (`on_send_message_ok` routes the
    /// `MessageId(ServerMessageId(random_id))` through MessagesManager,
    /// cpp:1560-1580 — unported; the port settles `Unit`).
    pub(crate) sends: Vec<Promise<()>>,
}

/// The planted accept handshake (the C++ `auth_state_.handshake` in the
/// `WaitAcceptResponse` state): the `gen_key` fingerprint the answer
/// checks (cpp:522-525, 1774-1779).
pub(crate) struct PendingAcceptHandshake {
    pub(crate) key_fingerprint: i64,
}

/// The `G()`/`td_` surface the secret-chats plane reads through. The
/// payload-encryption half of the send legs stays [blocked: tde2e].
pub trait SecretChatsEnv {
    fn is_authorized(&self) -> bool;
    fn use_secret_chats(&self) -> bool;

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
    /// The manager-held dispatch surface (the `CallManager::net` shape)
    /// — `None` keeps the net-inert posture (the pre-init windows and
    /// the net-inert tests).
    pub(crate) net: Option<crate::td::SecretNetHandle>,
    /// The parked query promises (the [`PendingSecretQueries`] lots).
    pub(crate) pending: PendingSecretQueries,
    /// `G()->get_dh_config()` (make_secret_chat_context,
    /// SecretChatsManager.cpp:341-346) — seeded with the built-in group
    /// ([`DhCache::built_in_dh_config`]); the `messages.getDhConfig`
    /// fetch family is not in this slice, the seed keeps the
    /// `has_config` gate honest.
    pub(crate) dh_config: DhConfig,
    /// The accept-path key fingerprints keyed by chat id — planted by
    /// the `encryptedChatRequested` intake, checked by the accept
    /// answer (the C++ `auth_state_.handshake` between SendAccept and
    /// the answer, cpp:517-529, 1774-1779).
    pub(crate) accept_handshakes: HashMap<i32, PendingAcceptHandshake>,
    /// The `read_history_query_` generation counter (cpp:322-332).
    next_read_history_generation: u64,
}

impl SecretChatsManager {
    /// `SecretChatsManager(parent, use_secret_chats)`
    /// (SecretChatsManager.cpp:23-25).
    pub fn new(use_secret_chats: bool) -> SecretChatsManager {
        SecretChatsManager {
            use_secret_chats,
            dh_config: DhCache::built_in_dh_config(),
            ..SecretChatsManager::default()
        }
    }

    /// `get_chat` — the registry read.
    pub fn get_chat(&self, secret_chat_id: SecretChatId) -> Option<&SecretChat> {
        self.chats.get(&secret_chat_id)
    }

    /// The `handshake.has_config()` gate (cpp:509-511, 519-520) over the
    /// seeded `G()->get_dh_config()` analog.
    fn has_dh_config(&self) -> bool {
        !self.dh_config.prime.is_empty()
    }

    /// The `Random::secure_int32() & 0x7fffffff` create receipt id
    /// (SecretChatsManager.cpp:99-104). The C++ retries on an actor-id
    /// collision; the port has no actor map to collide with.
    fn next_secret_chat_random_id() -> i32 {
        tdutils::random::secure_int32() & 0x7fffffff
    }

    /// The `td_->create_handler<…>()->send(...)` collapse — the wire
    /// query construction lives at the manager send sites; `None` keeps
    /// the net-inert posture (the tests drive the answer halves).
    fn send_secret_net_query(
        &self,
        kind: SecretNetKind,
        function: telegram_api::Function,
        what: &str,
    ) {
        match &self.net {
            Some(net) => net.dispatch(kind, function),
            None => tdutils::log_info!("Send {what} — [net-inert]"),
        }
    }

    /// `on_update_chat` (SecretChatsManager.cpp:37-58) with the actor
    /// `save_common_info` fold (cpp:1732-1741): the id/user/state pair
    /// plus the answer's access hash fold into the registry.
    pub fn on_update_chat(
        &mut self,
        env: &mut impl SecretChatsEnv,
        secret_chat_id: SecretChatId,
        user_id: UserId,
        access_hash: i64,
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
        chat.access_hash = access_hash;
        env.send_update(crate::td_api::Update::updateSecretChat(
            crate::td_api::updateSecretChat { secret_chat: None },
        ));
    }

    /// `SecretChatActor::on_update_chat(encryptedChatRequested)`
    /// (cpp:1740-1760) — the inbound-request intake: the record folds
    /// Waiting and the SendAccept tail runs (cpp:517-529): the
    /// handshake checks `g_a`, derives the key and dispatches
    /// `messages.acceptEncryption`.
    pub fn on_encrypted_chat_requested(
        &mut self,
        env: &mut impl SecretChatsEnv,
        secret_chat_id: SecretChatId,
        user_id: UserId,
        access_hash: i64,
        g_a: &[u8],
    ) {
        self.on_update_chat(
            env,
            secret_chat_id,
            user_id,
            access_hash,
            SecretChatState::Waiting,
        );
        if !self.has_dh_config() {
            // The C++ SendAccept waits for the config (cpp:519-520).
            tdutils::log_info!("Accept secret chat waits for the DH config");
            return;
        }
        let mut handshake = DhHandshake::new();
        handshake.set_config(self.dh_config.g, &self.dh_config.prime);
        handshake.set_g_a(g_a);
        if handshake.run_checks(true, &DhCache::instance()).is_error() {
            // The SendAccept failure is fatal (cpp:626-640): the chat
            // closes and the discard tail fires.
            self.on_secret_net_fatal(env, secret_chat_id, Some(user_id));
            tdutils::log_info!("Accept secret chat handshake failed");
            return;
        }
        let (key_fingerprint, _key) = handshake.gen_key();
        // The derived auth key persists with the actor plane —
        // [blocked: tde2e]; the fingerprint is what the answer checks.
        self.accept_handshakes.insert(
            secret_chat_id.get(),
            PendingAcceptHandshake { key_fingerprint },
        );
        self.send_secret_net_query(
            SecretNetKind::AcceptEncryption {
                secret_chat_id: secret_chat_id.get(),
            },
            telegram_api::Function::messages_acceptEncryption(
                telegram_api::messages_acceptEncryption {
                    peer: Some(telegram_api::InputEncryptedChat {
                        chat_id: secret_chat_id.get(),
                        access_hash,
                    }),
                    g_b: handshake.get_g_b(),
                    key_fingerprint,
                },
            ),
            "messages.acceptEncryption",
        );
    }

    /// `create_chat_chat_object` — the createNewSecretChat arm's
    /// chat-object answer: the C++ chains the local object build onto
    /// the create receipt (`force_create_dialog` + `get_chat_object`,
    /// UserManager.cpp:9864-9870). The dialog-plane object build is not
    /// in this slice — the promise drops here (the request stays
    /// unanswered, the `createPrivateChat` park convention) while the
    /// wire create proceeds.
    pub fn create_chat_chat_object(
        &mut self,
        env: &mut impl SecretChatsEnv,
        user_id: UserId,
        user_access_hash: i64,
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
        drop(promise);
        let random_id = Self::next_secret_chat_random_id();
        self.send_create_chat_query(random_id, user_id, user_access_hash);
    }

    /// `create_chat` (SecretChatsManager.cpp:98-106) — the create
    /// receipt settles with `SecretChatId(random_id)` at the dispatch
    /// (cpp:124); the wire answer only folds the chat state.
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
        let random_id = Self::next_secret_chat_random_id();
        promise.set_value(SecretChatId::new(random_id));
        self.send_create_chat_query(random_id, user_id, user_access_hash);
    }

    /// The shared `messages.requestEncryption` tail — `SendRequest`
    /// (SecretChatActor.cpp:507-516): the handshake is seeded from
    /// `G()->get_dh_config()` and its `g_b` rides as the request `g_a`.
    fn send_create_chat_query(&mut self, random_id: i32, user_id: UserId, user_access_hash: i64) {
        if !self.has_dh_config() {
            // The C++ SendRequest waits for the config (cpp:509-511).
            tdutils::log_info!("Create secret chat waits for the DH config");
            return;
        }
        let mut handshake = DhHandshake::new();
        handshake.set_config(self.dh_config.g, &self.dh_config.prime);
        let g_a = handshake.get_g_b();
        self.pending.creates.push(PendingSecretChatCreate {
            random_id,
            user_id,
            handshake,
        });
        self.send_secret_net_query(
            SecretNetKind::RequestEncryption,
            telegram_api::Function::messages_requestEncryption(
                telegram_api::messages_requestEncryption {
                    user_id: Some(telegram_api::InputUser::inputUser(
                        telegram_api::inputUser {
                            user_id: user_id.get(),
                            access_hash: user_access_hash,
                        },
                    )),
                    random_id,
                    g_a,
                },
            ),
            "messages.requestEncryption",
        );
    }

    /// `cancel_chat` (SecretChatsManager.cpp:108-112 with the actor
    /// close tail, cpp:692-722) — the close flow: the record folds
    /// Closed, the `messages.discardEncryption` tail dispatches and the
    /// receipt parks (the C++ absorbs every outcome into `Unit`,
    /// cpp:113-117). A chat with no record absorbs silently (the C++
    /// empty-actor no-op).
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
        let Some(chat) = self.chats.get(&secret_chat_id) else {
            // The empty-actor no-op — the C++ SafePromise absorbs into
            // `Unit` (SecretChatsManager.cpp:108-112).
            promise.set_value(());
            return;
        };
        let (user_id, access_hash) = (chat.user_id, chat.access_hash);
        // The close folds Closed and emits the chat update (the C++
        // send_update_secret_chat on the close tail, cpp:701).
        self.on_update_chat(
            env,
            secret_chat_id,
            user_id,
            access_hash,
            SecretChatState::Closed,
        );
        self.pending.discards.push(promise);
        self.send_discard_encryption_query(secret_chat_id, delete_history);
    }

    /// `messages.discardEncryption` (do_close_chat_impl,
    /// SecretChatActor.cpp:707-711). The C++ rides the chat's
    /// SequenceDispatcher with a one-year timeout; the port dispatches
    /// unordered over the plain handle (the documented divergence).
    fn send_discard_encryption_query(&self, secret_chat_id: SecretChatId, delete_history: bool) {
        self.send_secret_net_query(
            SecretNetKind::DiscardEncryption,
            telegram_api::Function::messages_discardEncryption(
                telegram_api::messages_discardEncryption {
                    flags: 0,
                    delete_history,
                    chat_id: secret_chat_id.get(),
                },
            ),
            "messages.discardEncryption",
        );
    }

    /// `send_message` (SecretChatsManager.cpp:114-120) — the wire leg
    /// of the send-message family. The payload buffer is the
    /// [blocked: tde2e] codec's product (`create_encrypted_message`,
    /// SecretChatActor.cpp:215-243); the leg rides with the placeholder
    /// empty buffer until the codec lands (no production caller yet).
    pub fn send_message(
        &mut self,
        _env: &mut impl SecretChatsEnv,
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
        let Some(chat) = self.chats.get(&secret_chat_id) else {
            // The manager SafePromise band (cpp:114-120).
            promise.set_error(Status::error(400, "Can't find secret chat"));
            return;
        };
        if chat.state == SecretChatState::Closed {
            promise.set_error(Status::error(400, "Chat is closed"));
            return;
        }
        if chat.state != SecretChatState::Active {
            // The actor Ready gate (cpp:294-297).
            promise.set_error(Status::error(400, "Can't access the chat"));
            return;
        }
        let access_hash = chat.access_hash;
        self.pending.sends.push(promise);
        self.send_secret_net_query(
            SecretNetKind::SendEncrypted { service: false },
            telegram_api::Function::messages_sendEncrypted(telegram_api::messages_sendEncrypted {
                flags: 0,
                silent: false,
                peer: Some(telegram_api::InputEncryptedChat {
                    chat_id: secret_chat_id.get(),
                    access_hash,
                }),
                random_id: tdutils::random::secure_int64(),
                data: Vec::new(),
            }),
            "messages.sendEncrypted",
        );
    }

    /// `send_message_action` (SecretChatsManager.cpp:122-128) — the
    /// port maps the service-message leg onto this seam:
    /// `messages.sendEncryptedService` (SecretChatActor.cpp:1386-1404,
    /// the `need_notify_user` arm), fire-and-forget like the C++
    /// action. The C++ typing action itself (`messages.setEncryptedTyping`,
    /// SecretChatActor.cpp:289-305, QueryType::Ignore) is not one of
    /// the five ticket families and stays parked. Not-Ready chats
    /// ignore the action (cpp:294-297).
    pub fn send_message_action(
        &mut self,
        _env: &mut impl SecretChatsEnv,
        secret_chat_id: SecretChatId,
    ) {
        let Some(chat) = self.chats.get(&secret_chat_id) else {
            // The empty-actor no-op (SecretChatsManager.cpp:125-127).
            return;
        };
        if chat.state != SecretChatState::Active {
            tdutils::log_info!("Ignore send_message_action: the chat is not Ready");
            return;
        }
        let access_hash = chat.access_hash;
        self.send_secret_net_query(
            SecretNetKind::SendEncrypted { service: true },
            telegram_api::Function::messages_sendEncryptedService(
                telegram_api::messages_sendEncryptedService {
                    peer: Some(telegram_api::InputEncryptedChat {
                        chat_id: secret_chat_id.get(),
                        access_hash,
                    }),
                    random_id: tdutils::random::secure_int64(),
                    data: Vec::new(),
                },
            ),
            "messages.sendEncryptedService",
        );
    }

    /// `send_read_history` (SecretChatsManager.cpp:130-134 with the
    /// actor tail, cpp:307-338): the close/Ready guards, the
    /// `date <= last_read_history_date_` dedup (cpp:319-320), the
    /// previous receipt settle (cpp:322-326) and the dispatch.
    pub fn send_read_history(
        &mut self,
        _env: &mut impl SecretChatsEnv,
        secret_chat_id: SecretChatId,
        date: i32,
        mut promise: Promise<()>,
    ) {
        let Some(chat) = self.chats.get(&secret_chat_id) else {
            // The manager SafePromise band (cpp:130-134); an invalid id
            // has no actor either.
            promise.set_error(Status::error(400, "Can't find secret chat"));
            return;
        };
        if chat.state == SecretChatState::Closed {
            promise.set_error(Status::error(400, "Chat is closed"));
            return;
        }
        if chat.state != SecretChatState::Active {
            // The actor Ready gate (cpp:313-316).
            promise.set_error(Status::error(400, "Can't access the chat"));
            return;
        }
        if date <= chat.last_read_history_date {
            promise.set_value(());
            return;
        }
        // The C++ cancels the previous read query and settles its
        // promise (cpp:322-326); the net plane has no query-cancel —
        // the parked previous receipt settles Unit (the observable
        // half) and the generation keeps the stale answers away.
        for (_, mut previous) in std::mem::take(&mut self.pending.read_history) {
            previous.set_value(());
        }
        let generation = self.next_read_history_generation;
        self.next_read_history_generation += 1;
        self.pending.read_history.push((generation, promise));
        let access_hash = chat.access_hash;
        let chat = self
            .chats
            .get_mut(&secret_chat_id)
            .expect("the read checked");
        chat.last_read_history_date = date;
        self.send_secret_net_query(
            SecretNetKind::ReadHistory { generation },
            telegram_api::Function::messages_readEncryptedHistory(
                telegram_api::messages_readEncryptedHistory {
                    peer: Some(telegram_api::InputEncryptedChat {
                        chat_id: secret_chat_id.get(),
                        access_hash,
                    }),
                    max_date: date,
                },
            ),
            "messages.readEncryptedHistory",
        );
    }

    /// The requestEncryption answer intake — the C++
    /// `on_update_chat(NetQueryPtr)` over the create entry
    /// (cpp:1805-1815 with the union arms 1740-1803).
    pub(crate) fn on_secret_net_create_answer(
        &mut self,
        env: &mut impl SecretChatsEnv,
        mut pending: PendingSecretChatCreate,
        answer: Result<telegram_api::EncryptedChat, Status>,
    ) {
        let chat = match answer {
            Ok(chat) => chat,
            Err(_) => {
                // The check_status fatal band (cpp:626-640): the close
                // fires the discard tail for the receipt id.
                self.on_secret_net_fatal(
                    env,
                    SecretChatId::new(pending.random_id),
                    Some(pending.user_id),
                );
                return;
            }
        };
        match chat {
            telegram_api::EncryptedChat::encryptedChatWaiting(update) => {
                // save_common_info + the state stays WaitRequestResponse
                // (cpp:1748-1755).
                self.on_update_chat(
                    env,
                    SecretChatId::new(update.id),
                    pending.user_id,
                    update.access_hash,
                    SecretChatState::Waiting,
                );
            }
            telegram_api::EncryptedChat::encryptedChat(update) => {
                // save_common_info + set_g_a + gen_key + the fingerprint
                // check (cpp:1756-1798); a failure is the C++ fatal
                // "Key fingerprint mismatch" band (cpp:1777-1779).
                let secret_chat_id = SecretChatId::new(update.id);
                pending.handshake.set_g_a(&update.g_a_or_b);
                let key_fingerprint = if pending
                    .handshake
                    .run_checks(true, &DhCache::instance())
                    .is_error()
                {
                    None
                } else {
                    Some(pending.handshake.gen_key().0)
                };
                if key_fingerprint != Some(update.key_fingerprint) {
                    self.on_secret_net_fatal(env, secret_chat_id, Some(pending.user_id));
                    return;
                }
                self.on_update_chat(
                    env,
                    secret_chat_id,
                    pending.user_id,
                    update.access_hash,
                    SecretChatState::Active,
                );
            }
            telegram_api::EncryptedChat::encryptedChatRequested(update) => {
                // The create actor ignores a Requested answer (the
                // state must be Empty, cpp:1743-1747).
                tdutils::log_info!(
                    "Unexpected encryptedChatRequested ignored: id {}",
                    update.id
                );
            }
            telegram_api::EncryptedChat::encryptedChatDiscarded(update) => {
                // The C++ cancel_chat(history_deleted, already
                // discarded) — no discard query fires (cpp:1800-1803).
                self.on_update_chat(
                    env,
                    SecretChatId::new(update.id),
                    pending.user_id,
                    0,
                    SecretChatState::Closed,
                );
            }
            telegram_api::EncryptedChat::encryptedChatEmpty(update) => {
                tdutils::log_info!("Unexpected encryptedChatEmpty ignored: id {}", update.id);
            }
        }
    }

    /// The acceptEncryption answer intake — the same union arms over
    /// the `WaitAcceptResponse` state (the key derived at SendAccept,
    /// only the fingerprint checks here, cpp:1769-1803).
    pub(crate) fn on_secret_net_accept_answer(
        &mut self,
        env: &mut impl SecretChatsEnv,
        secret_chat_id: SecretChatId,
        answer: Result<telegram_api::EncryptedChat, Status>,
    ) {
        let chat = match answer {
            Ok(chat) => chat,
            Err(_) => {
                // The check_status fatal band (cpp:626-640).
                self.on_secret_net_fatal(env, secret_chat_id, None);
                return;
            }
        };
        match chat {
            telegram_api::EncryptedChat::encryptedChat(update) => {
                let Some(planted) = self.accept_handshakes.remove(&secret_chat_id.get()) else {
                    tdutils::log_info!("Accept answer without the planted handshake");
                    return;
                };
                let secret_chat_id = SecretChatId::new(update.id);
                if planted.key_fingerprint != update.key_fingerprint {
                    // The C++ fatal "Key fingerprint mismatch"
                    // (cpp:1777-1779).
                    let user_id = self
                        .chats
                        .get(&secret_chat_id)
                        .map(|chat| chat.user_id)
                        .unwrap_or_default();
                    self.on_secret_net_fatal(env, secret_chat_id, Some(user_id));
                    return;
                }
                let user_id = self
                    .chats
                    .get(&secret_chat_id)
                    .map(|chat| chat.user_id)
                    .unwrap_or_default();
                self.on_update_chat(
                    env,
                    secret_chat_id,
                    user_id,
                    update.access_hash,
                    SecretChatState::Active,
                );
            }
            telegram_api::EncryptedChat::encryptedChatWaiting(update) => {
                // save_common_info only (cpp:1748-1755).
                let user_id = self
                    .chats
                    .get(&SecretChatId::new(update.id))
                    .map(|chat| chat.user_id)
                    .unwrap_or_default();
                self.on_update_chat(
                    env,
                    SecretChatId::new(update.id),
                    user_id,
                    update.access_hash,
                    SecretChatState::Waiting,
                );
            }
            telegram_api::EncryptedChat::encryptedChatRequested(update) => {
                tdutils::log_info!(
                    "Unexpected encryptedChatRequested ignored: id {}",
                    update.id
                );
            }
            telegram_api::EncryptedChat::encryptedChatDiscarded(update) => {
                // The C++ cancel_chat(history_deleted, already
                // discarded) (cpp:1800-1803); the planted handshake
                // dies with the chat.
                self.accept_handshakes.remove(&update.id);
                let user_id = self
                    .chats
                    .get(&SecretChatId::new(update.id))
                    .map(|chat| chat.user_id)
                    .unwrap_or_default();
                let access_hash = self
                    .chats
                    .get(&SecretChatId::new(update.id))
                    .map(|chat| chat.access_hash)
                    .unwrap_or_default();
                self.on_update_chat(
                    env,
                    SecretChatId::new(update.id),
                    user_id,
                    access_hash,
                    SecretChatState::Closed,
                );
            }
            telegram_api::EncryptedChat::encryptedChatEmpty(update) => {
                tdutils::log_info!("Unexpected encryptedChatEmpty ignored: id {}", update.id);
            }
        }
    }

    /// The `on_fatal_error` band (cpp:635-640): the record folds Closed
    /// (planted when the chat is new — the C++ close still emits the
    /// update) and the discard tail fires for the chat id.
    pub(crate) fn on_secret_net_fatal(
        &mut self,
        env: &mut impl SecretChatsEnv,
        secret_chat_id: SecretChatId,
        user_id: Option<UserId>,
    ) {
        let (fold_user, access_hash) = match self.chats.get(&secret_chat_id) {
            Some(chat) => (chat.user_id, chat.access_hash),
            None => (user_id.unwrap_or_default(), 0),
        };
        self.on_update_chat(
            env,
            secret_chat_id,
            fold_user,
            access_hash,
            SecretChatState::Closed,
        );
        self.send_discard_encryption_query(secret_chat_id, false);
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
    use tdutils::status::Result;

    type SecretChatIdSlot = std::sync::Arc<std::sync::Mutex<Option<Result<SecretChatId>>>>;

    /// The create-receipt slot — the receipt settles at the dispatch
    /// (cpp:124), net or no net.
    fn record_secret_chat_id() -> (Promise<SecretChatId>, SecretChatIdSlot) {
        struct Rec(SecretChatIdSlot);
        impl PromiseInterface<SecretChatId> for Rec {
            fn set_value(&mut self, value: SecretChatId) {
                *self.0.lock().unwrap() = Some(Result::Ok(value));
            }
            fn set_error(&mut self, error: Status) {
                *self.0.lock().unwrap() = Some(Result::Err(error));
            }
        }
        let slot: SecretChatIdSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
        (Promise::from_interface(Rec(slot.clone())), slot)
    }

    type UnitSlot = std::sync::Arc<std::sync::Mutex<Option<Result<()>>>>;

    fn record_unit() -> (Promise<()>, UnitSlot) {
        struct Rec(UnitSlot);
        impl PromiseInterface<()> for Rec {
            fn set_value(&mut self, _value: ()) {
                *self.0.lock().unwrap() = Some(Result::Ok(()));
            }
            fn set_error(&mut self, error: Status) {
                *self.0.lock().unwrap() = Some(Result::Err(error));
            }
        }
        let slot: UnitSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
        (Promise::from_interface(Rec(slot.clone())), slot)
    }

    struct TestEnv {
        authorized: bool,
        use_secret_chats: bool,
        updates: Vec<crate::td_api::Update>,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                authorized: true,
                use_secret_chats: true,
                updates: Vec::new(),
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
        let (promise, slot) = record_secret_chat_id();
        manager.create_chat(&mut env, UserId::new(5), 0, promise);
        match slot.lock().unwrap().take() {
            Some(Result::Err(status)) => {
                assert_eq!(status.message(), "Secret chats are not supported");
            }
            other => panic!("unexpected create result: {other:?}"),
        }
        assert!(manager.pending.creates.is_empty(), "no create dispatched");
    }

    #[test]
    fn create_chat_settles_the_receipt_before_the_wire() {
        let mut manager = SecretChatsManager::new(true);
        let mut env = TestEnv::default();
        let (promise, slot) = record_secret_chat_id();
        manager.create_chat(&mut env, UserId::new(5), 42, promise);
        match slot.lock().unwrap().take() {
            Some(Result::Ok(secret_chat_id)) => {
                assert!(secret_chat_id.is_valid());
                assert_eq!(secret_chat_id.get() & 0x8000_0000u32 as i32, 0, "the mask");
            }
            other => panic!("unexpected create result: {other:?}"),
        }
        assert_eq!(
            manager.pending.creates.len(),
            1,
            "net-inert parks the entry"
        );
    }

    #[test]
    fn close_folds_closed_and_parks_the_discard_receipt() {
        let mut manager = SecretChatsManager::new(true);
        let mut env = TestEnv::default();
        manager.on_update_chat(
            &mut env,
            SecretChatId::new(9),
            UserId::new(5),
            77,
            SecretChatState::Active,
        );
        let updates_before = env.updates.len();
        let (promise, _slot) = record_unit();
        manager.cancel_chat(&mut env, SecretChatId::new(9), false, promise);
        assert_eq!(
            manager.get_chat(SecretChatId::new(9)).unwrap().state,
            SecretChatState::Closed
        );
        assert_eq!(manager.pending.discards.len(), 1, "the receipt parks");
        assert_eq!(env.updates.len(), updates_before + 1, "the close emits");
        assert!(manager.pending.creates.is_empty());
    }

    #[test]
    fn close_absorbs_an_unknown_chat() {
        let mut manager = SecretChatsManager::new(true);
        let mut env = TestEnv::default();
        let (promise, slot) = record_unit();
        manager.cancel_chat(&mut env, SecretChatId::new(9), false, promise);
        match slot.lock().unwrap().take() {
            Some(Result::Ok(())) => {}
            other => panic!("unexpected close result: {other:?}"),
        }
        assert!(manager.pending.discards.is_empty(), "no query dispatched");
    }

    #[test]
    fn send_guards_run_before_the_wire() {
        let mut manager = SecretChatsManager::new(true);
        let mut env = TestEnv::default();
        manager.on_update_chat(
            &mut env,
            SecretChatId::new(9),
            UserId::new(5),
            77,
            SecretChatState::Waiting,
        );

        let (promise, slot) = record_unit();
        manager.send_message(&mut env, SecretChatId::new(0), promise);
        match slot.lock().unwrap().take() {
            Some(Result::Err(status)) => {
                assert_eq!(status.message(), "Invalid secret chat identifier specified");
            }
            other => panic!("unexpected send result: {other:?}"),
        }

        let (promise, slot) = record_unit();
        manager.send_message(&mut env, SecretChatId::new(12), promise);
        match slot.lock().unwrap().take() {
            Some(Result::Err(status)) => {
                assert_eq!(status.message(), "Can't find secret chat");
            }
            other => panic!("unexpected send result: {other:?}"),
        }

        let (promise, slot) = record_unit();
        manager.send_message(&mut env, SecretChatId::new(9), promise);
        match slot.lock().unwrap().take() {
            Some(Result::Err(status)) => {
                assert_eq!(status.message(), "Can't access the chat");
            }
            other => panic!("unexpected send result: {other:?}"),
        }
        assert!(manager.pending.sends.is_empty(), "nothing parked");
    }

    #[test]
    fn read_history_guards_and_date_dedup() {
        let mut manager = SecretChatsManager::new(true);
        let mut env = TestEnv::default();
        manager.on_update_chat(
            &mut env,
            SecretChatId::new(9),
            UserId::new(5),
            77,
            SecretChatState::Active,
        );
        manager.on_update_chat(
            &mut env,
            SecretChatId::new(12),
            UserId::new(6),
            78,
            SecretChatState::Waiting,
        );

        // the dedup band: date <= last_read_history_date_ settles Unit
        // without a query (cpp:319-320)
        manager
            .chats
            .get_mut(&SecretChatId::new(9))
            .unwrap()
            .last_read_history_date = 100;
        let (promise, slot) = record_unit();
        manager.send_read_history(&mut env, SecretChatId::new(9), 50, promise);
        match slot.lock().unwrap().take() {
            Some(Result::Ok(())) => {}
            other => panic!("unexpected read result: {other:?}"),
        }
        assert!(manager.pending.read_history.is_empty(), "no query parked");

        // the Ready gate (cpp:313-316)
        let (promise, slot) = record_unit();
        manager.send_read_history(&mut env, SecretChatId::new(12), 100, promise);
        match slot.lock().unwrap().take() {
            Some(Result::Err(status)) => {
                assert_eq!(status.message(), "Can't access the chat");
            }
            other => panic!("unexpected read result: {other:?}"),
        }

        // the manager SafePromise band (cpp:130-134)
        let (promise, slot) = record_unit();
        manager.send_read_history(&mut env, SecretChatId::new(13), 100, promise);
        match slot.lock().unwrap().take() {
            Some(Result::Err(status)) => {
                assert_eq!(status.message(), "Can't find secret chat");
            }
            other => panic!("unexpected read result: {other:?}"),
        }
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
