//! Port of `benchmark/bench_actor.cpp` (S37): `CreateActorBench`, the
//! `RingBench` message-passing ring (504 actors) and the `QueryBench`
//! client/server RPC pair, measured by the ported `bench` driver.
//!
//! Divergences from the C++ benchmark, documented in `bench/README.md`:
//!
//! - `RingBench` type 3 ("tail") is skipped: it drives the next actor
//!   through `get_actor_unsafe()->raw_event(...)`, a direct call that the
//!   Rust scheduler deliberately does not expose.
//! - `QueryBench` "immediate future" closes each iteration's future actor
//!   after the C++ checks instead of abandoning it (C++ lets the stack
//!   object die while the actor is still registered).
//! - `QueryBench` "dummy" writes its result into a client field instead of
//!   the C++ stack `int *res` (the C++ result is never read; carrying an
//!   owned write target per message would add an allocation to the measured
//!   path), and "lambda" carries its write target in an `Arc<Atomic>`
//!   (C++ captures a stack address).

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use bench::benchmark::{Benchmark, bench};
use tdactor::event::ActorThunk;
use tdactor::{
    Actor, ActorId, ActorOwn, ConcurrentScheduler, Event, EventFull, FutureActor, FutureActorState,
    PromiseActor, PromiseFuture, Raw, Scheduler, actor_messages, create_actor, send_closure,
    send_closure_later, send_event, send_thunk_immediately,
};
use tdutils::promise::{Promise, PromiseCreator};

/// The `TestActor` of `CreateActorBench`: stops on start and finishes the
/// scheduler from the last `tear_down` (the C++ static `actor_count_`).
struct TestActor {
    count: Arc<AtomicI32>,
}

impl Actor for TestActor {
    fn start_up(&mut self) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.stop();
    }

    fn tear_down(&mut self) {
        if self.count.fetch_sub(1, Ordering::Relaxed) == 1 {
            Scheduler::instance().finish();
        }
    }
}

/// `CreateActorBench`: create `n` actors that immediately stop.
struct CreateActorBench {
    sched: Option<ConcurrentScheduler>,
}

impl CreateActorBench {
    fn new() -> Self {
        Self { sched: None }
    }
}

impl Benchmark for CreateActorBench {
    fn get_description(&self) -> String {
        "CreateActor".into()
    }

    fn start_up(&mut self) {
        let mut sched = ConcurrentScheduler::new(0);
        sched.start();
        self.sched = Some(sched);
    }

    fn run(&mut self, n: usize) {
        let count = Arc::new(AtomicI32::new(0));
        let sched = self.sched.as_mut().expect("start_up not called");
        for _ in 0..n {
            let own = sched.create_actor_unsafe(
                0,
                "TestActor",
                TestActor {
                    count: count.clone(),
                },
            );
            let _ = own.release();
        }
        while sched.run_main(10.0) {}
    }

    fn tear_down(&mut self) {
        let mut sched = self.sched.take().expect("start_up not called");
        sched.finish();
    }
}

/// The `RingBench` send types (C++ `template <int type>`). Type 3 ("tail")
/// is not ported — see the module docs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RingType {
    Later,
    Immediate,
    Raw,
    Lambda,
}

impl RingType {
    fn name(self) -> &'static str {
        match self {
            RingType::Later => "later",
            RingType::Immediate => "immediate",
            RingType::Raw => "raw",
            RingType::Lambda => "lambda",
        }
    }
}

/// The C++ `RingBench::PassActor`: one node of the ring.
struct PassActor {
    id: i32,
    type_: RingType,
    next: Option<ActorId<PassActor>>,
    start_n: i32,
}

actor_messages! {
    enum PassMessage for PassActor {
        Pass { n: i32, } => pass(n),
        SetNext { id: i32, next: ActorId<PassActor>, } => set_next(id, next),
        SetStart { n: i32, } => set_start(n),
    }
}

impl PassActor {
    fn pass(&mut self, n: i32) {
        if n == 0 {
            Scheduler::instance().finish();
            return;
        }
        // Borrowed send — no refcount traffic per hop (the S37 hot path).
        let next = self.next.as_ref().expect("ring not wired");
        match self.type_ {
            RingType::Later => send_closure_later(next, PassMessage::Pass { n: n - 1 }),
            RingType::Immediate => send_closure(next, PassMessage::Pass { n: n - 1 }),
            RingType::Raw => send_event(next, Event::raw(Raw::from((n - 1) as u32))),
            RingType::Lambda => {
                // C++ send_lambda(next_actor, [n, ptr] { ptr->pass(n - 1); })
                // with a blind static_cast; the checked downcast replaces it.
                send_thunk_immediately(
                    next,
                    ActorThunk::from_boxed(Box::new(move |actor: &mut dyn Actor| {
                        let any: &mut dyn std::any::Any = actor;
                        any.downcast_mut::<PassActor>()
                            .expect("ring lambda addressed to a non-PassActor")
                            .pass(n - 1);
                    })),
                );
            }
        }
    }

    fn set_next(&mut self, id: i32, next: ActorId<PassActor>) {
        self.id = id;
        self.next = Some(next);
    }

    fn set_start(&mut self, n: i32) {
        self.start_n = n.max(100);
        self.wakeup();
    }
}

impl Actor for PassActor {
    fn raw_event(&mut self, raw: Raw) {
        self.pass(raw.u32() as i32);
    }

    fn wakeup(&mut self) {
        if self.start_n != 0 {
            let n = self.start_n;
            self.start_n = 0;
            self.pass(n);
        }
    }
}

/// `RingBench`: `actor_n` actors wired into a ring pass a message `n` times.
/// `thread_n` is the C++ `threads_n` (the number of worker schedulers);
/// actors are placed round-robin with `i % thread_n`.
struct RingBench {
    type_: RingType,
    actor_n: usize,
    thread_n: usize,
    sched: Option<ConcurrentScheduler>,
    actors: Vec<ActorOwn<PassActor>>,
}

impl RingBench {
    fn new(type_: RingType, actor_n: usize, thread_n: usize) -> Self {
        Self {
            type_,
            actor_n,
            thread_n,
            sched: None,
            actors: Vec::new(),
        }
    }
}

impl Benchmark for RingBench {
    fn get_description(&self) -> String {
        format!(
            "Ring (send_{}) (threads_n = {})",
            self.type_.name(),
            self.thread_n
        )
    }

    fn start_up(&mut self) {
        let mut sched = ConcurrentScheduler::new(self.thread_n);
        let mut actors = Vec::with_capacity(self.actor_n);
        for i in 0..self.actor_n {
            let sched_id = if self.thread_n != 0 {
                (i % self.thread_n) as i32
            } else {
                0
            };
            let own = sched.create_actor_unsafe(
                sched_id,
                "PassActor",
                PassActor {
                    id: i as i32,
                    type_: self.type_,
                    next: None,
                    start_n: 0,
                },
            );
            actors.push(own);
        }
        // C++ wires the ring by writing `next_actor` through
        // `get_actor_unsafe` before `start()`; the Rust port sends `SetNext`
        // messages instead (queued behind each actor's start event).
        {
            let _guard = sched.get_main_guard();
            for i in 0..self.actor_n {
                let next = actors[(i + 1) % self.actor_n].get();
                send_closure(
                    &actors[i].get(),
                    PassMessage::SetNext { id: i as i32, next },
                );
            }
        }
        sched.start();
        self.actors = actors;
        self.sched = Some(sched);
    }

    fn run(&mut self, n: usize) {
        let start_n = n.max(100) as i32;
        let first = self.actors[0].get();
        // C++ writes `start_n` directly before the first pump; the Rust port
        // sends it as a message, queued after the ring-wiring messages.
        {
            let _guard = self
                .sched
                .as_ref()
                .expect("start_up not called")
                .get_main_guard();
            send_closure(&first, PassMessage::SetStart { n: start_n });
        }
        while self
            .sched
            .as_mut()
            .expect("start_up not called")
            .run_main(10.0)
        {}
    }

    fn tear_down(&mut self) {
        let mut sched = self.sched.take().expect("start_up not called");
        sched.finish();
        self.actors.clear();
    }
}

/// The `QueryBench` RPC types (C++ `template <int type>`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum QueryType {
    Callback,
    ImmediateFuture,
    DelayedFuture,
    Dummy,
    Lambda,
    LambdaFuture,
}

impl QueryType {
    fn name(self) -> &'static str {
        match self {
            QueryType::Callback => "callback",
            QueryType::ImmediateFuture => "immediate future",
            QueryType::DelayedFuture => "delayed future",
            QueryType::Dummy => "dummy",
            QueryType::Lambda => "lambda",
            QueryType::LambdaFuture => "lambda_future",
        }
    }
}

/// The C++ `QueryBench::ClientActor`: answers RPCs from the server.
struct ClientActor {
    /// C++ `ClientActor::Callback` — a virtual callback; the Rust port is a
    /// boxed closure (the "callback" RPC type).
    callback: Box<dyn Fn(i32) + Send>,
    /// The "dummy" RPC type writes its result here instead of through an
    /// out-param: the C++ `&res` stack address is never read (the check-free
    /// type 3 branch), and carrying an owned write target per message would
    /// add an allocation to the measured path (see the module docs).
    dummy_out: Cell<i32>,
}

actor_messages! {
    enum ClientMessage for ClientActor {
        F { x: i32, } => f(x),
        Dummy { x: i32, } => dummy(x),
        FImmediatePromise { x: i32, promise: PromiseActor<i32>, } => f_immediate_promise(x, promise),
        FPromise { promise: Promise<()>, } => f_promise(promise),
    }
}

impl ClientActor {
    fn f(&mut self, x: i32) {
        (self.callback)(x * x);
    }

    fn dummy(&mut self, x: i32) {
        self.dummy_out.set(x * x);
    }

    fn f_immediate_promise(&mut self, x: i32, mut promise: PromiseActor<i32>) {
        promise.set_value(x * x);
    }

    fn f_promise(&mut self, mut promise: Promise<()>) {
        promise.set_value(());
    }
}

impl Actor for ClientActor {}

/// The C++ `QueryBench::ServerActor`: drives `n` RPCs through the client and
/// checks every result.
struct ServerActor {
    type_: QueryType,
    client: Option<ActorId<ClientActor>>,
    n: i32,
    /// The "delayed future" RPC type keeps the future alive between the send
    /// and the delivered result (C++ member `future_`).
    future: Option<Box<FutureActor<i32>>>,
}

actor_messages! {
    enum ServerMessage for ServerActor {
        Run { n: i32, } => run(n),
        OnResult { x: i32, } => on_result(x),
        ResultVal { val: i32, } => result_(val),
    }
}

impl ServerActor {
    fn new(type_: QueryType) -> Self {
        Self {
            type_,
            client: None,
            n: 0,
            future: None,
        }
    }

    fn run(&mut self, n: i32) {
        self.n = n;
        self.wakeup();
    }

    fn on_result(&mut self, x: i32) {
        assert_eq!(x, self.n * self.n);
        self.wakeup();
    }

    fn result_(&mut self, val: i32) {
        assert_eq!(val, self.n * self.n);
        self.wakeup();
    }

    /// Borrowed — the per-RPC sends take no refcount (the S37 hot path).
    fn client(&self) -> &ActorId<ClientActor> {
        self.client.as_ref().expect("start_up not called")
    }
}

impl Actor for ServerActor {
    fn start_up(&mut self) {
        let server_id = self.actor_id_of::<ServerActor>();
        // C++ create_actor<ClientActor>("Client", make_unique<ClientCallback>(actor_id(this))).
        let own = create_actor(
            "Client",
            ClientActor {
                callback: Box::new(move |x| {
                    send_closure(&server_id, ServerMessage::OnResult { x })
                }),
                dummy_out: Cell::new(0),
            },
        );
        self.client = Some(own.release());
    }

    fn raw_event(&mut self, raw: Raw) {
        // The "delayed future" RPC type: the future's subscribed event.
        debug_assert_eq!(raw.u64(), 1);
        let val = self
            .future
            .as_mut()
            .expect("future event without an armed future")
            .move_as_ok();
        assert_eq!(val, self.n * self.n);
        self.wakeup();
    }

    fn wakeup(&mut self) {
        loop {
            if self.n < 0 {
                Scheduler::instance().finish();
                return;
            }
            self.n -= 1;
            match self.type_ {
                QueryType::Callback => {
                    send_closure(self.client(), ClientMessage::F { x: self.n });
                    return;
                }
                QueryType::ImmediateFuture => {
                    let mut pf = PromiseFuture::<i32>::new();
                    let promise = pf.move_promise();
                    send_closure(
                        self.client(),
                        ClientMessage::FImmediatePromise { x: self.n, promise },
                    );
                    // The C++ checks: the client's pending start event (or the
                    // queued RPCs) blocks inline dispatch, so the future is
                    // still waiting right after the send.
                    {
                        let future = pf.future();
                        assert!(!future.is_ready());
                        assert_eq!(future.get_state(), FutureActorState::Waiting);
                    }
                    // C++ abandons the future (the stack object dies while the
                    // actor stays registered); the Rust port closes it instead
                    // — the one-shot promise delivery to a destroyed future is
                    // a no-op, and nothing dangles.
                    pf.future().close();
                }
                QueryType::DelayedFuture => {
                    let mut pf = PromiseFuture::<i32>::new();
                    let promise = pf.move_promise();
                    let mut future = pf.move_future();
                    // C++ EventCreator::raw(actor_id(), 1): the untyped
                    // ActorId<> form.
                    let server_id = self.actor_id();
                    future.set_event(EventFull::new(server_id, Event::raw(Raw::from(1u64))));
                    self.future = Some(future);
                    send_closure(
                        self.client(),
                        ClientMessage::FImmediatePromise { x: self.n, promise },
                    );
                    return;
                }
                QueryType::Dummy => {
                    // C++ sends a stack `int *res`; the Rust port writes to a
                    // client field instead (the C++ result is never read).
                    send_closure(self.client(), ClientMessage::Dummy { x: self.n });
                }
                QueryType::Lambda => {
                    let val = Arc::new(AtomicI32::new(0));
                    let captured = val.clone();
                    let n = self.n;
                    // C++ send_lambda(client_, [&] { val = n_ * n_; }): the
                    // thunk may run inline (val == n * n) or later (val == 0).
                    send_thunk_immediately(
                        self.client(),
                        ActorThunk::from_boxed(Box::new(move |_actor: &mut dyn Actor| {
                            captured.store(n * n, Ordering::Relaxed);
                        })),
                    );
                    let val = val.load(Ordering::Relaxed);
                    assert!(val == 0 || val == n * n);
                }
                QueryType::LambdaFuture => {
                    let server_id = self.actor_id_of::<ServerActor>();
                    let n = self.n;
                    // C++ PromiseCreator::lambda([actor_id, n](Unit) { send_closure(...) }).
                    let promise = PromiseCreator::lambda(move |()| {
                        send_closure(&server_id, ServerMessage::ResultVal { val: n * n });
                    });
                    send_closure(self.client(), ClientMessage::FPromise { promise });
                    return;
                }
            }
        }
    }
}

/// `QueryBench`: the client/server RPC pair driven `n` times.
struct QueryBench {
    type_: QueryType,
    sched: Option<ConcurrentScheduler>,
    server: Option<ActorId<ServerActor>>,
}

impl QueryBench {
    fn new(type_: QueryType) -> Self {
        Self {
            type_,
            sched: None,
            server: None,
        }
    }
}

impl Benchmark for QueryBench {
    fn get_description(&self) -> String {
        format!("QueryBench: {}", self.type_.name())
    }

    fn start_up(&mut self) {
        let mut sched = ConcurrentScheduler::new(0);
        let own = sched.create_actor_unsafe(0, "Server", ServerActor::new(self.type_));
        let server = own.release();
        sched.start();
        self.sched = Some(sched);
        self.server = Some(server);
    }

    fn run(&mut self, n: usize) {
        let server = self.server.as_ref().expect("start_up not called").clone();
        {
            // C++: auto guard = scheduler_->get_main_guard(); send_closure(...).
            let _guard = self
                .sched
                .as_ref()
                .expect("start_up not called")
                .get_main_guard();
            send_closure(&server, ServerMessage::Run { n: n as i32 });
        }
        while self
            .sched
            .as_mut()
            .expect("start_up not called")
            .run_main(10.0)
        {}
    }

    fn tear_down(&mut self) {
        self.server = None;
        let mut sched = self.sched.take().expect("start_up not called");
        sched.finish();
    }
}

fn main() {
    // Optional filter argument: only run benches whose description contains
    // the argument (used to profile a single bench).
    let filter = std::env::args().nth(1).unwrap_or_default();
    let call = |b: &mut dyn Benchmark| {
        if b.get_description().contains(&filter) {
            bench(b);
        }
    };
    // The C++ main() list, minus the two RingBench<3> ("tail") entries —
    // see the module docs for why they are skipped.
    call(&mut CreateActorBench::new());
    call(&mut RingBench::new(RingType::Lambda, 504, 0));
    call(&mut RingBench::new(RingType::Later, 504, 0));
    call(&mut RingBench::new(RingType::Immediate, 504, 0));
    call(&mut RingBench::new(RingType::Raw, 504, 0));
    call(&mut QueryBench::new(QueryType::LambdaFuture));
    call(&mut QueryBench::new(QueryType::Lambda));
    call(&mut QueryBench::new(QueryType::Dummy));
    call(&mut QueryBench::new(QueryType::DelayedFuture));
    call(&mut QueryBench::new(QueryType::ImmediateFuture));
    call(&mut QueryBench::new(QueryType::Callback));
    call(&mut RingBench::new(RingType::Later, 504, 10));
    call(&mut RingBench::new(RingType::Immediate, 504, 10));
    call(&mut RingBench::new(RingType::Raw, 504, 10));
    call(&mut RingBench::new(RingType::Later, 504, 2));
    call(&mut RingBench::new(RingType::Immediate, 504, 2));
    call(&mut RingBench::new(RingType::Raw, 504, 2));
}
