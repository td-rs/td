//! Ports of the `send_closure`-level tdactor tests:
//!
//! - `TEST(Actors, SendLater)` (`actors_simple.cpp`) → [`send_closure_ordering`]
//! - `TEST(Actors, simple_pass_event_arguments)` → [`pass_event_arguments`]
//! - the `Ping`/`Pong` pair → [`ping_pong`]
//! - `TEST(Actors, main)` (`actors_main.cpp`: the `MainQueryActor` fan-out
//!   over `QueryActor`/`Worker` chains, 10000 queries) → [`main_query_chain`]
//!
//! Porting notes:
//!
//! - C++ passes payloads by `const&`/`&&`/by-value; a Rust message owns its
//!   payloads (move), so the by-ref variants collapse into the by-value path
//!   (`X` is not `Clone`, so moving into the message is compile-checked).
//! - C++ `main` randomly migrates `QueryActor`s mid-run; the Rust
//!   `FutureActor` has no `on_start_migrate` yet (the C++
//!   `start_migrate(future, sched_id)` hook), so the port skips the random
//!   migration and crosses schedulers by registering the actors on random
//!   scheduler ids instead.
//! - `td::Random::fast` is replaced by a deterministic xorshift so the run
//!   stays reproducible.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tdactor::{
    Actor, ActorId, ConcurrentScheduler, Event, EventFull, FutureActor, FutureBox, PromiseActor,
    Raw, Scheduler, actor_messages, create_actor, register_actor, send_closure, send_closure_later,
    send_promise_immediately, send_promise_later,
};

/// `td::Random` stand-in: xorshift32, never seeded with zero.
struct Rng(u32);
impl Rng {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
    /// Inclusive on both ends, like `td::Random::fast(lo, hi)`.
    fn fast(&mut self, lo: u32, hi: u32) -> u32 {
        lo + self.next() % (hi - lo + 1)
    }
    fn fast_bool(&mut self) -> bool {
        self.next() & 1 == 0
    }
}

/// C++ `TEST(Actors, SendLater)`: `send_closure` runs the handler inline when
/// the destination is idle (empty mailbox), `send_closure_later` always goes
/// through the mailbox. Distinct characters make the ordering observable.
#[test]
fn send_closure_ordering() {
    let sb: Arc<Mutex<String>> = Arc::default();
    struct Scribe {
        sb: Arc<Mutex<String>>,
    }
    impl Actor for Scribe {}
    impl Scribe {
        fn write(&mut self, c: char) {
            self.sb.lock().unwrap().push(c);
        }
    }
    actor_messages! {
        enum ScribeMessage for Scribe {
            Write { c: char, } => write(c),
        }
    }

    let mut sched = ConcurrentScheduler::new(0);
    let own = {
        let _guard = sched.get_main_guard();
        create_actor("Scribe", Scribe { sb: sb.clone() })
    };
    let id = own.get();
    sched.start();
    {
        let _guard = sched.get_main_guard();
        let _ = sched.run_main_no_guard(0.01); // process start_up
        assert_eq!(sb.lock().unwrap().as_str(), "");

        send_closure(&id, ScribeMessage::Write { c: 'A' }); // inline
        send_closure_later(&id, ScribeMessage::Write { c: 'B' }); // mailbox
        send_closure(&id, ScribeMessage::Write { c: 'C' }); // mailbox not empty → queued
        assert_eq!(sb.lock().unwrap().as_str(), "A");

        let _ = sched.run_main_no_guard(0.01);
        assert_eq!(sb.lock().unwrap().as_str(), "ABC");
    }
    sched.finish();
}

/// C++ `TEST(Actors, simple_pass_event_arguments)`: arguments move through
/// the message and are dropped exactly once. `X` is not `Clone`, so the
/// by-value path (C++ `by_value`) is the only possible one; the by-ref C++
/// variants collapse into it.
#[test]
fn pass_event_arguments() {
    struct X {
        tag: &'static str,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for X {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct Receiver {
        got: Arc<Mutex<Vec<&'static str>>>,
    }
    impl Actor for Receiver {}
    impl Receiver {
        fn receive(&mut self, x: X) {
            self.got.lock().unwrap().push(x.tag);
        }
    }
    actor_messages! {
        enum ReceiverMessage for Receiver {
            Receive { x: X, } => receive(x),
        }
    }

    let drops = Arc::new(AtomicUsize::new(0));
    let got = Arc::new(Mutex::new(Vec::new()));
    let mut sched = ConcurrentScheduler::new(0);
    let own = {
        let _guard = sched.get_main_guard();
        create_actor("XR", Receiver { got: got.clone() })
    };
    let id = own.get();
    sched.start();
    {
        let _guard = sched.get_main_guard();
        let _ = sched.run_main_no_guard(0.01);

        // Temporary → immediate handler: handled inline, dropped with the
        // thunk before the send returns.
        send_closure(
            &id,
            ReceiverMessage::Receive {
                x: X {
                    tag: "tmp_immediate",
                    drops: drops.clone(),
                },
            },
        );
        assert_eq!(*got.lock().unwrap(), ["tmp_immediate"]);
        assert_eq!(drops.load(Ordering::SeqCst), 1);

        // Temporary → mailbox: the payload sits in the queue until the run.
        send_closure_later(
            &id,
            ReceiverMessage::Receive {
                x: X {
                    tag: "tmp_later",
                    drops: drops.clone(),
                },
            },
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1); // still held by the event
        let _ = sched.run_main_no_guard(0.01);
        assert_eq!(*got.lock().unwrap(), ["tmp_immediate", "tmp_later"]);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }
    sched.finish();
    assert_eq!(drops.load(Ordering::SeqCst), 2); // no copies, no leaks
}

/// The `Ping`/`Pong` pair: a `Ball` owned by the messages bounces between the
/// two actors; the last return finishes the scheduler.
#[test]
fn ping_pong() {
    const RALLIES: u32 = 10;
    struct Ball {
        rally: u32,
    }

    struct Ping {
        peer: Option<ActorId<Pong>>,
        rallies: u32,
    }
    impl Actor for Ping {}
    impl Ping {
        fn set_peer(&mut self, peer: ActorId<Pong>) {
            self.peer = Some(peer);
        }
        fn go(&mut self) {
            send_closure(
                self.peer.as_ref().unwrap(),
                PongMessage::Pong {
                    ball: Ball { rally: 0 },
                },
            );
        }
        fn ping(&mut self, ball: Ball) {
            if ball.rally == self.rallies {
                Scheduler::instance().finish();
                return;
            }
            send_closure(
                self.peer.as_ref().unwrap(),
                PongMessage::Pong {
                    ball: Ball {
                        rally: ball.rally + 1,
                    },
                },
            );
        }
    }
    actor_messages! {
        enum PingMessage for Ping {
            SetPeer { peer: ActorId<Pong>, } => set_peer(peer),
            Go => go(),
            Ping { ball: Ball, } => ping(ball),
        }
    }

    struct Pong {
        peer: ActorId<Ping>,
        hits: Arc<AtomicUsize>,
    }
    impl Actor for Pong {}
    impl Pong {
        fn pong(&mut self, ball: Ball) {
            self.hits.fetch_add(1, Ordering::SeqCst);
            send_closure(&self.peer, PingMessage::Ping { ball });
        }
    }
    actor_messages! {
        enum PongMessage for Pong {
            Pong { ball: Ball, } => pong(ball),
        }
    }

    let hits = Arc::new(AtomicUsize::new(0));
    let mut sched = ConcurrentScheduler::new(0);
    {
        let _guard = sched.get_main_guard();
        // `.release()`: the C++ test leaks ownership to the scheduler (the
        // actors live until the run ends); dropping an `ActorOwn` here would
        // send `hangup` and kill the actors before their first messages.
        let ping = create_actor(
            "Ping",
            Ping {
                peer: None,
                rallies: RALLIES,
            },
        )
        .release();
        let pong = create_actor(
            "Pong",
            Pong {
                peer: ping.clone(),
                hits: hits.clone(),
            },
        )
        .release();
        send_closure(&ping, PingMessage::SetPeer { peer: pong.clone() });
        send_closure(&ping, PingMessage::Go);
    }
    sched.start();
    while sched.run_main(10.0) {}
    sched.finish();
    // Pong serves rallies 0..=RALLIES (the ball that ends the game included).
    assert_eq!(hits.load(Ordering::SeqCst), (RALLIES + 1) as usize);
}

// ---- C++ TEST(Actors, main) ----

/// `x^p mod 2^32`, square-and-multiply — C++ `fast_pow_mod_uint32`.
fn fast_pow_mod(x: u32, p: u32) -> u32 {
    let (mut x, mut p, mut result) = (x, p, 1u32);
    while p > 0 {
        if p & 1 == 1 {
            result = result.wrapping_mul(x);
        }
        x = x.wrapping_mul(x);
        p >>= 1;
    }
    result
}

/// `x^p mod 2^32`, `p` multiplications — C++ `slow_pow_mod_uint32`.
fn slow_pow_mod(x: u32, p: u32) -> u32 {
    let mut result = x;
    for _ in 1..p {
        result = result.wrapping_mul(x);
    }
    result
}

/// C++ `ActorQuery`: non-copyable on purpose; the destructor asserts no query
/// is ever lost with pending work (C++ `LOG_CHECK(todo.empty())`).
struct ActorQuery {
    query_id: u32,
    result: u32,
    todo: Vec<i32>,
}
impl ActorQuery {
    fn next_pow(&mut self) -> u32 {
        assert!(!self.todo.is_empty());
        self.todo.pop().unwrap() as u32
    }
    fn ready(&self) -> bool {
        self.todo.is_empty()
    }
}
impl Drop for ActorQuery {
    fn drop(&mut self) {
        assert!(self.todo.is_empty(), "ActorQuery lost");
    }
}

/// C++ `fast_calc`.
fn fast_calc(q: &ActorQuery) -> u32 {
    let mut result = q.result;
    for &x in &q.todo {
        result = fast_pow_mod(result, x as u32);
    }
    result
}

/// C++ `QueryActor::Callback`.
trait QueryCallback: Send {
    fn on_result(&mut self, query: ActorQuery);
    fn on_closed(&mut self);
}

struct Worker;
impl Actor for Worker {}
impl Worker {
    fn query(&mut self, mut promise: PromiseActor<u32>, x: u32, p: u32) {
        let result = slow_pow_mod(x, p);
        promise.set_value(result);
    }
}

struct QueryActor {
    callback: Option<Box<dyn QueryCallback>>,
    pending: HashMap<u32, (FutureBox<u32>, ActorQuery)>,
    workers: Vec<ActorId<Worker>>,
    rng: Rng,
}
impl Actor for QueryActor {
    fn raw_event(&mut self, raw: Raw) {
        let query_id = raw.u32();
        let (mut future, mut query) = self.pending.remove(&query_id).expect("unknown query id");
        assert!(future.is_ready());
        query.result = future.move_as_ok();
        self.callback.as_mut().unwrap().on_result(query);
    }
}
impl QueryActor {
    fn set_callback(&mut self, callback: Box<dyn QueryCallback>) {
        self.callback = Some(callback);
    }
    fn set_workers(&mut self, workers: Vec<ActorId<Worker>>) {
        self.workers = workers;
    }
    fn query(&mut self, mut query: ActorQuery) {
        let x = query.result;
        let p = query.next_pow();
        if self.rng.fast(0, 3) != 0 && (p <= 1000 || self.workers.is_empty()) {
            query.result = slow_pow_mod(x, p);
            self.callback.as_mut().unwrap().on_result(query);
        } else {
            let idx = self.rng.next() as usize % self.workers.len();
            let worker = self.workers[idx].clone();
            let mut future = if self.rng.fast(0, 3) == 0 {
                send_promise_immediately(
                    worker,
                    move |w: &mut Worker, promise: PromiseActor<u32>| w.query(promise, x, p),
                )
            } else {
                send_promise_later(worker, move |w: &mut Worker, promise: PromiseActor<u32>| {
                    w.query(promise, x, p)
                })
            };
            if future.is_ready() {
                query.result = future.move_as_ok();
                self.callback.as_mut().unwrap().on_result(query);
            } else {
                future.set_event(EventFull::new(
                    self.actor_id(),
                    Event::raw(Raw::from(query.query_id)),
                ));
                let query_id = query.query_id;
                self.pending.insert(query_id, (future, query));
            }
        }
    }
    fn close(&mut self) {
        self.callback.take().unwrap().on_closed();
        self.stop();
    }
}
actor_messages! {
    enum QueryActorMessage for QueryActor {
        SetCallback { callback: Box<dyn QueryCallback>, } => set_callback(callback),
        SetWorkers { workers: Vec<ActorId<Worker>>, } => set_workers(workers),
        Query { query: ActorQuery, } => query(query),
        Close => close(),
    }
}

/// C++ `MainQueryActor::QueryActorCallback`: forward ready results to the
/// parent, unfinished ones to the next solver in the ring.
struct QueryActorCallback {
    parent: ActorId<MainQueryActor>,
    next_solver: ActorId<QueryActor>,
}
impl QueryCallback for QueryActorCallback {
    fn on_result(&mut self, query: ActorQuery) {
        if query.ready() {
            send_closure(&self.parent, MainQueryActorMessage::OnResult { query });
        } else {
            send_closure(&self.next_solver, QueryActorMessage::Query { query });
        }
    }
    fn on_closed(&mut self) {
        send_closure(&self.parent, MainQueryActorMessage::OnClosed);
    }
}

const QUERY_ACTORS: usize = 10;
const WORKERS: usize = 4;
const QUERY_CNT: u32 = 10_000;

struct MainQueryActor {
    expected: HashMap<u32, u32>,
    actors: Vec<ActorId<QueryActor>>,
    workers: Vec<ActorId<Worker>>,
    out_cnt: u32,
    in_cnt: u32,
    query_id: u32,
    ref_cnt: u32,
    threads_n: i32,
    rng: Rng,
}
impl Actor for MainQueryActor {
    fn start_up(&mut self) {
        for _ in 0..QUERY_ACTORS {
            let sched_id = self.random_sched_id();
            let actor = register_actor(
                "QueryActor",
                sched_id,
                QueryActor {
                    callback: None,
                    pending: HashMap::new(),
                    workers: Vec::new(),
                    rng: Rng(0x1234_5678 ^ self.rng.next()),
                },
            );
            self.actors.push(actor.release());
        }
        for _ in 0..WORKERS {
            let sched_id = self.random_sched_id();
            let worker = register_actor("Worker", sched_id, Worker);
            self.workers.push(worker.release());
        }
        for i in 0..QUERY_ACTORS {
            self.ref_cnt += 1;
            let callback = Box::new(QueryActorCallback {
                parent: self.actor_id_of::<MainQueryActor>(),
                next_solver: self.actors[(i + 1) % QUERY_ACTORS].clone(),
            });
            send_closure(&self.actors[i], QueryActorMessage::SetCallback { callback });
            send_closure(
                &self.actors[i],
                QueryActorMessage::SetWorkers {
                    workers: self.workers.clone(),
                },
            );
        }
        self.yield_();
    }
    fn wakeup(&mut self) {
        while self.out_cnt < self.in_cnt + 100 && self.out_cnt < QUERY_CNT {
            let idx = self.rng.next() as usize % self.actors.len();
            let actor = self.actors[idx].clone();
            let query = self.create_query();
            if self.rng.fast_bool() {
                send_closure(&actor, QueryActorMessage::Query { query });
            } else {
                send_closure_later(&actor, QueryActorMessage::Query { query });
            }
            self.out_cnt += 1;
        }
        if self.in_cnt == QUERY_CNT {
            self.in_cnt += 1;
            self.ref_cnt -= 1;
            for actor in &self.actors {
                send_closure(actor, QueryActorMessage::Close);
            }
        }
    }
}
impl MainQueryActor {
    fn random_sched_id(&mut self) -> i32 {
        if self.threads_n > 1 {
            self.rng.fast(2, self.threads_n as u32) as i32
        } else {
            0
        }
    }
    fn create_query(&mut self) -> ActorQuery {
        self.query_id += 2;
        let query_id = self.query_id;
        let query = ActorQuery {
            query_id,
            result: query_id,
            todo: vec![1, 1, 1, 1, 1, 1, 1, 1, 10_000],
        };
        self.expected.insert(query_id, fast_calc(&query));
        query
    }
    fn on_result(&mut self, query: ActorQuery) {
        assert!(query.ready());
        assert_eq!(
            Some(query.result),
            self.expected.get(&query.query_id).copied()
        );
        self.in_cnt += 1;
        self.wakeup();
    }
    fn on_closed(&mut self) {
        self.ref_cnt -= 1;
        if self.ref_cnt == 0 {
            Scheduler::instance().finish();
        }
    }
}
actor_messages! {
    enum MainQueryActorMessage for MainQueryActor {
        OnResult { query: ActorQuery, } => on_result(query),
        OnClosed => on_closed(),
    }
}

/// C++ `TEST(Actors, main)`: the full ring — 10000 queries pushed through 10
/// `QueryActor`s (mixed `send_closure`/`send_closure_later`, promise fan-out
/// to 4 `Worker`s, raw-event wakeups), results checked against `fast_calc`.
#[test]
fn main_query_chain() {
    let threads_n: i32 = 9;
    let mut sched = ConcurrentScheduler::new(threads_n as usize);
    sched.create_actor_unsafe(
        if threads_n > 1 { 1 } else { 0 },
        "MainQuery",
        MainQueryActor {
            expected: HashMap::new(),
            actors: Vec::new(),
            workers: Vec::new(),
            out_cnt: 0,
            in_cnt: 0,
            query_id: 1,
            ref_cnt: 1,
            threads_n,
            rng: Rng(0x9E37_79B9),
        },
    );
    sched.start();
    while sched.run_main(10.0) {}
    sched.finish();
}
