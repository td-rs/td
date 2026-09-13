//! Port of `td/actor/impl/ActorId.h` (+ the `ObjectPool` weak-pointer part of
//! `ActorId-decl.h`): weak actor references that survive across schedulers.
//!
//! In C++, an `ActorId` is a refcounted weak pointer into an `ObjectPool` of
//! `ActorInfo`s, and the pool slot stays alive while weak references exist
//! (this is what makes `is_alive()` and sends-from-any-thread safe). The Rust
//! port uses one refcounted `IdSlot` per actor:
//!
//! - `ref_cnt` — number of live `ActorId` copies; the slot is freed when it
//!   reaches zero (an actor is always referenced by its scheduler lists while
//!   alive, so a live actor's slot can never be freed);
//! - `alive` — set to `false` exactly once, on the actor's scheduler thread,
//!   at destruction; all senders check it first, so a dead actor drops its
//!   events exactly like the C++ null weak pointer;
//! - `sched` — the actor's scheduler id, with the top bit (`MIGRATING_BIT`)
//!   set while the actor is migrating — the port of C++
//!   `ActorInfo::sched_id_` + `migrate_dest_flag_atomic()`. It is the only
//!   field foreign threads read;
//! - `info` — a raw pointer to the `Box<ActorInfo>` allocation. It is written
//!   once at creation, dereferenced **only** on the actor's current scheduler
//!   thread (single-threaded access, the same invariant as C++), and freed at
//!   destruction after `alive` was published as `false` (after which no
//!   thread may dereference it);
//! - `name` — the actor's name, immutable after creation, for `get_name()`.
//!
//! This is why `IdSlot` is `Send + Sync` while `ActorInfo` itself is not.
//!
//! # The type parameter
//!
//! The C++ `ActorId<ActorType>` template is replaced by the generic marker
//! parameter `A` (defaulting to `dyn Actor`), which makes [`ActorId`],
//! [`ActorOwn`] and [`ActorShared`] typed without affecting the runtime
//! representation: the id is a session-unique value (the slot pointer), and
//! allocation never depends on `A`. The marker is inert — nothing dereferences
//! it — so re-typing is safe as long as the marker names the real actor type
//! or one of its supertraits; the only place a wrong marker can hurt is the
//! typed dispatch built in S33, which will be the actual unsafe boundary.

use std::cell::UnsafeCell;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

use crate::actor::Actor;
use crate::event::Event;
use crate::scheduler::{ActorInfo, Scheduler};

/// The migration flag stored in the top bit of the `sched` word — the C++
/// `(1 << 30)` in `ActorInfo::start_migrate`.
const MIGRATING_BIT: i32 = 1 << 30;

/// Per-actor shared slot behind every `ActorId`; see the module docs for the
/// invariants that make the `unsafe impl Send + Sync` sound.
pub(crate) struct IdSlot {
    ref_cnt: AtomicUsize,
    alive: AtomicBool,
    sched: AtomicI32,
    name: Box<str>,
    info: UnsafeCell<*mut ActorInfo>,
}

// SAFETY: `ref_cnt`, `alive` and `sched` are atomics, `name` is immutable
// after creation; `info` is written once at creation and only dereferenced on
// the actor's current scheduler thread (never from a foreign thread), so no
// data race on the pointee is possible through shared access to the slot.
unsafe impl Send for IdSlot {}
unsafe impl Sync for IdSlot {}

impl IdSlot {
    pub(crate) fn new(info: Box<ActorInfo>, sched_id: i32, name: &str) -> Box<Self> {
        Box::new(Self {
            ref_cnt: AtomicUsize::new(1),
            alive: AtomicBool::new(true),
            sched: AtomicI32::new(sched_id),
            name: name.into(),
            info: UnsafeCell::new(Box::into_raw(info)),
        })
    }
}

/// A weak, thread-safe reference to an actor — the port of C++
/// `ActorId<ActorType>`. `A` is an inert type marker (see the module docs);
/// the default is `dyn Actor`, i.e. the untyped form. Sending events to a
/// dead actor is a no-op.
///
/// [`ActorId`] is deliberately *not* `Copy` (copies are refcounted), unlike
/// the C++ value type; `clone` is cheap and matches the C++ semantics.
pub struct ActorId<A: ?Sized = dyn Actor> {
    slot: Option<NonNull<IdSlot>>,
    marker: PhantomData<fn() -> A>,
}

impl<A: ?Sized> ActorId<A> {
    pub(crate) fn new(slot: Box<IdSlot>) -> Self {
        // ref_cnt starts at 1 for this copy.
        let ptr = Box::into_raw(slot);
        Self {
            slot: NonNull::new(ptr),
            marker: PhantomData,
        }
    }

    fn empty_id() -> Self {
        Self {
            slot: None,
            marker: PhantomData,
        }
    }

    /// Whether this is a default-constructed (empty) id.
    pub fn empty(&self) -> bool {
        self.slot.is_none()
    }

    /// Whether the actor is still alive. Safe to call from any thread.
    pub fn is_alive(&self) -> bool {
        self.slot
            .is_some_and(|s| self.slot_of(s).alive.load(Ordering::Acquire))
    }

    /// The actor's name — the port of C++ `ActorId::get_name()`. Safe to call
    /// from any thread (the name is immutable once created). Returns the empty
    /// string for an empty id.
    pub fn get_name(&self) -> &str {
        match self.slot {
            Some(s) => &self.slot_of(s).name,
            None => "",
        }
    }

    /// Erase the type marker, returning the untyped id — the port of the C++
    /// implicit upcast to `ActorId<Actor>`.
    ///
    /// Consumes the id without dropping it: `slot` is `Copy`, so copying it
    /// out and forgetting `self` moves the reference without touching the
    /// refcount (a naive field move would still run `Drop` on `self` and
    /// double-drop the slot).
    pub fn erase(self) -> ActorId<dyn Actor> {
        let slot = self.slot;
        std::mem::forget(self);
        ActorId {
            slot,
            marker: PhantomData,
        }
    }

    /// Re-type this id to another marker type `B` — the port of the C++
    /// `static_cast<ActorId<Base>>(id)` conversions td performs between
    /// related actor types. The marker is inert (nothing dereferences it), so
    /// this is safe for any `B`; `B` should name the real actor type or one of
    /// its supertraits — the only consumer that cares is the typed dispatch
    /// built in S33.
    pub fn cast<B: ?Sized>(self) -> ActorId<B> {
        let slot = self.slot;
        std::mem::forget(self);
        ActorId {
            slot,
            marker: PhantomData,
        }
    }

    /// The `(sched_id, migrating)` pair — the port of
    /// `ActorInfo::migrate_dest_flag_atomic()`. Safe to call from any thread.
    pub(crate) fn sched(&self) -> (i32, bool) {
        let v = self.slot().sched.load(Ordering::Relaxed);
        (v & !MIGRATING_BIT, v & MIGRATING_BIT != 0)
    }

    pub(crate) fn start_migrate(&self, dest: i32) {
        self.slot()
            .sched
            .store(dest | MIGRATING_BIT, Ordering::Relaxed);
    }

    pub(crate) fn finish_migrate(&self) {
        let v = self.slot().sched.load(Ordering::Relaxed);
        self.slot()
            .sched
            .store(v & !MIGRATING_BIT, Ordering::Relaxed);
    }

    /// Marks the actor dead. Called by the scheduler thread that owns the
    /// actor, after which no other thread may dereference `info`.
    pub(crate) fn mark_dead(&self) {
        self.slot().alive.store(false, Ordering::Release);
        // Hygiene only: nothing reads `info` once `alive` is false.
        unsafe {
            *self.slot().info.get() = std::ptr::null_mut();
        }
    }

    /// Raw `ActorInfo` pointer. **Scheduler thread of the actor only** —
    /// matches the C++ comment "If actor is on our scheduler (thread) result
    /// will be valid".
    pub(crate) fn info_ptr(&self) -> *mut ActorInfo {
        unsafe { *self.slot().info.get() }
    }

    fn slot(&self) -> &IdSlot {
        self.slot_of(self.slot.expect("empty ActorId"))
    }

    #[allow(dead_code)]
    pub fn dbg_refcnt(&self) -> usize {
        self.slot
            .map(|s| self.slot_of(s).ref_cnt.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    fn slot_of(&self, s: NonNull<IdSlot>) -> &IdSlot {
        // SAFETY: the slot is refcounted by this very `ActorId` copy.
        unsafe { s.as_ref() }
    }
}

impl<A: ?Sized> Clone for ActorId<A> {
    fn clone(&self) -> Self {
        if let Some(s) = self.slot {
            self.slot_of(s).ref_cnt.fetch_add(1, Ordering::Relaxed);
        }
        Self {
            slot: self.slot,
            marker: PhantomData,
        }
    }
}

impl<A: ?Sized> Drop for ActorId<A> {
    fn drop(&mut self) {
        if let Some(s) = self.slot {
            let slot = self.slot_of(s);
            if slot.ref_cnt.fetch_sub(1, Ordering::AcqRel) == 1 {
                // SAFETY: last reference; the slot's `info` Box was already
                // freed by `SchedulerState::destroy_actor` if the actor died,
                // and an actor with zero id copies cannot be alive.
                unsafe {
                    drop(Box::from_raw(s.as_ptr()));
                }
            }
        }
    }
}

// SAFETY: same justification as `unsafe impl Send/Sync for IdSlot`; all
// shared access goes through the slot's atomics, and `PhantomData<fn() -> A>`
// does not impose any auto-trait requirements on `A`.
unsafe impl<A: ?Sized> Send for ActorId<A> {}
unsafe impl<A: ?Sized> Sync for ActorId<A> {}

impl<A: ?Sized> std::fmt::Debug for ActorId<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.slot {
            Some(s) => write!(
                f,
                "ActorId<{}>({:p})",
                std::any::type_name::<A>(),
                s.as_ptr()
            ),
            None => write!(f, "ActorId<{}>(empty)", std::any::type_name::<A>()),
        }
    }
}

// ActorId has identity semantics (one slot per actor), like the C++ weak
// pointer. Pointer equality on the slot is the exact port.
impl<A: ?Sized> PartialEq for ActorId<A> {
    fn eq(&self, other: &Self) -> bool {
        self.slot.map(|s| s.as_ptr()) == other.slot.map(|s| s.as_ptr())
    }
}
impl<A: ?Sized> Eq for ActorId<A> {}
impl<A: ?Sized> Hash for ActorId<A> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.slot.map(|s| s.as_ptr()).hash(state);
    }
}
impl<A: ?Sized> PartialOrd for ActorId<A> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<A: ?Sized> Ord for ActorId<A> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.slot
            .map(|s| s.as_ptr())
            .cmp(&other.slot.map(|s| s.as_ptr()))
    }
}

/// An owning actor reference — the port of C++ `ActorOwn<ActorType>`. Dropping
/// it sends `Event::hangup()` to the actor (C++ `ActorOwn::reset`), which
/// usually stops the actor.
pub struct ActorOwn<A: ?Sized = dyn Actor> {
    id: ActorId<A>,
}

impl<A: ?Sized> ActorOwn<A> {
    pub(crate) fn new(id: ActorId<A>) -> Self {
        Self { id }
    }

    /// Whether this reference is empty (holds no actor).
    pub fn empty(&self) -> bool {
        self.id.empty()
    }

    /// Whether the actor is still alive.
    pub fn is_alive(&self) -> bool {
        self.id.is_alive()
    }

    /// The underlying [`ActorId`], kept owned.
    pub fn get(&self) -> ActorId<A> {
        self.id.clone()
    }

    /// Erase the type marker — the port of the C++ implicit upcast to
    /// `ActorOwn<Actor>`.
    pub fn erase(mut self) -> ActorOwn<dyn Actor> {
        let id = std::mem::replace(&mut self.id, ActorId::empty_id());
        ActorOwn { id: id.erase() }
    }

    /// Releases ownership without sending `hangup` — the C++ `release()`.
    pub fn release(mut self) -> ActorId<A> {
        std::mem::replace(&mut self.id, ActorId::empty_id())
    }
}

impl<A: ?Sized> Drop for ActorOwn<A> {
    fn drop(&mut self) {
        if self.id.empty() {
            return;
        }
        // C++ ActorOwn::reset sends the hangup through the current scheduler
        // (and crashes without one). The Rust port is lenient when dropped
        // outside any scheduler context — the owning scheduler stops the actor
        // on teardown anyway.
        if let Some(state) = Scheduler::try_instance() {
            state.send_immediately(ActorRef::from(self.id.clone().erase()), Event::hangup());
        }
    }
}

impl<A: ?Sized> std::fmt::Debug for ActorOwn<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ActorOwn({:?})", self.id)
    }
}

/// A shared actor reference with a link token — the port of C++
/// `ActorShared<ActorType>`. Events sent through it carry the token, which
/// routes `Event::hangup()` to `Actor::hangup_shared` and is visible inside
/// the handler via `Actor::get_link_token`.
pub struct ActorShared<A: ?Sized = dyn Actor> {
    id: ActorId<A>,
    token: u64,
}

impl<A: ?Sized> ActorShared<A> {
    /// A shared reference with the given link token — the port of C++
    /// `ActorShared<ActorType>(id, token)` / `Actor::actor_shared(self, token)`.
    pub fn new(id: ActorId<A>, token: u64) -> Self {
        Self { id, token }
    }

    /// Whether this reference is empty (holds no actor).
    pub fn empty(&self) -> bool {
        self.id.empty()
    }

    /// Whether the actor is still alive.
    pub fn is_alive(&self) -> bool {
        self.id.is_alive()
    }

    /// The link token.
    pub fn token(&self) -> u64 {
        self.token
    }

    /// The underlying [`ActorId`].
    pub fn get(&self) -> ActorId<A> {
        self.id.clone()
    }

    /// Erase the type marker — the port of the C++ implicit upcast to
    /// `ActorShared<Actor>`.
    pub fn erase(mut self) -> ActorShared<dyn Actor> {
        let id = std::mem::replace(&mut self.id, ActorId::empty_id());
        ActorShared {
            id: id.erase(),
            token: self.token,
        }
    }

    /// Releases the id without sending `hangup`.
    pub fn release(mut self) -> ActorId<A> {
        std::mem::replace(&mut self.id, ActorId::empty_id())
    }
}

impl<A: ?Sized> Drop for ActorShared<A> {
    fn drop(&mut self) {
        if self.id.empty() {
            return;
        }
        if let Some(state) = Scheduler::try_instance() {
            state.send_immediately(
                ActorRef::from(self.id.clone().erase()).with_token(self.token),
                Event::hangup(),
            );
        }
    }
}

impl<A: ?Sized> std::fmt::Debug for ActorShared<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ActorShared({:?}, token={})", self.id, self.token)
    }
}

/// The send-side view of an actor reference — the port of C++ `ActorRef`: an
/// id plus an optional link token, constructible from any of the reference
/// types (of any marker type).
#[derive(Clone, Debug)]
pub struct ActorRef {
    pub(crate) id: ActorId<dyn Actor>,
    pub(crate) token: u64,
}

impl ActorRef {
    pub(crate) fn with_token(mut self, token: u64) -> Self {
        self.token = token;
        self
    }
}

impl<A: ?Sized> From<ActorId<A>> for ActorRef {
    fn from(id: ActorId<A>) -> Self {
        Self {
            id: id.erase(),
            token: 0,
        }
    }
}

impl<A: ?Sized> From<&ActorId<A>> for ActorRef {
    fn from(id: &ActorId<A>) -> Self {
        Self {
            id: id.clone().erase(),
            token: 0,
        }
    }
}

impl<A: ?Sized> From<ActorShared<A>> for ActorRef {
    fn from(shared: ActorShared<A>) -> Self {
        let token = shared.token();
        let id = shared.release();
        Self {
            id: id.erase(),
            token,
        }
    }
}

impl<A: ?Sized> From<&ActorShared<A>> for ActorRef {
    fn from(shared: &ActorShared<A>) -> Self {
        Self {
            id: shared.get().erase(),
            token: shared.token(),
        }
    }
}

impl<A: ?Sized> From<&ActorOwn<A>> for ActorRef {
    fn from(own: &ActorOwn<A>) -> Self {
        Self {
            id: own.get().erase(),
            token: 0,
        }
    }
}
