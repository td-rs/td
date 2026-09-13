//! Ports of the scheduler-level tdactor tests (`tdactor/test/actors_simple.cpp`
//! and `actors_main.cpp`): `SendLater`, `simple_hand_yield`, `do_after_stop`,
//! `call_after_destruct`, `later`, `send_from_other_threads`, `link_token`,
//! `simple_migrate`, plus a timeout test for the `set_timeout_*` machinery.
//! The promise tests (`promise`, `FastPromise`) live in `promise.rs` (S34).
//!
//! The tests that still follow in S33/S35: `MultiPromise`,
//! `MultiPromiseSendClosureLater` (S33 closures),
//! `always_wait_for_mailbox` (S35 `SleepActor`) and file-descriptor
//! `open_close`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tdactor::{
    Actor, ActorOwn, ActorShared, ConcurrentScheduler, Event, Raw, Scheduler, create_actor,
    send_event, send_event_later,
};

fn run_until_finished(sched: &mut ConcurrentScheduler) {
    let mut guard = 0;
    while sched.run_main(10.0) {
        guard += 1;
        assert!(guard < 100, "scheduler did not finish");
    }
}

/// C++ `TEST(Actors, SendLater)`: `send_event` runs the handler inline when
/// the actor is idle, `send_event_later` always goes through the mailbox.
#[test]
fn send_later_semantics() {
    let sb: Arc<Mutex<String>> = Arc::default();
    struct Worker {
        sb: Arc<Mutex<String>>,
    }
    impl Actor for Worker {
        fn raw_event(&mut self, raw: Raw) {
            self.sb.lock().unwrap().push(raw.u32() as u8 as char);
        }
    }

    let mut sched = ConcurrentScheduler::new(0);
    let id = {
        let _guard = sched.get_main_guard();
        create_actor("Worker", Worker { sb: sb.clone() })
    };
    sched.start();
    {
        let _guard = sched.get_main_guard();
        // Processes the Start event (start_up → yield → wakeup → loop_).
        let _ = sched.run_main_no_guard(0.01);
        assert_eq!(sb.lock().unwrap().as_str(), "");

        send_event(&id, Event::raw(Raw::from('A' as u32))); // inline
        send_event_later(&id, Event::raw(Raw::from('A' as u32))); // mailbox
        send_event(&id, Event::raw(Raw::from('A' as u32))); // mailbox not empty → queued
        assert_eq!(sb.lock().unwrap().as_str(), "A");

        let _ = sched.run_main_no_guard(0.01);
        assert_eq!(sb.lock().unwrap().as_str(), "AAA");
    }
    sched.finish();
}

/// C++ `TEST(Actors, simple_hand_yield)`: `yield` must add the actor to the
/// end of the ready queue (round-robin dispatch).
#[test]
fn simple_hand_yield() {
    let sb: Arc<Mutex<String>> = Arc::default();
    struct PrintChar {
        ch: char,
        cnt: usize,
        sb: Arc<Mutex<String>>,
    }
    impl Actor for PrintChar {
        fn start_up(&mut self) {
            self.yield_();
        }
        fn wakeup(&mut self) {
            if self.cnt == 0 {
                self.stop();
            } else {
                self.sb.lock().unwrap().push(self.ch);
                self.cnt -= 1;
                self.yield_();
            }
        }
    }

    const CNT: usize = 1000;
    let mut sched = ConcurrentScheduler::new(0);
    {
        let _guard = sched.get_main_guard();
        create_actor(
            "PrintA",
            PrintChar {
                ch: 'A',
                cnt: CNT,
                sb: sb.clone(),
            },
        )
        .release();
        create_actor(
            "PrintB",
            PrintChar {
                ch: 'B',
                cnt: CNT,
                sb: sb.clone(),
            },
        )
        .release();
        create_actor(
            "PrintC",
            PrintChar {
                ch: 'C',
                cnt: CNT,
                sb: sb.clone(),
            },
        )
        .release();
    }
    sched.start();
    let _ = sched.run_main(1.0);
    let expected = "ABC".repeat(CNT);
    assert_eq!(sb.lock().unwrap().as_str(), expected.as_str());
    sched.finish();
}

/// C++ `TEST(Actors, do_after_stop)`: `stop()` only sets a flag — the actor
/// is usable until the current event returns, and destroyed after it.
#[test]
fn do_after_stop() {
    let dropped = Arc::new(AtomicBool::new(false));
    struct DoAfterStop {
        dropped: Arc<AtomicBool>,
    }
    impl Actor for DoAfterStop {
        fn loop_(&mut self) {
            let ptr = Box::new(10u32);
            self.stop();
            assert_eq!(*ptr, 10);
            Scheduler::instance().finish();
        }
    }
    impl Drop for DoAfterStop {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    let mut sched = ConcurrentScheduler::new(0);
    sched.create_actor_unsafe(
        0,
        "DoAfterStop",
        DoAfterStop {
            dropped: dropped.clone(),
        },
    );
    sched.start();
    run_until_finished(&mut sched);
    assert!(dropped.load(Ordering::SeqCst));
    sched.finish();
}

/// C++ `TEST(Actors, call_after_destruct)`: sends to a destroyed actor are
/// dropped, and the master is torn down before its slave gets the hangup.
#[test]
fn call_after_destruct() {
    let master_dropped = Arc::new(AtomicBool::new(false));
    let slave_teardown = Arc::new(AtomicBool::new(false));
    let delivered = Arc::new(AtomicUsize::new(0));

    struct Slave {
        master: tdactor::ActorId<Master>,
        teardown: Arc<AtomicBool>,
    }
    impl Actor for Slave {
        fn hangup(&mut self) {
            // The master may already be destroyed — this must be a no-op.
            send_event(&self.master, Event::raw(Raw::from(1u32)));
        }
        fn tear_down(&mut self) {
            self.teardown.store(true, Ordering::SeqCst);
        }
    }

    struct Master {
        slave: Option<ActorOwn<Slave>>,
        dropped: Arc<AtomicBool>,
        delivered: Arc<AtomicUsize>,
        teardown: Arc<AtomicBool>,
    }
    impl Actor for Master {
        fn loop_(&mut self) {
            self.slave = Some(create_actor(
                "Slave",
                Slave {
                    master: self.actor_id_of::<Master>(),
                    teardown: self.teardown.clone(),
                },
            ));
            self.stop();
        }
        fn raw_event(&mut self, _raw: Raw) {
            self.delivered.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Drop for Master {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    let mut sched = ConcurrentScheduler::new(0);
    sched.create_actor_unsafe(
        0,
        "Master",
        Master {
            slave: None,
            dropped: master_dropped.clone(),
            delivered: delivered.clone(),
            teardown: slave_teardown.clone(),
        },
    );
    sched.start();
    // Run a bounded number of iterations; the scheduler only finishes when
    // the ConcurrentScheduler is dropped, so drive it until quiescent.
    for _ in 0..5 {
        let _ = sched.run_main(0.01);
    }
    assert!(master_dropped.load(Ordering::SeqCst));
    assert_eq!(delivered.load(Ordering::SeqCst), 0);
    sched.finish();
    drop(sched);
    assert!(slave_teardown.load(Ordering::SeqCst));
}

/// C++ `TEST(Actors, later)`: clearing a vector of `ActorOwn`s sends inline
/// hangups ("A"), the queued follow-ups run afterwards ("B").
#[test]
fn later_aaabbb() {
    let sb: Arc<Mutex<String>> = Arc::default();
    struct LaterSlave {
        // Held only for its Drop (sends the hangup to the parent).
        _parent: ActorShared<LaterMaster>,
        sb: Arc<Mutex<String>>,
    }
    impl Actor for LaterSlave {
        fn hangup(&mut self) {
            self.sb.lock().unwrap().push('A');
            send_event_later(self.actor_id(), Event::raw(Raw::from(0u32)));
        }
        fn raw_event(&mut self, _raw: Raw) {
            self.sb.lock().unwrap().push('B');
            self.stop();
        }
    }

    struct LaterMaster {
        children: Vec<ActorOwn<LaterSlave>>,
        cnt: usize,
        sb: Arc<Mutex<String>>,
    }
    impl Actor for LaterMaster {
        fn start_up(&mut self) {
            for _ in 0..self.cnt {
                self.children.push(create_actor(
                    "LaterSlave",
                    LaterSlave {
                        _parent: self.actor_shared_of::<LaterMaster>(u64::MAX),
                        sb: self.sb.clone(),
                    },
                ));
            }
            self.yield_();
        }
        fn loop_(&mut self) {
            self.children.clear();
        }
        fn hangup_shared(&mut self) {
            self.cnt -= 1;
            if self.cnt == 0 {
                Scheduler::instance().finish();
                self.stop();
            }
        }
    }

    let mut sched = ConcurrentScheduler::new(0);
    sched.create_actor_unsafe(
        0,
        "LaterMaster",
        LaterMaster {
            children: Vec::new(),
            cnt: 3,
            sb: sb.clone(),
        },
    );
    sched.start();
    run_until_finished(&mut sched);
    assert_eq!(sb.lock().unwrap().as_str(), "AAABBB");
    sched.finish();
}

/// C++ `TEST(Actors, send_from_other_threads)`: unrelated threads send
/// through `get_send_guard()`.
#[test]
fn send_from_other_threads() {
    let remaining = Arc::new(AtomicUsize::new(10));
    struct Listener {
        remaining: Arc<AtomicUsize>,
    }
    impl Actor for Listener {
        fn raw_event(&mut self, _raw: Raw) {
            if self.remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
                Scheduler::instance().finish();
            }
        }
    }

    let mut sched = ConcurrentScheduler::new(1);
    let listener = sched.create_actor_unsafe(
        1,
        "Listener",
        Listener {
            remaining: remaining.clone(),
        },
    );
    sched.start();
    let send_sched = sched.send_scheduler();
    let mut threads = Vec::new();
    for _ in 0..10 {
        let id = listener.get();
        threads.push(std::thread::spawn(move || {
            let _guard = send_sched.guard(false);
            send_event(&id, Event::raw(Raw::from(0u32)));
        }));
    }
    run_until_finished(&mut sched);
    for thread in threads {
        thread.join().unwrap();
    }
    sched.finish();
}

/// C++ `TEST(Actors, link_token)` (simplified, without `send_closure`): the
/// link token travels with immediate and mailbox events, and a hangup with a
/// token is routed to `hangup_shared`.
#[test]
fn link_token() {
    struct LinkSlave {
        // Held only for its Drop (sends the hangup with token 123 back).
        _parent: ActorShared<LinkMaster>,
    }
    impl Actor for LinkSlave {
        fn raw_event(&mut self, raw: Raw) {
            assert_eq!(raw.u64(), self.get_link_token());
        }
    }

    struct LinkMaster {
        child: Option<ActorOwn<LinkSlave>>,
    }
    impl Actor for LinkMaster {
        fn start_up(&mut self) {
            self.child = Some(create_actor(
                "LinkSlave",
                LinkSlave {
                    _parent: self.actor_shared_of::<LinkMaster>(123),
                },
            ));
            self.yield_();
        }
        fn loop_(&mut self) {
            let child = self.child.as_ref().unwrap().get();
            for token in 1u64..=100 {
                let shared = ActorShared::new(child.clone(), token);
                let event = Event::raw(Raw::from(token));
                if token % 2 == 0 {
                    send_event(shared, event);
                } else {
                    send_event_later(shared, event);
                }
            }
            send_event(&child, Event::hangup()); // slave stops → its ActorShared drops
        }
        fn hangup_shared(&mut self) {
            assert_eq!(self.get_link_token(), 123);
            Scheduler::instance().finish();
            self.stop();
        }
    }

    let mut sched = ConcurrentScheduler::new(0);
    sched.create_actor_unsafe(0, "LinkMaster", LinkMaster { child: None });
    sched.start();
    run_until_finished(&mut sched);
    sched.finish();
}

/// The `set_timeout_in`/`get_timeout`/`has_timeout`/`cancel_timeout` proxies
/// and the timeout event (C++ `td/actor/Timeout.h` semantics).
#[test]
fn actor_timeouts() {
    let expired = Arc::new(AtomicUsize::new(0));
    struct TimeoutActor {
        expired: Arc<AtomicUsize>,
    }
    impl Actor for TimeoutActor {
        fn start_up(&mut self) {
            self.set_timeout_in(10.0);
            assert!(self.has_timeout());
            let remaining = self.get_timeout();
            assert!(remaining > 0.0 && remaining <= 10.0);
            self.cancel_timeout();
            assert!(!self.has_timeout());
            assert_eq!(self.get_timeout(), 0.0);
            self.set_timeout_in(0.02);
        }
        fn timeout_expired(&mut self) {
            if self.expired.fetch_add(1, Ordering::SeqCst) == 0 {
                self.set_timeout_in(0.02);
            } else {
                Scheduler::instance().finish();
                self.stop();
            }
        }
    }

    let mut sched = ConcurrentScheduler::new(0);
    sched.create_actor_unsafe(
        0,
        "TimeoutActor",
        TimeoutActor {
            expired: expired.clone(),
        },
    );
    sched.start();
    run_until_finished(&mut sched);
    assert_eq!(expired.load(Ordering::SeqCst), 2);
    sched.finish();
}

/// C++ `TEST(Actors, simple_migrate)` semantics: an actor migrates between
/// schedulers; events sent to it before, during and after the migration are
/// all delivered on the target scheduler.
#[test]
fn simple_migrate() {
    let received = Arc::new(Mutex::new(0usize));
    let on_start = Arc::new(AtomicUsize::new(0));
    let on_finish = Arc::new(AtomicUsize::new(0));

    struct Migrator {
        received: Arc<Mutex<usize>>,
        on_start: Arc<AtomicUsize>,
        on_finish: Arc<AtomicUsize>,
    }
    impl Actor for Migrator {
        fn start_up(&mut self) {
            self.migrate(2);
        }
        fn on_start_migrate(&mut self, _sched_id: i32) {
            self.on_start.fetch_add(1, Ordering::SeqCst);
        }
        fn on_finish_migrate(&mut self) {
            self.on_finish.fetch_add(1, Ordering::SeqCst);
        }
        fn raw_event(&mut self, _raw: Raw) {
            let mut received = self.received.lock().unwrap();
            *received += 1;
            let done = *received == 3;
            drop(received);
            if done {
                Scheduler::instance().finish();
                self.stop();
            }
        }
    }

    struct Sender {
        target: tdactor::ActorId<Migrator>,
    }
    impl Actor for Sender {
        fn start_up(&mut self) {
            for _ in 0..3 {
                send_event_later(&self.target, Event::raw(Raw::from(0u32)));
            }
        }
    }

    let mut sched = ConcurrentScheduler::new(2);
    let migrator = sched.create_actor_unsafe(
        1,
        "Migrator",
        Migrator {
            received: received.clone(),
            on_start: on_start.clone(),
            on_finish: on_finish.clone(),
        },
    );
    sched.create_actor_unsafe(
        0,
        "Sender",
        Sender {
            target: migrator.get(),
        },
    );
    sched.start();
    run_until_finished(&mut sched);
    assert_eq!(on_start.load(Ordering::SeqCst), 1);
    assert_eq!(on_finish.load(Ordering::SeqCst), 1);
    assert_eq!(*received.lock().unwrap(), 3);
    sched.finish();
}
