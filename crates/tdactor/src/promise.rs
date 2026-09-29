//! Port of `td/actor/PromiseFuture.h`: `EventFull`, the event promise, and
//! the `PromiseActor`/`FutureActor` cross-actor RPC pair behind
//! `PromiseFuture` and `send_promise_immediately`/`send_promise_later`.
//!
//! # Mapping notes
//!
//! - The C++ `FutureActor<T>` is registered with the scheduler **by pointer**
//!   (`register_actor(name, &future)`, `Deleter::None`); the Rust port keeps
//!   it in a `Box` (stable address, movable handle) and registers it through
//!   the scheduler's borrowed-actor path, so [`PromiseFuture`] owns the
//!   object exactly like the C++ value member does.
//! - C++ `move_future()` returns the actor object itself (`FutureActor&&`).
//!   Rust cannot move a registered actor without breaking the registration
//!   pointer, so it returns a pooled handle ([`FutureBox`]) — same ownership,
//!   call surface.
//! - `PromiseActor::set_value`/`set_error` consume the actor id exactly like
//!   C++ (the `ActorRef(ActorOwn&&)` conversion releases it): the promise is
//!   one-shot, and a promise dropped unfulfilled delivers
//!   [`FutureActor::HANGUP_ERROR_CODE`] to the future.
//! - The typed dispatch here is the closure subset of the `send_closure`
//!   machinery (`Event::custom` thunks); the handler downcast goes through
//!   the checked `messages::downcast` (S33), which panics on a lying
//!   `ActorId<A>` marker where C++'s `static_cast` was UB.
//! - `promise_send_closure` takes a closure `FnOnce(&mut A, Result<T>)`
//!   instead of C++ `(method_pointer, args...)` — bound arguments are
//!   captured by the closure.

use tdutils::promise::{Promise, PromiseInterface};
use tdutils::status::{Result, Status};

use crate::actor::Actor;
use crate::actor_id::{ActorId, ActorOwn, ActorRef};
use crate::event::{ActorThunk, Event};
use crate::scheduler::Scheduler;

/// The port of C++ `EventFull`: an event addressed to an actor, with the
/// emit-once semantics (`try_emit`/`try_emit_later` consume it).
pub struct EventFull {
    id: ActorId,
    event: Event,
}

impl EventFull {
    /// C++ `EventFull(actor_id, event)`.
    pub fn new(id: ActorId, event: Event) -> Self {
        Self { id, event }
    }

    /// C++ `empty()` — checks the actor id, exactly like C++.
    pub fn empty(&self) -> bool {
        self.id.empty()
    }

    /// C++ `clear()`.
    pub fn clear(&mut self) {
        self.id = ActorId::empty_id();
        self.event = Event::no_type();
    }

    /// C++ `try_emit()`: send through `send_event` (inline when possible) and
    /// clear, preserving the event's link token.
    pub fn try_emit(&mut self) {
        if self.empty() {
            return;
        }
        let token = self.event.link_token;
        let event = std::mem::replace(&mut self.event, Event::no_type());
        let id = std::mem::replace(&mut self.id, ActorId::empty_id());
        Scheduler::instance().send_immediately(ActorRef::from(id).with_token(token), event);
    }

    /// C++ `try_emit_later()`: the `send_event_later` variant.
    pub fn try_emit_later(&mut self) {
        if self.empty() {
            return;
        }
        let token = self.event.link_token;
        let event = std::mem::replace(&mut self.event, Event::no_type());
        let id = std::mem::replace(&mut self.id, ActorId::empty_id());
        Scheduler::instance().send_later(ActorRef::from(id).with_token(token), event);
    }
}

impl Default for EventFull {
    fn default() -> Self {
        Self {
            id: ActorId::empty_id(),
            event: Event::no_type(),
        }
    }
}

/// The port of C++ `detail::EventPromise`: a `PromiseInterface<()>` that
/// emits one of two [`EventFull`]s (the ok event doubles as the fail event in
/// the one-event form, exactly like the C++ `use_ok_as_fail_` flag).
///
/// Like the C++ class it is non-copyable and its destructor emits the failure
/// side; it lives behind `Box` inside [`Promise`], so it never moves.
pub(crate) struct EventPromise {
    ok: EventFull,
    fail: EventFull,
    use_ok_as_fail: bool,
}

impl EventPromise {
    fn new_ok(ok: EventFull) -> Self {
        Self {
            ok,
            fail: EventFull::default(),
            use_ok_as_fail: true,
        }
    }

    fn new_ok_fail(ok: EventFull, fail: EventFull) -> Self {
        Self {
            ok,
            fail,
            use_ok_as_fail: false,
        }
    }

    fn do_set_error(&mut self) {
        if self.use_ok_as_fail {
            self.ok.try_emit();
        } else {
            self.ok.clear();
            self.fail.try_emit();
        }
    }
}

impl PromiseInterface<()> for EventPromise {
    fn set_value(&mut self, _value: ()) {
        self.ok.try_emit();
        self.fail.clear();
    }

    fn set_error(&mut self, _error: Status) {
        self.do_set_error();
    }
}

impl Drop for EventPromise {
    fn drop(&mut self) {
        self.do_set_error();
    }
}

/// C++ `create_event_promise(ok)`: the ok event is also emitted on error.
pub fn create_event_promise(ok: EventFull) -> Promise<()> {
    Promise::from_interface(EventPromise::new_ok(ok))
}

/// C++ `create_event_promise(ok, fail)`.
pub fn create_event_promise_ok_fail(ok: EventFull, fail: EventFull) -> Promise<()> {
    Promise::from_interface(EventPromise::new_ok_fail(ok, fail))
}

/// C++ `FutureActor::HANGUP_ERROR_CODE`: delivered when the promise is
/// dropped unfulfilled. A plain constant here because `Self::CONST` is not
/// permitted in anonymous constants.
const HANGUP_ERROR_CODE: i32 = 426487;

/// The states of [`FutureActor`] — C++ `FutureActor<T>::State`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FutureActorState {
    Waiting,
    Ready,
}

/// The port of C++ `FutureActor<T>`: a registered actor that receives the
/// result from its [`PromiseActor`] and hands it to the caller synchronously
/// (inline dispatch) or through a subscribed event.
pub struct FutureActor<T: Send + 'static> {
    event: EventFull,
    result: Option<Result<T>>,
    state: FutureActorState,
    /// This actor's own id, for `do_stop` (C++ uses `this`/`get_info()`).
    self_id: ActorId,
}

impl<T: Send + 'static> FutureActor<T> {
    /// C++ `FutureActor::HANGUP_ERROR_CODE`: delivered when the promise is
    /// dropped unfulfilled.
    pub const HANGUP_ERROR_CODE: i32 = HANGUP_ERROR_CODE;

    /// C++ `is_ok()`: ready and holding a value.
    pub fn is_ok(&self) -> bool {
        self.is_ready() && self.result.as_ref().is_some_and(|r| r.is_ok())
    }

    /// C++ `is_error()` — checks readiness first, exactly like C++.
    pub fn is_error(&self) -> bool {
        assert!(self.is_ready(), "is_error() before ready");
        self.is_ready() && self.result.as_ref().is_some_and(|r| r.is_error())
    }

    /// C++ `move_as_ok()` — panics unless ready and successful.
    pub fn move_as_ok(&mut self) -> T {
        self.move_as_result().move_as_ok()
    }

    /// C++ `move_as_error()` — panics unless ready and failed.
    pub fn move_as_error(&mut self) -> Status {
        self.move_as_result().move_as_error()
    }

    /// C++ `move_as_result()`: take the result and stop the actor (the
    /// `SCOPE_EXIT { do_stop(); }`).
    pub fn move_as_result(&mut self) -> Result<T> {
        assert!(self.is_ready(), "move_as_result() before ready");
        let result = self.result.take().expect("ready future without result");
        self.do_stop();
        result
    }

    /// C++ `is_ready()`: the actor is registered (not destroyed) and the
    /// result arrived. `self_id.is_alive()` plays the role of C++
    /// `Actor::empty()` (the info was destroyed by `do_stop`).
    pub fn is_ready(&self) -> bool {
        !self.self_id.empty() && self.self_id.is_alive() && self.state == FutureActorState::Ready
    }

    /// C++ `close()`: clear the event and the result and stop the actor.
    pub fn close(&mut self) {
        self.event.clear();
        self.result = None;
        self.do_stop();
    }

    /// C++ `set_event(event)`: subscribe to readiness — the event is emitted
    /// immediately when the result already arrived.
    pub fn set_event(&mut self, event: EventFull) {
        assert!(self.self_id.is_alive(), "set_event on an empty FutureActor");
        self.event = event;
        if self.state != FutureActorState::Waiting {
            self.event.try_emit_later();
        }
    }

    /// C++ `get_state()`.
    pub fn get_state(&self) -> FutureActorState {
        self.state
    }

    /// C++ `Actor::do_stop()` — `Scheduler::do_stop_actor(this)`: destroy this
    /// actor synchronously. Callable outside this actor's own dispatch.
    fn do_stop(&mut self) {
        Scheduler::instance().do_stop_actor(&self.self_id);
    }

    /// C++ `FutureActor::set_value` — called by [`PromiseActor::set_value`].
    fn set_value(&mut self, value: T) {
        self.set_result(Result::Ok(value));
    }

    /// C++ `FutureActor::set_error` — called by [`PromiseActor::set_error`].
    fn set_error(&mut self, error: Status) {
        self.set_result(Result::Err(error));
    }

    /// C++ `FutureActor::set_result`.
    fn set_result(&mut self, result: Result<T>) {
        assert_eq!(self.state, FutureActorState::Waiting, "double set_result");
        self.result = Some(result);
        self.state = FutureActorState::Ready;
        self.event.try_emit_later();
    }

    fn init(&mut self) {
        debug_assert!(self.self_id.empty(), "re-initializing a FutureActor");
        self.state = FutureActorState::Waiting;
        self.event.clear();
    }
}

impl<T: Send + 'static> Default for FutureActor<T> {
    /// C++ `FutureActor()`: the default `Result` is
    /// `Status::Error(500, "Empty FutureActor")` — unreachable because
    /// readiness gates every accessor, so it maps to `None`.
    fn default() -> Self {
        Self {
            event: EventFull::default(),
            result: None,
            state: FutureActorState::Waiting,
            self_id: ActorId::empty_id(),
        }
    }
}

impl<T: Send + 'static> Actor for FutureActor<T> {
    /// C++ `FutureActor::start_up` — explicitly empty (the default yields).
    fn start_up(&mut self) {}

    /// C++ `FutureActor::hangup`: the promise was dropped unfulfilled.
    fn hangup(&mut self) {
        self.set_error(Status::error_code::<HANGUP_ERROR_CODE>());
    }
}

/// The states of [`PromiseActor`] — C++ `PromiseActor<T>::State`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromiseActorState {
    Waiting,
    Hangup,
}

/// The port of C++ `PromiseActor<T>`: the promise side handed to the callee.
/// Completing it delivers the result to the paired [`FutureActor`]; dropping
/// it unfulfilled hangs the future up with
/// [`FutureActor::HANGUP_ERROR_CODE`].
pub struct PromiseActor<T: Send + 'static> {
    future_id: ActorOwn<FutureActor<T>>,
    state: PromiseActorState,
}

impl<T: Send + 'static> PromiseActor<T> {
    /// C++ `PromiseActor()`.
    pub fn new() -> Self {
        Self::default()
    }

    /// C++ `close()`: `future_id_.reset()` — sends `hangup` unless the id was
    /// already released by `set_value`/`set_error`.
    pub fn close(&mut self) {
        self.future_id = ActorOwn::default();
    }

    /// C++ `is_hangup()`: the future is gone (or the promise was already
    /// released). Once true, no further events are sent.
    pub fn is_hangup(&mut self) -> bool {
        if self.state == PromiseActorState::Hangup {
            return true;
        }
        if !self.future_id.is_alive() {
            self.state = PromiseActorState::Hangup;
            let own = std::mem::take(&mut self.future_id);
            let _ = own.release(); // the future is already gone: no hangup
            return true;
        }
        false
    }

    /// C++ `empty_promise()`.
    pub fn empty_promise(&self) -> bool {
        self.future_id.empty()
    }

    /// C++ `empty()`.
    pub fn empty(&self) -> bool {
        self.future_id.empty()
    }

    /// C++ `PromiseActor::set_value` (also the `PromiseInterface` override;
    /// exposed inherently so callers need no trait import, as in C++).
    pub fn set_value(&mut self, value: T) {
        self.send_to_future(move |future| future.set_value(value));
    }

    /// C++ `PromiseActor::set_error`.
    pub fn set_error(&mut self, error: Status) {
        self.send_to_future(move |future| future.set_error(error));
    }

    fn init(&mut self) {
        self.state = PromiseActorState::Waiting;
    }

    fn send_to_future(&mut self, event_fn: impl FnOnce(&mut FutureActor<T>) + Send + 'static) {
        if self.state == PromiseActorState::Waiting && !self.future_id.empty() {
            // C++ passes std::move(future_id_) into ActorRef(ActorOwn&&),
            // which releases the id — the promise is one-shot.
            let own = std::mem::take(&mut self.future_id);
            let id = own.release();
            Scheduler::instance().send_closure_immediately(
                ActorRef::from(id),
                ActorThunk::new(move |actor: &mut dyn Actor| {
                    // `future_id` is an ActorOwn<FutureActor<T>> (set by
                    // init_promise_future), so the actor behind `id` is a
                    // FutureActor<T>; `downcast` verifies it instead of
                    // trusting the marker the way the C++ static_cast did.
                    let future: &mut FutureActor<T> =
                        crate::messages::downcast::<FutureActor<T>>(actor);
                    event_fn(future);
                }),
            );
        }
    }
}

impl<T: Send + 'static> Default for PromiseActor<T> {
    /// C++ `PromiseActor()`: `state = Hangup`, empty id.
    fn default() -> Self {
        Self {
            future_id: ActorOwn::default(),
            state: PromiseActorState::Hangup,
        }
    }
}

impl<T: Send + 'static> PromiseInterface<T> for PromiseActor<T> {
    fn set_value(&mut self, value: T) {
        PromiseActor::set_value(self, value);
    }

    fn set_error(&mut self, error: Status) {
        PromiseActor::set_error(self, error);
    }
}

impl<T: Send + 'static> Drop for PromiseActor<T> {
    /// C++ `~PromiseActor()`: `close()`.
    fn drop(&mut self) {
        self.close();
    }
}

/// An owning handle to a heap [`FutureActor`] allocated from the
/// scheduler's per-thread [`block_pool`](crate::actor_id::block_pool) —
/// the port of the C++ `ObjectPool` block a stack `FutureActor` occupies.
///
/// The C++ future lives on the caller's stack and its registration holds a
/// raw pointer into it; safe Rust cannot register a movable stack object,
/// so the future needs a stable owned address — a pooled block gives it
/// one at freelist pop/push cost instead of the global malloc/free pair
/// (the S98 immediate-future pass: the plain `Box` was a quarter of the
/// row's remaining gap). Method calls go through `Deref`/`DerefMut`
/// unchanged; `move_future()`/`send_promise_*` hand this around where the
/// C++ moves the actor object itself.
pub struct FutureBox<T: Send + 'static> {
    ptr: std::ptr::NonNull<FutureActor<T>>,
}

// SAFETY: the pointee is owned exclusively by the handle; `T: Send` makes
// moving it across threads sound (the pool handles cross-thread frees).
unsafe impl<T: Send + 'static> Send for FutureBox<T> {}

impl<T: Send + 'static> FutureBox<T> {
    fn new(future: FutureActor<T>) -> Self {
        let layout = std::alloc::Layout::new::<FutureActor<T>>();
        let raw = crate::actor_id::block_pool::alloc(layout);
        assert!(!raw.is_null(), "future-actor pool allocation failed");
        // SAFETY: the block is fresh (`Layout::new::<FutureActor<T>>`-sized
        // and -aligned) and unaliased; the actor is written exactly once.
        unsafe {
            std::ptr::write(raw.cast::<FutureActor<T>>(), future);
            Self {
                ptr: std::ptr::NonNull::new_unchecked(raw.cast()),
            }
        }
    }
}

impl<T: Send + 'static> std::ops::Deref for FutureBox<T> {
    type Target = FutureActor<T>;
    fn deref(&self) -> &FutureActor<T> {
        // SAFETY: the block holds a live `FutureActor<T>` until `Drop`.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: Send + 'static> std::ops::DerefMut for FutureBox<T> {
    fn deref_mut(&mut self) -> &mut FutureActor<T> {
        // SAFETY: same live-actor contract, exclusive through `&mut self`.
        unsafe { self.ptr.as_mut() }
    }
}

impl<T: Send + 'static> Drop for FutureBox<T> {
    fn drop(&mut self) {
        // SAFETY: the actor (if the scheduler has not already destroyed the
        // registration — the actor object itself is owned right here) is
        // dropped exactly once, then the block returns to the pool.
        unsafe {
            std::ptr::drop_in_place(self.ptr.as_ptr());
            crate::actor_id::block_pool::dealloc(
                self.ptr.as_ptr().cast(),
                std::alloc::Layout::new::<FutureActor<T>>(),
            );
        }
    }
}

/// C++ `init_promise_future`: pair the promise and the future and register the
/// future on the current scheduler.
fn init_promise_future<T: Send + 'static>(
    promise: &mut PromiseActor<T>,
    future: &mut FutureActor<T>,
) {
    promise.init();
    future.init();
    // C++ register_actor("FutureActor", future) — by pointer, on the current
    // scheduler, and (ActorTraits<FutureActor<T>>::need_start_up == false)
    // without a start event.
    let own = unsafe {
        Scheduler::instance().register_actor_ptr("FutureActor", -1, future as *mut FutureActor<T>)
    };
    future.self_id = own.get().erase();
    promise.future_id = own;
    debug_assert!(future.self_id.is_alive());
}

/// The port of C++ `PromiseFuture<T>`: a paired [`PromiseActor`] and
/// [`FutureActor`] on the current scheduler.
pub struct PromiseFuture<T: Send + 'static> {
    promise: PromiseActor<T>,
    future: Option<FutureBox<T>>,
}

impl<T: Send + 'static> PromiseFuture<T> {
    /// The C++ constructor: registers the future on the current scheduler
    /// (a scheduler context is required, as in C++). No `Default` impl:
    /// construction has a side effect (actor registration) and requires a
    /// scheduler context, which `Default::default()` could not provide.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let mut future = FutureBox::new(FutureActor::default());
        let mut promise = PromiseActor::new();
        init_promise_future(&mut promise, &mut *future);
        Self {
            promise,
            future: Some(future),
        }
    }

    /// C++ `promise()`.
    pub fn promise(&mut self) -> &mut PromiseActor<T> {
        &mut self.promise
    }

    /// C++ `future()`.
    pub fn future(&mut self) -> &mut FutureActor<T> {
        self.future
            .as_deref_mut()
            .expect("future was already moved out")
    }

    /// C++ `move_promise()`. Unlike the C++ member move this replaces the
    /// member with a fresh (empty, hangup-state) promise — observably
    /// equivalent: the moved-from C++ promise is empty too.
    pub fn move_promise(&mut self) -> PromiseActor<T> {
        std::mem::replace(&mut self.promise, PromiseActor::new())
    }

    /// C++ `move_future()` — see [`FutureBox`]: the registered actor lives
    /// in a pooled block, so the handle moves.
    pub fn move_future(&mut self) -> FutureBox<T> {
        self.future.take().expect("future was already moved out")
    }
}

/// C++ `send_promise_immediately`: send the promise to `actor_id` (inline
/// when possible) and return the paired future (boxed — see the module docs).
/// The handler receives the callee and the promise; extra arguments are
/// captured by the closure (C++ binds them through
/// `create_immediate_closure`).
pub fn send_promise_immediately<T, A, F, R>(actor_id: ActorId<A>, handler: F) -> FutureBox<T>
where
    T: Send + 'static,
    A: Actor,
    F: FnOnce(&mut A, PromiseActor<T>) -> R + Send + 'static,
{
    let mut pf = PromiseFuture::<T>::new();
    let promise = pf.move_promise();
    Scheduler::instance().send_closure_immediately(
        ActorRef::from(actor_id),
        ActorThunk::new(move |actor: &mut dyn Actor| {
            // The ActorId<A> marker names the actor's real type;
            // `downcast` verifies it (the C++ static_cast trusted it).
            let callee = crate::messages::downcast::<A>(actor);
            handler(callee, promise);
        }),
    );
    pf.move_future()
}

/// C++ `send_promise_later`: the mailbox variant of
/// [`send_promise_immediately`].
pub fn send_promise_later<T, A, F, R>(actor_id: ActorId<A>, handler: F) -> FutureBox<T>
where
    T: Send + 'static,
    A: Actor,
    F: FnOnce(&mut A, PromiseActor<T>) -> R + Send + 'static,
{
    let mut pf = PromiseFuture::<T>::new();
    let promise = pf.move_promise();
    Scheduler::instance().send_closure_later(
        ActorRef::from(actor_id),
        ActorThunk::new(move |actor: &mut dyn Actor| {
            // Same as send_promise_immediately: `downcast` verifies.
            let callee = crate::messages::downcast::<A>(actor);
            handler(callee, promise);
        }),
    );
    pf.move_future()
}

/// C++ `promise_send_closure`: a handler that, when given a result, sends it
/// to `actor_id` through `send_closure` (`send_closure_immediately` here).
pub fn promise_send_closure<T, A, F, R>(
    actor_id: ActorId<A>,
    handler: F,
) -> impl FnOnce(Result<T>) + Send + 'static
where
    T: Send + 'static,
    A: Actor,
    F: FnOnce(&mut A, Result<T>) -> R + Send + 'static,
{
    move |result: Result<T>| {
        Scheduler::instance().send_closure_immediately(
            ActorRef::from(actor_id),
            ActorThunk::new(move |actor: &mut dyn Actor| {
                // Same as send_promise_immediately: `downcast` verifies.
                let callee = crate::messages::downcast::<A>(actor);
                handler(callee, result);
            }),
        );
    }
}

/// C++ `create_promise_from_promise_actor`: wrap a [`PromiseActor`] into a
/// plain [`Promise`].
pub fn create_promise_from_promise_actor<T: Send + 'static>(from: PromiseActor<T>) -> Promise<T> {
    Promise::from_interface(from)
}
