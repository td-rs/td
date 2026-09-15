//! Port of `td/telegram/QueryMerger.{h,cpp}` (M.4.4): broadcast duplicate
//! queries and cap concurrent merged batches.
//!
//! Mapping notes:
//! - C++ constructs the merger as a value member that registers itself by
//!   pointer; the Rust actor is scheduler-owned (`create_actor` in the
//!   owner's `start_up`) and driven through [`QueryMergerMessage`], which
//!   also removes the C++ direct-call cross-actor aliasing. Ordering is
//!   preserved (mailbox FIFO), so the merge invariants hold unchanged.
//! - C++ `set_merge_function` runs right after construction in the owner's
//!   `start_up`; here the merge function is a constructor argument.
//! - The C++ `source` parameter only feeds a `LOG(INFO)`; it is dropped (the
//!   port follows the S35 convention of carrying no logging).

use std::collections::{HashMap, VecDeque};

use tdactor::{Actor, actor_messages, promise_send_closure};
use tdutils::promise::{Promise, PromiseCreator};
use tdutils::status::Result;

/// C++ `QueryMerger::MergeFunction`: send one merged query for `query_ids`
/// and complete the promise with its result.
pub type MergeFunction = Box<dyn FnMut(Vec<i64>, Promise<()>) + Send>;

struct QueryInfo {
    promises: Vec<Promise<()>>,
}

impl QueryInfo {
    fn new() -> Self {
        Self { promises: Vec::new() }
    }
}

/// The port of C++ `class QueryMerger`.
pub struct QueryMerger {
    query_count: usize,
    max_concurrent_query_count: usize,
    max_merged_query_count: usize,
    merge_function: Option<MergeFunction>,
    pending_queries: VecDeque<i64>,
    queries: HashMap<i64, QueryInfo>,
}

impl QueryMerger {
    /// C++ `QueryMerger(name, max_concurrent_query_count, max_merged_query_count)`.
    pub fn new(
        max_concurrent_query_count: usize,
        max_merged_query_count: usize,
        merge_function: MergeFunction,
    ) -> Self {
        Self {
            query_count: 0,
            max_concurrent_query_count,
            max_merged_query_count,
            merge_function: Some(merge_function),
            pending_queries: VecDeque::new(),
            queries: HashMap::new(),
        }
    }

    /// C++ `QueryMerger::add_query`; duplicates just join the wait.
    pub fn add_query(&mut self, query_id: i64, promise: Promise<()>) {
        assert!(query_id != 0, "QueryMerger: zero query id");
        let query = self.queries.entry(query_id).or_insert_with(QueryInfo::new);
        query.promises.push(promise);
        if query.promises.len() != 1 {
            // duplicate query, just wait
            return;
        }
        self.pending_queries.push_back(query_id);
        self.loop_queries();
    }

    /// C++ `QueryMerger::send_query`.
    fn send_query(&mut self, query_ids: Vec<i64>) {
        let actor_id = self.actor_id_of::<Self>();
        let merge_function = self
            .merge_function
            .as_mut()
            .expect("QueryMerger without a merge function");
        self.query_count += 1;
        let ids_for_closure = query_ids.clone();
        let result_promise = PromiseCreator::lambda_result(promise_send_closure(
            actor_id,
            move |merger: &mut QueryMerger, result| {
                merger.on_get_query_result(ids_for_closure, result);
            },
        ));
        merge_function(query_ids, result_promise);
    }

    /// C++ `QueryMerger::on_get_query_result`.
    fn on_get_query_result(&mut self, query_ids: Vec<i64>, result: Result<()>) {
        self.query_count -= 1;
        for query_id in query_ids {
            let query = self
                .queries
                .remove(&query_id)
                .expect("QueryMerger: result for an unknown query");
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
        }
        self.loop_queries();
    }

    /// C++ `QueryMerger::loop`.
    fn loop_queries(&mut self) {
        if self.query_count == self.max_concurrent_query_count {
            return;
        }

        let mut query_ids: Vec<i64> = Vec::new();
        while let Some(query_id) = self.pending_queries.pop_front() {
            query_ids.push(query_id);
            if query_ids.len() == self.max_merged_query_count {
                self.send_query(std::mem::take(&mut query_ids));
                if self.query_count == self.max_concurrent_query_count {
                    break;
                }
            }
        }
        if !query_ids.is_empty() {
            self.send_query(query_ids);
        }
    }
}

impl Actor for QueryMerger {
    fn loop_(&mut self) {
        self.loop_queries();
    }
}

actor_messages! {
    pub enum QueryMergerMessage for QueryMerger {
        AddQuery {
            query_id: i64,
            promise: Promise<()>,
        } => add_query(query_id, promise),
    }
}
