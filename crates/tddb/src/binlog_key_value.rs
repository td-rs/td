//! Port of `td/db/BinlogKeyValue.h` (the `BinlogKeyValue<ConcurrentBinlog>`
//! shape): a key-value store persisted through the binlog, replayed on init.

use std::collections::HashMap;
use std::sync::Arc;

use crate::binlog::concurrent::ConcurrentBinlog;
use crate::binlog::event::BinlogEvent;
use crate::db_key::DbKey;
use tdutils::status::Result;
use tdutils::status::Status;
use tdutils::try_result;

/// `BinlogKeyValue::MAGIC`
pub const MAGIC: i32 = 0x2a28_0000;

/// TL string format used by `td::store(string)`/`td::parse(string)`:
/// `[length: i32][bytes][zero padding to 4-byte boundary]`.
pub(crate) fn store_tl_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as i32).to_le_bytes());
    out.extend_from_slice(s);
    let pad = (4 - s.len() % 4) % 4;
    out.extend(std::iter::repeat_n(0u8, pad));
}

pub(crate) fn parse_tl_string(parser: &mut &[u8]) -> Result<Vec<u8>> {
    if parser.len() < 4 {
        return Result::from_status(Status::error_message("Failed to parse string length"));
    }
    let len = i32::from_le_bytes(parser[0..4].try_into().unwrap());
    if len < 0 {
        return Result::from_status(Status::error_message("Negative string length"));
    }
    let len = len as usize;
    let padded = (len + 3) & !3;
    if parser.len() < 4 + padded {
        return Result::from_status(Status::error_message("String is too short"));
    }
    let bytes = parser[4..4 + len].to_vec();
    *parser = &parser[4 + padded..];
    Result::Ok(bytes)
}

/// `td::BinlogKeyValue<ConcurrentBinlog>`
pub struct BinlogKeyValue {
    binlog: Option<Arc<ConcurrentBinlog>>,
    // key -> (value, event_id)
    map: HashMap<String, (String, u64)>,
    magic: i32,
}

impl Default for BinlogKeyValue {
    fn default() -> Self {
        Self::new()
    }
}

/// Serialize `BinlogKeyValue::Event{key, value}` exactly like the C++ Storer.
fn event_to_bytes(key: &str, value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    store_tl_string(&mut out, key.as_bytes());
    store_tl_string(&mut out, value.as_bytes());
    out
}

impl BinlogKeyValue {
    pub fn new() -> BinlogKeyValue {
        BinlogKeyValue {
            binlog: None,
            map: HashMap::new(),
            magic: MAGIC,
        }
    }

    pub fn get_magic(&self) -> i32 {
        self.magic
    }

    fn parse_event(data: &[u8]) -> Result<(String, String)> {
        let mut parser = data;
        try_result!(key, parse_tl_string(&mut parser));
        try_result!(value, parse_tl_string(&mut parser));
        if !parser.is_empty() {
            return Result::from_status(Status::error_message("Extra data in event"));
        }
        Result::Ok((
            String::from_utf8_lossy(&key).into_owned(),
            String::from_utf8_lossy(&value).into_owned(),
        ))
    }

    /// `BinlogKeyValue::init`
    pub fn init(&mut self, name: &str, db_key: DbKey) -> Status {
        self.close();
        let mut concurrent = ConcurrentBinlog::new();
        {
            let map = &mut self.map;
            let callback =
                |binlog_event: &BinlogEvent| match Self::parse_event(binlog_event.get_data()) {
                    Result::Ok((key, value)) => {
                        if key.is_empty() {
                            tracing::error!("Have event with empty key");
                            return;
                        }
                        map.insert(key, (value, binlog_event.id));
                    }
                    Result::Err(status) => {
                        panic!("Failed to parse BinlogKeyValue event: {status}")
                    }
                };
            match concurrent.init(name, callback, db_key, DbKey::empty()) {
                Result::Ok(_) => {}
                Result::Err(err_status) => return err_status,
            }
        }
        self.binlog = Some(Arc::new(concurrent));
        Status::ok()
    }

    /// `BinlogKeyValue::close`
    pub fn close(&mut self) {
        self.map.clear();
        self.binlog = None;
    }

    /// `BinlogKeyValue::set` — returns the sequence number, or 0 if the value
    /// is unchanged.
    pub fn set(&mut self, key: &str, value: &str) -> u64 {
        assert!(!key.is_empty());
        let Some(binlog) = self.binlog.clone() else {
            panic!("BinlogKeyValue is not initialized");
        };
        let entry = self
            .map
            .entry(key.to_string())
            .or_insert_with(|| (String::new(), 0));
        if entry.1 != 0 && entry.0 == value {
            return 0;
        }
        let seq_no = binlog.next_event_id();
        let rewrite = entry.1 != 0;
        let event_id = if rewrite { entry.1 } else { seq_no };
        entry.0 = value.to_string();
        entry.1 = event_id;

        binlog.add_raw_event(
            seq_no,
            BinlogEvent::create_raw(
                event_id,
                self.magic,
                if rewrite {
                    BinlogEvent::FLAG_REWRITE
                } else {
                    0
                },
                &event_to_bytes(key, value),
            ),
        );
        seq_no
    }

    /// `BinlogKeyValue::erase`
    pub fn erase(&mut self, key: &str) -> u64 {
        let Some(binlog) = self.binlog.clone() else {
            panic!("BinlogKeyValue is not initialized");
        };
        let Some((_, event_id)) = self.map.remove(key) else {
            return 0;
        };
        let seq_no = binlog.next_event_id();
        binlog.add_raw_event(
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

    /// `BinlogKeyValue::isset`
    pub fn isset(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    /// `BinlogKeyValue::get` — empty string when the key is missing.
    pub fn get(&self, key: &str) -> String {
        self.map
            .get(key)
            .map(|(v, _)| v.clone())
            .unwrap_or_default()
    }

    /// `BinlogKeyValue::for_each`
    pub fn for_each(&self, mut func: impl FnMut(&str, &str)) {
        for (key, (value, _)) in &self.map {
            func(key, value);
        }
    }

    /// `BinlogKeyValue::get_all`
    pub fn get_all(&self) -> HashMap<String, String> {
        self.map
            .iter()
            .map(|(k, (v, _))| (k.clone(), v.clone()))
            .collect()
    }

    /// `BinlogKeyValue::force_sync`
    pub fn force_sync(&self, promise: Option<crate::binlog::concurrent::Promise>) {
        if let Some(binlog) = &self.binlog {
            binlog.force_sync(promise);
        }
    }

    /// `BinlogKeyValue::destroy`
    pub fn destroy(name: &str) -> Status {
        crate::binlog::destroy(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binlog::concurrent::ConcurrentBinlog;
    use std::io::Write;

    fn db_path(name: &str) -> String {
        format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn tl_string_layout_matches_cxx() {
        let mut out = Vec::new();
        store_tl_string(&mut out, b"ab");
        // [len:4][bytes:2][padding:2]
        assert_eq!(out, vec![2, 0, 0, 0, b'a', b'b', 0, 0]);

        let mut out = Vec::new();
        store_tl_string(&mut out, b"abcd");
        assert_eq!(out, vec![4, 0, 0, 0, b'a', b'b', b'c', b'd']);

        let mut parser = &out[..];
        let s = parse_tl_string(&mut parser).unwrap();
        assert_eq!(s, b"abcd");
        assert!(parser.is_empty());
    }

    #[test]
    fn binlog_key_value_round_trip() {
        let path = db_path("test_binlog_kv");
        let _ = BinlogKeyValue::destroy(&path);

        let mut kv = BinlogKeyValue::new();
        kv.init(&path, DbKey::empty()).ensure();
        assert_eq!(kv.set("a", "1"), 1);
        assert_eq!(kv.set("b", "2"), 2);
        assert_eq!(kv.get("a"), "1");
        // same value -> no-op
        assert_eq!(kv.set("a", "1"), 0);
        // changed value -> rewrite of the same event id
        assert_eq!(kv.set("a", "11"), 3);
        assert_eq!(kv.erase("b"), 4);
        assert_eq!(kv.erase("missing"), 0);
        assert!(!kv.isset("b"));
        kv.force_sync(None);
        kv.close();

        let mut kv = BinlogKeyValue::new();
        kv.init(&path, DbKey::empty()).ensure();
        assert_eq!(kv.get("a"), "11");
        assert!(!kv.isset("b"));
        kv.close();
        let _ = BinlogKeyValue::destroy(&path);
    }

    /// S41 verify: a "crash" (no close, torn tail) recovers on restart.
    #[test]
    fn binlog_key_value_crash_recovery() {
        let path = db_path("test_binlog_kv_crash");
        let _ = BinlogKeyValue::destroy(&path);

        {
            let mut kv = BinlogKeyValue::new();
            kv.init(&path, DbKey::empty()).ensure();
            kv.set("hello", "world");
            kv.set("cucu", "mber");
            // simulate a crash: torn tail + no close()
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(&[0xfa, 0x00, 0x00, 0x00, 0x01]).unwrap();
        }

        let mut kv = BinlogKeyValue::new();
        kv.init(&path, DbKey::empty()).ensure();
        assert_eq!(kv.get("hello"), "world");
        assert_eq!(kv.get("cucu"), "mber");

        // the binlog remains writable and the torn tail is gone
        kv.set("after", "restart");
        assert_eq!(kv.get("after"), "restart");
        kv.close();

        let mut kv = BinlogKeyValue::new();
        kv.init(&path, DbKey::empty()).ensure();
        assert_eq!(kv.get("hello"), "world");
        assert_eq!(kv.get("after"), "restart");
        kv.close();
        let _ = BinlogKeyValue::destroy(&path);
    }

    /// S41 verify: concurrent handles can append from multiple threads with
    /// unique event ids, and the file replays cleanly afterwards.
    #[test]
    fn concurrent_binlog_multithread() {
        use std::sync::Arc;

        let path = db_path("test_binlog_concurrent");
        let _ = BinlogKeyValue::destroy(&path);

        let mut binlog = ConcurrentBinlog::new();
        binlog
            .init(&path, |_| {}, DbKey::empty(), DbKey::empty())
            .unwrap();
        let binlog = Arc::new(binlog);

        let handles: Vec<_> = (0..4)
            .map(|t| {
                let binlog = binlog.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        // payloads must keep the event size 4-aligned
                        let mut data = format!("data-{t}-{i}").into_bytes();
                        while !data.len().is_multiple_of(4) {
                            data.push(b'.');
                        }
                        binlog.add(100 + t, &data);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        binlog.close(None);

        let mut binlog = ConcurrentBinlog::new();
        let mut replay = Vec::new();
        binlog
            .init(
                &path,
                |e| replay.push((e.id, e.type_, e.get_data().to_vec())),
                DbKey::empty(),
                DbKey::empty(),
            )
            .unwrap();
        assert_eq!(replay.len(), 100);
        let mut ids: Vec<u64> = replay.iter().map(|(id, _, _)| *id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 100, "event ids must be unique");
        binlog.close(None);
        let _ = BinlogKeyValue::destroy(&path);
    }
}
