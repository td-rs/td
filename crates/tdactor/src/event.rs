//! Port of `td/actor/impl/Event.h`: scheduler events.
//!
//! A small structure (the C++ one is 16 bytes) used to send events between
//! actors. There are some predefined types of events:
//!
//! - `NoType` — uninitialized event (also the moved-from state)
//! - `Start` — start actor
//! - `Stop` — stop actor
//! - `Yield` — wake up actor
//! - `Timeout` — some timeout has expired
//! - `Hangup` — hang up called
//! - `Raw` — just pass 8 bytes (the C++ `union Raw` is used for convenience)
//! - `Custom` — run an `FnOnce(&mut dyn Actor)` thunk: the payload of both
//!   closure halves of the C++ `send_closure` machinery (the
//!   `immediate_closure`/`delayed_closure` events; S34 promise RPC and the
//!   S33 message-enum dispatch in `crate::messages`)
//!
//! Events also carry a `link_token` (the token of the `ActorShared` they were
//! sent through, if any), which the scheduler exposes to the running actor via
//! `Actor::get_link_token` and uses to pick `hangup` vs `hangup_shared`.

use std::alloc::Layout;
use std::mem;

use crate::actor::Actor;

/// The number of words of inline storage in [`ActorThunk`]: closures up to
/// this size (the S37 bench hot path — every `send_closure` message that
/// fits) are stored inside the event itself with no allocation; larger
/// closures come from the per-thread [`block_pool`] (the C++ `ObjectPool`
/// semantics — the exact gap the S98 promise pass closed: the global
/// malloc/free pair per oversized closure was a quarter of the
/// immediate-future row); over-aligned closures fall back to a box. Three
/// words cover every inline bench payload (the promise-carrying messages
/// are far larger and pooled) at 72 bytes per `Event` — the S37
/// measurement.
const INLINE_WORDS: usize = 3;

/// The boxed form of a [`ActorThunk`] payload (a `Box<dyn FnOnce>` cannot be
/// spelled inline without tripping `clippy::type_complexity`).
pub type BoxedThunk = Box<dyn FnOnce(&mut dyn Actor) + Send>;

/// A `Custom` event payload: a one-shot thunk the scheduler runs on the
/// destination actor (the C++ `Event::immediate_closure`); public because the
/// free `send_closure_*` half of the S33 machinery hands these around.
///
/// Small closures are stored inline in the event — the C++ side allocates
/// every closure but serves it from a thread-local pool, so the port needs
/// the same zero-malloc hot path to stay within the S37 2× gate (the Rust
/// global allocator is an order of magnitude slower than td's pool).
/// Oversized closures take the pooled arm — the C++ `ObjectPool` shape
/// exactly.
pub enum ActorThunk {
    /// A closure stored in `data` (up to [`INLINE_WORDS`] words, at most
    /// `usize` alignment); `call` moves it out and runs it, `drop` drops it
    /// in place when the event is discarded without dispatch.
    Inline {
        data: [usize; INLINE_WORDS],
        call: unsafe fn(*mut u8, &mut dyn Actor),
        drop: unsafe fn(*mut u8),
    },
    /// A closure too large for the inline slot, stored in a
    /// [`block_pool`](crate::actor_id::block_pool) block (any alignment the
    /// closure needs, since buckets key on the full layout). `run`/`Drop`
    /// return the block to the pool — no global allocator traffic.
    Pooled {
        data: *mut u8,
        layout: Layout,
        call: unsafe fn(*mut u8, &mut dyn Actor),
        drop: unsafe fn(*mut u8),
    },
    /// A closure whose pool is unavailable (thread teardown) or whose
    /// allocation failed.
    Boxed(BoxedThunk),
}

// SAFETY: the thunk owns its payload exclusively; every arm holds either
// plain words, fn pointers, a `Send` boxed closure, or a pool block with a
// `F: Send + 'static` closure written in it. Moving the thunk moves the
// payload (the pooled block may then be freed on another thread — the
// pool's cross-thread contract).
unsafe impl Send for ActorThunk {}

impl ActorThunk {
    /// Store `f` inline when it fits, pool it when it does not.
    ///
    /// The bound is higher-ranked: the stored `call` field is an `unsafe fn`
    /// pointer, whose elided argument lifetime is late-bound, so `F` must be
    /// callable with any `&mut dyn Actor` borrow.
    pub(crate) fn new<F>(f: F) -> Self
    where
        F: for<'a> FnOnce(&'a mut dyn Actor) + Send + 'static,
    {
        if mem::size_of::<F>() <= mem::size_of::<[usize; INLINE_WORDS]>()
            && mem::align_of::<F>() <= mem::align_of::<usize>()
        {
            let mut thunk = ActorThunk::Inline {
                data: [0; INLINE_WORDS],
                call: Self::call_inline::<F>,
                drop: Self::drop_inline::<F>,
            };
            // SAFETY: `data` holds INLINE_WORDS words; the size and alignment
            // of `F` were checked above, so the closure fits and is properly
            // aligned.
            unsafe { std::ptr::write(thunk.data_ptr().cast::<F>(), f) };
            thunk
        } else {
            let layout = Layout::new::<F>();
            let data = crate::actor_id::block_pool::alloc(layout);
            if !data.is_null() {
                // SAFETY: the block is `layout`-sized and -aligned, freshly
                // taken from the pool (or the global allocator behind it),
                // and unaliased; `F` is written exactly once, here.
                unsafe { std::ptr::write(data.cast::<F>(), f) };
                ActorThunk::Pooled {
                    data,
                    layout,
                    call: Self::call_inline::<F>,
                    drop: Self::drop_inline::<F>,
                }
            } else {
                // The box coercion is a closure-lifetime generalization site:
                // an early-bound closure argument becomes late-bound here.
                ActorThunk::Boxed(Box::new(f))
            }
        }
    }

    /// `FnOnce::call_once` through the erased data pointer.
    unsafe fn call_inline<F>(data: *mut u8, actor: &mut dyn Actor)
    where
        F: for<'a> FnOnce(&'a mut dyn Actor),
    {
        // SAFETY: `data` points at a live, initialized `F` (the contract of
        // the `call` field); it is moved out and consumed by `call_once`.
        let f = unsafe { std::ptr::read(data.cast::<F>()) };
        f(actor);
    }

    /// Drop the stored closure without running it.
    unsafe fn drop_inline<F>(data: *mut u8) {
        // SAFETY: `data` points at a live, initialized `F` that was never run.
        unsafe { std::ptr::drop_in_place(data.cast::<F>()) };
    }

    fn data_ptr(&mut self) -> *mut u8 {
        match self {
            ActorThunk::Inline { data, .. } => data.as_mut_ptr().cast(),
            ActorThunk::Pooled { data, .. } => *data,
            ActorThunk::Boxed(_) => unreachable!("boxed thunks have no pooled data"),
        }
    }

    /// Consume the thunk and run it on the destination actor — the dispatch
    /// half of `Event::Custom`.
    pub(crate) fn run(self, actor: &mut dyn Actor) {
        let mut this = mem::ManuallyDrop::new(self);
        match &mut *this {
            ActorThunk::Inline { data, call, .. } => {
                // SAFETY: the closure is moved out and consumed by `call`; the
                // `ManuallyDrop` wrapper skips `ActorThunk::drop` afterwards,
                // so the moved-from storage is never dropped twice.
                unsafe { call(data.as_mut_ptr().cast(), actor) };
            }
            ActorThunk::Pooled {
                data, layout, call, ..
            } => {
                // SAFETY: same one-shot contract as the inline arm, plus the
                // block returning to the pool once the closure is consumed.
                unsafe {
                    call(*data, actor);
                    crate::actor_id::block_pool::dealloc(*data, *layout);
                }
            }
            ActorThunk::Boxed(f) => {
                // The box is taken out, run, and dropped here; the
                // `ManuallyDrop` wrapper skips the outer drop.
                let f = unsafe { std::ptr::read(f) };
                f(actor);
            }
        }
    }
}

impl Drop for ActorThunk {
    fn drop(&mut self) {
        match self {
            ActorThunk::Inline { data, drop, .. } => {
                // SAFETY: a dropped (never-run) inline thunk still holds a
                // live, initialized closure in `data` — run() is the only
                // consumer and it wraps the thunk in `ManuallyDrop`.
                unsafe { drop(data.as_mut_ptr().cast()) };
            }
            ActorThunk::Pooled {
                data, layout, drop, ..
            } => {
                // SAFETY: same never-run contract; the block returns to the
                // pool after the closure is dropped in place.
                unsafe {
                    drop(*data);
                    crate::actor_id::block_pool::dealloc(*data, *layout);
                }
            }
            ActorThunk::Boxed(_) => {}
        }
    }
}

impl From<Box<dyn FnOnce(&mut dyn Actor) + Send>> for ActorThunk {
    fn from(f: Box<dyn FnOnce(&mut dyn Actor) + Send>) -> Self {
        Self::from_boxed(f)
    }
}

impl ActorThunk {
    /// Build a thunk from an already-dyn boxed closure. The parameter is
    /// concrete on purpose: `Box::new(closure)` at the call site coerces to
    /// `Box<dyn FnOnce>` here, and that coercion is what generalizes the
    /// closure's argument lifetime — a generic `Into`/`From` bound would move
    /// the coercion behind a trait obligation and the lifetime check fails.
    ///
    /// The box itself becomes the stored closure (16 bytes — it fits the
    /// inline slot, so the event holds it without a second allocation).
    pub fn from_boxed(f: BoxedThunk) -> Self {
        ActorThunk::new(f)
    }
}

/// The predefined event kinds, in the same order as C++ `Event::Type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    NoType,
    Start,
    Stop,
    Yield,
    Timeout,
    Hangup,
    Raw,
    Custom,
}

/// Raw event payload — the `Event::Raw` union in C++ (`void *`, `uint32` and
/// `uint64` are all representable in one `u64`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Raw(pub u64);

impl Raw {
    /// The `u32` member of the C++ union.
    pub fn u32(self) -> u32 {
        self.0 as u32
    }

    /// The `u64` member of the C++ union.
    pub fn u64(self) -> u64 {
        self.0
    }

    /// The pointer member of the C++ union.
    pub fn ptr<T>(self) -> *mut T {
        self.0 as usize as *mut T
    }
}

impl From<u32> for Raw {
    fn from(value: u32) -> Self {
        Raw(u64::from(value))
    }
}

impl From<u64> for Raw {
    fn from(value: u64) -> Self {
        Raw(value)
    }
}

/// A scheduler event, moved by value exactly like the C++ one.
///
/// Only the scheduler mutates `link_token`; actors build events through the
/// factories and receive payloads through their `Actor` callbacks.
pub struct Event {
    pub(crate) ty: EventType,
    pub(crate) link_token: u64,
    pub(crate) raw: u64,
    pub(crate) custom: Option<ActorThunk>,
}

impl Event {
    /// Start the actor (`Actor::start_up`).
    pub fn start() -> Self {
        Self::of_type(EventType::Start)
    }

    /// Stop the actor (`Actor::tear_down`).
    pub fn stop() -> Self {
        Self::of_type(EventType::Stop)
    }

    /// Wake up the actor (`Actor::wakeup`); `yield` is a Rust keyword.
    pub fn yield_() -> Self {
        Self::of_type(EventType::Yield)
    }

    /// Notify the actor that a timeout has expired (`Actor::timeout_expired`).
    pub fn timeout() -> Self {
        Self::of_type(EventType::Timeout)
    }

    /// Hang up the actor (`Actor::hangup` or `Actor::hangup_shared` depending
    /// on the event's link token).
    pub fn hangup() -> Self {
        Self::of_type(EventType::Hangup)
    }

    /// Pass raw payload bytes (`Actor::raw_event`).
    pub fn raw(raw: Raw) -> Self {
        Self {
            ty: EventType::Raw,
            link_token: 0,
            raw: raw.0,
            custom: None,
        }
    }

    /// Run a closure on the destination actor (`Actor` method called by the
    /// scheduler, inline or through the mailbox) — the C++
    /// `Event::immediate_closure` (the `send_closure` thunk).
    pub fn custom(closure: ActorThunk) -> Self {
        Self {
            ty: EventType::Custom,
            link_token: 0,
            raw: 0,
            custom: Some(closure),
        }
    }

    /// A moved-from, empty event — the C++ `Event()` default.
    pub fn no_type() -> Self {
        Self::of_type(EventType::NoType)
    }

    fn of_type(ty: EventType) -> Self {
        Self {
            ty,
            link_token: 0,
            raw: 0,
            custom: None,
        }
    }
}
