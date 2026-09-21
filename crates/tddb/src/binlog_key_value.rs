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

/// TL string format used by `td::store(string)`/`td::parse(string)`
/// (`TlStorerUnsafe::store_string`, verified against the C++ with a
/// compiler probe): `[1-byte len][bytes]` below 254, `0xfe`/`0xff` length
/// forms above, zero padding to a 4-byte boundary.
pub(crate) fn store_tl_string(out: &mut Vec<u8>, s: &[u8]) {
    let len = s.len();
    let header = if len < 254 {
        out.push(len as u8);
        1
    } else if len < (1 << 24) {
        out.push(0xfe);
        out.extend_from_slice(&(len as u32).to_le_bytes()[..3]);
        4
    } else if len < (1usize << 32) {
        out.push(0xff);
        out.extend_from_slice(&(len as u64).to_le_bytes()[..4]);
        out.extend_from_slice(&[0, 0, 0]);
        8
    } else {
        panic!("String size {len} is too big to be stored");
    };
    out.extend_from_slice(s);
    let pad = (4 - (header + len) % 4) % 4;
    out.extend(std::iter::repeat_n(0u8, pad));
}

/// `TlParser::fetch_string` (the clean raw reader — the pmc's binary keys
/// and values must survive replay byte-identically).
pub(crate) fn parse_tl_string(parser: &mut &[u8]) -> Result<Vec<u8>> {
    if parser.len() < 4 {
        return Result::from_status(Status::error_message("Failed to parse string length"));
    }
    let first = parser[0];
    let (result_len, header, aligned_len) = if first < 254 {
        let len = first as usize;
        (len, 4usize, (len >> 2) << 2)
    } else if first == 254 {
        if parser.len() < 4 {
            return Result::from_status(Status::error_message("Failed to parse string length"));
        }
        let len = (parser[1] as usize) | ((parser[2] as usize) << 8) | ((parser[3] as usize) << 16);
        (len, 4, ((len + 3) >> 2) << 2)
    } else {
        if parser.len() < 8 {
            return Result::from_status(Status::error_message("Failed to parse string length"));
        }
        let mut len = 0u64;
        for i in (1..8).rev() {
            len = (len << 8) | parser[i] as u64;
        }
        if len > (usize::MAX - 3) as u64 {
            return Result::from_status(Status::error_message("Too big string found"));
        }
        (len as usize, 8, ((len as usize + 3) >> 2) << 2)
    };
    if parser.len() < header + aligned_len {
        return Result::from_status(Status::error_message("String is too short"));
    }
    let begin = if first == 255 {
        8
    } else if first == 254 {
        4
    } else {
        1
    };
    let bytes = parser[begin..begin + result_len].to_vec();
    *parser = &parser[header + aligned_len..];
    Result::Ok(bytes)
}

/// `td::BinlogKeyValue<ConcurrentBinlog>`. Keys and values are byte strings
/// (the C++ std::string): the sqlite encryption key is arbitrary binary and
/// must survive replay untouched, so the &str accessors are UTF-8
/// conveniences over the raw [`Self::set_raw`]/[`Self::get_raw`] core.
pub struct BinlogKeyValue {
    binlog: Option<Arc<ConcurrentBinlog>>,
    // key -> (value, event_id)
    map: HashMap<Vec<u8>, (Vec<u8>, u64)>,
    magic: i32,
}

impl Default for BinlogKeyValue {
    fn default() -> Self {
        Self::new()
    }
}

/// Serialize `BinlogKeyValue::Event{key, value}` exactly like the C++ Storer.
fn event_to_bytes(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    store_tl_string(&mut out, key);
    store_tl_string(&mut out, value);
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

    /// `BinlogKeyValue::set_magic`.
    pub fn set_magic(&mut self, magic: i32) {
        self.magic = magic;
    }

    /// `BinlogKeyValue::external_init_begin` — prepare for a replay driven
    /// by an external owner (several pmcs may share one binlog file, each
    /// consuming only the events written with its own magic).
    pub fn external_init_begin(&mut self, magic: i32) {
        self.magic = magic;
        self.map.clear();
    }

    /// `BinlogKeyValue::external_init_handle` — consume one replayed event.
    pub fn external_init_handle(&mut self, event: &BinlogEvent) {
        match Self::parse_event(event.get_data()) {
            Result::Ok((key, value)) => {
                if key.is_empty() {
                    tracing::error!("Have event with empty key");
                    return;
                }
                self.map.insert(key, (value, event.id));
            }
            Result::Err(status) => {
                panic!("Failed to parse BinlogKeyValue event: {status}")
            }
        }
    }

    /// `BinlogKeyValue::external_init_finish` — attach to the shared binlog
    /// after the replay. The events were already routed by the owner's
    /// callback, so unlike `init` no second replay happens.
    pub fn external_init_finish(&mut self, binlog: Arc<ConcurrentBinlog>) {
        self.binlog = Some(binlog);
    }

    fn parse_event(data: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut parser = data;
        try_result!(key, parse_tl_string(&mut parser));
        try_result!(value, parse_tl_string(&mut parser));
        if !parser.is_empty() {
            return Result::from_status(Status::error_message("Extra data in event"));
        }
        Result::Ok((key, value))
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
        self.set_raw(key.as_bytes(), value.as_bytes())
    }

    /// The raw byte-string form of `set`.
    pub fn set_raw(&mut self, key: &[u8], value: &[u8]) -> u64 {
        assert!(!key.is_empty());
        let Some(binlog) = self.binlog.clone() else {
            panic!("BinlogKeyValue is not initialized");
        };
        let entry = self
            .map
            .entry(key.to_vec())
            .or_insert_with(|| (Vec::new(), 0));
        if entry.1 != 0 && entry.0 == value {
            return 0;
        }
        let seq_no = binlog.next_event_id();
        let rewrite = entry.1 != 0;
        let event_id = if rewrite { entry.1 } else { seq_no };
        entry.0 = value.to_vec();
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
        let Some((_, event_id)) = self.map.remove(key.as_bytes()) else {
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

    /// `BinlogKeyValue::erase_by_prefix` (BinlogKeyValue.h:241) — erases
    /// every key beginning with the prefix. The C++ batch-allocates the
    /// binlog sequence numbers (`next_event_id(count)`); sequential single
    /// allocations produce the same id sequence.
    pub fn erase_by_prefix(&mut self, prefix: &[u8]) {
        let Some(binlog) = self.binlog.clone() else {
            panic!("BinlogKeyValue is not initialized");
        };
        let keys: Vec<Vec<u8>> = self
            .map
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect();
        for key in keys {
            let Some((_, event_id)) = self.map.remove(&key) else {
                continue;
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
        }
    }

    /// `KeyValueInterface::prefix_get` — the keyspace enumeration the
    /// DownloadManager's dlds# init load uses.
    pub fn prefix_get(&self, prefix: &str) -> Vec<(String, Vec<u8>)> {
        let prefix = prefix.as_bytes();
        self.map
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, (value, _))| (String::from_utf8_lossy(key).into_owned(), value.clone()))
            .collect()
    }

    /// `BinlogKeyValue::isset`
    pub fn isset(&self, key: &str) -> bool {
        self.map.contains_key(key.as_bytes())
    }

    /// `BinlogKeyValue::get` — empty string when the key is missing.
    pub fn get(&self, key: &str) -> String {
        String::from_utf8_lossy(&self.get_raw(key.as_bytes())).into_owned()
    }

    /// The raw byte-string form of `get` (values may be non-UTF-8, e.g. the
    /// sqlite encryption key).
    pub fn get_raw(&self, key: &[u8]) -> Vec<u8> {
        self.map
            .get(key)
            .map(|(v, _)| v.clone())
            .unwrap_or_default()
    }

    /// `BinlogKeyValue::for_each`
    pub fn for_each(&self, mut func: impl FnMut(&str, &str)) {
        for (key, (value, _)) in &self.map {
            func(
                &String::from_utf8_lossy(key),
                &String::from_utf8_lossy(value),
            );
        }
    }

    /// `BinlogKeyValue::get_all`
    pub fn get_all(&self) -> HashMap<String, String> {
        self.map
            .iter()
            .map(|(k, (v, _))| {
                (
                    String::from_utf8_lossy(k).into_owned(),
                    String::from_utf8_lossy(v).into_owned(),
                )
            })
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
        // [len:1][bytes:2][padding:1] — the TL framing of
        // `TlStorerUnsafe::store_string` (verified against the compiled
        // C++ with a compiler probe).
        assert_eq!(out, vec![2, b'a', b'b', 0]);

        let mut out = Vec::new();
        store_tl_string(&mut out, b"abcd");
        assert_eq!(out, vec![4, b'a', b'b', b'c', b'd', 0, 0, 0]);

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
