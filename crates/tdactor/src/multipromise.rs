//! Port of `td/actor/MultiPromise.{h,cpp}`: a fan-in promise — every
//! `get_promise()` hands out a promise whose resolution is deferred until the
//! `MultiPromiseActor` is stopped, and all handed-out promises resolve
//! together with the shared result.
//!
//! # Mapping notes
//!
//! - The C++ `MultiPromiseActor` is a **member actor**: registered by pointer
//!   on the first `get_promise()` (the `empty()` check), with
//!   `ActorTraits<MultiPromiseActor>::need_start_up = true` — so unlike the
//!   S34 `FutureActor` it gets `Event::start` and, critically, a
//!   `tear_down`. The Rust port passes `need_start_up = true` to the pointer
//!   registration for exactly that parity.
//! - C++ `MultiPromiseActorSafe` owns the member and, in its destructor,
//!   transfers a registered actor to the scheduler (`register_existing_actor`)
//!   so it survives the wrapper. The Rust `MultiPromiseActorSafe` keeps the
//!   `Box` alive for the same lifetime (the scheduler borrows it), so no
//!   transfer is needed; the unregistered case (promises added, `get_promise`
//!   never called — the C++ `MultiPromise` test's error path) drops the actor
//!   and its promises, and dropped promises fail with the standard
//!   dropped-promise error.
//! - `Result<Unit>` default-constructs to the success value in C++, so a
//!   `tear_down` without `set_result` resolves every promise with `Ok(())`;
//!   the port seeds `result` with `Ok(())` the same way. The result is
//!   cloned for all but the last promise (`set_result(result.clone())`).

use std::cell::Cell;

use tdutils::promise::Promise;
use tdutils::status::Result;

use crate::actor::Actor;
use crate::actor_id::ActorId;
use crate::event::{Event, Raw};
use crate::promise::{FutureActor, PromiseFuture, create_promise_from_promise_actor};
use crate::scheduler::Scheduler;

/// The port of C++ `class MultiPromiseActor` (an `Actor` +
/// `MultiPromiseInterface`): registered lazily on the first
/// [`get_promise`](MultiPromiseActor::get_promise), collects one raw event per
/// outstanding future, and resolves every promise together on
/// `tear_down`/completion.
pub struct MultiPromiseActor {
    name: String,
    /// C++ `Actor::empty()` — `false` once the actor is registered.
    registered: Cell<bool>,
    /// The actor's own id, captured at registration for `future.set_event`.
    self_id: Option<ActorId>,
    promises: Vec<Promise<()>>,
    // `Box` is load-bearing (not a clippy `vec_box` false positive to fix by
    // unboxing): every `FutureActor` is registered with the scheduler by
    // pointer (`register_actor_ptr`), so moving a bare value inside the
    // growing `Vec` would dangle the registration.
    #[allow(clippy::vec_box)]
    futures: Vec<Box<FutureActor<()>>>,
    received_results: usize,
    ignore_errors: bool,
    result: Result<()>,
}

impl MultiPromiseActor {
    /// C++ `MultiPromiseActor(name)`.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            registered: Cell::new(false),
            self_id: None,
            promises: Vec::new(),
            futures: Vec::new(),
            received_results: 0,
            ignore_errors: false,
            result: Result::Ok(()),
        }
    }

    /// C++ `MultiPromiseInterface::add_promise`.
    pub fn add_promise(&mut self, promise: Promise<()>) {
        self.promises.push(promise);
    }

    /// C++ `MultiPromiseInterface::get_promise`: registers the actor on the
    /// first call, then hands out one more promise tied to the shared result.
    pub fn get_promise(&mut self) -> Promise<()> {
        if !self.registered.get() {
            let name = self.name.clone();
            let own = unsafe {
                Scheduler::instance().register_actor_ptr(&name, -1, self as *mut Self, true)
            };
            self.registered.set(true);
            self.self_id = Some(own.get().erase());
        }
        assert!(
            !self.promises.is_empty(),
            "MultiPromiseActor::get_promise without add_promise"
        );

        let mut pf = PromiseFuture::<()>::new();
        let promise = pf.move_promise();
        let mut future = pf.move_future();
        future.set_event(crate::promise::EventFull::new(
            self.self_id.clone().expect("registered"),
            Event::raw(Raw::from(0u32)),
        ));
        self.futures.push(future);
        create_promise_from_promise_actor(promise)
    }

    /// C++ `MultiPromiseInterface::set_ignore_errors`.
    pub fn set_ignore_errors(&mut self, ignore_errors: bool) {
        self.ignore_errors = ignore_errors;
    }

    /// C++ `MultiPromiseInterface::promise_count`.
    pub fn promise_count(&self) -> usize {
        self.promises.len()
    }

    /// Whether the actor was registered — the C++ `Actor::empty()` check the
    /// `MultiPromiseActorSafe` destructor makes.
    pub fn is_registered(&self) -> bool {
        self.registered.get()
    }

    fn is_alive(&self) -> bool {
        self.self_id.as_ref().is_some_and(|id| id.is_alive())
    }

    /// C++ `set_result`: stash the shared result and stop (the `tear_down`
    /// then delivers it).
    fn set_result(&mut self, result: Result<()>) {
        self.result = result;
        self.stop();
    }

    fn raw_event_impl(&mut self, _raw: Raw) {
        self.received_results += 1;
        if self.received_results == self.futures.len() {
            if !self.ignore_errors {
                for future in &mut self.futures {
                    let result = future.move_as_result();
                    if result.is_error() {
                        return self.set_result(result);
                    }
                }
            }
            self.set_result(Result::Ok(()));
        }
    }

    fn tear_down_impl(&mut self) {
        // MultiPromiseActor must be cleared before it begins to send out the
        // result (a promise may re-enter).
        let promises = std::mem::take(&mut self.promises);
        self.futures.clear();
        self.received_results = 0;
        let result = std::mem::replace(&mut self.result, Result::Ok(()));

        let mut promises = promises.into_iter();
        let last = promises.next_back();
        for promise in promises {
            let mut promise = promise;
            promise.set_result(result.clone());
        }
        if let Some(mut promise) = last {
            promise.set_result(result);
        }
    }
}

impl Actor for MultiPromiseActor {
    fn raw_event(&mut self, raw: Raw) {
        self.raw_event_impl(raw);
    }

    fn tear_down(&mut self) {
        self.tear_down_impl();
    }
}

/// The port of C++ `class MultiPromiseActorSafe`: a non-actor wrapper owning
/// the [`MultiPromiseActor`] and forwarding the interface; unlike the raw
/// actor it is usable before any scheduler context exists (the C++ version
/// additionally transfers a registered actor to the scheduler in its
/// destructor — the Rust port keeps the `Box` alive instead, which the
/// borrowed registration makes equivalent).
pub struct MultiPromiseActorSafe {
    inner: Option<Box<MultiPromiseActor>>,
}

impl MultiPromiseActorSafe {
    /// C++ `MultiPromiseActorSafe(name)`.
    pub fn new(name: &str) -> Self {
        Self {
            inner: Some(Box::new(MultiPromiseActor::new(name))),
        }
    }

    fn get(&mut self) -> &mut MultiPromiseActor {
        self.inner.as_mut().expect("MultiPromiseActorSafe emptied")
    }

    /// C++ `MultiPromiseInterface::add_promise`.
    pub fn add_promise(&mut self, promise: Promise<()>) {
        self.get().add_promise(promise);
    }

    /// C++ `MultiPromiseInterface::get_promise`.
    pub fn get_promise(&mut self) -> Promise<()> {
        self.get().get_promise()
    }

    /// C++ `MultiPromiseInterface::set_ignore_errors`.
    pub fn set_ignore_errors(&mut self, ignore_errors: bool) {
        self.get().set_ignore_errors(ignore_errors);
    }

    /// C++ `MultiPromiseInterface::promise_count`.
    pub fn promise_count(&self) -> usize {
        self.inner
            .as_ref()
            .expect("MultiPromiseActorSafe emptied")
            .promise_count()
    }
}

impl Drop for MultiPromiseActorSafe {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        // The C++ destructor transfers a registered actor to the scheduler
        // (`register_existing_actor(...).release()`), so it outlives the
        // wrapper and is torn down at scheduler close. The port mirrors this
        // by leaking the pinned `Box` while the scheduler still borrows it:
        // the borrowed registration is never freed by the scheduler (the S31
        // divergence for member actors), and `tear_down` still runs through
        // the live registration. An unregistered or already-destroyed actor
        // is dropped normally (its promises fail as dropped, the C++
        // `MultiPromise` error path).
        if inner.is_registered() && inner.is_alive() {
            std::mem::forget(inner);
        }
    }
}
