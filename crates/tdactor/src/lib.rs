//! Port of `td/actor/actor.h`: the actor runtime.
//!
//! S31 — the scheduler: one OS thread per scheduler (each running a
//! `current_thread` tokio runtime), per-actor mailboxes, the ready/pending
//! queues, the timeout heap, cross-scheduler routing and actor migration.
//! Typed `ActorId<A>` and the `send_closure` message machinery land with
//! S32/S33; until then actors communicate through [`Raw`] events.
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
//!     target: tdactor::ActorId,
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
pub mod event;
pub mod scheduler;

pub use actor::Actor;
pub use actor_id::{ActorId, ActorOwn, ActorRef, ActorShared};
pub use event::{Event, EventType, Raw};
pub use scheduler::{ConcurrentScheduler, Scheduler, SchedulerGuard, SchedulerPtr, SchedulerState};

/// Create an actor on the current scheduler — the free `td::create_actor`.
pub fn create_actor<A: Actor>(name: &str, actor: A) -> ActorOwn {
    Scheduler::instance().create_actor(name, actor)
}

/// Create an actor on the given scheduler — the free
/// `td::create_actor_on_scheduler`.
pub fn create_actor_on_scheduler<A: Actor>(name: &str, sched_id: i32, actor: A) -> ActorOwn {
    Scheduler::instance().create_actor_on_scheduler(name, sched_id, actor)
}

/// Register an actor on the given scheduler (the owning overload) — the free
/// `td::register_actor`.
pub fn register_actor<A: Actor>(name: &str, sched_id: i32, actor: A) -> ActorOwn {
    Scheduler::instance().register_actor(name, sched_id, actor)
}

/// Send an event, running the handler inline when possible — the free
/// `td::send_event` (`Scheduler::send_immediately`).
pub fn send_event(actor_ref: impl Into<ActorRef>, event: Event) {
    Scheduler::instance().send_immediately(actor_ref, event);
}

/// Send an event through the mailbox — the free `td::send_event_later`
/// (`Scheduler::send_later`).
pub fn send_event_later(actor_ref: impl Into<ActorRef>, event: Event) {
    Scheduler::instance().send_later(actor_ref, event);
}
