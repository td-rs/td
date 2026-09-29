//! Port of `td/db/binlog`: the tdlib write-ahead log. Record format is
//! byte-identical with the C++ implementation — the same layout
//! `[size: i32][id: i64][type: i32][flags: i32][extra: i64][data][crc32: i32]`,
//! little-endian, with a CRC32 (IEEE) over everything before the tail.
//!
//! AesCtr-encrypted binlogs: the first event is a plaintext
//! `AesCtrEncryption` handshake (type -3, id 0) carrying
//! `[flags i32][key_salt][iv][key_hash]` as TL byte-strings; every byte
//! after it is AES-256-CTR keystream over the record stream. A keyed open
//! of an unencrypted binlog (fresh or not) reindexes it with a handshake,
//! like C++; a handshake that doesn't verify against the key is the
//! "Wrong password" error. `change_key` stays unported.

use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::path::Path;

use crate::db_key::DbKey;
use tdutils::crypto::AesCtrState;
use tdutils::random;
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
    pub wrong_password: bool,
}

/// `Binlog::Error::WrongPassword`
const WRONG_PASSWORD: i32 = -1037284;

/// `Binlog::EncryptionType`
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum EncryptionType {
    #[default]
    None,
    AesCtr,
}

/// `detail::AesCtrEncryptionEvent` (Binlog.cpp:31) — the binlog handshake
/// event. Stored as one TL record: `flags i32 = 0` then `key_salt`, `iv`,
/// `key_hash` as byte-strings (i32 LE length + payload + zero padding to a
/// multiple of 4).
struct AesCtrEncryptionEvent {
    key_salt: Vec<u8>,
    iv: Vec<u8>,
    key_hash: Vec<u8>,
}

impl AesCtrEncryptionEvent {
    /// `default_salt_size()` / `key_size()` / `iv_size()` / `hash_size()`
    /// — 32 / 32 / 16 / 32.
    const DEFAULT_SALT_SIZE: usize = 32;
    const KEY_SIZE: usize = 32;
    const IV_SIZE: usize = 16;
    const HASH_SIZE: usize = 32;

    /// `kdf_iteration_count()` — password keys; raw keys use the fast count.
    const KDF_ITERATION_COUNT: u32 = 60002;
    const KDF_FAST_ITERATION_COUNT: u32 = 2;

    /// The HMAC message of `generate_hash` (Binlog.cpp:69).
    const HASH_MESSAGE: &[u8] = b"cucumbers everywhere";

    /// `AesCtrEncryptionEvent::generate_key(db_key)`
    fn generate_key(&self, db_key: &DbKey) -> [u8; 32] {
        assert!(!db_key.is_empty());
        let iteration_count = if db_key.is_raw_key() {
            Self::KDF_FAST_ITERATION_COUNT
        } else {
            Self::KDF_ITERATION_COUNT
        };
        let mut key = [0u8; Self::KEY_SIZE];
        tdutils::crypto::pbkdf2_sha256(db_key.data(), &self.key_salt, iteration_count, &mut key);
        key
    }

    /// `AesCtrEncryptionEvent::generate_hash(key)`
    fn generate_hash(key: &[u8]) -> [u8; 32] {
        tdutils::crypto::hmac_sha256(key, Self::HASH_MESSAGE)
    }

    /// `AesCtrEncryptionEvent::store` — the TL payload for `create_raw`.
    /// Strings use the td TL encoding (TlStorerUnsafe::store_string): one
    /// length byte for < 254, payload, then zero padding to a 4-aligned
    /// total counting the length byte.
    fn store(&self) -> Vec<u8> {
        fn put_string(out: &mut Vec<u8>, s: &[u8]) {
            assert!(s.len() < 254, "TL string too long for the 1-byte form");
            let total = s.len() + 1;
            out.push(s.len() as u8);
            out.extend_from_slice(s);
            out.resize(out.len() + (4 - total % 4) % 4, 0);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&0i32.to_le_bytes()); // BEGIN/END_STORE_FLAGS
        debug_assert_eq!(self.key_salt.len(), Self::DEFAULT_SALT_SIZE);
        debug_assert_eq!(self.iv.len(), Self::IV_SIZE);
        debug_assert_eq!(self.key_hash.len(), Self::HASH_SIZE);
        put_string(&mut out, &self.key_salt);
        put_string(&mut out, &self.iv);
        put_string(&mut out, &self.key_hash);
        out
    }

    /// `AesCtrEncryptionEvent::parse` + `fetch_end` — `None` on any
    /// structural error (TlParser error in C++).
    fn parse(data: &[u8]) -> Option<AesCtrEncryptionEvent> {
        /// TlParser::fetch_string — the 1- and 4-byte length forms; padding
        /// zeroes are skipped without verification, like C++.
        fn get_string(data: &mut &[u8]) -> Option<Vec<u8>> {
            let first = *data.first()?;
            let (header, len) = if first < 254 {
                (1usize, first as usize)
            } else if first == 254 {
                if data.len() < 4 {
                    return None;
                }
                let len = data[1] as usize | (data[2] as usize) << 8 | (data[3] as usize) << 16;
                (4usize, len)
            } else {
                return None; // the 8-byte form — impossible for our sizes
            };
            let total = header + len;
            if data.len() < total {
                return None;
            }
            let s = data[header..total].to_vec();
            let pad = (4 - total % 4) % 4;
            if data.len() < total + pad {
                return None;
            }
            *data = &data[total + pad..];
            Some(s)
        }
        let mut rest = data;
        if rest.len() < 4 || rest[..4] != [0, 0, 0, 0] {
            return None; // flags
        }
        rest = &rest[4..];
        let key_salt = get_string(&mut rest)?;
        let iv = get_string(&mut rest)?;
        let key_hash = get_string(&mut rest)?;
        if !rest.is_empty() {
            return None; // fetch_end
        }
        Some(AesCtrEncryptionEvent {
            key_salt,
            iv,
            key_hash,
        })
    }
}

/// `td::Binlog` — create/open, load and replay, append, flush/sync, close,
/// destroy, reindex and AesCtr encryption. `change_key` stays unported.
pub struct Binlog {
    file: Option<File>,
    path: String,
    info: BinlogInfo,
    processor: BinlogEventsProcessor,
    last_event_id: u64,
    fd_size: i64,
    fd_events: u64,
    db_key: DbKey,
    old_db_key: DbKey,
    db_key_used: bool,
    encryption_type: EncryptionType,
    /// `aes_ctr_key_` — the working AES key (raw or derived).
    aes_ctr_key: [u8; 32],
    /// `aes_ctr_key_salt_` — non-empty once a handshake established a salt;
    /// a reindex with an empty salt mints a fresh one.
    aes_ctr_key_salt: Vec<u8>,
    /// The write-stream cipher — C++ attaches it to the buffered fd writer
    /// (`update_write_encryption`); here `write_bytes` runs it inline.
    write_ctr: Option<AesCtrState>,
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
            old_db_key: DbKey::empty(),
            db_key_used: false,
            encryption_type: EncryptionType::None,
            aes_ctr_key: [0u8; 32],
            aes_ctr_key_salt: Vec::new(),
            write_ctr: None,
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

        self.db_key = db_key;
        self.old_db_key = old_db_key;
        self.db_key_used = false;
        self.encryption_type = EncryptionType::None;
        self.aes_ctr_key_salt.clear();
        self.write_ctr = None;
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

        if self.info.wrong_password {
            let _ = self.close(false);
            return Status::error(WRONG_PASSWORD, "Wrong password");
        }

        // Keyed open of an unencrypted binlog, or keyless open of an
        // encrypted one: reindex with (respectively without) a handshake
        // (Binlog.cpp:221).
        if (!self.db_key.is_empty() && !self.db_key_used)
            || (self.db_key.is_empty() && self.encryption_type != EncryptionType::None)
        {
            self.aes_ctr_key_salt.clear();
            self.do_reindex();
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
            // Binlog.cpp:336 — the `[reindex] ` cond fires only for the
            // replayed writes during a reindex; the Run-state writes log
            // at their add sites (the pmc layer carries the caller's
            // BinlogDebugInfo, BinlogKeyValue.h:196).
            if self.state == State::Reindex {
                tdutils::vlog!(
                    tdutils::logging::verbosity::BINLOG,
                    "binlog",
                    at "Binlog.cpp",
                    336,
                    "Write binlog event: [reindex] LogEvent[[id:0x{:016x}][type:{}][flags:{}][data:{}]][{}]",
                    event.id,
                    event.type_,
                    event.flags,
                    event.raw_size(),
                    "/project/td/tddb/td/db/binlog/Binlog.cpp:613"
                );
            }
            self.write_bytes(event.raw_event());
        }

        if event.type_ == BinlogEvent::TYPE_AES_CTR_ENCRYPTION {
            let Some(hs) = AesCtrEncryptionEvent::parse(event.get_data()) else {
                // C++ LOG_CHECKs the TlParser status (Binlog.cpp:348-350).
                panic!("Failed to parse AesCtrEncryption event");
            };
            match self.state {
                State::Load => {
                    // Verified and applied by load_binlog, which owns the
                    // read buffer and decrypts the tail in place.
                }
                State::Reindex | State::Run => {
                    // The handshake was just written by reset_encryption;
                    // arm the write stream (Binlog.cpp:380-386).
                    let key = if self.aes_ctr_key_salt == hs.key_salt {
                        self.aes_ctr_key
                    } else {
                        hs.generate_key(&self.db_key)
                    };
                    self.update_encryption(&key, &hs.iv);
                    self.flush();
                }
                State::Empty => unreachable!(),
            }
            self.encryption_type = EncryptionType::AesCtr;
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
        let encrypted;
        let out: &[u8] = if let Some(ctr) = self.write_ctr.as_mut() {
            let mut buf = bytes.to_vec();
            ctr.encrypt_in_place(&mut buf);
            encrypted = buf;
            &encrypted
        } else {
            bytes
        };
        let file = self.file.as_mut().expect("binlog is not open");
        if let Err(err) = file.write_all(out) {
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

    /// `Binlog::update_encryption(key, iv)` — set the working key material
    /// and arm the write stream (C++ arms its fd reader/writer flows).
    fn update_encryption(&mut self, key: &[u8; 32], iv: &[u8]) {
        self.aes_ctr_key = *key;
        let mut state = AesCtrState::new();
        state.init(key, iv);
        self.write_ctr = Some(state);
    }

    /// `Binlog::reset_encryption` — mint (or reuse) the handshake event and
    /// write it; `do_event` arms the write stream from it.
    fn reset_encryption(&mut self) {
        if self.db_key.is_empty() {
            self.encryption_type = EncryptionType::None;
            return;
        }

        let mut event = AesCtrEncryptionEvent {
            key_salt: Vec::new(),
            iv: vec![0u8; AesCtrEncryptionEvent::IV_SIZE],
            key_hash: Vec::new(),
        };
        if self.aes_ctr_key_salt.is_empty() {
            event.key_salt = vec![0u8; AesCtrEncryptionEvent::DEFAULT_SALT_SIZE];
            random::secure_bytes(&mut event.key_salt);
        } else {
            event.key_salt = self.aes_ctr_key_salt.clone();
        }
        random::secure_bytes(&mut event.iv);

        let key = if self.aes_ctr_key_salt == event.key_salt {
            self.aes_ctr_key
        } else {
            event.generate_key(&self.db_key)
        };
        self.aes_ctr_key = key;
        self.aes_ctr_key_salt = event.key_salt.clone();
        event.key_hash = AesCtrEncryptionEvent::generate_hash(&key).to_vec();

        let raw =
            BinlogEvent::create_raw(0, BinlogEvent::TYPE_AES_CTR_ENCRYPTION, 0, &event.store());
        let event = match BinlogEvent::init(raw) {
            Result::Ok(event) => event,
            Result::Err(status) => panic!("Trying to add bad binlog event: {status}"),
        };
        self.do_event(event);
    }

    /// `Binlog::do_reindex` — compactify: replay live events into `path.new`,
    /// then atomically replace the old file. A reindex of an encrypted binlog
    /// rewrites the handshake first (`reset_encryption`) and the replayed
    /// events are re-encrypted; with an empty key the encryption is dropped.
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
        self.encryption_type = EncryptionType::None;
        self.write_ctr = None;
        self.reset_encryption();

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

        // The whole file is read up front, so the read-side decryption is
        // done in place: after a handshake verifies, the bytes past it are
        // the AES-256-CTR keystream over the remaining records. The very
        // state that decrypted the tail stays armed as the write stream —
        // appends continue the same keystream, like the C++ shared
        // `aes_ctr_state_` between its read and write flows.
        let mut pos = 0usize;
        loop {
            // The per-event reader is sliced at `pos`, so its offsets are
            // slice-relative; rebase to file-absolute like the single
            // persistent reader did.
            let (mut event, consumed) = {
                let mut reader = BinlogReader::new(&buffer[pos..]);
                match reader.read_next() {
                    Result::Ok(Some(event)) => (event, reader.offset() as usize),
                    Result::Ok(None) => break, // clean EOF or truncated tail
                    Result::Err(status) => {
                        if status.code() == -2 {
                            // trailing garbage after a valid event prefix: truncate
                            let offset = pos + reader.offset() as usize;
                            let file = self.file.as_mut().expect("binlog is not open");
                            let _ = file.seek(std::io::SeekFrom::Start(offset as u64));
                            let _ = file.set_len(offset as u64);
                            break;
                        }
                        // any other parse/validate error stops the load at
                        // the last good event (Binlog.cpp:537-538); the
                        // offset check below truncates the file
                        tracing::error!("{status}");
                        break;
                    }
                }
            };
            event.offset = (pos + consumed) as i64;

            if event.type_ == BinlogEvent::TYPE_AES_CTR_ENCRYPTION {
                let Some(hs) = AesCtrEncryptionEvent::parse(event.get_data()) else {
                    return Status::error_message("Failed to parse AesCtrEncryption event");
                };
                let mut key = [0u8; 32];
                let mut verified = false;
                if !self.db_key.is_empty() {
                    key = hs.generate_key(&self.db_key);
                    verified = AesCtrEncryptionEvent::generate_hash(&key).as_slice() == hs.key_hash;
                }
                if !verified && !self.old_db_key.is_empty() {
                    key = hs.generate_key(&self.old_db_key);
                    verified = AesCtrEncryptionEvent::generate_hash(&key).as_slice() == hs.key_hash;
                }
                if !verified {
                    self.info.wrong_password = true;
                    break;
                }
                self.db_key_used = true;
                self.encryption_type = EncryptionType::AesCtr;
                self.aes_ctr_key = key;
                self.aes_ctr_key_salt = hs.key_salt.clone();
                let mut state = AesCtrState::new();
                state.init(&key, &hs.iv);
                state.decrypt_in_place(&mut buffer[pos + consumed..]);
                self.write_ctr = Some(state);
            }

            self.do_add_event(event);
            pos += consumed;
        }

        if self.info.wrong_password {
            // leave the file untouched for a retry with the right key
            // (Binlog.cpp:531-533 returns early)
            return Status::ok();
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

    /// Encryption gate, direction 1: a binlog written by the C++
    /// implementation with `DbKey::password("WhatIDisThat")`
    /// (conformance/cpp/binlog_encrypted_sample.cpp write mode) decrypts
    /// and replays in Rust; the same flow as the unencrypted sample above.
    #[test]
    fn binlog_parse_cpp_encrypted_sample() {
        let path = format!(
            "{}/tests/data/sample_cpp_encrypted.binlog",
            env!("CARGO_MANIFEST_DIR")
        );
        let mut binlog = Binlog::new();
        let mut replay = Vec::new();
        binlog
            .init(
                &path,
                &mut |e| replay.push((e.id, e.type_, e.flags, e.get_data().to_vec())),
                DbKey::password("WhatIDisThat"),
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
        assert!(!binlog.get_info().wrong_password);
        binlog.close(false).ensure();

        // the wrong password is rejected and leaves the file intact
        let size_before = std::fs::metadata(&path).unwrap().len();
        let mut binlog = Binlog::new();
        let status = binlog.init(
            &path,
            &mut |_| {},
            DbKey::password("wrong password"),
            DbKey::empty(),
        );
        assert_eq!(
            (status.code(), status.message()),
            (WRONG_PASSWORD, "Wrong password")
        );
        assert_eq!(std::fs::metadata(&path).unwrap().len(), size_before);
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

#[cfg(test)]
mod encryption_tests {
    use super::*;

    fn db_path(name: &str) -> String {
        format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    /// The first raw record of the file: `(size, id, type)`.
    fn first_record_header(path: &str) -> (usize, u64, i32) {
        let data = std::fs::read(path).unwrap();
        assert!(data.len() >= 28);
        let size = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
        let id = u64::from_le_bytes(data[4..12].try_into().unwrap());
        let type_ = i32::from_le_bytes(data[12..16].try_into().unwrap());
        assert_eq!(size % 4, 0);
        (size, id, type_)
    }

    #[test]
    fn binlog_encrypted_round_trip_and_append_continuation() {
        let path = db_path("test_binlog_encrypted");
        let _ = destroy(&path);
        let key = DbKey::password("WhatIDisThat");

        let id_a;
        {
            let mut binlog = Binlog::new();
            binlog
                .init(&path, &mut |_| {}, key.clone(), DbKey::empty())
                .ensure();
            assert!(binlog.get_info().was_created);
            id_a = binlog.add(1, b"AAAA");
            binlog.sync();
            binlog.close(true).ensure();
        }

        // The fresh keyed open reindexed: the file now starts with the
        // AesCtrEncryption handshake (service id 0, type -3) and the events
        // after it are ciphertext — the plaintext record encoding of the
        // event appears nowhere in the file.
        let (hs_size, hs_id, hs_type) = first_record_header(&path);
        assert_eq!((hs_id, hs_type), (0, BinlogEvent::TYPE_AES_CTR_ENCRYPTION));
        let plaintext_a = BinlogEvent::create_raw(id_a, 1, 0, b"AAAA");
        let file = std::fs::read(&path).unwrap();
        assert!(
            !file.windows(plaintext_a.len()).any(|w| w == plaintext_a),
            "event record must not appear in plaintext"
        );
        // CTR preserves length: handshake + one encrypted record
        assert_eq!(file.len(), hs_size + plaintext_a.len());

        // Reopen with the right key, replay, and append — the appended
        // events must continue the keystream, not restart it.
        let id_b;
        {
            let mut binlog = Binlog::new();
            let mut replay = Vec::new();
            binlog
                .init(
                    &path,
                    &mut |e| replay.push((e.id, e.type_, e.get_data().to_vec())),
                    key.clone(),
                    DbKey::empty(),
                )
                .ensure();
            assert_eq!(replay, vec![(id_a, 1, b"AAAA".to_vec())]);
            assert_eq!(binlog.peek_next_event_id(), id_a + 1);
            id_b = binlog.add(2, &[b'y'; 2000]);
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
                    key.clone(),
                    DbKey::empty(),
                )
                .ensure();
            assert_eq!(
                replay,
                vec![(id_a, 1, b"AAAA".to_vec()), (id_b, 2, vec![b'y'; 2000])]
            );
            assert!(!binlog.get_info().wrong_password);
            binlog.close(false).ensure();
        }
        // the handshake is still the single first record
        assert_eq!(first_record_header(&path).1, 0);
        let _ = destroy(&path);
    }

    #[test]
    fn binlog_wrong_password() {
        let path = db_path("test_binlog_wrong_password");
        let _ = destroy(&path);
        {
            let mut binlog = Binlog::new();
            binlog
                .init(
                    &path,
                    &mut |_| {},
                    DbKey::password("correct horse battery staple"),
                    DbKey::empty(),
                )
                .ensure();
            binlog.add(1, b"AAAA");
            binlog.sync();
            binlog.close(true).ensure();
        }
        let size_before = std::fs::metadata(&path).unwrap().len();

        let mut binlog = Binlog::new();
        let status = binlog.init(
            &path,
            &mut |_| {},
            DbKey::password("wrong password"),
            DbKey::empty(),
        );
        assert!(status.is_error());
        assert_eq!(status.message(), "Wrong password");
        assert_eq!(status.code(), WRONG_PASSWORD);

        // the encrypted database is left untouched for a retry
        assert_eq!(std::fs::metadata(&path).unwrap().len(), size_before);
        let _ = destroy(&path);
    }

    #[test]
    fn binlog_keyed_open_of_plaintext_reindexes() {
        let path = db_path("test_binlog_rekey");
        let _ = destroy(&path);
        {
            let mut binlog = Binlog::new();
            binlog
                .init(&path, &mut |_| {}, DbKey::empty(), DbKey::empty())
                .ensure();
            binlog.add(1, b"AAAA");
            binlog.sync();
            binlog.close(true).ensure();
        }
        assert_ne!(
            first_record_header(&path).2,
            BinlogEvent::TYPE_AES_CTR_ENCRYPTION
        );

        let key = DbKey::password("WhatIDisThat");
        let mut replay = Vec::new();
        let mut binlog = Binlog::new();
        binlog
            .init(
                &path,
                &mut |e| replay.push((e.id, e.get_data().to_vec())),
                key.clone(),
                DbKey::empty(),
            )
            .ensure();
        assert_eq!(replay, vec![(1u64, b"AAAA".to_vec())]);
        assert_eq!(
            first_record_header(&path).2,
            BinlogEvent::TYPE_AES_CTR_ENCRYPTION
        );

        // and it survives a full reopen with the key
        binlog.close(true).ensure();
        let mut binlog = Binlog::new();
        let mut replay = Vec::new();
        binlog
            .init(
                &path,
                &mut |e| replay.push((e.id, e.get_data().to_vec())),
                key,
                DbKey::empty(),
            )
            .ensure();
        assert_eq!(replay, vec![(1u64, b"AAAA".to_vec())]);
        let _ = destroy(&path);
    }

    #[test]
    fn binlog_raw_key_round_trip() {
        let path = db_path("test_binlog_raw_key");
        let _ = destroy(&path);
        let key = DbKey::raw_key([7u8; 32]);
        {
            let mut binlog = Binlog::new();
            binlog
                .init(&path, &mut |_| {}, key.clone(), DbKey::empty())
                .ensure();
            binlog.add(1, b"AAAA");
            binlog.sync();
            binlog.close(true).ensure();
        }
        assert_eq!(
            first_record_header(&path).2,
            BinlogEvent::TYPE_AES_CTR_ENCRYPTION
        );
        let mut binlog = Binlog::new();
        let mut replay = Vec::new();
        binlog
            .init(
                &path,
                &mut |e| replay.push((e.id, e.get_data().to_vec())),
                key,
                DbKey::empty(),
            )
            .ensure();
        assert_eq!(replay, vec![(1u64, b"AAAA".to_vec())]);
        let _ = destroy(&path);
    }

    /// Bloat reindex under encryption: the compaction rewrites the
    /// handshake (reusing the salt, fresh iv) and re-encrypts the replay.
    #[test]
    fn binlog_encrypted_reindex_compacts() {
        let path = db_path("test_binlog_encrypted_reindex");
        let _ = destroy(&path);
        let key = DbKey::password("WhatIDisThat");
        let mut binlog = Binlog::new();
        binlog
            .init(&path, &mut |_| {}, key.clone(), DbKey::empty())
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
        assert!(size < 10_000, "reindex must compact, got {size}");
        binlog.close(true).ensure();

        let mut binlog = Binlog::new();
        let mut replay = Vec::new();
        binlog
            .init(
                &path,
                &mut |e| replay.push((e.id, e.get_data().to_vec())),
                key,
                DbKey::empty(),
            )
            .ensure();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].0, id);
        assert_eq!(replay[0].1, last);
        let _ = destroy(&path);
    }
}
