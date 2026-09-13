//! Ports of the remaining `tdactor/test/actors_simple.cpp` suites that S35
//! unlocks: `MultiPromise`, `MultiPromiseSendClosureLater` (both need the
//! `SleepActor`), `always_wait_for_mailbox` and `stop_in_teardown`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tdactor::{
    Actor, ActorId, ConcurrentScheduler, MultiPromiseActorSafe, Scheduler, SleepActor,
    actor_messages, create_actor, send_closure, send_closure_later, send_lambda_later,
};
use tdutils::promise::{Promise, PromiseCreator};
use tdutils::status::Result;

fn run_until_finished(sched: &mut ConcurrentScheduler) {
    let mut guard = 0;
    while sched.run_main(10.0) {
        guard += 1;
        assert!(guard < 1000, "scheduler did not finish");
    }
}

/// C++ `TEST(Actors, MultiPromise)`: the outer multipromise fails (its only
/// promise is dropped with the unregistered actor), which chains into the
/// inner one where ten `SleepActor`s resolve through the shared result.
#[test]
fn multi_promise() {
    struct MultiPromise2;
    impl Actor for MultiPromise2 {
        fn start_up(&mut self) {
            let promise: Promise<()> = PromiseCreator::lambda_result(|result: Result<()>| {
                assert!(result.is_ok(), "inner multipromise must succeed");
                Scheduler::instance().finish();
            });

            let mut multi_promise = MultiPromiseActorSafe::new("MultiPromiseActor2");
            multi_promise.add_promise(promise);
            for _ in 0..10 {
                let promise = multi_promise.get_promise();
                create_actor("Sleep", SleepActor::new(0.02, promise)).release();
            }
            // The wrapper is dropped here: the registered actor outlives it
            // (the C++ `register_existing_actor` transfer), the sleeps
            // resolve, the shared result reaches the finish promise.
        }
    }

    struct MultiPromise1 {
        done: Arc<AtomicUsize>,
    }
    impl Actor for MultiPromise1 {
        fn start_up(&mut self) {
            let done = self.done.clone();
            let promise: Promise<()> = PromiseCreator::lambda_result(move |result: Result<()>| {
                // The outer multipromise actor is dropped unregistered: its
                // promises fail with the dropped-promise error.
                assert!(result.is_error());
                done.fetch_add(1, Ordering::SeqCst);
                create_actor("B", MultiPromise2);
            });
            let mut multi_promise = MultiPromiseActorSafe::new("MultiPromiseActor1");
            multi_promise.add_promise(promise);
            // No `get_promise`: the actor is never registered, the C++
            // destructor takes the drop path.
        }
    }

    let done = Arc::new(AtomicUsize::new(0));
    let mut sched = ConcurrentScheduler::new(0);
    sched.create_actor_unsafe(0, "A", MultiPromise1 { done: done.clone() });
    sched.start();
    run_until_finished(&mut sched);
    sched.finish();
    assert_eq!(done.load(Ordering::SeqCst), 1);
}

/// C++ `TEST(Actors, MultiPromiseSendClosureLater)`: the promise callback
/// must observe the earlier `send_closure_later` having run — registration
/// order (the delayed call is created first) decides.
#[test]
fn multi_promise_send_closure_later() {
    struct DelayedCall {
        step: Arc<Mutex<i32>>,
    }
    impl Actor for DelayedCall {}
    impl DelayedCall {
        fn on_called(&mut self) {
            let mut step = self.step.lock().unwrap();
            assert_eq!(*step, 0);
            *step = 1;
        }
    }
    actor_messages! {
        enum DelayedCallMessage for DelayedCall {
            OnCalled => on_called(),
        }
    }

    struct TestActor {
        step: Arc<Mutex<i32>>,
        delayed_call: Option<ActorId<DelayedCall>>,
        mpa: MultiPromiseActorSafe,
    }
    impl Actor for TestActor {
        fn start_up(&mut self) {
            let delayed_call = create_actor(
                "DelayedCall",
                DelayedCall {
                    step: self.step.clone(),
                },
            );
            self.delayed_call = Some(delayed_call.get());

            let step = self.step.clone();
            self.mpa.add_promise(PromiseCreator::lambda(move |()| {
                let mut step = step.lock().unwrap();
                assert_eq!(*step, 1);
                *step += 1;
                Scheduler::instance().finish();
            }));

            let mut lock = self.mpa.get_promise();
            send_closure_later(
                self.delayed_call.as_ref().unwrap(),
                DelayedCallMessage::OnCalled,
            );
            lock.set_value(());
        }

        fn tear_down(&mut self) {
            assert_eq!(*self.step.lock().unwrap(), 2);
        }
    }

    let step = Arc::new(Mutex::new(0));
    let mut sched = ConcurrentScheduler::new(0);
    sched.create_actor_unsafe(
        0,
        "MultiPromiseSendClosureLaterTest",
        TestActor {
            step: step.clone(),
            delayed_call: None,
            mpa: MultiPromiseActorSafe::new("MultiPromiseActor"),
        },
    );
    sched.start();
    run_until_finished(&mut sched);
    sched.finish();
    assert_eq!(*step.lock().unwrap(), 2);
}

/// C++ `TEST(Actors, always_wait_for_mailbox)`: two queued messages must both
/// run before the reply — the actor waits for its mailbox even when the first
/// message finishes the work.
#[test]
fn always_wait_for_mailbox() {
    struct AlwaysWaitForMailbox {
        was_f: Arc<AtomicUsize>,
    }
    impl Actor for AlwaysWaitForMailbox {
        fn start_up(&mut self) {
            let actor_id: ActorId<Self> = self.actor_id_of::<Self>();
            let was_f = self.was_f.clone();
            create_actor(
                "Sleep",
                SleepActor::new(
                    0.1,
                    PromiseCreator::lambda(move |()| {
                        assert_eq!(was_f.load(Ordering::SeqCst), 0);
                        for _ in 0..2 {
                            send_lambda_later(
                                actor_id.clone(),
                                |actor: &mut AlwaysWaitForMailbox| {
                                    actor.g();
                                },
                            );
                        }
                    }),
                ),
            )
            .release();
        }
    }
    impl AlwaysWaitForMailbox {
        fn f(&mut self) {
            self.was_f.store(1, Ordering::SeqCst);
            Scheduler::instance().finish();
        }
        fn g(&mut self) {
            send_closure(self.actor_id_of::<Self>(), WaiterMessage::F);
        }
    }
    actor_messages! {
        enum WaiterMessage for AlwaysWaitForMailbox {
            F => f(),
        }
    }

    let was_f = Arc::new(AtomicUsize::new(0));
    let mut sched = ConcurrentScheduler::new(0);
    sched.create_actor_unsafe(
        0,
        "AlwaysWaitForMailbox",
        AlwaysWaitForMailbox {
            was_f: was_f.clone(),
        },
    );
    sched.start();
    run_until_finished(&mut sched);
    sched.finish();
    assert_eq!(was_f.load(Ordering::SeqCst), 1);
}

/// C++ `TEST(Actors, stop_in_teardown)`: `stop()` from `tear_down` must not
/// recurse — the actor is already being destroyed.
#[test]
fn stop_in_teardown() {
    struct StopInTeardown {
        torn_down: Arc<AtomicUsize>,
    }
    impl Actor for StopInTeardown {
        fn loop_(&mut self) {
            self.stop();
        }

        fn tear_down(&mut self) {
            self.stop();
            Scheduler::instance().finish();
            self.torn_down.fetch_add(1, Ordering::SeqCst);
        }
    }

    let torn_down = Arc::new(AtomicUsize::new(0));
    let mut sched = ConcurrentScheduler::new(0);
    sched.create_actor_unsafe(
        0,
        "A",
        StopInTeardown {
            torn_down: torn_down.clone(),
        },
    );
    sched.start();
    run_until_finished(&mut sched);
    sched.finish();
    assert_eq!(torn_down.load(Ordering::SeqCst), 1);
}
