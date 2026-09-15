//! Ports of the MultiPromise tests from `tdactor/test/actors_simple.cpp`:
//! `TEST(Actors, MultiPromise)` (the Safe fan-in over ten sleeps, and the
//! never-registered Safe delivering its "Lost promise" error) and
//! `TEST(Actors, MultiPromiseSendClosureLater)` (a queued closure must run
//! before the fan-in result is delivered).

use std::sync::Arc;
use std::sync::Mutex;

use tdactor::{
    Actor, ConcurrentScheduler, MultiPromiseActor, MultiPromiseActorSafe, Scheduler, SleepActor,
    actor_messages, create_actor, send_closure_later,
};
use tdutils::promise::PromiseCreator;
use tdutils::status::Result;

/// C++ `MultiPromise2`: ten sleeps feed the fan-in; its single queued promise
/// must complete with `Ok`.
struct MultiPromise2;
impl Actor for MultiPromise2 {
    fn start_up(&mut self) {
        let promise = PromiseCreator::lambda_result(|result: Result<()>| {
            result.move_as_ok();
            Scheduler::instance().finish();
        });

        let mut multi_promise = MultiPromiseActorSafe::new("MultiPromiseActor2");
        multi_promise.add_promise(promise);
        for _ in 0..10 {
            create_actor("Sleep", SleepActor::new(0.1, multi_promise.get_promise())).release();
        }
        // `multi_promise` drops here — the fan-in actor must survive it.
    }
}

/// C++ `MultiPromise1`: a promise queued on a never-registered fan-in; the
/// handle's destructor must complete it with an error.
struct MultiPromise1;
impl Actor for MultiPromise1 {
    fn start_up(&mut self) {
        let promise = PromiseCreator::lambda_result(|result: Result<()>| {
            assert!(result.is_error(), "the lost promise must deliver an error");
            create_actor("B", MultiPromise2).release();
        });
        let mut multi_promise = MultiPromiseActorSafe::new("MultiPromiseActor1");
        multi_promise.add_promise(promise);
    }
}

/// C++ `TEST(Actors, MultiPromise)`.
#[test]
fn multi_promise() {
    let mut scheduler = ConcurrentScheduler::new(0);
    scheduler.create_actor_unsafe(0, "A", MultiPromise1);
    scheduler.start();
    while scheduler.run_main(10.0) {}
    scheduler.finish();
}

/// C++ `DelayedCall`: flips the shared step when its queued closure runs.
struct DelayedCall;
impl Actor for DelayedCall {}
impl DelayedCall {
    fn on_called(&mut self, step: Arc<Mutex<i32>>) {
        let mut step = step.lock().unwrap();
        assert_eq!(*step, 0);
        *step = 1;
    }
}
actor_messages! {
    enum DelayedCallMessage for DelayedCall {
        OnCalled {
            step: Arc<Mutex<i32>>,
        } => on_called(step),
    }
}

/// C++ `MultiPromiseSendClosureLaterTest`: the bare (member-actor) fan-in.
struct MultiPromiseSendClosureLaterTest {
    step: Arc<Mutex<i32>>,
    mpa: MultiPromiseActor,
}
impl Actor for MultiPromiseSendClosureLaterTest {
    fn start_up(&mut self) {
        let delayed = create_actor("DelayedCall", DelayedCall).release();
        self.mpa.add_promise(PromiseCreator::lambda({
            let step = self.step.clone();
            move |_unit: ()| {
                let mut step = step.lock().unwrap();
                assert_eq!(*step, 1, "queued closure must run before delivery");
                *step += 1;
                drop(step);
                Scheduler::instance().finish();
            }
        }));
        let mut lock = self.mpa.get_promise();
        send_closure_later(
            &delayed,
            DelayedCallMessage::OnCalled {
                step: self.step.clone(),
            },
        );
        lock.set_value(());
    }

    fn tear_down(&mut self) {
        assert_eq!(*self.step.lock().unwrap(), 2);
    }
}

/// C++ `TEST(Actors, MultiPromiseSendClosureLater)`.
#[test]
fn multi_promise_send_closure_later() {
    let mut scheduler = ConcurrentScheduler::new(0);
    scheduler.create_actor_unsafe(
        0,
        "MultiPromiseSendClosureLaterTest",
        MultiPromiseSendClosureLaterTest {
            step: Arc::new(Mutex::new(0)),
            mpa: MultiPromiseActor::new("MultiPromiseActor"),
        },
    );
    scheduler.start();
    let mut rounds = 0u32;
    while scheduler.run_main(1.0) {
        rounds += 1;
        assert!(rounds < 50, "test 2 did not converge after {rounds} rounds");
    }
    eprintln!("TEST2: loop exited after {rounds} rounds");
    scheduler.finish();
}
