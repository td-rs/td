//! `SuggestedActionManager` (td/telegram/SuggestedActionManager.{h,cpp}) —
//! the M.33 Wave-3 suggested-action manager. The net-drain 52 slice lands
//! the query plane: DismissSuggestionQuery/CheckPromoDataQuery
//! (SuggestedActionManager.cpp:26-105) ride the account dispatch surface
//! (the shared `AccountEnv` net handle — the sibling account families'
//! plane) and the answers drain through `Td::on_account_net_answer` into
//! the intakes here. The `hideSuggestedAction` td_api arm routes through
//! the free [`dismiss_suggested_action`] ladder (Requests.cpp:8144-8148 →
//! SuggestedAction.cpp:312-355). The C++ actor `hangup` (cpp:125-134)
//! stays collapsed: the Rust close path drops the parked promises without
//! the request-aborted errors (the collapsed-actor convention, the
//! terms-of-service precedent).

use crate::dialog_id::DialogId;
use crate::store_parse::{self, Parser};
use crate::suggested_action::{
    SuggestedAction, SuggestedActionType, get_update_suggested_actions_object,
    remove_suggested_action, update_suggested_actions,
};
use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The `G()`/`td_` surface of the manager.
pub trait SuggestedActionManagerEnv {
    fn close_flag(&self) -> bool;

    fn pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;
    fn pmc_set_raw(&mut self, key: &[u8], value: &[u8]);
    fn pmc_erase(&mut self, key: &str);

    fn get_option_string(&self, name: &str, default_value: &str) -> String;
    /// `G()->get_option_integer` — the free dismiss ladder's SetPassword
    /// arm (SuggestedAction.cpp:332).
    fn get_option_integer(&self, name: &str, default_value: i64) -> i64;
    /// `G()->set_option_empty` — the free dismiss ladder's SetPassword
    /// arm (SuggestedAction.cpp:342).
    fn set_option_empty(&mut self, name: &str);

    fn send_update(&mut self, update: td_api::Update);

    /// `td_->dialog_manager_->check_dialog_access(dialog_id, false, Read,
    /// "dismiss_suggested_action")` (cpp:163-165) — the dismiss
    /// dialog-branch gate.
    fn check_dialog_access(&mut self, dialog_id: DialogId) -> std::result::Result<(), Status>;

    /// `td_->dialog_manager_->get_input_peer(dialog_id, Read)`
    /// (cpp:37-39) — the dismiss send-leg peer read.
    fn get_input_peer(&mut self, dialog_id: DialogId) -> Option<telegram_api::InputPeer>;

    /// `DismissSuggestionQuery::send` (cpp:34-46) — the wire
    /// `help.dismissSuggestion(input_peer, action_str)`; the peer branch
    /// is the handler's own (the valid dialog resolves its read peer, the
    /// empty dialog sends `inputPeerEmpty`).
    fn send_dismiss_suggestion_query(&mut self, action: &SuggestedAction);

    /// `CheckPromoDataQuery::send` (cpp:72-74) — the wire
    /// `help.getPromoData`; the answer handler is the
    /// `Td::on_account_net_answer` `CheckPromoData` arm (cpp:76-104).
    fn send_get_promo_data_query(&mut self);
}

/// One `dismiss_suggested_action_queries_` entry — the C++
/// `FlatHashMap<SuggestedAction, vector<Promise<Unit>>>` lot. The Rust key
/// collapses to `(dialog_id, type)` and the full action rides the lot for
/// the answer tails (the kind is `Copy` and cannot carry the custom
/// type/url; two concurrent same-key custom dismisses share a lot — the
/// campaign §5 single-flight note).
pub(crate) struct DismissSuggestionLot {
    /// The stored action the entry-level match picked (cpp:173-178) — the
    /// answer tails remove by its full identity.
    pub(crate) action: SuggestedAction,
    /// The parked caller promises; the answer settles the whole lot
    /// (cpp:202-204, 219).
    pub(crate) promises: Vec<Promise<()>>,
}

/// The collapsed `SuggestedActionManager`.
#[derive(Default)]
pub struct SuggestedActionManager {
    suggested_actions: Vec<SuggestedAction>,
    dialog_suggested_actions: std::collections::BTreeMap<i64, Vec<SuggestedAction>>,
    /// The parked dismiss lots keyed by `(dialog_id, type)` — the C++
    /// `dismiss_suggested_action_queries_` (the entry sends only the
    /// head, cpp:187).
    pub(crate) dismiss_suggested_action_queries:
        std::collections::BTreeMap<(i64, i32), DismissSuggestionLot>,
    /// The login-email check promises parked at the promo-data send — the
    /// C++ carries the promise into the CheckPromoDataQuery handler
    /// (cpp:276-278); the Rust drain pops this FIFO lot (the §5 pairing
    /// note, the terms-of-service accept precedent).
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

    /// `update_suggested_actions` (SuggestedActionManager.cpp) — the
    /// value helper + the persistent-filter + the update emission. The
    /// while-dismissing gate mirrors cpp:141-144 (the in-flight dismiss
    /// owns the list).
    pub fn update_suggested_actions(
        &mut self,
        env: &mut impl SuggestedActionManagerEnv,
        new_suggested_actions: Vec<SuggestedAction>,
    ) {
        if !self.dismiss_suggested_action_queries.is_empty() {
            // do not update suggested actions while dismissing an action
            return;
        }
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

    /// `dismiss_suggested_action` (SuggestedActionManager.cpp:156-193) —
    /// the dismiss flow: the empty/dialog/present pre-checks, the
    /// optimistic removal + save, the promise park keyed by the action
    /// and the head-only wire send.
    pub fn dismiss_suggested_action(
        &mut self,
        env: &mut impl SuggestedActionManagerEnv,
        mut suggested_action: SuggestedAction,
        mut promise: Promise<()>,
    ) {
        // cpp:157-160 — the empty action string (the Empty type and any
        // unmapped form) settles ok without a query.
        if suggested_action.get_suggested_action_str().is_empty() {
            return promise.set_value(());
        }

        if suggested_action.dialog_id != DialogId::default() {
            // cpp:162-171 — the dialog branch: the access gate, the
            // pending-suggestion membership check, the removal.
            if let Err(status) = env.check_dialog_access(suggested_action.dialog_id) {
                return promise.set_error(status);
            }
            let present = self
                .dialog_suggested_actions
                .get(&suggested_action.dialog_id.get())
                .is_some_and(|actions| actions.contains(&suggested_action));
            if !present {
                return promise.set_value(());
            }
            self.remove_dialog_suggested_action(env, suggested_action.clone());
        } else {
            // cpp:173-182 — pick up the stored copy (the relogin days /
            // custom fields), remove + save; an absent action settles ok.
            let mut action = suggested_action;
            for stored in &self.suggested_actions {
                if *stored == action {
                    action = stored.clone();
                    break;
                }
            }
            if !remove_suggested_action(&mut self.suggested_actions, action.clone()) {
                return promise.set_value(());
            }
            self.save_suggested_actions(env);
            env.send_update(get_update_suggested_actions_object(
                None,
                &[],
                &[action.clone()],
                "remove_suggested_action",
                &env.get_option_string(
                    "premium_manage_subscription_url",
                    "https://t.me/premiumbot?start=status",
                ),
            ));
            suggested_action = action;
        }

        // cpp:185-192 — the park + the head-only send.
        let key = (
            suggested_action.dialog_id.get(),
            suggested_action.type_ as i32,
        );
        let lot = self
            .dismiss_suggested_action_queries
            .entry(key)
            .or_insert_with(|| DismissSuggestionLot {
                action: suggested_action.clone(),
                promises: Vec::new(),
            });
        lot.promises.push(promise);
        if lot.promises.len() == 1 {
            env.send_dismiss_suggestion_query(&lot.action);
        }
    }

    /// `on_dismiss_suggested_action` (SuggestedActionManager.cpp:195-220)
    /// — the dismiss answer intake over the `(dialog_id, type)` lot key.
    /// Returns whether the no-dialog ok branch ran — the caller (the
    /// `Td::on_account_net_answer` drain) then runs the
    /// `ConfigManager::reload_app_config` tail (cpp:216), which needs the
    /// td-level config surface.
    pub fn on_dismiss_suggested_action(
        &mut self,
        env: &mut impl SuggestedActionManagerEnv,
        key: (i64, i32),
        result: std::result::Result<(), Status>,
    ) -> bool {
        if env.close_flag() {
            return false;
        }

        // cpp:200-204 — the CHECK band: a live query's lot always exists.
        let Some(lot) = self.dismiss_suggested_action_queries.remove(&key) else {
            tdutils::log_error!("on_dismiss_suggested_action has no parked lot {key:?}");
            return false;
        };

        let mut promises = lot.promises;
        let suggested_action = lot.action;
        let status = match result {
            Err(status) => Some(status),
            Ok(()) => {
                // cpp:210-217 — the removal tails.
                if suggested_action.dialog_id != DialogId::default() {
                    self.remove_dialog_suggested_action(env, suggested_action.clone());
                } else {
                    if remove_suggested_action(
                        &mut self.suggested_actions,
                        suggested_action.clone(),
                    ) {
                        self.save_suggested_actions(env);
                    }
                    // cpp:216 — reload_app_config; the drain runs it (the
                    // return flag).
                    for mut promise in promises.drain(..) {
                        promise.set_value(());
                    }
                    return true;
                }
                None
            }
        };
        if let Some(status) = status {
            // cpp:206-208 — fail_promises.
            for mut promise in promises.drain(..) {
                promise.set_error(status.clone());
            }
        } else {
            for mut promise in promises.drain(..) {
                promise.set_value(());
            }
        }
        false
    }

    /// `remove_dialog_suggested_action` (SuggestedActionManager.cpp:265-274)
    /// — the free `remove_suggested_action` emits the removed update on
    /// the successful removal (SuggestedAction.cpp:302-309) and the empty
    /// map entry erases.
    pub fn remove_dialog_suggested_action(
        &mut self,
        env: &mut impl SuggestedActionManagerEnv,
        action: SuggestedAction,
    ) {
        let dialog_key = action.dialog_id.get();
        let Some(actions) = self.dialog_suggested_actions.get_mut(&dialog_key) else {
            return;
        };
        let removed = remove_suggested_action(actions, action.clone());
        let now_empty = actions.is_empty();
        if removed {
            env.send_update(get_update_suggested_actions_object(
                None,
                &[],
                &[action],
                "remove_suggested_action",
                &env.get_option_string(
                    "premium_manage_subscription_url",
                    "https://t.me/premiumbot?start=status",
                ),
            ));
        }
        if now_empty {
            self.dialog_suggested_actions.remove(&dialog_key);
        }
    }

    /// `set_dialog_pending_suggestions` (SuggestedActionManager.cpp).
    pub fn set_dialog_pending_suggestions(
        &mut self,
        env: &mut impl SuggestedActionManagerEnv,
        dialog_id: DialogId,
        pending_suggestions: Vec<String>,
    ) {
        if !self.dismiss_suggested_action_queries.is_empty() {
            // cpp:236-238 — the while-dismissing gate.
            return;
        }
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
    /// convention; the C++ carries the promise through the query handler
    /// — the §5 pairing note).
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

/// The free `dismiss_suggested_action` (SuggestedAction.cpp:312-355) —
/// the `hideSuggestedAction` request entry (Requests.cpp:8144-8148): the
/// type ladder over the empty/noskip/set-password bands, everything else
/// routing into the manager flow.
pub fn dismiss_suggested_action(
    env: &mut impl SuggestedActionManagerEnv,
    manager: &mut SuggestedActionManager,
    action: SuggestedAction,
    mut promise: Promise<()>,
) {
    match action.type_ {
        // cpp:315-316
        SuggestedActionType::Empty => {
            promise.set_error(Status::error(400, "Action must be non-empty"))
        }
        // cpp:317-318
        SuggestedActionType::SetupLoginEmailNoskip => {
            promise.set_error(Status::error(400, "The action can't be hidden"))
        }
        // cpp:329-347
        SuggestedActionType::SetPassword => {
            if action.otherwise_relogin_days < 0 {
                // cpp:333-334
                return promise
                    .set_error(Status::error(400, "Invalid authorization_delay specified"));
            }
            if action.otherwise_relogin_days == 0 {
                // cpp:335-338 — the plain dismiss flow.
                manager.dismiss_suggested_action(env, action, promise);
                return;
            }
            // cpp:339-346 — the matching option band emits the removal
            // update and clears the option; every band settles ok without
            // a wire query.
            let days = env.get_option_integer("otherwise_relogin_days", 0) as i32;
            if days == action.otherwise_relogin_days {
                let removed_actions = vec![action];
                env.send_update(get_update_suggested_actions_object(
                    None,
                    &[],
                    &removed_actions,
                    "dismiss_suggested_action",
                    &env.get_option_string(
                        "premium_manage_subscription_url",
                        "https://t.me/premiumbot?start=status",
                    ),
                ));
                env.set_option_empty("otherwise_relogin_days");
            }
            promise.set_value(());
        }
        // cpp:319-328 — every dismissable type routes into the manager.
        _ => manager.dismiss_suggested_action(env, action, promise),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggested_action::SuggestedActionType;
    use std::collections::BTreeMap;

    struct TestEnv {
        pmc: BTreeMap<Vec<u8>, Vec<u8>>,
        erased: Vec<String>,
        updates: Vec<td_api::Update>,
        queries: Vec<String>,
        options: BTreeMap<String, i64>,
        cleared_options: Vec<String>,
        dialog_access_ok: bool,
        close: bool,
    }

    impl TestEnv {
        fn new() -> TestEnv {
            TestEnv {
                pmc: BTreeMap::new(),
                erased: Vec::new(),
                updates: Vec::new(),
                queries: Vec::new(),
                options: BTreeMap::new(),
                cleared_options: Vec::new(),
                dialog_access_ok: false,
                close: false,
            }
        }
    }

    impl SuggestedActionManagerEnv for TestEnv {
        fn close_flag(&self) -> bool {
            self.close
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

        fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
            *self.options.get(name).unwrap_or(&default_value)
        }

        fn set_option_empty(&mut self, name: &str) {
            self.options.remove(name);
            self.cleared_options.push(name.to_string());
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn check_dialog_access(&mut self, dialog_id: DialogId) -> std::result::Result<(), Status> {
            if dialog_id.is_valid() && self.dialog_access_ok {
                Ok(())
            } else {
                Err(Status::error(400, "Chat not found"))
            }
        }

        fn get_input_peer(&mut self, _dialog_id: DialogId) -> Option<telegram_api::InputPeer> {
            Some(telegram_api::InputPeer::inputPeerEmpty(
                telegram_api::inputPeerEmpty {},
            ))
        }

        fn send_dismiss_suggestion_query(&mut self, action: &SuggestedAction) {
            self.queries.push(format!(
                "dismissSuggestion {}",
                action.get_suggested_action_str()
            ));
        }

        fn send_get_promo_data_query(&mut self) {
            self.queries.push("getPromoData".to_string());
        }
    }

    type UnitSlot = std::sync::Arc<std::sync::Mutex<Option<std::result::Result<(), Status>>>>;

    struct Recording(UnitSlot);

    impl tdutils::promise::PromiseInterface<()> for Recording {
        fn set_value(&mut self, _value: ()) {
            *self.0.lock().unwrap() = Some(Ok(()));
        }

        fn set_error(&mut self, error: Status) {
            *self.0.lock().unwrap() = Some(Err(error));
        }
    }

    fn record() -> (Promise<()>, UnitSlot) {
        let recorded: UnitSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
        (
            Promise::from_interface(Recording(recorded.clone())),
            recorded,
        )
    }

    fn settle(slot: &UnitSlot) {
        match slot.lock().unwrap().take().expect("the promise settled") {
            Ok(()) => (),
            Err(status) => panic!(
                "expected ok, got error: {} {}",
                status.code(),
                status.message()
            ),
        }
    }

    fn settle_err(slot: &UnitSlot) -> Status {
        match slot.lock().unwrap().take().expect("the promise settled") {
            Err(status) => status,
            Ok(()) => panic!("expected error, got ok"),
        }
    }

    #[test]
    fn update_filters_to_persistent_and_persists() {
        let mut env = TestEnv::new();
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
        let mut env2 = TestEnv::new();
        env2.pmc = env.pmc.clone();
        fresh.start_up(&mut env2);
        assert_eq!(fresh.suggested_actions.len(), 1);
    }

    #[test]
    fn hide_removes_and_emits() {
        let mut env = TestEnv::new();
        let mut manager = SuggestedActionManager::new();
        let action = SuggestedAction::from_action_str("PREMIUM_UPGRADE");
        manager.update_suggested_actions(&mut env, vec![action.clone()]);
        manager.hide_suggested_action(&mut env, action);
        assert!(manager.suggested_actions.is_empty());
        assert_eq!(env.updates.len(), 2);
        assert_eq!(env.erased, vec!["suggested_actions".to_string()]);
    }

    #[test]
    fn dismiss_parks_the_present_action_only() {
        let mut env = TestEnv::new();
        let mut manager = SuggestedActionManager::new();

        // An absent action settles ok with no query and no removal
        // (cpp:179-181).
        let (promise, slot) = record();
        manager.dismiss_suggested_action(
            &mut env,
            SuggestedAction::from_action_str("PREMIUM_UPGRADE"),
            promise,
        );
        settle(&slot);
        assert!(env.queries.is_empty(), "no wire query for an absent action");

        // A present action emits the removal update, saves and parks;
        // the head-only send (cpp:187). A second dismiss of the same
        // action settles ok inline — the optimistic removal at the first
        // entry took the action out (cpp:173-181), so the lot holds
        // exactly the head promise.
        let action = SuggestedAction::from_action_str("PREMIUM_UPGRADE");
        manager.update_suggested_actions(&mut env, vec![action.clone()]);
        let updates_before = env.updates.len();
        let (promise, slot) = record();
        manager.dismiss_suggested_action(&mut env, action.clone(), promise);
        let (promise2, slot2) = record();
        manager.dismiss_suggested_action(&mut env, action.clone(), promise2);
        settle(&slot2);
        assert_eq!(env.queries, ["dismissSuggestion PREMIUM_UPGRADE"]);
        assert!(manager.suggested_actions.is_empty(), "removed at entry");
        assert_eq!(env.updates.len(), updates_before + 1, "the removal update");
        assert_eq!(
            manager
                .dismiss_suggested_action_queries
                .get(&(0, SuggestedActionType::UpgradePremium as i32))
                .map(|lot| lot.promises.len()),
            Some(1),
            "the second dismiss settles inline, not in the lot"
        );

        // The ok answer settles the lot (cpp:219) and reports the
        // reload tail (cpp:216).
        let reload = manager.on_dismiss_suggested_action(
            &mut env,
            (0, SuggestedActionType::UpgradePremium as i32),
            Ok(()),
        );
        assert!(reload, "the no-dialog ok branch reloads the app config");
        settle(&slot);
        assert!(manager.dismiss_suggested_action_queries.is_empty());

        // The error answer fails the parked lot (cpp:206-208).
        manager.update_suggested_actions(&mut env, vec![action.clone()]);
        let (promise, slot) = record();
        manager.dismiss_suggested_action(&mut env, action.clone(), promise);
        manager.on_dismiss_suggested_action(
            &mut env,
            (0, SuggestedActionType::UpgradePremium as i32),
            Err(Status::error(400, "SUGGESTION_INVALID")),
        );
        let status = settle_err(&slot);
        assert_eq!(status.code(), 400);
        assert_eq!(status.message(), "SUGGESTION_INVALID");
    }

    #[test]
    fn dismiss_dialog_branch_bands() {
        let mut env = TestEnv::new();
        let mut manager = SuggestedActionManager::new();
        let action = SuggestedAction::from_action_str_and_dialog(
            "CONVERT_GIGAGROUP",
            DialogId::new(-1002147483647),
        );

        // The access gate error forwards to the promise (cpp:163-165).
        let (promise, slot) = record();
        manager.dismiss_suggested_action(&mut env, action.clone(), promise);
        let status = settle_err(&slot);
        assert_eq!(status.message(), "Chat not found");
        assert!(env.queries.is_empty());

        // The membership band — an accessible dialog without the pending
        // suggestion settles ok with no query (cpp:167-170).
        env.dialog_access_ok = true;
        let (promise, slot) = record();
        manager.dismiss_suggested_action(&mut env, action.clone(), promise);
        settle(&slot);
        assert!(env.queries.is_empty());

        // The present dialog suggestion removes from the dialog map
        // (cpp:171) and parks; the answer's dialog branch reports no
        // reload (cpp:210-211).
        manager
            .dialog_suggested_actions
            .insert(-1002147483647, vec![action.clone()]);
        let (promise, slot) = record();
        manager.dismiss_suggested_action(&mut env, action.clone(), promise);
        assert_eq!(env.queries, ["dismissSuggestion CONVERT_GIGAGROUP"]);
        assert!(
            !manager
                .dialog_suggested_actions
                .get(&-1002147483647)
                .is_some_and(|actions| actions.contains(&action)),
            "removed from the dialog map"
        );
        let reload = manager.on_dismiss_suggested_action(
            &mut env,
            (
                -1002147483647,
                SuggestedActionType::ConvertToGigagroup as i32,
            ),
            Ok(()),
        );
        assert!(!reload, "the dialog branch never reloads the app config");
        settle(&slot);
    }

    #[test]
    fn update_and_pending_gates_hold_while_dismissing() {
        let mut env = TestEnv::new();
        let mut manager = SuggestedActionManager::new();
        let action = SuggestedAction::from_action_str("PREMIUM_UPGRADE");
        manager.update_suggested_actions(&mut env, vec![action.clone()]);
        let (promise, _slot) = record();
        manager.dismiss_suggested_action(&mut env, action.clone(), promise);

        // cpp:141-144 — the update gate.
        manager.update_suggested_actions(
            &mut env,
            vec![SuggestedAction::from_action_str("PREMIUM_RESTORE")],
        );
        assert!(
            manager
                .suggested_actions
                .iter()
                .all(|stored| stored.type_ != SuggestedActionType::RestorePremium),
            "no update while dismissing"
        );

        // cpp:236-238 — the pending-suggestions gate.
        manager.set_dialog_pending_suggestions(
            &mut env,
            DialogId::new(-100),
            vec!["CONVERT_GIGAGROUP".to_string()],
        );
        assert!(
            !manager.dialog_suggested_actions.contains_key(&-100),
            "no pending suggestions while dismissing"
        );
    }

    #[test]
    fn login_email_park_and_pop() {
        let mut env = TestEnv::new();
        let mut manager = SuggestedActionManager::new();
        let (promise, _slot) = record();
        manager.is_login_email_address_required(&mut env, promise);
        assert_eq!(env.queries, ["getPromoData"]);
        assert!(manager.take_login_email_promise().is_some());
        assert!(manager.take_login_email_promise().is_none());
    }

    #[test]
    fn the_free_ladder_bands() {
        let mut env = TestEnv::new();
        let mut manager = SuggestedActionManager::new();

        // cpp:315-316 — the empty band.
        let (promise, slot) = record();
        dismiss_suggested_action(&mut env, &mut manager, SuggestedAction::default(), promise);
        let status = settle_err(&slot);
        assert_eq!(status.message(), "Action must be non-empty");

        // cpp:317-318 — the noskip band.
        let (promise, slot) = record();
        dismiss_suggested_action(
            &mut env,
            &mut manager,
            SuggestedAction::from_action_str("SETUP_LOGIN_EMAIL_NOSKIP"),
            promise,
        );
        let status = settle_err(&slot);
        assert_eq!(status.message(), "The action can't be hidden");

        // cpp:333-334 — the invalid delay band.
        let mut action = SuggestedAction::from_action_str("SETUP_PASSWORD");
        action.otherwise_relogin_days = -1;
        let (promise, slot) = record();
        dismiss_suggested_action(&mut env, &mut manager, action, promise);
        let status = settle_err(&slot);
        assert_eq!(status.message(), "Invalid authorization_delay specified");

        // cpp:339-346 — the matching days band emits the removal update,
        // clears the option and settles ok without a wire query.
        let mut action = SuggestedAction::from_action_str("SETUP_PASSWORD");
        action.otherwise_relogin_days = 7;
        env.options.insert("otherwise_relogin_days".to_string(), 7);
        let (promise, slot) = record();
        dismiss_suggested_action(&mut env, &mut manager, action, promise);
        settle(&slot);
        assert!(env.queries.is_empty(), "no wire query for the days band");
        assert_eq!(env.cleared_options, ["otherwise_relogin_days"]);
        match env.updates.last() {
            Some(td_api::Update::updateSuggestedActions(update)) => {
                assert_eq!(update.removed_actions.len(), 1);
            }
            other => panic!("the days band emits the removal update: {other:?}"),
        }

        // The non-matching days band settles ok with no update.
        let mut action = SuggestedAction::from_action_str("SETUP_PASSWORD");
        action.otherwise_relogin_days = 7;
        let updates_before = env.updates.len();
        let (promise, slot) = record();
        dismiss_suggested_action(&mut env, &mut manager, action, promise);
        settle(&slot);
        assert_eq!(env.updates.len(), updates_before);
        assert_eq!(env.cleared_options.len(), 1);

        // cpp:335-338 — the zero-delay form routes into the dismiss flow
        // (the absent action settles ok inline).
        let action = SuggestedAction::from_action_str("SETUP_PASSWORD");
        let (promise, slot) = record();
        dismiss_suggested_action(&mut env, &mut manager, action, promise);
        settle(&slot);
        assert!(env.queries.is_empty());
    }

    #[test]
    fn on_dismiss_close_gate_drops() {
        // cpp:196-198 — the close flag returns before the lot pops; the
        // parked promises stay parked (the collapsed close path drops
        // them, the hangup note in the module doc).
        let mut env = TestEnv::new();
        let mut manager = SuggestedActionManager::new();
        let action = SuggestedAction::from_action_str("PREMIUM_UPGRADE");
        manager.update_suggested_actions(&mut env, vec![action.clone()]);
        let (promise, slot) = record();
        manager.dismiss_suggested_action(&mut env, action, promise);
        env.close = true;
        manager.on_dismiss_suggested_action(
            &mut env,
            (0, SuggestedActionType::UpgradePremium as i32),
            Ok(()),
        );
        assert!(slot.lock().unwrap().is_none(), "the close gate parks on");
        assert_eq!(manager.dismiss_suggested_action_queries.len(), 1);
    }
}
