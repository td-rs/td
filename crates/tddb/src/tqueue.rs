//! Port of `td/db/TQueue`: persistent queue-of-queues with event-id based
//! reads, GC of expired events, and pluggable storage (memory or binlog).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Instant;

use crate::binlog::concurrent::ConcurrentBinlog;
use crate::binlog::event::BinlogEvent;
use crate::binlog_key_value::{parse_tl_string, store_tl_string};
use tdutils::random;
use tdutils::status::Result;
use tdutils::status::Status;

/// `TQueue::EventId`
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash, Default)]
pub struct EventId(i32);

/// `TQueue::Event`
#[derive(Clone, Debug, Default)]
pub struct Event {
    pub id: EventId,
    pub expires_at: i32,
    pub data: Vec<u8>,
    pub extra: i64,
}

/// `TQueue::RawEvent`
#[derive(Clone, Debug)]
pub struct RawEvent {
    pub log_event_id: u64,
    pub event_id: EventId,
    pub expires_at: i32,
    pub data: Vec<u8>,
    pub extra: i64,
}

pub type QueueId = i64;

/// `TQueue::StorageCallback`
pub trait StorageCallback {
    fn push(&mut self, queue_id: QueueId, event: &RawEvent) -> u64;
    fn pop(&mut self, log_event_id: u64);
    fn pop_batch(&mut self, log_event_ids: Vec<u64>) {
        for id in log_event_ids {
            self.pop(id);
        }
    }
    fn close(&mut self);
    /// Downcast support (C++ uses concrete template types instead).
    fn as_any(&mut self) -> &mut dyn std::any::Any;
}

/// `td::TQueue`
pub trait TQueue {
    fn set_callback(&mut self, callback: Box<dyn StorageCallback>);
    fn extract_callback(&mut self) -> Option<Box<dyn StorageCallback>>;
    fn do_push(&mut self, queue_id: QueueId, raw_event: RawEvent) -> bool;
    fn push(
        &mut self,
        queue_id: QueueId,
        data: Vec<u8>,
        expires_at: i32,
        extra: i64,
        hint_new_id: EventId,
    ) -> Result<EventId>;
    fn forget(&mut self, queue_id: QueueId, event_id: EventId);
    fn clear(&mut self, queue_id: QueueId, keep_count: usize) -> BTreeMap<EventId, RawEvent>;
    fn get_head(&self, queue_id: QueueId) -> EventId;
    fn get_tail(&self, queue_id: QueueId) -> EventId;
    /// C++ takes a pre-sized `MutableSpan<Event>`; the caller sizes
    /// `result_events` (placeholders) and it is truncated to the ready count.
    fn get(
        &mut self,
        queue_id: QueueId,
        from_id: EventId,
        forget_previous: bool,
        unix_time_now: i32,
        result_events: &mut Vec<Event>,
    ) -> Result<usize>;
    fn get_size(&self, queue_id: QueueId) -> usize;
    /// returns (number of deleted events, whether GC was completed)
    fn run_gc(&mut self, unix_time_now: i32) -> (i64, bool);
    fn close(&mut self);
}

impl EventId {
    pub const MAX_ID: i32 = 2_000_000_000;

    pub fn from_int32(id: i32) -> Result<EventId> {
        if !Self::is_valid_id(id) {
            return Result::from_status(Status::error_message("Invalid ID"));
        }
        Result::Ok(EventId(id))
    }

    pub fn is_valid(self) -> bool {
        !self.empty() && Self::is_valid_id(self.0)
    }

    pub fn value(self) -> i32 {
        self.0
    }

    pub fn next(self) -> Result<EventId> {
        Self::from_int32(self.0 + 1)
    }

    pub fn advance(self, offset: usize) -> Result<EventId> {
        match self.0.checked_add_unsigned(offset as u32) {
            Some(new_id) => Self::from_int32(new_id),
            None => Result::from_status(Status::error_message("Invalid ID")),
        }
    }

    pub fn empty(self) -> bool {
        self.0 == 0
    }

    fn is_valid_id(id: i32) -> bool {
        (0..Self::MAX_ID).contains(&id)
    }
}

#[derive(Default)]
struct Queue {
    tail_id: EventId,
    events: BTreeMap<EventId, RawEvent>,
    total_event_length: usize,
    gc_at: i32,
}

/// `td::TQueueImpl`
#[derive(Default)]
pub struct TQueueImpl {
    queues: HashMap<QueueId, Queue>,
    queue_gc_at: BTreeSet<(i32, QueueId)>,
    callback: Option<Box<dyn StorageCallback>>,
}

const MAX_EVENT_LENGTH: usize = 65536 * 8;
const MAX_QUEUE_EVENTS: usize = 100000;
const MAX_TOTAL_EVENT_LENGTH: usize = 1 << 27;

impl TQueueImpl {
    fn get_queue_head(q: &Queue) -> EventId {
        match q.events.keys().next() {
            Some(first) => *first,
            None => q.tail_id,
        }
    }

    fn get_queue_size(q: &Queue) -> usize {
        match q.events.keys().next_back() {
            None => 0,
            Some(last) => {
                let placeholder = q.events[last].data.is_empty();
                q.events.len() - usize::from(placeholder)
            }
        }
    }

    fn remove_event(q: &mut Queue, key: EventId) {
        if let Some(event) = q.events.remove(&key) {
            q.total_event_length -= event.data.len();
        }
    }
}

/// Disjoint-field view of `TQueueImpl` for one queue's mutable operations.
struct QueueMut<'a> {
    callback: &'a mut Option<Box<dyn StorageCallback>>,
    queue_gc_at: &'a mut BTreeSet<(i32, QueueId)>,
    q: &'a mut Queue,
}

impl QueueMut<'_> {
    /// `TQueueImpl::pop` (the iterator walk becomes a key-based operation)
    fn pop_event(&mut self, queue_id: QueueId, key: EventId, tail_id: EventId) {
        let Some(event) = self.q.events.get(&key).cloned() else {
            return;
        };
        if self.callback.is_none() || event.log_event_id == 0 {
            TQueueImpl::remove_event(self.q, key);
            return;
        }

        if matches!(event.event_id.next(), Result::Ok(next) if next == tail_id) {
            if !event.data.is_empty() {
                self.q.total_event_length -= event.data.len();
                let mut cleared = event.clone();
                cleared.data = Vec::new();
                self.q.events.insert(key, cleared.clone());
                self.callback.as_mut().unwrap().push(queue_id, &cleared);
            }
            // placeholder stays in place (the C++ advances the iterator)
        } else {
            self.callback.as_mut().unwrap().pop(event.log_event_id);
            TQueueImpl::remove_event(self.q, key);
        }
    }

    /// `TQueueImpl::schedule_queue_gc`
    fn schedule_queue_gc(&mut self, queue_id: QueueId, gc_at: i32) {
        if self.q.gc_at != 0 {
            let was_there = self.queue_gc_at.remove(&(self.q.gc_at, queue_id));
            assert!(was_there);
        }
        self.q.gc_at = gc_at;
        if self.q.gc_at != 0 {
            let inserted = self.queue_gc_at.insert((self.q.gc_at, queue_id));
            assert!(inserted);
        }
    }

    /// `TQueueImpl::do_get`
    fn do_get(
        &mut self,
        queue_id: QueueId,
        from_id: EventId,
        forget_previous: bool,
        unix_time_now: i32,
        result_events: &mut Vec<Event>,
    ) {
        if forget_previous {
            let forgotten: Vec<EventId> = self.q.events.range(..from_id).map(|(k, _)| *k).collect();
            for key in forgotten {
                let tail_id = self.q.tail_id;
                self.pop_event(queue_id, key, tail_id);
            }
        }

        let mut ready_n = 0;
        let keys: Vec<EventId> = self.q.events.range(from_id..).map(|(k, _)| *k).collect();
        for key in keys {
            let event = &self.q.events[&key];
            if event.expires_at < unix_time_now || event.data.is_empty() {
                let tail_id = self.q.tail_id;
                self.pop_event(queue_id, key, tail_id);
            } else {
                if ready_n == result_events.len() {
                    break;
                }
                result_events[ready_n] = Event {
                    id: event.event_id,
                    expires_at: event.expires_at,
                    data: event.data.clone(),
                    extra: event.extra,
                };
                ready_n += 1;
            }
        }
        result_events.truncate(ready_n);
    }
}

impl Default for Box<dyn TQueue> {
    fn default() -> Self {
        Box::new(TQueueImpl::default())
    }
}

impl TQueue for TQueueImpl {
    fn set_callback(&mut self, callback: Box<dyn StorageCallback>) {
        self.callback = Some(callback);
    }

    fn extract_callback(&mut self) -> Option<Box<dyn StorageCallback>> {
        self.callback.take()
    }

    /// `TQueueImpl::do_push`
    fn do_push(&mut self, queue_id: QueueId, mut raw_event: RawEvent) -> bool {
        assert!(raw_event.event_id.is_valid());
        // raw_event.data can be empty when replaying binlog
        if raw_event.data.len() > MAX_EVENT_LENGTH || queue_id == 0 {
            return false;
        }
        let TQueueImpl {
            queues,
            queue_gc_at,
            callback,
        } = self;
        let mut parts = QueueMut {
            callback,
            queue_gc_at,
            q: queues.entry(queue_id).or_default(),
        };
        if parts.q.events.len() >= MAX_QUEUE_EVENTS
            || parts.q.total_event_length > MAX_TOTAL_EVENT_LENGTH - raw_event.data.len()
            || raw_event.expires_at <= 0
        {
            return false;
        }
        let event_id = raw_event.event_id;
        if event_id < parts.q.tail_id {
            return false;
        }

        if let Some(last_key) = parts.q.events.keys().next_back().copied() {
            let is_placeholder = parts.q.events[&last_key].data.is_empty();
            if is_placeholder {
                let log_event_id = parts.q.events[&last_key].log_event_id;
                if log_event_id != 0 {
                    parts.callback.as_mut().unwrap().pop(log_event_id);
                }
                parts.q.events.remove(&last_key);
            }
        }
        let queue_was_empty = parts.q.events.is_empty();
        if queue_was_empty && !raw_event.data.is_empty() {
            parts.schedule_queue_gc(queue_id, raw_event.expires_at);
        }

        if raw_event.log_event_id == 0
            && let Some(callback) = parts.callback.as_mut()
        {
            raw_event.log_event_id = callback.push(queue_id, &raw_event);
        }
        parts.q.tail_id = raw_event.event_id.next().unwrap();
        parts.q.total_event_length += raw_event.data.len();
        parts.q.events.insert(raw_event.event_id, raw_event);
        true
    }

    /// `TQueueImpl::push`
    fn push(
        &mut self,
        queue_id: QueueId,
        data: Vec<u8>,
        expires_at: i32,
        extra: i64,
        hint_new_id: EventId,
    ) -> Result<EventId> {
        if data.is_empty() {
            return Result::from_status(Status::error_message("Data is empty"));
        }
        if data.len() > MAX_EVENT_LENGTH {
            return Result::from_status(Status::error_message("Data is too big"));
        }
        if queue_id == 0 {
            return Result::from_status(Status::error_message("Queue identifier is invalid"));
        }

        {
            let q = self.queues.entry(queue_id).or_default();
            if q.events.len() >= MAX_QUEUE_EVENTS {
                return Result::from_status(Status::error_message("Queue is full"));
            }
            if q.total_event_length > MAX_TOTAL_EVENT_LENGTH - data.len() {
                return Result::from_status(Status::error_message("Queue size is too big"));
            }
            if expires_at <= 0 {
                return Result::from_status(Status::error_message(
                    "Failed to add already expired event",
                ));
            }
        }
        let mut event_id;
        loop {
            let TQueueImpl {
                queues,
                queue_gc_at,
                callback,
            } = self;
            let mut parts = QueueMut {
                callback,
                queue_gc_at,
                q: queues.entry(queue_id).or_default(),
            };
            if parts.q.tail_id.empty() {
                if hint_new_id.empty() {
                    parts.q.tail_id = EventId::from_int32(random::fast(
                        2 * MAX_QUEUE_EVENTS.max(1_000_000) as i32 + 1,
                        EventId::MAX_ID / 2,
                    ))
                    .unwrap();
                } else {
                    parts.q.tail_id = hint_new_id;
                }
            }
            event_id = parts.q.tail_id;
            assert!(event_id.is_valid());
            if event_id.next().is_ok() {
                break;
            }
            // event-id space exhausted: pop everything and start over
            let keys: Vec<EventId> = parts.q.events.keys().copied().collect();
            for key in keys {
                parts.pop_event(queue_id, key, EventId::default());
            }
            parts.q.tail_id = EventId::default();
            assert!(
                hint_new_id.next().is_ok(),
                "hint id at the end of the range"
            );
        }

        let raw_event = RawEvent {
            log_event_id: 0,
            event_id,
            expires_at,
            data,
            extra,
        };
        let is_added = self.do_push(queue_id, raw_event);
        assert!(is_added);
        Result::Ok(event_id)
    }

    /// `TQueueImpl::get_head`
    fn get_head(&self, queue_id: QueueId) -> EventId {
        match self.queues.get(&queue_id) {
            None => EventId::default(),
            Some(q) => Self::get_queue_head(q),
        }
    }

    /// `TQueueImpl::get_tail`
    fn get_tail(&self, queue_id: QueueId) -> EventId {
        match self.queues.get(&queue_id) {
            None => EventId::default(),
            Some(q) => q.tail_id,
        }
    }

    /// `TQueueImpl::forget`
    fn forget(&mut self, queue_id: QueueId, event_id: EventId) {
        let TQueueImpl {
            queues,
            queue_gc_at,
            callback,
        } = self;
        let Some(q) = queues.get_mut(&queue_id) else {
            return;
        };
        if !q.events.contains_key(&event_id) {
            return;
        }
        let tail_id = q.tail_id;
        let mut parts = QueueMut {
            callback,
            queue_gc_at,
            q,
        };
        parts.pop_event(queue_id, event_id, tail_id);
    }

    /// `TQueueImpl::clear`
    fn clear(&mut self, queue_id: QueueId, keep_count: usize) -> BTreeMap<EventId, RawEvent> {
        let TQueueImpl { queues, .. } = self;
        let Some(q) = queues.get_mut(&queue_id) else {
            return BTreeMap::new();
        };
        let size = Self::get_queue_size(q);
        if size <= keep_count {
            return BTreeMap::new();
        }

        // C++ leaves the last event as an empty placeholder when nothing is
        // kept and the storage can rewrite it
        let mut split_index = size - keep_count;
        if keep_count == 0 {
            let last_key = *q.events.keys().next_back().unwrap();
            let rewrite_placeholder = self.callback.is_some()
                && q.events[&last_key].log_event_id != 0
                && !q.events[&last_key].data.is_empty();
            if rewrite_placeholder {
                let mut cleared = q.events[&last_key].clone();
                q.total_event_length -= cleared.data.len();
                cleared.data = Vec::new();
                q.events.insert(last_key, cleared.clone());
                if let Some(callback) = self.callback.as_mut() {
                    callback.push(queue_id, &cleared);
                }
                // the placeholder itself is kept, like C++'s ++end_it
                split_index = size - 1;
            }
        }
        let split_key = q.events.keys().nth(split_index).copied();

        if self.callback.is_some() {
            let collect_ids = |events: &BTreeMap<EventId, RawEvent>| {
                events
                    .values()
                    .filter_map(|event| {
                        if event.log_event_id != 0 {
                            Some(event.log_event_id)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<u64>>()
            };
            let deleted_log_event_ids: Vec<u64> = match split_key {
                Some(split_key) => collect_ids(
                    &q.events
                        .range(..split_key)
                        .map(|(k, v)| (*k, v.clone()))
                        .collect(),
                ),
                None => collect_ids(&q.events),
            };
            if let Some(callback) = self.callback.as_mut() {
                callback.pop_batch(deleted_log_event_ids);
            }
        }

        let deleted = match split_key {
            Some(split_key) => {
                // events >= split_key stay, everything before is deleted
                let kept = q.events.split_off(&split_key);
                std::mem::replace(&mut q.events, kept)
            }
            None => std::mem::take(&mut q.events),
        };
        q.total_event_length = q.events.values().map(|e| e.data.len()).sum();

        deleted
    }

    /// `TQueueImpl::get`
    fn get(
        &mut self,
        queue_id: QueueId,
        from_id: EventId,
        forget_previous: bool,
        unix_time_now: i32,
        result_events: &mut Vec<Event>,
    ) -> Result<usize> {
        let TQueueImpl {
            queues,
            queue_gc_at,
            callback,
        } = self;
        let Some(q) = queues.get_mut(&queue_id) else {
            result_events.clear();
            return Result::Ok(0);
        };
        // Some sanity checks
        if from_id.value() > q.tail_id.value() + 10 {
            return Result::from_status(Status::error_message(
                "Specified from_id is in the future",
            ));
        }
        if from_id.value() < Self::get_queue_head(q).value() - MAX_QUEUE_EVENTS as i32 {
            return Result::from_status(Status::error_message("Specified from_id is in the past"));
        }

        let mut parts = QueueMut {
            callback,
            queue_gc_at,
            q,
        };
        parts.do_get(
            queue_id,
            from_id,
            forget_previous,
            unix_time_now,
            result_events,
        );
        Result::Ok(TQueueImpl::get_queue_size(parts.q))
    }

    /// `TQueueImpl::run_gc`
    fn run_gc(&mut self, unix_time_now: i32) -> (i64, bool) {
        let mut deleted_events: i64 = 0;
        let max_finish_time = Instant::now() + std::time::Duration::from_millis(50);
        let mut counter: u64 = 0;
        while let Some(&(gc_at, queue_id)) = self.queue_gc_at.first() {
            if gc_at >= unix_time_now {
                break;
            }
            let TQueueImpl {
                queues,
                queue_gc_at,
                callback,
            } = self;
            let mut parts = QueueMut {
                callback,
                queue_gc_at,
                q: queues.entry(queue_id).or_default(),
            };
            assert_eq!(parts.q.gc_at, gc_at);
            let mut new_gc_at = 0;

            if !parts.q.events.is_empty() {
                let size_before = TQueueImpl::get_queue_size(parts.q);
                let keys: Vec<EventId> = parts.q.events.keys().copied().collect();
                for key in keys {
                    counter += 1;
                    if (counter & 128) == 0 && Instant::now() >= max_finish_time {
                        if new_gc_at == 0 {
                            new_gc_at = parts.q.events[&key].expires_at;
                        }
                        break;
                    }
                    let event = &parts.q.events[&key];
                    if event.expires_at < unix_time_now || event.data.is_empty() {
                        let tail_id = parts.q.tail_id;
                        parts.pop_event(queue_id, key, tail_id);
                    } else {
                        if new_gc_at != 0 {
                            break;
                        }
                        new_gc_at = event.expires_at;
                    }
                }
                let size_after = TQueueImpl::get_queue_size(parts.q);
                assert!(size_after <= size_before);
                deleted_events += (size_before - size_after) as i64;
            }
            parts.schedule_queue_gc(queue_id, new_gc_at);
            if Instant::now() >= max_finish_time {
                return (deleted_events, false);
            }
        }
        (deleted_events, true)
    }

    /// `TQueueImpl::get_size`
    fn get_size(&self, queue_id: QueueId) -> usize {
        match self.queues.get(&queue_id) {
            None => 0,
            Some(q) => Self::get_queue_size(q),
        }
    }

    /// `TQueueImpl::close`
    fn close(&mut self) {
        if let Some(mut callback) = self.callback.take() {
            callback.close();
        }
    }
}

/// `TQueue::create()`
pub fn create() -> Box<dyn TQueue> {
    Box::new(TQueueImpl::default())
}

/// `TQueueLogEvent` wire format:
/// `[queue_id: i64][event_id: i32][expires_at: i32][data: TL string][extra: i64?]`
fn serialize_log_event(
    queue_id: QueueId,
    event_id: i32,
    expires_at: i32,
    data: &[u8],
    extra: i64,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&queue_id.to_le_bytes());
    out.extend_from_slice(&event_id.to_le_bytes());
    out.extend_from_slice(&expires_at.to_le_bytes());
    store_tl_string(&mut out, data);
    if extra != 0 {
        out.extend_from_slice(&extra.to_le_bytes());
    }
    out
}

fn parse_log_event(data: &[u8], has_extra: i32) -> Result<(QueueId, i32, i32, Vec<u8>, i64)> {
    let parser = data;
    if parser.len() < 20 {
        return Result::from_status(Status::error_message("Log event is too small"));
    }
    let queue_id = i64::from_le_bytes(parser[0..8].try_into().unwrap());
    let event_id = i32::from_le_bytes(parser[8..12].try_into().unwrap());
    let expires_at = i32::from_le_bytes(parser[12..16].try_into().unwrap());
    let mut rest = &parser[16..];
    let data_vec = match parse_tl_string(&mut rest) {
        Result::Ok(v) => v,
        Result::Err(status) => return Result::from_status(status),
    };
    let data = data_vec;
    let extra = if has_extra == 0 {
        0
    } else {
        if rest.len() < 8 {
            return Result::from_status(Status::error_message("Missing extra"));
        }
        i64::from_le_bytes(rest[0..8].try_into().unwrap())
    };
    Result::Ok((queue_id, event_id, expires_at, data, extra))
}

/// `TQueue::BINLOG_EVENT_TYPE`
pub const BINLOG_EVENT_TYPE: i32 = 2314;

/// `td::TQueueBinlog<ConcurrentBinlog>`
pub struct TQueueBinlog {
    binlog: Option<Arc<ConcurrentBinlog>>,
}

impl TQueueBinlog {
    pub fn new() -> TQueueBinlog {
        TQueueBinlog { binlog: None }
    }

    pub fn set_binlog(&mut self, binlog: Arc<ConcurrentBinlog>) {
        self.binlog = Some(binlog);
    }

    /// `TQueueBinlog::replay`
    pub fn replay(&self, binlog_event: &BinlogEvent, q: &mut dyn TQueue) -> Status {
        let has_extra = binlog_event.type_ - BINLOG_EVENT_TYPE;
        if has_extra != 0 && has_extra != 1 {
            return Status::error_message("Wrong magic");
        }
        let parsed = match parse_log_event(binlog_event.get_data(), has_extra) {
            Result::Ok(parsed) => parsed,
            Result::Err(status) => return status,
        };
        let (queue_id, event_id, expires_at, data, extra) = parsed;
        let event_id = match EventId::from_int32(event_id) {
            Result::Ok(event_id) => event_id,
            Result::Err(status) => return status,
        };
        let raw_event = RawEvent {
            log_event_id: binlog_event.id,
            event_id,
            expires_at,
            data,
            extra,
        };
        if !q.do_push(queue_id, raw_event) {
            return Status::error_message("Failed to add event");
        }
        Status::ok()
    }
}

impl Default for TQueueBinlog {
    fn default() -> Self {
        Self::new()
    }
}

impl StorageCallback for TQueueBinlog {
    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn push(&mut self, queue_id: QueueId, event: &RawEvent) -> u64 {
        let binlog = self.binlog.as_ref().expect("TQueueBinlog has no binlog");
        let data = serialize_log_event(
            queue_id,
            event.event_id.value(),
            event.expires_at,
            &event.data,
            event.extra,
        );
        let magic = BINLOG_EVENT_TYPE + i32::from(event.extra != 0);
        if event.log_event_id == 0 {
            binlog.add(magic, &data)
        } else {
            binlog.rewrite(event.log_event_id, magic, &data);
            event.log_event_id
        }
    }

    fn pop(&mut self, log_event_id: u64) {
        self.binlog
            .as_ref()
            .expect("TQueueBinlog has no binlog")
            .erase(log_event_id);
    }

    fn close(&mut self) {
        if let Some(binlog) = self.binlog.take() {
            binlog.close(None);
        }
    }
}

/// `td::TQueueMemoryStorage`
#[derive(Default)]
pub struct TQueueMemoryStorage {
    next_log_event_id: u64,
    events: BTreeMap<u64, (QueueId, RawEvent)>,
}

impl TQueueMemoryStorage {
    pub fn new() -> TQueueMemoryStorage {
        TQueueMemoryStorage {
            next_log_event_id: 1,
            events: BTreeMap::new(),
        }
    }

    /// `TQueueMemoryStorage::replay`
    pub fn replay(&self, q: &mut dyn TQueue) {
        for (log_event_id, (queue_id, raw_event)) in self.events.iter() {
            let mut raw_event = raw_event.clone();
            raw_event.log_event_id = *log_event_id;
            let is_added = q.do_push(*queue_id, raw_event);
            assert!(is_added);
        }
    }
}

impl StorageCallback for TQueueMemoryStorage {
    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn push(&mut self, queue_id: QueueId, event: &RawEvent) -> u64 {
        let log_event_id = if event.log_event_id == 0 {
            let id = self.next_log_event_id;
            self.next_log_event_id += 1;
            id
        } else {
            event.log_event_id
        };
        self.events.insert(log_event_id, (queue_id, event.clone()));
        log_event_id
    }

    fn pop(&mut self, log_event_id: u64) {
        self.events.remove(&log_event_id);
    }

    fn close(&mut self) {
        self.events.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binlog::destroy;
    use crate::db_key::DbKey;
    use std::sync::Arc;
    use tdutils::random::Xorshift128plus;

    /// tdlib test `TEST(TQueue, hands)`.
    #[test]
    fn tqueue_hands() {
        let mut tqueue = create();
        let qid = 12;
        assert!(tqueue.get_head(qid).empty());
        assert!(tqueue.get_tail(qid).empty());
        tqueue
            .push(qid, b"hello".to_vec(), 1, 0, EventId::default())
            .unwrap();
        let head = tqueue.get_head(qid);
        let tail = tqueue.get_tail(qid);
        assert_eq!(head.next().unwrap(), tail);
        let mut events = vec![Event::default(); 100];

        assert_eq!(tqueue.get(qid, head, true, 0, &mut events).unwrap(), 1);
        assert_eq!(tqueue.get(qid, head, true, 0, &mut events).unwrap(), 1);
        assert_eq!(tqueue.get(qid, tail, false, 0, &mut events).unwrap(), 1);
        assert_eq!(tqueue.get(qid, head, true, 0, &mut events).unwrap(), 1);
        assert_eq!(tqueue.get(qid, tail, true, 0, &mut events).unwrap(), 0);
        assert_eq!(tqueue.get(qid, head, true, 0, &mut events).unwrap(), 0);
    }

    /// tdlib test `TEST(TQueue, clear)` (scaled to 10k events).
    #[test]
    fn tqueue_clear() {
        let mut tqueue = create();
        let now = 0;
        for _ in 0..10000 {
            tqueue
                .push(
                    1,
                    vec![b'a'; random::fast(100, 500) as usize],
                    now + 600000,
                    0,
                    EventId::default(),
                )
                .unwrap();
        }
        let tail_id = tqueue.get_tail(1);
        let keep_count = random::fast(0, 2) as usize;
        let deleted_events = tqueue.clear(1, keep_count);
        assert_eq!(tqueue.get_size(1), keep_count);
        assert_eq!(tqueue.get_head(1).advance(keep_count).unwrap(), tail_id);
        assert_eq!(tqueue.get_tail(1), tail_id);
        assert_eq!(deleted_events.len(), 10000 - keep_count);
    }

    /// Differential port of tdlib test `TEST(TQueue, random)` (scaled to 20k
    /// steps): a baseline in-memory queue, a memory-storage queue, and a
    /// binlog-backed queue must stay identical across pushes, reads, GCs and
    /// restarts (memory replay + binlog replay).
    #[test]
    fn tqueue_random_differential() {
        struct TestQ {
            baseline: Box<dyn TQueue>,
            memory: Box<dyn TQueue>,
            binlog_q: Box<dyn TQueue>,
            binlog_path: String,
        }

        impl TestQ {
            fn new(binlog_path: &str) -> TestQ {
                let baseline = create();

                let mut memory = create();
                let memory_storage = TQueueMemoryStorage::new();
                memory.set_callback(Box::new(memory_storage));

                let mut binlog_q = create();
                let mut tqueue_binlog = TQueueBinlog::new();
                destroy(binlog_path).ensure();
                let mut binlog_handle = ConcurrentBinlog::new();
                binlog_handle
                    .init(binlog_path, |_| {}, DbKey::empty(), DbKey::empty())
                    .unwrap();
                let binlog = Arc::new(binlog_handle);
                tqueue_binlog.set_binlog(binlog.clone());
                binlog_q.set_callback(Box::new(tqueue_binlog));

                TestQ {
                    baseline,
                    memory,
                    binlog_q,
                    binlog_path: binlog_path.to_string(),
                }
            }

            fn restart(&mut self, rnd: &mut Xorshift128plus, now: i32) {
                if rnd.fast(0, 10) == 0 {
                    self.baseline.run_gc(now);
                }

                // memory replay: extract the storage, replay into a new queue
                let memory_storage = match self.memory.extract_callback() {
                    Some(mut callback) => {
                        match callback.as_any().downcast_mut::<TQueueMemoryStorage>() {
                            Some(storage) => std::mem::take(storage),
                            None => panic!("memory callback is not a TQueueMemoryStorage"),
                        }
                    }
                    None => panic!("memory queue has no callback"),
                };
                self.memory = create();
                memory_storage.replay(self.memory.as_mut());
                self.memory.set_callback(Box::new(memory_storage));
                if rnd.fast(0, 10) == 0 {
                    self.memory.run_gc(now);
                }

                if rnd.fast(0, 30) != 0 {
                    return;
                }
                // binlog restart: full replay from disk
                self.binlog_q = create();
                let mut tqueue_binlog = TQueueBinlog::new();
                let mut binlog_handle = ConcurrentBinlog::new();
                {
                    let tqueue_binlog_ref = &tqueue_binlog;
                    let q_ref = &mut *self.binlog_q;
                    binlog_handle
                        .init(
                            &self.binlog_path,
                            |event| {
                                let _ = tqueue_binlog_ref.replay(event, q_ref);
                            },
                            DbKey::empty(),
                            DbKey::empty(),
                        )
                        .unwrap();
                }
                let binlog = Arc::new(binlog_handle);
                tqueue_binlog.set_binlog(binlog);
                self.binlog_q.set_callback(Box::new(tqueue_binlog));
                if rnd.fast(0, 2) == 0 {
                    self.binlog_q.run_gc(now);
                }
            }

            fn push(
                &mut self,
                queue_id: QueueId,
                data: &[u8],
                expires_at: i32,
                new_id: EventId,
            ) -> EventId {
                let a_id = self
                    .baseline
                    .push(queue_id, data.to_vec(), expires_at, 0, new_id)
                    .unwrap();
                let b_id = self
                    .memory
                    .push(queue_id, data.to_vec(), expires_at, 0, new_id)
                    .unwrap();
                let c_id = self
                    .binlog_q
                    .push(queue_id, data.to_vec(), expires_at, 0, new_id)
                    .unwrap();
                assert_eq!(a_id, b_id);
                assert_eq!(a_id, c_id);
                a_id
            }

            fn check_get(&mut self, qid: QueueId, rnd: &mut Xorshift128plus, now: i32) {
                let mut a = vec![Event::default(); 10];
                let mut b = vec![Event::default(); 10];
                let mut c = vec![Event::default(); 10];

                let mut a_from = self.baseline.get_head(qid);
                if let Result::Ok(advanced) =
                    a_from.advance(rnd.fast(0, 20).saturating_sub(10) as usize)
                {
                    a_from = advanced;
                }
                self.baseline.get(qid, a_from, true, now, &mut a).unwrap();
                self.memory.get(qid, a_from, true, now, &mut b).unwrap();
                self.binlog_q.get(qid, a_from, true, now, &mut c).unwrap();
                assert_eq!(a.len(), b.len());
                assert_eq!(a.len(), c.len());
                for i in 0..a.len() {
                    assert_eq!(a[i].id, b[i].id);
                    assert_eq!(a[i].id, c[i].id);
                    assert_eq!(a[i].data, b[i].data);
                    assert_eq!(a[i].data, c[i].data);
                }
            }
        }

        impl Drop for TestQ {
            fn drop(&mut self) {
                let _ = destroy(&self.binlog_path);
            }
        }

        let mut rnd = Xorshift128plus::from_seed(123);
        let mut q = TestQ::new(&format!(
            "{}/tests/data/test_tqueue_random",
            env!("CARGO_MANIFEST_DIR")
        ));
        let mut now = 1000i32;
        for step in 0..20000u32 {
            let pick = rnd.fast(0, 155);
            if pick < 100 {
                let queue_id = rnd.fast(1, 10) as QueueId;
                let data = if rnd.fast(0, 10000) == 0 {
                    vec![0u8; 1 << 19]
                } else {
                    rnd.next_u64().to_string().into_bytes()
                };
                let first_id = if rnd.fast(0, 3) == 0 {
                    EventId::from_int32(EventId::MAX_ID - 20).unwrap()
                } else {
                    EventId::from_int32(rnd.fast(1_000_000_000, 1_500_000_000)).unwrap()
                };
                let expires_at = now + rnd.fast(-10, 10) * 10 + 5;
                q.push(queue_id, &data, expires_at, first_id);
            } else if pick < 110 {
                let tail = q.baseline.get_tail(rnd.fast(1, 10) as QueueId);
                let _ = tail;
            } else if pick < 150 {
                q.check_get(rnd.fast(1, 10) as QueueId, &mut rnd, now);
            } else if pick < 155 {
                now += 10;
            } else {
                q.restart(&mut rnd, now);
            }
            let _ = step;
        }
    }
}
