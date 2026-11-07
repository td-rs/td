//! `SuggestedActionManager` (td/telegram/SuggestedActionManager.{h,cpp}) —
//! the M.33 Wave-3 suggested-action manager. The login-email promo
//! family (CheckPromoDataQuery) rides the account dispatch plane
//! (SuggestedActionManager.cpp:65-105): the send arm dispatches
//! `help.getPromoData` and the `Td::on_account_net_answer` drain settles
//! the parked request promise (the SETUP_LOGIN_EMAIL scan, the 404 "Not
//! Found" band). The DismissSuggestionQuery send stays a
//! `[blocked: net]` seam (the net-drain dismiss family).

use crate::dialog_id::DialogId;
use crate::store_parse::{self, Parser};
use crate::suggested_action::{
    SuggestedAction, get_update_suggested_actions_object, remove_suggested_action,
    update_suggested_actions,
};
use crate::td_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The `G()`/`td_` surface of the manager.
pub trait SuggestedActionManagerEnv {
    fn close_flag(&self) -> bool;

    fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;
    fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]);
    fn pmc_erase(&mut self, key: &str);

    fn get_option_string(&self, name: &str, default_value: &str) -> String;

    fn send_update(&mut self, update: td_api::Update);

    /// `DismissSuggestionQuery::send` — `[blocked: net]`.
    fn send_dismiss_suggestion_query(&mut self, action: &SuggestedAction) {
        tdutils::log_info!("Send help.dismissSuggestion for {action:?} — [blocked: net]");
    }

    /// `CheckPromoDataQuery::send` (SuggestedActionManager.cpp:72-74) —
    /// the wire `help.getPromoData`; the answer handler is the
    /// `Td::on_account_net_answer` `CheckPromoData` arm (cpp:76-104).
    fn send_get_promo_data_query(&mut self);
}

/// The collapsed `SuggestedActionManager`.
#[derive(Default)]
pub struct SuggestedActionManager {
    suggested_actions: Vec<SuggestedAction>,
    dialog_suggested_actions: std::collections::BTreeMap<i64, Vec<SuggestedAction>>,
    /// The `()`-typed promises parked at their `[blocked: net]` dismiss
    /// sends, keyed by the action.
    pub(crate) dismiss_suggested_action_queries:
        std::collections::BTreeMap<(i64, i32), Vec<Promise<()>>>,
    /// The login-email check promises parked at the promo-data send — the
    /// C++ carries the promise into the CheckPromoDataQuery handler
    /// (cpp:276-278); the Rust drain pops this FIFO lot.
    pub(crate) parked_login_email_promises: Vec<Promise<()>>,
}

impl SuggestedActionManager {
    pub fn new() -> SuggestedActionManager {
        SuggestedActionManager::default()
    }

    /// `start_up` (SuggestedActionManager.cpp) — the pmc load.
    pub fn start_up(&mut self, env: &mut impl SuggestedActionManagerEnv) {
        let log_event_string = env.pmc_get_raw(b"suggested_actions");
        if log_event_string.is_empty() {
            return;
        }
        let parse = || -> Result<Vec<SuggestedAction>, &'static str> {
            let mut parser = Parser::new_log_event(&log_event_string)?;
            let count = parser.i32_()?;
            let mut actions = Vec::with_capacity(count as usize);
            for _ in 0..count {
                actions.push(SuggestedAction::parse(&mut parser)?);
            }
            Ok(actions)
        };
        match parse() {
            Ok(actions) => self.suggested_actions = actions,
            Err(error) => {
                tdutils::log_error!("Failed to parse suggested actions: {error}");
                self.suggested_actions.clear();
            }
        }
    }

    /// `save_suggested_actions` (SuggestedActionManager.cpp).
    fn save_suggested_actions(&mut self, env: &mut impl SuggestedActionManagerEnv) {
        if self.suggested_actions.is_empty() {
            env.pmc_erase("suggested_actions");
            return;
        }
        let mut data = Vec::new();
        store_parse::store_log_event_version(&mut data);
        store_parse::store_i32(&mut data, self.suggested_actions.len() as i32);
        for action in &self.suggested_actions {
            action.store(&mut data);
        }
        env.pmc_set_raw(b"suggested_actions", &data);
    }

    /// `update_suggested_actions` (SuggestedActionManager.cpp) — the value
    /// helper + the persistent-filter + the update emission.
    pub fn update_suggested_actions(
        &mut self,
        env: &mut impl SuggestedActionManagerEnv,
        new_suggested_actions: Vec<SuggestedAction>,
    ) {
        let persistent_actions: Vec<SuggestedAction> = new_suggested_actions
            .into_iter()
            .filter(SuggestedAction::is_persistent)
            .collect();
        if update_suggested_actions(&mut self.suggested_actions, persistent_actions) {
            self.save_suggested_actions(env);
            env.send_update(get_update_suggested_actions_object(
                None,
                &self.suggested_actions,
                &[],
                "update_suggested_actions",
                &env.get_option_string(
                    "premium_manage_subscription_url",
                    "https://t.me/premiumbot?start=status",
                ),
            ));
        }
    }

    /// `hide_suggested_action` (SuggestedActionManager.cpp).
    pub fn hide_suggested_action(
        &mut self,
        env: &mut impl SuggestedActionManagerEnv,
        suggested_action: SuggestedAction,
    ) {
        let mut removed_actions = Vec::new();
        if remove_suggested_action(&mut self.suggested_actions, suggested_action.clone()) {
            removed_actions.push(suggested_action);
            self.save_suggested_actions(env);
        }
        if !removed_actions.is_empty() {
            env.send_update(get_update_suggested_actions_object(
                None,
                &[],
                &removed_actions,
                "hide_suggested_action",
                &env.get_option_string(
                    "premium_manage_subscription_url",
                    "https://t.me/premiumbot?start=status",
                ),
            ));
        }
    }

    /// `dismiss_suggested_action` (SuggestedActionManager.cpp) — the query
    /// is the seam; the answer handler is
    /// [`SuggestedActionManager::on_dismiss_suggested_action`]. The
    /// `dismissSuggestedAction` td_api arm does not exist in this
    /// revision; the flow serves the internal callers once they land.
    pub fn dismiss_suggested_action(
        &mut self,
        env: &mut impl SuggestedActionManagerEnv,
        suggested_action: SuggestedAction,
        promise: Promise<()>,
    ) {
        let key = (
            suggested_action.dialog_id.get(),
            suggested_action.type_ as i32,
        );
        self.dismiss_suggested_action_queries
            .entry(key)
            .or_default()
            .push(promise);
        env.send_dismiss_suggestion_query(&suggested_action);
    }

    /// `on_dismiss_suggested_action` (SuggestedActionManager.cpp) — the
    /// dismiss answer handler.
    pub fn on_dismiss_suggested_action(
        &mut self,
        suggested_action: SuggestedAction,
        result: std::result::Result<(), Status>,
    ) {
        let key = (
            suggested_action.dialog_id.get(),
            suggested_action.type_ as i32,
        );
        let mut promises = self.dismiss_suggested_action_queries.remove(&key);
        if let Some(promises) = &mut promises {
            for promise in promises.drain(..) {
                let mut promise = promise;
                match &result {
                    Ok(()) => promise.set_value(()),
                    Err(status) => promise.set_error(status.clone()),
                }
            }
        }
    }

    /// `remove_dialog_suggested_action` (SuggestedActionManager.cpp).
    pub fn remove_dialog_suggested_action(&mut self, action: SuggestedAction) {
        if let Some(actions) = self
            .dialog_suggested_actions
            .get_mut(&action.dialog_id.get())
        {
            actions.retain(|current| *current != action);
        }
    }

    /// `set_dialog_pending_suggestions` (SuggestedActionManager.cpp).
    pub fn set_dialog_pending_suggestions(
        &mut self,
        env: &mut impl SuggestedActionManagerEnv,
        dialog_id: DialogId,
        pending_suggestions: Vec<String>,
    ) {
        let mut actions = Vec::with_capacity(pending_suggestions.len());
        for suggestion in pending_suggestions {
            let action = SuggestedAction::from_pending_suggestion(
                None,
                &tdmtproto::telegram_api::PendingSuggestion {
                    suggestion,
                    title: None,
                    description: None,
                    url: "https://t.me".to_string(),
                },
            );
            if !action.is_empty() {
                actions.push(action);
            }
        }
        self.dialog_suggested_actions
            .insert(dialog_id.get(), actions);
        let _ = env;
    }

    /// `is_login_email_address_required` (SuggestedActionManager.cpp:276-278)
    /// — the CheckPromoDataQuery send with the request promise.
    pub fn is_login_email_address_required(
        &mut self,
        env: &mut impl SuggestedActionManagerEnv,
        promise: Promise<()>,
    ) {
        env.send_get_promo_data_query();
        self.parked_login_email_promises.push(promise);
    }

    /// Pop the head of the login-email lot (the FIFO `take_promise`
    /// convention; the C++ carries the promise through the
    /// CheckPromoDataQuery handler — SuggestedActionManager.cpp:276-278 —
    /// the §5 pairing note).
    pub(crate) fn take_login_email_promise(&mut self) -> Option<Promise<()>> {
        if self.parked_login_email_promises.is_empty() {
            None
        } else {
            Some(self.parked_login_email_promises.remove(0))
        }
    }

    /// `get_current_state` (Requests.cpp:2237 area).
    pub fn get_current_state(
        &self,
        env: &impl SuggestedActionManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if self.suggested_actions.is_empty() {
            return;
        }
        updates.push(get_update_suggested_actions_object(
            None,
            &self.suggested_actions,
            &[],
            "get_current_state",
            &env.get_option_string(
                "premium_manage_subscription_url",
                "https://t.me/premiumbot?start=status",
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct TestEnv {
        pmc: BTreeMap<Vec<u8>, Vec<u8>>,
        erased: Vec<String>,
        updates: Vec<td_api::Update>,
        queries: Vec<String>,
    }

    impl TestEnv {
        fn new() -> TestEnv {
            TestEnv {
                pmc: BTreeMap::new(),
                erased: Vec::new(),
                updates: Vec::new(),
                queries: Vec::new(),
            }
        }
    }

    impl SuggestedActionManagerEnv for TestEnv {
        fn close_flag(&self) -> bool {
            false
        }

        fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.pmc.get(key).cloned().unwrap_or_default()
        }

        fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.pmc.insert(key.to_vec(), value.to_vec());
        }

        fn pmc_erase(&mut self, key: &str) {
            self.erased.push(key.to_string());
        }

        fn get_option_string(&self, _name: &str, default_value: &str) -> String {
            default_value.to_string()
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn send_dismiss_suggestion_query(&mut self, _action: &SuggestedAction) {
            self.queries.push("dismissSuggestion".to_string());
        }

        fn send_get_promo_data_query(&mut self) {
            self.queries.push("getPromoData".to_string());
        }
    }

    #[test]
    fn login_email_park_and_pop() {
        // The request promise parks at the send; the answer drain pops
        // the head FIFO (the C++ carries the promise through the query
        // handler — cpp:276-278, the §5 pairing note).
        let mut env = TestEnv::new();
        let mut manager = SuggestedActionManager::new();
        let (promise, _slot) = record();
        manager.is_login_email_address_required(&mut env, promise);
        assert_eq!(env.queries, ["getPromoData"]);
        assert!(manager.take_login_email_promise().is_some());
        assert!(manager.take_login_email_promise().is_none());
    }

    #[test]
    fn update_filters_to_persistent_and_persists() {
        let mut env = TestEnv {
            pmc: BTreeMap::new(),
            erased: Vec::new(),
            updates: Vec::new(),
            queries: Vec::new(),
        };
        let mut manager = SuggestedActionManager::new();
        manager.update_suggested_actions(
            &mut env,
            vec![
                SuggestedAction::from_action_str("PREMIUM_UPGRADE"),
                SuggestedAction::from_action_str("SETUP_LOGIN_EMAIL"),
            ],
        );
        assert_eq!(
            manager.suggested_actions.len(),
            1,
            "only persistent actions persist"
        );
        assert_eq!(env.updates.len(), 1);
        assert!(env.pmc.contains_key(b"suggested_actions".as_slice()));

        // a fresh manager loads the same list
        let mut fresh = SuggestedActionManager::new();
        let mut env2 = TestEnv {
            pmc: env.pmc.clone(),
            erased: Vec::new(),
            updates: Vec::new(),
            queries: Vec::new(),
        };
        fresh.start_up(&mut env2);
        assert_eq!(fresh.suggested_actions.len(), 1);
    }

    #[test]
    fn hide_removes_and_emits() {
        let mut env = TestEnv {
            pmc: BTreeMap::new(),
            erased: Vec::new(),
            updates: Vec::new(),
            queries: Vec::new(),
        };
        let mut manager = SuggestedActionManager::new();
        let action = SuggestedAction::from_action_str("PREMIUM_UPGRADE");
        manager.update_suggested_actions(&mut env, vec![action.clone()]);
        manager.hide_suggested_action(&mut env, action);
        assert!(manager.suggested_actions.is_empty());
        assert_eq!(env.updates.len(), 2);
        assert_eq!(env.erased, vec!["suggested_actions".to_string()]);
    }

    #[test]
    fn dismiss_parks_and_the_answer_drains() {
        let mut env = TestEnv {
            pmc: BTreeMap::new(),
            erased: Vec::new(),
            updates: Vec::new(),
            queries: Vec::new(),
        };
        let mut manager = SuggestedActionManager::new();
        let action = SuggestedAction::from_action_str("PREMIUM_UPGRADE");
        let (promise, recorded) = record();
        manager.dismiss_suggested_action(&mut env, action.clone(), promise);
        assert_eq!(env.queries.len(), 1);
        assert_eq!(
            manager
                .dismiss_suggested_action_queries
                .get(&(0, SuggestedActionType::UpgradePremium as i32))
                .map(Vec::len),
            Some(1)
        );
        manager.on_dismiss_suggested_action(action, Ok(()));
        assert!(recorded.0.lock().unwrap().is_some());
        assert!(manager.dismiss_suggested_action_queries.is_empty());
    }

    struct Recorded(std::sync::Arc<std::sync::Mutex<Option<std::result::Result<(), Status>>>>);

    struct Recording(std::sync::Arc<std::sync::Mutex<Option<std::result::Result<(), Status>>>>);

    impl tdutils::promise::PromiseInterface<()> for Recording {
        fn set_value(&mut self, _value: ()) {
            *self.0.lock().unwrap() = Some(Ok(()));
        }

        fn set_error(&mut self, error: Status) {
            *self.0.lock().unwrap() = Some(Err(error));
        }
    }

    fn record() -> (Promise<()>, Recorded) {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(Recording(recorded.clone())),
            Recorded(recorded),
        )
    }

    use crate::suggested_action::SuggestedActionType;
}
