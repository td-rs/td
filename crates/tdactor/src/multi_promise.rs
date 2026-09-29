//! Port of `td/actor/MultiPromise.{h,cpp}` (M.4.4): the query fan-in — many
//! `Promise<()>`s completed together once every sub-query has answered, with
//! the first error winning unless errors are ignored.
//!
//! # Mapping notes
//!
//! - The C++ `MultiPromiseActor` is registered lazily (`register_actor(name_,
//!   this)` on the first `get_promise`) and *by pointer* — the object may be
//!   a member of the calling actor (`Deleter::None`). The Rust [`MultiPromiseActor`]
//!   mirrors this: the borrowed registration ([`Scheduler::register_actor_ptr`],
//!   like S35's `Timeout`) happens inside `get_promise`, and the `ActorOwn`
//!   lives inside the actor so that dropping the owning `Box` hangs it up.
//! - C++ `MultiPromiseActorSafe` hands ownership to the scheduler in its
//!   destructor (`register_existing_actor`) so the actor *survives the
//!   handle* and delivers after its futures complete. The Rust port reaches
//!   the same lifecycle differently: [`MultiPromiseActorSafe`] creates the
//!   actor scheduler-owned on first `get_promise` and shares the state
//!   through an `Arc<Mutex<..>>` — the handle never needs to outlive the
//!   actor, and the actor always lives until it stops itself in `raw_event`.
//!   (`MultiPromiseActorSafe::drop` with pending but unregistered promises
//!   drops them, exactly like the C++ unique_ptr delete path.)
//! - The shared state is taken out of the mutex before promises are
//!   completed — the C++ comment "MultiPromiseActor should be cleared before
//!   it begins to send out result", plus Rust re-entrancy hygiene.
//! - The C++ leak of un-drained `FutureActor`s on the error path becomes an
//!   explicit `close()` (their registration must not dangle).

use std::sync::{Arc, Mutex};

use tdutils::promise::Promise;
use tdutils::status::Result;

use crate::actor::Actor;
use crate::actor_id::{ActorId, ActorOwn};
use crate::create_actor;
use crate::event::{Event, Raw};
use crate::promise::{EventFull, FutureBox, PromiseFuture, create_promise_from_promise_actor};
use crate::scheduler::Scheduler;

/// The port of C++ `MultiPromiseInterface`: the fan-in surface shared by the
/// actor and its safe handle.
pub trait MultiPromiseInterface: Send {
    /// Queue one result receiver.
    fn add_promise(&mut self, promise: Promise<()>);
    /// Register interest and get the "lock" promise: completing it counts as
    /// one finished sub-query.
    fn get_promise(&mut self) -> Promise<()>;
    /// Number of queued result receivers.
    fn promise_count(&self) -> usize;
    /// When set, sub-query errors do not fail the fan-in.
    fn set_ignore_errors(&mut self, ignore_errors: bool);
}

/// The registered sub-query futures. `FutureActor`s are scheduler-registered
/// by stable address (see `promise.rs` — "Rust cannot move a registered actor
/// without breaking the registration pointer"), so they must stay boxed
/// inside the vector.
type Futures = Vec<FutureBox<()>>;

/// The state shared between the actor and its safe handle — the C++ object's
/// fields.
struct State {
    promises: Vec<Promise<()>>,
    futures: Futures,
    received_results: usize,
    ignore_errors: bool,
    result: Option<Result<()>>,
}

impl State {
    /// C++ default-constructed fields.
    fn new() -> Self {
        Self {
            promises: Vec::new(),
            futures: Vec::new(),
            received_results: 0,
            ignore_errors: false,
            result: None,
        }
    }

    /// C++ `MultiPromiseActor::add_promise` body.
    fn add_promise(&mut self, promise: Promise<()>) {
        self.promises.push(promise);
    }

    /// C++ `MultiPromiseActor::get_promise` body: pair a promise/future,
    /// subscribe the future to the actor's raw events.
    fn get_promise(&mut self, actor_id: ActorId<MultiPromiseActor>) -> Promise<()> {
        assert!(
            !self.promises.is_empty(),
            "MultiPromiseActor::get_promise without a prior add_promise"
        );

        let mut promise_future = PromiseFuture::<()>::new();
        let promise = promise_future.move_promise();
        let mut future = promise_future.move_future();

        future.set_event(EventFull::new(actor_id.clone().erase(), Event::raw(Raw(0))));
        self.futures.push(future);
        create_promise_from_promise_actor(promise)
    }

    /// C++ `MultiPromiseActor::raw_event` body, split so no lock is held
    /// while futures are drained: returns the finished sub-queries'
    /// `FutureActor`s once all of them fired.
    fn on_raw_event(&mut self) -> (bool, bool, Futures) {
        self.received_results += 1;
        if self.received_results != self.futures.len() {
            return (false, self.ignore_errors, Vec::new());
        }
        let futures = std::mem::take(&mut self.futures);
        (true, self.ignore_errors, futures)
    }

    /// C++ `MultiPromiseActor::tear_down` body: take everything out (the
    /// C++ "cleared before it begins to send out result") for delivery.
    fn take_for_delivery(&mut self) -> (Vec<Promise<()>>, Result<()>) {
        (
            std::mem::take(&mut self.promises),
            self.result.take().unwrap_or(Result::Ok(())),
        )
    }
}

/// Deliver `result` to every promise — clones for all but the last, like the
/// C++ `tear_down`.
fn deliver(promises: Vec<Promise<()>>, result: Result<()>) {
    let mut promises = promises;
    if !promises.is_empty() {
        let last = promises.len() - 1;
        for promise in promises.iter_mut().take(last) {
            promise.set_result(result.clone());
        }
        let mut last_promise = promises.pop().expect("len > 0 above");
        last_promise.set_result(result);
    }
}

/// Drain the fired futures, scanning for the first error (C++ `raw_event`).
/// Drained futures stop themselves (inside `move_as_result`); leftovers are
/// `close()`d so their registration does not dangle (C++ `~FutureActor` did
/// `do_stop`).
fn drain_futures(ignore_errors: bool, futures: &mut [FutureBox<()>]) -> Result<()> {
    let mut outcome = Result::Ok(());
    let mut drained = 0usize;
    if !ignore_errors {
        for future in futures.iter_mut() {
            let result = future.move_as_result();
            drained += 1;
            if let Result::Err(status) = result {
                outcome = Result::Err(status);
                break;
            }
        }
    }
    for future in futures.iter_mut().skip(drained) {
        future.close();
    }
    outcome
}

/// The port of C++ `MultiPromiseActor`: the fan-in actor. Create it as a
/// member/`Box` of the calling actor (the C++ member-actor idiom); it
/// registers itself on the first [`get_promise`](MultiPromiseActor::get_promise),
/// which therefore requires a scheduler context, as in C++.
pub struct MultiPromiseActor {
    name: String,
    state: Arc<Mutex<State>>,
    /// The borrowed self-registration, empty until the first `get_promise`
    /// (the C++ `empty()` check). Held inside the actor: dropping the owning
    /// `Box` sends hangup → stop → `tear_down` delivers the promises.
    own: ActorOwn<MultiPromiseActor>,
}

impl MultiPromiseActor {
    /// C++ `MultiPromiseActor(name)`: no registration side effect.
    pub fn new(name: impl Into<String>) -> Self {
        Self::with_state(name.into(), Arc::new(Mutex::new(State::new())))
    }

    /// The Safe-handle constructor: the actor and the handle share one state.
    fn with_state(name: String, state: Arc<Mutex<State>>) -> Self {
        Self {
            name,
            state,
            own: ActorOwn::default(),
        }
    }

    /// C++ `MultiPromiseActor::empty()` — not registered yet.
    pub fn empty(&self) -> bool {
        self.own.empty()
    }

    pub fn add_promise(&mut self, promise: Promise<()>) {
        self.state.lock().expect("state mutex").add_promise(promise);
    }

    pub fn get_promise(&mut self) -> Promise<()> {
        if self.own.empty() {
            // C++ `register_actor(name_, this)`: first use, by pointer, with
            // a start event (`ActorTraits<MultiPromiseActor>::need_start_up`).
            // SAFETY: `self` lives in the caller's `Box` (stable address) and
            // outlives the registration; dropping the `Box` drops `own`
            // first, which stops and unregisters the actor.
            let name = self.name.clone();
            self.own = unsafe {
                Scheduler::instance().register_actor_ptr(&name, -1, std::ptr::from_mut(self))
            };
        }
        let actor_id = self.own.get();
        self.state
            .lock()
            .expect("state mutex")
            .get_promise(actor_id)
    }

    pub fn set_ignore_errors(&mut self, ignore_errors: bool) {
        self.state.lock().expect("state mutex").ignore_errors = ignore_errors;
    }

    pub fn promise_count(&self) -> usize {
        self.state.lock().expect("state mutex").promises.len()
    }

    /// C++ delivers in `tear_down`; the Rust scheduler skips `tear_down` for
    /// borrowed (pointer-) registrations ("tears down only what started"), so
    /// every destruction entry point finalizes explicitly. Idempotent: the
    /// state is drained on the first delivery (the C++ "cleared before it
    /// begins to send out result").
    fn finalize(&mut self) {
        let (promises, result) = self.state.lock().expect("state mutex").take_for_delivery();
        deliver(promises, result);
    }
}

impl Actor for MultiPromiseActor {
    fn raw_event(&mut self, _raw: Raw) {
        let (done, ignore_errors, mut futures) =
            self.state.lock().expect("state mutex").on_raw_event();
        if !done {
            return;
        }
        let result = drain_futures(ignore_errors, &mut futures);
        self.state.lock().expect("state mutex").result = Some(result);
        self.finalize();
        self.stop();
    }

    /// The owner-`Box`-drop path (the C++ member-actor destruction): complete
    /// the queued promises instead of relying on the skipped `tear_down`.
    fn hangup(&mut self) {
        self.finalize();
        self.stop();
    }

    fn tear_down(&mut self) {
        // Backstop for the owned (Safe) registration, where tear_down runs.
        self.finalize();
    }
}

impl MultiPromiseInterface for MultiPromiseActor {
    fn add_promise(&mut self, promise: Promise<()>) {
        MultiPromiseActor::add_promise(self, promise);
    }

    fn get_promise(&mut self) -> Promise<()> {
        MultiPromiseActor::get_promise(self)
    }

    fn promise_count(&self) -> usize {
        MultiPromiseActor::promise_count(self)
    }

    fn set_ignore_errors(&mut self, ignore_errors: bool) {
        MultiPromiseActor::set_ignore_errors(self, ignore_errors);
    }
}

/// The port of C++ `MultiPromiseActorSafe`: the handle for query fan-ins
/// whose actor must outlive the scope that started them (C++
/// `register_existing_actor` in `~MultiPromiseActorSafe`). The underlying
/// actor is created scheduler-owned on the first `get_promise` (which
/// requires a scheduler context, as in C++) and survives this handle's drop.
pub struct MultiPromiseActorSafe {
    name: String,
    state: Arc<Mutex<State>>,
    actor_id: Option<ActorId<MultiPromiseActor>>,
}

impl MultiPromiseActorSafe {
    /// C++ `MultiPromiseActorSafe(name)`: nothing is created yet.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            state: Arc::new(Mutex::new(State::new())),
            actor_id: None,
        }
    }

    pub fn add_promise(&mut self, promise: Promise<()>) {
        self.state.lock().expect("state mutex").add_promise(promise);
    }

    pub fn get_promise(&mut self) -> Promise<()> {
        if self.actor_id.is_none() {
            // The actor shares this handle's state Arc — the C++ Safe wraps
            // the very object that runs as the actor.
            let own = create_actor(
                &self.name,
                MultiPromiseActor::with_state(self.name.clone(), Arc::clone(&self.state)),
            );
            // `release()`: the scheduler owns the actor — no hangup when this
            // handle drops; the actor lives until it stops itself after the
            // last future fires.
            self.actor_id = Some(own.release());
        }
        let actor_id = self
            .actor_id
            .clone()
            .expect("actor id was just created above");
        self.state
            .lock()
            .expect("state mutex")
            .get_promise(actor_id)
    }

    pub fn set_ignore_errors(&mut self, ignore_errors: bool) {
        self.state.lock().expect("state mutex").ignore_errors = ignore_errors;
    }

    pub fn promise_count(&self) -> usize {
        self.state.lock().expect("state mutex").promises.len()
    }
}

impl Drop for MultiPromiseActorSafe {
    /// C++ `~MultiPromiseActorSafe`: if the actor was never created, queued
    /// promises are simply destroyed (a dropped `LambdaPromise` fires its
    /// "Lost promise" error, matching the C++ promise-destruction path); if
    /// it was, nothing to do — the scheduler owns it and it will deliver.
    fn drop(&mut self) {}
}

impl MultiPromiseInterface for MultiPromiseActorSafe {
    fn add_promise(&mut self, promise: Promise<()>) {
        MultiPromiseActorSafe::add_promise(self, promise);
    }

    fn get_promise(&mut self) -> Promise<()> {
        MultiPromiseActorSafe::get_promise(self)
    }

    fn promise_count(&self) -> usize {
        MultiPromiseActorSafe::promise_count(self)
    }

    fn set_ignore_errors(&mut self, ignore_errors: bool) {
        MultiPromiseActorSafe::set_ignore_errors(self, ignore_errors);
    }
}

/// The port of C++ `class MultiPromise`: type-erased [`MultiPromiseInterface`].
pub struct MultiPromise(Box<dyn MultiPromiseInterface>);

impl MultiPromise {
    /// C++ `MultiPromise(impl)`.
    pub fn new(impl_: Box<dyn MultiPromiseInterface>) -> Self {
        Self(impl_)
    }
}

impl MultiPromiseInterface for MultiPromise {
    fn add_promise(&mut self, promise: Promise<()>) {
        self.0.add_promise(promise);
    }

    fn get_promise(&mut self) -> Promise<()> {
        self.0.get_promise()
    }

    fn promise_count(&self) -> usize {
        self.0.promise_count()
    }

    fn set_ignore_errors(&mut self, ignore_errors: bool) {
        self.0.set_ignore_errors(ignore_errors);
    }
}
