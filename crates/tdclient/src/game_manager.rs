//! Port of `td/telegram/GameManager.{h,cpp}` — the M.46 bots-slice
//! manager over the bot-only setGameScore/getGameHighScores requests
//! (Requests.cpp:4911, :4928) and the `messages.setGameScore`/
//! `messages.getGameHighScores` queries.
//!
//! Seams:
//! - The two `Td::ResultHandler` queries ride the [`GameManagerEnv`]
//!   net seams — `[blocked: net]`, the promises park (the M.41
//!   convention); the answer handlers
//!   ([`GameManager::on_set_game_score_query`],
//!   [`GameManager::on_get_game_high_scores_query`]) are real and unit-tested.
//! - `SetGameScoreQuery::on_result` hops into
//!   `td_->updates_manager_->on_get_updates` (GameManager.cpp:60) — the
//!   [`GameManagerEnv::on_get_updates`] seam; the production env calls
//!   the landed M.7 UpdatesManager.
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

    /// `GetGameHighScoresQuery::send` (GameManager.cpp:79-88) —
    /// `[blocked: net]`; the answer arrives through
    /// [`GameManager::on_get_game_high_scores_query`].
    fn send_get_game_high_scores_query(
        &mut self,
        dialog_id: DialogId,
        message_id: MessageId,
        input_user: telegram_api::InputUser,
    ) {
        let _ = (dialog_id, message_id, input_user);
        tdutils::log_info!("Send messages.getGameHighScores — [blocked: net]");
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

/// A parked `messages.getGameHighScores` query.
struct PendingGetGameHighScoresQuery {
    dialog_id: DialogId,
    promise: Promise<td_api::GameHighScores>,
}

/// The port of `td::GameManager`.
#[derive(Default)]
pub struct GameManager {
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

        env.send_get_game_high_scores_query(dialog_id, message_id, input_user);
        self.pending_get_game_high_scores_queries
            .push(PendingGetGameHighScoresQuery { dialog_id, promise });
    }

    /// `GetGameHighScoresQuery::on_result`/`on_error`
    /// (GameManager.cpp:90-102).
    pub fn on_get_game_high_scores_query(
        &mut self,
        env: &mut impl GameManagerEnv,
        result: Result<telegram_api::messages_HighScores, Status>,
    ) {
        let Some(query) = self.pending_get_game_high_scores_queries.pop() else {
            tdutils::log_info!("Receive an unexpected messages.getGameHighScores answer");
            return;
        };
        let mut promise = query.promise;
        match result {
            Ok(high_scores) => {
                let scores = self.get_game_high_scores_object(env, high_scores);
                promise.set_value(scores);
            }
            Err(status) => {
                env.on_get_dialog_error(query.dialog_id, &status, "GetGameHighScoresQuery");
                promise.set_error(status);
            }
        }
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

    /// The recording env: the bot flag, the message-plane answer toggles
    /// and the fired query seams.
    struct TestEnv {
        bot: bool,
        known_users: Vec<i64>,
        have_message: bool,
        can_set_score: bool,
        message_answer: Option<td_api::Message>,
        queries: Vec<String>,
        updates: usize,
        dialog_errors: Vec<String>,
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

        fn send_get_game_high_scores_query(
            &mut self,
            dialog_id: crate::dialog_id::DialogId,
            message_id: MessageId,
            _input_user: telegram_api::InputUser,
        ) {
            self.queries.push(format!(
                "getGameHighScores:{}:{}",
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
    fn get_game_high_scores_guards_and_answers() {
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

        // the healthy path parks and answers
        let (promise, outcomes) = recording_promise();
        manager.get_game_high_scores(&mut env, message_full_id(1 << 20), UserId::new(9), promise);
        assert_eq!(env.queries, vec!["getGameHighScores:-3:1048576"]);
        assert!(outcomes.lock().unwrap().is_empty());
        manager.on_get_game_high_scores_query(&mut env, Ok(sample_high_scores()));
        let outcomes = outcomes.lock().unwrap();
        assert_eq!(outcomes[0].as_ref().expect("answered").scores.len(), 2);

        // an error maps through the dialog-error sink
        let (promise, outcomes) = recording_promise();
        manager.get_game_high_scores(&mut env, message_full_id(1 << 20), UserId::new(9), promise);
        manager.on_get_game_high_scores_query(
            &mut env,
            Err(Status::error(400, "CHAT_ADMIN_REQUIRED")),
        );
        assert_eq!(env.dialog_errors, vec!["-3:CHAT_ADMIN_REQUIRED"]);
        assert!(matches!(
            &outcomes.lock().unwrap()[0],
            Err(m) if m == "400 CHAT_ADMIN_REQUIRED"
        ));
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
