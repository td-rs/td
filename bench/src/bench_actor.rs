//
// Port of `benchmark/bench_actor.cpp` (S37): the actor-runtime performance
// gate. Measures actor creation, ring-of-actors message passing over every
// send path, and the client-server query patterns (callback, immediate and
// delayed PromiseActor, dummy store, send_lambda, PromiseCreator::lambda).
//
// Send-path mapping (S33's typed message-enum dispatch does not exist yet, so
// C++ `send_closure` lands on the same `Event::custom` thunk machinery S34
// built for the promise RPC — `Scheduler::send_closure_immediately`):
//
// | C++                             | Rust                                        |
// |---------------------------------|---------------------------------------------|
// | `send_closure_later(next, ..)`   | `send_event_later(next, Event::custom(..))` |
// | `send_closure(next, ..)`         | `send_event(next, Event::custom(..))`       |
// | `send_event(next, raw)`          | `send_event(next, Event::raw(..))`          |
// | `..get_actor_unsafe()->raw(..)`  | `ActorId::get_actor_unsafe()` direct call   |
// | `send_lambda(next, ..)`          | `send_event(next, Event::custom(..))`       |
//
// Two C++ captures of stack locals by reference (`dummy`'s `&res`,
// `send_lambda`'s `&val`) become a thread-local `Cell`: Rust thunks are
// `'static` and the C++ capture is UB if the send defers. QueryBench runs on
// one scheduler (threads_n = 0), so the thunk dereferences the same thread's
// cell — word-capture cost, no leak, sound even when deferred.
//
// `bench/actor-gate.sh` runs this binary against the C++ one and fails when
// any case drops below half the C++ ops/sec.

use std::cell::Cell;
use std::sync::atomic::{AtomicI32, Ordering};

use tdactor::promise::{
    send_promise_immediately, EventFull, FutureActor, FutureActorState, PromiseActor, PromiseFuture,
};
use tdactor::{
    create_actor, send_event, send_event_later, Actor, ActorId, ActorOwn, ConcurrentScheduler,
    Event, Raw, Scheduler,
};
use tdutils::benchmark::{bench, Benchmark};
use tdutils::promise::{Promise, PromiseCreator};

/// The C++ `static_cast` inside `send_closure`: the thunk is dispatched on
/// the actor behind a typed `ActorId<A>`, so the marker names the real type.
/// A wrong marker is UB exactly like the C++ cast.
unsafe fn downcast_actor<A: Actor>(actor: &mut dyn Actor) -> &mut A {
    unsafe { &mut *(actor as *mut dyn Actor as *mut A) }
}

// ---- CreateActorBench ----

struct TestActor;

static ACTOR_COUNT: AtomicI32 = AtomicI32::new(0);

impl Actor for TestActor {
    fn start_up(&mut self) {
        ACTOR_COUNT.fetch_add(1, Ordering::Relaxed);
        self.stop();
    }

    fn tear_down(&mut self) {
        if ACTOR_COUNT.fetch_sub(1, Ordering::Relaxed) == 1 {
            Scheduler::instance().finish();
        }
    }
}

struct CreateActorBench {
    scheduler: Option<ConcurrentScheduler>,
}

impl Benchmark for CreateActorBench {
    fn get_description(&self) -> String {
        "CreateActor".to_owned()
    }

    fn start_up(&mut self) {
        let mut scheduler = ConcurrentScheduler::new(0);
        scheduler.start();
        self.scheduler = Some(scheduler);
    }

    fn tear_down(&mut self) {
        if let Some(mut scheduler) = self.scheduler.take() {
            scheduler.finish();
        }
    }

    fn run(&mut self, n: i32) {
        let scheduler = self.scheduler.as_mut().unwrap();
        for _ in 0..n {
            scheduler.create_actor_unsafe(0, "TestActor", TestActor).release();
        }
        while scheduler.run_main(10.0) {
            // empty
        }
    }
}

// ---- RingBench ----

const RING_TYPES: [&str; 5] = ["later", "immediate", "raw", "tail", "lambda"];

struct PassActor<const TYPE: usize> {
    /// C++ `id` — only read by the commented-out LOG.
    #[allow(dead_code)]
    id: i32,
    /// The C++ default (empty) `ActorId` before the ring is linked.
    next: Option<ActorId<PassActor<TYPE>>>,
    start_n: i32,
}

impl<const TYPE: usize> PassActor<TYPE> {
    fn pass(&mut self, n: i32) {
        if n == 0 {
            Scheduler::instance().finish();
            return;
        }
        let next = self.next.as_ref().expect("ring actor without a link").clone();
        match TYPE {
            0 => {
                let n = n - 1;
                send_event_later(
                    next,
                    Event::custom(Box::new(move |actor: &mut dyn Actor| {
                        unsafe { downcast_actor::<PassActor<TYPE>>(actor) }.pass(n);
                    })),
                );
            }
            1 => {
                let n = n - 1;
                send_event(
                    next,
                    Event::custom(Box::new(move |actor: &mut dyn Actor| {
                        unsafe { downcast_actor::<PassActor<TYPE>>(actor) }.pass(n);
                    })),
                );
            }
            2 => {
                send_event(next, Event::raw(Raw::from((n - 1) as u32)));
            }
            3 => {
                if n % 5000 == 0 {
                    // The C++ mailbox hop every 5000 passes (stack depth and
                    // scheduler pumping); the counter is the un-decremented
                    // one, exactly like C++.
                    let n = n - 1;
                    send_event_later(
                        next,
                        Event::custom(Box::new(move |actor: &mut dyn Actor| {
                            unsafe { downcast_actor::<PassActor<TYPE>>(actor) }.pass(n);
                        })),
                    );
                } else {
                    let n = n - 1;
                    // SAFETY: single-scheduler ring (threads_n = 0); the
                    // C++ raw dereference, including its mid-dispatch slot
                    // re-entry when the ring wraps.
                    unsafe { next.get_actor_unsafe().unwrap() }.raw_event(Raw::from(n as u32));
                }
            }
            4 => {
                let n = n - 1;
                send_event(
                    next,
                    Event::custom(Box::new(move |actor: &mut dyn Actor| {
                        unsafe { downcast_actor::<PassActor<TYPE>>(actor) }.pass(n);
                    })),
                );
            }
            _ => unreachable!("ring type"),
        }
    }
}

impl<const TYPE: usize> Actor for PassActor<TYPE> {
    fn raw_event(&mut self, raw: Raw) {
        self.pass(raw.u32() as i32);
    }

    fn start_up(&mut self) {
        self.yield_();
    }

    fn wakeup(&mut self) {
        if self.start_n != 0 {
            let n = self.start_n;
            self.start_n = 0;
            self.pass(n);
        }
    }
}

struct RingBench<const TYPE: usize> {
    actor_n: i32,
    thread_n: i32,
    actor_array: Vec<ActorId<PassActor<TYPE>>>,
    scheduler: Option<ConcurrentScheduler>,
}

impl<const TYPE: usize> RingBench<TYPE> {
    fn new(actor_n: i32, thread_n: i32) -> Self {
        Self {
            actor_n,
            thread_n,
            actor_array: Vec::new(),
            scheduler: None,
        }
    }
}

impl<const TYPE: usize> Benchmark for RingBench<TYPE> {
    fn get_description(&self) -> String {
        format!(
            "Ring (send_{}) (threads_n = {})",
            RING_TYPES[TYPE], self.thread_n
        )
    }

    fn start_up(&mut self) {
        let mut scheduler = ConcurrentScheduler::new(self.thread_n as usize);

        self.actor_array = (0..self.actor_n)
            .map(|i| {
                let sched_id = if self.thread_n != 0 {
                    i % self.thread_n
                } else {
                    0
                };
                scheduler
                    .create_actor_unsafe(
                        sched_id,
                        "PassActor",
                        PassActor {
                            id: i,
                            next: None,
                            start_n: 0,
                        },
                    )
                    .release()
            })
            .collect();
        for i in 0..self.actor_n {
            let next = self.actor_array[((i + 1) % self.actor_n) as usize].clone();
            // SAFETY: pre-start, single-threaded setup on the caller thread.
            unsafe { self.actor_array[i as usize].get_actor_unsafe().unwrap() }.next = Some(next);
        }
        scheduler.start();
        self.scheduler = Some(scheduler);
    }

    fn run(&mut self, n: i32) {
        // first actor is on main_thread
        // SAFETY: scheduler 0 is pumped on the caller's thread and the ring
        // is not mid-dispatch here.
        unsafe { self.actor_array[0].get_actor_unsafe().unwrap() }.start_n = n.max(100);
        while self.scheduler.as_mut().unwrap().run_main(10.0) {
            // empty
        }
    }

    fn tear_down(&mut self) {
        let mut scheduler = self.scheduler.take().expect("ring without a scheduler");
        scheduler.finish();
    }
}

// ---- QueryBench ----

const QUERY_TYPES: [&str; 6] = [
    "callback",
    "immediate future",
    "delayed future",
    "dummy",
    "lambda",
    "lambda_future",
];

thread_local! {
    /// The C++ `int res` / `int val` stack locals captured by reference; a
    /// thread-local (not a stack slot) because Rust thunks are `'static`.
    static SCRATCH: Cell<i32> = const { Cell::new(0) };
}

trait ClientCallback: Send {
    fn on_result(&mut self, x: i32);
}

struct ClientActor {
    callback: Box<dyn ClientCallback>,
}

impl Actor for ClientActor {}

impl ClientActor {
    fn f(&mut self, x: i32) {
        self.callback.on_result(x * x);
    }

    fn f_immediate_promise(&mut self, x: i32, mut promise: PromiseActor<i32>) {
        promise.set_value(x * x);
    }

    fn dummy(&mut self, x: i32, res: &Cell<i32>) {
        res.set(x * x);
    }

    fn f_promise(&mut self, mut promise: Promise<()>) {
        promise.set_value(());
    }
}

struct ServerCallback<const TYPE: usize> {
    server: ActorId<ServerActor<TYPE>>,
}

impl<const TYPE: usize> ClientCallback for ServerCallback<TYPE> {
    fn on_result(&mut self, x: i32) {
        let server = self.server.clone();
        send_event(
            server,
            Event::custom(Box::new(move |actor: &mut dyn Actor| {
                unsafe { downcast_actor::<ServerActor<TYPE>>(actor) }.on_result(x);
            })),
        );
    }
}

struct ServerActor<const TYPE: usize> {
    client: Option<ActorId<ClientActor>>,
    n: i32,
    /// The C++ `FutureActor<int> future_` member (boxed — the S34 port keeps
    /// the registered-by-pointer actor on the heap).
    future: Option<Box<FutureActor<i32>>>,
}

impl<const TYPE: usize> ServerActor<TYPE> {
    fn run(&mut self, n: i32) {
        self.n = n;
        self.wakeup();
    }

    fn on_result(&mut self, x: i32) {
        assert_eq!(x, self.n * self.n);
        self.wakeup();
    }

    fn result(&mut self, val: i32) {
        assert_eq!(val, self.n * self.n);
        self.wakeup();
    }

    fn wakeup(&mut self) {
        // the C++ stack local starts fresh at 0 every wakeup
        SCRATCH.with(Cell::take);
        loop {
            if self.n < 0 {
                Scheduler::instance().finish();
                return;
            }
            self.n -= 1;
            let n = self.n;
            let client = self.client.as_ref().expect("client actor").clone();
            match TYPE {
                0 => {
                    send_event(
                        client,
                        Event::custom(Box::new(move |actor: &mut dyn Actor| {
                            unsafe { downcast_actor::<ClientActor>(actor) }.f(n);
                        })),
                    );
                    return;
                }
                1 => {
                    let future = send_promise_immediately::<i32, _, _, _>(
                        client,
                        move |callee: &mut ClientActor, promise| {
                            callee.f_immediate_promise(n, promise);
                        },
                    );
                    assert!(!future.is_ready());
                    assert!(future.get_state() == FutureActorState::Waiting);
                    // The C++ local dies at scope end; here the drop stops the
                    // registered future actor.
                    drop(future);
                    // no return: the C++ loop hammers everything inline
                }
                2 => {
                    let mut promise_future = PromiseFuture::<i32>::new();
                    promise_future
                        .future()
                        .set_event(EventFull::new(self.actor_id(), Event::raw(Raw(1))));
                    let promise = promise_future.move_promise();
                    self.future = Some(promise_future.move_future());
                    send_event(
                        client,
                        Event::custom(Box::new(move |actor: &mut dyn Actor| {
                            unsafe { downcast_actor::<ClientActor>(actor) }
                                .f_immediate_promise(n, promise);
                        })),
                    );
                    return;
                }
                3 => {
                    send_event(
                        client,
                        Event::custom(Box::new(move |actor: &mut dyn Actor| {
                            SCRATCH.with(|res| {
                                unsafe { downcast_actor::<ClientActor>(actor) }.dummy(n, res);
                            });
                        })),
                    );
                    // no return
                }
                4 => {
                    send_event(
                        client,
                        Event::custom(Box::new(move |_actor: &mut dyn Actor| {
                            SCRATCH.with(|val| val.set(n * n));
                        })),
                    );
                    let val = SCRATCH.with(Cell::get);
                    assert!(val == 0 || val == n * n);
                    // no return
                }
                5 => {
                    let server = self.actor_id_of::<Self>();
                    let promise = PromiseCreator::lambda(move |_: ()| {
                        send_event(
                            server,
                            Event::custom(Box::new(move |actor: &mut dyn Actor| {
                                unsafe { downcast_actor::<ServerActor<TYPE>>(actor) }
                                    .result(n * n);
                            })),
                        );
                    });
                    send_event(
                        client,
                        Event::custom(Box::new(move |actor: &mut dyn Actor| {
                            unsafe { downcast_actor::<ClientActor>(actor) }.f_promise(promise);
                        })),
                    );
                    return;
                }
                _ => unreachable!("query type"),
            }
        }
    }
}

impl<const TYPE: usize> Actor for ServerActor<TYPE> {
    fn start_up(&mut self) {
        self.client = Some(
            create_actor(
                "Client",
                ClientActor {
                    callback: Box::new(ServerCallback::<TYPE> {
                        server: self.actor_id_of::<Self>(),
                    }),
                },
            )
            .release(),
        );
    }

    fn raw_event(&mut self, raw: Raw) {
        assert_eq!(raw.u64(), 1);
        let val = self
            .future
            .as_mut()
            .expect("raw event without a future")
            .move_as_ok();
        assert_eq!(val, self.n * self.n);
        self.wakeup();
    }
}

struct QueryBench<const TYPE: usize> {
    scheduler: Option<ConcurrentScheduler>,
    server: Option<ActorOwn<ServerActor<TYPE>>>,
}

impl<const TYPE: usize> QueryBench<TYPE> {
    fn new() -> Self {
        Self {
            scheduler: None,
            server: None,
        }
    }
}

impl<const TYPE: usize> Benchmark for QueryBench<TYPE> {
    fn get_description(&self) -> String {
        format!("QueryBench: {}", QUERY_TYPES[TYPE])
    }

    fn start_up(&mut self) {
        let mut scheduler = ConcurrentScheduler::new(0);
        self.server = Some(scheduler.create_actor_unsafe(
            0,
            "Server",
            ServerActor::<TYPE> {
                client: None,
                n: 0,
                future: None,
            },
        ));
        scheduler.start();
        self.scheduler = Some(scheduler);
    }

    fn run(&mut self, n: i32) {
        // first actor is on main_thread
        {
            let scheduler = self.scheduler.as_mut().unwrap();
            let guard = scheduler.get_main_guard();
            let server = self.server.as_ref().unwrap().get();
            send_event(
                server,
                Event::custom(Box::new(move |actor: &mut dyn Actor| {
                    unsafe { downcast_actor::<ServerActor<TYPE>>(actor) }.run(n);
                })),
            );
            drop(guard);
        }
        while self.scheduler.as_mut().unwrap().run_main(10.0) {
            // empty
        }
    }

    fn tear_down(&mut self) {
        // C++ `server_.release()` — no hangup; destroyed with the scheduler.
        if let Some(server) = self.server.take() {
            server.release();
        }
        if let Some(mut scheduler) = self.scheduler.take() {
            scheduler.finish();
        }
    }
}

fn main() {
    bench(&mut CreateActorBench { scheduler: None }, 1.0);
    bench(&mut RingBench::<4>::new(504, 0), 1.0);
    bench(&mut RingBench::<3>::new(504, 0), 1.0);
    bench(&mut RingBench::<0>::new(504, 0), 1.0);
    bench(&mut RingBench::<1>::new(504, 0), 1.0);
    bench(&mut RingBench::<2>::new(504, 0), 1.0);
    bench(&mut QueryBench::<5>::new(), 1.0);
    bench(&mut QueryBench::<4>::new(), 1.0);
    bench(&mut QueryBench::<3>::new(), 1.0);
    bench(&mut QueryBench::<2>::new(), 1.0);
    bench(&mut QueryBench::<1>::new(), 1.0);
    bench(&mut QueryBench::<0>::new(), 1.0);
    bench(&mut RingBench::<3>::new(504, 0), 1.0);
    bench(&mut RingBench::<0>::new(504, 10), 1.0);
    bench(&mut RingBench::<1>::new(504, 10), 1.0);
    bench(&mut RingBench::<2>::new(504, 10), 1.0);
    bench(&mut RingBench::<0>::new(504, 2), 1.0);
    bench(&mut RingBench::<1>::new(504, 2), 1.0);
    bench(&mut RingBench::<2>::new(504, 2), 1.0);
}
