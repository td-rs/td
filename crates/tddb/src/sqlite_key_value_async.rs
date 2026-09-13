//! Port of `td/db/SqliteKeyValueAsync`: buffered asynchronous writes over a
//! [`SqliteKeyValueSafe`]. Writes land in a shared buffer (visible to `get`
//! immediately, like the C++ buffer fast path) and are applied by a small
//! worker thread ~2 seconds after the first pending write — the C++ actor
//! alarm becomes a deadline tick. `close` flushes and stops the worker.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::sqlite_connection_safe::SqliteKeyValueSafe;

/// C++ flush timeout (2 seconds since the first buffered write).
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(20);

enum Command {
    CloseAndFlush,
}

struct Inner {
    buffer: Mutex<HashMap<String, Option<String>>>,
    pending_prefix_erases: Mutex<Vec<Vec<u8>>>,
    kv_safe: Arc<SqliteKeyValueSafe>,
}

impl Inner {
    fn drain(&self) -> HashMap<String, Option<String>> {
        let batch: HashMap<String, Option<String>> =
            std::mem::take(&mut *self.buffer.lock().unwrap());
        let prefix_erases: Vec<Vec<u8>> =
            std::mem::take(&mut *self.pending_prefix_erases.lock().unwrap());
        if !batch.is_empty() || !prefix_erases.is_empty() {
            self.kv_safe.with_kv(|kv| {
                for (key, value) in batch.iter() {
                    match value {
                        Some(value) => kv.set(key.as_bytes(), value.as_bytes()),
                        None => kv.erase(key.as_bytes()),
                    }
                }
                for prefix in &prefix_erases {
                    kv.erase_by_prefix(prefix);
                }
            });
        }
        batch
    }
}

/// `td::SqliteKeyValueAsync`
pub struct SqliteKeyValueAsync {
    inner: Arc<Inner>,
    sender: Mutex<Option<Sender<Command>>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl SqliteKeyValueAsync {
    /// `SqliteKeyValueAsync::init`
    pub fn init(kv_safe: Arc<SqliteKeyValueSafe>) -> SqliteKeyValueAsync {
        let inner = Arc::new(Inner {
            buffer: Mutex::new(HashMap::new()),
            pending_prefix_erases: Mutex::new(Vec::new()),
            kv_safe,
        });
        let (sender, receiver) = channel::<Command>();
        let worker_inner = inner.clone();
        let worker = std::thread::Builder::new()
            .name("SqliteKeyValueAsync".to_string())
            .spawn(move || flush_loop(&worker_inner, &receiver))
            .expect("failed to spawn SqliteKeyValueAsync worker");

        SqliteKeyValueAsync {
            inner,
            sender: Mutex::new(Some(sender)),
            worker: Mutex::new(Some(worker)),
        }
    }

    /// `SqliteKeyValueAsyncInterface::set`
    pub fn set(&self, key: String, value: String) {
        self.inner.buffer.lock().unwrap().insert(key, Some(value));
    }

    /// `SqliteKeyValueAsyncInterface::set_all`
    pub fn set_all(&self, key_values: HashMap<String, String>) {
        let mut buffer = self.inner.buffer.lock().unwrap();
        for (key, value) in key_values {
            buffer.insert(key, Some(value));
        }
    }

    /// `SqliteKeyValueAsyncInterface::erase` — buffered as a tombstone.
    pub fn erase(&self, key: String) {
        self.inner.buffer.lock().unwrap().insert(key, None);
    }

    /// `SqliteKeyValueAsyncInterface::erase_by_prefix` — buffered tombstones
    /// for already-buffered keys; unbuffered keys are erased at flush.
    pub fn erase_by_prefix(&self, key_prefix: &str) {
        let prefix = key_prefix.as_bytes();
        let mut buffer = self.inner.buffer.lock().unwrap();
        let keys: Vec<String> = buffer
            .keys()
            .filter(|k| k.as_bytes().starts_with(prefix))
            .cloned()
            .collect();
        for key in keys {
            buffer.insert(key, None);
        }
        self.inner
            .pending_prefix_erases
            .lock()
            .unwrap()
            .push(prefix.to_vec());
    }

    /// `SqliteKeyValueAsyncInterface::get` — buffered values answer
    /// immediately; erasures answer empty; otherwise read through.
    pub fn get(&self, key: &str) -> String {
        if let Some(value) = self.inner.buffer.lock().unwrap().get(key) {
            return value.clone().unwrap_or_default();
        }
        self.inner
            .kv_safe
            .with_kv(|kv| String::from_utf8_lossy(&kv.get(key.as_bytes())).into_owned())
    }

    /// Drains the buffer synchronously (tests and shutdown paths).
    pub fn flush_now(&self) {
        self.inner.drain();
    }

    /// `SqliteKeyValueAsyncInterface::close` — flush, stop the worker, close.
    pub fn close(&mut self) {
        self.flush_now();
        if let Some(sender) = self.sender.lock().unwrap().take() {
            let _ = sender.send(Command::CloseAndFlush);
        }
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = worker.join();
        }
        self.inner.kv_safe.close();
    }
}

fn flush_loop(inner: &Inner, receiver: &Receiver<Command>) {
    let mut first_pending: Option<Instant> = None;
    loop {
        if matches!(receiver.try_recv(), Ok(Command::CloseAndFlush)) {
            inner.drain();
            return;
        }
        // Take the emptiness check outside the match: a guard held by the
        // scrutinee would outlive the arm body and deadlock `drain`.
        let buffer_non_empty = !inner.buffer.lock().unwrap().is_empty();
        match (buffer_non_empty, first_pending) {
            (false, _) => {
                first_pending = None;
                std::thread::sleep(TICK);
            }
            (true, None) => {
                first_pending = Some(Instant::now());
                std::thread::sleep(TICK);
            }
            (true, Some(start)) if start.elapsed() >= FLUSH_TIMEOUT => {
                inner.drain();
                first_pending = None;
            }
            (true, Some(_)) => std::thread::sleep(TICK),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db_key::DbKey;
    use crate::sqlite_connection_safe::SqliteConnectionSafe;

    fn db_path(name: &str) -> String {
        format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn sqlite_key_value_async_buffered_writes() {
        let path = db_path("test_skv_async");
        let _ = crate::sqlite_db::destroy(&path);

        let safe = Arc::new(SqliteConnectionSafe::new(&path, DbKey::empty()));
        safe.init(&path, DbKey::empty()).ensure();
        let kv_safe = Arc::new(SqliteKeyValueSafe::new("kv", safe.clone()));

        let mut kv_async = SqliteKeyValueAsync::init(kv_safe.clone());
        kv_async.set("a".to_string(), "1".to_string());
        kv_async.set("bb".to_string(), "22".to_string());

        // buffered value is visible immediately
        assert_eq!(kv_async.get("a"), "1");
        assert_eq!(kv_async.get("bb"), "22");
        assert_eq!(kv_async.get("missing"), "");

        // erase tombstone answers empty even before flush
        kv_async.erase("bb".to_string());
        assert_eq!(kv_async.get("bb"), "");

        // force the batch through and reopen to verify persistence
        kv_async.flush_now();
        kv_async.close();
        drop(kv_async);

        let mut kv = crate::sqlite_key_value::SqliteKeyValue::new();
        kv.init_with_connection(safe.get(), "kv").ensure();
        assert_eq!(kv.get(b"a"), b"1");
        assert!(kv.get(b"bb").is_empty());
        kv.close();

        safe.close_and_destroy();
        assert!(!std::path::Path::new(&path).exists());
    }

    #[test]
    fn sqlite_key_value_async_lazy_flush_lands() {
        let path = db_path("test_skv_async_lazy");
        let _ = crate::sqlite_db::destroy(&path);

        let safe = Arc::new(SqliteConnectionSafe::new(&path, DbKey::empty()));
        safe.init(&path, DbKey::empty()).ensure();
        let kv_safe = Arc::new(SqliteKeyValueSafe::new("kv", safe.clone()));

        let mut kv_async = SqliteKeyValueAsync::init(kv_safe.clone());
        kv_async.set("lazy".to_string(), "value".to_string());

        // not yet flushed: only the buffer has it; the sync kv still doesn't
        kv_safe.with_kv(|kv| assert!(kv.get(b"lazy").is_empty()));

        // the worker flushes within FLUSH_TIMEOUT + tick
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            let done = kv_safe.with_kv(|kv| !kv.get(b"lazy").is_empty());
            if done || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        kv_safe.with_kv(|kv| assert_eq!(kv.get(b"lazy"), b"value"));
        kv_async.close();

        safe.close_and_destroy();
    }
}
