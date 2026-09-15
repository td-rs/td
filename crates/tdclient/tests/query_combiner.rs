//! QueryCombiner behavior tests (tdlib ships no dedicated unit test — the
//! class is covered there only through its call sites): identical queries
//! combine into one send, results broadcast to every waiter, errors
//! propagate, promise-less queries ride the `min_delay` window while promised
//! ones bypass it, and a closed combiner never sends.

use std::sync::{Arc, Mutex};

use tdactor::{
    Actor, ActorOwn, ConcurrentScheduler, Scheduler, SleepActor, actor_messages, create_actor,
    send_closure,
};
use tdclient::query_combiner::{QueryCombiner, QueryCombinerMessage};
use tdutils::promise::{Promise, PromiseCreator};
use tdutils::status::{Result, Status};

/// The slot a test's `send_query` promise drops the result promise into.
type Slot = Arc<Mutex<Option<Promise<()>>>>;
/// `(tag, ok)` completion records.
type Log = Arc<Mutex<Vec<(&'static str, bool)>>>;

/// C++ `send_query` argument: a `Promise<Promise<()>>` whose fulfillment hands
/// the combiner the promise to complete the query with. An error here is the
/// dropped/ignored promise (silent, like the C++ destroyed interface).
fn send_query_to(slot: Slot) -> Promise<Promise<()>> {
    PromiseCreator::lambda_result(move |result: Result<Promise<()>>| {
        if let Result::Ok(promise) = result {
            *slot.lock().unwrap() = Some(promise);
        }
    })
}

fn recorder(log: Log, tag: &'static str) -> Promise<()> {
    PromiseCreator::lambda_result(move |result: Result<()>| {
        log.lock().unwrap().push((tag, result.is_ok()));
    })
}

struct Driver {
    combiner: Option<ActorOwn<QueryCombiner>>,
    delayed_combiner: Option<ActorOwn<QueryCombiner>>,
    slot: Slot,
    delayed_slot: Slot,
    log: Log,
    step: usize,
}
impl Actor for Driver {
    fn start_up(&mut self) {
        self.combiner = Some(create_actor(
            "QueryCombiner",
            QueryCombiner::new(0.0, Box::new(|| false)),
        ));
        self.delayed_combiner = Some(create_actor(
            "DelayedQueryCombiner",
            QueryCombiner::new(0.05, Box::new(|| false)),
        ));
        self.yield_();
    }

    fn loop_(&mut self) {
        self.run_step();
    }
}
actor_messages! {
    enum DriverMessage for Driver {
        DelayedTick {
            tick: u8,
        } => on_delayed_tick(tick),
    }
}

impl Driver {
    fn combiner_id(&self) -> tdactor::ActorId<QueryCombiner> {
        self.combiner.as_ref().expect("created in start_up").get()
    }

    fn delayed_combiner_id(&self) -> tdactor::ActorId<QueryCombiner> {
        self.delayed_combiner
            .as_ref()
            .expect("created in start_up")
            .get()
    }

    fn run_step(&mut self) {
        match self.step {
            0 => {
                // Two waiters on the same query id → exactly one send.
                send_closure(
                    &self.combiner_id(),
                    QueryCombinerMessage::AddQuery {
                        query_id: 1,
                        send_query: send_query_to(self.slot.clone()),
                        promise: recorder(self.log.clone(), "a"),
                    },
                );
                send_closure(
                    &self.combiner_id(),
                    QueryCombinerMessage::AddQuery {
                        query_id: 1,
                        // a promised add never uses the send_query promise
                        send_query: Promise::default(),
                        promise: recorder(self.log.clone(), "b"),
                    },
                );
                self.step = 1;
                self.yield_();
            }
            1 => {
                let mut fulfill = self
                    .slot
                    .lock()
                    .unwrap()
                    .take()
                    .expect("query 1 must be sent");
                fulfill.set_value(());
                self.step = 2;
                self.yield_();
            }
            2 => {
                assert_eq!(
                    *self.log.lock().unwrap(),
                    vec![("a", true), ("b", true)],
                    "both waiters must receive the combined result"
                );
                // Error path: the combined query fails, the waiter gets it.
                send_closure(
                    &self.combiner_id(),
                    QueryCombinerMessage::AddQuery {
                        query_id: 2,
                        send_query: send_query_to(self.slot.clone()),
                        promise: recorder(self.log.clone(), "c"),
                    },
                );
                self.step = 3;
                self.yield_();
            }
            3 => {
                let mut fulfill = self
                    .slot
                    .lock()
                    .unwrap()
                    .take()
                    .expect("query 2 must be sent");
                fulfill.set_error(Status::error_message("boom"));
                self.step = 4;
                self.yield_();
            }
            4 => {
                assert_eq!(
                    self.log.lock().unwrap().last(),
                    Some(&("c", false)),
                    "the error must reach the waiter"
                );
                // A promised query sends immediately even before any window
                // opened (the C++ `next_query_time_` starts at construction;
                // it only spaces sends AFTER each one).
                send_closure(
                    &self.delayed_combiner_id(),
                    QueryCombinerMessage::AddQuery {
                        query_id: 8,
                        send_query: send_query_to(self.delayed_slot.clone()),
                        promise: recorder(self.log.clone(), "d"),
                    },
                );
                self.step = 5;
                self.yield_();
            }
            5 => {
                // id 8 went out and opened the 0.05 s window; a promise-less
                // query must now ride it.
                let mut fulfill = self
                    .delayed_slot
                    .lock()
                    .unwrap()
                    .take()
                    .expect("promised query must send immediately");
                fulfill.set_value(());
                send_closure(
                    &self.delayed_combiner_id(),
                    QueryCombinerMessage::AddQuery {
                        query_id: 7,
                        send_query: send_query_to(self.delayed_slot.clone()),
                        promise: Promise::default(),
                    },
                );
                self.step = 6;
                self.yield_();
            }
            6 => {
                assert!(
                    self.delayed_slot.lock().unwrap().is_none(),
                    "promise-less query must ride the delay window"
                );
                // Arm the delayed-send check.
                let timer_promise = PromiseCreator::lambda_result({
                    let id = self.actor_id_of::<Driver>();
                    move |result: Result<()>| {
                        result.move_as_ok();
                        send_closure(&id, DriverMessage::DelayedTick { tick: 0 });
                    }
                });
                create_actor("Timer", SleepActor::new(0.2, timer_promise)).release();
                self.step = 7;
                self.yield_();
            }
            _ => {}
        }
    }

    fn on_delayed_tick(&mut self, _tick: u8) {
        let mut fulfill = self
            .delayed_slot
            .lock()
            .unwrap()
            .take()
            .expect("delayed query must be sent after the window");
        fulfill.set_value(());
        assert_eq!(
            *self.log.lock().unwrap(),
            vec![("a", true), ("b", true), ("c", false), ("d", true)]
        );
        Scheduler::instance().finish();
    }
}

#[test]
fn query_combiner_combines_broadcasts_and_delays() {
    let mut scheduler = ConcurrentScheduler::new(0);
    scheduler.create_actor_unsafe(
        0,
        "Driver",
        Driver {
            combiner: None,
            delayed_combiner: None,
            slot: Arc::new(Mutex::new(None)),
            delayed_slot: Arc::new(Mutex::new(None)),
            log: Arc::new(Mutex::new(Vec::new())),
            step: 0,
        },
    );
    scheduler.start();
    let mut rounds = 0;
    while scheduler.run_main(10.0) {
        rounds += 1;
        assert!(rounds < 1_000_000, "QueryCombiner test did not converge");
    }
    scheduler.finish();
}

/// A closed combiner never sends, not even after the delay window.
struct ClosedDriver {
    combiner: Option<ActorOwn<QueryCombiner>>,
    slot: Slot,
}
impl Actor for ClosedDriver {
    fn start_up(&mut self) {
        self.combiner = Some(create_actor(
            "ClosedQueryCombiner",
            QueryCombiner::new(0.01, Box::new(|| true)),
        ));
        send_closure(
            &self.combiner.as_ref().expect("created above").get(),
            QueryCombinerMessage::AddQuery {
                query_id: 1,
                send_query: send_query_to(self.slot.clone()),
                promise: Promise::default(),
            },
        );
        let check = PromiseCreator::lambda_result({
            let slot = self.slot.clone();
            move |result: Result<()>| {
                result.move_as_ok();
                assert!(
                    slot.lock().unwrap().is_none(),
                    "closed combiner must not send"
                );
                Scheduler::instance().finish();
            }
        });
        create_actor("Check", SleepActor::new(0.1, check)).release();
    }
}

#[test]
fn query_combiner_does_not_send_while_closed() {
    let mut scheduler = ConcurrentScheduler::new(0);
    scheduler.create_actor_unsafe(
        0,
        "ClosedDriver",
        ClosedDriver {
            combiner: None,
            slot: Arc::new(Mutex::new(None)),
        },
    );
    scheduler.start();
    while scheduler.run_main(10.0) {}
    scheduler.finish();
}
