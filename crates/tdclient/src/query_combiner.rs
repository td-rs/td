//! Port of `td/telegram/QueryCombiner.{h,cpp}` (M.4.4): combine identical
//! queries into one in-flight request, optionally delaying promise-less
//! queries by `min_delay` to respect flood limits.
//!
//! Mapping notes:
//! - Like `QueryMerger`, the C++ value-member actor becomes a
//!   scheduler-owned actor driven through [`QueryCombinerMessage`] (mailbox
//!   FIFO keeps the combine semantics unchanged).
//! - C++ `G()->close_flag()` has no singleton to read in the Rust port; the
//!   closed check is an injected `Box<dyn Fn() -> bool + Send>` (the
//!   integration can capture a Global handle). The C++
//!   `set_timeout_in`/`timeout_expired` pairing rides the default
//!   `loop_` dispatch of the ported `Actor` trait.

use std::collections::{HashMap, VecDeque};

use tdactor::{Actor, actor_messages, promise_send_closure};
use tdutils::promise::{Promise, PromiseCreator};
use tdutils::status::Result;
use tdutils::time;

struct QueryInfo {
    promises: Vec<Promise<()>>,
    is_sent: bool,
    send_query: Option<Promise<Promise<()>>>,
}

impl QueryInfo {
    fn new() -> Self {
        Self {
            promises: Vec::new(),
            is_sent: false,
            send_query: None,
        }
    }
}

/// The port of C++ `class QueryCombiner`.
pub struct QueryCombiner {
    is_closed: Box<dyn Fn() -> bool + Send>,
    query_count: i32,
    next_query_time: f64,
    min_delay: f64,
    delayed_queries: VecDeque<i64>,
    queries: HashMap<i64, QueryInfo>,
}

impl QueryCombiner {
    /// C++ `QueryCombiner(name, min_delay)`; `is_closed` replaces the C++
    /// `G()->close_flag()` probe (see the module docs).
    pub fn new(min_delay: f64, is_closed: Box<dyn Fn() -> bool + Send>) -> Self {
        Self {
            is_closed,
            query_count: 0,
            next_query_time: time::now(),
            min_delay,
            delayed_queries: VecDeque::new(),
            queries: HashMap::new(),
        }
    }

    /// C++ `QueryCombiner::add_query`.
    pub fn add_query(
        &mut self,
        query_id: i64,
        send_query: Promise<Promise<()>>,
        promise: Promise<()>,
    ) {
        let query = self.queries.entry(query_id).or_insert_with(QueryInfo::new);
        if !promise.is_empty() {
            query.promises.push(promise);
        } else if self.min_delay > 0.0 && !query.is_sent {
            // if there is no promise, then no one waits for response
            // we can delay query to not exceed any flood limit
            if query.send_query.is_some() {
                // the query is already delayed
                return;
            }
            query.send_query = Some(send_query);
            self.delayed_queries.push_back(query_id);
            self.loop_queries();
            return;
        }
        let query = self.queries.get_mut(&query_id).expect("entry just created");
        if query.is_sent {
            // just wait for the result
            return;
        }

        if query.send_query.is_none() {
            query.send_query = Some(send_query);
        }
        self.do_send_query(query_id);
    }

    /// C++ `QueryCombiner::do_send_query`.
    fn do_send_query(&mut self, query_id: i64) {
        let query = self
            .queries
            .get_mut(&query_id)
            .expect("QueryCombiner: sending an unknown query");
        query.is_sent = true;
        let mut send_query = query
            .send_query
            .take()
            .expect("QueryCombiner: no send_query promise");
        self.next_query_time = time::now() + self.min_delay;
        self.query_count += 1;
        let actor_id = self.actor_id_of::<Self>();
        let result_promise = PromiseCreator::lambda_result(promise_send_closure(
            actor_id,
            move |combiner: &mut QueryCombiner, result| {
                combiner.on_get_query_result(query_id, result);
            },
        ));
        send_query.set_value(result_promise);
    }

    /// C++ `QueryCombiner::on_get_query_result`.
    fn on_get_query_result(&mut self, query_id: i64, result: Result<()>) {
        self.query_count -= 1;
        let query = self
            .queries
            .remove(&query_id)
            .expect("QueryCombiner: result for an unknown query");
        assert!(query.is_sent, "QueryCombiner: result for an unsent query");
        match &result {
            Result::Ok(()) => {
                for mut promise in query.promises {
                    promise.set_value(());
                }
            }
            Result::Err(status) => {
                for mut promise in query.promises {
                    promise.set_error(status.clone());
                }
            }
        }
        self.loop_queries();
    }

    /// C++ `QueryCombiner::loop`.
    fn loop_queries(&mut self) {
        if (self.is_closed)() {
            return;
        }

        let now = time::now();
        if now < self.next_query_time {
            self.set_timeout_in(self.next_query_time - now + 0.001);
            return;
        }
        if self.query_count != 0 {
            return;
        }

        while let Some(query_id) = self.delayed_queries.pop_front() {
            let ready = self
                .queries
                .get(&query_id)
                .is_some_and(|query| !query.is_sent);
            if ready {
                self.do_send_query(query_id);
                break;
            }
        }
    }
}

impl Actor for QueryCombiner {
    fn loop_(&mut self) {
        self.loop_queries();
    }
}

actor_messages! {
    pub enum QueryCombinerMessage for QueryCombiner {
        AddQuery {
            query_id: i64,
            send_query: Promise<Promise<()>>,
            promise: Promise<()>,
        } => add_query(query_id, send_query, promise),
    }
}
