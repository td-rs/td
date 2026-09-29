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

use self::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::alloc::Layout;
use std::cell::UnsafeCell;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ptr::NonNull;

/// The atomics of [`IdSlot`]: plain `std` atomics normally, loom's modelable
/// ones under the `loom` feature (the `IdSlot` protocol — `ref_cnt`/`alive` —
/// is the concurrency surface verified at S36; see the module below).
mod atomic {
    #[cfg(feature = "loom")]
    pub use loom::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
    #[cfg(not(feature = "loom"))]
    pub use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
}

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
    /// Double-free detector: swapped to `true` by the last `ActorId::drop`
    /// right before the free. A second swap observing `true` is a protocol
    /// bug (two last-references) — the assert fires in every debug build
    /// and under the loom model, where the raw `Box::from_raw` double free
    /// would be silent UB. Compiled out of release builds: the swap is a
    /// locked instruction on every actor death (the S97 CreateActor pass)
    /// and the protocol it guards is verified by the model.
    #[cfg(any(debug_assertions, feature = "loom"))]
    freed: AtomicBool,
    /// The actor's debug name, stored inline — the C++ copies the name
    /// into its pool block too (`ActorInfo::init`'s `name_.assign` has no
    /// truncation); the 63-byte buffer covers every name in the tree (the
    /// longest is 37) without the boxed string's extra malloc per
    /// registration (the S37 CreateActor gate).
    name: ActorName,
    /// The block's allocation layout — the block is freed raw by the last
    /// `ActorId` drop (see [`SlotBlock`](Self::allocate_block)).
    block_layout: Layout,
    info: UnsafeCell<*mut ActorInfo>,
}

/// A small inline string — [`IdSlot::name`] (see the field docs).
pub(crate) struct ActorName {
    buf: [u8; 63],
    len: u8,
}

impl ActorName {
    fn new(name: &str) -> Self {
        debug_assert!(
            name.len() <= 63,
            "actor name overflows the inline buffer: {name}"
        );
        let bytes = name.as_bytes();
        let len = bytes.len().min(63);
        let mut buf = [0; 63];
        buf[..len].copy_from_slice(&bytes[..len]);
        Self {
            buf,
            len: len as u8,
        }
    }

    fn as_str(&self) -> &str {
        // SAFETY: the bytes were copied from a `&str` prefix.
        unsafe { std::str::from_utf8_unchecked(&self.buf[..self.len as usize]) }
    }
}

impl IdSlot {
    /// The actor's name for logging (the C++ `ActorInfo::get_name`).
    pub(crate) fn name_str(&self) -> &str {
        self.name.as_str()
    }
}

// SAFETY: `ref_cnt`, `alive` and `sched` are atomics, `name` is immutable
// after creation; `info` is written once at creation and only dereferenced on
// the actor's current scheduler thread (never from a foreign thread), so no
// data race on the pointee is possible through shared access to the slot.
unsafe impl Send for IdSlot {}
unsafe impl Sync for IdSlot {}

/// The port of C++ `ObjectPool<ActorInfo>`: registration blocks come from a
/// per-thread freelist instead of the global allocator. The C++ pool has one
/// fixed block size (the actor lives outside its `ActorInfo`); this port
/// co-locates the actor in the block, so the pool is *size-classed* — one
/// bucket per allocation layout, each capped so retention stays bounded.
///
/// Frees may land on any thread (a foreign `ActorId` copy can be the last
/// reference); the block then joins *that* thread's bucket, which is sound —
/// the memory is plain reusable storage, fully rewritten on the next
/// registration.
pub(crate) mod block_pool {
    use std::alloc::Layout;
    use std::cell::RefCell;

    /// Retention is unbounded, exactly like the C++ `ObjectPool` freelist
    /// (ObjectPool.h: `head_` is a plain lock-free stack; `get_storage`
    /// news only when it is empty, `release_storage` always pushes — no
    /// cap). Every earlier capped form here collapsed under batch-shaped
    /// load: `CreateActorBench` and the promise rows register *n* actors
    /// and destroy them in one later pump, so allocs precede frees and a
    /// capped pool misses almost every block in the measured passes (the
    /// S98 probe measured the 256-block cap at a ~0% hit rate — 2 global
    /// mallocs per actor cycle — where the unbounded C++ pool rides the
    /// calibration round at ~100%). The memory profile is the C++ one:
    /// each thread's pool grows to its peak block usage and stays there.
    /// This is a deliberate parity guarantee, not an oversight.
    ///
    /// Bucket-table cap: distinct layouts, not blocks, are bounded — the
    /// C++ equivalent is one pool per C++ type; 256 covers the actor zoo
    /// with room for the pooled thunk/future layouts (each bucket is one
    /// `Vec` header). The overflow falls back to the global allocator.
    const MAX_BUCKETS: usize = 256;

    thread_local! {
        static POOL: RefCell<Pool> = const { RefCell::new(Pool::new()) };
    }

    /// The pool state — `IdSlot` blocks plus the other pooled payloads that
    /// share it (the S98 pass: oversized send-closure thunks, future-actor
    /// boxes, mailbox buffers). The last-used bucket is cached: the hot
    /// loops alternate between a handful of layouts and the linear bucket
    /// scan itself was measurable on the per-iteration rows.
    struct Pool {
        buckets: Vec<(usize, usize, Vec<*mut u8>)>, // size, align, blocks
        last: Option<(usize, usize, usize)>,        // (size, align, bucket index)
    }

    impl Pool {
        const fn new() -> Self {
            Self {
                buckets: Vec::new(),
                last: None,
            }
        }

        fn bucket_of(&mut self, size: usize, align: usize) -> Option<usize> {
            if let Some((ls, la, idx)) = self.last
                && ls == size
                && la == align
            {
                return Some(idx);
            }
            self.buckets
                .iter()
                .position(|(s, a, _)| *s == size && *a == align)
                .inspect(|&idx| self.last = Some((size, align, idx)))
        }
    }

    pub(crate) fn alloc(layout: Layout) -> *mut u8 {
        let pooled = POOL
            .try_with(|pool| {
                let mut pool = pool.borrow_mut();
                pool.bucket_of(layout.size(), layout.align())
                    .and_then(|idx| pool.buckets[idx].2.pop())
            })
            .unwrap_or(None);
        pooled.unwrap_or_else(|| unsafe { std::alloc::alloc(layout) })
    }

    pub(crate) fn dealloc(ptr: *mut u8, layout: Layout) {
        let pooled = POOL
            .try_with(|pool| {
                let mut pool = pool.borrow_mut();
                match pool.bucket_of(layout.size(), layout.align()) {
                    Some(idx) => {
                        pool.buckets[idx].2.push(ptr);
                        true
                    }
                    None => {
                        if pool.buckets.len() < MAX_BUCKETS {
                            pool.buckets
                                .push((layout.size(), layout.align(), vec![ptr]));
                            pool.last =
                                Some((layout.size(), layout.align(), pool.buckets.len() - 1));
                            true
                        } else {
                            false
                        }
                    }
                }
            })
            .unwrap_or(false);
        if !pooled {
            // SAFETY: the layout matches the allocation (the caller carries
            // it alongside the pointer, as the block does).
            unsafe { std::alloc::dealloc(ptr, layout) };
        }
    }
}

/// The single allocation behind a registered actor — the C++ ObjectPool's
/// one block: `[IdSlot | ActorInfo | actor storage]`, carved with a raw
/// layout so the concrete `A`'s object lives inside it too. The S37
/// CreateActor gate measured the three separate mallocs of the earlier
/// design (slot+info, actor, name) against this shape; the slot sits at
/// offset 0, so its pointer doubles as the block base for the final free.
///
/// Lifecycle: `destroy_actor` drops the actor and the `ActorInfo` in place
/// when the actor dies; the block itself lives until the last `ActorId`
/// copy drops (the slot outlives the actor — sends to a dead actor are
/// no-ops that need the atomics) and is then returned to the per-thread
/// [`block_pool`] (or freed, over cap).
impl IdSlot {
    /// Allocate the block and write the slot (the caller writes the
    /// `ActorInfo` and the actor object into the returned pointers).
    pub(crate) fn allocate_block<A: Actor>(
        sched_id: i32,
        name: &str,
    ) -> (*mut IdSlot, *mut ActorInfo, *mut A) {
        let (layout, info_offset) = Layout::new::<IdSlot>()
            .extend(Layout::new::<ActorInfo>())
            .expect("slot + info layout");
        let (total, actor_offset) = layout
            .extend(Layout::new::<A>())
            .expect("slot + info + actor layout");
        // SAFETY: the layout has nonzero size (IdSlot is not a ZST) and the
        // block is written before it is shared.
        let block = block_pool::alloc(total);
        if block.is_null() {
            std::alloc::handle_alloc_error(total);
        }
        // SAFETY: freshly allocated memory, not yet aliased; the slot is
        // written here, the rest by the caller.
        unsafe {
            std::ptr::write(
                block.cast::<IdSlot>(),
                IdSlot {
                    ref_cnt: AtomicUsize::new(1),
                    alive: AtomicBool::new(true),
                    sched: AtomicI32::new(sched_id),
                    #[cfg(any(debug_assertions, feature = "loom"))]
                    freed: AtomicBool::new(false),
                    name: ActorName::new(name),
                    block_layout: total,
                    info: UnsafeCell::new(block.add(info_offset).cast()),
                },
            );
            (
                block.cast(),
                block.add(info_offset).cast(),
                block.add(actor_offset).cast(),
            )
        }
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
    pub(crate) fn from_raw_block(ptr: *mut IdSlot) -> Self {
        // ref_cnt starts at 1 inside the freshly written slot; the slot is
        // the block's first field, so the pointer is also the block base.
        Self {
            slot: NonNull::new(ptr),
            marker: PhantomData,
        }
    }

    pub(crate) fn empty_id() -> Self {
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
            Some(s) => self.slot_of(s).name.as_str(),
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

    /// Coerce a shared reference to the erased id form — the borrowed half of
    /// [`ActorRef`] (the S37 hot path: `send_closure(&id, ...)` takes no
    /// refcount). The type parameter is a zero-sized `PhantomData` marker, so
    /// `ActorId<A>` and `ActorId<dyn Actor>` share the representation and the
    /// cast is sound.
    pub(crate) fn erase_ref(&self) -> &ActorId<dyn Actor> {
        // SAFETY: see the docs — the only data is the slot pointer.
        unsafe { &*(self as *const ActorId<A> as *const ActorId<dyn Actor>) }
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

    /// The raw slot pointer (null for an empty id) — the S37 hot-path form:
    /// the dispatch context stores this instead of an owned copy, so the
    /// inline dispatch pays no refcount. The pointed-to slot stays alive via
    /// the copies owned by the scheduler lists and the senders (the same
    /// protocol as the C++ `ActorId` pool index).
    pub(crate) fn slot_ptr(&self) -> *const IdSlot {
        self.slot.map_or(std::ptr::null(), |s| s.as_ptr())
    }

    /// Rebuild an owned `ActorId` from a live slot pointer (the reverse of
    /// [`slot_ptr`](Self::slot_ptr)) — used by `Scheduler::current_actor`,
    /// which must hand out an owning copy.
    ///
    /// # Safety
    ///
    /// `slot` must point at a live `IdSlot` whose actor is the one being
    /// dispatched on this thread.
    pub(crate) unsafe fn from_slot_ptr(slot: NonNull<IdSlot>) -> Self {
        // SAFETY: the caller guarantees a live slot (see the fn docs).
        unsafe { slot.as_ref() }
            .ref_cnt
            .fetch_add(1, Ordering::AcqRel);
        Self {
            slot: Some(slot),
            marker: PhantomData,
        }
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
                #[cfg(any(debug_assertions, feature = "loom"))]
                assert!(
                    !slot.freed.swap(true, Ordering::AcqRel),
                    "IdSlot double free: two ActorId drops observed the last reference"
                );
                // SAFETY: last reference. The block's `ActorInfo` half (and
                // the actor inside it) were dropped in place by
                // `destroy_actor` if the actor died (an actor with zero id
                // copies cannot be alive); what remains is the slot itself,
                // dropped here, and the block, returned to the per-thread
                // pool (the C++ ObjectPool release) or freed with its
                // stored layout over cap.
                unsafe {
                    let layout = slot.block_layout;
                    std::ptr::drop_in_place(s.as_ptr());
                    let block = s.as_ptr() as *mut u8;
                    block_pool::dealloc(block, layout);
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

/// The C++ `ActorOwn()` default: an empty own.
impl<A: ?Sized> Default for ActorOwn<A> {
    fn default() -> Self {
        Self {
            id: ActorId::empty_id(),
        }
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

/// The C++ `ActorShared()` default: an empty shared reference (no hangup on
/// drop).
impl<A: ?Sized> Default for ActorShared<A> {
    fn default() -> Self {
        Self {
            id: ActorId::empty_id(),
            token: 0,
        }
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
///
/// A send-time actor reference is either a *borrowed* [`ActorId`] (the hot
/// path: `send_closure(&id, ...)` pays no refcount — the caller's owned copy
/// outlives the synchronous send, and the C++ `ActorId` is a plain value
/// with no such cost at all) or an owned one, when the caller hands over an
/// id by value.
#[derive(Clone, Debug)]
pub struct ActorRef<'a> {
    id: ActorRefId<'a>,
    pub(crate) token: u64,
}

#[derive(Clone, Debug)]
enum ActorRefId<'a> {
    Owned(ActorId<dyn Actor>),
    Borrowed(&'a ActorId<dyn Actor>),
}

impl ActorRef<'_> {
    pub(crate) fn with_token(mut self, token: u64) -> Self {
        self.token = token;
        self
    }

    /// The target id, borrowed (either variant).
    pub(crate) fn id(&self) -> &ActorId<dyn Actor> {
        match &self.id {
            ActorRefId::Owned(id) => id,
            ActorRefId::Borrowed(id) => id,
        }
    }
}

impl<'a, A: ?Sized> From<ActorId<A>> for ActorRef<'a> {
    fn from(id: ActorId<A>) -> Self {
        Self {
            id: ActorRefId::Owned(id.erase()),
            token: 0,
        }
    }
}

impl<'a, A: ?Sized> From<&'a ActorId<A>> for ActorRef<'a> {
    fn from(id: &'a ActorId<A>) -> Self {
        Self {
            id: ActorRefId::Borrowed(id.erase_ref()),
            token: 0,
        }
    }
}

impl<'a, A: ?Sized> From<ActorShared<A>> for ActorRef<'a> {
    fn from(shared: ActorShared<A>) -> Self {
        let token = shared.token();
        let id = shared.release();
        Self {
            id: ActorRefId::Owned(id.erase()),
            token,
        }
    }
}

impl<'a, A: ?Sized> From<&'a ActorShared<A>> for ActorRef<'a> {
    fn from(shared: &'a ActorShared<A>) -> Self {
        Self {
            id: ActorRefId::Borrowed(shared.id.erase_ref()),
            token: shared.token(),
        }
    }
}

impl<'a, A: ?Sized> From<&'a ActorOwn<A>> for ActorRef<'a> {
    fn from(own: &'a ActorOwn<A>) -> Self {
        Self {
            id: ActorRefId::Borrowed(own.id.erase_ref()),
            token: 0,
        }
    }
}
/// Loom model of the [`IdSlot`] protocol (S36 "loom check"): the scheduler
/// thread may kill the actor while a foreign thread holds a reference and
/// checks liveness. The free protocol is verified on both sides:
/// *exactly once* — the `freed` swap in `ActorId::drop` asserts no second
/// last-reference under any interleaving (a raw double `Box::from_raw` would
/// be silent UB the model cannot see); *at least once* — RAII plus
/// `handle.join()` structurally guarantees every spawned drop ran before the
/// model closure returns. A leak would have to be a missing drop, which the
/// model's structure rules out.
#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use super::{ActorId, IdSlot};
    use crate::actor::Actor;
    use crate::scheduler::{ActorInfo, ActorSlot};

    struct Dummy;

    impl Actor for Dummy {}

    fn make_id() -> ActorId {
        let (slot_ptr, info_ptr, _actor_ptr) = IdSlot::allocate_block::<Dummy>(0, "loom");
        // SAFETY: freshly allocated; the info is written before the slot is
        // shared.
        unsafe {
            let mut info = ActorInfo::new(slot_ptr, ActorSlot::Empty, false);
            info.fix_links();
            std::ptr::write(info_ptr, info);
        }
        ActorId::from_raw_block(slot_ptr)
    }

    #[test]
    fn slot_freed_after_last_ref_despite_death_race() {
        loom::model(|| {
            let id = make_id();
            let foreign = id.clone();
            let handle = loom::thread::Builder::new()
                .spawn(move || {
                    // The foreign send path: an acquire load of `alive`, then
                    // the reference is dropped.
                    let _alive = foreign.is_alive();
                    drop(foreign);
                })
                .expect("loom thread");
            // The owning scheduler thread publishes death; the local copy
            // keeps the slot alive until it is dropped below.
            id.mark_dead();
            drop(id);
            handle.join().expect("loom thread panicked");
        });
    }

    #[test]
    fn slot_outlives_all_local_copies() {
        loom::model(|| {
            let id = make_id();
            let clone = id.clone();
            // Two local copies on one thread: the slot is freed by the drop
            // that observes ref_cnt == 1, not by the first one out.
            drop(clone);
            assert!(id.is_alive());
            id.mark_dead();
            assert!(!id.is_alive());
            drop(id);
        });
    }
}
