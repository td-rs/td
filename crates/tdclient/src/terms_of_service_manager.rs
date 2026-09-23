//! `TermsOfServiceManager` (td/telegram/TermsOfServiceManager.{h,cpp}) —
//! the M.33 Wave-3 terms-of-service manager. The queries
//! (GetTermsOfServiceUpdate/AcceptTermsOfService) are `[blocked: net]`
//! seams; the accept answer handler is real.

use crate::td_api;
use crate::terms_of_service::TermsOfService;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The `G()`/`td_` surface of the manager.
pub trait TermsOfServiceManagerEnv {
    fn send_update(&mut self, update: td_api::Update);

    /// `GetTermsOfServiceUpdateQuery::send` — `[blocked: net]`.
    fn send_get_terms_of_service_update_query(&mut self) {
        tdutils::log_info!("Send help.getTermsOfServiceUpdate — [blocked: net]");
    }

    /// `AcceptTermsOfServiceQuery::send` — `[blocked: net]`.
    fn send_accept_terms_of_service_query(&mut self, terms_of_service_id: &str) {
        tdutils::log_info!(
            "Send help.acceptTermsOfService for {terms_of_service_id} — [blocked: net]"
        );
    }
}

/// The collapsed `TermsOfServiceManager`.
#[derive(Default)]
pub struct TermsOfServiceManager {
    pending_terms_of_service: TermsOfService,
    is_inited: bool,
    /// The `()`-typed promises parked at their `[blocked: net]` accept
    /// sends.
    pub net_blocked_promises: Vec<Promise<()>>,
}

impl TermsOfServiceManager {
    pub fn new() -> TermsOfServiceManager {
        TermsOfServiceManager::default()
    }

    /// `init` (TermsOfServiceManager.cpp:95-100 area) — the auth-success
    /// hook.
    pub fn init(&mut self, env: &mut impl TermsOfServiceManagerEnv) {
        if self.is_inited {
            return;
        }
        self.is_inited = true;
        self.get_terms_of_service(env, Promise::default());
    }

    /// `get_terms_of_service` (TermsOfServiceManager.cpp) — the query is
    /// the seam; the answer handler is
    /// [`TermsOfServiceManager::on_get_terms_of_service`].
    fn get_terms_of_service(
        &mut self,
        env: &mut impl TermsOfServiceManagerEnv,
        _promise: Promise<()>,
    ) {
        env.send_get_terms_of_service_update_query();
    }

    /// `on_get_terms_of_service` (TermsOfServiceManager.cpp) — the result
    /// handler the net seam will call.
    pub fn on_get_terms_of_service(
        &mut self,
        env: &mut impl TermsOfServiceManagerEnv,
        result: std::result::Result<(i32, TermsOfService), Status>,
    ) {
        match result {
            Ok((_expires, terms_of_service)) => {
                if terms_of_service.get_id() != self.pending_terms_of_service.get_id() {
                    self.pending_terms_of_service = terms_of_service;
                    env.send_update(self.get_update_terms_of_service_object());
                }
            }
            Err(_status) => {
                // the C++ logs and drops the poll failure
            }
        }
    }

    /// `accept_terms_of_service` (TermsOfServiceManager.cpp) — the query
    /// is the seam; the answer handler is
    /// [`TermsOfServiceManager::on_accept_terms_of_service`].
    pub fn accept_terms_of_service(
        &mut self,
        env: &mut impl TermsOfServiceManagerEnv,
        terms_of_service_id: String,
        promise: Promise<()>,
    ) {
        env.send_accept_terms_of_service_query(&terms_of_service_id);
        self.net_blocked_promises.push(promise);
    }

    /// `on_accept_terms_of_service` — the accept answer handler.
    pub fn on_accept_terms_of_service(&mut self, env: &mut impl TermsOfServiceManagerEnv) {
        if self.pending_terms_of_service.get_id().is_empty() {
            return;
        }
        self.pending_terms_of_service = TermsOfService::default();
        env.send_update(self.get_update_terms_of_service_object());
    }

    /// `get_update_terms_of_service_object` (TermsOfServiceManager.cpp).
    pub fn get_update_terms_of_service_object(&self) -> td_api::Update {
        td_api::Update::updateTermsOfService(td_api::updateTermsOfService {
            terms_of_service_id: self.pending_terms_of_service.get_id().to_string(),
            terms_of_service: None,
        })
    }

    /// `get_current_state` (Requests.cpp:2204 area).
    pub fn get_current_state(
        &self,
        env: &impl TermsOfServiceManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        let _ = env;
        if !self.pending_terms_of_service.get_id().is_empty() {
            updates.push(self.get_update_terms_of_service_object());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnv {
        updates: Vec<td_api::Update>,
        queries: Vec<String>,
    }

    impl TermsOfServiceManagerEnv for TestEnv {
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

    #[test]
    fn init_polls_once_and_the_answer_emits_an_update() {
        let mut env = TestEnv {
            updates: Vec::new(),
            queries: Vec::new(),
        };
        let mut manager = TermsOfServiceManager::new();
        manager.init(&mut env);
        manager.init(&mut env);
        assert_eq!(env.queries.len(), 1);

        // build a non-empty terms through the wire ctor
        let wire = tdmtproto::telegram_api::help_TermsOfService {
            flags: 0,
            popup: true,
            id: Some(tdmtproto::telegram_api::DataJSON {
                data: "tos-1".to_string(),
            }),
            text: "text".to_string(),
            entities: Vec::new(),
            min_age_confirm: 0,
        };
        let terms = crate::terms_of_service::TermsOfService::from_telegram(Some(&wire));
        manager.on_get_terms_of_service(&mut env, Ok((10, terms)));
        assert_eq!(env.updates.len(), 1);

        let (promise, recorded) = record();
        manager.accept_terms_of_service(&mut env, "tos-1".to_string(), promise);
        assert!(
            env.queries
                .iter()
                .any(|q| q == "acceptTermsOfService tos-1")
        );
        assert_eq!(manager.net_blocked_promises.len(), 1);
        assert!(
            !recorded.is_some(),
            "the accept promise parks with the seam"
        );
    }

    struct Recorded(std::sync::Arc<std::sync::Mutex<Option<std::result::Result<(), Status>>>>);

    impl Recorded {
        fn is_some(&self) -> bool {
            self.0.lock().unwrap().is_some()
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
