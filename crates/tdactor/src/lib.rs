//! Port of `td/actor/actor.h`: the actor runtime.
//!
//! S31 — the scheduler: one OS thread per scheduler (each running a
//! `current_thread` tokio runtime), per-actor mailboxes, the ready/pending
//! queues, the timeout heap, cross-scheduler routing and actor migration.
//! S32 adds the typed `ActorId<A>`/`ActorOwn<A>`/`ActorShared<A>` markers.
//! S34 adds the promise/future machinery (`PromiseActor`/`FutureActor`/the
//! `send_promise_*` RPC) together with the `immediate_closure` subset of the
//! `send_closure` machinery it needs. S33 completes the machinery with the
//! per-actor message-enum dispatch: declare handlers with
//! [`actor_messages!`] and send with [`send_closure`]/[`send_closure_later`].
//! S35 adds the timeout utilities — the one-shot [`Timeout`], the keyed
//! [`MultiTimeout`] and [`SleepActor`] ([`timeout`]) plus the
//! [`AlarmManager`] ([`alarm_manager`]) — together with the free
//! `send_closure_immediately`/`send_closure_later` sends.
//!
//! ```
//! use std::sync::{Arc, Mutex};
//! use tdactor::{send_event_later, Actor, ConcurrentScheduler, Event, Raw, Scheduler};
//!
//! struct Counter {
//!     hits: Arc<Mutex<usize>>,
//! }
//!
//! impl Actor for Counter {
//!     fn raw_event(&mut self, _raw: Raw) {
//!         *self.hits.lock().unwrap() += 1;
//!         Scheduler::instance().finish();
//!     }
//! }
//!
//! struct Driver {
//!     target: tdactor::ActorId<Counter>,
//! }
//!
//! impl Actor for Driver {
//!     fn start_up(&mut self) {
//!         send_event_later(&self.target, Event::raw(Raw::from(0u32)));
//!     }
//! }
//!
//! let hits = Arc::new(Mutex::new(0usize));
//! let mut sched = ConcurrentScheduler::new(0);
//! let counter = sched.create_actor_unsafe(0, "Counter", Counter { hits: hits.clone() });
//! sched.create_actor_unsafe(0, "Driver", Driver { target: counter.get() });
//! sched.start();
//! while sched.run_main(10.0) {}
//! sched.finish();
//! assert_eq!(*hits.lock().unwrap(), 1);
//! ```

pub mod actor;
pub mod actor_id;
pub mod alarm_manager;
pub mod event;
pub mod messages;
pub mod multi_promise;
pub mod promise;
pub mod scheduler;
pub mod timeout;

pub use actor::Actor;
pub use actor_id::{ActorId, ActorOwn, ActorRef, ActorShared};
pub use alarm_manager::AlarmManager;
pub use event::{Event, EventType, Raw};
pub use messages::{ActorMessage, send_closure, send_closure_later};
pub use multi_promise::{
    MultiPromise, MultiPromiseActor, MultiPromiseActorSafe, MultiPromiseInterface,
};
pub use promise::{
    EventFull, FutureActor, FutureActorState, FutureBox, PromiseActor, PromiseFuture,
    create_event_promise, create_event_promise_ok_fail, create_promise_from_promise_actor,
    promise_send_closure, send_promise_immediately, send_promise_later,
};
pub use scheduler::{ConcurrentScheduler, Scheduler, SchedulerGuard, SchedulerPtr, SchedulerState};
pub use timeout::{MultiTimeout, SleepActor, Timeout};

/// Create an actor on the current scheduler — the free `td::create_actor`.
pub fn create_actor<A: Actor>(name: &str, actor: A) -> ActorOwn<A> {
    Scheduler::instance().create_actor(name, actor)
}

/// Create an actor on the given scheduler — the free
/// `td::create_actor_on_scheduler`.
pub fn create_actor_on_scheduler<A: Actor>(name: &str, sched_id: i32, actor: A) -> ActorOwn<A> {
    Scheduler::instance().create_actor_on_scheduler(name, sched_id, actor)
}

/// Register an actor on the given scheduler (the owning overload) — the free
/// `td::register_actor`.
pub fn register_actor<A: Actor>(name: &str, sched_id: i32, actor: A) -> ActorOwn<A> {
    Scheduler::instance().register_actor(name, sched_id, actor)
}

/// Send an event, running the handler inline when possible — the free
/// `td::send_event` (`Scheduler::send_immediately`).
pub fn send_event<'a>(actor_ref: impl Into<ActorRef<'a>>, event: Event) {
    Scheduler::instance().send_immediately(actor_ref, event);
}
/// Send an event through the mailbox — the free `td::send_event_later`
/// (`Scheduler::send_later`).
pub fn send_event_later<'a>(actor_ref: impl Into<ActorRef<'a>>, event: Event) {
    Scheduler::instance().send_later(actor_ref, event);
}
/// Send a raw thunk inline-when-idle — the free `td::send_closure` half
/// kept for the pre-S33 call sites (`Scheduler::send_closure_immediately`);
/// typed dispatch is [`send_closure`]/[`send_closure_later`] from
/// [`messages`]. Boxed closures are passed through
/// [`ActorThunk::from_boxed`](crate::event::ActorThunk::from_boxed) (see the
/// scheduler method docs for why the parameter is concrete).
pub fn send_thunk_immediately<'a>(
    actor_ref: impl Into<ActorRef<'a>>,
    closure: crate::event::ActorThunk,
) {
    Scheduler::instance().send_closure_immediately(actor_ref.into(), closure);
}

/// Send a raw thunk through the mailbox (`Scheduler::send_closure_later`).
pub fn send_thunk_later<'a>(actor_ref: impl Into<ActorRef<'a>>, closure: crate::event::ActorThunk) {
    Scheduler::instance().send_closure_later(actor_ref.into(), closure);
}

/// Send a lambda, running it inline when the actor is idle — the free
/// `td::send_lambda` (`Scheduler::send_lambda_immediately`): exactly like
/// the C++ template, the closure runs *directly* on the inline path (no
/// allocation, no `Event`); only the queued path wraps it into one.
pub fn send_lambda_immediately<'a, F>(actor_ref: impl Into<ActorRef<'a>>, f: F)
where
    F: for<'x> FnOnce(&'x mut dyn Actor) + Send + 'static,
{
    Scheduler::instance().send_message_immediately(actor_ref.into(), f);
}
