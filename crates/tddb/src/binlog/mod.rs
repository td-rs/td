//! Port of `td/db/binlog`: the tdlib write-ahead log. Record format is
//! byte-identical with the C++ implementation — the same layout
//! `[size: i32][id: i64][type: i32][flags: i32][extra: i64][data][crc32: i32]`,
//! little-endian, with a CRC32 (IEEE) over everything before the tail.

use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::path::Path;

use crate::db_key::DbKey;
use tdutils::status::Result;
use tdutils::status::Status;

pub mod concurrent;
pub mod event;
pub mod events_processor;
pub mod reader;

pub use event::BinlogEvent;
pub use events_processor::BinlogEventsProcessor;
pub use reader::BinlogReader;

/// `Binlog::BinlogInfo`
#[derive(Clone, Copy, Debug, Default)]
pub struct BinlogInfo {
    pub was_created: bool,
    pub last_event_id: u64,
    pub is_opened: bool,
}

/// `td::Binlog` — the unencrypted subset: create/open, load and replay,
/// append, flush/sync, close, destroy. Reindex, `change_key` and AesCtr
/// encryption arrive with the ConcurrentBinlog / TdDb steps.
pub struct Binlog {
    file: Option<File>,
    path: String,
    info: BinlogInfo,
    processor: BinlogEventsProcessor,
    last_event_id: u64,
    fd_size: i64,
    fd_events: u64,
    db_key: DbKey,
    db_key_used: bool,
    state: State,
    pending_events: Vec<BinlogEvent>,
    need_sync: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Empty,
    Load,
    Reindex,
    Run,
}

impl Default for Binlog {
    fn default() -> Self {
        Self::new()
    }
}

/// `Binlog::destroy`
pub fn destroy(path: &str) -> Status {
    // delete regenerated version first to avoid it becoming main version
    let _ = std::fs::remove_file(format!("{path}.new"));
    let _ = std::fs::remove_file(path);
    Status::ok()
}

impl Binlog {
    pub fn new() -> Binlog {
        Binlog {
            file: None,
            path: String::new(),
            info: BinlogInfo::default(),
            processor: BinlogEventsProcessor::new(),
            last_event_id: 0,
            fd_size: 0,
            fd_events: 0,
            db_key: DbKey::empty(),
            db_key_used: false,
            state: State::Empty,
            pending_events: Vec::new(),
            need_sync: false,
        }
    }

    pub fn empty(&self) -> bool {
        self.file.is_none()
    }

    pub fn get_path(&self) -> &str {
        &self.path
    }

    pub fn get_info(&self) -> BinlogInfo {
        self.info
    }

    /// `Binlog::next_event_id()`
    pub fn next_event_id(&mut self) -> u64 {
        self.last_event_id += 1;
        self.last_event_id
    }

    /// `Binlog::next_event_id(shift)`
    pub fn next_event_id_shift(&mut self, shift: i32) -> u64 {
        let res = self.last_event_id + 1;
        self.last_event_id += shift as u64;
        res
    }

    /// `Binlog::peek_next_event_id()`
    pub fn peek_next_event_id(&self) -> u64 {
        self.last_event_id + 1
    }

    /// `Binlog::init`
    pub fn init(
        &mut self,
        path: &str,
        callback: &mut dyn FnMut(&BinlogEvent),
        db_key: DbKey,
        old_db_key: DbKey,
    ) -> Status {
        let _ = self.close(false);
        let _ = old_db_key;

        self.db_key = db_key;
        self.db_key_used = false;
        self.processor = BinlogEventsProcessor::new();

        // try to restore binlog from regenerated version
        if Path::new(path).metadata().is_err() {
            let _ = std::fs::rename(format!("{path}.new"), path);
        }

        self.info = BinlogInfo::default();
        self.info.was_created = Path::new(path).metadata().is_err();

        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
        {
            Ok(file) => file,
            Err(err) => {
                return Status::error_message(&format!("Failed to open binlog \"{path}\": {err}"));
            }
        };
        if let Err(err) = lock_file(&file) {
            return Status::error_message(&format!("Failed to lock binlog \"{path}\": {err}"));
        }
        self.file = Some(file);
        self.fd_size = 0;
        self.path = path.to_string();

        let status = self.load_binlog(callback);
        if status.is_error() {
            let _ = self.close(false);
            return status;
        }
        self.info.last_event_id = self.processor.last_event_id();
        self.last_event_id = self.processor.last_event_id();

        // Unlike C++, this subset never writes an encryption event; a keyed
        // open of an unencrypted binlog is reported instead of silently
        // rekeying (reindex/change_key arrive with S41).
        if !self.db_key.is_empty() && !self.db_key_used {
            return Status::error_message("Wrong password");
        }

        self.info.is_opened = true;
        Status::ok()
    }

    /// `Binlog::add`
    pub fn add(&mut self, type_: i32, data: &[u8]) -> u64 {
        let event_id = self.next_event_id();
        self.add_raw_event(BinlogEvent::create_raw(event_id, type_, 0, data));
        event_id
    }

    /// `Binlog::rewrite`
    pub fn rewrite(&mut self, event_id: u64, type_: i32, data: &[u8]) -> u64 {
        let seq_no = self.next_event_id();
        self.add_raw_event(BinlogEvent::create_raw(
            event_id,
            type_,
            BinlogEvent::FLAG_REWRITE,
            data,
        ));
        seq_no
    }

    /// `Binlog::erase`
    pub fn erase(&mut self, event_id: u64) -> u64 {
        let seq_no = self.next_event_id();
        self.add_raw_event(BinlogEvent::create_raw(
            event_id,
            BinlogEvent::TYPE_EMPTY,
            BinlogEvent::FLAG_REWRITE,
            &[],
        ));
        seq_no
    }

    /// `Binlog::add_raw_event` / `Binlog::add_event`
    pub fn add_raw_event(&mut self, raw_event: Vec<u8>) {
        let event = match BinlogEvent::init(raw_event) {
            Result::Ok(event) => event,
            Result::Err(status) => panic!("Trying to add bad binlog event: {status}"),
        };
        self.add_event(event);
    }

    /// `Binlog::add_event`
    pub fn add_event(&mut self, event: BinlogEvent) {
        if !event.size.is_multiple_of(4) {
            panic!("Trying to add event with bad size {}", event.size);
        }
        self.do_add_event(event);
        self.lazy_flush();

        if self.state == State::Run {
            let fd_size = self.fd_size;
            let need_reindex = |min_size: i64, rate: i64| {
                fd_size > min_size && fd_size / rate > self.processor.total_raw_events_size()
            };
            if need_reindex(50000, 5)
                || need_reindex(100000, 4)
                || need_reindex(300000, 3)
                || need_reindex(500000, 2)
            {
                self.do_reindex();
            }
        }
    }

    /// `Binlog::do_add_event`
    fn do_add_event(&mut self, event: BinlogEvent) {
        if event.flags & BinlogEvent::FLAG_PARTIAL != 0 {
            let mut event = event;
            event.flags &= !BinlogEvent::FLAG_PARTIAL;
            self.pending_events.push(event);
        } else {
            let pending = std::mem::take(&mut self.pending_events);
            for pending_event in pending {
                self.do_event(pending_event);
            }
            self.do_event(event);
        }
    }

    /// `Binlog::do_event` (the subset without AesCtrEncryption handling)
    fn do_event(&mut self, event: BinlogEvent) {
        let event_size = event.raw_size() as i64;

        if self.state == State::Run || self.state == State::Reindex {
            let validate_status = event.validate();
            if validate_status.is_error() {
                panic!("Failed to validate binlog event {validate_status}");
            }
            self.write_bytes(event.raw_event());
        }

        if event.type_ < 0 && event.type_ == BinlogEvent::TYPE_AES_CTR_ENCRYPTION {
            panic!("AesCtr-encrypted binlogs are not supported yet");
        }

        if self.state != State::Reindex {
            let status = self.processor.add_event(event);
            if status.is_error() {
                match self.state {
                    State::Load => {
                        let file = self.file.as_mut().expect("binlog is not open");
                        let _ = file.seek(std::io::SeekFrom::Start(self.fd_size as u64));
                        let _ = file.set_len(self.fd_size as u64);
                        self.need_sync = true;
                        let _ = status;
                        return;
                    }
                    _ => panic!(
                        "Failed to add binlog event in state {:?}: {status}",
                        self.state
                    ),
                }
            }
        }

        self.fd_events += 1;
        self.fd_size += event_size;
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        let file = self.file.as_mut().expect("binlog is not open");
        if let Err(err) = file.write_all(bytes) {
            panic!("Failed to write binlog: {err}");
        }
        self.need_sync = true;
    }

    /// `Binlog::sync`
    pub fn sync(&mut self) {
        self.flush();
        if self.need_sync {
            if let Some(Err(err)) = self.file.as_ref().map(|file| file.sync_all()) {
                panic!("Failed to sync binlog: {err}");
            }
            self.need_sync = false;
        }
    }

    /// `Binlog::flush`
    pub fn flush(&mut self) {
        if self.state == State::Load {
            return;
        }
        if let Some(Err(err)) = self.file.as_mut().map(|file| file.flush()) {
            panic!("Failed to flush binlog: {err}");
        }
    }

    /// `Binlog::lazy_flush` — this subset writes through on every event, so
    /// only the sync bookkeeping remains.
    fn lazy_flush(&mut self) {}

    /// `Binlog::do_reindex` — compactify: replay live events into `path.new`,
    /// then atomically replace the old file. Encryption events are not part
    /// of this subset yet (change_key lands with the encryption slice).
    fn do_reindex(&mut self) {
        self.flush();
        assert_eq!(self.state, State::Run);
        self.state = State::Reindex;

        let new_path = format!("{}.new", self.path);
        let new_file = match OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&new_path)
        {
            Ok(file) => file,
            Err(err) => {
                tracing::error!("Can't open new binlog for regenerate: {err}");
                self.state = State::Run;
                return;
            }
        };
        let old_file = std::mem::replace(self.file.as_mut().unwrap(), new_file);
        self.fd_size = 0;
        self.fd_events = 0;

        // NB: clone, as in C++ ("no move is actually happens")
        let mut live = Vec::new();
        self.processor.for_each(|event| live.push(event.clone()));
        for event in live {
            self.do_event(event);
        }
        self.flush();

        // finish reindex: unlink old, rename new
        drop(old_file);
        if let Err(err) = std::fs::remove_file(&self.path) {
            panic!("Failed to unlink old binlog: {err}");
        }
        if let Err(err) = std::fs::rename(&new_path, &self.path) {
            panic!("Failed to rename binlog: {err}");
        }
        let real_size = std::fs::metadata(&self.path)
            .map(|m| m.len() as i64)
            .unwrap_or(-1);
        assert_eq!(self.fd_size, real_size, "reindexed binlog size mismatch");

        self.state = State::Run;
    }

    /// `Binlog::close`
    pub fn close(&mut self, need_sync: bool) -> Status {
        let Some(_file) = self.file.take() else {
            return Status::ok();
        };
        if need_sync {
            self.sync();
        } else {
            self.flush();
        }
        // File drop closes the fd and releases the flock.
        self.path.clear();
        self.info.is_opened = false;
        self.need_sync = false;
        Status::ok()
    }

    /// `Binlog::close_and_destroy`
    pub fn close_and_destroy(&mut self) -> Status {
        let path = std::mem::take(&mut self.path);
        let close_status = self.close(false);
        let _ = destroy(&path);
        close_status
    }

    /// `Binlog::load_binlog`
    fn load_binlog(&mut self, callback: &mut dyn FnMut(&BinlogEvent)) -> Status {
        self.state = State::Load;

        let mut buffer = Vec::new();
        {
            let file = self.file.as_mut().expect("binlog is not open");
            if let Err(err) = file.seek(std::io::SeekFrom::Start(0)) {
                return Status::error_message(&format!("Failed to seek binlog: {err}"));
            }
            if let Err(err) = file.read_to_end(&mut buffer) {
                return Status::error_message(&format!("Failed to read binlog: {err}"));
            }
        }

        let mut reader = BinlogReader::new(&buffer);
        let mut loaded_offset = reader.offset();
        loop {
            match reader.read_next() {
                Result::Ok(Some(event)) => {
                    self.do_add_event(event);
                    loaded_offset = reader.offset();
                }
                Result::Ok(None) => break, // clean EOF or truncated tail
                Result::Err(status) => {
                    if status.code() == -2 {
                        // trailing garbage after a valid event prefix: truncate
                        let offset = reader.offset();
                        let file = self.file.as_mut().expect("binlog is not open");
                        let _ = file.seek(std::io::SeekFrom::Start(offset as u64));
                        let _ = file.set_len(offset as u64);
                        loaded_offset = offset;
                        break;
                    }
                    return status;
                }
            }
        }

        let offset = self.processor.offset();
        assert!(offset >= 0);
        self.processor.for_each(callback);

        let file = self.file.as_mut().expect("binlog is not open");
        let fd_size = match file.metadata() {
            Ok(meta) => meta.len() as i64,
            Err(err) => return Status::error_message(&format!("Failed to stat binlog: {err}")),
        };
        if offset != fd_size {
            let file = self.file.as_mut().expect("binlog is not open");
            let _ = file.seek(std::io::SeekFrom::Start(offset as u64));
            let _ = file.set_len(offset as u64);
            self.db_key_used = false; // force reindex (S41)
        }
        assert_eq!(self.fd_size, offset, "loaded size mismatch");
        let _ = loaded_offset;
        self.state = State::Run;
        Status::ok()
    }
}

/// `FileFd::lock(FileFd::LockFlags::Write)` — an exclusive advisory lock.
/// C++ parity: POSIX fcntl/F_SETLK vs Windows LockFileEx (FileFd.cpp:452-495);
/// the unix side uses flock (equivalent advisory semantics for our callers).
#[cfg(unix)]
fn lock_file(file: &File) -> std::result::Result<(), String> {
    use std::os::fd::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(format!("flock failed: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(windows)]
fn lock_file(file: &File) -> std::result::Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x0000_0001;
    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;
    #[repr(C)]
    #[allow(non_snake_case)]
    struct Overlapped {
        Internal: usize,
        InternalHigh: usize,
        Pointer: *mut core::ffi::c_void,
        hEvent: isize,
    }
    unsafe extern "system" {
        fn LockFileEx(
            hFile: isize,
            dwFlags: u32,
            dwReserved: u32,
            nNumberOfBytesToLockLow: u32,
            nNumberOfBytesToLockHigh: u32,
            lpOverlapped: *mut Overlapped,
        ) -> i32;
    }
    // The whole file (MAXDWORD/MAXDWORD), exclusive, non-blocking.
    let mut overlapped = Overlapped {
        Internal: 0,
        InternalHigh: 0,
        Pointer: core::ptr::null_mut(),
        hEvent: 0,
    };
    let rc = unsafe {
        LockFileEx(
            file.as_raw_handle() as isize,
            LOCKFILE_FAIL_IMMEDIATELY | LOCKFILE_EXCLUSIVE_LOCK,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if rc == 0 {
        return Err(format!(
            "LockFileEx failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::event::crc32;
    use super::*;

    fn db_path(name: &str) -> String {
        format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn event_round_trip_and_validation() {
        let data = b"AAAA";
        let raw = BinlogEvent::create_raw(7, 42, 0, data);
        let event = BinlogEvent::init(raw.clone()).unwrap();
        assert_eq!(event.size as usize, raw.len());
        assert_eq!(event.id, 7);
        assert_eq!(event.type_, 42);
        assert_eq!(event.flags, 0);
        assert_eq!(event.extra, 0);
        assert_eq!(event.get_data(), data);
        event.validate().ensure();
        assert_eq!(event.validate().code(), 0);

        // C++ layout: [size][id][type][flags][extra][data][crc32]
        assert_eq!(&raw[0..4], &(raw.len() as i32).to_le_bytes());
        assert_eq!(&raw[4..12], &7u64.to_le_bytes());
        assert_eq!(&raw[12..16], &42i32.to_le_bytes());
        assert_eq!(&raw[16..20], &0i32.to_le_bytes());
        assert_eq!(&raw[20..28], &0u64.to_le_bytes());
        assert_eq!(&raw[28..32], data);
        // CRC over everything before the tail
        let crc = crc32(&raw[..raw.len() - 4]);
        assert_eq!(&raw[raw.len() - 4..], &crc.to_le_bytes());

        // flip one data byte -> CRC mismatch
        let mut corrupted = raw.clone();
        let mid = corrupted.len() - 5;
        corrupted[mid] ^= 0xff;
        let event = BinlogEvent::init(corrupted).unwrap();
        assert!(event.validate().is_error());

        // truncated record -> structural error
        assert!(BinlogEvent::init(raw[..raw.len() - 1].to_vec()).is_error());
        assert!(BinlogEvent::init(vec![0u8; 10]).is_error());
    }

    #[test]
    fn processor_replay_semantics() {
        let mut p = BinlogEventsProcessor::new();
        p.add_event(BinlogEvent::init(BinlogEvent::create_raw(1, 10, 0, b"a")).unwrap())
            .ensure();
        p.add_event(BinlogEvent::init(BinlogEvent::create_raw(2, 20, 0, b"bb")).unwrap())
            .ensure();
        // service events are skipped
        p.add_event(
            BinlogEvent::init(BinlogEvent::create_raw(3, BinlogEvent::TYPE_HEADER, 0, b""))
                .unwrap(),
        )
        .ensure();

        // rewrite event 1
        p.add_event(
            BinlogEvent::init(BinlogEvent::create_raw(
                1,
                10,
                BinlogEvent::FLAG_REWRITE,
                b"aaa",
            ))
            .unwrap(),
        )
        .ensure();
        // erase event 2
        p.add_event(
            BinlogEvent::init(BinlogEvent::create_raw(
                2,
                BinlogEvent::TYPE_EMPTY,
                BinlogEvent::FLAG_REWRITE,
                &[],
            ))
            .unwrap(),
        )
        .ensure();

        let mut live = Vec::new();
        p.for_each(|e| live.push((e.id, e.type_.to_string(), e.get_data().to_vec())));
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].0, 1);
        assert_eq!(live[0].2, b"aaa");
        assert_eq!(p.last_event_id(), 2);
        // offsets are assigned by the reader; hand-built events stay at -1
        assert_eq!(p.offset(), -1);

        // rewrite with an id below the last one but not present is an error
        // (erase 1 first: its slot becomes deleted)
        p.add_event(
            BinlogEvent::init(BinlogEvent::create_raw(
                1,
                BinlogEvent::TYPE_EMPTY,
                BinlogEvent::FLAG_REWRITE,
                &[],
            ))
            .unwrap(),
        )
        .ensure();
        assert!(
            p.add_event(
                BinlogEvent::init(BinlogEvent::create_raw(
                    1,
                    10,
                    BinlogEvent::FLAG_REWRITE,
                    b"x"
                ))
                .unwrap()
            )
            .is_error()
        );
    }

    #[test]
    fn binlog_file_round_trip() {
        let path = db_path("test_binlog_round_trip");
        let _ = destroy(&path);

        let id_a;
        let id_b;
        {
            let mut binlog = Binlog::new();
            binlog
                .init(&path, &mut |_| {}, DbKey::empty(), DbKey::empty())
                .ensure();
            assert!(binlog.get_info().was_created);
            id_a = binlog.add(1, b"AAAA");
            id_b = binlog.add(2, &[b'x'; 1000]);
            binlog.sync();
            binlog.close(true).ensure();
        }

        {
            let mut binlog = Binlog::new();
            let mut replay = Vec::new();
            binlog
                .init(
                    &path,
                    &mut |e| replay.push((e.id, e.type_, e.get_data().to_vec())),
                    DbKey::empty(),
                    DbKey::empty(),
                )
                .ensure();
            assert_eq!(
                replay,
                vec![(id_a, 1, b"AAAA".to_vec()), (id_b, 2, vec![b'x'; 1000])]
            );
            assert_eq!(binlog.peek_next_event_id(), id_b + 1);

            // rewrite a, erase b, add c
            let seq = binlog.rewrite(id_a, 1, b"AAAA_REWRITE");
            let erase_seq = binlog.erase(id_b);
            let id_c = binlog.add(3, b"CCCC");
            assert_eq!((seq, erase_seq, id_c), (3, 4, 5));
            binlog.sync();
            binlog.close(true).ensure();

            // final replay
            let mut binlog = Binlog::new();
            let mut final_replay = Vec::new();
            binlog
                .init(
                    &path,
                    &mut |e| final_replay.push((e.id, e.type_, e.get_data().to_vec())),
                    DbKey::empty(),
                    DbKey::empty(),
                )
                .ensure();
            assert_eq!(final_replay.len(), 2);
            assert_eq!(final_replay[0], (id_a, 1, b"AAAA_REWRITE".to_vec()));
            assert_eq!(final_replay[1], (id_c, 3, b"CCCC".to_vec()));
            let _ = destroy(&path);
        }
    }

    #[test]
    fn binlog_truncated_tail_is_repaired() {
        let path = db_path("test_binlog_truncated");
        let _ = destroy(&path);
        {
            let mut binlog = Binlog::new();
            binlog
                .init(&path, &mut |_| {}, DbKey::empty(), DbKey::empty())
                .ensure();
            binlog.add(1, b"AAAA");
            binlog.add(2, b"BBBB");
            binlog.sync();
            binlog.close(true).ensure();
        }
        // simulate a crash mid-write: append a partial record
        let partial = BinlogEvent::create_raw(3, 3, 0, b"C");
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(&partial[..partial.len() - 3]).unwrap();
        }
        let size_before = std::fs::metadata(&path).unwrap().len();

        let mut binlog = Binlog::new();
        let mut replay = Vec::new();
        binlog
            .init(
                &path,
                &mut |e| replay.push((e.id, e.type_)),
                DbKey::empty(),
                DbKey::empty(),
            )
            .ensure();
        assert_eq!(replay, vec![(1, 1), (2, 2)]);
        let size_after = std::fs::metadata(&path).unwrap().len();
        assert!(
            size_after < size_before,
            "file must be truncated back to the last good event"
        );
        // new appends continue after the repaired offset
        let id = binlog.add(3, b"CCCC");
        assert_eq!(id, 3);
        binlog.sync();
        binlog.close(true).ensure();
        let _ = destroy(&path);
    }

    #[test]
    fn binlog_destroy_removes_regenerated_version() {
        let path = db_path("test_binlog_destroy");
        let _ = destroy(&path);
        std::fs::write(&path, b"x").unwrap();
        std::fs::write(format!("{path}.new"), b"y").unwrap();
        destroy(&path).ensure();
        assert!(!Path::new(&path).exists());
        assert!(!Path::new(&format!("{path}.new")).exists());
    }
}

#[cfg(test)]
mod cpp_conformance_tests {
    use super::*;

    /// S40 gate, direction 1: a binlog written by the C++ implementation
    /// (conformance/cpp/binlog_write_sample.cpp) parses and replays in Rust.
    /// Expected replay verified with conformance/cpp/binlog_read_sample:
    /// event 1 rewritten to "AAAA_REWRITE", event 2 erased, event 5 "CCCC".
    #[test]
    fn binlog_parse_cpp_sample() {
        let path = format!(
            "{}/tests/data/sample_cpp.binlog",
            env!("CARGO_MANIFEST_DIR")
        );
        let mut binlog = Binlog::new();
        let mut replay = Vec::new();
        binlog
            .init(
                &path,
                &mut |e| replay.push((e.id, e.type_, e.flags, e.get_data().to_vec())),
                DbKey::empty(),
                DbKey::empty(),
            )
            .ensure();
        assert_eq!(
            replay,
            vec![
                (1u64, 1i32, 0i32, b"AAAA_REWRITE".to_vec()),
                (5u64, 3i32, 0i32, b"CCCC".to_vec()),
            ]
        );
        assert_eq!(binlog.peek_next_event_id(), 6);
        binlog.close(false).ensure();
    }
}

#[cfg(test)]
mod reindex_tests {
    use super::*;

    fn db_path(name: &str) -> String {
        format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    /// Bloat-driven reindex (C++ add_event thresholds): one live event kept
    /// rewritten until the file compacts itself.
    #[test]
    fn binlog_reindex_compacts_on_bloat() {
        let path = db_path("test_binlog_reindex");
        let _ = destroy(&path);

        let mut binlog = Binlog::new();
        binlog
            .init(&path, &mut |_| {}, DbKey::empty(), DbKey::empty())
            .ensure();
        let id = binlog.add(1, &[b'x'; 1000]);
        let mut last = Vec::new();
        for i in 0..200usize {
            last = vec![b'y'; 1000];
            last[0] = (i % 251) as u8;
            binlog.rewrite(id, 1, &last);
        }
        binlog.sync();
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size < 10_000, "reindex must compact the file, got {size}");
        assert_eq!(binlog.peek_next_event_id(), id + 201);
        binlog.close(true).ensure();

        let mut binlog = Binlog::new();
        let mut replay = Vec::new();
        binlog
            .init(
                &path,
                &mut |e| replay.push((e.id, e.get_data().to_vec())),
                DbKey::empty(),
                DbKey::empty(),
            )
            .ensure();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].0, id);
        assert_eq!(replay[0].1, last);
        let _ = destroy(&path);
    }
}
