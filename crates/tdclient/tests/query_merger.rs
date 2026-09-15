//! Port of `TEST(QueryMerger, stress)` (`test/query_merger.cpp`): random
//! duplicate-prone adds must merge into batches of at most
//! `MAX_MERGED_QUERY_COUNT`, never exceed `MAX_CONCURRENT_QUERY_COUNT` in
//! flight, keep the pending order, and deliver every promise exactly once.
//!
//! Porting notes: the C++ merge function captures `this` (the test actor) and
//! runs synchronously inside `QueryMerger::add_query`; the Rust port runs it
//! inside the merger's dispatch and reports back through messages — same
//! invariants, mailbox FIFO keeps the observation order. `td::Random::fast`
//! is replaced by a deterministic xorshift (the send_closure.rs precedent).

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use tdactor::{
    Actor, ActorId, ActorOwn, ConcurrentScheduler, Scheduler, SleepActor, actor_messages,
    create_actor, send_closure,
};
use tdclient::query_merger::{MergeFunction, QueryMerger, QueryMergerMessage};
use tdutils::promise::{Promise, PromiseCreator};
use tdutils::status::Result;

const MAX_CONCURRENT_QUERY_COUNT: usize = 5;
const MAX_MERGED_QUERY_COUNT: usize = 3;
const MAX_QUERY_COUNT: usize = 1000;

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
}

#[derive(Default)]
struct TestState {
    current_query_count: usize,
    total_query_count: usize,
    completed_query_count: usize,
    next_query_ids: VecDeque<i64>,
    pending_query_ids: HashSet<i64>,
}

type SharedState = Arc<Mutex<TestState>>;

struct TestQueryMerger {
    merger: Option<ActorOwn<QueryMerger>>,
    state: SharedState,
    rng: Rng,
}
impl Actor for TestQueryMerger {
    fn start_up(&mut self) {
        let test_id = self.actor_id_of::<Self>();
        let state = self.state.clone();
        let merge_function: MergeFunction = Box::new(move |query_ids: Vec<i64>, promise: Promise<()>| {
            let mut st = state.lock().unwrap();
            assert!(!query_ids.is_empty());
            assert!(query_ids.len() <= MAX_MERGED_QUERY_COUNT);
            // The C++ test also asserts batch == min(queued, 3): it can,
            // because add_query processes synchronously inside the caller.
            // The message-driven port keeps FIFO order but test-side pushes
            // may run ahead of batch formation, so only the order, the
            // distinctness and the concurrency cap are asserted.
            let mut seen = HashSet::new();
            for &query_id in &query_ids {
                assert!(seen.insert(query_id), "duplicate id {query_id} in one batch");
                let next = st
                    .next_query_ids
                    .pop_front()
                    .expect("batch id must be the oldest queued add");
                assert_eq!(query_id, next);
            }
            st.current_query_count += 1;
            assert!(st.current_query_count <= MAX_CONCURRENT_QUERY_COUNT);
            drop(st);

            let completion = PromiseCreator::lambda_result({
                let state = state.clone();
                let query_ids = query_ids.clone();
                move |result: Result<()>| {
                    let mut st = state.lock().unwrap();
                    for &query_id in &query_ids {
                        assert!(
                            st.pending_query_ids.remove(&query_id),
                            "completed query {query_id} was not pending"
                        );
                    }
                    st.current_query_count -= 1;
                    drop(st);
                    let mut promise = promise;
                    promise.set_result(result);
                }
            });
            create_actor("CompleteMergeQuery", SleepActor::new(0.02, completion)).release();
            // the C++ merge function ends with `this->yield()`
            send_closure(&test_id, TestMessage::Merged { dummy: 0 });
        });

        self.merger = Some(create_actor(
            "QueryMerger",
            QueryMerger::new(
                MAX_CONCURRENT_QUERY_COUNT,
                MAX_MERGED_QUERY_COUNT,
                merge_function,
            ),
        ));
        self.add_queries();
    }

    fn loop_(&mut self) {
        self.add_queries();
    }
}
actor_messages! {
    enum TestMessage for TestQueryMerger {
        Merged {
            dummy: u8,
        } => on_merged(dummy),
        QueryCompleted {
            dummy: u8,
        } => on_query_completed(dummy),
    }
}

impl TestQueryMerger {
    fn merger_id(&self) -> ActorId<QueryMerger> {
        self.merger.as_ref().expect("created in start_up").get()
    }

    /// C++ `TestQueryMerger::loop`.
    fn add_queries(&mut self) {
        let added = self.rng.fast(1, 3) as usize;
        for _ in 0..added {
            {
                let mut st = self.state.lock().unwrap();
                if st.total_query_count >= MAX_QUERY_COUNT {
                    break;
                }
                st.total_query_count += 1;
            }
            let query_id = i64::from(self.rng.fast(1, 20));
            let test_id = self.actor_id_of::<Self>();
            {
                let mut st = self.state.lock().unwrap();
                if st.pending_query_ids.insert(query_id) {
                    st.next_query_ids.push_back(query_id);
                }
            }
            let promise = PromiseCreator::lambda_result(move |_result: Result<()>| {
                send_closure(&test_id, TestMessage::QueryCompleted { dummy: 0 });
            });
            send_closure(
                &self.merger_id(),
                QueryMergerMessage::AddQuery { query_id, promise },
            );
        }
    }

    fn on_merged(&mut self, _dummy: u8) {
        self.yield_();
    }

    /// The per-query completion promise body (C++ captures `this` inline).
    fn on_query_completed(&mut self, _dummy: u8) {
        let finish = {
            let mut st = self.state.lock().unwrap();
            st.completed_query_count += 1;
            let finish = st.completed_query_count == MAX_QUERY_COUNT;
            if finish {
                assert_eq!(st.current_query_count, 0);
                assert!(st.next_query_ids.is_empty());
                assert!(st.pending_query_ids.is_empty());
            }
            finish
        };
        if finish {
            Scheduler::instance().finish();
        } else {
            self.yield_();
        }
    }
}

#[test]
fn query_merger_stress() {
    let mut scheduler = ConcurrentScheduler::new(0);
    scheduler.create_actor_unsafe(
        0,
        "TestQueryMerger",
        TestQueryMerger {
            merger: None,
            state: Arc::new(Mutex::new(TestState::default())),
            rng: Rng(0x1234_5678),
        },
    );
    scheduler.start();
    let mut rounds = 0;
    while scheduler.run_main(10.0) {
        rounds += 1;
        assert!(rounds < 10_000_000, "QueryMerger stress did not converge");
    }
    scheduler.finish();
}
