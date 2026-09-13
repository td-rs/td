//! Port of `td/telegram/Client.{h,cpp}` (S67 scaffold; the full multi-client
//! scheduler port is S70).
//!
//! The scaffolding covers the observable ABI semantics: `create_client_id`
//! allocates identifiers, `send` routes synchronous requests through
//! [`crate::synchronous_requests`] immediately and queues an error response
//! for the rest (the Td actor that would process them lands in S70), and
//! `receive` pops responses with the C++ timeout semantics — the legacy
//! `Client` blocks up to `timeout` seconds, the new `ClientManager` ignores
//! the timeout and pumps once (Client.cpp:111-173).

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

use crate::synchronous_requests;
use crate::td_api::{Function, Object};

/// `Client::Request` (the legacy path) / the new API's
/// `ClientManager::send(client_id, request_id, request)`.
pub struct Request {
    pub id: u64,
    pub function: Function,
}

/// `ClientManager::Response`.
pub struct Response {
    /// The client the response belongs to (`0` is not a valid client id).
    pub client_id: i32,
    /// The request id, or 0 for updates.
    pub request_id: u64,
    /// The response object (a `nullptr` in C++ means "no response").
    pub object: Option<Object>,
}

#[derive(Default)]
struct ClientManagerInner {
    responses: Mutex<VecDeque<Response>>,
    wakeup: Condvar,
    /// Identifiers handed out by `create_client_id` (the C++ registers the
    /// client at creation; the Td actor is created lazily on first send).
    known_clients: Mutex<HashSet<i32>>,
    next_client_id: Mutex<i32>,
}

/// `td::ClientManager` — one response queue for all clients of this manager
/// (the process-wide singleton serves the `td_create_client_id` API).
pub struct ClientManager {
    inner: Arc<ClientManagerInner>,
}

impl Default for ClientManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ClientManagerInner {
                next_client_id: Mutex::new(1),
                ..ClientManagerInner::default()
            }),
        }
    }

    /// `ClientManager::create_client_id`.
    pub fn create_client_id(&self) -> i32 {
        let mut next = self.inner.next_client_id.lock().unwrap();
        let client_id = *next;
        *next += 1;
        self.inner.known_clients.lock().unwrap().insert(client_id);
        client_id
    }

    /// `ClientManager::send`: synchronous requests are answered immediately
    /// (their C++ path through the Td actor is unobservable), everything
    /// else queues the interim error until the Td actor lands (S70).
    pub fn send(&self, client_id: i32, request_id: u64, request: Function) {
        // client_id 0 is the legacy Client path (its manager is private, so
        // every id it sees is its own); the C++ legacy Client bypasses the
        // client-id lookup the same way.
        if client_id != 0
            && !self
                .inner
                .known_clients
                .lock()
                .unwrap()
                .contains(&client_id)
        {
            self.push_response(Response {
                client_id,
                request_id,
                object: Some(synchronous_requests::make_error(
                    400,
                    "Wrong client identifier",
                )),
            });
            return;
        }
        let object = if synchronous_requests::is_synchronous_request(&request) {
            synchronous_requests::run_request(request)
        } else {
            synchronous_requests::make_error(500, "TDLib function is not implemented yet")
        };
        self.push_response(Response {
            client_id,
            request_id,
            object: Some(object),
        });
    }

    fn push_response(&self, response: Response) {
        self.inner.responses.lock().unwrap().push_back(response);
        self.inner.wakeup.notify_one();
    }

    /// `ClientManager::receive`: like the C++ new API, the timeout is
    /// ignored and one non-blocking pump is performed (Client.cpp:134-140).
    pub fn receive(&self, _timeout: f64) -> Response {
        self.inner
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Response {
                client_id: 0,
                request_id: 0,
                object: None,
            })
    }

    /// `Client::receive` (legacy): block on the queue up to `timeout`
    /// seconds, returning `None` (a null pointer in C++) on expiry.
    pub fn receive_blocking(&self, timeout: f64) -> Option<Response> {
        let mut responses = self.inner.responses.lock().unwrap();
        if let Some(response) = responses.pop_front() {
            return Some(response);
        }
        let (guard, wait_result) = self
            .inner
            .wakeup
            .wait_timeout(
                responses,
                std::time::Duration::from_secs_f64(timeout.max(0.0)),
            )
            .unwrap();
        responses = guard;
        if wait_result.timed_out() {
            return None;
        }
        responses.pop_front()
    }

    /// `ClientManager::execute` — the static synchronous request path.
    pub fn execute(request: Function) -> Object {
        synchronous_requests::run_request(request)
    }

    /// `ClientManager::close(client_id)`: release the client. The C++ drops
    /// the Td actor when `authorizationStateClosed` is observed; the
    /// scaffold just forgets the id.
    pub fn close(&self, client_id: i32) {
        self.inner.known_clients.lock().unwrap().remove(&client_id);
    }
}

/// The process-wide manager behind the `td_create_client_id` JSON API
/// (`ClientManager::get_manager_singleton`, leaked like the C++).
pub fn get_manager_singleton() -> &'static ClientManager {
    static MANAGER: std::sync::OnceLock<ClientManager> = std::sync::OnceLock::new();
    MANAGER.get_or_init(ClientManager::new)
}

/// `td::Client` — the legacy `td_json_client_*` interface owns a private
/// manager, like the C++ (each legacy Client wraps its own MultiImpl).
pub struct Client {
    manager: ClientManager,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Self {
            manager: ClientManager::new(),
        }
    }

    /// `Client::send` (legacy): the client id of a legacy client is 0.
    pub fn send(&self, request: Request) {
        self.manager.send(0, request.id, request.function);
    }

    /// `Client::receive` (legacy): blocks up to `timeout` seconds.
    pub fn receive(&self, timeout: f64) -> Option<Response> {
        self.manager.receive_blocking(timeout)
    }

    /// `Client::execute` (legacy): the synchronous path, client-independent.
    pub fn execute(request: Request) -> Object {
        ClientManager::execute(request.function)
    }

    /// `Client::close`: a closed client ignores further requests (the C++
    /// returns errors for sends after close).
    pub fn close(&self) {
        self.manager.close(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::td_api;

    fn version_request(_id: u64) -> Function {
        Function::getOption(td_api::getOption {
            name: "version".to_string(),
        })
    }

    #[test]
    fn create_client_id_is_strictly_increasing() {
        let manager = ClientManager::new();
        let first = manager.create_client_id();
        let second = manager.create_client_id();
        assert!(first > 0);
        assert!(second > first);
    }

    #[test]
    fn synchronous_request_is_answered_immediately() {
        let manager = ClientManager::new();
        let client_id = manager.create_client_id();
        manager.send(client_id, 7, version_request(7));
        let response = manager.receive(1.0);
        assert_eq!(response.client_id, client_id);
        assert_eq!(response.request_id, 7);
        assert_eq!(
            response.object.unwrap().to_json(),
            r#"{"@type":"optionValueString","value":"1.8.67"}"#
        );
    }

    #[test]
    fn unknown_client_identifier_is_rejected() {
        let manager = ClientManager::new();
        manager.send(12345, 1, version_request(1));
        let response = manager.receive(0.0);
        assert_eq!(
            response.object.unwrap().to_json(),
            r#"{"@type":"error","code":400,"message":"Wrong client identifier"}"#
        );
    }

    #[test]
    fn non_synchronous_request_reports_the_interim_error() {
        let manager = ClientManager::new();
        let client_id = manager.create_client_id();
        manager.send(client_id, 1, Function::getMe(td_api::getMe));
        let response = manager.receive(0.0);
        assert_eq!(
            response.object.unwrap().to_json(),
            r#"{"@type":"error","code":500,"message":"TDLib function is not implemented yet"}"#
        );
    }

    #[test]
    fn legacy_client_blocks_up_to_the_timeout() {
        let client = Client::new();
        let start = std::time::Instant::now();
        assert!(client.receive(0.05).is_none());
        assert!(start.elapsed() >= std::time::Duration::from_millis(40));
        client.send(Request {
            id: 1,
            function: version_request(1),
        });
        let response = client.receive(1.0).unwrap();
        assert_eq!(response.request_id, 1);
        assert_eq!(
            response.object.unwrap().to_json(),
            r#"{"@type":"optionValueString","value":"1.8.67"}"#
        );
    }

    #[test]
    fn execute_is_client_independent() {
        let object = Client::execute(Request {
            id: 0,
            function: version_request(0),
        });
        assert_eq!(
            object.to_json(),
            r#"{"@type":"optionValueString","value":"1.8.67"}"#
        );
    }
}
