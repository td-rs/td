//! Port of `td/actor/impl/Scheduler.{h,cpp}` and `td/actor/ConcurrentScheduler.h`:
//! the actor scheduler.
//!
//! # Mapping to C++
//!
//! - one OS thread per scheduler (C++ spawns one `td::thread` per scheduler;
//!   here each thread runs a `current_thread` tokio runtime, so the single
//!   pump task never migrates threads and the thread-locals below are valid);
//! - scheduler 0 is the *main* scheduler, pumped by the caller through
//!   [`ConcurrentScheduler::run_main`], schedulers `1..=n` run on their own
//!   threads, and the last *send* scheduler exists only so unrelated threads
//!   can obtain a scheduler context via [`ConcurrentScheduler::get_send_guard`]
//!   and route sends through the outbound channels — exactly like C++
//!   `ConcurrentScheduler` + `get_send_guard()`;
//! - actors on the same scheduler communicate through per-actor mailboxes
//!   (`VecDeque<Event>`); cross-scheduler events travel over unbounded tokio
//!   channels, mirroring `MpscPollableQueue<EventFull>` + eventfd wakeups;
//! - [`SchedulerState`] is only ever touched by its own thread (or read
//!   atomically through [`ActorId`] slots from foreign threads), so it uses
//!   interior mutability throughout — there is never a live `&mut` into it
//!   while actor code runs, and actor re-entry (`send` to self, inline
//!   dispatch) is sound.
//!
//! # Known divergences
//!
//! - The C++ timeout queue is a `KHeap` with decrease-key (`fix`); this port
//!   uses a `BinaryHeap` with per-actor timeout generations (lazy deletion).
//!   Semantics are identical; the constant factors are checked at S37.
//! - `ActorContext`/`need_context` (log tags) is not ported yet — it lands
//!   with the logging wiring (S36/S67).
//! - `Event::Custom` carries the immediate-closure subset landed with S34
//!   (`send_closure_immediately`/`send_closure_later` thunks); the
//!   `delayed_closure`/message-enum machinery (S33) is still missing, so
//!   `start_migrate`/`finish_migrate` per-event hooks are currently no-ops.

use std::cell::{Cell, RefCell, UnsafeCell};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;

use crate::actor::Actor;
use crate::actor_id::{ActorId, ActorOwn, ActorRef};
use crate::event::{Event, EventType};

/// The `EventContext::Stop` flag of the C++ scheduler.
const FLAG_STOP: u32 = 1;
/// The `EventContext::Migrate` flag of the C++ scheduler.
const FLAG_MIGRATE: u32 = 2;

/// Time the scheduler polls for when nothing is scheduled — the C++
/// `Timestamp::in(10000)` in `Scheduler::get_timeout`.
const IDLE_POLL: Duration = Duration::from_secs(10);

/// The actor storage behind [`ActorInfo`]: either the scheduler owns the actor
/// (C++ `Deleter::Destroy`) or the caller does (C++ `Deleter::None`, the
/// by-pointer `register_actor` overload — `PromiseFuture`'s `FutureActor`).
pub(crate) enum ActorSlot {
    Empty,
    Owned(Box<dyn Actor>),
    Borrowed(*mut dyn Actor),
}

impl ActorSlot {
    /// `ActorInfo::get_actor_unsafe()`: the live actor, if any.
    fn as_mut(&mut self) -> Option<&mut dyn Actor> {
        match self {
            Self::Empty => None,
            Self::Owned(boxed) => Some(boxed.as_mut()),
            // SAFETY: a borrowed actor is valid until its destruction (the
            // `register_actor_ptr` contract, identical to C++ `Deleter::None`).
            Self::Borrowed(ptr) => {
                if ptr.is_null() {
                    None
                } else {
                    Some(unsafe { &mut **ptr })
                }
            }
        }
    }
}

/// Port of C++ `ActorInfo` (the `mailbox_`, `is_running_` and timeout parts;
/// the atomic `sched_id_` lives in the `IdSlot` shared with [`ActorId`]).
pub(crate) struct ActorInfo {
    pub actor: RefCell<ActorSlot>,
    pub mailbox: RefCell<VecDeque<Event>>,
    pub is_running: Cell<bool>,
    pub in_ready: Cell<bool>,
    pub in_pending: Cell<bool>,
    /// Incremented on every timeout set/cancel; heap entries with a stale
    /// generation are dropped lazily (the `KHeap::fix`/`erase` replacement).
    pub timeout_gen: Cell<u64>,
    /// The live deadline, if a timeout is armed (`in_heap` + heap key in C++).
    pub deadline: Cell<Option<Instant>>,
}

impl ActorInfo {
    fn new(actor: ActorSlot) -> Self {
        Self {
            actor: RefCell::new(actor),
            mailbox: RefCell::new(VecDeque::new()),
            is_running: Cell::new(false),
            in_ready: Cell::new(false),
            in_pending: Cell::new(false),
            timeout_gen: Cell::new(0),
            deadline: Cell::new(None),
        }
    }
}

/// The event context of the currently dispatched event — the port of C++
/// `Scheduler::EventContext` + `event_context_ptr_`.
#[derive(Default)]
struct EventContext {
    actor: Option<ActorId>,
    dest: i32,
    flags: u32,
    link_token: u64,
}

thread_local! {
    /// The scheduler of the current thread (`Scheduler::scheduler_` in C++).
    /// Set by [`SchedulerGuard`]; foreign threads may point it at the send
    /// scheduler, in which case sends always route through the channels.
    static CURRENT_SCHEDULER: Cell<*const SchedulerState> = const { Cell::new(std::ptr::null()) };

    /// The context of the event being dispatched (`event_context_ptr_`).
    static EVENT_CTX: RefCell<EventContext> = const {
        RefCell::new(EventContext {
            actor: None,
            dest: 0,
            flags: 0,
            link_token: 0,
        })
    };
}

/// One heap entry of the timeout queue: the deadline, the actor and the
/// generation under which the timeout was armed.
struct TimeoutEntry(Reverse<Instant>, ActorId, u64);

impl PartialEq for TimeoutEntry {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0 && self.1 == other.1 && self.2 == other.2
    }
}
impl Eq for TimeoutEntry {}
impl PartialOrd for TimeoutEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TimeoutEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .cmp(&other.0)
            .then(self.1.cmp(&other.1))
            .then(self.2.cmp(&other.2))
    }
}

/// Messages crossing scheduler boundaries — the `EventFull` payloads of the
/// C++ `MpscPollableQueue`s.
enum Inbound {
    /// An empty message, used only to wake the pump (C++ `writer_put({})`).
    Wakeup,
    /// The actor with this id has migrated here (C++ `Event::raw(actor_info)`
    /// with an empty actor id).
    Migrate(ActorId),
    /// An event for an actor (C++ `EventFull`).
    Event { id: ActorId, event: Event },
}

/// The state of one scheduler — the port of C++ `class Scheduler`.
///
/// All fields are mutated only by the scheduler's own thread (through shared
/// references + interior mutability; see the module docs) or read by foreign
/// threads through the atomic parts of [`ActorId`] slots.
pub struct SchedulerState {
    pub(crate) sched_id: i32,
    pub(crate) sched_count: i32,
    actor_count: Cell<i32>,
    /// Actors with pending mailbox events, in dispatch order (`ready_actors_`).
    ready: RefCell<VecDeque<ActorId>>,
    /// Idle actors (`pending_actors_`).
    pending: RefCell<HashSet<ActorId>>,
    /// The timeout heap (`timeout_queue_`).
    timeouts: RefCell<BinaryHeap<TimeoutEntry>>,
    /// Events for actors that are migrating to this scheduler but have not
    /// arrived yet (`pending_events_`).
    pending_events: RefCell<HashMap<ActorId, Vec<Event>>>,
    /// Set during teardown; sends after it are dropped. Atomic because the
    /// send-scheduler path reads it from foreign threads (it is only ever
    /// *written* by the owning thread, so the read is race-free in practice,
    /// and the atomic makes it formally so).
    close_flag: AtomicBool,
    yield_flag: Cell<bool>,
    has_guard: Cell<bool>,
    /// One outbound queue per scheduler (this one's id included) —
    /// `outbound_queues_`.
    outbound: Vec<UnboundedSender<Inbound>>,
    /// This scheduler's inbound queue (`inbound_queue_`). Inside an
    /// `UnsafeCell` because the pump holds a mutable receiver reference
    /// across awaits (the scheduler thread runs a single, non-`Send` task on
    /// a `current_thread` runtime, so nothing else can touch it).
    inbound: UnsafeCell<Option<UnboundedReceiver<Inbound>>>,
    /// A clone of this scheduler's own sender, for `wakeup()`.
    wakeup_tx: UnboundedSender<Inbound>,
    /// The main scheduler's inbound sender — `finish()` wakes the main pump so
    /// `run_main` returns promptly (C++ `ConcurrentScheduler::on_finish`).
    main_wakeup: UnboundedSender<Inbound>,
    /// Set by `finish()`; owned by the [`ConcurrentScheduler`].
    finished: Arc<AtomicBool>,
}

impl SchedulerState {
    fn new(
        sched_id: i32,
        sched_count: i32,
        inbound: UnboundedReceiver<Inbound>,
        wakeup_tx: UnboundedSender<Inbound>,
        main_wakeup: UnboundedSender<Inbound>,
        outbound: Vec<UnboundedSender<Inbound>>,
        finished: Arc<AtomicBool>,
    ) -> Self {
        Self {
            sched_id,
            sched_count,
            actor_count: Cell::new(0),
            ready: RefCell::new(VecDeque::new()),
            pending: RefCell::new(HashSet::new()),
            timeouts: RefCell::new(BinaryHeap::new()),
            pending_events: RefCell::new(HashMap::new()),
            close_flag: AtomicBool::new(false),
            yield_flag: Cell::new(false),
            has_guard: Cell::new(false),
            outbound,
            inbound: UnsafeCell::new(Some(inbound)),
            wakeup_tx,
            main_wakeup,
            finished,
        }
    }

    /// `ActorInfo` of a live actor. Only valid on the actor's scheduler thread
    /// (single-threaded access; the same invariant as C++).
    fn info(&self, id: ActorId) -> &ActorInfo {
        unsafe { &*id.info_ptr() }
    }

    // ---- actor registration (Scheduler.h template part) ----

    /// `Scheduler::create_actor`: register on the current scheduler.
    pub fn create_actor<A: Actor>(&self, name: &str, actor: A) -> ActorOwn<A> {
        self.register_actor_impl(name, ActorSlot::Owned(Box::new(actor)), self.sched_id)
    }

    /// `Scheduler::create_actor_on_scheduler`.
    pub fn create_actor_on_scheduler<A: Actor>(
        &self,
        name: &str,
        sched_id: i32,
        actor: A,
    ) -> ActorOwn<A> {
        self.register_actor_impl(name, ActorSlot::Owned(Box::new(actor)), sched_id)
    }

    /// `Scheduler::register_actor` (the `Deleter::Destroy` overload: the
    /// scheduler owns the actor).
    pub fn register_actor<A: Actor>(&self, name: &str, sched_id: i32, actor: A) -> ActorOwn<A> {
        self.register_actor_impl(name, ActorSlot::Owned(Box::new(actor)), sched_id)
    }

    /// `Scheduler::register_actor(name, actor_ptr)` — the C++ `Deleter::None`
    /// overload: the scheduler runs but does not own the actor; the caller
    /// drops it after the actor is destroyed. Like C++
    /// `ActorTraits<FutureActor<T>>::need_start_up = false`, no `Event::start`
    /// is sent for actors registered this way.
    ///
    /// # Safety
    ///
    /// `actor` must point to a valid `A` that outlives the registration: the
    /// scheduler may run the actor's methods through it at any time after this
    /// call, and stops dereferencing it once the actor is destroyed
    /// (`do_stop_actor`, a hangup-driven `stop`, or scheduler teardown).
    pub(crate) unsafe fn register_actor_ptr<A: Actor>(
        &self,
        name: &str,
        sched_id: i32,
        actor: *mut A,
    ) -> ActorOwn<A> {
        self.register_actor_impl(name, ActorSlot::Borrowed(actor as *mut dyn Actor), sched_id)
    }

    fn register_actor_impl<A: Actor>(
        &self,
        name: &str,
        actor: ActorSlot,
        sched_id: i32,
    ) -> ActorOwn<A> {
        // C++ sends Event::start to owned actors only (need_start_up is false
        // for borrowed ones; see `register_actor_ptr`).
        let need_start_up = matches!(actor, ActorSlot::Owned(_));
        assert!(
            self.has_guard.get(),
            "create_actor requires a scheduler guard"
        );
        let sched_id = if sched_id == -1 {
            self.sched_id
        } else {
            sched_id
        };
        assert!(
            sched_id == self.sched_id || (0 <= sched_id && sched_id < self.sched_count),
            "invalid scheduler id {sched_id} (sched_count = {})",
            self.sched_count
        );

        let slot = crate::actor_id::IdSlot::new(Box::new(ActorInfo::new(actor)), sched_id, name);
        // The allocation is type-independent (the "session-unique id" of the
        // plan); the typed `ActorId<A>` is a pure marker added on the way out.
        let id: ActorId = ActorId::new(slot);
        self.actor_count.set(self.actor_count.get() + 1);

        if sched_id != self.sched_id {
            self.send_later(id.clone(), Event::start());
            self.do_migrate_actor(id.clone(), sched_id);
        } else {
            self.pending.borrow_mut().insert(id.clone());
            self.info(id.clone()).in_pending.set(true);
            if need_start_up {
                self.send_later(id.clone(), Event::start());
            }
        }
        ActorOwn::new(id.cast::<A>())
    }

    // ---- sending (Scheduler.cpp send paths) ----

    /// `Scheduler::send_immediately` / free `send_event`: runs the handler
    /// inline when possible, otherwise queues it.
    pub fn send_immediately(&self, actor_ref: impl Into<ActorRef>, event: Event) {
        self.send_immediately_impl(actor_ref.into(), event);
    }

    /// `Scheduler::send_later` / free `send_event_later`: always through the
    /// mailbox.
    pub fn send_later(&self, actor_ref: impl Into<ActorRef>, event: Event) {
        self.send_later_impl(actor_ref.into(), event);
    }

    /// `Scheduler::send_closure_immediately` — the send half of C++
    /// `send_closure`: run the closure inline when the destination is idle
    /// (not running, empty mailbox), otherwise through the mailbox.
    pub(crate) fn send_closure_immediately(
        &self,
        actor_ref: ActorRef,
        closure: crate::event::ActorThunk,
    ) {
        self.send_immediately_impl(actor_ref, Event::custom(closure));
    }

    /// `Scheduler::send_closure_later`: always through the mailbox.
    pub(crate) fn send_closure_later(
        &self,
        actor_ref: ActorRef,
        closure: crate::event::ActorThunk,
    ) {
        self.send_later_impl(actor_ref, Event::custom(closure));
    }

    fn send_later_impl(&self, actor_ref: ActorRef, mut event: Event) {
        event.link_token = actor_ref.token;
        let id = actor_ref.id;
        if id.empty() || !id.is_alive() || self.close_flag.load(Ordering::Relaxed) {
            return;
        }
        let (sched, migrating) = id.sched();
        if sched == self.sched_id {
            if migrating {
                self.pending_events
                    .borrow_mut()
                    .entry(id)
                    .or_default()
                    .push(event);
            } else {
                self.add_to_mailbox(id, event);
            }
        } else {
            let _ = self.outbound[sched as usize].send(Inbound::Event { id, event });
        }
    }

    fn send_immediately_impl(&self, actor_ref: ActorRef, mut event: Event) {
        event.link_token = actor_ref.token;
        let id = actor_ref.id;
        if id.empty() || !id.is_alive() || self.close_flag.load(Ordering::Relaxed) {
            return;
        }
        let (sched, migrating) = id.sched();
        if sched == self.sched_id {
            if !migrating {
                // C++ `get_actor_sched_id_to_send_immediately`: run inline iff
                // not running and the mailbox is empty.
                let can_immediately = {
                    let info = self.info(id.clone());
                    !info.is_running.get() && info.mailbox.borrow().is_empty()
                };
                if can_immediately {
                    self.run_event_guarded(id, event);
                } else {
                    self.add_to_mailbox(id, event);
                }
            } else {
                self.pending_events
                    .borrow_mut()
                    .entry(id)
                    .or_default()
                    .push(event);
            }
        } else {
            let _ = self.outbound[sched as usize].send(Inbound::Event { id, event });
        }
    }

    /// `Scheduler::add_to_mailbox`: queue the event and move an idle actor to
    /// the ready queue.
    fn add_to_mailbox(&self, id: ActorId, event: Event) {
        let info = self.info(id.clone());
        if !info.is_running.get() {
            if info.in_pending.get() {
                self.pending.borrow_mut().remove(&id);
                info.in_pending.set(false);
                self.ready.borrow_mut().push_back(id.clone());
                info.in_ready.set(true);
            } else if !info.in_ready.get() {
                self.ready.borrow_mut().push_back(id.clone());
                info.in_ready.set(true);
            }
        }
        info.mailbox.borrow_mut().push_back(event);
    }

    // ---- dispatch (EventGuard + do_event) ----

    /// `Scheduler::do_event`.
    fn do_event(&self, id: ActorId, event: Event) {
        EVENT_CTX.with(|ctx| ctx.borrow_mut().link_token = event.link_token);
        let info = self.info(id);
        let mut slot = info.actor.borrow_mut();
        let actor = slot.as_mut().expect("dispatched to a dead actor");
        match event.ty {
            EventType::Start => actor.start_up(),
            EventType::Stop => actor.tear_down(),
            EventType::Yield => actor.wakeup(),
            EventType::Timeout => actor.timeout_expired(),
            EventType::Hangup => {
                if event.link_token != 0 {
                    actor.hangup_shared();
                } else {
                    actor.hangup();
                }
            }
            EventType::Raw => actor.raw_event(crate::event::Raw(event.raw)),
            EventType::Custom => {
                let closure = event.custom.expect("custom event without payload");
                closure(actor)
            }
            EventType::NoType => unreachable!("empty event dispatched"),
        }
    }

    /// The beginning of the C++ `EventGuard`: marks the actor running and
    /// installs the event context.
    fn enter_actor(&self, id: ActorId) {
        let info = self.info(id.clone());
        info.is_running.set(true);
        self.remove_from_lists(id.clone());
        EVENT_CTX.with(|ctx| {
            let mut ctx = ctx.borrow_mut();
            ctx.actor = Some(id);
            ctx.dest = 0;
            ctx.flags = 0;
            ctx.link_token = 0;
        });
    }

    /// The end of the C++ `EventGuard` destructor: re-queues the actor and
    /// honors the `Stop`/`Migrate` flags set during the run.
    fn exit_actor(&self, id: ActorId) {
        let info = self.info(id.clone());
        let mailbox_empty = info.mailbox.borrow().is_empty();
        if mailbox_empty {
            self.pending.borrow_mut().insert(id.clone());
            info.in_pending.set(true);
        } else {
            self.ready.borrow_mut().push_back(id.clone());
            info.in_ready.set(true);
        }
        info.is_running.set(false);
        let (flags, dest) = EVENT_CTX.with(|ctx| {
            let ctx = ctx.borrow();
            (ctx.flags, ctx.dest)
        });
        EVENT_CTX.with(|ctx| {
            let mut ctx = ctx.borrow_mut();
            ctx.actor = None;
            ctx.flags = 0;
            ctx.link_token = 0;
        });
        if flags & FLAG_STOP != 0 {
            self.do_stop_actor(id);
        } else if flags & FLAG_MIGRATE != 0 {
            self.do_migrate_actor(id, dest);
        }
    }

    /// A single-event inline dispatch — the C++ `EventGuard` around
    /// `send_immediately_impl`'s inline path.
    fn run_event_guarded(&self, id: ActorId, event: Event) {
        self.enter_actor(id.clone());
        self.do_event(id.clone(), event);
        self.exit_actor(id);
    }

    /// `Scheduler::flush_mailbox`: drain the mailbox under one guard, until it
    /// is empty or the actor asked to stop/migrate.
    fn flush_mailbox(&self, id: ActorId) {
        let info = self.info(id.clone());
        let mailbox_len = info.mailbox.borrow().len();
        debug_assert_ne!(mailbox_len, 0, "flushing an empty mailbox");
        self.enter_actor(id.clone());
        let mut processed = 0;
        while processed < mailbox_len {
            let flags = EVENT_CTX.with(|ctx| ctx.borrow().flags);
            if flags != 0 {
                break;
            }
            let event = info
                .mailbox
                .borrow_mut()
                .pop_front()
                .expect("mailbox emptied under guard");
            processed += 1;
            self.do_event(id.clone(), event);
        }
        self.exit_actor(id);
    }

    // ---- lifecycle (do_stop_actor, destroy, migration) ----

    /// `Scheduler::do_stop_actor` — the path behind C++ `Actor::do_stop()`:
    /// run `tear_down` and destroy the actor synchronously, callable from any
    /// actor on this scheduler (unlike `stop()`, which only flags the
    /// currently dispatched actor). The C++ version wraps the teardown in its
    /// own `EventGuard` whose destructor restores the outer event context;
    /// the whole context is saved and restored here.
    ///
    /// C++ would crash on a dead actor; Rust returns silently in release and
    /// asserts in debug. Destroying the currently dispatched actor (`do_stop`
    /// on `self`) is UB in Rust and rejected.
    pub(crate) fn do_stop_actor(&self, id: ActorId) {
        debug_assert!(!id.sched().1, "stopping a migrating actor");
        if !id.is_alive() {
            debug_assert!(false, "do_stop_actor on a dead actor");
            return;
        }
        debug_assert!(
            EVENT_CTX.with(|ctx| ctx.borrow().actor.as_ref() != Some(&id)),
            "do_stop_actor on the dispatched actor is UB in Rust (use stop())"
        );
        let saved = EVENT_CTX.with(|ctx| ctx.replace(EventContext::default()));
        // The C++ keeps its EventGuard (is_running = true) across the
        // destruction: sends to this actor from its own teardown/Drop land in
        // the mailbox (cleared by destroy_actor) instead of dispatching
        // inline. destroy_actor frees the ActorInfo, so afterwards only the
        // context restore remains (no list fixup — the actor is unlinked).
        self.enter_actor(id.clone());
        self.do_event(id.clone(), Event::stop());
        self.destroy_actor(id.clone());
        EVENT_CTX.with(|ctx| {
            *ctx.borrow_mut() = saved;
        });
    }

    /// `Scheduler::destroy_actor` + the pool release: drop the actor and free
    /// its `ActorInfo` after publishing death.
    fn destroy_actor(&self, id: ActorId) {
        let info_ptr = id.info_ptr();
        self.remove_from_lists(id.clone());
        self.actor_count.set(self.actor_count.get() - 1);
        debug_assert!(self.actor_count.get() >= 0);
        let info = unsafe { &*info_ptr };
        info.timeout_gen.set(info.timeout_gen.get() + 1);
        info.deadline.set(None);
        // Drop the actor first (C++ destroys the actor before the mailbox).
        // An Owned box is freed here; a Borrowed actor (Deleter::None) is
        // only unregistered — its owner drops it.
        let _ = std::mem::replace(&mut *info.actor.borrow_mut(), ActorSlot::Empty);
        info.mailbox.borrow_mut().clear();
        id.mark_dead();
        // SAFETY: `mark_dead` published alive=false; no thread dereferences
        // the ActorInfo after that, so freeing is sound.
        unsafe {
            drop(Box::from_raw(info_ptr));
        }
    }

    /// `Scheduler::migrate_actor`: record the request; applied after the
    /// current event by `exit_actor`.
    pub(crate) fn migrate_actor(&self, id: ActorId, dest: i32) {
        if dest == self.sched_id {
            return;
        }
        EVENT_CTX.with(|ctx| {
            let mut ctx = ctx.borrow_mut();
            debug_assert_eq!(ctx.actor.as_ref(), Some(&id), "migrate outside of dispatch");
            ctx.flags |= FLAG_MIGRATE;
            ctx.dest = dest;
        });
    }

    /// `Scheduler::do_migrate_actor`.
    fn do_migrate_actor(&self, id: ActorId, dest: i32) {
        if dest == self.sched_id {
            return;
        }
        self.start_migrate_actor(id.clone(), dest);
        let _ = self.outbound[dest as usize].send(Inbound::Migrate(id));
    }

    /// `Scheduler::start_migrate_actor`.
    fn start_migrate_actor(&self, id: ActorId, dest: i32) {
        self.actor_count.set(self.actor_count.get() - 1);
        {
            let info = self.info(id.clone());
            info.actor
                .borrow_mut()
                .as_mut()
                .expect("migrating a dead actor")
                .on_start_migrate(dest);
            // C++ also calls start_migrate() on every mailbox event; a no-op
            // until custom events (S33) carry migration hooks.
        }
        id.start_migrate(dest);
        self.remove_from_lists(id.clone());
        let info = self.info(id.clone());
        info.timeout_gen.set(info.timeout_gen.get() + 1);
        info.deadline.set(None);
    }

    /// `Scheduler::register_migrated_actor`.
    fn register_migrated_actor(&self, id: ActorId) {
        self.actor_count.set(self.actor_count.get() + 1);
        let (dest, migrating) = id.sched();
        debug_assert!(
            migrating && dest == self.sched_id,
            "registering a non-migrating actor"
        );
        id.finish_migrate();
        if let Some(events) = self.pending_events.borrow_mut().remove(&id) {
            self.info(id.clone()).mailbox.borrow_mut().extend(events);
        }
        let info = self.info(id.clone());
        let mailbox_empty = info.mailbox.borrow().is_empty();
        if mailbox_empty {
            self.pending.borrow_mut().insert(id.clone());
            info.in_pending.set(true);
        } else {
            self.ready.borrow_mut().push_back(id.clone());
            info.in_ready.set(true);
        }
        info.actor
            .borrow_mut()
            .as_mut()
            .expect("migrated a dead actor")
            .on_finish_migrate();
    }

    /// Unlink the actor from the ready/pending lists.
    fn remove_from_lists(&self, id: ActorId) {
        let info = self.info(id.clone());
        if info.in_pending.get() {
            self.pending.borrow_mut().remove(&id);
            info.in_pending.set(false);
        }
        if info.in_ready.get() {
            // O(n) where the C++ intrusive list is O(1); revisit at S37.
            self.ready.borrow_mut().retain(|x| *x != id);
            info.in_ready.set(false);
        }
    }

    // ---- actor proxies called from the Actor trait ----

    pub(crate) fn yield_actor(&self, id: ActorId) {
        self.send_later(id, Event::yield_());
    }

    pub(crate) fn stop_actor(&self, id: ActorId) {
        EVENT_CTX.with(|ctx| {
            let mut ctx = ctx.borrow_mut();
            debug_assert_eq!(ctx.actor.as_ref(), Some(&id), "stop outside of dispatch");
            ctx.flags |= FLAG_STOP;
        });
    }

    pub(crate) fn get_link_token(&self, id: ActorId) -> u64 {
        EVENT_CTX.with(|ctx| {
            let ctx = ctx.borrow();
            debug_assert_eq!(
                ctx.actor.as_ref(),
                Some(&id),
                "get_link_token outside of dispatch"
            );
            ctx.link_token
        })
    }

    // ---- timeouts ----

    pub(crate) fn get_actor_timeout(&self, id: ActorId) -> Option<f64> {
        let info = self.info(id);
        info.deadline.get().map(|at| {
            let now = Instant::now();
            if at <= now {
                0.0
            } else {
                (at - now).as_secs_f64()
            }
        })
    }

    pub(crate) fn set_actor_timeout_in(&self, id: ActorId, timeout: f64) {
        // C++ clamps to [0, 1e10] then converts to an absolute time.
        self.set_actor_timeout_at(id, timeout.clamp(0.0, 1e10));
    }

    pub(crate) fn set_actor_timeout_at(&self, id: ActorId, timeout_at: f64) {
        let info = self.info(id.clone());
        let generation = info.timeout_gen.get() + 1;
        info.timeout_gen.set(generation);
        let at = Instant::now() + Duration::from_secs_f64(timeout_at.max(0.0));
        info.deadline.set(Some(at));
        self.timeouts
            .borrow_mut()
            .push(TimeoutEntry(Reverse(at), id, generation));
    }

    pub(crate) fn cancel_actor_timeout(&self, id: ActorId) {
        let info = self.info(id.clone());
        if info.deadline.get().is_some() {
            info.timeout_gen.set(info.timeout_gen.get() + 1);
            info.deadline.set(None);
        }
    }

    /// The deadline of the next live timeout, dropping stale heap entries —
    /// the live part of C++ `Scheduler::get_timeout`.
    fn next_timeout(&self) -> Option<Instant> {
        let mut heap = self.timeouts.borrow_mut();
        while let Some(TimeoutEntry(Reverse(at), id, generation)) = heap.peek() {
            let info = self.info(id.clone());
            if info.timeout_gen.get() == *generation {
                return Some(*at);
            }
            heap.pop();
        }
        None
    }

    // ---- the event pump ----

    fn run_mailbox(&self) {
        let ready: Vec<ActorId> = self.ready.borrow_mut().drain(..).collect();
        for id in ready {
            self.flush_mailbox(id);
        }
    }

    fn run_timeout(&self) {
        let now = Instant::now();
        loop {
            let entry = {
                let mut heap = self.timeouts.borrow_mut();
                match heap.peek() {
                    Some(TimeoutEntry(Reverse(at), ..)) if *at <= now => heap.pop(),
                    _ => None,
                }
            };
            let Some(TimeoutEntry(_, id, generation)) = entry else {
                break;
            };
            let info = self.info(id.clone());
            if info.timeout_gen.get() == generation {
                info.deadline.set(None);
                self.send_immediately_impl(ActorRef::from(id), Event::timeout());
            }
        }
    }

    /// `Scheduler::run_events`.
    fn run_events(&self, deadline: Instant) {
        loop {
            self.run_mailbox();
            self.run_timeout();
            if self.ready.borrow().is_empty() || Instant::now() >= deadline {
                break;
            }
        }
    }

    fn process_inbound(&self, items: Vec<Inbound>) {
        for item in items {
            match item {
                Inbound::Wakeup => {}
                Inbound::Migrate(id) => {
                    if !id.is_alive() {
                        continue;
                    }
                    let (dest, migrating) = id.sched();
                    if migrating && dest == self.sched_id {
                        self.register_migrated_actor(id);
                    }
                }
                Inbound::Event { id, event } => {
                    if !id.is_alive() {
                        continue;
                    }
                    let (sched, migrating) = id.sched();
                    if sched == self.sched_id {
                        if migrating {
                            self.pending_events
                                .borrow_mut()
                                .entry(id)
                                .or_default()
                                .push(event);
                        } else {
                            // The C++ EventFull::try_emit preserves the event's
                            // link token; so does this.
                            let token = event.link_token;
                            self.send_immediately_impl(ActorRef::from(id).with_token(token), event);
                        }
                    } else {
                        let _ = self.outbound[sched as usize].send(Inbound::Event { id, event });
                    }
                }
            }
        }
        // The C++ ServiceActor::loop ends with yield().
        self.yield_flag.set(true);
    }

    /// One iteration of `Scheduler::run(timeout)`: process events, then wait
    /// until either inbound traffic or the earlier of the deadline and the
    /// next timeout, then process events again.
    pub(crate) async fn run_once(&self, deadline: Instant) {
        self.yield_flag.set(false);
        self.run_events(deadline);
        if !self.yield_flag.get() {
            let wait = self
                .next_timeout()
                .unwrap_or_else(|| Instant::now() + IDLE_POLL)
                .min(deadline);
            tokio::select! {
                biased;
                item = self.recv_inbound() => self.handle_inbound(item),
                _ = tokio::time::sleep_until(wait) => {}
            }
            self.run_events(deadline);
        }
    }

    /// Await the next inbound message, returning it (it is handed to
    /// [`handle_inbound`](Self::handle_inbound) together with whatever else
    /// arrived meanwhile).
    async fn recv_inbound(&self) -> Option<Inbound> {
        // SAFETY: only the pump task of this scheduler's own thread touches
        // the receiver (see the field docs).
        let inbound = unsafe { &mut *self.inbound.get() };
        inbound
            .as_mut()
            .expect("inbound receiver missing")
            .recv()
            .await
    }

    /// Collect the freshly received message plus everything already queued,
    /// then deliver all of them.
    fn handle_inbound(&self, first: Option<Inbound>) {
        let mut items = Vec::new();
        if let Some(item) = first {
            items.push(item);
        }
        {
            // SAFETY: same as `recv_inbound`.
            let inbound = unsafe { &mut *self.inbound.get() };
            let inbound = inbound.as_mut().expect("inbound receiver missing");
            while let Ok(item) = inbound.try_recv() {
                items.push(item);
            }
        }
        self.process_inbound(items);
    }

    /// `Scheduler::finish`: ask the owner to stop running; wakes up the main
    /// pump (C++ `callback_->on_finish()`).
    pub fn finish(&self) {
        self.finished.store(true, Ordering::Relaxed);
        let _ = self.main_wakeup.send(Inbound::Wakeup);
    }

    /// `Scheduler::wakeup`: wake the pump from any thread.
    pub fn wakeup(&self) {
        let _ = self.wakeup_tx.send(Inbound::Wakeup);
    }

    /// `Scheduler::yield`: stop processing mailboxes and go back to polling.
    pub fn yield_(&self) {
        self.yield_flag.set(true);
    }

    /// `Scheduler::clear`: stop every remaining actor (runs `tear_down`).
    fn clear(&self) {
        if self.close_flag.swap(true, Ordering::AcqRel) {
            return;
        }
        loop {
            let next = if let Some(id) = self.ready.borrow_mut().pop_front() {
                self.info(id.clone()).in_ready.set(false);
                Some(id)
            } else {
                let next_pending = self.pending.borrow().iter().next().cloned();
                if let Some(id) = next_pending.as_ref() {
                    self.pending.borrow_mut().remove(id);
                    self.info(id.clone()).in_pending.set(false);
                }
                next_pending
            };
            match next {
                Some(id) => self.do_stop_actor(id),
                None => break,
            }
        }
    }
}

impl Drop for SchedulerState {
    fn drop(&mut self) {
        self.clear();
    }
}

/// The current-thread scheduler facade — the port of the static half of C++
/// `Scheduler` (`instance()`, `set_scheduler`, `context()`).
pub struct Scheduler;

impl Scheduler {
    /// `Scheduler::instance()`: the scheduler of the current thread. Panics
    /// outside any scheduler context (C++ would crash).
    pub fn instance() -> &'static SchedulerState {
        Self::try_instance().expect("no scheduler in the current thread context")
    }

    pub(crate) fn try_instance() -> Option<&'static SchedulerState> {
        CURRENT_SCHEDULER.with(|current| {
            let ptr = current.get();
            if ptr.is_null() {
                None
            } else {
                Some(unsafe { &*ptr })
            }
        })
    }

    /// The actor currently being dispatched, for the `Actor` trait proxies.
    pub(crate) fn current_actor() -> ActorId {
        EVENT_CTX.with(|ctx| {
            ctx.borrow()
                .actor
                .clone()
                .expect("no actor is being dispatched")
        })
    }
}

/// Sets the thread-local scheduler for a scope — the port of C++
/// `SchedulerGuard` (locking) / `get_const_guard` (non-locking).
pub struct SchedulerGuard {
    prev: *const SchedulerState,
    locked_state: Option<*const SchedulerState>,
}

impl SchedulerGuard {
    pub(crate) fn new(state: &SchedulerState, lock: bool) -> Self {
        Self::from_raw(state as *const SchedulerState, lock)
    }

    pub(crate) fn from_raw(ptr: *const SchedulerState, lock: bool) -> Self {
        let state = unsafe { &*ptr };
        let locked_state = if lock {
            assert!(!state.has_guard.get(), "scheduler is already guarded");
            state.has_guard.set(true);
            Some(ptr)
        } else {
            None
        };
        let prev = CURRENT_SCHEDULER.with(|current| current.replace(ptr));
        Self { prev, locked_state }
    }
}

impl Drop for SchedulerGuard {
    fn drop(&mut self) {
        CURRENT_SCHEDULER.with(|current| current.set(self.prev));
        if let Some(ptr) = self.locked_state {
            let state = unsafe { &*ptr };
            state.has_guard.set(false);
        }
    }
}

/// A cheap, thread-safe handle to a scheduler, for building a
/// [`SchedulerGuard`] on another thread — the send side of C++
/// `ConcurrentScheduler::get_send_guard()`.
///
/// It carries only a raw pointer; all cross-thread access through it goes to
/// immutable fields, atomics, or channel senders.
#[derive(Clone, Copy)]
pub struct SchedulerPtr(*const SchedulerState);

// SAFETY: a SchedulerPtr is only used to create guards, which only read
// immutable fields and atomics of the state (see the module docs); it never
// dereferences the non-Sync interior-mutable parts from a foreign thread.
unsafe impl Send for SchedulerPtr {}
unsafe impl Sync for SchedulerPtr {}

impl SchedulerPtr {
    /// Build a guard for the current thread — `lock = false` for the send
    /// scheduler (the C++ `get_const_guard`).
    pub fn guard(self, lock: bool) -> SchedulerGuard {
        SchedulerGuard::from_raw(self.0, lock)
    }
}

/// The port of C++ `ConcurrentScheduler`: `n` schedulers on their own
/// threads, scheduler 0 pumped by the caller via [`run_main`](Self::run_main),
/// plus the send scheduler for unrelated threads.
pub struct ConcurrentScheduler {
    /// Schedulers `1..=n`; scheduler 0 is kept separate as `main`.
    schedulers: Vec<SchedulerState>,
    main: Box<SchedulerState>,
    send_state: Box<SchedulerState>,
    runtime: tokio::runtime::Runtime,
    threads: Vec<std::thread::JoinHandle<()>>,
    finished: Arc<AtomicBool>,
    state: Cell<ConcurrentSchedulerState>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ConcurrentSchedulerState {
    Start,
    Run,
}

impl ConcurrentScheduler {
    /// The port of the C++ constructor: build the schedulers and their
    /// outbound/inbound queues. `additional_thread_count` is the number of
    /// non-main schedulers (the C++ `threads_n - 1`).
    pub fn new(additional_thread_count: usize) -> Self {
        let sched_count = additional_thread_count + 2; // n+1 real schedulers + the send one
        let finished = Arc::new(AtomicBool::new(false));

        // The main channel is created first: every scheduler's `finish()`
        // wakes the main pump.
        // Build every queue first: C++ passes the same full `outbound` vector
        // to `Scheduler::init` for every scheduler, so each one can route to
        // all others.
        let (main_tx, main_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut outbound = Vec::with_capacity(additional_thread_count + 1);
        let mut inbounds = Vec::with_capacity(additional_thread_count + 1);
        outbound.push(main_tx.clone());
        inbounds.push(main_rx);
        for _ in 1..=additional_thread_count {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            outbound.push(tx.clone());
            inbounds.push(rx);
        }
        let mut schedulers = Vec::with_capacity(additional_thread_count);
        for i in 0..=additional_thread_count {
            let tx = outbound[i].clone();
            schedulers.push(SchedulerState::new(
                i as i32,
                (additional_thread_count + 1) as i32,
                inbounds.remove(0),
                tx,
                main_tx.clone(),
                outbound.clone(),
                finished.clone(),
            ));
        }
        // The send scheduler: knows about every other scheduler, nothing knows
        // about it, and it never runs. C++ pushes its queue after the regular
        // schedulers were initialized, so only it sees the full outbound list.
        let (send_tx, send_rx) = tokio::sync::mpsc::unbounded_channel();
        outbound.push(send_tx.clone());
        let send_state = Box::new(SchedulerState::new(
            sched_count as i32 - 1,
            sched_count as i32,
            send_rx,
            send_tx,
            main_tx,
            outbound,
            finished.clone(),
        ));
        let main = Box::new(schedulers.remove(0));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("failed to build the main scheduler runtime");

        Self {
            schedulers,
            main,
            send_state,
            runtime,
            threads: Vec::new(),
            finished,
            state: Cell::new(ConcurrentSchedulerState::Start),
        }
    }

    /// `ConcurrentScheduler::create_actor_unsafe`: create an actor on any
    /// scheduler, before [`start`](Self::start).
    pub fn create_actor_unsafe<A: Actor>(
        &self,
        sched_id: i32,
        name: &str,
        actor: A,
    ) -> ActorOwn<A> {
        self.register_actor_unsafe(sched_id, name, actor)
    }

    /// `ConcurrentScheduler::register_actor_unsafe`.
    pub fn register_actor_unsafe<A: Actor>(
        &self,
        sched_id: i32,
        name: &str,
        actor: A,
    ) -> ActorOwn<A> {
        let state = self.state_for(sched_id);
        let _guard = SchedulerGuard::new(state, true);
        state.create_actor(name, actor)
    }

    fn state_for(&self, sched_id: i32) -> &SchedulerState {
        if sched_id == 0 {
            &self.main
        } else if sched_id >= 1 && (sched_id as usize) <= self.schedulers.len() {
            &self.schedulers[sched_id as usize - 1]
        } else {
            assert_eq!(
                sched_id, self.send_state.sched_id,
                "invalid scheduler id {sched_id}"
            );
            &self.send_state
        }
    }

    /// `ConcurrentScheduler::start`: spawn the non-main scheduler threads.
    pub fn start(&mut self) {
        assert_eq!(
            self.state.get(),
            ConcurrentSchedulerState::Start,
            "start called twice"
        );
        self.finished.store(false, Ordering::Relaxed);
        for state in self.schedulers.drain(..) {
            let finished = self.finished.clone();
            self.threads.push(std::thread::spawn(move || {
                scheduler_thread(state, finished);
            }));
        }
        self.state.set(ConcurrentSchedulerState::Run);
    }

    /// `ConcurrentScheduler::run_main`: run the main scheduler on the caller's
    /// thread for up to `timeout` seconds; returns `false` once finished.
    pub fn run_main(&mut self, timeout: f64) -> bool {
        assert_eq!(
            self.state.get(),
            ConcurrentSchedulerState::Run,
            "run_main before start"
        );
        let _guard = SchedulerGuard::new(&self.main, true);
        let deadline = Instant::now() + Duration::from_secs_f64(timeout);
        self.runtime.block_on(self.main.run_once(deadline));
        !self.finished.load(Ordering::Relaxed)
    }

    /// `Scheduler::run_no_guard`: like [`run_main`](Self::run_main), but
    /// requires the caller to already hold the main guard (the C++ test
    /// pattern of `get_guard()` + `run_no_guard` for sending events directly
    /// from the test thread between runs).
    pub fn run_main_no_guard(&mut self, timeout: f64) -> bool {
        assert_eq!(
            self.state.get(),
            ConcurrentSchedulerState::Run,
            "run_main before start"
        );
        assert!(
            self.main.has_guard.get(),
            "run_main_no_guard requires the main guard to be held"
        );
        let deadline = Instant::now() + Duration::from_secs_f64(timeout);
        self.runtime.block_on(self.main.run_once(deadline));
        !self.finished.load(Ordering::Relaxed)
    }

    /// `ConcurrentScheduler::get_main_guard`.
    pub fn get_main_guard(&self) -> SchedulerGuard {
        SchedulerGuard::new(&self.main, true)
    }

    /// `ConcurrentScheduler::get_send_guard`: a non-locking guard pointing at
    /// the send scheduler, for sends on the current thread.
    pub fn get_send_guard(&self) -> SchedulerGuard {
        self.send_scheduler().guard(false)
    }

    /// A thread-safe handle to the send scheduler — capture it and call
    /// [`guard`](SchedulerPtr::guard) on the foreign thread (the C++
    /// `get_send_guard()` called inside the thread).
    pub fn send_scheduler(&self) -> SchedulerPtr {
        SchedulerPtr(&*self.send_state as *const SchedulerState)
    }

    /// `ConcurrentScheduler::wakeup`: wake the main scheduler.
    pub fn wakeup(&self) {
        self.main.wakeup();
    }

    /// `ConcurrentScheduler::is_finished`.
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    /// `ConcurrentScheduler::finish`: stop the scheduler threads and join
    /// them. Remaining actors are torn down when the schedulers are dropped.
    pub fn finish(&mut self) {
        if self.state.get() == ConcurrentSchedulerState::Start {
            return;
        }
        self.finished.store(true, Ordering::Relaxed);
        self.wakeup();
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

/// The body of a non-main scheduler thread: build the current-thread runtime
/// and pump until finished (C++: `while (!is_finished()) sched->run(10s);`).
fn scheduler_thread(state: SchedulerState, finished: Arc<AtomicBool>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("failed to build a scheduler runtime");
    let ptr = Box::into_raw(Box::new(state));
    let _guard = SchedulerGuard::from_raw(ptr, true);
    runtime.block_on(async {
        let state = unsafe { &*ptr };
        while !finished.load(Ordering::Relaxed) {
            let deadline = Instant::now() + Duration::from_secs_f64(10.0);
            state.run_once(deadline).await;
        }
    });
    // Guard dropped first (unlocks has_guard), then the state is dropped,
    // which stops the remaining actors (Scheduler::~Scheduler → clear()).
    drop(_guard);
    unsafe {
        drop(Box::from_raw(ptr));
    }
}
