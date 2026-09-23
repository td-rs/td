//!

use crate::chat_manager::ChannelId;
use crate::dialog_id::{DialogId, DialogType};
use crate::store_parse::{Parser, store_flags, store_i32};
use std::collections::HashMap;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `ChannelRecommendationManager::CHANNEL_RECOMMENDATIONS_CACHE_TIME`
/// (ChannelRecommendationManager.h:76) — stamps the reload deadline that
/// the `reload_recommended_channels` net tail consumes.
#[allow(dead_code)] // the [blocked: net] reload tail is the only writer of the deadline it feeds
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

    // -- the [blocked: net] query tails; the loaded-promise vectors
    // -- stay parked in the manager (the answers would come from the
    // -- query callbacks, which never fire offline)
    /// `GetChannelRecommendationsQuery` (ChannelRecommendationManager.cpp:
    /// 31-84) — `reload_recommended_channels` with an empty channel id.
    fn reload_recommended_channels(&mut self, channel_id: ChannelId);
    /// `save_app_log` (Application.cpp) — the open-channel app log;
    /// [blocked: net], the promise parks.
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
    #[allow(dead_code)] // filled by the [blocked: net] reload tail; read by the same intake
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

    #[allow(dead_code)] // the getChatSimilarChats db-load half — lands with the net-backed reload
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
        !(!have_all && is_premium)
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
        _use_database: bool,
        promise: Promise<crate::td_api::Chats>,
    ) {
        self.get_recommended_channels_queries.push(promise);
        if self.get_recommended_channels_queries.len() == 1 {
            // The database load + the reload both end in the [blocked:
            // net] query seam; the promises park in the query vector.
            env.reload_recommended_channels(ChannelId::new(0));
        }
    }

    /// `get_channel_recommendations` (ChannelRecommendationManager.cpp:298-359).
    pub fn get_channel_recommendations(
        &mut self,
        env: &mut impl ChannelRecommendationManagerEnv,
        dialog_id: DialogId,
        return_local: bool,
        chats_promise: Option<Promise<crate::td_api::Chats>>,
        count_promise: Option<Promise<i32>>,
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
        self.load_channel_recommendations(
            env,
            channel_id,
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
        if let Some(chats_promise) = chats_promise {
            queries.push(chats_promise);
        }
        if queries.len() == 1 {
            // The [blocked: net] reload; the count queries with
            // return_local answer -1 exactly like the C++ reload.
            if return_local {
                if let Some(promises) = self
                    .get_channel_recommendation_count_queries
                    .remove(&(true, channel_id))
                {
                    for mut promise in promises {
                        promise.set_value(-1);
                    }
                }
            }
            env.reload_recommended_channels(channel_id);
        }
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
        reloaded: RefCell<Vec<i64>>,
        opened: RefCell<Vec<(i64, i64)>>,
    }

    impl ChannelRecommendationManagerEnv for TestEnv {
        fn close_flag(&self) -> bool {
            false
        }

        fn now(&self) -> f64 {
            0.0
        }

        fn use_message_database(&self) -> bool {
            false
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

        fn sqlite_pmc_get_raw(&mut self, _key: &[u8]) -> Vec<u8> {
            Vec::new()
        }

        fn sqlite_pmc_set_raw(&mut self, _key: &[u8], _value: &[u8]) {}

        fn sqlite_pmc_erase(&mut self, _key: &[u8]) {}

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
        let mut env = TestEnv {
            members: HashSet::new(),
            input_peer_channels: HashSet::new(),
            broadcasts: HashSet::new(),
            dialogs: HashSet::new(),
            reloaded: RefCell::new(Vec::new()),
            opened: RefCell::new(Vec::new()),
        };
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
        let mut env = TestEnv {
            members: HashSet::new(),
            input_peer_channels: HashSet::new(),
            broadcasts: HashSet::new(),
            dialogs: HashSet::from([42]),
            reloaded: RefCell::new(Vec::new()),
            opened: RefCell::new(Vec::new()),
        };
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
        let mut env = TestEnv {
            members: HashSet::new(),
            input_peer_channels: HashSet::from([3]),
            broadcasts: HashSet::new(),
            dialogs: HashSet::from([channel_dialog(3).get()]),
            reloaded: RefCell::new(Vec::new()),
            opened: RefCell::new(Vec::new()),
        };
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
        let mut env = TestEnv {
            members: HashSet::new(),
            input_peer_channels: HashSet::from([4]),
            broadcasts: HashSet::from([4]),
            dialogs: HashSet::from([channel_dialog(4).get()]),
            reloaded: RefCell::new(Vec::new()),
            opened: RefCell::new(Vec::new()),
        };
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
        let mut env = TestEnv {
            members: HashSet::new(),
            input_peer_channels: HashSet::new(),
            broadcasts: HashSet::new(),
            // the encoded dialog ids (the have_dialog_force probe) plus the
            // user dialog, so the invalid-opened leg dies on the type check
            dialogs: HashSet::from([
                channel_dialog(5).get(),
                channel_dialog(6).get(),
                DialogId::from_user_id(crate::user_manager::UserId::new(6)).get(),
            ]),
            reloaded: RefCell::new(Vec::new()),
            opened: RefCell::new(Vec::new()),
        };
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
        let mut env = TestEnv {
            members: HashSet::from([1]),
            input_peer_channels: HashSet::from([1, 2]),
            broadcasts: HashSet::new(),
            dialogs: HashSet::new(),
            reloaded: RefCell::new(Vec::new()),
            opened: RefCell::new(Vec::new()),
        };
        let manager = ChannelRecommendationManager::new();
        assert!(!manager.is_suitable_recommended_channel(&mut env, ChannelId::new(1)));
        assert!(manager.is_suitable_recommended_channel(&mut env, ChannelId::new(2)));
        assert!(!manager.is_suitable_recommended_channel(&mut env, ChannelId::new(3)));
    }
}
