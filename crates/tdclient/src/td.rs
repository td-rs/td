//! Port of the minimal `td/telegram/Td.{h,cpp}` actor (S70 subset): the
//! request entry point the ClientManager talks to, the `TdCallback`
//! response channel, and the startup-state emission the C++ performs before
//! answering any request (`get_common_state` + the initial
//! `authorizationStateWaitTdlibParameters`).
//!
//! The request path implements the C++ `Td::request` WaitParameters gate
//! (Td.cpp:299-374): the synchronous set answers immediately, preauth
//! requests queue in `pending_preauthentication_requests_`, and everything
//! else gets the 400 initialization-parameters message.

use tdactor::{Actor, ActorOwn, actor_messages, send_closure_later};

use crate::option_manager::OptionManager;
use crate::synchronous_requests;
use crate::td_api::{self, Function, Object};

/// `td::TdCallback` — the response channel from the Td actor back to the
/// ClientManager. The receiver-side callback's destructor sends the
/// `{client_id, 0, nullptr}` close notification (see `client.rs`).
pub trait TdCallback: Send {
    fn on_result(&mut self, id: u64, result: Object);
    fn on_error(&mut self, id: u64, error: td_api::Error);
}

/// The minimal Td actor: forwards every response through the callback and
/// emits the startup state on `start_up`.
pub struct Td {
    callback: Option<Box<dyn TdCallback>>,
    options: OptionManager,
    /// `Td::pending_preauthentication_requests_` — queued in
    /// WaitParameters, replayed by `setTdlibParameters` (Wave 1).
    pending_preauthentication_requests: Vec<(u64, Function)>,
}

impl Td {
    pub fn new(callback: Box<dyn TdCallback>, is_test_dc: bool) -> Self {
        Self {
            callback: Some(callback),
            options: OptionManager::new(is_test_dc),
            pending_preauthentication_requests: Vec::new(),
        }
    }

    /// `Td::request` — the WaitParameters gate verbatim (Td.cpp:299-374):
    /// the synchronous set answers immediately (incl. the synchronous
    /// `getOption` pair); `getAuthorizationState` answers with the bare
    /// `AuthorizationState` (the scheme's response type — Td.cpp:308);
    /// preinitialization requests run (interim 500 until their managers);
    /// **preauthentication requests queue** in
    /// `pending_preauthentication_requests_` without a response (the C++
    /// `emplace_back`); everything else gets the 400 "Initialization
    /// parameters are needed" message. The queues replay on
    /// `setTdlibParameters` with the real Td::init (Wave 1).
    fn request(&mut self, request_id: u64, function: Function) {
        let callback = self.callback.as_mut().expect("request after close");
        match &function {
            Function::getAuthorizationState(_) => {
                // `send_result(id, get_fake_authorization_state_object())` —
                // the BARE state, not the update wrapper.
                callback.on_result(
                    request_id,
                    Object::authorizationStateWaitTdlibParameters(
                        td_api::authorizationStateWaitTdlibParameters,
                    ),
                );
            }
            _ if synchronous_requests::is_synchronous_request(&function) => {
                // Includes getOption("version"/"commit_hash") — the C++
                // `SynchronousRequests::is_synchronous_request` gate in
                // Td::request precedes the state machine.
                callback.on_result(request_id, synchronous_requests::run_request(function));
            }
            _ if is_preinitialization_request(&function) => {
                // Interim until the test-request managers land.
                callback.on_error(
                    request_id,
                    td_api::Error {
                        code: 500,
                        message: "TDLib function is not implemented yet".to_string(),
                    },
                );
            }
            _ if is_preauthentication_request(&function) => {
                // `pending_preauthentication_requests_.emplace_back` — no
                // response until setTdlibParameters.
                self.pending_preauthentication_requests
                    .push((request_id, function));
            }
            _ => {
                // Everything else — including the authentication requests,
                // which the C++ WaitParameters gate does not special-case.
                callback.on_error(
                    request_id,
                    td_api::Error {
                        code: 400,
                        message:
                            "Initialization parameters are needed: call setTdlibParameters first"
                                .to_string(),
                    },
                );
            }
        }
    }

    /// `Td::start_up`: the C++ emits the common option state first, then the
    /// initial authorization state (the receive order pinned by the S68
    /// oracle).
    fn emit_start_up(&mut self) {
        let callback = self.callback.as_mut().expect("start_up without callback");
        let mut updates = Vec::new();
        OptionManager::get_common_state(&mut updates);
        for update in updates {
            callback.on_result(0, update);
        }
        callback.on_result(
            0,
            Object::updateAuthorizationState(td_api::updateAuthorizationState {
                authorization_state:
                    td_api::AuthorizationState::authorizationStateWaitTdlibParameters(
                        td_api::authorizationStateWaitTdlibParameters,
                    ),
            }),
        );
    }

    /// The queued `updateOption` broadcasts from the OptionManager, emitted
    /// as responses with request_id 0 (the C++ `Td::send_update` path).
    fn flush_option_updates(&mut self) {
        let callback = self.callback.as_mut().expect("flush after close");
        for update in self.options.take_updates() {
            callback.on_result(0, update);
        }
    }

    /// `Td` close: dropping the callback sends the close notification.
    fn close(&mut self) {
        drop(self.callback.take());
    }
}

actor_messages! {
    pub enum TdMessage for Td {
        Request { request_id: u64, function: Function, } => request(request_id, function),
        FlushOptionUpdates => flush_option_updates(),
        Close => close(),
    }
}

impl Actor for Td {
    fn start_up(&mut self) {
        self.emit_start_up();
    }

    fn tear_down(&mut self) {
        self.close();
    }

    // No `loop_`: the S70 subset is purely message-driven (the real C++
    // `Td::loop` yields to poll pending queries — that arrives with the
    // request dispatcher in Wave 1).
}

/// `Td::is_preinitialization_request` (Td.cpp:188-206).
fn is_preinitialization_request(function: &Function) -> bool {
    matches!(
        function,
        Function::getCurrentState(_)
            | Function::setAlarm(_)
            | Function::testUseUpdate(_)
            | Function::testCallEmpty(_)
            | Function::testSquareInt(_)
            | Function::testCallString(_)
            | Function::testCallBytes(_)
            | Function::testCallVectorInt(_)
            | Function::testCallVectorIntObject(_)
            | Function::testCallVectorString(_)
            | Function::testCallVectorStringObject(_)
            | Function::testProxy(_)
    )
}

/// `Td::is_preauthentication_request` (Td.cpp:208-250).
fn is_preauthentication_request(function: &Function) -> bool {
    matches!(
        function,
        Function::getInternalLink(_)
            | Function::getInternalLinkType(_)
            | Function::getLocalizationTargetInfo(_)
            | Function::getLanguagePackInfo(_)
            | Function::getLanguagePackStrings(_)
            | Function::synchronizeLanguagePack(_)
            | Function::addCustomServerLanguagePack(_)
            | Function::setCustomLanguagePack(_)
            | Function::editCustomLanguagePackInfo(_)
            | Function::setCustomLanguagePackString(_)
            | Function::deleteLanguagePack(_)
            | Function::processPushNotification(_)
            | Function::getOption(_)
            | Function::setOption(_)
            | Function::getStorageStatistics(_)
            | Function::getStorageStatisticsFast(_)
            | Function::getDatabaseStatistics(_)
            | Function::setNetworkType(_)
            | Function::getNetworkStatistics(_)
            | Function::addNetworkStatistics(_)
            | Function::resetNetworkStatistics(_)
            | Function::setApplicationVerificationToken(_)
            | Function::getCountries(_)
            | Function::getCountryCode(_)
            | Function::getPhoneNumberInfo(_)
            | Function::getDeepLinkInfo(_)
            | Function::getApplicationConfig(_)
            | Function::saveApplicationLogEvent(_)
            | Function::addProxy(_)
            | Function::editProxy(_)
            | Function::enableProxy(_)
            | Function::disableProxy(_)
            | Function::removeProxy(_)
            | Function::getProxies(_)
            | Function::pingProxy(_)
            | Function::testNetwork(_)
    )
}

/// Convenience: send a request through the typed message channel.
pub fn send_request(actor: &ActorOwn<Td>, request_id: u64, function: Function) {
    send_closure_later(
        actor,
        TdMessage::Request {
            request_id,
            function,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tdactor::{ConcurrentScheduler, create_actor};

    #[derive(Default)]
    struct CollectCallback {
        responses: Arc<Mutex<Vec<(u64, Object)>>>,
        errors: Arc<Mutex<Vec<(u64, td_api::Error)>>>,
    }

    impl TdCallback for CollectCallback {
        fn on_result(&mut self, id: u64, result: Object) {
            self.responses.lock().unwrap().push((id, result));
        }
        fn on_error(&mut self, id: u64, error: td_api::Error) {
            self.errors.lock().unwrap().push((id, error));
        }
    }

    /// Start a scheduler with one Td actor, pump `start_up`, and hand the
    /// pieces to the test.
    fn run_actor_with<T>(
        test: impl FnOnce(&ActorOwn<Td>, &CollectCallback, &mut ConcurrentScheduler) -> T,
    ) -> T {
        let callback = CollectCallback::default();
        let mut scheduler = ConcurrentScheduler::new(0);
        let actor = {
            let _guard = scheduler.get_main_guard();
            create_actor(
                "Td",
                Td::new(
                    Box::new(CollectCallback {
                        responses: callback.responses.clone(),
                        errors: callback.errors.clone(),
                    }),
                    false,
                ),
            )
        };
        scheduler.start();
        let _ = scheduler.run_main(0.01); // process start_up
        let result = test(&actor, &callback, &mut scheduler);
        scheduler.finish();
        result
    }

    #[test]
    fn startup_emits_common_state_then_auth_state() {
        run_actor_with(|_actor, callback, scheduler| {
            let _ = scheduler.run_main(0.01);
            let responses = callback.responses.lock().unwrap();
            // version, commit_hash, then authorizationStateWaitTdlibParameters.
            assert_eq!(responses.len(), 3);
            assert_eq!(responses[0].0, 0);
            assert!(matches!(
                &responses[0].1,
                Object::updateOption(u) if u.name == "version"
            ));
            assert!(matches!(
                &responses[1].1,
                Object::updateOption(u) if u.name == "commit_hash"
            ));
            assert!(matches!(
                &responses[2].1,
                Object::updateAuthorizationState(u)
                    if matches!(
                        u.authorization_state,
                        td_api::AuthorizationState::authorizationStateWaitTdlibParameters(_)
                    )
            ));
        });
    }

    #[test]
    fn synchronous_request_answers_immediately() {
        run_actor_with(|actor, callback, scheduler| {
            let _ = scheduler.run_main(0.01);
            let send_ptr = scheduler.send_scheduler();
            {
                let _guard = send_ptr.guard(false);
                send_request(
                    actor,
                    7,
                    Function::getOption(td_api::getOption {
                        name: "version".to_string(),
                    }),
                );
            }
            let _ = scheduler.run_main(0.01);
            let responses = callback.responses.lock().unwrap();
            assert!(responses.iter().any(|(id, object)| {
                *id == 7 && matches!(object, Object::optionValueString(v) if v.value == "1.8.67")
            }));
        });
    }

    #[test]
    fn wait_parameters_gating_matches_cpp() {
        run_actor_with(|actor, callback, scheduler| {
            let _ = scheduler.run_main(0.01);
            let send_ptr = scheduler.send_scheduler();
            {
                let _guard = send_ptr.guard(false);
                // Unclassified (getMe) and authentication (logOut) requests:
                // the C++ WaitParameters gate 400s both (Td.cpp:353-356).
                send_request(actor, 8, Function::getMe(td_api::getMe));
            }
            {
                let _guard = send_ptr.guard(false);
                send_request(actor, 9, Function::logOut(td_api::logOut));
            }
            let _ = scheduler.run_main(0.01);
            let errors = callback.errors.lock().unwrap();
            let error_of = |id| {
                errors
                    .iter()
                    .find(|(error_id, _)| *error_id == id)
                    .map(|(_, error)| error.clone())
                    .expect("the error response")
            };
            for id in [8, 9] {
                assert_eq!(
                    error_of(id).message,
                    "Initialization parameters are needed: call setTdlibParameters first"
                );
                assert_eq!(error_of(id).code, 400);
            }
        });
    }

    #[test]
    fn get_authorization_state_returns_the_bare_state() {
        run_actor_with(|actor, callback, scheduler| {
            let _ = scheduler.run_main(0.01);
            let send_ptr = scheduler.send_scheduler();
            {
                let _guard = send_ptr.guard(false);
                send_request(
                    actor,
                    10,
                    Function::getAuthorizationState(td_api::getAuthorizationState),
                );
            }
            let _ = scheduler.run_main(0.01);
            let responses = callback.responses.lock().unwrap();
            // `get_fake_authorization_state_object` — the bare state, not the
            // update wrapper (Td.cpp:308).
            assert!(responses.iter().any(|(id, object)| {
                *id == 10 && matches!(object, Object::authorizationStateWaitTdlibParameters(_))
            }));
        });
    }

    #[test]
    fn preauth_requests_queue_without_a_response() {
        run_actor_with(|actor, callback, scheduler| {
            let _ = scheduler.run_main(0.01);
            let send_ptr = scheduler.send_scheduler();
            {
                let _guard = send_ptr.guard(false);
                // setOption queues in pending_preauthentication_requests_
                // (the C++ emplace_back) — no response in WaitParameters.
                send_request(
                    actor,
                    11,
                    Function::setOption(td_api::setOption {
                        name: "message_unload_delay".to_string(),
                        value: td_api::OptionValue::optionValueInteger(
                            td_api::optionValueInteger { value: 59 },
                        ),
                    }),
                );
            }
            {
                let _guard = send_ptr.guard(false);
                // A non-synchronous getOption queues the same way.
                send_request(
                    actor,
                    12,
                    Function::getOption(td_api::getOption {
                        name: "database_directory".to_string(),
                    }),
                );
            }
            let _ = scheduler.run_main(0.01);
            assert!(
                callback
                    .responses
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(id, _)| *id == 0),
                "no request responses in WaitParameters"
            );
            assert!(callback.errors.lock().unwrap().is_empty());
        });
    }
}
