//! Port of `td/db/binlog/ConcurrentBinlog` + `BinlogInterface`: a shared,
//! thread-safe binlog handle with atomic event-id allocation.
//!
//! C++ dispatches every operation through `BinlogActor`; the actor runtime is
//! Track D (not ported yet), so this subset serializes on a mutex and runs
//! promises inline. Event-id semantics match exactly:
//! `init` seeds the counter with `peek_next_event_id()` and
//! `next_event_id()` returns `fetch_add(1)` (the old value).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::event::BinlogEvent;
use super::{Binlog, BinlogInfo};
use crate::db_key::DbKey;
use tdutils::status::Result;
use tdutils::status::Status;

/// `Promise<Unit>` — a one-shot completion callback.
pub type Promise = Box<dyn FnOnce() + Send>;

struct Inner {
    binlog: Option<Binlog>,
    path: String,
    /// next expected seq_no; events are applied in allocation order, like the
    /// C++ BinlogActor OrderedEventsProcessor
    next_seq: u64,
    pending: BTreeMap<u64, Vec<u8>>,
}

/// `td::ConcurrentBinlog`
pub struct ConcurrentBinlog {
    inner: Arc<Mutex<Inner>>,
    last_event_id: Arc<AtomicU64>,
}

impl ConcurrentBinlog {
    pub fn new() -> ConcurrentBinlog {
        ConcurrentBinlog {
            inner: Arc::new(Mutex::new(Inner {
                binlog: None,
                path: String::new(),
                next_seq: 0,
                pending: BTreeMap::new(),
            })),
            last_event_id: Arc::new(AtomicU64::new(0)),
        }
    }

    /// `ConcurrentBinlog::init`
    pub fn init(
        &mut self,
        path: &str,
        callback: impl FnMut(&BinlogEvent),
        db_key: DbKey,
        old_db_key: DbKey,
    ) -> Result<BinlogInfo> {
        let mut binlog = Binlog::new();
        let info = {
            let mut callback = callback;
            let status = binlog.init(path, &mut callback, db_key, old_db_key);
            if status.is_error() {
                return Result::from_status(status);
            }
            binlog.get_info()
        };
        let mut inner = self.inner.lock().unwrap();
        inner.path = binlog.get_path().to_string();
        self.last_event_id
            .store(binlog.peek_next_event_id(), Ordering::Relaxed);
        inner.next_seq = binlog.peek_next_event_id();
        inner.binlog = Some(binlog);
        Result::Ok(info)
    }

    /// `BinlogInterface::next_event_id()`
    pub fn next_event_id(&self) -> u64 {
        self.last_event_id.fetch_add(1, Ordering::Relaxed)
    }

    /// `BinlogInterface::next_event_id(shift)`
    pub fn next_event_id_shift(&self, shift: i32) -> u64 {
        self.last_event_id
            .fetch_add(shift as u64, Ordering::Relaxed)
    }

    pub fn get_path(&self) -> String {
        self.inner.lock().unwrap().path.clone()
    }

    /// `BinlogInterface::add`
    pub fn add(&self, type_: i32, data: &[u8]) -> u64 {
        let event_id = self.next_event_id();
        self.add_raw_event(event_id, BinlogEvent::create_raw(event_id, type_, 0, data));
        event_id
    }

    /// `BinlogInterface::rewrite`
    pub fn rewrite(&self, event_id: u64, type_: i32, data: &[u8]) -> u64 {
        let seq_no = self.next_event_id();
        self.add_raw_event(
            seq_no,
            BinlogEvent::create_raw(event_id, type_, BinlogEvent::FLAG_REWRITE, data),
        );
        seq_no
    }

    /// `BinlogInterface::erase`
    pub fn erase(&self, event_id: u64) -> u64 {
        let seq_no = self.next_event_id();
        self.add_raw_event(
            seq_no,
            BinlogEvent::create_raw(
                event_id,
                BinlogEvent::TYPE_EMPTY,
                BinlogEvent::FLAG_REWRITE,
                &[],
            ),
        );
        seq_no
    }

    /// `BinlogInterface::erase_batch` — consecutive ids, returns the first.
    pub fn erase_batch(&self, event_ids: &[u64]) -> u64 {
        if event_ids.is_empty() {
            return 0;
        }
        let seq_no = self.next_event_id_shift(event_ids.len() as i32);
        for (offset, event_id) in event_ids.iter().enumerate() {
            self.add_raw_event(
                seq_no + offset as u64,
                BinlogEvent::create_raw(
                    *event_id,
                    BinlogEvent::TYPE_EMPTY,
                    BinlogEvent::FLAG_REWRITE,
                    &[],
                ),
            );
        }
        seq_no
    }

    /// `BinlogInterface::add_raw_event` — `seq_no` is the slot allocated from
    /// the event-id counter; events are applied in seq order.
    pub fn add_raw_event(&self, seq_no: u64, raw_event: Vec<u8>) {
        let mut inner = self.inner.lock().unwrap();
        inner.pending.insert(seq_no, raw_event);
        loop {
            let next = inner.next_seq;
            let Some(raw) = inner.pending.remove(&next) else {
                break;
            };
            if let Some(binlog) = inner.binlog.as_mut() {
                binlog.add_raw_event(raw);
            }
            inner.next_seq += 1;
        }
    }

    /// `BinlogInterface::force_sync`
    pub fn force_sync(&self, promise: Option<Promise>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(binlog) = inner.binlog.as_mut() {
            binlog.sync();
        }
        drop(inner);
        if let Some(promise) = promise {
            promise();
        }
    }

    /// `BinlogInterface::force_flush`
    pub fn force_flush(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(binlog) = inner.binlog.as_mut() {
            binlog.flush();
        }
    }

    /// `BinlogInterface::change_key` — requires the encryption slice
    /// (reindex-with-encryption), reported as an error like a wrong password.
    pub fn change_key(&self, _db_key: DbKey, promise: Option<Promise>) -> Status {
        let status = Status::error_message("change_key is not supported yet");
        if let Some(promise) = promise {
            promise();
        }
        status
    }

    /// `BinlogInterface::close`
    pub fn close(&self, promise: Option<Promise>) {
        // Recover from a poisoned lock: a panic in an earlier binlog call
        // must not turn the close path (reachable from destructors) into
        // an abort that masks the original failure.
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(mut binlog) = inner.binlog.take() {
            binlog.close(true).ensure();
        }
        inner.path.clear();
        drop(inner);
        if let Some(promise) = promise {
            promise();
        }
    }

    /// `BinlogInterface::close_and_destroy`
    pub fn close_and_destroy(&self, promise: Option<Promise>) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(mut binlog) = inner.binlog.take() {
            binlog.close_and_destroy().ensure();
        }
        inner.path.clear();
        drop(inner);
        if let Some(promise) = promise {
            promise();
        }
    }
}

impl Default for ConcurrentBinlog {
    fn default() -> Self {
        Self::new()
    }
}
