use crate::chat_manager::ChannelId;
use crate::dialog_id::{DialogId, DialogType};
use crate::store_parse::{Parser, store_flags, store_i32};
use std::collections::HashMap;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The in-flight query kind of the net-drain 61 channel-recommendation
/// family — the C++ `GetChannelRecommendationsQuery`
/// (ChannelRecommendationManager.cpp:31-84) collapsed onto a dispatcher
/// tag; the answers drain through
/// [`crate::td::Td::on_channel_recommendation_net_answer`] over the
/// env-carried [`crate::td::ChannelRecommendationNetHandle`]. The kind
/// carries the handler's `channel_id_` (the zero form is the
/// recommended-channels reload, cpp:47-48) for the answer routing and
/// the `on_get_channel_error` head (cpp:75-79); the promise lambda's
/// actor hop collapses onto the drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChannelRecommendationNetKind {
    /// `GetChannelRecommendationsQuery` (ChannelRecommendationManager.cpp:31-84)
    /// — `channels.getChannelRecommendations`; the zero channel id is
    /// the recommended-channels form.
    GetChannelRecommendations { channel_id: ChannelId },
}

/// `ChannelRecommendationManager::CHANNEL_RECOMMENDATIONS_CACHE_TIME`
/// (ChannelRecommendationManager.h:76) — stamps the reload deadline that
/// the `reload_recommended_channels` net tail consumes.
const CHANNEL_RECOMMENDATIONS_CACHE_TIME: f64 = 24.0 * 60.0 * 60.0;

/// The `Td *td` slice of the collapsed ChannelRecommendationManager.
pub trait ChannelRecommendationManagerEnv {
    fn close_flag(&self) -> bool;
    fn now(&self) -> f64;
    fn use_message_database(&self) -> bool;
    fn get_option_boolean(&self, name: &str, default_value: bool) -> bool;

    // -- ChatManager probes
    /// `chat_manager_->get_channel_status(channel_id).is_member()`.
    fn channel_is_member(&self, channel_id: ChannelId) -> bool;
    /// `chat_manager_->have_input_peer_channel(channel_id,
    /// AccessRights::Read)`.
    fn have_input_peer_channel(&mut self, channel_id: ChannelId) -> bool;
    /// `chat_manager_->is_broadcast_channel(channel_id)`.
    fn is_broadcast_channel(&self, channel_id: ChannelId) -> bool;
    /// `chat_manager_->get_input_channel(channel_id)` — `None` is the
    /// C++ nullptr.
    fn get_input_channel(&mut self, channel_id: ChannelId) -> bool;

    // -- DialogManager probes
    /// `dialog_manager_->have_dialog_force(dialog_id, source)`.
    fn have_dialog_force(&self, dialog_id: DialogId) -> bool;
    /// `dialog_manager_->force_create_dialog(dialog_id, source)` — the
    /// M.6 stub no-op.
    fn force_create_dialog(&mut self, dialog_id: DialogId);
    /// `dialog_manager_->get_chats_object(total_count, dialog_ids,
    /// source)`.
    fn get_chats_object(
        &mut self,
        total_count: i32,
        dialog_ids: &[DialogId],
    ) -> crate::td_api::Chats;

    // -- the sqlite pmc (the recommended-channels records)
    fn sqlite_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;
    fn sqlite_pmc_set_raw(&mut self, key: &[u8], value: &[u8]);
    fn sqlite_pmc_erase(&mut self, key: &[u8]);

    // -- `ChatManager::get_channel_ids(chats, source)` — the answer walk.
    fn get_channel_ids(&mut self, chats: Vec<telegram_api::Chat>, source: &str) -> Vec<ChannelId>;

    /// `ChatManager::on_get_channel_error` — the answer-walk error head.
    fn on_get_channel_error(
        &mut self,
        channel_id: ChannelId,
        status: &Status,
        source: &str,
    ) -> bool;

    /// `ChatManager::get_input_channel(channel_id)` — the wire form (the
    /// `None` is the C++ nullptr).
    fn get_input_channel_object(
        &mut self,
        channel_id: ChannelId,
    ) -> Option<tdmtproto::telegram_api::InputChannel>;

    // -- the net-drain 61 query send arm (the td.rs override dispatches
    // -- through the ChannelRecommendationNetKind plane; the default
    // -- keeps the offline unit-test posture)
    /// `GetChannelRecommendationsQuery::send` (ChannelRecommendationManager.cpp:
    /// 47-58) — `reload_recommended_channels` with an empty channel id
    /// or `reload_channel_recommendations` with a valid one.
    fn reload_recommended_channels(&mut self, channel_id: ChannelId);
    /// `save_app_log` (Application.cpp) — the open-channel app log;
    /// [blocked: net] (the app-log plane), the promise parks.
    fn save_app_log(&mut self, ref_channel_id: i64, open_channel_id: i64, promise: Promise<()>);
}

/// `ChannelRecommendationManager::RecommendedDialogs`
/// (ChannelRecommendationManager.h:52-62) — the cached record.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecommendedDialogs {
    pub dialog_ids: Vec<DialogId>,
    pub total_count: i32,
    pub next_reload_time: f64,
}

impl RecommendedDialogs {
    /// `RecommendedDialogs::store` (ChannelRecommendationManager.cpp:87-101).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_dialog_ids = !self.dialog_ids.is_empty();
        let has_total_count = self.total_count as usize != self.dialog_ids.len();
        store_flags(out, [has_dialog_ids, has_total_count]);
        if has_dialog_ids {
            store_i32(out, self.dialog_ids.len() as i32);
            for dialog_id in &self.dialog_ids {
                dialog_id.store(out);
            }
        }
        crate::store_parse::store_f64(out, self.next_reload_time);
        if has_total_count {
            store_i32(out, self.total_count);
        }
    }

    /// `RecommendedDialogs::parse` (ChannelRecommendationManager.cpp:103-120).
    pub fn parse(parser: &mut Parser<'_>) -> Result<RecommendedDialogs, &'static str> {
        let [has_dialog_ids, has_total_count] = parser.parse_flags()?;
        let mut dialog_ids = Vec::new();
        if has_dialog_ids {
            let len = parser.i32_()?;
            if len < 0 {
                return Err("Wrong vector length");
            }
            for _ in 0..len {
                dialog_ids.push(DialogId::parse_from(parser)?);
            }
        }
        let next_reload_time = parser.f64_()?;
        let total_count = if has_total_count {
            parser.i32_()?
        } else {
            dialog_ids.len() as i32
        };
        Ok(RecommendedDialogs {
            dialog_ids,
            total_count,
            next_reload_time,
        })
    }
}

/// `td::ChannelRecommendationManager` (ChannelRecommendationManager.h:47).
#[derive(Default)]
pub struct ChannelRecommendationManager {
    pub(crate) are_recommended_channels_inited: bool,
    pub(crate) recommended_channels: RecommendedDialogs,
    pub(crate) channel_recommended_dialogs: HashMap<ChannelId, RecommendedDialogs>,
    pub(crate) get_recommended_channels_queries: Vec<Promise<crate::td_api::Chats>>,
    pub(crate) get_channel_recommendations_queries:
        HashMap<ChannelId, Vec<Promise<crate::td_api::Chats>>>,
    pub(crate) get_channel_recommendation_count_queries:
        HashMap<(bool, ChannelId), Vec<Promise<i32>>>,
}

impl ChannelRecommendationManager {
    pub fn new() -> ChannelRecommendationManager {
        ChannelRecommendationManager::default()
    }

    fn get_recommended_channels_database_key() -> Vec<u8> {
        b"recommended_channels".to_vec()
    }

    fn get_channel_recommendations_database_key(channel_id: ChannelId) -> Vec<u8> {
        format!("channel_recommendations{}", channel_id.get()).into_bytes()
    }

    /// `is_suitable_recommended_channel(ChannelId)`
    /// (ChannelRecommendationManager.cpp:140-143).
    pub fn is_suitable_recommended_channel(
        &self,
        env: &mut impl ChannelRecommendationManagerEnv,
        channel_id: ChannelId,
    ) -> bool {
        !env.channel_is_member(channel_id) && env.have_input_peer_channel(channel_id)
    }

    /// `is_suitable_recommended_channel(DialogId)`
    /// (ChannelRecommendationManager.cpp:133-138).
    fn is_suitable_recommended_dialog(
        &self,
        env: &mut impl ChannelRecommendationManagerEnv,
        dialog_id: DialogId,
    ) -> bool {
        if dialog_id.get_type() != DialogType::Channel {
            return false;
        }
        self.is_suitable_recommended_channel(env, dialog_id.get_channel_id())
    }

    /// `are_suitable_recommended_dialogs`
    /// (ChannelRecommendationManager.cpp:145-158).
    fn are_suitable_recommended_dialogs(
        &self,
        env: &mut impl ChannelRecommendationManagerEnv,
        recommended_dialogs: &RecommendedDialogs,
    ) -> bool {
        for &recommended_dialog_id in &recommended_dialogs.dialog_ids {
            if !self.is_suitable_recommended_dialog(env, recommended_dialog_id) {
                return false;
            }
        }
        let is_premium = env.get_option_boolean("is_premium", false);
        let have_all =
            recommended_dialogs.dialog_ids.len() as i32 == recommended_dialogs.total_count;
        have_all || !is_premium
    }

    /// `get_recommended_channels` (ChannelRecommendationManager.cpp:160-181).
    pub fn get_recommended_channels(
        &mut self,
        env: &mut impl ChannelRecommendationManagerEnv,
        mut promise: Promise<crate::td_api::Chats>,
    ) {
        let mut use_database = true;
        if self.are_recommended_channels_inited {
            if self.are_suitable_recommended_dialogs(env, &self.recommended_channels) {
                let next_reload_time = self.recommended_channels.next_reload_time;
                let object = env.get_chats_object(
                    self.recommended_channels.total_count,
                    &self.recommended_channels.dialog_ids.clone(),
                );
                promise.set_value(object);
                if next_reload_time > env.now() {
                    return;
                }
                // the C++ clears the promise and continues into the load
            } else {
                self.are_recommended_channels_inited = false;
                if env.use_message_database() {
                    env.sqlite_pmc_erase(&Self::get_recommended_channels_database_key());
                }
            }
            use_database = false;
        }
        self.load_recommended_channels(env, use_database, promise);
    }

    /// `load_recommended_channels` (ChannelRecommendationManager.cpp:187-201).
    fn load_recommended_channels(
        &mut self,
        env: &mut impl ChannelRecommendationManagerEnv,
        use_database: bool,
        promise: Promise<crate::td_api::Chats>,
    ) {
        self.get_recommended_channels_queries.push(promise);
        if self.get_recommended_channels_queries.len() == 1 {
            if env.use_message_database() && use_database {
                // the collapsed sync pmc read (the C++ async get +
                // on_load_recommended_channels_from_database hop)
                let value = env.sqlite_pmc_get_raw(&Self::get_recommended_channels_database_key());
                self.on_load_recommended_channels_from_database(env, value);
            } else {
                self.reload_recommended_channels(env);
            }
        }
    }

    /// `reload_recommended_channels` (ChannelRecommendationManager.cpp:229-236)
    /// — the promise lambda's actor hop collapses onto the
    /// `ChannelRecommendationNetKind` drain.
    fn reload_recommended_channels(&mut self, env: &mut impl ChannelRecommendationManagerEnv) {
        env.reload_recommended_channels(ChannelId::new(0));
    }

    /// `fail_load_recommended_channels_queries`
    /// (ChannelRecommendationManager.cpp:203-206).
    fn fail_load_recommended_channels_queries(&mut self, error: Status) {
        assert!(!self.get_recommended_channels_queries.is_empty());
        let queries = std::mem::take(&mut self.get_recommended_channels_queries);
        for mut promise in queries {
            promise.set_error(error.clone());
        }
    }

    /// `finish_load_recommended_channels_queries`
    /// (ChannelRecommendationManager.cpp:208-218).
    fn finish_load_recommended_channels_queries(
        &mut self,
        env: &mut impl ChannelRecommendationManagerEnv,
        total_count: i32,
        dialog_ids: Vec<DialogId>,
    ) {
        self.are_recommended_channels_inited = true;
        let queries = std::mem::take(&mut self.get_recommended_channels_queries);
        assert!(!queries.is_empty());
        for promise in queries {
            let object = env.get_chats_object(total_count, &dialog_ids);
            let mut promise = promise;
            promise.set_value(object);
        }
    }

    /// `on_load_recommended_channels_from_database`
    /// (ChannelRecommendationManager.cpp:220-248) — the C++
    /// `Dependencies::resolve_force` over the record's dialogs collapses
    /// onto the suitability probe (the same user/channel reads the
    /// walk's is_suitable band makes).
    fn on_load_recommended_channels_from_database(
        &mut self,
        env: &mut impl ChannelRecommendationManagerEnv,
        value: Vec<u8>,
    ) {
        if env.close_flag() {
            return self
                .fail_load_recommended_channels_queries(Status::error(500, "Request aborted"));
        }
        if value.is_empty() {
            return self.reload_recommended_channels(env);
        }
        let record = match RecommendedDialogs::parse(&mut Parser::new(&value)) {
            Ok(record) => record,
            Err(_) => {
                self.recommended_channels = RecommendedDialogs::default();
                env.sqlite_pmc_erase(&Self::get_recommended_channels_database_key());
                return self.reload_recommended_channels(env);
            }
        };
        if !self.are_suitable_recommended_dialogs(env, &record) {
            self.recommended_channels = RecommendedDialogs::default();
            env.sqlite_pmc_erase(&Self::get_recommended_channels_database_key());
            return self.reload_recommended_channels(env);
        }
        let next_reload_time = record.next_reload_time;
        let total_count = record.total_count;
        let dialog_ids = record.dialog_ids.clone();
        self.recommended_channels = record;
        self.finish_load_recommended_channels_queries(env, total_count, dialog_ids);
        if next_reload_time <= env.now() {
            self.load_recommended_channels(env, false, Promise::default());
        }
    }

    /// `on_get_recommended_channels` (ChannelRecommendationManager.cpp:250-287)
    /// — the recommended-channels reload answer walk (the net-drain 61
    /// drain routes here with the parsed `(count, chats)` pair).
    pub(crate) fn on_get_recommended_channels(
        &mut self,
        env: &mut impl ChannelRecommendationManagerEnv,
        result: Result<(i32, Vec<telegram_api::Chat>), Status>,
    ) {
        // `G()->ignore_result_if_closing` (cpp:252)
        if env.close_flag() {
            return;
        }
        let (mut total_count, chats) = match result {
            Ok(pair) => pair,
            Err(error) => return self.fail_load_recommended_channels_queries(error),
        };
        let channel_ids = env.get_channel_ids(chats, "on_get_recommended_channels");
        let mut dialog_ids = Vec::new();
        if total_count < channel_ids.len() as i32 {
            tdutils::log_error!(
                "Receive total_count = {total_count} and {} recommended chats",
                channel_ids.len()
            );
            total_count = channel_ids.len() as i32;
        }
        for recommended_channel_id in channel_ids {
            let recommended_dialog_id = DialogId::from_channel_id(recommended_channel_id);
            env.force_create_dialog(recommended_dialog_id);
            if self.is_suitable_recommended_channel(env, recommended_channel_id) {
                dialog_ids.push(recommended_dialog_id);
            } else {
                total_count -= 1;
            }
        }
        self.recommended_channels = RecommendedDialogs {
            total_count,
            next_reload_time: env.now() + CHANNEL_RECOMMENDATIONS_CACHE_TIME,
            dialog_ids: dialog_ids.clone(),
        };
        if env.use_message_database() {
            let mut data = Vec::new();
            self.recommended_channels.store(&mut data);
            env.sqlite_pmc_set_raw(&Self::get_recommended_channels_database_key(), &data);
        }
        self.finish_load_recommended_channels_queries(env, total_count, dialog_ids);
    }

    /// `get_channel_recommendations` (ChannelRecommendationManager.cpp:298-359).
    pub fn get_channel_recommendations(
        &mut self,
        env: &mut impl ChannelRecommendationManagerEnv,
        dialog_id: DialogId,
        return_local: bool,
        mut chats_promise: Option<Promise<crate::td_api::Chats>>,
        mut count_promise: Option<Promise<i32>>,
    ) {
        if !env.have_dialog_force(dialog_id) {
            if let Some(mut promise) = chats_promise {
                promise.set_error(Status::error(400, "Chat not found"));
            }
            if let Some(mut promise) = count_promise {
                promise.set_error(Status::error(400, "Chat not found"));
            }
            return;
        }
        if dialog_id.get_type() != DialogType::Channel {
            if let Some(mut promise) = chats_promise {
                promise.set_value(crate::td_api::Chats {
                    total_count: 0,
                    chat_ids: Vec::new(),
                });
            }
            if let Some(mut promise) = count_promise {
                promise.set_value(0);
            }
            return;
        }
        let channel_id = dialog_id.get_channel_id();
        if !env.is_broadcast_channel(channel_id) || !env.get_input_channel(channel_id) {
            if let Some(mut promise) = chats_promise {
                promise.set_value(crate::td_api::Chats {
                    total_count: 0,
                    chat_ids: Vec::new(),
                });
            }
            if let Some(mut promise) = count_promise {
                promise.set_value(0);
            }
            return;
        }
        // the cached-record path (ChannelRecommendationManager.cpp:341-363)
        let mut use_database = true;
        if let Some(record) = self.channel_recommended_dialogs.get(&channel_id) {
            if self.are_suitable_recommended_dialogs(env, record) {
                let next_reload_time = record.next_reload_time;
                if let Some(mut promise) = chats_promise.take() {
                    let object = env.get_chats_object(record.total_count, &record.dialog_ids);
                    promise.set_value(object);
                }
                if let Some(mut promise) = count_promise.take() {
                    promise.set_value(record.total_count);
                }
                if next_reload_time > env.now() {
                    return;
                }
            } else {
                tdutils::log_info!("Drop cache for similar chats of {dialog_id:?}");
                self.channel_recommended_dialogs.remove(&channel_id);
                if env.use_message_database() {
                    env.sqlite_pmc_erase(&Self::get_channel_recommendations_database_key(
                        channel_id,
                    ));
                }
            }
            use_database = false;
        }
        self.load_channel_recommendations(
            env,
            channel_id,
            use_database,
            return_local,
            chats_promise,
            count_promise,
        );
    }

    /// `load_channel_recommendations` (ChannelRecommendationManager.cpp:365-385).
    fn load_channel_recommendations(
        &mut self,
        env: &mut impl ChannelRecommendationManagerEnv,
        channel_id: ChannelId,
        use_database: bool,
        return_local: bool,
        chats_promise: Option<Promise<crate::td_api::Chats>>,
        count_promise: Option<Promise<i32>>,
    ) {
        if let Some(count_promise) = count_promise {
            self.get_channel_recommendation_count_queries
                .entry((return_local, channel_id))
                .or_default()
                .push(count_promise);
        }
        let queries = self
            .get_channel_recommendations_queries
            .entry(channel_id)
            .or_default();
        // the C++ pushes the (possibly null) chats promise unconditionally
        // (cpp:380) — the count-only requests must still arm the reload;
        // the port's empty `Promise::default()` is the null form
        queries.push(chats_promise.unwrap_or_default());
        if queries.len() == 1 {
            if env.use_message_database() && use_database {
                // the collapsed sync pmc read (the C++ async get +
                // on_load_channel_recommendations_from_database hop)
                let value = env.sqlite_pmc_get_raw(
                    &Self::get_channel_recommendations_database_key(channel_id),
                );
                self.on_load_channel_recommendations_from_database(env, channel_id, value);
            } else {
                self.reload_channel_recommendations(env, channel_id);
            }
        }
    }

    /// `reload_channel_recommendations` (ChannelRecommendationManager.cpp:476-493)
    /// — the `return_local` count queries answer -1 before the query
    /// (cpp:477-487), then the wire send rides the env arm.
    fn reload_channel_recommendations(
        &mut self,
        env: &mut impl ChannelRecommendationManagerEnv,
        channel_id: ChannelId,
    ) {
        if let Some(promises) = self
            .get_channel_recommendation_count_queries
            .remove(&(true, channel_id))
        {
            for mut promise in promises {
                promise.set_value(-1);
            }
        }
        env.reload_recommended_channels(channel_id);
    }

    /// `fail_load_channel_recommendations_queries`
    /// (ChannelRecommendationManager.cpp:387-403).
    fn fail_load_channel_recommendations_queries(&mut self, channel_id: ChannelId, error: Status) {
        for return_local in [false, true] {
            if let Some(promises) = self
                .get_channel_recommendation_count_queries
                .remove(&(return_local, channel_id))
            {
                assert!(!promises.is_empty());
                for mut promise in promises {
                    promise.set_error(error.clone());
                }
            }
        }
        let promises = self
            .get_channel_recommendations_queries
            .remove(&channel_id)
            .expect("the queries park before the reload");
        assert!(!promises.is_empty());
        for mut promise in promises {
            promise.set_error(error.clone());
        }
    }

    /// `finish_load_channel_recommendations_queries`
    /// (ChannelRecommendationManager.cpp:405-423).
    fn finish_load_channel_recommendations_queries(
        &mut self,
        env: &mut impl ChannelRecommendationManagerEnv,
        channel_id: ChannelId,
        total_count: i32,
        dialog_ids: Vec<DialogId>,
    ) {
        for return_local in [false, true] {
            if let Some(promises) = self
                .get_channel_recommendation_count_queries
                .remove(&(return_local, channel_id))
            {
                assert!(!promises.is_empty());
                for mut promise in promises {
                    promise.set_value(total_count);
                }
            }
        }
        let promises = self
            .get_channel_recommendations_queries
            .remove(&channel_id)
            .expect("the queries park before the reload");
        assert!(!promises.is_empty());
        for promise in promises {
            let object = env.get_chats_object(total_count, &dialog_ids);
            let mut promise = promise;
            promise.set_value(object);
        }
    }

    /// `on_load_channel_recommendations_from_database`
    /// (ChannelRecommendationManager.cpp:425-455) — the same collapsed
    /// sync-pmc + suitability shape as the recommended-channels half.
    fn on_load_channel_recommendations_from_database(
        &mut self,
        env: &mut impl ChannelRecommendationManagerEnv,
        channel_id: ChannelId,
        value: Vec<u8>,
    ) {
        if env.close_flag() {
            return self.fail_load_channel_recommendations_queries(
                channel_id,
                Status::error(500, "Request aborted"),
            );
        }
        if value.is_empty() {
            return self.reload_channel_recommendations(env, channel_id);
        }
        let record = match RecommendedDialogs::parse(&mut Parser::new(&value)) {
            Ok(record) => record,
            Err(_) => {
                self.channel_recommended_dialogs.remove(&channel_id);
                env.sqlite_pmc_erase(&Self::get_channel_recommendations_database_key(channel_id));
                return self.reload_channel_recommendations(env, channel_id);
            }
        };
        if !self.are_suitable_recommended_dialogs(env, &record) {
            self.channel_recommended_dialogs.remove(&channel_id);
            env.sqlite_pmc_erase(&Self::get_channel_recommendations_database_key(channel_id));
            return self.reload_channel_recommendations(env, channel_id);
        }
        let next_reload_time = record.next_reload_time;
        let total_count = record.total_count;
        let dialog_ids = record.dialog_ids.clone();
        self.channel_recommended_dialogs.insert(channel_id, record);
        self.finish_load_channel_recommendations_queries(env, channel_id, total_count, dialog_ids);
        if next_reload_time <= env.now() {
            self.load_channel_recommendations(env, channel_id, false, false, None, None);
        }
    }

    /// `on_get_channel_recommendations` (ChannelRecommendationManager.cpp:495-522)
    /// — the per-channel reload answer walk.
    pub(crate) fn on_get_channel_recommendations(
        &mut self,
        env: &mut impl ChannelRecommendationManagerEnv,
        channel_id: ChannelId,
        result: Result<(i32, Vec<telegram_api::Chat>), Status>,
    ) {
        // `G()->ignore_result_if_closing` (cpp:497)
        if env.close_flag() {
            return;
        }
        let (mut total_count, chats) = match result {
            Ok(pair) => pair,
            Err(error) => return self.fail_load_channel_recommendations_queries(channel_id, error),
        };
        let channel_ids = env.get_channel_ids(chats, "on_get_channel_recommendations");
        let mut dialog_ids = Vec::new();
        if total_count < channel_ids.len() as i32 {
            tdutils::log_error!(
                "Receive total_count = {total_count} and {} similar chats for {channel_id:?}",
                channel_ids.len()
            );
            total_count = channel_ids.len() as i32;
        }
        for recommended_channel_id in channel_ids {
            let recommended_dialog_id = DialogId::from_channel_id(recommended_channel_id);
            env.force_create_dialog(recommended_dialog_id);
            if self.is_suitable_recommended_channel(env, recommended_channel_id) {
                dialog_ids.push(recommended_dialog_id);
            } else {
                total_count -= 1;
            }
        }
        self.channel_recommended_dialogs.insert(
            channel_id,
            RecommendedDialogs {
                total_count,
                next_reload_time: env.now() + CHANNEL_RECOMMENDATIONS_CACHE_TIME,
                dialog_ids: dialog_ids.clone(),
            },
        );
        if env.use_message_database() {
            let record = &self.channel_recommended_dialogs[&channel_id];
            let mut data = Vec::new();
            record.store(&mut data);
            env.sqlite_pmc_set_raw(
                &Self::get_channel_recommendations_database_key(channel_id),
                &data,
            );
        }
        self.finish_load_channel_recommendations_queries(env, channel_id, total_count, dialog_ids);
    }

    /// `open_channel_recommended_channel`
    /// (ChannelRecommendationManager.cpp:524-540).
    pub fn open_channel_recommended_channel(
        &mut self,
        env: &mut impl ChannelRecommendationManagerEnv,
        dialog_id: DialogId,
        opened_dialog_id: DialogId,
        mut promise: Promise<()>,
    ) {
        if !env.have_dialog_force(dialog_id) || !env.have_dialog_force(opened_dialog_id) {
            return promise.set_error(Status::error(400, "Chat not found"));
        }
        if dialog_id.get_type() != DialogType::Channel
            || opened_dialog_id.get_type() != DialogType::Channel
        {
            return promise.set_error(Status::error(400, "Invalid chat specified"));
        }
        env.save_app_log(
            dialog_id.get_channel_id().get(),
            opened_dialog_id.get_channel_id().get(),
            promise,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;

    struct TestEnv {
        members: HashSet<i64>,
        input_peer_channels: HashSet<i64>,
        broadcasts: HashSet<i64>,
        dialogs: HashSet<i64>,
        /// the `use_message_database` flag (the pmc legs).
        use_db: bool,
        /// the sqlite pmc stand-in (the collapsed sync read).
        pmc: RefCell<HashMap<Vec<u8>, Vec<u8>>>,
        reloaded: RefCell<Vec<i64>>,
        opened: RefCell<Vec<(i64, i64)>>,
    }

    impl TestEnv {
        fn new(
            members: HashSet<i64>,
            input_peer_channels: HashSet<i64>,
            broadcasts: HashSet<i64>,
            dialogs: HashSet<i64>,
        ) -> TestEnv {
            TestEnv {
                members,
                input_peer_channels,
                broadcasts,
                dialogs,
                use_db: false,
                pmc: RefCell::new(HashMap::new()),
                reloaded: RefCell::new(Vec::new()),
                opened: RefCell::new(Vec::new()),
            }
        }
    }

    impl ChannelRecommendationManagerEnv for TestEnv {
        fn close_flag(&self) -> bool {
            false
        }

        fn now(&self) -> f64 {
            0.0
        }

        fn use_message_database(&self) -> bool {
            self.use_db
        }

        fn get_option_boolean(&self, _name: &str, default_value: bool) -> bool {
            default_value
        }

        fn channel_is_member(&self, channel_id: ChannelId) -> bool {
            self.members.contains(&channel_id.get())
        }

        fn have_input_peer_channel(&mut self, channel_id: ChannelId) -> bool {
            self.input_peer_channels.contains(&channel_id.get())
        }

        fn is_broadcast_channel(&self, channel_id: ChannelId) -> bool {
            self.broadcasts.contains(&channel_id.get())
        }

        fn get_input_channel(&mut self, channel_id: ChannelId) -> bool {
            self.input_peer_channels.contains(&channel_id.get())
        }

        fn have_dialog_force(&self, dialog_id: DialogId) -> bool {
            self.dialogs.contains(&dialog_id.get())
        }

        fn force_create_dialog(&mut self, _dialog_id: DialogId) {}

        fn get_chats_object(
            &mut self,
            total_count: i32,
            dialog_ids: &[DialogId],
        ) -> crate::td_api::Chats {
            crate::td_api::Chats {
                total_count,
                chat_ids: dialog_ids.iter().map(|dialog_id| dialog_id.get()).collect(),
            }
        }

        fn sqlite_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.pmc.borrow().get(key).cloned().unwrap_or_default()
        }

        fn sqlite_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.pmc.borrow_mut().insert(key.to_vec(), value.to_vec());
        }

        fn sqlite_pmc_erase(&mut self, key: &[u8]) {
            self.pmc.borrow_mut().remove(key);
        }

        fn get_channel_ids(
            &mut self,
            chats: Vec<telegram_api::Chat>,
            _source: &str,
        ) -> Vec<ChannelId> {
            chats
                .iter()
                .map(|chat| match chat {
                    telegram_api::Chat::channel(channel) => ChannelId::new(channel.id),
                    telegram_api::Chat::channelForbidden(channel) => ChannelId::new(channel.id),
                    _ => ChannelId::new(0),
                })
                .filter(|channel_id| {
                    channel_id.is_valid()
                        && self
                            .dialogs
                            .contains(&DialogId::from_channel_id(*channel_id).get())
                })
                .collect()
        }

        fn on_get_channel_error(
            &mut self,
            _channel_id: ChannelId,
            _status: &Status,
            _source: &str,
        ) -> bool {
            false
        }

        fn get_input_channel_object(
            &mut self,
            channel_id: ChannelId,
        ) -> Option<tdmtproto::telegram_api::InputChannel> {
            self.input_peer_channels
                .contains(&channel_id.get())
                .then(|| {
                    tdmtproto::telegram_api::InputChannel::inputChannel(
                        tdmtproto::telegram_api::inputChannel {
                            channel_id: channel_id.get(),
                            access_hash: 0,
                        },
                    )
                })
        }

        fn reload_recommended_channels(&mut self, channel_id: ChannelId) {
            self.reloaded.borrow_mut().push(channel_id.get());
        }

        fn save_app_log(
            &mut self,
            ref_channel_id: i64,
            open_channel_id: i64,
            _promise: Promise<()>,
        ) {
            self.opened
                .borrow_mut()
                .push((ref_channel_id, open_channel_id));
        }
    }

    fn channel_dialog(id: i64) -> DialogId {
        DialogId::from_channel_id(ChannelId::new(id))
    }

    fn flag_promise<T: Send + Default + 'static>() -> (
        Promise<T>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        struct FlagPromise<T> {
            value_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
            error_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
            _phantom: std::marker::PhantomData<T>,
        }
        impl<T: Send + 'static> tdutils::promise::PromiseInterface<T> for FlagPromise<T> {
            fn set_value(&mut self, _value: T) {
                self.value_flag
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            fn set_error(&mut self, _error: Status) {
                self.error_flag
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let value_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let error_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        (
            Promise::from_interface(FlagPromise::<T> {
                value_flag: value_flag.clone(),
                error_flag: error_flag.clone(),
                _phantom: std::marker::PhantomData,
            }),
            value_flag,
            error_flag,
        )
    }

    #[test]
    fn record_round_trips() {
        let record = RecommendedDialogs {
            dialog_ids: vec![channel_dialog(5), channel_dialog(6)],
            total_count: 7,
            next_reload_time: 12345.5,
        };
        let mut out = Vec::new();
        record.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(RecommendedDialogs::parse(&mut parser).unwrap(), record);

        // the total_count flag is skipped when it equals the id count
        let record = RecommendedDialogs {
            dialog_ids: vec![channel_dialog(5)],
            total_count: 1,
            next_reload_time: 0.0,
        };
        let mut out = Vec::new();
        record.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(RecommendedDialogs::parse(&mut parser).unwrap(), record);
    }

    #[test]
    fn unknown_dialog_fails_both_promises() {
        let mut env = TestEnv::new(
            HashSet::new(),
            HashSet::new(),
            HashSet::new(),
            HashSet::new(),
        );
        let mut manager = ChannelRecommendationManager::new();
        let (chats_promise, _, chats_failed) = flag_promise::<crate::td_api::Chats>();
        let (count_promise, _, count_failed) = flag_promise::<i32>();
        manager.get_channel_recommendations(
            &mut env,
            channel_dialog(1),
            false,
            Some(chats_promise),
            Some(count_promise),
        );
        assert!(chats_failed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(count_failed.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn non_channel_answers_empty() {
        let mut env = TestEnv::new(
            HashSet::new(),
            HashSet::new(),
            HashSet::new(),
            HashSet::from([42]),
        );
        let mut manager = ChannelRecommendationManager::new();
        let user_dialog = DialogId::from_user_id(crate::user_manager::UserId::new(42));
        let (chats_promise, chats_fulfilled, _) = flag_promise::<crate::td_api::Chats>();
        manager.get_channel_recommendations(
            &mut env,
            user_dialog,
            false,
            Some(chats_promise),
            None,
        );
        assert!(chats_fulfilled.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn non_broadcast_channel_answers_empty() {
        let mut env = TestEnv::new(
            HashSet::new(),
            HashSet::from([3]),
            HashSet::new(),
            HashSet::from([channel_dialog(3).get()]),
        );
        let mut manager = ChannelRecommendationManager::new();
        let (chats_promise, chats_fulfilled, _) = flag_promise::<crate::td_api::Chats>();
        manager.get_channel_recommendations(
            &mut env,
            channel_dialog(3),
            false,
            Some(chats_promise),
            None,
        );
        assert!(chats_fulfilled.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn broadcast_channel_loads_and_parks() {
        let mut env = TestEnv::new(
            HashSet::new(),
            HashSet::from([4]),
            HashSet::from([4]),
            HashSet::from([channel_dialog(4).get()]),
        );
        let mut manager = ChannelRecommendationManager::new();
        let (chats_promise, chats_fulfilled, chats_failed) = flag_promise::<crate::td_api::Chats>();
        manager.get_channel_recommendations(
            &mut env,
            channel_dialog(4),
            false,
            Some(chats_promise),
            None,
        );
        // the net reload parks the promise unanswered offline
        assert!(!chats_fulfilled.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!chats_failed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(env.reloaded.borrow().as_slice(), &[4]);
    }

    #[test]
    fn open_requires_two_channel_dialogs() {
        // the encoded dialog ids (the have_dialog_force probe) plus the
        // user dialog, so the invalid-opened leg dies on the type check
        let mut env = TestEnv::new(
            HashSet::new(),
            HashSet::new(),
            HashSet::new(),
            HashSet::from([
                channel_dialog(5).get(),
                channel_dialog(6).get(),
                DialogId::from_user_id(crate::user_manager::UserId::new(6)).get(),
            ]),
        );
        let mut manager = ChannelRecommendationManager::new();
        let (promise, _, failed) = flag_promise::<()>();
        manager.open_channel_recommended_channel(
            &mut env,
            channel_dialog(5),
            channel_dialog(6),
            promise,
        );
        assert!(!failed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(env.opened.borrow().as_slice(), &[(5, 6)]);

        // a user dialog is invalid
        let (promise, _, failed) = flag_promise::<()>();
        manager.open_channel_recommended_channel(
            &mut env,
            channel_dialog(5),
            DialogId::from_user_id(crate::user_manager::UserId::new(6)),
            promise,
        );
        assert!(failed.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn suitability_needs_non_member_with_input_peer() {
        let mut env = TestEnv::new(
            HashSet::from([1]),
            HashSet::from([1, 2]),
            HashSet::new(),
            HashSet::new(),
        );
        let manager = ChannelRecommendationManager::new();
        assert!(!manager.is_suitable_recommended_channel(&mut env, ChannelId::new(1)));
        assert!(manager.is_suitable_recommended_channel(&mut env, ChannelId::new(2)));
        assert!(!manager.is_suitable_recommended_channel(&mut env, ChannelId::new(3)));
    }

    /// One wire channel answer form (the `get_channel_ids` stand-in
    /// accepts the two channel forms).
    fn wire_channel(id: i64) -> telegram_api::Chat {
        telegram_api::Chat::channel(telegram_api::channel {
            broadcast: true,
            id,
            access_hash: 777,
            title: "rec".to_owned(),
            ..telegram_api::channel::default()
        })
    }

    /// A value-capturing promise (the band tests read the settled
    /// value, not just the fired flag) — the error folds to its message
    /// so the bands compare plainly.
    type Captured<T> = std::sync::Arc<std::sync::Mutex<Option<Result<T, String>>>>;
    fn capturing_promise<T: Send + 'static>() -> (Promise<T>, Captured<T>) {
        struct CapturingPromise<T> {
            results: Captured<T>,
        }
        impl<T: Send + 'static> tdutils::promise::PromiseInterface<T> for CapturingPromise<T> {
            fn set_value(&mut self, value: T) {
                *self.results.lock().unwrap() = Some(Ok(value));
            }
            fn set_error(&mut self, error: Status) {
                *self.results.lock().unwrap() = Some(Err(error.message().to_owned()));
            }
        }
        let results: Captured<T> = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(CapturingPromise {
                results: std::sync::Arc::clone(&results),
            }),
            results,
        )
    }

    #[test]
    fn recommended_channels_answer_walk_drops_unsuitable() {
        // channel 5 is a suitable recommendation; channel 6 is a member
        // (the C++ is_suitable band drops it, cpp:133-143/259-271)
        let mut env = TestEnv::new(
            HashSet::from([6]),
            HashSet::from([5, 6]),
            HashSet::new(),
            HashSet::from([channel_dialog(5).get(), channel_dialog(6).get()]),
        );
        let mut manager = ChannelRecommendationManager::new();
        let (chats_promise, chats_result) = capturing_promise::<crate::td_api::Chats>();
        manager.load_recommended_channels(&mut env, false, chats_promise);
        // the reload armed at the head (cpp:187-201)
        assert_eq!(env.reloaded.borrow().as_slice(), &[0]);

        // the wire answer: two chats, one member — the count decrements
        // (cpp:250-287)
        manager
            .on_get_recommended_channels(&mut env, Ok((2, vec![wire_channel(5), wire_channel(6)])));
        let settled = chats_result
            .lock()
            .unwrap()
            .take()
            .expect("the promise settles");
        let Ok(chats) = settled else {
            panic!("the walk settles ok");
        };
        assert_eq!(chats.total_count, 1, "the member drops the count");
        assert_eq!(chats.chat_ids, vec![channel_dialog(5).get()]);
        assert_eq!(
            manager.recommended_channels.dialog_ids,
            vec![channel_dialog(5)]
        );
        assert!(manager.are_recommended_channels_inited);

        // the error band fails the parked lot (cpp:203-206)
        let (chats_promise, chats_result) = capturing_promise::<crate::td_api::Chats>();
        manager.load_recommended_channels(&mut env, false, chats_promise);
        manager.on_get_recommended_channels(&mut env, Err(Status::error(400, "CHANNEL_PRIVATE")));
        let settled = chats_result
            .lock()
            .unwrap()
            .take()
            .expect("the promise settles");
        assert_eq!(settled, Err("CHANNEL_PRIVATE".to_owned()));
    }

    #[test]
    fn return_local_count_settles_minus_one_on_the_reload_head() {
        // the -1 band (cpp:476-487): the return_local count queries
        // settle before the wire query leaves
        let mut env = TestEnv::new(
            HashSet::new(),
            HashSet::from([4]),
            HashSet::from([4]),
            HashSet::from([channel_dialog(4).get()]),
        );
        let mut manager = ChannelRecommendationManager::new();
        let (count_promise, count_result) = capturing_promise::<i32>();
        manager.get_channel_recommendations(
            &mut env,
            channel_dialog(4),
            true,
            None,
            Some(count_promise),
        );
        assert_eq!(
            count_result.lock().unwrap().take(),
            Some(Ok(-1)),
            "the return_local -1 band"
        );
        // the wire reload armed behind the settle
        assert_eq!(env.reloaded.borrow().as_slice(), &[4]);
        // and the null chats promise parks silently (cpp:380)
        assert_eq!(
            manager
                .get_channel_recommendations_queries
                .get(&ChannelId::new(4))
                .map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn database_record_settles_and_refires_when_expired() {
        // a suitable stored record answers from the pmc; the expired
        // deadline (now = 0.0) refires the reload (cpp:220-248)
        let mut env = TestEnv::new(
            HashSet::new(),
            HashSet::from([5]),
            HashSet::new(),
            HashSet::from([channel_dialog(5).get()]),
        );
        env.use_db = true;
        let record = RecommendedDialogs {
            dialog_ids: vec![channel_dialog(5)],
            total_count: 3,
            next_reload_time: 0.0,
        };
        let mut data = Vec::new();
        record.store(&mut data);
        env.pmc.borrow_mut().insert(
            ChannelRecommendationManager::get_recommended_channels_database_key(),
            data,
        );

        let mut manager = ChannelRecommendationManager::new();
        let (chats_promise, chats_result) = capturing_promise::<crate::td_api::Chats>();
        manager.load_recommended_channels(&mut env, true, chats_promise);
        // the record settled the promise with its stored total_count
        let settled = chats_result
            .lock()
            .unwrap()
            .take()
            .expect("the promise settles");
        assert_eq!(settled.map(|chats| chats.total_count), Ok(3));
        // ... and the expired deadline armed the reload behind it
        assert_eq!(env.reloaded.borrow().as_slice(), &[0]);

        // a corrupted record erases itself and refires the reload
        // (a fresh manager — the lot above still parks the first reload)
        env.pmc.borrow_mut().insert(
            ChannelRecommendationManager::get_recommended_channels_database_key(),
            b"garbage".to_vec(),
        );
        let mut manager = ChannelRecommendationManager::new();
        let (chats_promise, chats_result) = capturing_promise::<crate::td_api::Chats>();
        manager.load_recommended_channels(&mut env, true, chats_promise);
        assert!(
            chats_result.lock().unwrap().is_none(),
            "the reload parks instead"
        );
        assert_eq!(env.reloaded.borrow().as_slice(), &[0, 0]);
        assert!(
            env.pmc
                .borrow()
                .get(&ChannelRecommendationManager::get_recommended_channels_database_key())
                .is_none()
        );
    }
}
