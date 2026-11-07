//! Port of `td/telegram/GameManager.{h,cpp}` — the M.46 bots-slice
//! manager over the bot-only setGameScore/getGameHighScores requests
//! (Requests.cpp:4911, :4928) and the `messages.setGameScore`/
//! `messages.getGameHighScores` queries.
//!
//! Seams:
//! - `GetGameHighScoresQuery` rides the `GameNetKind` plane (net-drain
//! 57): the manager-held [`GameNetHandle`] dispatches the wire query
//! and the answer drains through
//! [`crate::td::Td::on_game_net_answer`] into
//! [`GameManager::get_game_high_scores_object`].
//! - `SetGameScoreQuery::send` is the [`GameManagerEnv`] net seam —
//! `[blocked: net]`, the promise parks (the M.41 convention); the
//! answer handler ([`GameManager::on_set_game_score_query`]) hops into
//! `td_->updates_manager_->on_get_updates` (GameManager.cpp:60) — the
//! [`GameManagerEnv::on_get_updates`] seam; the production env calls
//! the landed M.7 UpdatesManager.
//! - The MessagesManager reads (`have_message_force`,
//!   `can_set_game_score`, `get_message_object`) are [blocked: M.10]
//!   through the env trait.

use crate::dialog_id::DialogId;
use crate::message_id::{MessageFullId, MessageId};
use crate::td_api;
use crate::user_manager::{AccessRights, UserId};
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The in-flight query kinds of the net-drain 57 game families — the
/// C++ `SetGameScoreQuery`/`GetGameHighScoresQuery` `Td::ResultHandler`
/// classes (GameManager.cpp:28-103) collapsed onto dispatcher tags; the
/// answers drain through [`crate::td::Td::on_game_net_answer`]. The
/// kinds carry the handlers' `dialog_id_` members (the `on_error`
/// dialog head); the request promises park in the manager's family lots
/// (the C++ carries them on the queries' `promise_` members — §5's
/// FIFO-per-family shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GameNetKind {
    /// `GetGameHighScoresQuery` (GameManager.cpp:70-103) —
    /// `messages.getGameHighScores`.
    GetGameHighScores {
        dialog_id: DialogId,
    },
}

/// The `G()`/manager slice the collapsed manager reads through.
pub trait GameManagerEnv {
    /// `td_->auth_manager_->is_bot()`.
    fn is_bot(&self) -> bool;

    /// `td_->user_manager_->get_input_user(user_id)`.
    fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser, Status>;

    /// `td_->user_manager_->on_get_users(users, source)` — the intake.
    fn on_get_users(&mut self, users: Vec<telegram_api::User>, source: &str);

    /// `td_->user_manager_->get_user_id_object(user_id, source)`.
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64;

    /// `td_->dialog_manager_->check_dialog_access(dialog_id, false,
    /// rights, source)`.
    fn check_dialog_access(
        &mut self,
        dialog_id: DialogId,
        access_rights: AccessRights,
        source: &str,
    ) -> Result<(), Status>;

    /// `td_->dialog_manager_->on_get_dialog_error(dialog_id, status,
    /// source)`.
    fn on_get_dialog_error(&mut self, dialog_id: DialogId, status: &Status, source: &str);

    /// `td_->messages_manager_->have_message_force(message_full_id,
    /// source)` — [blocked: M.10].
    fn have_message_force(&mut self, message_full_id: MessageFullId, source: &str) -> bool;

    /// `td_->messages_manager_->can_set_game_score(message_full_id)` —
    /// [blocked: M.10].
    fn can_set_game_score(&mut self, message_full_id: MessageFullId) -> bool;

    /// `td_->messages_manager_->get_message_object(message_full_id,
    /// source)` — [blocked: M.10]; `None` models the unresolvable
    /// message.
    fn get_message_object(
        &mut self,
        message_full_id: MessageFullId,
        source: &str,
    ) -> Option<td_api::Message>;

    /// `td_->dialog_manager_->get_input_peer(dialog_id, rights)` —
    /// the query send legs resolve the peer (the `ForumTopicEnv`
    /// split shape; GameManager.cpp:82).
    fn get_input_peer(
        &mut self,
        dialog_id: DialogId,
        access_rights: AccessRights,
    ) -> Option<telegram_api::InputPeer>;

    /// `td_->updates_manager_->on_get_updates(updates, promise)`
    /// (GameManager.cpp:60) — the production env calls the M.7
    /// UpdatesManager.
    fn on_get_updates(&mut self, updates: telegram_api::Updates, promise: Promise<()>);

    /// `SetGameScoreQuery::send` (GameManager.cpp:36-50) —
    /// `[blocked: net]`; the answer arrives through
    /// [`GameManager::on_set_game_score_query`].
    fn send_set_game_score_query(
        &mut self,
        dialog_id: DialogId,
        message_id: MessageId,
        edit_message: bool,
        input_user: telegram_api::InputUser,
        score: i32,
        force: bool,
    ) {
        let _ = (
            dialog_id,
            message_id,
            edit_message,
            input_user,
            score,
            force,
        );
        tdutils::log_info!("Send messages.setGameScore — [blocked: net]");
    }
}

/// A parked `messages.setGameScore` query — the ResultHandler keeps the
/// dialog for the error mapping and the message coordinates for the
/// `on_set_game_score` hop.
struct PendingSetGameScoreQuery {
    dialog_id: DialogId,
    message_full_id: MessageFullId,
    promise: Promise<td_api::Message>,
}

/// A parked `messages.getGameHighScores` query — the handler's
/// `dialog_id_`/`promise_` members (GameManager.cpp:71-72).
struct PendingGetGameHighScoresQuery {
    dialog_id: DialogId,
    promise: Promise<td_api::GameHighScores>,
}

/// The port of `td::GameManager`.
#[derive(Default)]
pub struct GameManager {
    /// The manager-held dispatch surface of the net-drain 57 families —
    /// `None` keeps the net-inert posture (the pre-init windows and the
    /// net-inert tests; the `GroupCallManager::net` shape — the shared
    /// [`crate::td::BotEnv`] view resolves the peers at the send legs).
    pub(crate) net: Option<crate::td::GameNetHandle>,
    /// The parked `messages.setGameScore` queries.
    pending_set_game_score_queries: Vec<PendingSetGameScoreQuery>,
    /// The parked `messages.getGameHighScores` queries.
    pending_get_game_high_scores_queries: Vec<PendingGetGameHighScoresQuery>,
}

impl GameManager {
    /// `GameManager::GameManager` (GameManager.cpp:105-106).
    pub fn new() -> GameManager {
        GameManager::default()
    }

    /// `set_game_score` (GameManager.cpp:114-140) — the C++
    /// `CHECK(td_->auth_manager_->is_bot())` is the assert.
    // the C++ argument list is the port shape (env + 7 params)
    #[allow(clippy::too_many_arguments)]
    pub fn set_game_score(
        &mut self,
        env: &mut impl GameManagerEnv,
        message_full_id: MessageFullId,
        edit_message: bool,
        user_id: UserId,
        score: i32,
        force: bool,
        mut promise: Promise<td_api::Message>,
    ) {
        assert!(env.is_bot());

        let dialog_id = DialogId::new(message_full_id.get_dialog_id().get());
        if let Err(status) =
            env.check_dialog_access(dialog_id, AccessRights::Edit, "set_game_score")
        {
            return promise.set_error(status);
        }
        if !env.have_message_force(message_full_id, "set_game_score") {
            return promise.set_error(Status::error(400, "Message not found"));
        }

        let input_user = match env.get_input_user(user_id) {
            Ok(input_user) => input_user,
            Err(status) => return promise.set_error(status),
        };

        if !env.can_set_game_score(message_full_id) {
            return promise.set_error(Status::error(400, "Game score can't be set"));
        }

        env.send_set_game_score_query(
            dialog_id,
            message_full_id.get_message_id(),
            edit_message,
            input_user,
            score,
            force,
        );
        self.pending_set_game_score_queries
            .push(PendingSetGameScoreQuery {
                dialog_id,
                message_full_id,
                promise,
            });
    }

    /// `SetGameScoreQuery::on_result`/`on_error`
    /// (GameManager.cpp:52-67) — the C++ passes the `on_set_game_score`
    /// continuation into `on_get_updates`; the collapsed seam applies
    /// the updates inline and the hop follows.
    pub fn on_set_game_score_query(
        &mut self,
        env: &mut impl GameManagerEnv,
        result: Result<telegram_api::Updates, Status>,
    ) {
        let Some(query) = self.pending_set_game_score_queries.pop() else {
            tdutils::log_info!("Receive an unexpected messages.setGameScore answer");
            return;
        };
        match result {
            Ok(updates) => {
                env.on_get_updates(updates, Promise::default());
                self.on_set_game_score(env, query.message_full_id, query.promise);
            }
            Err(status) => {
                tdutils::log_info!("Receive error for SetGameScoreQuery: {status}");
                env.on_get_dialog_error(query.dialog_id, &status, "SetGameScoreQuery");
                let mut promise = query.promise;
                promise.set_error(status);
            }
        }
    }

    /// `on_set_game_score` (GameManager.cpp:142-145).
    fn on_set_game_score(
        &mut self,
        env: &mut impl GameManagerEnv,
        message_full_id: MessageFullId,
        mut promise: Promise<td_api::Message>,
    ) {
        match env.get_message_object(message_full_id, "on_set_game_score") {
            Some(message) => promise.set_value(message),
            // unreachable in the C++ (have_message_force checked earlier);
            // the [blocked: M.10] seam keeps the error surface
            None => promise.set_error(Status::error(500, "Receive invalid message")),
        }
    }

    /// `get_game_high_scores` (GameManager.cpp:147-167) — the C++
    /// `CHECK(td_->auth_manager_->is_bot())` is the assert.
    pub fn get_game_high_scores(
        &mut self,
        env: &mut impl GameManagerEnv,
        message_full_id: MessageFullId,
        user_id: UserId,
        mut promise: Promise<td_api::GameHighScores>,
    ) {
        assert!(env.is_bot());

        let dialog_id = DialogId::new(message_full_id.get_dialog_id().get());
        if let Err(status) =
            env.check_dialog_access(dialog_id, AccessRights::Read, "get_game_high_scores")
        {
            return promise.set_error(status);
        }

        if !env.have_message_force(message_full_id, "get_game_high_scores") {
            return promise.set_error(Status::error(400, "Message not found"));
        }

        let message_id = message_full_id.get_message_id();
        if message_id.is_scheduled() || !message_id.is_server() {
            return promise.set_error(Status::error(400, "Wrong message identifier specified"));
        }

        let input_user = match env.get_input_user(user_id) {
            Ok(input_user) => input_user,
            Err(status) => return promise.set_error(status),
        };

        self.send_get_game_high_scores_query(
            env,
            dialog_id,
            message_id,
            input_user,
            promise,
        );
    }

    /// The `td_->create_handler<GetGameHighScoresQuery>(promise)
    /// ->send(...)` collapse (GameManager.cpp:166): the request promise
    /// parks in the family lot (the C++ carries it on the query's
    /// `promise_` member, cpp:71 — §5's FIFO shape), then the read peer
    /// resolves and the wire query dispatches. `None` on the handle
    /// keeps the net-inert posture.
    pub(crate) fn send_get_game_high_scores_query(
        &mut self,
        env: &mut impl GameManagerEnv,
        dialog_id: DialogId,
        message_id: MessageId,
        input_user: telegram_api::InputUser,
        promise: Promise<td_api::GameHighScores>,
    ) {
        self.pending_get_game_high_scores_queries
            .push(PendingGetGameHighScoresQuery { dialog_id, promise });
        let kind = GameNetKind::GetGameHighScores { dialog_id };
        match (
            self.net.clone(),
            env.get_input_peer(dialog_id, AccessRights::Read),
        ) {
            // `GetGameHighScoresQuery::send` (cpp:79-88)
            (Some(net), Some(peer)) => net.dispatch(
                kind,
                telegram_api::Function::messages_getGameHighScores(
                    telegram_api::messages_getGameHighScores {
                        peer: Some(Box::new(peer)),
                        id: message_id.get_server_message_id().get(),
                        user_id: Some(input_user),
                    },
                ),
            ),
            // the C++ `CHECK(input_peer != nullptr)` (cpp:83) — the
            // `check_dialog_access` ladder gated the flow; the None band
            // settles through the completed queue (the message-query
            // CHECK convention, MessageQueryManager.cpp:1182-1193)
            (Some(net), None) => net.complete_error(kind, 400, "Can't access the chat"),
            (None, _) => tdutils::log_info!("Send messages.getGameHighScores — [net-inert]"),
        }
    }

    /// The high-scores family promise pop for the
    /// [`crate::td::Td::on_game_net_answer`] drain — the FIFO head the
    /// answer pairs with (the C++ promise rides the query).
    pub(crate) fn pop_get_game_high_scores_promise(
        &mut self,
    ) -> Option<Promise<td_api::GameHighScores>> {
        self.pending_get_game_high_scores_queries
            .pop()
            .map(|query| query.promise)
    }

    /// `get_game_high_scores_object` (GameManager.cpp:169-186) — public
    /// like the C++ (the inline-answers path reads it too).
    pub fn get_game_high_scores_object(
        &mut self,
        env: &mut impl GameManagerEnv,
        high_scores: telegram_api::messages_HighScores,
    ) -> td_api::GameHighScores {
        env.on_get_users(high_scores.users, "get_game_high_scores_object");

        let mut result = td_api::GameHighScores { scores: Vec::new() };
        for high_score in high_scores.scores {
            let position = high_score.pos;
            let user_id = UserId::new(high_score.user_id);
            let score = high_score.score;
            if position <= 0 || !user_id.is_valid() || score < 0 {
                tdutils::log_error!("Receive wrong high score {position} {score}");
                continue;
            }
            result.scores.push(td_api::GameHighScore {
                position,
                user_id: env.get_user_id_object(user_id, "get_game_high_scores_object"),
                score,
            });
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdutils::promise::PromiseInterface;

    /// The recording env: the bot flag, the message-plane answer toggles,
    /// the fired set-query seam and the peer-resolution band (the get
    /// send leg reads the peer; the [net-inert] None-handle posture is
    /// what these unit bands drive).
    struct TestEnv {
        bot: bool,
        known_users: Vec<i64>,
        have_message: bool,
        can_set_score: bool,
        message_answer: Option<td_api::Message>,
        queries: Vec<String>,
        updates: usize,
        dialog_errors: Vec<String>,
        resolve_peer: bool,
        users_intakes: usize,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                bot: true,
                known_users: vec![9],
                have_message: true,
                can_set_score: true,
                message_answer: Some(td_api::Message::default()),
                queries: Vec::new(),
                updates: 0,
                dialog_errors: Vec::new(),
                resolve_peer: true,
                users_intakes: 0,
            }
        }
    }

    impl GameManagerEnv for TestEnv {
        fn is_bot(&self) -> bool {
            self.bot
        }

        fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser, Status> {
            if !self.known_users.contains(&user_id.get()) {
                return Err(Status::error(400, "User not found"));
            }
            Ok(telegram_api::InputUser::inputUser(
                telegram_api::inputUser {
                    user_id: user_id.get(),
                    access_hash: 3,
                },
            ))
        }

        fn on_get_users(&mut self, users: Vec<telegram_api::User>, _source: &str) {
            self.users_intakes += users.len();
        }

        fn get_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
            user_id.get()
        }

        fn check_dialog_access(
            &mut self,
            dialog_id: crate::dialog_id::DialogId,
            _access_rights: AccessRights,
            _source: &str,
        ) -> Result<(), Status> {
            if dialog_id.is_valid() {
                Ok(())
            } else {
                Err(Status::error(400, "Chat not found"))
            }
        }

        fn on_get_dialog_error(
            &mut self,
            dialog_id: crate::dialog_id::DialogId,
            status: &Status,
            _source: &str,
        ) {
            self.dialog_errors
                .push(format!("{}:{}", dialog_id.get(), status.message()));
        }

        fn have_message_force(
            &mut self,
            message_full_id: crate::message_id::MessageFullId,
            _source: &str,
        ) -> bool {
            self.have_message && message_full_id.get_message_id().get() != -3
        }

        fn can_set_game_score(
            &mut self,
            message_full_id: crate::message_id::MessageFullId,
        ) -> bool {
            self.can_set_score && message_full_id.get_message_id().get() != -4
        }

        fn get_message_object(
            &mut self,
            _message_full_id: crate::message_id::MessageFullId,
            _source: &str,
        ) -> Option<td_api::Message> {
            self.message_answer.clone()
        }

        fn get_input_peer(
            &mut self,
            _dialog_id: crate::dialog_id::DialogId,
            _access_rights: AccessRights,
        ) -> Option<telegram_api::InputPeer> {
            self.resolve_peer
                .then(|| telegram_api::InputPeer::inputPeerSelf(telegram_api::inputPeerSelf {}))
        }

        fn on_get_updates(&mut self, _updates: telegram_api::Updates, _promise: Promise<()>) {
            self.updates += 1;
        }

        fn send_set_game_score_query(
            &mut self,
            dialog_id: crate::dialog_id::DialogId,
            message_id: MessageId,
            edit_message: bool,
            _input_user: telegram_api::InputUser,
            score: i32,
            force: bool,
        ) {
            self.queries.push(format!(
                "setGameScore:{}:{}:{edit_message}:{score}:{force}",
                dialog_id.get(),
                message_id.get()
            ));
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

    fn message_full_id(message_id: i64) -> crate::message_id::MessageFullId {
        crate::message_id::MessageFullId::new(
            crate::message_id::DialogId::new(-3),
            MessageId::new(message_id),
        )
    }

    fn sample_high_scores() -> telegram_api::messages_HighScores {
        telegram_api::messages_HighScores {
            scores: vec![
                telegram_api::HighScore {
                    pos: 1,
                    user_id: 9,
                    score: 100,
                },
                telegram_api::HighScore {
                    pos: 0,
                    user_id: 9,
                    score: 50,
                },
                telegram_api::HighScore {
                    pos: -1,
                    user_id: 9,
                    score: 40,
                },
                telegram_api::HighScore {
                    pos: 2,
                    user_id: 0,
                    score: -1,
                },
                telegram_api::HighScore {
                    pos: 2,
                    user_id: 10,
                    score: 30,
                },
            ],
            users: vec![
                telegram_api::User::user(telegram_api::user {
                    flags: 1 << 15,
                    id: 9,
                    ..Default::default()
                }),
                telegram_api::User::user(telegram_api::user {
                    flags: 1 << 15,
                    id: 10,
                    ..Default::default()
                }),
            ],
        }
    }

    #[test]
    fn get_game_high_scores_object_filters_and_intakes() {
        let mut env = TestEnv::default();
        let mut manager = GameManager::new();

        let scores = manager.get_game_high_scores_object(&mut env, sample_high_scores());
        assert_eq!(env.users_intakes, 2);
        // the invalid positions/scores are dropped, the survivors keep the
        // object-facing user ids
        assert_eq!(scores.scores.len(), 2);
        assert_eq!(scores.scores[0].position, 1);
        assert_eq!(scores.scores[0].user_id, 9);
        assert_eq!(scores.scores[0].score, 100);
        assert_eq!(scores.scores[1].position, 2);
        assert_eq!(scores.scores[1].user_id, 10);
        assert_eq!(scores.scores[1].score, 30);
    }

    #[test]
    fn get_game_high_scores_guards_and_parks() {
        let mut env = TestEnv::default();
        let mut manager = GameManager::new();

        // an unknown user fails the input-user guard after the message check
        let (promise, outcomes) = recording_promise();
        manager.get_game_high_scores(
            &mut env,
            message_full_id(1 << 20),
            UserId::new(999),
            promise,
        );
        assert!(matches!(&outcomes.lock().unwrap()[0], Err(m) if m == "400 User not found"));

        // an unknown message
        env.have_message = false;
        let (promise, outcomes) = recording_promise();
        manager.get_game_high_scores(&mut env, message_full_id(1 << 20), UserId::new(9), promise);
        assert!(matches!(&outcomes.lock().unwrap()[0], Err(m) if m == "400 Message not found"));
        env.have_message = true;

        // scheduled and non-server ids
        let (promise, outcomes) = recording_promise();
        manager.get_game_high_scores(
            &mut env,
            message_full_id((1 << 20) | 4),
            UserId::new(9),
            promise,
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(m) if m == "400 Wrong message identifier specified")
        );
        let (promise, outcomes) = recording_promise();
        manager.get_game_high_scores(
            &mut env,
            message_full_id((1 << 20) | 2),
            UserId::new(9),
            promise,
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(m) if m == "400 Wrong message identifier specified")
        );

        // the healthy path parks the promise (the C++ handler carries it
        // on the query; the wire form and the answer intakes ride the
        // netted round-trip)
        let (promise, outcomes) = recording_promise();
        manager.get_game_high_scores(&mut env, message_full_id(1 << 20), UserId::new(9), promise);
        assert!(outcomes.lock().unwrap().is_empty());
        assert!(manager.pop_get_game_high_scores_promise().is_some());
        assert!(manager.pop_get_game_high_scores_promise().is_none());
    }

    #[test]
    fn set_game_score_guards_and_completes_through_updates() {
        let mut env = TestEnv::default();
        let mut manager = GameManager::new();

        // can_set_game_score=false → the 400 guard (the -4 marker id)
        let (promise, outcomes) = recording_promise();
        manager.set_game_score(
            &mut env,
            message_full_id(-4),
            false,
            UserId::new(9),
            10,
            false,
            promise,
        );
        assert!(
            matches!(&outcomes.lock().unwrap()[0], Err(m) if m == "400 Game score can't be set")
        );

        // an unknown user fails before the score guard
        let (promise, outcomes) = recording_promise();
        manager.set_game_score(
            &mut env,
            message_full_id(1 << 20),
            false,
            UserId::new(999),
            10,
            false,
            promise,
        );
        assert!(matches!(&outcomes.lock().unwrap()[0], Err(m) if m == "400 User not found"));

        // the healthy path parks, applies the updates and answers the message
        let (promise, outcomes) = recording_promise();
        manager.set_game_score(
            &mut env,
            message_full_id(1 << 20),
            true,
            UserId::new(9),
            10,
            true,
            promise,
        );
        assert_eq!(env.queries, vec!["setGameScore:-3:1048576:true:10:true"]);
        assert!(outcomes.lock().unwrap().is_empty());
        manager.on_set_game_score_query(
            &mut env,
            Ok(telegram_api::Updates::updatesTooLong(
                telegram_api::updatesTooLong {},
            )),
        );
        assert_eq!(env.updates, 1);
        assert_eq!(outcomes.lock().unwrap().len(), 1);

        // an error maps through the dialog-error sink
        let (promise, outcomes) = recording_promise();
        manager.set_game_score(
            &mut env,
            message_full_id(1 << 20),
            false,
            UserId::new(9),
            5,
            false,
            promise,
        );
        manager.on_set_game_score_query(
            &mut env,
            Err(Status::error(400, "MESSAGE_EDIT_TIME_EXPIRED")),
        );
        assert_eq!(env.dialog_errors, vec!["-3:MESSAGE_EDIT_TIME_EXPIRED"]);
        assert!(matches!(
            &outcomes.lock().unwrap()[0],
            Err(m) if m == "400 MESSAGE_EDIT_TIME_EXPIRED"
        ));
    }

    #[test]
    fn get_game_high_scores_parks_net_inert_when_no_dispatcher() {
        // net-drain 57: the manager-held handle is `None` (the pre-init
        // windows, the net-inert tests) — the promise parks in the family
        // lot like the C++ handler holds it on the query
        let mut env = TestEnv::default();
        let mut manager = GameManager::new();
        let (promise, outcomes) = recording_promise();
        manager.get_game_high_scores(&mut env, message_full_id(1 << 20), UserId::new(9), promise);
        assert!(outcomes.lock().unwrap().is_empty(), "nothing settles offline");
        assert!(
            manager.pop_get_game_high_scores_promise().is_some(),
            "the promise parks like the C++ handler holds it"
        );
        // the lot is empty — the answer drain's log band pairs nothing
        assert!(manager.pop_get_game_high_scores_promise().is_none());
    }

    #[test]
    #[should_panic(expected = "assertion failed: env.is_bot()")]
    fn set_game_score_checks_the_bot_client() {
        // the C++ CHECK(td_->auth_manager_->is_bot()) asserts; the
        // request arm's CHECK_IS_BOT fires first in the request flows
        let mut env = TestEnv {
            bot: false,
            ..Default::default()
        };
        let mut manager = GameManager::new();
        manager.set_game_score(
            &mut env,
            message_full_id(1 << 20),
            false,
            UserId::new(9),
            10,
            false,
            Promise::default(),
        );
    }
}
