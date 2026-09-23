//! Port of `td/telegram/BotRecommendationManager.{h,cpp}` — the M.46
//! bots-slice manager over the `getBotSimilarBots`/`getBotSimilarBotCount`
//! /`openBotSimilarBot` requests (Requests.cpp:3212-3229) and the
//! `bots.getBotRecommendations` query.
//!
//! Seams:
//! - `GetBotRecommendationsQuery::send`
//!   (BotRecommendationManager.cpp:39-48) is the
//!   [`BotRecommendationManagerEnv::send_get_bot_recommendations_query`]
//!   net seam — `[blocked: net]`, the answer arrives through
//!   [`BotRecommendationManager::on_get_bot_recommendations`].
//! - The `G()->td_db()->get_sqlite_pmc()` row store (the C++
//!   `SqliteKeyValueAsync` callback hop, BotRecommendationManager.cpp:193-198
//!   / :347-349) collapses into the synchronous
//!   `sqlite_pmc_get_raw`/`set_raw`/`erase` env surfaces — the M.1 TdDb
//!   divergence (`TdDb::common_kv`).
//! - `Dependencies::resolve_force`
//!   (BotRecommendationManager.cpp:267) is the
//!   [`BotRecommendationManagerEnv::resolve_dependencies`] seam
//!   ([blocked: M.4 Dependencies]); the default trusts the synchronous
//!   user cache.
//! - `save_app_log` (BotRecommendationManager.cpp:364) is the
//!   [`BotRecommendationManagerEnv::save_app_log`] seam — the
//!   `bots.openRecommendedBot`... app-log query is a network leg
//!   ([blocked: net]); the promise parks with the seam.

use crate::dialog_id::DialogId;
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::UserId;
use std::collections::HashMap;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `BotRecommendationManager::BOT_RECOMMENDATIONS_CACHE_TIME`
/// (BotRecommendationManager.h:35) — some reasonable limit.
const BOT_RECOMMENDATIONS_CACHE_TIME: i32 = 86400;

/// The `G()`/manager slice the collapsed manager reads through.
pub trait BotRecommendationManagerEnv {
    /// `td_->user_manager_->is_user_bot(bot_user_id)`.
    fn is_user_bot(&mut self, bot_user_id: UserId) -> bool;

    /// `td_->user_manager_->get_input_user(bot_user_id)`.
    fn get_input_user(&mut self, bot_user_id: UserId) -> Result<telegram_api::InputUser, Status>;

    /// `td_->user_manager_->on_get_user(user, source)` — the intake.
    fn on_get_user(&mut self, user: telegram_api::User, source: &str) -> UserId;

    /// `td_->user_manager_->get_user_id_object(user_id, source)`.
    fn get_user_id_object(&mut self, user_id: UserId, source: &str) -> i64;

    /// `td_->user_manager_->get_users_object(total_count, user_ids)`.
    fn get_users_object(&mut self, total_count: i32, user_ids: &[UserId]) -> td_api::Users;

    /// `td_->option_manager_->get_option_boolean(name)` (the C++ default
    /// false).
    fn get_option_boolean(&mut self, name: &str) -> bool;

    /// `G()->use_message_database()`.
    fn use_message_database(&mut self) -> bool;

    /// `G()->close_flag()`.
    fn close_flag(&mut self) -> bool;

    /// `G()->td_db()->get_sqlite_pmc()->get` — the synchronous collapse
    /// of the `SqliteKeyValueAsync` read (the M.1 TdDb divergence).
    fn sqlite_pmc_get_raw(&mut self, key: &str) -> Vec<u8>;

    /// `G()->td_db()->get_sqlite_pmc()->set`.
    fn sqlite_pmc_set_raw(&mut self, key: &str, value: &[u8]);

    /// `G()->td_db()->get_sqlite_pmc()->erase`.
    fn sqlite_pmc_erase(&mut self, key: &str);

    /// `Dependencies::add(user_id)` × N + `resolve_force(td, source)`
    /// (BotRecommendationManager.cpp:263-267) — [blocked: M.4
    /// Dependencies]; the default trusts the synchronous user cache.
    fn resolve_dependencies(&mut self, user_ids: &[UserId], source: &str) -> bool {
        let _ = (user_ids, source);
        true
    }

    /// `save_app_log(td, "bots.open_recommended_bot", DialogId(),
    /// jsonObject, promise)` (BotRecommendationManager.cpp:364) — the
    /// app-log query is a network leg ([blocked: net]); the promise
    /// parks with the seam.
    fn save_app_log(
        &mut self,
        name: &str,
        dialog_id: DialogId,
        value: telegram_api::JSONValue,
        promise: Promise<()>,
    ) {
        let _ = (dialog_id, value, promise);
        tdutils::log_info!("save_app_log({name}) — [blocked: net]");
    }

    /// `GetBotRecommendationsQuery::send`
    /// (BotRecommendationManager.cpp:39-48) — `[blocked: net]`; the
    /// answer arrives through
    /// [`BotRecommendationManager::on_get_bot_recommendations`].
    fn send_get_bot_recommendations_query(&mut self, bot_user_id: UserId) {
        let _ = bot_user_id;
        tdutils::log_info!("Send bots.getBotRecommendations — [blocked: net]");
    }
}

/// `BotRecommendationManager::RecommendedBots` (BotRecommendationManager.h:37-47).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecommendedBots {
    total_count: i32,
    bot_user_ids: Vec<UserId>,
    next_reload_time: f64,
}

impl RecommendedBots {
    /// `RecommendedBots::store` (BotRecommendationManager.cpp:66-81) —
    /// byte-exact: the flag block, then the optional id vector, then the
    /// reload time, then the optional total count.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_bot_user_ids = !self.bot_user_ids.is_empty();
        let has_total_count = self.total_count != self.bot_user_ids.len() as i32;
        store_parse::store_flags(out, [has_bot_user_ids, has_total_count]);
        if has_bot_user_ids {
            store_parse::store_i32(out, self.bot_user_ids.len() as i32);
            for bot_user_id in &self.bot_user_ids {
                store_parse::store_i64(out, bot_user_id.get());
            }
        }
        store_parse::store_f64(out, self.next_reload_time);
        if has_total_count {
            store_parse::store_i32(out, self.total_count);
        }
    }

    /// `RecommendedBots::parse` (BotRecommendationManager.cpp:83-100) —
    /// byte-exact; a row without the total count derives it from the
    /// vector size.
    pub fn parse(parser: &mut Parser) -> Result<RecommendedBots, &'static str> {
        let [has_bot_user_ids, has_total_count] = parser.parse_flags()?;
        let mut bot_user_ids = Vec::new();
        if has_bot_user_ids {
            let size = parser.i32_()?;
            if size < 0 {
                return Result::Err("Wrong vector length");
            }
            bot_user_ids.reserve(size as usize);
            for _ in 0..size {
                bot_user_ids.push(UserId::new(parser.i64_()?));
            }
        }
        let next_reload_time = parser.f64_()?;
        let total_count = if has_total_count {
            parser.i32_()?
        } else {
            bot_user_ids.len() as i32
        };
        Ok(RecommendedBots {
            total_count,
            bot_user_ids,
            next_reload_time,
        })
    }
}

/// The `get_bot_recommendations` early-error pair (the C++
/// `users_promise.set_error(...); count_promise.set_error(...)` block,
/// BotRecommendationManager.cpp:131-138, :141-147).
fn fail_both(
    mut users_promise: Promise<td_api::Users>,
    mut count_promise: Promise<i32>,
    error: Status,
) {
    if !users_promise.is_empty() {
        users_promise.set_error(error.clone());
    }
    if !count_promise.is_empty() {
        count_promise.set_error(error);
    }
}

/// The port of `td::BotRecommendationManager` (the actor shell collapses;
/// the C++ `td_`/`parent_` pair becomes the env-parameter methods).
#[derive(Default)]
pub struct BotRecommendationManager {
    /// `bot_recommended_bots_`.
    bot_recommended_bots: HashMap<UserId, RecommendedBots>,
    /// `get_bot_recommendations_queries_`.
    get_bot_recommendations_queries: HashMap<UserId, Vec<Promise<td_api::Users>>>,
    /// `get_bot_recommendation_count_queries_[2]` — indexed by
    /// `return_local`.
    get_bot_recommendation_count_queries: [HashMap<UserId, Vec<Promise<i32>>>; 2],
}

impl BotRecommendationManager {
    /// `BotRecommendationManager::BotRecommendationManager`
    /// (BotRecommendationManager.cpp:102-103).
    pub fn new() -> BotRecommendationManager {
        BotRecommendationManager::default()
    }

    /// `is_suitable_recommended_bot`
    /// (BotRecommendationManager.cpp:109-111).
    fn is_suitable_recommended_bot(
        env: &mut impl BotRecommendationManagerEnv,
        bot_user_id: UserId,
    ) -> bool {
        env.is_user_bot(bot_user_id)
    }

    /// `are_suitable_recommended_bots`
    /// (BotRecommendationManager.cpp:113-125).
    fn are_suitable_recommended_bots(
        env: &mut impl BotRecommendationManagerEnv,
        recommended_bots: &RecommendedBots,
    ) -> bool {
        for &recommended_bot_user_id in &recommended_bots.bot_user_ids {
            if !Self::is_suitable_recommended_bot(env, recommended_bot_user_id) {
                return false;
            }
        }
        let is_premium = env.get_option_boolean("is_premium");
        let have_all = recommended_bots.bot_user_ids.len() == recommended_bots.total_count as usize;
        if !have_all && is_premium {
            return false;
        }
        true
    }

    /// `get_bot_recommendations` (BotRecommendationManager.cpp:127-177).
    pub fn get_bot_recommendations(
        &mut self,
        env: &mut impl BotRecommendationManagerEnv,
        bot_user_id: UserId,
        return_local: bool,
        mut users_promise: Promise<td_api::Users>,
        mut count_promise: Promise<i32>,
    ) {
        let r_bot_input_user = env.get_input_user(bot_user_id);
        if let Err(error) = r_bot_input_user {
            // the C++ clones the error for each promise
            fail_both(users_promise, count_promise, error);
            return;
        }
        if !env.is_user_bot(bot_user_id) {
            fail_both(
                users_promise,
                count_promise,
                Status::error(400, "Bot not found"),
            );
            return;
        }
        let mut use_database = true;
        if let Some(recommended_bots) = self.bot_recommended_bots.get(&bot_user_id).cloned() {
            if Self::are_suitable_recommended_bots(env, &recommended_bots) {
                let next_reload_time = recommended_bots.next_reload_time;
                if !users_promise.is_empty() {
                    let users = env.get_users_object(
                        recommended_bots.total_count,
                        &recommended_bots.bot_user_ids,
                    );
                    users_promise.set_value(users);
                }
                if !count_promise.is_empty() {
                    count_promise.set_value(recommended_bots.total_count);
                }
                if next_reload_time > tdutils::time::now() {
                    return;
                }
                // the C++ nulls the promises (`users_promise = {}`); the
                // load keeps the empty holders exactly like the C++ pass
                users_promise = Promise::default();
                count_promise = Promise::default();
            } else {
                tdutils::log_info!("Drop cache for similar bots of {bot_user_id:?}");
                self.bot_recommended_bots.remove(&bot_user_id);
                if env.use_message_database() {
                    env.sqlite_pmc_erase(&Self::get_bot_recommendations_database_key(bot_user_id));
                }
            }
            use_database = false;
        }
        self.load_bot_recommendations(
            env,
            bot_user_id,
            use_database,
            return_local,
            users_promise,
            count_promise,
        );
    }

    /// `get_bot_recommendations_database_key`
    /// (BotRecommendationManager.cpp:179-181).
    fn get_bot_recommendations_database_key(bot_user_id: UserId) -> String {
        format!("bot_recommendations{}", bot_user_id.get())
    }

    /// `load_bot_recommendations` (BotRecommendationManager.cpp:183-203).
    fn load_bot_recommendations(
        &mut self,
        env: &mut impl BotRecommendationManagerEnv,
        bot_user_id: UserId,
        use_database: bool,
        return_local: bool,
        users_promise: Promise<td_api::Users>,
        count_promise: Promise<i32>,
    ) {
        let count_promise = count_promise;
        if !count_promise.is_empty() {
            self.get_bot_recommendation_count_queries[return_local as usize]
                .entry(bot_user_id)
                .or_default()
                .push(count_promise);
        }
        let queries = self
            .get_bot_recommendations_queries
            .entry(bot_user_id)
            .or_default();
        queries.push(users_promise);
        if queries.len() == 1 {
            if env.use_message_database() && use_database {
                // the C++ `SqliteKeyValueAsync::get` callback hops through
                // `on_load_bot_recommendations_from_database`; the
                // collapsed read is synchronous (the M.1 divergence)
                let value = env
                    .sqlite_pmc_get_raw(&Self::get_bot_recommendations_database_key(bot_user_id));
                self.on_load_bot_recommendations_from_database(env, bot_user_id, value);
            } else {
                self.reload_bot_recommendations(env, bot_user_id);
            }
        }
    }

    /// `fail_load_bot_recommendations_queries`
    /// (BotRecommendationManager.cpp:205-221).
    fn fail_load_bot_recommendations_queries(&mut self, bot_user_id: UserId, error: Status) {
        for return_local in 0..2 {
            if let Some(mut promises) =
                self.get_bot_recommendation_count_queries[return_local].remove(&bot_user_id)
            {
                for mut promise in promises.drain(..) {
                    promise.set_error(error.clone());
                }
            }
        }
        if let Some(mut promises) = self.get_bot_recommendations_queries.remove(&bot_user_id) {
            for mut promise in promises.drain(..) {
                promise.set_error(error.clone());
            }
        }
    }

    /// `finish_load_bot_recommendations_queries`
    /// (BotRecommendationManager.cpp:223-247).
    fn finish_load_bot_recommendations_queries(
        &mut self,
        env: &mut impl BotRecommendationManagerEnv,
        bot_user_id: UserId,
        total_count: i32,
        bot_user_ids: Vec<UserId>,
    ) {
        for return_local in 0..2 {
            if let Some(mut promises) =
                self.get_bot_recommendation_count_queries[return_local].remove(&bot_user_id)
            {
                for mut promise in promises.drain(..) {
                    promise.set_value(total_count);
                }
            }
        }
        if let Some(mut promises) = self.get_bot_recommendations_queries.remove(&bot_user_id) {
            for mut promise in promises.drain(..) {
                if !promise.is_empty() {
                    let users = env.get_users_object(total_count, &bot_user_ids);
                    promise.set_value(users);
                }
            }
        }
    }

    /// `on_load_bot_recommendations_from_database`
    /// (BotRecommendationManager.cpp:249-280) — the collapsed synchronous
    /// form of the pmc callback.
    fn on_load_bot_recommendations_from_database(
        &mut self,
        env: &mut impl BotRecommendationManagerEnv,
        bot_user_id: UserId,
        value: Vec<u8>,
    ) {
        if env.close_flag() {
            return self.fail_load_bot_recommendations_queries(
                bot_user_id,
                Status::error(500, "Request aborted"),
            );
        }

        if value.is_empty() {
            return self.reload_bot_recommendations(env, bot_user_id);
        }
        let mut parser = match Parser::new_log_event(&value) {
            Ok(parser) => parser,
            Err(_) => {
                self.bot_recommended_bots.remove(&bot_user_id);
                env.sqlite_pmc_erase(&Self::get_bot_recommendations_database_key(bot_user_id));
                return self.reload_bot_recommendations(env, bot_user_id);
            }
        };
        let parsed = RecommendedBots::parse(&mut parser);
        let recommended_bots = match parsed {
            Ok(recommended_bots) => recommended_bots,
            Err(_) => {
                self.bot_recommended_bots.remove(&bot_user_id);
                env.sqlite_pmc_erase(&Self::get_bot_recommendations_database_key(bot_user_id));
                return self.reload_bot_recommendations(env, bot_user_id);
            }
        };
        if !env.resolve_dependencies(
            &recommended_bots.bot_user_ids,
            "on_load_bot_recommendations_from_database",
        ) || !Self::are_suitable_recommended_bots(env, &recommended_bots)
        {
            self.bot_recommended_bots.remove(&bot_user_id);
            env.sqlite_pmc_erase(&Self::get_bot_recommendations_database_key(bot_user_id));
            return self.reload_bot_recommendations(env, bot_user_id);
        }

        let next_reload_time = recommended_bots.next_reload_time;
        let total_count = recommended_bots.total_count;
        let bot_user_ids = recommended_bots.bot_user_ids.clone();
        self.bot_recommended_bots
            .insert(bot_user_id, recommended_bots);
        self.finish_load_bot_recommendations_queries(env, bot_user_id, total_count, bot_user_ids);

        if next_reload_time <= tdutils::time::now() {
            self.load_bot_recommendations(
                env,
                bot_user_id,
                false,
                false,
                Promise::default(),
                Promise::default(),
            );
        }
    }

    /// `reload_bot_recommendations` (BotRecommendationManager.cpp:282-297)
    /// — the local-only count requests answer `-1` before the query send.
    fn reload_bot_recommendations(
        &mut self,
        env: &mut impl BotRecommendationManagerEnv,
        bot_user_id: UserId,
    ) {
        if let Some(mut promises) =
            self.get_bot_recommendation_count_queries[1].remove(&bot_user_id)
        {
            for mut promise in promises.drain(..) {
                promise.set_value(-1);
            }
        }
        env.send_get_bot_recommendations_query(bot_user_id);
    }

    /// `GetBotRecommendationsQuery::on_result` +
    /// `BotRecommendationManager::on_get_bot_recommendations`
    /// (BotRecommendationManager.cpp:50-64, :299-352).
    pub fn on_get_bot_recommendations(
        &mut self,
        env: &mut impl BotRecommendationManagerEnv,
        bot_user_id: UserId,
        r_users: Result<telegram_api::users_Users, Status>,
    ) {
        let users_ptr = match r_users {
            Err(error) => {
                if env.close_flag() {
                    return; // G()->ignore_result_if_closing
                }
                return self.fail_load_bot_recommendations_queries(bot_user_id, error);
            }
            Ok(users_ptr) => users_ptr,
        };

        let (mut total_count, users): (i32, Vec<telegram_api::User>) = match users_ptr {
            telegram_api::users_Users::users_users(users_obj) => {
                let total_count = users_obj.users.len() as i32;
                (total_count, users_obj.users)
            }
            telegram_api::users_Users::users_usersSlice(users_obj) => {
                let mut total_count = users_obj.count;
                if total_count < users_obj.users.len() as i32 {
                    tdutils::log_error!(
                        "Receive total_count = {total_count} and {} similar bots for {bot_user_id:?}",
                        users_obj.users.len()
                    );
                    total_count = users_obj.users.len() as i32;
                }
                (total_count, users_obj.users)
            }
        };

        // `td_->user_manager_->get_user_ids(users, source)` — the intake
        // returns the ids (UserManager.cpp:10242-10252)
        let mut recommended_bot_user_ids = Vec::with_capacity(users.len());
        for user in users {
            recommended_bot_user_ids.push(env.on_get_user(user, "on_get_bot_recommendations"));
        }
        let mut bot_user_ids = Vec::new();
        for recommended_bot_user_id in recommended_bot_user_ids {
            if Self::is_suitable_recommended_bot(env, recommended_bot_user_id) {
                bot_user_ids.push(recommended_bot_user_id);
            } else {
                total_count -= 1;
            }
        }
        let recommended_bots = self.bot_recommended_bots.entry(bot_user_id).or_default();
        recommended_bots.total_count = total_count;
        recommended_bots.bot_user_ids = bot_user_ids.clone();
        recommended_bots.next_reload_time =
            tdutils::time::now() + BOT_RECOMMENDATIONS_CACHE_TIME as f64;

        if env.use_message_database() {
            let mut value = Vec::new();
            store_parse::store_log_event_version(&mut value);
            recommended_bots.store(&mut value);
            env.sqlite_pmc_set_raw(
                &Self::get_bot_recommendations_database_key(bot_user_id),
                &value,
            );
        }

        self.finish_load_bot_recommendations_queries(env, bot_user_id, total_count, bot_user_ids);
    }

    /// `open_bot_recommended_bot` (BotRecommendationManager.cpp:354-366).
    pub fn open_bot_recommended_bot(
        &mut self,
        env: &mut impl BotRecommendationManagerEnv,
        bot_user_id: UserId,
        opened_bot_user_id: UserId,
        mut promise: Promise<()>,
    ) {
        if !env.is_user_bot(bot_user_id) || !env.is_user_bot(opened_bot_user_id) {
            return promise.set_error(Status::error(400, "Bot not found"));
        }
        let data = vec![
            telegram_api::JSONObjectValue {
                key: "ref_bot_id".to_owned(),
                value: Some(Box::new(telegram_api::JSONValue::jsonString(
                    telegram_api::jsonString {
                        value: bot_user_id.get().to_string(),
                    },
                ))),
            },
            telegram_api::JSONObjectValue {
                key: "open_bot_id".to_owned(),
                value: Some(Box::new(telegram_api::JSONValue::jsonString(
                    telegram_api::jsonString {
                        value: opened_bot_user_id.get().to_string(),
                    },
                ))),
            },
        ];
        env.save_app_log(
            "bots.open_recommended_bot",
            DialogId::new(0),
            telegram_api::JSONValue::jsonObject(telegram_api::jsonObject {
                value: data.into_iter().map(Box::new).collect(),
            }),
            promise,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdutils::promise::PromiseInterface;

    /// The recording env: the user cache (bot flags by id), the pmc rows,
    /// the option values and the fired query seams.
    struct TestEnv {
        bots: Vec<i64>,
        known: Vec<i64>,
        is_premium: bool,
        use_message_database: bool,
        pmc: HashMap<String, Vec<u8>>,
        queries: Vec<String>,
        app_logs: Vec<String>,
        resolved_all: bool,
    }

    impl Default for TestEnv {
        fn default() -> Self {
            TestEnv {
                bots: vec![1],
                known: vec![1, 2, 3],
                is_premium: false,
                use_message_database: true,
                pmc: HashMap::new(),
                queries: Vec::new(),
                app_logs: Vec::new(),
                resolved_all: true,
            }
        }
    }

    impl BotRecommendationManagerEnv for TestEnv {
        fn is_user_bot(&mut self, bot_user_id: UserId) -> bool {
            self.bots.contains(&bot_user_id.get())
        }

        fn get_input_user(
            &mut self,
            bot_user_id: UserId,
        ) -> Result<telegram_api::InputUser, Status> {
            if !self.known.contains(&bot_user_id.get()) {
                return Err(Status::error(400, "User not found"));
            }
            Ok(telegram_api::InputUser::inputUser(
                telegram_api::inputUser {
                    user_id: bot_user_id.get(),
                    access_hash: 0,
                },
            ))
        }

        fn on_get_user(&mut self, user: telegram_api::User, source: &str) -> UserId {
            self.queries.push(format!("{source}:intake"));
            let telegram_api::User::user(user) = user else {
                return UserId::default();
            };
            let user_id = UserId::new(user.id);
            if !self.known.contains(&user_id.get()) {
                self.known.push(user_id.get());
            }
            user_id
        }

        fn get_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
            user_id.get()
        }

        fn get_users_object(&mut self, total_count: i32, user_ids: &[UserId]) -> td_api::Users {
            td_api::Users {
                total_count,
                user_ids: user_ids.iter().map(|id| id.get()).collect(),
            }
        }

        fn get_option_boolean(&mut self, name: &str) -> bool {
            name == "is_premium" && self.is_premium
        }

        fn use_message_database(&mut self) -> bool {
            self.use_message_database
        }

        fn close_flag(&mut self) -> bool {
            false
        }

        fn sqlite_pmc_get_raw(&mut self, key: &str) -> Vec<u8> {
            self.pmc.get(key).cloned().unwrap_or_default()
        }

        fn sqlite_pmc_set_raw(&mut self, key: &str, value: &[u8]) {
            self.pmc.insert(key.to_owned(), value.to_vec());
        }

        fn sqlite_pmc_erase(&mut self, key: &str) {
            self.pmc.remove(key);
        }

        fn resolve_dependencies(&mut self, user_ids: &[UserId], _source: &str) -> bool {
            self.resolved_all && user_ids.iter().all(|id| self.known.contains(&id.get()))
        }

        fn send_get_bot_recommendations_query(&mut self, bot_user_id: UserId) {
            self.queries
                .push(format!("getBotRecommendations:{}", bot_user_id.get()));
        }

        fn save_app_log(
            &mut self,
            name: &str,
            _dialog_id: DialogId,
            _value: telegram_api::JSONValue,
            _promise: Promise<()>,
        ) {
            self.app_logs.push(name.to_owned());
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

    fn empty_users() -> Promise<td_api::Users> {
        Promise::default()
    }

    fn empty_count() -> Promise<i32> {
        Promise::default()
    }

    fn user(id: i64, is_bot: bool) -> telegram_api::User {
        telegram_api::User::user(telegram_api::user {
            // flags bit 14 = deleted, bit 15 = bot
            flags: if is_bot { 1 << 15 } else { 0 },
            id,
            ..Default::default()
        })
    }

    #[test]
    fn recommended_bots_store_parse_is_byte_symmetric() {
        let value = RecommendedBots {
            total_count: 2,
            bot_user_ids: vec![UserId::new(1), UserId::new(2)],
            next_reload_time: 1234.5,
        };
        let mut stored = Vec::new();
        store_parse::store_log_event_version(&mut stored);
        value.store(&mut stored);
        let mut parser = Parser::new_log_event(&stored).expect("framed");
        let parsed = RecommendedBots::parse(&mut parser).expect("parses");
        parser.fetch_end().expect("fully consumed");
        assert_eq!(parsed, value);

        // the no-ids shape derives the total count from the size
        let bare = RecommendedBots {
            total_count: 0,
            bot_user_ids: Vec::new(),
            next_reload_time: 9.0,
        };
        let mut stored = Vec::new();
        bare.store(&mut stored);
        let mut parser = Parser::new(&stored);
        let parsed = RecommendedBots::parse(&mut parser).expect("parses");
        parser.fetch_end().expect("fully consumed");
        assert_eq!(parsed, bare);
        assert_eq!(parsed.total_count, 0);

        // a total-count mismatch keeps the flag and the explicit count
        let mismatch = RecommendedBots {
            total_count: 7,
            bot_user_ids: vec![UserId::new(1)],
            next_reload_time: 1.0,
        };
        let mut stored = Vec::new();
        mismatch.store(&mut stored);
        let mut parser = Parser::new(&stored);
        let parsed = RecommendedBots::parse(&mut parser).expect("parses");
        assert_eq!(parsed.total_count, 7);
        assert_eq!(parsed.bot_user_ids, vec![UserId::new(1)]);
    }

    #[test]
    fn get_bot_recommendations_rejects_unknown_and_non_bot_users() {
        let mut env = TestEnv::default();
        let mut manager = BotRecommendationManager::new();

        // an unknown bot fails the input-user guard for both promises
        let (users_promise, users_outcomes) = recording_promise();
        let (count_promise, count_outcomes) = recording_promise();
        manager.get_bot_recommendations(
            &mut env,
            UserId::new(999),
            false,
            users_promise,
            count_promise,
        );
        assert!(matches!(&users_outcomes.lock().unwrap()[0], Err(m) if m == "400 User not found"));
        assert!(matches!(&count_outcomes.lock().unwrap()[0], Err(m) if m == "400 User not found"));

        // a known non-bot answers "Bot not found"
        env.known.push(50);
        let (users_promise, users_outcomes) = recording_promise();
        let (count_promise, count_outcomes) = recording_promise();
        manager.get_bot_recommendations(
            &mut env,
            UserId::new(50),
            false,
            users_promise,
            count_promise,
        );
        assert!(matches!(&users_outcomes.lock().unwrap()[0], Err(m) if m == "400 Bot not found"));
        assert!(matches!(&count_outcomes.lock().unwrap()[0], Err(m) if m == "400 Bot not found"));
    }

    #[test]
    fn get_bot_recommendations_loads_then_caches() {
        let mut env = TestEnv::default();
        let mut manager = BotRecommendationManager::new();

        // the first request parks the query (no cache)
        let (users_promise, users_outcomes) = recording_promise();
        let (count_promise, count_outcomes) = recording_promise();
        manager.get_bot_recommendations(
            &mut env,
            UserId::new(1),
            false,
            users_promise,
            count_promise,
        );
        assert_eq!(env.queries, vec!["getBotRecommendations:1"]);
        assert!(users_outcomes.lock().unwrap().is_empty());

        // the query answer fills both promises and stores the pmc row
        env.bots = vec![1, 3];
        manager.on_get_bot_recommendations(
            &mut env,
            UserId::new(1),
            Ok(telegram_api::users_Users::users_usersSlice(
                telegram_api::users_usersSlice {
                    count: 3,
                    users: vec![user(1, true), user(2, false), user(3, true)],
                },
            )),
        );
        let users_outcomes = users_outcomes.lock().unwrap();
        let users = users_outcomes[0].as_ref().expect("answered");
        // the non-bot decrements the total count and is filtered out
        assert_eq!(users.total_count, 2);
        assert_eq!(users.user_ids, vec![1, 3]);
        assert_eq!(count_outcomes.lock().unwrap().clone(), vec![Ok(2)]);
        assert!(env.pmc.contains_key("bot_recommendations1"));

        // a second request inside the cache window answers synchronously
        env.queries.clear();
        let (users_promise, users_outcomes) = recording_promise();
        let (count_promise, count_outcomes) = recording_promise();
        manager.get_bot_recommendations(
            &mut env,
            UserId::new(1),
            false,
            users_promise,
            count_promise,
        );
        assert!(env.queries.is_empty());
        assert_eq!(users_outcomes.lock().unwrap().len(), 1);
        assert_eq!(count_outcomes.lock().unwrap().clone(), vec![Ok(2)]);
    }

    #[test]
    fn expired_cache_reloads_through_the_row() {
        let mut env = TestEnv::default();
        let mut manager = BotRecommendationManager::new();

        // load + answer
        manager.get_bot_recommendations(
            &mut env,
            UserId::new(1),
            false,
            empty_users(),
            empty_count(),
        );
        manager.on_get_bot_recommendations(
            &mut env,
            UserId::new(1),
            Ok(telegram_api::users_Users::users_users(
                telegram_api::users_users {
                    users: vec![user(3, true)],
                },
            )),
        );

        // age the row past the cache window
        manager
            .bot_recommended_bots
            .get_mut(&UserId::new(1))
            .expect("cached")
            .next_reload_time = tdutils::time::now() - 1.0;

        // the stale cache answers the promises, then the row (already
        // fresh in memory) skips the second reload
        let (users_promise, users_outcomes) = recording_promise();
        manager.get_bot_recommendations(
            &mut env,
            UserId::new(1),
            false,
            users_promise,
            empty_count(),
        );
        assert_eq!(users_outcomes.lock().unwrap().len(), 1);
    }

    #[test]
    fn on_load_from_database_recovers_a_corrupt_row() {
        let mut env = TestEnv::default();
        let mut manager = BotRecommendationManager::new();

        // prime a real row, then corrupt it behind the manager's back
        manager.get_bot_recommendations(
            &mut env,
            UserId::new(1),
            false,
            empty_users(),
            empty_count(),
        );
        manager.on_get_bot_recommendations(
            &mut env,
            UserId::new(1),
            Ok(telegram_api::users_Users::users_users(
                telegram_api::users_users {
                    users: vec![user(1, true)],
                },
            )),
        );
        assert!(env.pmc.contains_key("bot_recommendations1"));
        env.pmc
            .insert("bot_recommendations1".to_owned(), b"garbage".to_vec());
        // the corrupt row is only reachable after a restart drops the
        // in-memory cache
        manager.bot_recommended_bots.clear();

        // a fresh load reads the corrupt row, erases it and reloads
        env.queries.clear();
        manager.get_bot_recommendations(
            &mut env,
            UserId::new(1),
            false,
            empty_users(),
            empty_count(),
        );
        assert!(!env.pmc.contains_key("bot_recommendations1"));
        assert!(env.queries.contains(&"getBotRecommendations:1".to_owned()));
    }

    #[test]
    fn reload_answers_the_local_count_with_minus_one() {
        let mut env = TestEnv::default();
        let mut manager = BotRecommendationManager::new();

        // return_local = true parks a count promise that answers -1 on the
        // reload path
        let (count_promise, count_outcomes) = recording_promise();
        manager.get_bot_recommendations(
            &mut env,
            UserId::new(1),
            true,
            empty_users(),
            count_promise,
        );
        assert_eq!(count_outcomes.lock().unwrap().clone(), vec![Ok(-1)]);
        assert_eq!(env.queries, vec!["getBotRecommendations:1"]);

        // a query error fails the parked users promise
        let (users_promise, users_outcomes) = recording_promise();
        manager.load_bot_recommendations(
            &mut env,
            UserId::new(1),
            false,
            false,
            users_promise,
            empty_count(),
        );
        manager.on_get_bot_recommendations(
            &mut env,
            UserId::new(1),
            Err(Status::error(400, "BOT_INVALID")),
        );
        assert!(matches!(&users_outcomes.lock().unwrap()[0], Err(m) if m == "400 BOT_INVALID"));
    }

    #[test]
    fn open_bot_recommended_bot_guards_and_logs() {
        let mut env = TestEnv::default();
        let mut manager = BotRecommendationManager::new();

        // a non-bot opened id fails the guard
        let (promise, outcomes) = recording_promise();
        manager.open_bot_recommended_bot(&mut env, UserId::new(1), UserId::new(2), promise);
        assert!(matches!(&outcomes.lock().unwrap()[0], Err(m) if m == "400 Bot not found"));
        assert!(env.app_logs.is_empty());

        // a bot pair reaches the app-log seam with the ref/open payload
        env.known.push(2);
        env.bots.push(2);
        let (promise, outcomes) = recording_promise();
        manager.open_bot_recommended_bot(&mut env, UserId::new(1), UserId::new(2), promise);
        assert_eq!(env.app_logs, vec!["bots.open_recommended_bot"]);
        assert!(outcomes.lock().unwrap().is_empty());
    }
}
