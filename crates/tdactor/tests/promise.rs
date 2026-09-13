//! Ports of the promise/future tdactor tests (`tdactor/test/actors_simple.cpp`):
//! `promise` (the tdutils `Promise` semantics) and `FastPromise`
//! (`PromiseFuture` with inline dispatch), plus S34 coverage the C++ tests
//! leave to the machinery itself: the dropped-promise hangup path and the
//! `send_promise_immediately` cross-actor RPC.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use tdactor::{
    Actor, ActorId, ConcurrentScheduler, FutureActor, PromiseActor, PromiseFuture, Scheduler,
    send_promise_immediately,
};
use tdutils::promise::{Promise, PromiseCreator};
use tdutils::status::{Result, Status};

/// C++ `TEST(Actors, promise)`: `PromiseCreator::lambda` semantics — a value
/// handler receives `T::default()` on error (the C++ `Auto()` conversion), a
/// result handler receives the error itself.
#[test]
fn promise() {
    let value = Arc::new(AtomicI32::new(-1));
    {
        let v = value.clone();
        let mut p1: Promise<i32> = PromiseCreator::lambda(move |x| v.store(x, Ordering::Relaxed));
        p1.set_error(Status::error_message("Test error"));
        assert_eq!(value.load(Ordering::Relaxed), 0);
    }
    {
        let v = value.clone();
        let mut p2: Promise<i32> = PromiseCreator::lambda_result(move |_x| {
            v.store(1, Ordering::Relaxed);
        });
        p2.set_error(Status::error_message("Test error"));
        assert_eq!(value.load(Ordering::Relaxed), 1);
    }
}

/// C++ `TEST(Actors, FastPromise)`: `PromiseActor::set_value` dispatches
/// inline to the idle `FutureActor` (no start event, empty mailbox), which
/// becomes ready synchronously; `move_as_ok` stops it.
#[test]
fn fast_promise() {
    struct FastPromise;
    impl Actor for FastPromise {
        fn start_up(&mut self) {
            let mut pf = PromiseFuture::<i32>::new();
            let mut promise = pf.move_promise();
            let mut future = pf.move_future();
            assert!(!future.is_ready());
            promise.set_value(123);
            assert_eq!(future.get_state(), tdactor::FutureActorState::Ready);
            assert_eq!(future.move_as_ok(), 123);
            Scheduler::instance().finish();
        }
    }

    let mut sched = ConcurrentScheduler::new(0);
    let _a = sched.create_actor_unsafe(0, "FastPromise", FastPromise);
    sched.start();
    while sched.run_main(10.0) {}
    sched.finish();
}

/// The dropped-promise path: an unfulfilled [`PromiseActor`] sends
/// `hangup`, and the future turns into `FutureActor::HANGUP_ERROR_CODE`.
#[test]
fn future_hangup() {
    struct Dropper;
    impl Actor for Dropper {
        fn start_up(&mut self) {
            let mut pf = PromiseFuture::<i32>::new();
            let promise = pf.move_promise();
            let mut future = pf.move_future();
            assert!(!future.is_ready());
            drop(promise); // ~PromiseActor → close() → hangup (inline)
            assert!(future.is_error());
            assert_eq!(
                future.move_as_error().code(),
                FutureActor::<i32>::HANGUP_ERROR_CODE
            );
            Scheduler::instance().finish();
        }
    }

    let mut sched = ConcurrentScheduler::new(0);
    let _a = sched.create_actor_unsafe(0, "Dropper", Dropper);
    sched.start();
    while sched.run_main(10.0) {}
    sched.finish();
}

/// `send_promise_immediately`: the cross-actor RPC — the callee completes the
/// promise, the caller reads the result on its future. The callee must be
/// idle for the inline dispatch (its mailbox is empty), so the caller yields
/// once first.
#[test]
fn send_promise_immediately_semantics() {
    struct Callee;
    impl Actor for Callee {}

    struct Caller {
        callee: ActorId<Callee>,
    }
    impl Actor for Caller {
        fn start_up(&mut self) {
            self.yield_();
        }
        fn wakeup(&mut self) {
            let mut future = send_promise_immediately(
                self.callee.clone(),
                |_: &mut Callee, mut promise: PromiseActor<i32>| {
                    promise.set_value(40 + 2);
                },
            );
            assert!(future.is_ok());
            assert_eq!(future.move_as_ok(), 42);
            Scheduler::instance().finish();
        }
    }

    let mut sched = ConcurrentScheduler::new(0);
    let callee = sched.create_actor_unsafe(0, "Callee", Callee);
    let _caller = sched.create_actor_unsafe(
        0,
        "Caller",
        Caller {
            callee: callee.get(),
        },
    );
    sched.start();
    while sched.run_main(10.0) {}
    sched.finish();
}

/// `send_promise_immediately` to an actor that drops the promise: the future
/// observes `HANGUP_ERROR_CODE` — the cross-actor "no result" convention.
#[test]
fn send_promise_immediately_dropped() {
    struct Silent;
    impl Actor for Silent {}

    struct Caller {
        silent: ActorId<Silent>,
    }
    impl Actor for Caller {
        fn start_up(&mut self) {
            self.yield_();
        }
        fn wakeup(&mut self) {
            let mut future = send_promise_immediately(
                self.silent.clone(),
                |_: &mut Silent, _promise: PromiseActor<i32>| {},
            );
            assert!(future.is_error());
            assert_eq!(
                future.move_as_error().code(),
                FutureActor::<i32>::HANGUP_ERROR_CODE
            );
            Scheduler::instance().finish();
        }
    }

    let mut sched = ConcurrentScheduler::new(0);
    let silent = sched.create_actor_unsafe(0, "Silent", Silent);
    let _caller = sched.create_actor_unsafe(
        0,
        "Caller",
        Caller {
            silent: silent.get(),
        },
    );
    sched.start();
    while sched.run_main(10.0) {}
    sched.finish();
}

/// The error half of the RPC: the callee reports a status, the caller reads
/// it back with `move_as_error`.
#[test]
fn send_promise_immediately_error() {
    struct Failing;
    impl Actor for Failing {}

    struct Caller {
        failing: ActorId<Failing>,
    }
    impl Actor for Caller {
        fn start_up(&mut self) {
            self.yield_();
        }
        fn wakeup(&mut self) {
            let mut future = send_promise_immediately(
                self.failing.clone(),
                |_: &mut Failing, mut promise: PromiseActor<Result<i32>>| {
                    promise.set_error(Status::error(123, "callee failed"));
                },
            );
            assert!(future.is_error());
            let status = future.move_as_error();
            assert_eq!(status.code(), 123);
            assert_eq!(status.message(), "callee failed");
            Scheduler::instance().finish();
        }
    }

    let mut sched = ConcurrentScheduler::new(0);
    let failing = sched.create_actor_unsafe(0, "Failing", Failing);
    let _caller = sched.create_actor_unsafe(
        0,
        "Caller",
        Caller {
            failing: failing.get(),
        },
    );
    sched.start();
    while sched.run_main(10.0) {}
    sched.finish();
}
