//! Port of `td/telegram/BotQueries.{h,cpp}` (Wave 3, M.45 — Bots I): the
//! bot-side custom request surface (`sendCustomRequest`,
//! `answerCustomQuery`, `setBotUpdatesStatus`).
//!
//! Seams: the query sends are the [`BotInfoManagerEnv`] seams, net-live
//! over the `BotInfoNetHandle` dispatcher; the promises park in
//! [`crate::bot_info_manager::BotInfoManager`] and the answers drain
//! through `Td::on_bot_info_net_answer` — except
//! `set_bot_updates_status`, whose C++ handler has no promise
//! (BotQueries.cpp:77-100): the fire-and-forget send answers `ok`
//! immediately, exactly like the C++ `promise.set_value(Unit())`.

use crate::bot_info_manager::BotInfoManager;
use crate::bot_info_manager::BotInfoManagerEnv;
use crate::td_api;
use tdutils::promise::Promise;

/// `send_bot_custom_query` (BotQueries.cpp:102-105).
pub fn send_bot_custom_query(
    manager: &mut BotInfoManager,
    env: &mut impl BotInfoManagerEnv,
    method: String,
    parameters: String,
    promise: Promise<td_api::CustomRequestResult>,
) {
    manager.park_send_custom_request_query(promise);
    env.send_bots_send_custom_request_query(&method, &parameters);
}

/// `answer_bot_custom_query` (BotQueries.cpp:107-109).
pub fn answer_bot_custom_query(
    manager: &mut BotInfoManager,
    env: &mut impl BotInfoManagerEnv,
    custom_query_id: i64,
    data: String,
    promise: Promise<()>,
) {
    manager.park_answer_custom_query_query(promise);
    env.send_bots_answer_webhook_json_query(custom_query_id, &data);
}

/// `set_bot_updates_status` (BotQueries.cpp:111-114).
pub fn set_bot_updates_status(
    env: &mut impl BotInfoManagerEnv,
    pending_update_count: i32,
    error_message: String,
    mut promise: Promise<()>,
) {
    env.send_help_set_bot_updates_status_query(pending_update_count, &error_message);
    promise.set_value(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot_info_manager::test_env::{self, TestBotEnv};
    use crate::global::Global;
    use crate::option_manager::OptionManager;
    use crate::user_manager::UserManager;
    use tdutils::promise::PromiseInterface;
    use tdutils::status::Status;

    struct Recorded<T: Send + 'static>(
        std::sync::Arc<std::sync::Mutex<Option<std::result::Result<T, Status>>>>,
    );

    struct RecordingPromise<T: Send + 'static> {
        recorded: Recorded<T>,
    }

    impl<T: Send + 'static> PromiseInterface<T> for RecordingPromise<T> {
        fn set_value(&mut self, value: T) {
            *self.recorded.0.lock().unwrap() = Some(Result::Ok(value));
        }

        fn set_error(&mut self, error: Status) {
            *self.recorded.0.lock().unwrap() = Some(Result::Err(error));
        }
    }

    fn recording_promise<T: Send + 'static>() -> (Promise<T>, Recorded<T>) {
        let recorded = Recorded(std::sync::Arc::new(std::sync::Mutex::new(None)));
        (
            Promise::from_interface(RecordingPromise {
                recorded: Recorded(recorded.0.clone()),
            }),
            recorded,
        )
    }

    #[test]
    fn bot_queries_park_or_answer() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();
        let mut env: TestBotEnv =
            test_env::fixture(&mut global, Some(&mut options), &mut user_manager, true, 1);
        let mut manager = BotInfoManager::new();

        // the custom request parks behind the net seam
        let (promise, recorded) = recording_promise();
        send_bot_custom_query(
            &mut manager,
            &mut env,
            "getMe".to_owned(),
            "{}".to_owned(),
            promise,
        );
        assert!(recorded.0.lock().unwrap().is_none());
        assert_eq!(
            env.queries,
            vec!["Send bots.sendCustomRequest — [net-inert]"]
        );

        // the webhook answer parks too
        let (promise, recorded) = recording_promise();
        answer_bot_custom_query(&mut manager, &mut env, 7, "{\"x\":1}".to_owned(), promise);
        assert!(recorded.0.lock().unwrap().is_none());

        // the updates-status send is fire-and-forget: the ok answer is
        // immediate (BotQueries.cpp:111-114)
        let (promise, recorded) = recording_promise();
        set_bot_updates_status(&mut env, 3, String::new(), promise);
        let recorded = recorded.0.lock().unwrap().clone();
        assert!(recorded.unwrap().is_ok());
        assert_eq!(
            env.queries.last().unwrap(),
            "Send help.setBotUpdatesStatus — [net-inert]"
        );
    }
}
