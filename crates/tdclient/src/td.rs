//! Port of the minimal `td/telegram/Td.{h,cpp}` actor (S70 subset): the
//! request entry point the ClientManager talks to, the `TdCallback`
//! response channel, and the startup-state emission the C++ performs before
//! answering any request (`get_common_state` + the initial
//! `authorizationStateWaitTdlibParameters`).
//!
//! Synchronous requests are answered immediately (`SynchronousRequests`,
//! like the C++ `Td::request` short-circuit); everything else reports the
//! interim 500 until the request dispatcher lands (Wave 1).

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
}

impl Td {
    pub fn new(callback: Box<dyn TdCallback>, is_test_dc: bool) -> Self {
        Self {
            callback: Some(callback),
            options: OptionManager::new(is_test_dc),
        }
    }

    /// `Td::request` (the S70 subset): synchronous requests answer
    /// immediately, everything else is the interim 500.
    fn request(&mut self, request_id: u64, function: Function) {
        let callback = self.callback.as_mut().expect("request after close");
        if synchronous_requests::is_synchronous_request(&function) {
            callback.on_result(request_id, synchronous_requests::run_request(function));
        } else {
            callback.on_error(
                request_id,
                td_api::Error {
                    code: 500,
                    message: "TDLib function is not implemented yet".to_string(),
                },
            );
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
    fn non_synchronous_request_reports_the_interim_error() {
        run_actor_with(|actor, callback, scheduler| {
            let _ = scheduler.run_main(0.01);
            let send_ptr = scheduler.send_scheduler();
            {
                let _guard = send_ptr.guard(false);
                send_request(actor, 8, Function::getMe(td_api::getMe));
            }
            let _ = scheduler.run_main(0.01);
            let errors = callback.errors.lock().unwrap();
            assert!(
                errors
                    .iter()
                    .any(|(id, error)| *id == 8 && error.code == 500)
            );
        });
    }
}
