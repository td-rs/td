//! Port of `td/actor/impl/Actor-decl.h` (and the `Actor` half of `Actor.h`):
//! the base trait of every actor, with the scheduler proxies.
//!
//! The C++ class carries its `ActorInfo` (and through it the scheduler
//! context) in a private field; the Rust trait has no fields, so all proxies
//! resolve the current actor through the scheduler's thread-local event
//! context, which is only valid while a handler is running. This is
//! equivalent: an actor's methods only ever run inside `Scheduler::do_event`.

use crate::actor_id::{ActorId, ActorShared};
use crate::event::Raw;
use crate::scheduler::Scheduler;

/// The port of C++ `class Actor`. One method per C++ virtual, keeping the
/// default implementations verbatim (including the `loop()` → `wakeup()` /
/// `timeout_expired()` dispatch).
///
/// # Notes on renames
///
/// - C++ `loop()` → [`loop_`](Actor::loop_) (`loop` is a Rust keyword);
/// - C++ `yield()` → [`yield_`](Actor::yield_) (`yield` is reserved);
/// - C++ `do_stop()` is not exposed: it destroys the actor while its own
///   method runs, which is UB in Rust. Destruction happens in
///   `SchedulerState::destroy_actor` after `tear_down`.
pub trait Actor: Send + 'static {
    /// The port of C++ `start_up`: by default just yields.
    fn start_up(&mut self) {
        self.yield_();
    }

    /// The port of C++ `tear_down`.
    fn tear_down(&mut self) {}

    /// The port of C++ `wakeup`: by default runs `loop_`.
    fn wakeup(&mut self) {
        self.loop_();
    }

    /// The port of C++ `hangup`: by default stops the actor.
    fn hangup(&mut self) {
        self.stop();
    }

    /// The port of C++ `hangup_shared` (a hangup with a non-zero link token):
    /// by default ignored.
    fn hangup_shared(&mut self) {}

    /// The port of C++ `timeout_expired`: by default runs `loop_`.
    fn timeout_expired(&mut self) {
        self.loop_();
    }

    /// The port of C++ `raw_event`.
    fn raw_event(&mut self, _event: Raw) {}

    /// The port of C++ `loop`.
    fn loop_(&mut self) {}

    /// The port of C++ `on_start_migrate`: called on the source scheduler
    /// before the actor is migrated.
    fn on_start_migrate(&mut self, _sched_id: i32) {}

    /// The port of C++ `on_finish_migrate`: called on the target scheduler
    /// after the actor was migrated.
    fn on_finish_migrate(&mut self) {}

    // ---- scheduler proxies (C++ inline methods on Actor) ----

    /// Requeue this actor at the end of the ready queue — the port of
    /// C++ `Actor::yield()` / `Scheduler::yield_actor`.
    fn yield_(&mut self) {
        Scheduler::instance().yield_actor(Scheduler::current_actor());
    }

    /// Request stop after the current event — the port of C++ `Actor::stop()`
    /// / `Scheduler::stop_actor`.
    fn stop(&mut self) {
        Scheduler::instance().stop_actor(Scheduler::current_actor());
    }

    /// Whether this actor has an active timeout.
    fn has_timeout(&self) -> bool {
        Scheduler::instance()
            .get_actor_timeout(Scheduler::current_actor())
            .is_some()
    }

    /// Seconds until the timeout fires, or `0.0` if none is set — the port of
    /// C++ `Scheduler::get_actor_timeout`.
    fn get_timeout(&self) -> f64 {
        Scheduler::instance()
            .get_actor_timeout(Scheduler::current_actor())
            .unwrap_or(0.0)
    }

    /// Set a timeout `timeout` seconds from now (clamped to `[0, 1e10]`) — the
    /// port of C++ `Actor::set_timeout_in`.
    fn set_timeout_in(&mut self, timeout: f64) {
        Scheduler::instance().set_actor_timeout_in(Scheduler::current_actor(), timeout);
    }

    /// Set a timeout at absolute `timeout_at` seconds on the adjusted
    /// monotonic clock (the [`tdutils::time::now`] base) — the port of C++
    /// `Actor::set_timeout_at`. Past values fire on the next pump.
    fn set_timeout_at(&mut self, timeout_at: f64) {
        Scheduler::instance().set_actor_timeout_at(Scheduler::current_actor(), timeout_at);
    }

    /// Cancel the active timeout — the port of C++ `Actor::cancel_timeout`.
    fn cancel_timeout(&mut self) {
        Scheduler::instance().cancel_actor_timeout(Scheduler::current_actor());
    }

    /// Migrate this actor to another scheduler after the current event — the
    /// port of C++ `Actor::migrate` / `Scheduler::migrate_actor`.
    fn migrate(&mut self, sched_id: i32) {
        Scheduler::instance().migrate_actor(Scheduler::current_actor(), sched_id);
    }

    /// The link token of the event currently being dispatched, or `0` — the
    /// port of C++ `Actor::get_link_token`.
    fn get_link_token(&self) -> u64 {
        Scheduler::instance().get_link_token(Scheduler::current_actor())
    }

    /// This actor's own [`ActorId`] (the untyped form) — the port of C++
    /// `Actor::actor_id()`.
    fn actor_id(&self) -> ActorId {
        Scheduler::current_actor()
    }

    /// This actor's own id with the type marker `A` — the port of C++
    /// `Actor::actor_id(this)`, where `this` names the concrete type. `A`
    /// must be the actor's type or one of its supertraits (the marker is
    /// inert; see the `actor_id` module docs). `Self: Sized` keeps the trait
    /// dyn-compatible; inside an actor method `Self` is the concrete type.
    fn actor_id_of<A: ?Sized>(&self) -> ActorId<A>
    where
        Self: Sized,
    {
        Scheduler::current_actor().cast::<A>()
    }

    /// A shared reference to this actor with the given link token — the port
    /// of C++ `Actor::actor_shared(this, token)`.
    fn actor_shared_of<A: ?Sized>(&self, token: u64) -> ActorShared<A>
    where
        Self: Sized,
    {
        ActorShared::new(self.actor_id_of::<A>(), token)
    }

    /// This actor's name — the port of C++ `Actor::get_name()`. Returns an
    /// owned string (the id the name borrows from is thread-local).
    fn get_name(&self) -> String {
        Scheduler::current_actor().get_name().to_owned()
    }
}
