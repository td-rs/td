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
//!   with the logging wiring (S67).
//! - `Event::Custom` carries closures (the S34 immediate-closure subset and
//!   the S33 message-enum machinery); `start_migrate`/`finish_migrate`
//!   per-event hooks are currently no-ops until custom events carry
//!   migration hooks.

use std::cell::{Cell, RefCell, UnsafeCell};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;

use crate::actor::Actor;
use crate::actor_id::{ActorId, ActorOwn, ActorRef, IdSlot};
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
    /// The actor object lives inside the `IdSlot` block allocation, right
    /// after the `ActorInfo` (the C++ `ActorInfo<T>` actor storage — one
    /// malloc per actor, the S37 CreateActor gate). The fn-pointer pair
    /// rebuilds the trait object and drops the concrete `A`; raw-pointer
    /// unsizing is not expressible as a method, so the concrete type is
    /// erased here at registration time.
    OwnedInline {
        ptr: *mut (),
        as_mut: unsafe fn(*mut ()) -> *mut (dyn Actor + 'static),
        drop: unsafe fn(*mut ()),
    },
    Borrowed(*mut dyn Actor),
}

impl ActorSlot {
    /// `ActorInfo::get_actor_unsafe()`: the live actor, if any.
    fn as_mut(&mut self) -> Option<&mut dyn Actor> {
        match self {
            Self::Empty => None,
            // SAFETY: the actor is live until `destroy_actor` drops it in
            // place (the block outlives it).
            Self::OwnedInline { ptr, as_mut, .. } => Some(unsafe { &mut *as_mut(*ptr) }),
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

    /// The trait-object rebuild for `OwnedInline` — `*mut A` unsized to the
    /// erased object pointer (the vtable travels with the metadata).
    unsafe fn as_mut_actor<A: Actor>(p: *mut ()) -> *mut (dyn Actor + 'static) {
        p as *mut A as *mut (dyn Actor + 'static)
    }

    /// The in-place drop for `OwnedInline`.
    unsafe fn drop_actor<A: Actor>(p: *mut ()) {
        // SAFETY: `p` points at a live, initialized `A` that is dropped
        // exactly once, here.
        unsafe { std::ptr::drop_in_place(p as *mut A) };
    }
}

/// The registration input of [`SchedulerState::register_actor_impl`]: the
/// actor object by value (owned by the scheduler) or by pointer (the C++
/// `Deleter::None` overload).
enum RegisterActor<A: Actor> {
    Owned(A),
    Borrowed(*mut dyn Actor),
}

/// Port of C++ `ActorInfo` (the `mailbox_`, `is_running_` and timeout parts;
/// the atomic `sched_id_` lives in the `IdSlot` shared with [`ActorId`]).
pub(crate) struct ActorInfo {
    pub actor: RefCell<ActorSlot>,
    pub mailbox: RefCell<VecDeque<Event>>,
    pub is_running: Cell<bool>,
    pub in_ready: Cell<bool>,
    pub in_pending: Cell<bool>,
    /// Position in the scheduler's `pending` vector (valid while `in_pending`;
    /// kept updated on `swap_remove` — the `KHeap`-style index of the S37
    /// intrusive-list replacement).
    pub pending_index: Cell<usize>,
    /// Incremented on every timeout set/cancel; heap entries with a stale
    /// generation are dropped lazily (the `KHeap::fix`/`erase` replacement).
    pub timeout_gen: Cell<u64>,
    /// The live deadline, if a timeout is armed (`in_heap` + heap key in C++).
    pub deadline: Cell<Option<Instant>>,
    /// Whether the actor was registered for a `start_up` (C++
    /// `ActorTraits::need_start_up`: true for owned actors, false for the
    /// by-pointer registrations). `do_stop_actor` only runs `tear_down` for
    /// such actors — an actor that never starts never tears down.
    pub need_start_up: bool,
}

impl ActorInfo {
    pub(crate) fn new(actor: ActorSlot, need_start_up: bool) -> Self {
        Self {
            actor: RefCell::new(actor),
            mailbox: RefCell::new(VecDeque::new()),
            is_running: Cell::new(false),
            in_ready: Cell::new(false),
            in_pending: Cell::new(false),
            pending_index: Cell::new(0),
            timeout_gen: Cell::new(0),
            deadline: Cell::new(None),
            need_start_up,
        }
    }
}

/// The event context of the currently dispatched event — the port of C++
/// `Scheduler::EventContext` + `event_context_ptr_`.
struct EventContext {
    /// The slot of the dispatched actor, as a raw pointer (null when none):
    /// the context *borrows* the caller's owned copy instead of cloning, so
    /// the inline dispatch pays no refcount (the S37 hot path).
    actor: *const IdSlot,
    dest: i32,
    flags: u32,
    link_token: u64,
}

impl Default for EventContext {
    fn default() -> Self {
        Self {
            actor: std::ptr::null(),
            dest: 0,
            flags: 0,
            link_token: 0,
        }
    }
}

/// The per-thread scheduler pointer — the only thread-local on the hot path
/// (the S37 profile measured `thread::local::LocalKey::with` at over a third
/// of each inline hop; the dispatch context itself moved into
/// [`SchedulerState`], which is single-thread-owned, so the inline dispatch
/// pays exactly one thread-local access: `Scheduler::instance()`).
struct SchedulerTls {
    /// The scheduler of the current thread (`Scheduler::scheduler_` in C++).
    /// Set by [`SchedulerGuard`]; foreign threads may point it at the send
    /// scheduler, in which case sends always route through the channels.
    scheduler: Cell<*const SchedulerState>,
}

thread_local! {
    static TLS: SchedulerTls = const {
        SchedulerTls {
            scheduler: Cell::new(std::ptr::null()),
        }
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
    /// Idle actors (`pending_actors_`). A vector with `swap_remove` and a
    /// per-actor index in [`ActorInfo::pending_index`] — O(1) pointer work on
    /// every dispatch enter/exit, the intrusive-list equivalent (the S37
    /// profile showed the `HashSet` insert alone taking a quarter of each
    /// inline hop; the hash wasn't the cost, the table insert was).
    pending: RefCell<Vec<ActorId>>,
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
    /// The context of the dispatch currently running on this scheduler —
    /// `Scheduler::event_context_ptr_` in C++. Each dispatch guard installs
    /// its own frame-local `EventContext` and swaps the pointer (the C++
    /// `EventGuard` save/restore chain): nested inline dispatches stack
    /// through the call stack, so no heap, no `RefCell`, and no thread-local
    /// on the hot path. An `AtomicPtr` so the state stays `Send + Sync`
    /// (the state moves to the worker threads); only the scheduler's own
    /// thread ever touches it — the same invariant as `inbound`.
    ctx: AtomicPtr<EventContext>,
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
            pending: RefCell::new(Vec::new()),
            timeouts: RefCell::new(BinaryHeap::new()),
            pending_events: RefCell::new(HashMap::new()),
            close_flag: AtomicBool::new(false),
            yield_flag: Cell::new(false),
            has_guard: Cell::new(false),
            outbound,
            inbound: UnsafeCell::new(Some(inbound)),
            ctx: AtomicPtr::new(std::ptr::null_mut()),
            wakeup_tx,
            main_wakeup,
            finished,
        }
    }

    /// `ActorInfo` of a live actor. Only valid on the actor's scheduler thread
    /// (single-threaded access; the same invariant as C++). Takes `&ActorId`:
    /// the caller always holds a copy (the scheduler's ready/pending lists or
    /// the event's own id), so no refcount bump is needed on the hot path.
    fn info(&self, id: &ActorId) -> &ActorInfo {
        unsafe { &*id.info_ptr() }
    }

    /// Run `f` against the current dispatch's event context — valid only
    /// inside a dispatch guard (the C++ `Scheduler::event_context()`).
    ///
    /// # Safety of the pointer discipline
    ///
    /// `ctx` always points at a live, frame-local `EventContext` of an
    /// enclosing dispatch on this scheduler's thread (or is null outside
    /// any dispatch — debug-asserted here). Each context object has exactly
    /// one guard frame deriving `&mut` from the pointer at a time: while a
    /// nested dispatch runs, the pointer targets the inner frame's context,
    /// so the outer one is never aliased.
    fn with_ctx<R>(&self, f: impl FnOnce(&mut EventContext) -> R) -> R {
        // SAFETY: only the scheduler's own thread touches `ctx` (the
        // `inbound` invariant), and the dispatch guard keeps the pointee
        // alive for the duration.
        let ptr = self.ctx.load(Ordering::Relaxed);
        debug_assert!(!ptr.is_null(), "no dispatch context on this thread");
        // SAFETY: see the function docs.
        f(unsafe { &mut *ptr })
    }

    /// Install a fresh guard context, returning the enclosing one (the C++
    /// `EventGuard` save half).
    fn install_ctx(&self, ctx: &EventContext) -> *const EventContext {
        self.ctx.swap(
            ctx as *const EventContext as *mut EventContext,
            Ordering::Relaxed,
        )
    }

    /// Restore the enclosing guard context (the C++ `EventGuard` restore
    /// half).
    fn restore_ctx(&self, prev: *const EventContext) {
        self.ctx.store(prev.cast_mut(), Ordering::Relaxed);
    }

    // ---- actor registration (Scheduler.h template part) ----

    /// `Scheduler::create_actor`: register on the current scheduler.
    pub fn create_actor<A: Actor>(&self, name: &str, actor: A) -> ActorOwn<A> {
        self.register_actor_impl(name, RegisterActor::Owned(actor), self.sched_id)
    }

    /// `Scheduler::create_actor_on_scheduler`.
    pub fn create_actor_on_scheduler<A: Actor>(
        &self,
        name: &str,
        sched_id: i32,
        actor: A,
    ) -> ActorOwn<A> {
        self.register_actor_impl(name, RegisterActor::Owned(actor), sched_id)
    }

    /// `Scheduler::register_actor` (the `Deleter::Destroy` overload: the
    /// scheduler owns the actor).
    pub fn register_actor<A: Actor>(&self, name: &str, sched_id: i32, actor: A) -> ActorOwn<A> {
        self.register_actor_impl(name, RegisterActor::Owned(actor), sched_id)
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
        self.register_actor_impl(
            name,
            RegisterActor::Borrowed(actor as *mut dyn Actor),
            sched_id,
        )
    }

    fn register_actor_impl<A: Actor>(
        &self,
        name: &str,
        actor: RegisterActor<A>,
        sched_id: i32,
    ) -> ActorOwn<A> {
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

        // One allocation: `[IdSlot | ActorInfo | actor]` — the C++ ObjectPool
        // block shape (the S37 CreateActor gate). The actor object is written
        // into the block; borrowed actors leave their storage empty.
        let (slot_ptr, info_ptr, actor_storage) =
            crate::actor_id::IdSlot::allocate_block::<A>(sched_id, name);
        // C++ sends Event::start to owned actors only (need_start_up is false
        // for borrowed ones; see `register_actor_ptr`). The flag is recorded
        // on the ActorInfo: `do_stop_actor` tears down only what started.
        let (actor_slot, need_start_up) = match actor {
            RegisterActor::Owned(a) => {
                // SAFETY: `actor_storage` is fresh block memory, unaliased;
                // the actor is written exactly once, here.
                unsafe { std::ptr::write(actor_storage, a) };
                (
                    ActorSlot::OwnedInline {
                        ptr: actor_storage as *mut (),
                        as_mut: ActorSlot::as_mut_actor::<A>,
                        drop: ActorSlot::drop_actor::<A>,
                    },
                    true,
                )
            }
            RegisterActor::Borrowed(p) => (ActorSlot::Borrowed(p), false),
        };
        // SAFETY: `info_ptr` is fresh block memory, unaliased.
        unsafe { std::ptr::write(info_ptr, ActorInfo::new(actor_slot, need_start_up)) };
        // The allocation is type-independent (the "session-unique id" of the
        // plan); the typed `ActorId<A>` is a pure marker added on the way out.
        let id: ActorId = ActorId::from_raw_block(slot_ptr);
        self.actor_count.set(self.actor_count.get() + 1);

        if sched_id != self.sched_id {
            self.send_later(id.clone(), Event::start());
            self.do_migrate_actor(id.clone(), sched_id);
        } else {
            self.insert_pending(&id);
            if need_start_up {
                self.send_later(id.clone(), Event::start());
            }
        }
        ActorOwn::new(id.cast::<A>())
    }

    // ---- sending (Scheduler.cpp send paths) ----

    /// `Scheduler::send_immediately` / free `send_event`: runs the handler
    /// inline when possible, otherwise queues it.
    pub fn send_immediately<'a>(&self, actor_ref: impl Into<ActorRef<'a>>, event: Event) {
        self.send_immediately_impl(actor_ref.into(), event);
    }

    /// `Scheduler::send_later` / free `send_event_later`: always through the
    /// mailbox.
    pub fn send_later<'a>(&self, actor_ref: impl Into<ActorRef<'a>>, event: Event) {
        self.send_later_impl(actor_ref.into(), event);
    }

    /// `Scheduler::send_closure_immediately` — the send half of C++
    /// `send_closure`: run the closure inline when the destination is idle
    /// (not running, empty mailbox), otherwise through the mailbox.
    ///
    /// The parameter is concrete, not generic: boxed-closure call sites must
    /// coerce through [`ActorThunk::from_boxed`] (the coercion is what
    /// generalizes the closure's argument lifetime; a generic `Into` bound
    /// would move the coercion behind a trait obligation and lose it).
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

    fn send_later_impl(&self, actor_ref: ActorRef<'_>, mut event: Event) {
        event.link_token = actor_ref.token;
        let id = actor_ref.id();
        if id.empty() || !id.is_alive() || self.close_flag.load(Ordering::Relaxed) {
            return;
        }
        let (sched, migrating) = id.sched();
        if sched == self.sched_id {
            if migrating {
                self.pending_events
                    .borrow_mut()
                    .entry(id.clone())
                    .or_default()
                    .push(event);
            } else {
                self.add_to_mailbox(id, event);
            }
        } else {
            let _ = self.outbound[sched as usize].send(Inbound::Event {
                id: id.clone(),
                event,
            });
        }
    }

    fn send_immediately_impl(&self, actor_ref: ActorRef<'_>, mut event: Event) {
        event.link_token = actor_ref.token;
        let id = actor_ref.id();
        if id.empty() || !id.is_alive() || self.close_flag.load(Ordering::Relaxed) {
            return;
        }
        let (sched, migrating) = id.sched();
        if sched == self.sched_id {
            if !migrating {
                // C++ `get_actor_sched_id_to_send_immediately`: run inline iff
                // not running and the mailbox is empty.
                let can_immediately = {
                    let info = self.info(id);
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
                    .entry(id.clone())
                    .or_default()
                    .push(event);
            }
        } else {
            let _ = self.outbound[sched as usize].send(Inbound::Event {
                id: id.clone(),
                event,
            });
        }
    }

    /// `Scheduler::add_to_mailbox`: queue the event and move an idle actor to
    /// the ready queue.
    fn add_to_mailbox(&self, id: &ActorId, event: Event) {
        let info = self.info(id);
        if !info.is_running.get() {
            if info.in_pending.get() {
                // The pending copy moves into the ready list — no refcount
                // touch (the caller's own copy keeps the slot alive).
                if let Some(copy) = self.take_pending(id) {
                    self.ready.borrow_mut().push_back(copy);
                    info.in_ready.set(true);
                } else {
                    self.ready.borrow_mut().push_back(id.clone());
                    info.in_ready.set(true);
                }
            } else if !info.in_ready.get() {
                self.ready.borrow_mut().push_back(id.clone());
                info.in_ready.set(true);
            }
        }
        info.mailbox.borrow_mut().push_back(event);
    }

    // ---- dispatch (EventGuard + do_event) ----

    /// `Scheduler::do_event`. The caller installs the event's link token in
    /// the guard context — the inline dispatch writes it once, with the
    /// context, instead of paying a separate thread-local write here.
    fn do_event(&self, id: &ActorId, event: Event) {
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
                closure.run(actor)
            }
            EventType::NoType => unreachable!("empty event dispatched"),
        }
    }

    /// The beginning of the C++ `EventGuard` (minus the context install,
    /// which the guard frame does): marks the actor running and unlinks it
    /// from the pending list, handing the list's copy back to the guard so
    /// it can be re-queued by move — no refcount touch on the hot path.
    fn enter_actor(&self, id: &ActorId) -> Option<ActorId> {
        let info = self.info(id);
        info.is_running.set(true);
        self.take_pending(id)
    }

    /// The end of the C++ `EventGuard` destructor: re-queues the actor —
    /// moving the guard's list copy back instead of cloning — clears
    /// `is_running`, and returns the `Stop`/`Migrate` flags, restoring the
    /// enclosing context in the same `TLS.with` that reads them (one
    /// thread-local access for both). The restore must happen *before* the
    /// flags are honored — `do_stop_actor` asserts it is not stopping the
    /// dispatched actor.
    fn exit_actor(
        &self,
        id: &ActorId,
        list_copy: Option<ActorId>,
        prev_ctx: *const EventContext,
    ) -> (u32, i32) {
        let info = self.info(id);
        let mailbox_empty = info.mailbox.borrow().is_empty();
        if mailbox_empty {
            match list_copy {
                Some(copy) => self.put_pending(copy),
                None => self.insert_pending(id),
            }
        } else {
            match list_copy {
                Some(copy) => self.ready.borrow_mut().push_back(copy),
                None => self.ready.borrow_mut().push_back(id.clone()),
            }
            info.in_ready.set(true);
        }
        info.is_running.set(false);
        // SAFETY: the guard frame installed its own context before the
        // dispatch; the pointer is valid until this restore.
        let ctx = unsafe { &*self.ctx.load(Ordering::Relaxed) };
        let out = (ctx.flags, ctx.dest);
        self.restore_ctx(prev_ctx);
        out
    }

    /// Honor the `Stop`/`Migrate` flags after the guard's context was
    /// restored (the C++ EventGuard destructor tail).
    fn honor_flags(&self, id: &ActorId, flags: u32, dest: i32) {
        if flags & FLAG_STOP != 0 {
            self.do_stop_actor(id.clone());
        } else if flags & FLAG_MIGRATE != 0 {
            self.do_migrate_actor(id.clone(), dest);
        }
    }

    /// A single-event inline dispatch — the C++ `EventGuard` around
    /// `send_immediately_impl`'s inline path: a frame-local context swapped
    /// into the thread local for the duration of the dispatch. The id is
    /// borrowed from the caller's `ActorRef` — the inline path takes no
    /// refcount.
    fn run_event_guarded(&self, id: &ActorId, event: Event) {
        // The link token is installed with the context — the inline dispatch
        // pays no separate context write in `do_event`.
        let ctx = EventContext {
            actor: id.slot_ptr(),
            dest: 0,
            flags: 0,
            link_token: event.link_token,
        };
        let prev = self.install_ctx(&ctx);
        let list_copy = self.enter_actor(id);
        self.do_event(id, event);
        let (flags, dest) = self.exit_actor(id, list_copy, prev);
        self.honor_flags(id, flags, dest);
    }

    /// `Scheduler::flush_mailbox`: drain the mailbox under one guard, until it
    /// is empty or the actor asked to stop/migrate.
    fn flush_mailbox(&self, id: ActorId) {
        let info = self.info(&id);
        let mailbox_len = info.mailbox.borrow().len();
        debug_assert_ne!(mailbox_len, 0, "flushing an empty mailbox");
        let ctx = EventContext {
            actor: id.slot_ptr(),
            dest: 0,
            flags: 0,
            link_token: 0,
        };
        let prev = self.install_ctx(&ctx);
        self.enter_actor(&id);
        let mut processed = 0;
        while processed < mailbox_len {
            let flags = self.with_ctx(|ctx| ctx.flags);
            if flags != 0 {
                break;
            }
            let event = info
                .mailbox
                .borrow_mut()
                .pop_front()
                .expect("mailbox emptied under guard");
            processed += 1;
            // Each mailbox event carries its own link token; the inline
            // dispatch installs it with the guard context instead.
            self.with_ctx(|ctx| ctx.link_token = event.link_token);
            self.do_event(&id, event);
        }
        // The caller (`run_mailbox`) handed over the ready-list copy; pass
        // it back by move on exit. The flag-honoring id is cloned on the
        // (rare) Stop/Migrate path only.
        let flags = self.with_ctx(|ctx| ctx.flags);
        let info = self.info(&id);
        let mailbox_empty = info.mailbox.borrow().is_empty();
        let honor_id = if flags != 0 { Some(id.clone()) } else { None };
        if mailbox_empty {
            self.put_pending(id);
        } else {
            self.ready.borrow_mut().push_back(id);
            info.in_ready.set(true);
        }
        info.is_running.set(false);
        let dest = self.with_ctx(|ctx| ctx.dest);
        self.restore_ctx(prev);
        if let Some(honor_id) = honor_id {
            self.honor_flags(&honor_id, flags, dest);
        }
    }

    // ---- lifecycle (do_stop_actor, destroy, migration) ----

    /// `Scheduler::do_stop_actor` — the path behind C++ `Actor::do_stop()`:
    /// run `tear_down` and destroy the actor synchronously, callable from any
    /// actor on this scheduler (unlike `stop()`, which only flags the
    /// currently dispatched actor). The C++ version wraps the teardown in its
    /// own `EventGuard` whose destructor restores the outer event context;
    /// the frame-local context swap is the same save/restore here.
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
        let current = self.ctx.load(Ordering::Relaxed);
        debug_assert!(
            current.is_null() || unsafe { (*current).actor != id.slot_ptr() },
            "do_stop_actor on the dispatched actor is UB in Rust (use stop())"
        );
        // C++ `do_stop_actor` dispatches `Event::stop` only for actors with
        // `need_start_up()` (`ActorTraits<T>`): an actor that never started
        // never tears down (Scheduler.cpp do_stop_actor). The flag lives in
        // the `ActorInfo` and is read before `destroy_actor` frees it. The
        // C++ keeps its EventGuard (is_running = true) across the
        // destruction: sends to this actor from its own teardown/Drop land
        // in the mailbox (cleared by destroy_actor) instead of dispatching
        // inline — the guard context is installed here for the same reason.
        let stop_ctx = EventContext {
            actor: id.slot_ptr(),
            dest: 0,
            flags: 0,
            link_token: 0,
        };
        let prev = self.install_ctx(&stop_ctx);
        let need_stop_event = self.info(&id).need_start_up;
        if need_stop_event {
            // The pending-list copy is dropped here: `destroy_actor`'s
            // `remove_from_lists` has nothing left to unlink, and the owned
            // `id` keeps the slot alive through the teardown.
            let _list_copy = self.enter_actor(&id);
            self.do_event(&id, Event::stop());
        }
        self.destroy_actor(id);
        self.restore_ctx(prev);
    }

    /// `Scheduler::destroy_actor` + the pool release: drop the actor and free
    /// its `ActorInfo` after publishing death.
    fn destroy_actor(&self, id: ActorId) {
        let info_ptr = id.info_ptr();
        self.remove_from_lists(&id);
        self.actor_count.set(self.actor_count.get() - 1);
        debug_assert!(self.actor_count.get() >= 0);
        let info = unsafe { &*info_ptr };
        info.timeout_gen.set(info.timeout_gen.get() + 1);
        info.deadline.set(None);
        // Drop the actor first (C++ destroys the actor before the mailbox).
        // An OwnedInline actor is dropped in place (its block memory is
        // freed by the last ActorId drop); a Borrowed actor (Deleter::None)
        // is only unregistered — its owner drops it.
        let old = std::mem::replace(&mut *info.actor.borrow_mut(), ActorSlot::Empty);
        if let ActorSlot::OwnedInline { ptr, drop, .. } = old {
            // SAFETY: the actor is live, and dropped exactly once here.
            unsafe { drop(ptr) };
        }
        info.mailbox.borrow_mut().clear();
        id.mark_dead();
        // SAFETY: `mark_dead` published alive=false; no thread dereferences
        // the ActorInfo after that. It is dropped in place — the block it
        // lives in (co-located with the `IdSlot`) is freed by the last
        // `ActorId` drop.
        unsafe {
            std::ptr::drop_in_place(info_ptr);
        }
    }

    /// `Scheduler::migrate_actor`: record the request; applied after the
    /// current event by `exit_actor`.
    pub(crate) fn migrate_actor(&self, id: ActorId, dest: i32) {
        if dest == self.sched_id {
            return;
        }
        self.with_ctx(|ctx| {
            debug_assert_eq!(ctx.actor, id.slot_ptr(), "migrate outside of dispatch");
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
            let info = self.info(&id);
            info.actor
                .borrow_mut()
                .as_mut()
                .expect("migrating a dead actor")
                .on_start_migrate(dest);
            // C++ also calls start_migrate() on every mailbox event; a no-op
            // until custom events (S33) carry migration hooks.
        }
        id.start_migrate(dest);
        self.remove_from_lists(&id);
        let info = self.info(&id);
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
            self.info(&id).mailbox.borrow_mut().extend(events);
        }
        let info = self.info(&id);
        let mailbox_empty = info.mailbox.borrow().is_empty();
        info.actor
            .borrow_mut()
            .as_mut()
            .expect("migrated a dead actor")
            .on_finish_migrate();
        if mailbox_empty {
            // The migrated-in copy moves into the pending list — no
            // refcount touch.
            self.put_pending(id);
        } else {
            self.ready.borrow_mut().push_back(id);
            info.in_ready.set(true);
        }
    }

    /// Unlink the actor from the ready/pending lists (the destroy/migrate
    /// paths — the list copies are dropped here).
    fn remove_from_lists(&self, id: &ActorId) {
        if self.info(id).in_pending.get() {
            // The taken copy is dropped: the actor is leaving this scheduler
            // for good.
            let _removed = self.take_pending(id);
        }
        let info = self.info(id);
        if info.in_ready.get() {
            // O(n) where the C++ intrusive list is O(1); revisit at S37.
            self.ready.borrow_mut().retain(|x| *x != *id);
            info.in_ready.set(false);
        }
    }

    /// Mark the actor idle: push it onto the `pending` vector and record its
    /// position (the O(1) replacement for the `HashSet` insert — see the
    /// `pending` field docs). The clone form; the hot dispatch path moves its
    /// own copy through [`put_pending`](Self::put_pending)/[`take_pending`]
    /// instead.
    fn insert_pending(&self, id: &ActorId) {
        let info = self.info(id);
        debug_assert!(!info.in_pending.get(), "insert_pending twice");
        info.in_pending.set(true);
        let mut pending = self.pending.borrow_mut();
        info.pending_index.set(pending.len());
        pending.push(id.clone());
    }

    /// [`insert_pending`](Self::insert_pending) with an owned copy — the
    /// dispatch moves the list copy back with no refcount touch.
    fn put_pending(&self, id: ActorId) {
        let info = self.info(&id);
        debug_assert!(!info.in_pending.get(), "put_pending twice");
        info.in_pending.set(true);
        let mut pending = self.pending.borrow_mut();
        info.pending_index.set(pending.len());
        pending.push(id);
    }

    /// Unmark the idle actor: `swap_remove` it from the `pending` vector,
    /// fix the position of the element that took its place, and hand the
    /// taken copy to the caller (which moves it back on exit, into the ready
    /// list, or drops it).
    fn take_pending(&self, id: &ActorId) -> Option<ActorId> {
        let info = self.info(id);
        if !info.in_pending.get() {
            return None;
        }
        info.in_pending.set(false);
        let mut pending = self.pending.borrow_mut();
        let idx = info.pending_index.get();
        let removed = pending.swap_remove(idx);
        debug_assert_eq!(&removed, id, "pending index out of sync");
        if idx < pending.len() {
            self.info(&pending[idx]).pending_index.set(idx);
        }
        Some(removed)
    }

    // ---- actor proxies called from the Actor trait ----

    pub(crate) fn yield_actor(&self, id: ActorId) {
        self.send_later(id, Event::yield_());
    }

    pub(crate) fn stop_actor(&self, id: ActorId) {
        self.with_ctx(|ctx| {
            debug_assert_eq!(ctx.actor, id.slot_ptr(), "stop outside of dispatch");
            ctx.flags |= FLAG_STOP;
        });
    }

    pub(crate) fn get_link_token(&self, id: ActorId) -> u64 {
        self.with_ctx(|ctx| {
            debug_assert_eq!(
                ctx.actor,
                id.slot_ptr(),
                "get_link_token outside of dispatch"
            );
            ctx.link_token
        })
    }

    // ---- timeouts ----

    pub(crate) fn get_actor_timeout(&self, id: ActorId) -> Option<f64> {
        let info = self.info(&id);
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
        // C++ `Scheduler::set_actor_timeout_in`: clamp to [0, 1e10], then a
        // plain `set_actor_timeout_at` with the absolute deadline.
        self.set_actor_timeout_at(id, tdutils::time::now() + timeout.clamp(0.0, 1e10));
    }

    /// `timeout_at` is absolute — seconds on the adjusted monotonic clock
    /// ([`crate::time_now`] base), exactly like C++
    /// `Scheduler::set_actor_timeout_at(ActorInfo, Timestamp)`. Past values
    /// fire on the next pump; future values are capped at +1e10 s (the C++
    /// `Timestamp::in(1e10, ...)` clamp, also keeping `Instant` in range).
    pub(crate) fn set_actor_timeout_at(&self, id: ActorId, timeout_at: f64) {
        let info = self.info(&id);
        let generation = info.timeout_gen.get() + 1;
        info.timeout_gen.set(generation);
        let at = Instant::now()
            + Duration::from_secs_f64((timeout_at - tdutils::time::now()).clamp(0.0, 1e10));
        info.deadline.set(Some(at));
        self.timeouts
            .borrow_mut()
            .push(TimeoutEntry(Reverse(at), id, generation));
    }

    pub(crate) fn cancel_actor_timeout(&self, id: ActorId) {
        let info = self.info(&id);
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
            if !id.is_alive() {
                // The actor died with an armed timeout: `destroy_actor`
                // bumped the generation (so the entry is stale) and freed
                // the `ActorInfo` in place — pop it without the deref.
                heap.pop();
                continue;
            }
            let info = self.info(id);
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
            let info = self.info(&id);
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

    /// `Scheduler::clear`: stop every remaining actor (runs `tear_down` for
    /// actors that ever had `start_up`).
    ///
    /// Like the C++ `Scheduler::clear`, this installs a scheduler guard for
    /// the sweep, so `tear_down`/`Drop` code can still call
    /// `Scheduler::instance()` — sends during close are dropped via
    /// `close_flag` instead of delivered. Skipped when a guard is already
    /// held (clear from inside a live `run`); the C++ would CHECK-fail there.
    fn clear(&self) {
        if self.close_flag.swap(true, Ordering::AcqRel) {
            return;
        }
        let _guard = if !self.has_guard.get() {
            Some(SchedulerGuard::from_raw(
                self as *const SchedulerState,
                true,
            ))
        } else {
            None
        };
        loop {
            let next = if let Some(id) = self.ready.borrow_mut().pop_front() {
                self.info(&id).in_ready.set(false);
                Some(id)
            } else {
                // Pop from the end of the pending vector (order is
                // irrelevant during close — sends are dropped via
                // `close_flag`); the popped element's flag is cleared.
                let next_pending = self.pending.borrow_mut().pop();
                if let Some(id) = next_pending.as_ref() {
                    self.info(id).in_pending.set(false);
                }
                next_pending
            };
            match next {
                Some(id) => self.do_stop_actor(id),
                None => break,
            }
        }
        // The C++ releases the whole `actor_info_pool_` here; every actor
        // was in ready/pending (actors mid-dispatch hold a guard and clear
        // is their last stop). A nonzero count is a leak.
        debug_assert_eq!(
            self.actor_count.get(),
            0,
            "Scheduler::clear left {} actors alive",
            self.actor_count.get()
        );
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

    /// Whether a scheduler exists on the current thread. The per-actor
    /// timer methods ([`crate::Actor::set_timeout_at`] and friends) panic
    /// outside a scheduler; callers that mirror state into them (and that
    /// must stay usable in direct-call tests) check this first.
    pub fn in_scheduler_context() -> bool {
        Self::try_instance().is_some()
    }

    /// A thread-safe handle to this thread's scheduler, for building a
    /// [`SchedulerGuard`] on another thread — the actor-side capture
    /// counterpart of [`ConcurrentScheduler::send_scheduler`](ConcurrentScheduler::send_scheduler).
    /// Sends routed through it reach any actor (the destination's scheduler
    /// is resolved per actor id), so a worker thread can deliver results
    /// into the actor system.
    pub fn instance_ptr() -> SchedulerPtr {
        SchedulerPtr(Self::instance() as *const SchedulerState)
    }

    pub(crate) fn try_instance() -> Option<&'static SchedulerState> {
        TLS.with(|tls| {
            let ptr = tls.scheduler.get();
            if ptr.is_null() {
                None
            } else {
                Some(unsafe { &*ptr })
            }
        })
    }

    /// The actor currently being dispatched, for the `Actor` trait proxies.
    pub(crate) fn current_actor() -> ActorId {
        let state = Self::instance();
        // SAFETY: only the scheduler's own thread touches `ctx` (see the
        // field docs).
        let ptr = state.ctx.load(Ordering::Relaxed);
        debug_assert!(!ptr.is_null(), "no dispatch context on this thread");
        let actor = unsafe { (*ptr).actor };
        if actor.is_null() {
            panic!("no actor is being dispatched");
        }
        // SAFETY: the dispatch's own id copy keeps the slot alive; the
        // returned copy takes its own reference.
        unsafe { ActorId::from_slot_ptr(std::ptr::NonNull::new_unchecked(actor.cast_mut())) }
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
        let prev = TLS.with(|tls| tls.scheduler.replace(ptr));
        Self { prev, locked_state }
    }
}

impl Drop for SchedulerGuard {
    fn drop(&mut self) {
        TLS.with(|tls| tls.scheduler.set(self.prev));
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
    /// Clones of the worker schedulers' inbound senders, so `finish` can wake
    /// them (C++ `ConcurrentScheduler::on_finish` wakes all `schedulers_`).
    worker_wakeups: Vec<tokio::sync::mpsc::UnboundedSender<Inbound>>,
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
        let worker_wakeups: Vec<_> = outbound.iter().skip(1).cloned().collect();
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
            worker_wakeups,
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
        // C++ on_finish wakes every scheduler, so workers blocked on their
        // inbound queue notice `finished` immediately instead of after the
        // 10 s idle poll.
        for tx in &self.worker_wakeups {
            let _ = tx.send(Inbound::Wakeup);
        }
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
// Runs on the std atomics only: under the `loom` feature the IdSlot
// primitives must stay inside a loom model, and this test drives a real
// scheduler.
#[cfg(all(test, not(feature = "loom")))]
mod lifecycle_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    /// C++ `Scheduler::do_stop_actor` runs `tear_down` only for actors that
    /// ever had a start (`need_start_up`): a borrowed actor (the by-pointer
    /// `register_actor` overload, C++ `Deleter::None`) is unregistered and
    /// its owner drops it — no `tear_down`, no hangup-driven double stop.
    #[test]
    fn borrowed_actor_never_gets_tear_down() {
        struct Borrowed {
            torn_down: Arc<AtomicBool>,
            dropped: Arc<AtomicBool>,
        }
        impl Actor for Borrowed {
            fn tear_down(&mut self) {
                self.torn_down.store(true, AtomicOrdering::SeqCst);
            }
        }
        impl Drop for Borrowed {
            fn drop(&mut self) {
                self.dropped.store(true, AtomicOrdering::SeqCst);
            }
        }

        let sched = ConcurrentScheduler::new(0);
        let torn_down = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        {
            let mut boxed = Box::new(Borrowed {
                torn_down: torn_down.clone(),
                dropped: dropped.clone(),
            });
            {
                let _guard = SchedulerGuard::new(&sched.main, true);
                let own = unsafe { sched.main.register_actor_ptr("borrowed", 0, &mut *boxed) };
                drop(own); // hangup → stop → do_stop_actor → destroy (no tear_down)
            }
            // The scheduler only unregistered the actor; the owner still
            // holds it and drops it here.
            assert!(!dropped.load(AtomicOrdering::SeqCst));
        }
        assert!(dropped.load(AtomicOrdering::SeqCst), "owner drops it");
        assert!(
            !torn_down.load(AtomicOrdering::SeqCst),
            "an actor that never started never tears down"
        );
    }
}
