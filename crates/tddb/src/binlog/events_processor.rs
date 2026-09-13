//! Port of `td/db/binlog/detail/BinlogEventsProcessor`: replays the event
//! stream into the live set — applies `Rewrite` in place, `Empty` deletes,
//! skips negative service types, and compacts deleted entries.

use super::event::BinlogEvent;
use tdutils::status::Status;

/// `td::detail::BinlogEventsProcessor`
#[derive(Default)]
pub struct BinlogEventsProcessor {
    // holds (event_id * 2 + was_deleted)
    event_ids: Vec<u64>,
    events: Vec<Option<BinlogEvent>>,
    total_events: usize,
    empty_events: usize,
    last_event_id: u64,
    offset: i64,
    total_raw_events_size: i64,
}

impl BinlogEventsProcessor {
    pub fn new() -> BinlogEventsProcessor {
        BinlogEventsProcessor::default()
    }

    /// `BinlogEventsProcessor::add_event`
    pub fn add_event(&mut self, event: BinlogEvent) -> Status {
        self.do_event(event)
    }

    /// `BinlogEventsProcessor::for_each` — visit live events in id order.
    pub fn for_each(&self, mut callback: impl FnMut(&BinlogEvent)) {
        for (i, event_id) in self.event_ids.iter().enumerate() {
            if event_id & 1 == 0 {
                callback(self.events[i].as_ref().expect("live event missing"));
            }
        }
    }

    /// `BinlogEventsProcessor::last_event_id`
    pub fn last_event_id(&self) -> u64 {
        self.last_event_id
    }

    /// `BinlogEventsProcessor::offset`
    pub fn offset(&self) -> i64 {
        self.offset
    }

    /// `BinlogEventsProcessor::total_raw_events_size`
    pub fn total_raw_events_size(&self) -> i64 {
        self.total_raw_events_size
    }

    /// `BinlogEventsProcessor::do_event`
    fn do_event(&mut self, mut event: BinlogEvent) -> Status {
        self.offset = event.offset;
        let fixed_event_id = event.id * 2;
        if event.flags & BinlogEvent::FLAG_REWRITE != 0
            && !self.event_ids.is_empty()
            && *self.event_ids.last().unwrap() >= fixed_event_id
        {
            let it = self
                .event_ids
                .iter()
                .enumerate()
                .find(|(_, id)| **id >= fixed_event_id);
            let Some((pos, found_id)) = it else {
                return Status::error_message(&format!(
                    "Ignore rewrite log event LogEvent[id = {:x}, type = {}, flags = {}, data = {} bytes]",
                    event.id,
                    event.type_,
                    event.flags,
                    event.get_data().len()
                ));
            };
            if *found_id != fixed_event_id {
                return Status::error_message(&format!(
                    "Ignore rewrite log event LogEvent[id = {:x}, type = {}, flags = {}, data = {} bytes]",
                    event.id,
                    event.type_,
                    event.flags,
                    event.get_data().len()
                ));
            }
            self.total_raw_events_size -=
                self.events[pos].as_ref().map_or(0, |e| e.raw_size()) as i64;
            if event.type_ == BinlogEvent::TYPE_EMPTY {
                self.event_ids[pos] += 1;
                self.empty_events += 1;
                self.events[pos] = None;
            } else {
                event.flags &= !BinlogEvent::FLAG_REWRITE;
                self.total_raw_events_size += event.raw_size() as i64;
                self.events[pos] = Some(event);
            }
        } else if event.type_ < 0 {
            // just skip service events
        } else {
            if !(self.event_ids.is_empty() || *self.event_ids.last().unwrap() < fixed_event_id) {
                return Status::error_message(&format!(
                    "{} {} {} {} LogEvent[id = {:x}, type = {}, flags = {}, data = {} bytes] {} {}",
                    self.offset,
                    self.event_ids.len(),
                    self.event_ids.last().unwrap(),
                    fixed_event_id,
                    event.id,
                    event.type_,
                    event.flags,
                    event.get_data().len(),
                    self.total_events,
                    self.total_raw_events_size
                ));
            }
            self.last_event_id = event.id;
            self.total_raw_events_size += event.raw_size() as i64;
            self.total_events += 1;
            self.event_ids.push(fixed_event_id);
            self.events.push(Some(event));
        }

        if self.total_events > 10 && self.empty_events * 4 > self.total_events * 3 {
            self.compactify();
        }
        Status::ok()
    }

    /// `BinlogEventsProcessor::compactify`
    fn compactify(&mut self) {
        assert_eq!(self.event_ids.len(), self.events.len());
        let mut kept_ids = Vec::with_capacity(self.event_ids.len());
        let mut kept_events = Vec::with_capacity(self.events.len());
        for (id, event) in self.event_ids.drain(..).zip(self.events.drain(..)) {
            if id & 1 == 0 {
                kept_ids.push(id);
                kept_events.push(event);
            }
        }
        self.event_ids = kept_ids;
        self.events = kept_events;
        self.total_events = self.event_ids.len();
        self.empty_events = 0;
        assert_eq!(self.event_ids.len(), self.events.len());
    }
}
