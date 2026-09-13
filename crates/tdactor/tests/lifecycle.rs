//! S36 lifecycle coverage: close semantics (C++ `Scheduler::clear` runs under
//! a guard, C++ `ConcurrentScheduler::finish` wakes every scheduler), the
//! no-leak guarantee (`tear_down`/`Drop` exactly once), and sends attempted
//! during close.
//!
//! The C++ tests leave most of this to the machinery itself; these pin the
//! observable lifecycle contract.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tdactor::{
    Actor, ActorId, ConcurrentScheduler, Event, Raw, Scheduler, create_actor, send_event_later,
};

/// Per-actor lifecycle counters.
#[derive(Clone, Default)]
struct Counts {
    started: Arc<AtomicUsize>,
    torn_down: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

struct Counted {
    counts: Counts,
    /// Another actor to poke from `tear_down`. During close it may already be
    /// dead (the send must be dropped silently); on a live scheduler it must
    /// be delivered.
    other: Option<ActorId>,
}

impl Actor for Counted {
    fn start_up(&mut self) {
        self.counts.started.fetch_add(1, Ordering::SeqCst);
    }

    fn tear_down(&mut self) {
        self.counts.torn_down.fetch_add(1, Ordering::SeqCst);
        // The scheduler context must still be reachable from `tear_down`
        // (C++ Scheduler::clear takes a guard) even while the scheduler is
        // closing; the sends themselves are dropped via close_flag.
        if let Some(other) = &self.other {
            send_event_later(other, Event::raw(Raw::from(1u32)));
        }
        send_event_later(self.actor_id(), Event::raw(Raw::from(2u32)));
        Scheduler::instance().finish();
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.counts.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

/// Dropping the `ConcurrentScheduler` (C++ `Scheduler::clear`) tears down and
/// drops every remaining actor — even actors that were created but never run
/// — with a live scheduler context, and sends from `tear_down` are dropped
/// (to a dead actor, to a closing actor, to itself) instead of panicking.
#[test]
fn close_tears_down_all_actors_with_a_live_context() {
    let sched = ConcurrentScheduler::new(0);
    let counts_a = Counts::default();
    let counts_b = Counts::default();
    {
        let _guard = sched.get_main_guard();
        let own_a = create_actor(
            "a",
            Counted {
                counts: counts_a.clone(),
                other: None,
            },
        );
        let id_a = own_a.get();
        let own_b = create_actor(
            "b",
            Counted {
                counts: counts_b.clone(),
                other: Some(id_a.erase()),
            },
        );
        drop(own_a); // hangup queued behind the pending start
        drop(own_b);
    }
    // No start(), no run_main(): nothing was ever dispatched.
    drop(sched);

    for counts in [&counts_a, &counts_b] {
        assert_eq!(counts.torn_down.load(Ordering::SeqCst), 1, "tear_down");
        assert_eq!(counts.dropped.load(Ordering::SeqCst), 1, "Drop");
        assert_eq!(counts.started.load(Ordering::SeqCst), 0, "never started");
    }
}

/// A hangup stops the actor exactly once: start_up ran, tear_down ran, Drop
/// ran once — and the loop finishes instead of spinning forever.
#[test]
fn hangup_stops_actor_exactly_once() {
    let mut sched = ConcurrentScheduler::new(0);
    sched.start();
    let counts = Counts::default();
    {
        let _guard = sched.get_main_guard();
        let own = create_actor(
            "once",
            Counted {
                counts: counts.clone(),
                other: None,
            },
        );
        drop(own);
    }
    while sched.run_main(0.1) {}
    assert_eq!(counts.started.load(Ordering::SeqCst), 1);
    assert_eq!(counts.torn_down.load(Ordering::SeqCst), 1);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
}

/// `finish()` wakes the worker schedulers (C++ `ConcurrentScheduler::on_finish`
/// wakes all of `schedulers_`), so the join is prompt and the worker's
/// remaining actors are torn down on their own thread.
#[test]
fn finish_wakes_workers_and_tears_down_their_actors() {
    let mut sched = ConcurrentScheduler::new(1);
    let counts = Counts::default();
    sched.create_actor_unsafe(
        1,
        "worker",
        Counted {
            counts: counts.clone(),
            other: None,
        },
    );
    sched.start();
    let began = Instant::now();
    sched.finish(); // joins the worker thread
    // Without worker wakeups this join stalls until the 10 s idle poll fires.
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "finish() stalled for {:?}",
        began.elapsed()
    );
    // Whether the worker ran its start before `finish` is racy (the same
    // race exists in C++: the thread loop checks `is_finished` between
    // runs); the contract is the prompt join plus the teardown below.
    assert!(counts.started.load(Ordering::SeqCst) <= 1);
    assert_eq!(counts.torn_down.load(Ordering::SeqCst), 1);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
}
