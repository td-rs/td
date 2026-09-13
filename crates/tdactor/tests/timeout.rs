//! S35 coverage: the `Timeout`, `MultiTimeout` and `SleepActor` utilities of
//! `td/actor/{Timeout,MultiTimeout,SleepActor}.h` and the `AlarmManager` of
//! `td/telegram/AlarmManager.h` (the C++ tdactor tests don't cover them, so,
//! like the S34 promise tests, this is usage-shaped coverage).
//!
//! Construction pattern: `Timeout::new`/`MultiTimeout::new`/`AlarmManager::new`
//! register a borrowed actor on the current scheduler (the C++ member-actor
//! constructor), so owners are built under `get_main_guard()` and then handed
//! to `create_actor_unsafe` (which takes its own guard and rejects nesting).

use std::sync::{Arc, Mutex};

use tdactor::{
    Actor, ActorId, ActorOwn, ActorRef, ActorShared, AlarmManager, ConcurrentScheduler,
    MultiTimeout, Scheduler, SleepActor, Timeout,
};
use tdutils::promise::{Promise, PromiseCreator};
use tdutils::status::Result;

/// Stops the scheduler after `0` seconds — bounds every test even when the
/// code under test never fires.
struct Finisher(f64);

impl Actor for Finisher {
    fn start_up(&mut self) {
        self.set_timeout_in(self.0);
    }

    fn timeout_expired(&mut self) {
        Scheduler::instance().finish();
    }
}

fn run(sched: &mut ConcurrentScheduler) {
    sched.start();
    while sched.run_main(10.0) {}
    sched.finish();
}

/// The `send_closure_later(actor_id, &T::method, ..)` building block the S35
/// utilities are exercised through.
fn send_to<A: Actor, F>(id: ActorId<A>, handler: F)
where
    F: FnOnce(&mut A) + Send + 'static,
{
    tdactor::send_closure_later(
        ActorRef::from(id),
        Box::new(move |actor| {
            // SAFETY: the `ActorId<A>` marker names the actor's real type (the
            // C++ `static_cast` in `send_closure`).
            let actor = unsafe { &mut *(actor as *mut dyn Actor as *mut A) };
            handler(actor);
        }),
    );
}

/// C++ `Timeout` semantics: fires exactly once and reports the pending
/// timeout while armed.
#[test]
fn timeout_fires_once() {
    struct Owner {
        timeout: Box<Timeout>,
        fired: Arc<Mutex<Vec<&'static str>>>,
    }
    impl Actor for Owner {
        fn start_up(&mut self) {
            assert!(!self.timeout.has_timeout());
            assert_eq!(self.timeout.get_timeout(), 0.0);

            let owner = self.actor_id_of::<Self>();
            self.timeout.set_callback(Box::new(move || {
                send_to(owner, |owner| {
                    owner.fired.lock().unwrap().push("fired");
                    // The one-shot timeout consumed itself.
                    assert!(!owner.timeout.has_timeout());
                    assert_eq!(owner.timeout.get_timeout(), 0.0);
                    Scheduler::instance().finish();
                });
            }));
            self.timeout.set_timeout_in(0.02);
            assert!(self.timeout.has_timeout());
            let remaining = self.timeout.get_timeout();
            assert!(remaining > 0.0 && remaining <= 0.02);
        }
    }

    let fired = Arc::new(Mutex::new(Vec::new()));
    let mut sched = ConcurrentScheduler::new(0);
    let owner = {
        let _guard = sched.get_main_guard();
        Owner {
            timeout: Timeout::new(),
            fired: fired.clone(),
        }
    };
    sched.create_actor_unsafe(0, "Owner", owner);
    sched.create_actor_unsafe(0, "Finisher", Finisher(0.2));
    run(&mut sched);

    assert_eq!(*fired.lock().unwrap(), ["fired"]);
}

/// C++ `Timeout::cancel_timeout`: cancels the timeout and clears the
/// callback, so it never fires.
#[test]
fn timeout_cancel_prevents_fire() {
    struct Owner {
        timeout: Box<Timeout>,
        fired: Arc<Mutex<Vec<&'static str>>>,
    }
    impl Actor for Owner {
        fn start_up(&mut self) {
            let fired = self.fired.clone();
            self.timeout
                .set_callback(Box::new(move || fired.lock().unwrap().push("fired")));
            self.timeout.set_timeout_in(0.02);
            assert!(self.timeout.has_timeout());
            self.timeout.cancel_timeout();
            assert!(!self.timeout.has_timeout());
        }
    }

    let fired = Arc::new(Mutex::new(Vec::new()));
    let mut sched = ConcurrentScheduler::new(0);
    let owner = {
        let _guard = sched.get_main_guard();
        Owner {
            timeout: Timeout::new(),
            fired: fired.clone(),
        }
    };
    sched.create_actor_unsafe(0, "Owner", owner);
    sched.create_actor_unsafe(0, "Finisher", Finisher(0.1));
    run(&mut sched);

    assert!(fired.lock().unwrap().is_empty());
}

/// C++ `MultiTimeout` semantics: per-key expiries fire in deadline order,
/// `cancel_timeout` drops a pending key, `set_timeout_at` upserts (the key
/// fires once, at the latest deadline), `add_timeout_at` keeps the first
/// deadline (the memcache semantics).
#[test]
fn multi_timeout_ordering() {
    struct Owner {
        timeouts: Box<MultiTimeout>,
        fired: Arc<Mutex<Vec<i64>>>,
    }
    impl Actor for Owner {
        fn start_up(&mut self) {
            let fired = self.fired.clone();
            self.timeouts.set_callback(Box::new(move |key| {
                fired.lock().unwrap().push(key);
            }));

            // 1 fires at +0.10.
            self.timeouts.set_timeout_in(1, 0.10);
            // 2 and 3 share a deadline; 2 is canceled before it can fire.
            self.timeouts.set_timeout_in(2, 0.02);
            self.timeouts.set_timeout_in(3, 0.02);
            assert!(self.timeouts.has_timeout(2));
            self.timeouts.cancel_timeout(2);
            assert!(!self.timeouts.has_timeout(2));
            // add keeps the first deadline: 4 fires at +0.05, not +0.20.
            self.timeouts.add_timeout_in(4, 0.05);
            self.timeouts.add_timeout_in(4, 0.20);
            // set upserts: 10 fires once, at +0.15.
            self.timeouts.set_timeout_in(10, 0.05);
            self.timeouts.set_timeout_in(10, 0.15);
            assert!(!self.timeouts.has_timeout(99));
        }
    }

    let fired = Arc::new(Mutex::new(Vec::new()));
    let mut sched = ConcurrentScheduler::new(0);
    let owner = {
        let _guard = sched.get_main_guard();
        Owner {
            timeouts: MultiTimeout::new("TestTimeout"),
            fired: fired.clone(),
        }
    };
    sched.create_actor_unsafe(0, "Owner", owner);
    sched.create_actor_unsafe(0, "Finisher", Finisher(0.3));
    run(&mut sched);

    assert_eq!(*fired.lock().unwrap(), [3, 4, 1, 10]);
}

/// C++ `MultiTimeout::run_all`: fires every pending key immediately and
/// leaves no timeout behind.
#[test]
fn multi_timeout_run_all() {
    struct Owner {
        timeouts: Box<MultiTimeout>,
        fired: Arc<Mutex<Vec<i64>>>,
        ran_all: Arc<Mutex<bool>>,
    }
    impl Actor for Owner {
        fn start_up(&mut self) {
            let fired = self.fired.clone();
            self.timeouts.set_callback(Box::new(move |key| {
                fired.lock().unwrap().push(key);
            }));
            self.timeouts.set_timeout_in(5, 50.0);
            self.timeouts.set_timeout_in(6, 50.0);

            let owner = self.actor_id_of::<Self>();
            let ran_all = self.ran_all.clone();
            send_to(owner, move |owner| {
                owner.timeouts.run_all();
                // run_all drained the queue.
                assert!(!owner.timeouts.has_timeout(5));
                assert!(!owner.timeouts.has_timeout(6));
                *ran_all.lock().unwrap() = true;
            });
        }
    }

    let fired = Arc::new(Mutex::new(Vec::new()));
    let ran_all = Arc::new(Mutex::new(false));
    let mut sched = ConcurrentScheduler::new(0);
    let owner = {
        let _guard = sched.get_main_guard();
        Owner {
            timeouts: MultiTimeout::new("TestTimeout"),
            fired: fired.clone(),
            ran_all: ran_all.clone(),
        }
    };
    sched.create_actor_unsafe(0, "Owner", owner);
    sched.create_actor_unsafe(0, "Finisher", Finisher(0.1));
    run(&mut sched);

    assert_eq!(*fired.lock().unwrap(), [5, 6]);
    assert!(*ran_all.lock().unwrap());
}

/// C++ `SleepActor`: resolves the promise once and stops.
#[test]
fn sleep_actor_resolves_promise() {
    let resolved = Arc::new(Mutex::new(false));
    let promise: Promise<()> = {
        let resolved = resolved.clone();
        PromiseCreator::lambda(move |()| *resolved.lock().unwrap() = true)
    };

    let mut sched = ConcurrentScheduler::new(0);
    sched.create_actor_unsafe(0, "SleepActor", SleepActor::new(0.02, promise));
    sched.create_actor_unsafe(0, "Finisher", Finisher(0.2));
    run(&mut sched);

    assert!(*resolved.lock().unwrap());
}

/// C++ `AlarmManager::set_alarm`: rejects out-of-range seconds with 400 and
/// resolves valid alarms in deadline order.
#[test]
fn alarm_manager_validates_and_fires() {
    struct Owner {
        manager: ActorOwn<AlarmManager>,
        events: Arc<Mutex<Vec<(&'static str, i32)>>>,
    }
    impl Actor for Owner {
        fn start_up(&mut self) {
            // Out-of-range seconds: rejected with 400, promise failed inline.
            for (seconds, tag) in [(-1.0, "neg"), (4e9, "huge")] {
                let events = self.events.clone();
                let promise: Promise<()> = PromiseCreator::lambda_result(
                    move |result: Result<()>| match result {
                        Result::Ok(()) => events.lock().unwrap().push((tag, 0)),
                        Result::Err(status) => events.lock().unwrap().push((tag, status.code())),
                    },
                );
                send_to(self.manager.get(), move |manager| {
                    manager.set_alarm(seconds, promise);
                });
            }
            // Valid alarms resolve in deadline order.
            for (seconds, tag) in [(0.02, "first"), (0.04, "second")] {
                let events = self.events.clone();
                let promise: Promise<()> = PromiseCreator::lambda_result(
                    move |result: Result<()>| match result {
                        Result::Ok(()) => events.lock().unwrap().push((tag, 0)),
                        Result::Err(status) => events.lock().unwrap().push((tag, status.code())),
                    },
                );
                send_to(self.manager.get(), move |manager| {
                    manager.set_alarm(seconds, promise);
                });
            }
        }
    }

    let events = Arc::new(Mutex::new(Vec::new()));
    let mut sched = ConcurrentScheduler::new(0);
    let manager = {
        let _guard = sched.get_main_guard();
        AlarmManager::new(ActorShared::<()>::default())
    };
    let owner = Owner {
        manager: sched.create_actor_unsafe(0, "AlarmManager", *manager),
        events: events.clone(),
    };
    sched.create_actor_unsafe(0, "Owner", owner);
    sched.create_actor_unsafe(0, "Finisher", Finisher(0.2));
    run(&mut sched);

    assert_eq!(
        *events.lock().unwrap(),
        [("neg", 400), ("huge", 400), ("first", 0), ("second", 0)]
    );
}

/// C++ `AlarmManager::tear_down`: pending alarms fail with the request-abort
/// error (500) when the manager dies.
#[test]
fn alarm_manager_teardown_aborts_pending() {
    struct Owner {
        manager: ActorOwn<AlarmManager>,
        aborted: Arc<Mutex<Vec<i32>>>,
    }
    impl Actor for Owner {
        fn start_up(&mut self) {
            let aborted = self.aborted.clone();
            let promise: Promise<()> = PromiseCreator::lambda_result(
                move |result: Result<()>| match result {
                    Result::Ok(()) => aborted.lock().unwrap().push(0),
                    Result::Err(status) => aborted.lock().unwrap().push(status.code()),
                },
            );
            send_to(self.manager.get(), move |manager| {
                manager.set_alarm(50.0, promise);
            });
            // Dropping the own sends hangup → stop → tear_down, which aborts
            // the pending alarm (the hangup is queued after set_alarm).
            self.manager = ActorOwn::default();
        }
    }

    let aborted = Arc::new(Mutex::new(Vec::new()));
    let mut sched = ConcurrentScheduler::new(0);
    let manager = {
        let _guard = sched.get_main_guard();
        AlarmManager::new(ActorShared::<()>::default())
    };
    let owner = Owner {
        manager: sched.create_actor_unsafe(0, "AlarmManager", *manager),
        aborted: aborted.clone(),
    };
    sched.create_actor_unsafe(0, "Owner", owner);
    sched.create_actor_unsafe(0, "Finisher", Finisher(0.1));
    run(&mut sched);

    assert_eq!(*aborted.lock().unwrap(), [500]);
}
