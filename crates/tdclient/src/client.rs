//! Port of `td/telegram/Client.{h,cpp}` (S70): `TdReceiver` (the output
//! queue with the C++ timeout/clamp/close semantics), `MultiImpl` (a
//! ConcurrentScheduler hosting a `MultiTd` actor per pool), `MultiImplPool`
//! (least-loaded selection), `ClientManager` (the client-id API with lazy
//! client creation, closed-client release and the synthesized
//! `authorizationStateClosed` updates) and the legacy `Client`.
//!
//! Client ids come from the process-wide `MultiImpl::create_id` counter,
//! which the legacy `Client` also consumes — the S68 oracle pinned the C++
//! numbering (legacy takes 1, the first `td_create_client_id` returns 2).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};

use tdactor::scheduler::{ConcurrentScheduler, SchedulerPtr};
use tdactor::{Actor, ActorOwn, actor_messages, create_actor, send_closure};

use crate::synchronous_requests;
use crate::td::{self, Td, TdCallback};
use crate::td_api::{self, Function, Object};

/// `Client::Request` (the legacy path).
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

impl Response {
    fn none() -> Self {
        Self {
            client_id: 0,
            request_id: 0,
            object: None,
        }
    }
}

/// The `MpscPollableQueue<Response>` of the C++ `TdReceiver` on a
/// condvar-backed deque.
struct OutputQueue {
    queue: Mutex<VecDeque<Response>>,
    wakeup: Condvar,
}

impl OutputQueue {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            wakeup: Condvar::new(),
        }
    }

    /// `MpscPollableQueue::writer_put` — any thread.
    fn writer_put(&self, response: Response) {
        self.queue.lock().unwrap().push_back(response);
        self.wakeup.notify_one();
    }

    /// `reader_wait(timeout_ms)`: pop, or wait up to `timeout_ms`
    /// milliseconds (the C++ eventfd wait); 0 never waits.
    fn reader_wait(&self, timeout_ms: i64) -> Option<Response> {
        let mut queue = self.queue.lock().unwrap();
        if let Some(response) = queue.pop_front() {
            return Some(response);
        }
        if timeout_ms == 0 {
            return None;
        }
        let (guard, wait_result) = self
            .wakeup
            .wait_timeout(queue, std::time::Duration::from_millis(timeout_ms as u64))
            .unwrap();
        queue = guard;
        if wait_result.timed_out() {
            return None;
        }
        queue.pop_front()
    }
}

/// `td::Client::TdReceiver` — the response queue of a manager, with the
/// C++ receive_lock guard (concurrent receives abort like the C++ LOG(FATAL)).
struct TdReceiver {
    output_queue: Arc<OutputQueue>,
    receive_lock: AtomicBool,
}

impl TdReceiver {
    fn new() -> Self {
        Self {
            output_queue: Arc::new(OutputQueue::new()),
            receive_lock: AtomicBool::new(false),
        }
    }

    fn receive(&self, timeout: f64, from_manager: bool) -> Response {
        // Client.cpp:281/294 — the td_requests wait bookends.
        tdutils::vlog!(
            tdutils::logging::verbosity::TD_REQUESTS,
            "td_requests",
            at "Client.cpp",
            281,
            "Begin to wait for updates with timeout {timeout:.6}"
        );
        if self.receive_lock.swap(true, Ordering::AcqRel) {
            let message = if from_manager {
                "Receive must not be called simultaneously from two different threads, but this has just \
                 happened. Call it from a fixed thread, dedicated for updates and response processing."
            } else {
                "Receive is called after Client destroy, or simultaneously from different threads"
            };
            tdutils::log_fatal!("{message}");
        }
        let response = self.receive_unlocked(timeout.clamp(0.0, 1_000_000.0));
        // Not `debug_assert!` — the swap is the actual release and must run
        // in release builds too (a stripped release aborted the second
        // receive in the S68 oracle run).
        let was_locked = self.receive_lock.swap(false, Ordering::AcqRel);
        assert!(was_locked, "receive lock released twice");
        tdutils::vlog!(
            tdutils::logging::verbosity::TD_REQUESTS,
            "td_requests",
            at "Client.cpp",
            294,
            "End to wait for updates, returning object {} {:p}",
            response.request_id,
            response.object.as_ref().map_or(std::ptr::null::<()>(), |_| {
                // The C++ prints the object's heap pointer; any stable
                // non-null address gives the same line shape.
                &response as *const _ as *const ()
            })
        );
        response
    }

    fn receive_unlocked(&self, timeout: f64) -> Response {
        // C++ `static_cast<int>(timeout * 1000)` on the eventfd wait.
        let timeout_ms = (timeout * 1000.0) as i64;
        self.output_queue
            .reader_wait(timeout_ms)
            .unwrap_or_else(Response::none)
    }

    fn add_response(&self, client_id: i32, id: u64, result: Option<Object>) {
        self.output_queue.writer_put(Response {
            client_id,
            request_id: id,
            object: result,
        });
    }

    /// `TdReceiver::create_callback`: responses go to the queue; the
    /// destructor posts the `{client_id, 0, nullptr}` close notification.
    fn create_callback(&self, client_id: i32) -> Box<dyn TdCallback> {
        Box::new(ReceiverCallback {
            client_id,
            output_queue: self.output_queue.clone(),
        })
    }
}

/// The C++ `TdReceiver::Callback`.
struct ReceiverCallback {
    client_id: i32,
    output_queue: Arc<OutputQueue>,
}

impl TdCallback for ReceiverCallback {
    fn on_result(&mut self, id: u64, result: Object) {
        self.output_queue.writer_put(Response {
            client_id: self.client_id,
            request_id: id,
            object: Some(result),
        });
    }

    fn on_error(&mut self, id: u64, error: td_api::Error) {
        self.output_queue.writer_put(Response {
            client_id: self.client_id,
            request_id: id,
            object: Some(Object::error(error)),
        });
    }
}

impl Drop for ReceiverCallback {
    fn drop(&mut self) {
        self.output_queue.writer_put(Response {
            client_id: self.client_id,
            request_id: 0,
            object: None,
        });
    }
}

/// `Client.cpp:MultiTd` — one Td actor per client of a `MultiImpl`.
#[derive(Default)]
struct MultiTd {
    tds: HashMap<i32, ActorOwn<Td>>,
}

actor_messages! {
    enum MultiTdMessage for MultiTd {
        Create { td_id: i32, callback: Box<dyn TdCallback>, } => create(td_id, callback),
        Send { client_id: i32, request_id: u64, request: Function, } => send(client_id, request_id, request),
        Close { td_id: i32, } => close(td_id),
    }
}

impl MultiTd {
    fn create(&mut self, td_id: i32, callback: Box<dyn TdCallback>) {
        debug_assert!(!self.tds.contains_key(&td_id), "double client create");
        // `MultiTd::create` (Client.cpp:246-254): the instance tag
        // (`to_string(td_id)`, the `[#N]` header column) is installed
        // around the `create_actor<Td>` call so the new Td actor's
        // context inherits it, and every actor it later creates inherits
        // it in turn; scheduler-internal actors keep the tag empty.
        let saved_tag = tdutils::logging::current_log_tag();
        tdutils::logging::set_log_tag(tdutils::logging::intern_log_tag(&td_id.to_string()));
        let td = create_actor("Td", Td::new(callback));
        tdutils::logging::set_log_tag(saved_tag);
        self.tds.insert(td_id, td);
    }

    fn send(&mut self, client_id: i32, request_id: u64, request: Function) {
        let td = self.tds.get(&client_id).expect("send to an unknown client");
        td::send_request(td, request_id, request);
    }

    fn close(&mut self, td_id: i32) {
        // ActorOwn drop → hangup → Td tear_down → callback drop → the close
        // notification. Not `debug_assert!` — the remove is the teardown
        // itself and must run in release (the C++ CHECK is release-checked).
        assert!(self.tds.remove(&td_id).is_some(), "close unknown client");
    }
}

// No `loop_`: like the `Td` children, `MultiTd` only reacts to messages
// (a perpetual `yield_()` loop would hot-spin the scheduler pump).
impl Actor for MultiTd {}

/// `Client.cpp:MultiImpl` — a ConcurrentScheduler (3 extra threads) with a
/// scheduler thread pumping `run_main(10)`, hosting one `MultiTd` actor.
pub struct MultiImpl {
    scheduler: Arc<Mutex<ConcurrentScheduler>>,
    send_ptr: SchedulerPtr,
    multi_td: Option<ActorOwn<MultiTd>>,
    scheduler_thread: Option<std::thread::JoinHandle<()>>,
    /// The lock-free shutdown signal (the wake half of
    /// `ConcurrentScheduler::finish`): the pump thread holds the scheduler
    /// Mutex across its whole 10 s `run_main` slice and re-acquires it
    /// immediately, so a shutdown that needs the same Mutex starves on the
    /// gap between slices — the close_synthesizes deadlock. Signalled
    /// lock-free, exactly like the C++ finish + eventfd wakes.
    shutdown: tdactor::ShutdownSignal,
}

/// `MultiImpl::current_id_` — the process-wide client id counter.
static CURRENT_ID: AtomicI32 = AtomicI32::new(1);

impl Default for MultiImpl {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiImpl {
    const ADDITIONAL_THREAD_COUNT: usize = 3;

    pub fn new() -> Self {
        let mut scheduler = ConcurrentScheduler::new(Self::ADDITIONAL_THREAD_COUNT);
        let (multi_td, send_ptr) = {
            let _guard = scheduler.get_main_guard();
            let actor = create_actor("MultiTd", MultiTd::default());
            (actor, scheduler.send_scheduler())
        };
        scheduler.start();
        let shutdown = scheduler.shutdown_signal();
        let scheduler = Arc::new(Mutex::new(scheduler));
        let thread_scheduler = Arc::clone(&scheduler);
        // the run_main pump rides the same 8 MiB actor-thread stack as the
        // scheduler workers (the C++ pthread default; see scheduler.rs)
        let scheduler_thread = std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                // The C++ `while (concurrent_scheduler->run_main(10)) {}` —
                // 10 SECONDS per slice (Client.cpp:695); inbound events wake
                // the idle pump immediately through the wakeup channel. The
                // port first read `10` as 10 ms and spun the Td scheduler at
                // ~100 Hz idle — the 1024-log parity run caught it.
                while thread_scheduler.lock().unwrap().run_main(10.0) {}
            })
            .expect("the scheduler pump thread spawn");
        Self {
            scheduler,
            send_ptr,
            shutdown,
            multi_td: Some(multi_td),
            scheduler_thread: Some(scheduler_thread),
        }
    }

    /// `MultiImpl::create_id`.
    pub fn create_id() -> i32 {
        let result = CURRENT_ID.fetch_add(1, Ordering::Relaxed);
        // A wrapped counter goes non-positive — the C++ CHECK on the uint32
        // counter's range.
        assert!(result > 0, "client id overflow");
        result
    }

    /// `MultiImpl::is_valid_client_id`.
    pub fn is_valid_client_id(client_id: i32) -> bool {
        client_id > 0 && client_id < CURRENT_ID.load(Ordering::Relaxed)
    }

    fn send_closure_to_multi_td(&self, message: MultiTdMessage) {
        let _guard = self.send_ptr.guard(false);
        send_closure(self.multi_td.as_ref().expect("multi_td alive"), message);
    }

    /// `MultiImpl::create`.
    pub fn create(&self, td_id: i32, callback: Box<dyn TdCallback>) {
        self.send_closure_to_multi_td(MultiTdMessage::Create { td_id, callback });
    }

    /// `MultiImpl::send`.
    pub fn send(&self, client_id: i32, request_id: u64, request: Function) {
        self.send_closure_to_multi_td(MultiTdMessage::Send {
            client_id,
            request_id,
            request,
        });
    }

    /// `MultiImpl::close`.
    pub fn close(&self, client_id: i32) {
        self.send_closure_to_multi_td(MultiTdMessage::Close { td_id: client_id });
    }
}

impl Drop for MultiImpl {
    fn drop(&mut self) {
        {
            let _guard = self.send_ptr.guard(false);
            // ActorOwn drop → hangup → MultiTd teardown → each Td's callback
            // destructor posts the close notification.
            self.multi_td.take();
        }
        // Signal the shutdown BEFORE any locked finish: the pump thread
        // holds the scheduler Mutex across its 10 s slice, and the signal
        // ends the woken slice (run_main returns false) so the joins and
        // the locked finish below run uncontended.
        self.shutdown.signal();
        if let Some(thread) = self.scheduler_thread.take() {
            let _ = thread.join();
        }
        self.scheduler.lock().unwrap().finish();
    }
}

/// `Client.cpp:MultiImplPool` — hardware-concurrency-sized pool, least-loaded
/// selection via `Weak` strong counts.
struct MultiImplPool {
    mutex: Mutex<Vec<Weak<MultiImpl>>>,
}

impl Default for MultiImplPool {
    fn default() -> Self {
        Self {
            mutex: Mutex::new(Vec::new()),
        }
    }
}

impl MultiImplPool {
    fn get(&self) -> Arc<MultiImpl> {
        let mut impls = self.mutex.lock().unwrap();
        if impls.is_empty() {
            // `clamp(thread::hardware_concurrency(), 8u, 20u) * 5 / 4`.
            let hardware = std::thread::available_parallelism()
                .map(|count| count.get())
                .unwrap_or(8);
            let max_client_threads = hardware.clamp(8, 20) * 5 / 4;
            impls.resize(max_client_threads, Weak::new());
        }
        let best = impls
            .iter_mut()
            .min_by_key(|weak| weak.strong_count())
            .expect("pool is non-empty");
        if let Some(existing) = best.upgrade() {
            return existing;
        }
        let fresh = Arc::new(MultiImpl::new());
        *best = Arc::downgrade(&fresh);
        fresh
    }
}

/// `ClientManager::Impl` internals.
struct Impl {
    multi_impl: Arc<MultiImpl>,
    impls: RwLock<HashMap<i32, MultiImplInfo>>,
    receiver: TdReceiver,
}

/// `ClientManager::Impl::MultiImplInfo`.
struct MultiImplInfo {
    impl_: Option<Arc<MultiImpl>>,
    is_closed: bool,
}

impl Impl {
    fn new() -> Self {
        // The C++ `ClientManager::Impl` owns its `MultiImpl` for its whole
        // lifetime (`multi_impl_`); the per-client records only reference
        // it. If the last per-client Arc were dropped inline — the
        // close-notification erase in `receive` — the scheduler shutdown
        // and thread join would race the pump that is still delivering the
        // notification (the S81 python-example hang, 1-in-N). The manager
        // holds the strong reference instead.
        let pool = MultiImplPool::default();
        let multi_impl = pool.get();
        Self {
            multi_impl,
            impls: RwLock::new(HashMap::new()),
            receiver: TdReceiver::new(),
        }
    }

    fn create_client_id(&self) -> i32 {
        let client_id = MultiImpl::create_id();
        // The C++ creates the Td actor right here — Client.cpp marks the
        // lazy variant with "TODO: create Td actor here lazily", i.e. the
        // shipped behavior is eager — so the first td_receive on a fresh
        // client id yields the startup updateAuthorizationState without any
        // preceding td_send, which every shipped binding's receive loop
        // expects (the S81 python-example run hangs otherwise).
        let callback = self.receiver.create_callback(client_id);
        let impl_ = Arc::clone(&self.multi_impl);
        impl_.create(client_id, callback);
        self.impls.write().unwrap().insert(
            client_id,
            MultiImplInfo {
                impl_: Some(impl_),
                is_closed: false,
            },
        );
        client_id
    }

    fn send(&self, client_id: i32, request_id: u64, request: Function) {
        {
            let lock = self.impls.read().unwrap();
            if !MultiImpl::is_valid_client_id(client_id) {
                drop(lock);
                self.receiver.add_response(
                    client_id,
                    request_id,
                    Some(synchronous_requests::make_error(
                        400,
                        "Invalid TDLib instance specified",
                    )),
                );
                return;
            }
            if lock
                .get(&client_id)
                .is_some_and(|info| info.impl_.is_none())
            {
                drop(lock);
                let mut write = self.impls.write().unwrap();
                let info = write.get_mut(&client_id).expect("client known");
                if info.impl_.is_none() {
                    let callback = self.receiver.create_callback(client_id);
                    let impl_ = Arc::clone(&self.multi_impl);
                    impl_.create(client_id, callback);
                    info.impl_ = Some(impl_);
                }
                drop(write);
            }
        }
        let lock = self.impls.read().unwrap();
        let Some(info) = lock.get(&client_id) else {
            drop(lock);
            self.receiver.add_response(
                client_id,
                request_id,
                Some(synchronous_requests::make_error(500, "Request aborted")),
            );
            return;
        };
        if info.is_closed {
            drop(lock);
            self.receiver.add_response(
                client_id,
                request_id,
                Some(synchronous_requests::make_error(500, "Request aborted")),
            );
            return;
        }
        info.impl_
            .as_ref()
            .expect("impl created above")
            .send(client_id, request_id, request);
    }

    fn receive(&self, timeout: f64) -> Response {
        let response = self.receiver.receive(timeout, true);
        // A consumed authorizationStateClosed update releases the client
        // silently (the C++ replaces it with a null response).
        if response.request_id == 0
            && matches!(
                &response.object,
                Some(Object::updateAuthorizationState(update))
                    if matches!(
                        update.authorization_state,
                        Some(td_api::AuthorizationState::authorizationStateClosed(_))
                    )
            )
        {
            let mut lock = self.impls.write().unwrap();
            self.close_impl_locked(&mut lock, response.client_id);
            return Response {
                client_id: 0,
                request_id: 0,
                object: None,
            };
        }
        // The close notification (callback destructor) becomes a synthesized
        // authorizationStateClosed update, and the client is erased.
        if response.object.is_none() && response.client_id != 0 && response.request_id == 0 {
            let mut lock = self.impls.write().unwrap();
            let info = lock
                .get(&response.client_id)
                .expect("close notification for a known client");
            assert!(info.is_closed, "unexpected close notification");
            lock.remove(&response.client_id);
            drop(lock);
            return Response {
                client_id: response.client_id,
                request_id: 0,
                object: Some(Object::updateAuthorizationState(
                    td_api::updateAuthorizationState {
                        authorization_state: Some(
                            td_api::AuthorizationState::authorizationStateClosed(
                                td_api::authorizationStateClosed,
                            ),
                        ),
                    },
                )),
            };
        }
        response
    }

    fn close_impl(&self, client_id: i32) {
        let mut lock = self.impls.write().unwrap();
        self.close_impl_locked(&mut lock, client_id);
    }

    fn close_impl_locked(&self, lock: &mut HashMap<i32, MultiImplInfo>, client_id: i32) {
        let info = lock.get_mut(&client_id).expect("closing a known client");
        if !info.is_closed {
            info.is_closed = true;
            if let Some(impl_) = &info.impl_ {
                impl_.close(client_id);
            } else {
                self.receiver.add_response(client_id, 0, None);
            }
        }
    }
}

impl Drop for Impl {
    fn drop(&mut self) {
        // The C++ closes every client and drains the close notifications.
        let client_ids: Vec<i32> = self.impls.read().unwrap().keys().copied().collect();
        for client_id in client_ids {
            self.close_impl(client_id);
        }
        while !self.impls.read().unwrap().is_empty() {
            let _ = self.receive(0.1);
        }
    }
}

/// `td::ClientManager` — the client-id API (the process-wide singleton
/// serves `td_create_client_id`).
pub struct ClientManager {
    impl_: Arc<Impl>,
}

impl Default for ClientManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientManager {
    pub fn new() -> Self {
        Self {
            impl_: Arc::new(Impl::new()),
        }
    }

    /// `ClientManager::create_client_id`.
    pub fn create_client_id(&self) -> i32 {
        self.impl_.create_client_id()
    }

    /// `ClientManager::send`: may be called from any thread.
    pub fn send(&self, client_id: i32, request_id: u64, request: Function) {
        self.impl_.send(client_id, request_id, request);
    }

    /// `ClientManager::receive`: blocks up to `timeout` seconds (clamped to
    /// [0, 1000000]); must not be called from two threads simultaneously.
    pub fn receive(&self, timeout: f64) -> Response {
        self.impl_.receive(timeout)
    }

    /// `ClientManager::execute` — the static synchronous request path.
    pub fn execute(request: Function) -> Object {
        synchronous_requests::run_request(request)
    }

    /// `ClientManager::close(client_id)`.
    pub fn close(&self, client_id: i32) {
        self.impl_.close_impl(client_id);
    }
}

/// The process-wide manager behind the `td_create_client_id` JSON API
/// (`ClientManager::get_manager_singleton`, leaked like the C++).
pub fn get_manager_singleton() -> &'static ClientManager {
    static MANAGER: std::sync::OnceLock<ClientManager> = std::sync::OnceLock::new();
    MANAGER.get_or_init(ClientManager::new)
}

/// The function-local `static MultiImplPool pool` of `Client::Impl` —
/// process-wide, shared by every legacy `Client` (but not with the manager
/// pools).
fn legacy_pool() -> &'static MultiImplPool {
    static POOL: std::sync::OnceLock<MultiImplPool> = std::sync::OnceLock::new();
    POOL.get_or_init(MultiImplPool::default)
}

/// `Client.cpp:Client::Impl` — the legacy `td_json_client_*` path: one
/// process-wide pool, an **eagerly** created Td (the startup state arrives
/// without any request, unlike the manager's lazy clients) and a private
/// receiver. The `request.function == nullptr` half of the C++
/// wrong-request check has no Rust equivalent (`Function` is never null).
struct ClientImpl {
    multi_impl: Arc<MultiImpl>,
    receiver: TdReceiver,
    td_id: i32,
}

impl ClientImpl {
    fn new() -> Self {
        let multi_impl = legacy_pool().get();
        let td_id = MultiImpl::create_id();
        let receiver = TdReceiver::new();
        multi_impl.create(td_id, receiver.create_callback(td_id));
        Self {
            multi_impl,
            receiver,
            td_id,
        }
    }

    /// `Client::Impl::send`.
    fn send(&self, request: Request) {
        if request.id == 0 {
            tdutils::log_error!("Drop wrong request 0");
            return;
        }
        self.multi_impl
            .send(self.td_id, request.id, request.function);
    }

    /// `Client::Impl::receive` — `from_manager = false` (the legacy
    /// concurrent-receive `LOG(FATAL)` message differs).
    fn receive_raw(&self, timeout: f64) -> Response {
        self.receiver.receive(timeout, false)
    }
}

impl Drop for ClientImpl {
    fn drop(&mut self) {
        // `~Client::Impl`: close the Td, then drain until its close
        // notification `{td_id, 0, nullptr}`.
        self.multi_impl.close(self.td_id);
        loop {
            let response = self.receiver.receive(0.1, false);
            if response.object.is_none() && response.client_id != 0 && response.request_id == 0 {
                break;
            }
        }
    }
}

/// `td::Client` — the legacy `td_json_client_*` interface.
pub struct Client {
    impl_: ClientImpl,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Self {
            impl_: ClientImpl::new(),
        }
    }

    /// `Client::send` (legacy).
    pub fn send(&self, request: Request) {
        self.impl_.send(request);
    }

    /// `Client::receive` (legacy): blocks up to `timeout` seconds; `None` on
    /// timeout (the C++ null pointer).
    pub fn receive(&self, timeout: f64) -> Option<Response> {
        let response = self.impl_.receive_raw(timeout);
        response.object.as_ref()?;
        Some(response)
    }

    /// `Client::execute` (legacy): the synchronous path, client-independent.
    pub fn execute(request: Request) -> Object {
        ClientManager::execute(request.function)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version_request() -> Function {
        Function::getOption(td_api::getOption {
            name: "version".to_string(),
        })
    }

    /// Receive up to N responses, returning the ones with objects.
    fn drain(manager: &ClientManager, max: usize) -> Vec<Response> {
        let mut collected = Vec::new();
        for _ in 0..max {
            let response = manager.receive(0.3);
            if response.object.is_some() {
                collected.push(response);
            } else {
                break;
            }
        }
        collected
    }

    #[test]
    fn client_ids_are_process_wide_and_strictly_increasing() {
        let manager = ClientManager::new();
        let first = manager.create_client_id();
        let second = manager.create_client_id();
        assert!(first > 0);
        // Tests run in parallel, so other tests may consume ids in between —
        // only the ordering is guaranteed.
        assert!(second > first);
        // `id > 0 && id < current_id` — every handed-out id is valid; ids at
        // or beyond the counter (never handed out) are not.
        assert!(MultiImpl::is_valid_client_id(first));
        assert!(MultiImpl::is_valid_client_id(second));
        assert!(!MultiImpl::is_valid_client_id(0));
        assert!(!MultiImpl::is_valid_client_id(-5));
        assert!(!MultiImpl::is_valid_client_id(1_000_000));
    }

    #[test]
    fn startup_state_then_synchronous_answers() {
        let manager = ClientManager::new();
        let client_id = manager.create_client_id();
        manager.send(client_id, 7, version_request());
        let responses = drain(&manager, 8);
        // startup: updateOption version, commit_hash, auth state — then the answer.
        assert!(responses.len() >= 4, "got {} responses", responses.len());
        assert!(matches!(
            &responses[0].object,
            Some(Object::updateOption(u)) if u.name == "version"
        ));
        assert!(matches!(
            &responses[2].object,
            Some(Object::updateAuthorizationState(u))
                if matches!(u.authorization_state, Some(td_api::AuthorizationState::authorizationStateWaitTdlibParameters(_)))
        ));
        let answer = responses
            .iter()
            .find(|r| r.request_id == 7)
            .expect("the getOption answer");
        assert_eq!(
            answer.object.as_ref().unwrap().to_json(),
            r#"{"@type":"optionValueString","value":"1.8.67"}"#
        );
    }

    #[test]
    fn invalid_client_ids_are_rejected() {
        let manager = ClientManager::new();
        manager.send(12345, 1, version_request());
        let response = manager.receive(0.3);
        assert_eq!(
            response.object.unwrap().to_json(),
            r#"{"@type":"error","code":400,"message":"Invalid TDLib instance specified"}"#
        );
    }

    #[test]
    fn close_synthesizes_the_closed_update() {
        let manager = ClientManager::new();
        let client_id = manager.create_client_id();
        manager.send(client_id, 1, version_request());
        let _ = drain(&manager, 8);
        manager.close(client_id);
        // The real Closed update is consumed silently by `receive` (it
        // releases the client), the drain must skip the null responses and
        // the trailing Closing update to reach the synthesized one.
        let mut closed = None;
        for _ in 0..8 {
            let response = manager.receive(0.3);
            let is_closed = matches!(
                &response.object,
                Some(Object::updateAuthorizationState(u))
                    if matches!(
                        u.authorization_state,
                        Some(td_api::AuthorizationState::authorizationStateClosed(_))
                    )
            );
            if is_closed {
                closed = Some(response);
                break;
            }
        }
        let closed = closed.expect("the synthesized closed update");
        assert_eq!(closed.client_id, client_id);
        assert_eq!(closed.request_id, 0);
    }

    #[test]
    fn send_after_close_reports_aborted() {
        let manager = ClientManager::new();
        let client_id = manager.create_client_id();
        manager.send(client_id, 1, version_request());
        let _ = drain(&manager, 8);
        manager.close(client_id);
        // Drain through the full close sequence: the real Closing update,
        // the swallowed Closed (a null response that releases the client),
        // and the notification's synthesized closed. Afterwards the client
        // is erased and sends answer 500 "Request aborted" (Client.cpp:515).
        for _ in 0..8 {
            let response = manager.receive(0.3);
            if matches!(
                &response.object,
                Some(Object::updateAuthorizationState(u))
                    if matches!(u.authorization_state, Some(td_api::AuthorizationState::authorizationStateClosed(_)))
            ) {
                break;
            }
        }
        manager.send(client_id, 2, version_request());
        let response = manager.receive(0.3);
        assert_eq!(
            response.object.unwrap().to_json(),
            r#"{"@type":"error","code":500,"message":"Request aborted"}"#
        );
    }

    #[test]
    fn legacy_client_round_trips() {
        let client = Client::new();
        client.send(Request {
            id: 1,
            function: version_request(),
        });
        let mut answer = None;
        // Generous budget: the full-workspace run loads every core, and the
        // pooled scheduler may take a while to pump under contention.
        for _ in 0..60 {
            match client.receive(0.3) {
                Some(response) if response.request_id == 1 => {
                    answer = Some(response);
                    break;
                }
                Some(_) => {} // the eager startup state precedes the answer
                None => break,
            }
        }
        assert_eq!(
            answer
                .expect("the getOption answer")
                .object
                .unwrap()
                .to_json(),
            r#"{"@type":"optionValueString","value":"1.8.67"}"#
        );
        // Drop is `~Client::Impl`: close the Td, drain its close notification.
    }

    #[test]
    fn legacy_client_starts_eagerly_then_times_out() {
        let client = Client::new();
        // The Td exists without any request: the startup state (version,
        // commit_hash, the auth state) arrives on its own.
        let mut updates = Vec::new();
        for _ in 0..4 {
            match client.receive(0.3) {
                Some(response) => updates.push(response),
                None => break,
            }
        }
        assert!(updates.len() >= 3, "got {} startup updates", updates.len());
        assert!(matches!(
            &updates[0].object,
            Some(Object::updateOption(u)) if u.name == "version"
        ));
        // The queue is empty afterwards: the timeout is real.
        let start = std::time::Instant::now();
        assert!(client.receive(0.05).is_none());
        assert!(start.elapsed() >= std::time::Duration::from_millis(40));
    }

    #[test]
    fn manager_multi_client_answers_carry_the_right_client_id() {
        let manager = ClientManager::new();
        let first = manager.create_client_id();
        let second = manager.create_client_id();
        manager.send(first, 11, version_request());
        manager.send(second, 22, version_request());
        let mut seen_first = false;
        let mut seen_second = false;
        for _ in 0..16 {
            let response = manager.receive(0.3);
            let Some(object) = response.object else {
                break;
            };
            if response.request_id == 11 {
                assert_eq!(
                    object.to_json(),
                    r#"{"@type":"optionValueString","value":"1.8.67"}"#
                );
                assert_eq!(response.client_id, first);
                seen_first = true;
            } else if response.request_id == 22 {
                assert_eq!(response.client_id, second);
                seen_second = true;
            }
            if seen_first && seen_second {
                return;
            }
        }
        panic!("seen_first = {seen_first}, seen_second = {seen_second}");
    }

    #[test]
    fn send_to_a_valid_but_unknown_id_is_aborted() {
        let manager = ClientManager::new();
        // Valid per `is_valid_client_id` (< the counter) but never handed to
        // this manager, so it has no `MultiImplInfo` entry — the C++
        // `it == impls_.end()` branch.
        let id = MultiImpl::create_id();
        assert!(MultiImpl::is_valid_client_id(id));
        manager.send(id, 3, version_request());
        let response = manager.receive(0.3);
        assert_eq!(
            response.object.unwrap().to_json(),
            r#"{"@type":"error","code":500,"message":"Request aborted"}"#
        );
    }
}
