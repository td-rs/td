//! `TermsOfServiceManager` (td/telegram/TermsOfServiceManager.{h,cpp}) —
//! the M.33 Wave-3 terms-of-service manager. The net-drain 51 slice
//! lands the query plane: GetTermsOfServiceUpdateQuery/
//! AcceptTermsOfServiceQuery ride the account dispatch surface (the
//! shared `AccountEnv` net handle — the sibling account families'
//! plane) and the answers drain through `Td::on_account_net_answer`
//! into the intakes here. The C++ actor timer plane
//! (schedule_get_terms_of_service/timeout_expired, cpp:117-135) stays
//! collapsed: the init poll and the accept re-poll send directly, and
//! the expiry/`Random::fast` reschedules are documented at their
//! sites.

use crate::td_api;
use crate::terms_of_service::TermsOfService;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The `G()`/`td_` surface of the manager.
pub trait TermsOfServiceManagerEnv {
    /// `G()->close_flag()` — the init gate (cpp:106-108).
    fn close_flag(&self) -> bool;

    /// `td_->auth_manager_->is_bot()` — the init gate (cpp:109). The
    /// `is_authorized()` re-check beside it folds into the Rust call
    /// site invariant: `Td::init_terms_of_service_manager` is the
    /// AuthManager.cpp:1551 auth-success moment itself.
    fn is_bot(&self) -> bool;

    fn send_update(&mut self, update: td_api::Update);

    /// `GetTermsOfServiceUpdateQuery::send` (cpp:31-34).
    fn send_get_terms_of_service_update_query(&mut self);

    /// `AcceptTermsOfServiceQuery::send` (cpp:71-74) — the dataJSON id.
    fn send_accept_terms_of_service_query(&mut self, terms_of_service_id: &str);
}

/// The collapsed `TermsOfServiceManager`.
#[derive(Default)]
pub struct TermsOfServiceManager {
    pending_terms_of_service: TermsOfService,
    is_inited: bool,
    /// The `()`-typed accept promises parked at their sends — the C++
    /// carries them through the query handler into
    /// `on_accept_terms_of_service` (cpp:173-181); the Rust drain pops
    /// this FIFO lot (the campaign §5 pairing note).
    pub(crate) pending_accept_promises: Vec<Promise<()>>,
}

impl TermsOfServiceManager {
    pub fn new() -> TermsOfServiceManager {
        TermsOfServiceManager::default()
    }

    /// `init` (cpp:105-115) — the auth-success hook.
    pub fn init(&mut self, env: &mut impl TermsOfServiceManagerEnv) {
        if env.close_flag() || self.is_inited || env.is_bot() {
            return;
        }
        self.is_inited = true;
        self.get_terms_of_service(env);
    }

    /// `get_terms_of_service` (cpp:169-171) — the poll send. The C++
    /// carries the routing promise on the query (the lambda at
    /// cpp:130-133); the Rust drain calls
    /// [`TermsOfServiceManager::on_get_terms_of_service`] directly.
    fn get_terms_of_service(&mut self, env: &mut impl TermsOfServiceManagerEnv) {
        env.send_get_terms_of_service_update_query();
    }

    /// `on_get_terms_of_service` (cpp:145-167) — the poll answer
    /// intake. A non-empty pending always re-emits the update (the
    /// reference sends on every non-empty poll — the id-equality
    /// compare the M.33 port carried is not the C++ shape); the
    /// expiry/error reschedules ride the collapsed timer plane (the
    /// clamp at cpp:159, `Random::fast(10, 60)` at cpp:153 — the next
    /// poll arrives with the next auth-success init or accept
    /// re-poll).
    pub fn on_get_terms_of_service(
        &mut self,
        env: &mut impl TermsOfServiceManagerEnv,
        result: std::result::Result<(i32, TermsOfService), Status>,
    ) {
        match result {
            Ok((_expires_at, terms_of_service)) => {
                self.pending_terms_of_service = terms_of_service;
                if let Some(update) = self.get_update_terms_of_service_object() {
                    env.send_update(update);
                }
            }
            Err(_status) => {
                // the C++ logs and reschedules; the poll failure drops
            }
        }
    }

    /// `accept_terms_of_service` (cpp:173-181) — the send + the park
    /// (the reference sends unconditionally; no pending gate).
    pub fn accept_terms_of_service(
        &mut self,
        env: &mut impl TermsOfServiceManagerEnv,
        terms_of_service_id: String,
        promise: Promise<()>,
    ) {
        env.send_accept_terms_of_service_query(&terms_of_service_id);
        self.pending_accept_promises.push(promise);
    }

    /// Pop the head of the accept lot (the account `take_promise`
    /// convention; FIFO per §5).
    pub(crate) fn take_accept_promise(&mut self) -> Option<Promise<()>> {
        if self.pending_accept_promises.is_empty() {
            None
        } else {
            Some(self.pending_accept_promises.remove(0))
        }
    }

    /// `on_accept_terms_of_service` (cpp:184-188) — the accept answer
    /// intake: the pending clears, the promise settles and the
    /// schedule(0) re-poll sends immediately (the collapsed timer).
    pub fn on_accept_terms_of_service(
        &mut self,
        env: &mut impl TermsOfServiceManagerEnv,
        mut promise: Promise<()>,
    ) {
        self.pending_terms_of_service = TermsOfService::default();
        promise.set_value(());
        self.get_terms_of_service(env);
    }

    /// `get_update_terms_of_service_object` (cpp:136-143) — `None` for
    /// the empty pending; the full terms object rides the update.
    pub fn get_update_terms_of_service_object(&self) -> Option<td_api::Update> {
        let terms_of_service = self
            .pending_terms_of_service
            .get_terms_of_service_object(None)?;
        Some(td_api::Update::updateTermsOfService(
            td_api::updateTermsOfService {
                terms_of_service_id: self.pending_terms_of_service.get_id().to_string(),
                terms_of_service: Some(terms_of_service),
            },
        ))
    }

    /// `get_current_state` (cpp:190-201) — the `is_inited_` gate folds
    /// into the pending-empty invariant (only post-init polls set the
    /// pending).
    pub fn get_current_state(
        &self,
        env: &impl TermsOfServiceManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        let _ = env;
        if let Some(update) = self.get_update_terms_of_service_object() {
            updates.push(update);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnv {
        updates: Vec<td_api::Update>,
        queries: Vec<String>,
        close: bool,
        bot: bool,
    }

    impl TestEnv {
        fn new() -> TestEnv {
            TestEnv {
                updates: Vec::new(),
                queries: Vec::new(),
                close: false,
                bot: false,
            }
        }
    }

    impl TermsOfServiceManagerEnv for TestEnv {
        fn close_flag(&self) -> bool {
            self.close
        }

        fn is_bot(&self) -> bool {
            self.bot
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn send_get_terms_of_service_update_query(&mut self) {
            self.queries.push("getTermsOfServiceUpdate".to_string());
        }

        fn send_accept_terms_of_service_query(&mut self, terms_of_service_id: &str) {
            self.queries
                .push(format!("acceptTermsOfService {terms_of_service_id}"));
        }
    }

    fn terms(id: &str, text: &str) -> TermsOfService {
        TermsOfService::from_telegram(Some(&tdmtproto::telegram_api::help_TermsOfService {
            flags: 1 << 1,
            popup: true,
            id: Some(tdmtproto::telegram_api::DataJSON {
                data: id.to_string(),
            }),
            text: text.to_string(),
            entities: Vec::new(),
            min_age_confirm: 16,
        }))
    }

    #[test]
    fn init_polls_once_and_the_answer_emits_the_update() {
        let mut env = TestEnv::new();
        let mut manager = TermsOfServiceManager::new();
        manager.init(&mut env);
        manager.init(&mut env);
        assert_eq!(env.queries, ["getTermsOfServiceUpdate"]);

        manager.on_get_terms_of_service(&mut env, Ok((1_700_003_600, terms("tos-1", "Please"))));
        assert_eq!(env.updates.len(), 1);
        match &env.updates[0] {
            td_api::Update::updateTermsOfService(update) => {
                assert_eq!(update.terms_of_service_id, "tos-1");
                let object = update.terms_of_service.as_ref().unwrap();
                assert_eq!(object.min_user_age, 16);
                assert!(object.show_popup);
                assert_eq!(object.text.as_ref().unwrap().text, "Please");
            }
            other => panic!("unexpected update: {other:?}"),
        }

        // The always-send band — the reference re-emits on every
        // non-empty poll, no id compare (cpp:157-161).
        manager.on_get_terms_of_service(&mut env, Ok((1_700_003_600, terms("tos-1", "Please"))));
        assert_eq!(env.updates.len(), 2);

        // The empty answer clears the pending — no update (cpp:148-149
        // with the null-object branch at cpp:158-160).
        manager.on_get_terms_of_service(&mut env, Ok((1_700_003_600, TermsOfService::default())));
        assert_eq!(env.updates.len(), 2);
        assert!(manager.get_update_terms_of_service_object().is_none());
    }

    #[test]
    fn init_gates_mirror_the_reference() {
        // The close band (cpp:106-108) — the gate does not latch
        // is_inited_, a later open init still polls.
        let mut env = TestEnv::new();
        env.close = true;
        let mut manager = TermsOfServiceManager::new();
        manager.init(&mut env);
        assert!(env.queries.is_empty());
        assert!(!manager.is_inited);
        env.close = false;
        manager.init(&mut env);
        assert_eq!(env.queries.len(), 1);

        // The bot band (cpp:109) — same non-latching shape.
        let mut env = TestEnv::new();
        env.bot = true;
        let mut manager = TermsOfServiceManager::new();
        manager.init(&mut env);
        assert!(env.queries.is_empty());
        assert!(!manager.is_inited);
        env.bot = false;
        manager.init(&mut env);
        assert_eq!(env.queries.len(), 1);
    }

    #[test]
    fn accept_parks_then_the_intake_resolves_and_re_polls() {
        let mut env = TestEnv::new();
        let mut manager = TermsOfServiceManager::new();
        manager.init(&mut env);
        manager.on_get_terms_of_service(&mut env, Ok((1_700_003_600, terms("tos-1", "Please"))));
        assert_eq!(env.updates.len(), 1);

        let (promise, recorded) = record();
        manager.accept_terms_of_service(&mut env, "tos-1".to_string(), promise);
        assert!(
            env.queries
                .iter()
                .any(|q| q == "acceptTermsOfService tos-1")
        );
        assert!(recorded.is_none(), "the accept promise parks at the send");
        assert_eq!(manager.pending_accept_promises.len(), 1);

        // The answer intake (cpp:184-188) — the pending clears with no
        // update on the accept itself, the promise resolves and the
        // schedule(0) re-poll sends.
        let promise = manager.take_accept_promise().unwrap();
        manager.on_accept_terms_of_service(&mut env, promise);
        assert!(matches!(
            *recorded.0.lock().unwrap(),
            Some(std::result::Result::Ok(()))
        ));
        assert!(manager.get_update_terms_of_service_object().is_none());
        assert_eq!(env.updates.len(), 1);
        assert_eq!(
            env.queries
                .iter()
                .filter(|q| *q == "getTermsOfServiceUpdate")
                .count(),
            2
        );
    }

    #[test]
    fn the_poll_error_band_drops() {
        let mut env = TestEnv::new();
        let mut manager = TermsOfServiceManager::new();
        manager.init(&mut env);
        manager.on_get_terms_of_service(&mut env, Err(Status::error(500, "FLOOD_WAIT")));
        assert!(env.updates.is_empty());
    }

    struct Recorded(std::sync::Arc<std::sync::Mutex<Option<std::result::Result<(), Status>>>>);

    impl Recorded {
        fn is_none(&self) -> bool {
            self.0.lock().unwrap().is_none()
        }
    }

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
}
