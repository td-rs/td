//! Port of `td/actor/Timeout.h`, `td/actor/MultiTimeout.h` and
//! `td/actor/SleepActor.h`: the actor-level timeout utilities.
//!
//! # Mapping notes
//!
//! - The C++ constructors register the actor **by pointer** — the
//!   member-actor idiom: the object lives as a field of the owning actor and
//!   the scheduler holds a non-owning registration (`Deleter::None`). The
//!   Rust port heap-pins the object in a `Box` (a stable address, so moves of
//!   the owner don't dangle it) and registers through the borrowed-actor
//!   path, like S34's `FutureActor`. Construction therefore requires a
//!   scheduler context, exactly like C++ construction under
//!   `create_actor<T>(...)`. Dropping the `Box` drops the internal
//!   [`ActorOwn`], which sends `hangup` → `stop` and unregisters the borrowed
//!   actor without freeing it — the C++ registry entry, released at
//!   construction, is instead cleaned up here.
//! - C++ callbacks receive `(void *data, int64 key)` with `data` pointing at
//!   the owning actor (`set_callback_data(this)`); a sound Rust callback
//!   captures the owner's [`crate::ActorId`] (or other `Send + 'static` state)
//!   instead — see [`crate::alarm_manager`] for the canonical translation.
//! - The C++ timeout queue is a `KHeap<double>` + `std::set<Item>` with
//!   decrease-key `fix`; this port is a `BinaryHeap` + `HashMap` with lazy
//!   deletion (stale entries are skipped when popped), the same style as the
//!   scheduler's own timeout queue (S31 divergence). `set_timeout_at` re-arms
//!   the actor timeout unconditionally where C++ does it only when the heap
//!   top changed — semantically identical, constant factors stay with S37.
//! - The C++ `LOG_CHECK`s about a pending `Timeout` event in the actor's
//!   mailbox (scheduler-internal debug invariants) are not carried over.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};

use crate::actor::Actor;
use crate::actor_id::ActorOwn;
use crate::scheduler::Scheduler;

use tdutils::promise::Promise;

/// An absolute deadline in seconds on the adjusted monotonic clock — the C++
/// `KHeap<double>` key. `total_cmp` gives the heap a total order without
/// pretending f64 is (NaN deadlines never enter the queue; see
/// [`MultiTimeout::set_timeout_at`]).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Deadline(f64);

impl Eq for Deadline {}

impl Ord for Deadline {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl PartialOrd for Deadline {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The port of C++ `class Timeout`: a one-shot, self-registering timeout.
///
/// Create it as a `Box` field of an owning actor (the constructor registers
/// the borrowed actor `"Timeout"` on the current scheduler), set a callback,
/// and arm it; the callback runs once from `timeout_expired` and the timeout
/// rearms nothing. `cancel_timeout` clears the callback, so a callback
/// capturing the owner must be `Send` and must not borrow it.
pub struct Timeout {
    callback: Option<Box<dyn FnOnce() + Send>>,
    /// The registration of this borrowed actor. Kept inside the object (the
    /// C++ constructor `.release()`s its `ActorOwn` and leaks the entry);
    /// dropping it here unregisters cleanly on owner destruction.
    own: ActorOwn<Timeout>,
}

impl Timeout {
    /// The C++ constructor: registers the actor on the current scheduler.
    /// Requires a scheduler context — construct owners under
    /// `create_actor`/`start_up`, as in C++.
    #[allow(clippy::new_without_default)] // construction has the registration side effect (see above); no `Default`
    pub fn new() -> Box<Timeout> {
        // SAFETY: the actor is heap-pinned for its whole life (the owner
        // holds the `Box`); the internal `ActorOwn` is dropped first in the
        // field drop order, which unregisters before the memory is freed.
        let mut timeout = Box::new(Timeout {
            callback: None,
            own: ActorOwn::default(),
        });
        timeout.own =
            unsafe { Scheduler::instance().register_actor_ptr("Timeout", -1, &mut *timeout) };
        timeout
    }

    /// The port of C++ `set_callback` + `set_callback_data`: the callback is
    /// the pair, bound by the closure (capture the owner's `ActorId`, not a
    /// pointer to the owner).
    pub fn set_callback(&mut self, callback: Box<dyn FnOnce() + Send>) {
        self.callback = Some(callback);
    }

    /// The port of C++ `Timeout::has_timeout`.
    pub fn has_timeout(&self) -> bool {
        Scheduler::instance().get_actor_timeout(self.id()).is_some()
    }

    /// The port of C++ `Timeout::get_timeout`: seconds until the timeout
    /// fires, or `0.0`.
    pub fn get_timeout(&self) -> f64 {
        Scheduler::instance()
            .get_actor_timeout(self.id())
            .unwrap_or(0.0)
    }

    /// The port of C++ `Timeout::set_timeout_in`.
    pub fn set_timeout_in(&mut self, timeout: f64) {
        Scheduler::instance().set_actor_timeout_in(self.id(), timeout);
    }

    /// The port of C++ `Timeout::set_timeout_at` (an absolute deadline).
    pub fn set_timeout_at(&mut self, timeout_at: f64) {
        Scheduler::instance().set_actor_timeout_at(self.id(), timeout_at);
    }

    /// The port of C++ `Timeout::cancel_timeout`: cancels the timeout and
    /// clears the callback, so the callback never fires after this.
    pub fn cancel_timeout(&mut self) {
        if self.has_timeout() {
            Scheduler::instance().cancel_actor_timeout(self.id());
            self.callback = None;
        }
    }

    fn id(&self) -> crate::actor_id::ActorId {
        self.own.get().erase()
    }
}

impl Actor for Timeout {
    fn timeout_expired(&mut self) {
        debug_assert!(!self.has_timeout());
        // C++ CHECKs the callback and moves it out before the call; the same
        // one-shot semantics.
        let callback = self
            .callback
            .take()
            .expect("Timeout::timeout_expired without a callback");
        callback();
    }
}

/// The port of C++ `class MultiTimeout`: one actor timeout multiplexed over
/// any number of `i64` keys, firing the callback once per expired key.
///
/// Created and used as a `Box` field of an owning actor; the callback (set
/// via [`set_callback`](MultiTimeout::set_callback)) runs synchronously from
/// the MultiTimeout actor's own `timeout_expired`, in the same order as C++:
/// all expired keys are collected, the actor timeout is re-armed, then the
/// callback fires for each key.
pub struct MultiTimeout {
    callback: Option<Box<dyn FnMut(i64) + Send>>,
    /// key → absolute deadline (C++ `std::set<Item> items_`).
    key_deadline: HashMap<i64, Deadline>,
    /// min-heap of `(deadline, key)` (C++ `KHeap<double> timeout_queue_`);
    /// entries stale because of `set_timeout_at` upserts or cancels are
    /// dropped lazily when popped.
    timeout_queue: BinaryHeap<Reverse<(Deadline, i64)>>,
    own: ActorOwn<MultiTimeout>,
}

impl MultiTimeout {
    /// The C++ constructor: registers the actor under `name` on the current
    /// scheduler. Requires a scheduler context — construct owners under
    /// `create_actor`/`start_up`, as in C++.
    pub fn new(name: &str) -> Box<MultiTimeout> {
        // SAFETY: same as `Timeout::new`.
        let mut timeout = Box::new(MultiTimeout {
            callback: None,
            key_deadline: HashMap::new(),
            timeout_queue: BinaryHeap::new(),
            own: ActorOwn::default(),
        });
        timeout.own = unsafe { Scheduler::instance().register_actor_ptr(name, -1, &mut *timeout) };
        timeout
    }

    /// The port of C++ `set_callback` + `set_callback_data`: the callback is
    /// the pair, bound by the closure. It survives across fires (unlike
    /// [`Timeout`]) and may re-arm or cancel keys re-entrantly — the queue is
    /// already updated before the first call, as in C++.
    pub fn set_callback(&mut self, callback: Box<dyn FnMut(i64) + Send>) {
        self.callback = Some(callback);
    }

    /// The port of C++ `MultiTimeout::has_timeout`.
    pub fn has_timeout(&self, key: i64) -> bool {
        self.key_deadline.contains_key(&key)
    }

    /// The port of C++ `MultiTimeout::set_timeout_in`.
    pub fn set_timeout_in(&mut self, key: i64, timeout: f64) {
        self.set_timeout_at(key, tdutils::time::now() + timeout);
    }

    /// The port of C++ `MultiTimeout::add_timeout_in`.
    pub fn add_timeout_in(&mut self, key: i64, timeout: f64) {
        self.add_timeout_at(key, tdutils::time::now() + timeout);
    }

    /// The port of C++ `MultiTimeout::set_timeout_at`: upsert — replaces an
    /// existing timeout for `key`.
    ///
    /// # Panics
    /// If `timeout` is NaN (it would break the heap's ordering; C++ would
    /// silently misbehave instead).
    pub fn set_timeout_at(&mut self, key: i64, timeout: f64) {
        assert!(!timeout.is_nan(), "MultiTimeout::set_timeout_at(NaN)");
        let deadline = Deadline(timeout);
        self.key_deadline.insert(key, deadline);
        self.timeout_queue.push(Reverse((deadline, key)));
        self.update_timeout();
    }

    /// The port of C++ `MultiTimeout::add_timeout_at` — the memcache
    /// semantics: does not replace an existing timeout for `key`.
    ///
    /// # Panics
    /// If `timeout` is NaN.
    pub fn add_timeout_at(&mut self, key: i64, timeout: f64) {
        assert!(!timeout.is_nan(), "MultiTimeout::add_timeout_at(NaN)");
        if self.key_deadline.contains_key(&key) {
            return;
        }
        let deadline = Deadline(timeout);
        self.key_deadline.insert(key, deadline);
        self.timeout_queue.push(Reverse((deadline, key)));
        self.update_timeout();
    }

    /// The port of C++ `MultiTimeout::cancel_timeout`: a no-op for keys
    /// without a timeout.
    pub fn cancel_timeout(&mut self, key: i64) {
        if self.key_deadline.remove(&key).is_some() {
            self.update_timeout();
        }
    }

    /// The port of C++ `MultiTimeout::run_all`: fires the callback for every
    /// key with a timeout, immediately.
    pub fn run_all(&mut self) {
        let expired_keys = self.pop_expired_keys(tdutils::time::now_cached() + 1e10);
        if !expired_keys.is_empty() {
            self.update_timeout();
        }
        self.fire(expired_keys);
    }

    /// The port of C++ `MultiTimeout::update_timeout`: re-arm the actor
    /// timeout at the earliest deadline, or cancel it when no keys are left.
    /// C++ only does this when the heap top changed; doing it
    /// unconditionally re-arms with the same deadline, which is observably
    /// identical (the lazy scheduler heap absorbs the extra entries).
    fn update_timeout(&mut self) {
        match self.key_deadline.values().min() {
            Some(&Deadline(deadline)) => {
                Scheduler::instance().set_actor_timeout_at(self.id(), deadline)
            }
            None => Scheduler::instance().cancel_actor_timeout(self.id()),
        }
    }

    /// The port of C++ `MultiTimeout::get_expired_keys`: pop and erase every
    /// key with `deadline < now`, dropping stale heap entries on the way.
    fn pop_expired_keys(&mut self, now: f64) -> Vec<i64> {
        let mut expired_keys = Vec::new();
        while let Some(&Reverse((deadline, key))) = self.timeout_queue.peek() {
            if deadline.0 >= now {
                break;
            }
            self.timeout_queue.pop();
            if self.key_deadline.get(&key) == Some(&deadline) {
                self.key_deadline.remove(&key);
                expired_keys.push(key);
            }
        }
        expired_keys
    }

    /// The port of the tail of C++ `timeout_expired`/`run_all`: call the
    /// callback once per key.
    fn fire(&mut self, expired_keys: Vec<i64>) {
        if expired_keys.is_empty() {
            return;
        }
        let callback = self
            .callback
            .as_mut()
            .expect("MultiTimeout fired without a callback");
        for key in expired_keys {
            callback(key);
        }
    }

    fn id(&self) -> crate::actor_id::ActorId {
        self.own.get().erase()
    }
}

impl Actor for MultiTimeout {
    fn timeout_expired(&mut self) {
        // C++ order: collect, re-arm, then fire — so a callback may arm or
        // cancel keys re-entrantly.
        let expired_keys = self.pop_expired_keys(tdutils::time::now_cached());
        if !self.key_deadline.is_empty() {
            self.update_timeout();
        }
        self.fire(expired_keys);
    }
}

/// The port of C++ `class SleepActor`: a one-shot sleep that resolves a
/// [`Promise<()>`](Promise) and stops
/// (`ActorTraits<SleepActor>::need_start_up = true`, `need_context = false`
/// are the defaults here).
pub struct SleepActor {
    timeout: f64,
    promise: Promise<()>,
}

impl SleepActor {
    /// The port of C++ `SleepActor(timeout, promise)`.
    pub fn new(timeout: f64, promise: Promise<()>) -> Self {
        Self { timeout, promise }
    }
}

impl Actor for SleepActor {
    fn start_up(&mut self) {
        self.set_timeout_in(self.timeout);
    }

    fn timeout_expired(&mut self) {
        self.promise.set_value(());
        self.stop();
    }
}
