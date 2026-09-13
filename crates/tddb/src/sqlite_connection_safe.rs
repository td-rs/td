//! Port of `td/db/SqliteConnectionSafe` + `td/db/SqliteKeyValueSafe`: a
//! shared, safely-closable connection and a key-value view on top of it.
//!
//! C++ gives each scheduler thread its own connection via
//! LazySchedulerLocalStorage; the actor runtime is Track D, so this subset
//! keeps a single shared connection (SqliteDb clones share one handle, as in
//! C++), which is what TdDb's single-threaded usage needs.

use std::sync::{Arc, Mutex};

use crate::db_key::DbKey;
use crate::sqlite_db::{SqliteDb, destroy};
use crate::sqlite_key_value::SqliteKeyValue;
use tdutils::status::Result;
use tdutils::status::Status;

/// `td::SqliteConnectionSafe`
pub struct SqliteConnectionSafe {
    inner: Mutex<Option<SqliteDb>>,
    path: Mutex<String>,
    // kept for the SqliteConnectionSafe::set/open parity with C++ (the key is
    // applied at init time via SqliteDb::open_with_key)
    #[allow(dead_code)]
    db_key: DbKey,
}

impl SqliteConnectionSafe {
    /// `SqliteConnectionSafe::set` — install an already-opened connection.
    pub fn set(&self, db: SqliteDb) {
        *self.inner.lock().unwrap() = Some(db);
    }

    /// `SqliteConnectionSafe::init` — open (READ|WRITE|CREATE) + WAL +
    /// secure_delete, like the C++ lsls factory.
    pub fn init(&self, path: &str, db_key: DbKey) -> Status {
        let db = match SqliteDb::open_with_key(path, true, &db_key, None) {
            Result::Ok(db) => db,
            Result::Err(status) => return status,
        };
        db.exec("PRAGMA journal_mode=WAL").ensure();
        db.exec("PRAGMA secure_delete=1").ensure();
        *self.path.lock().unwrap() = path.to_string();
        self.set(db);
        Status::ok()
    }

    pub fn new(path: &str, db_key: DbKey) -> SqliteConnectionSafe {
        SqliteConnectionSafe {
            inner: Mutex::new(None),
            path: Mutex::new(path.to_string()),
            db_key,
        }
    }

    /// `SqliteConnectionSafe::get` — a clone sharing the connection.
    pub fn get(&self) -> SqliteDb {
        self.inner
            .lock()
            .unwrap()
            .clone()
            .expect("SqliteConnectionSafe is not initialized")
    }

    /// `SqliteConnectionSafe::close`
    pub fn close(&self) {
        self.inner.lock().unwrap().take();
    }

    /// `SqliteConnectionSafe::close_and_destroy`
    pub fn close_and_destroy(&self) {
        self.close();
        let path = self.path.lock().unwrap().clone();
        let _ = destroy(&path);
    }
}

/// `td::SqliteKeyValueSafe`
pub struct SqliteKeyValueSafe {
    inner: Mutex<Option<SqliteKeyValue>>,
    name: String,
    safe_connection: Arc<SqliteConnectionSafe>,
}

impl SqliteKeyValueSafe {
    /// `SqliteKeyValueSafe::new` — the key-value view is opened lazily per
    /// user in C++ (lsls); here it opens on first `get` through the mutex.
    pub fn new(name: &str, safe_connection: Arc<SqliteConnectionSafe>) -> SqliteKeyValueSafe {
        SqliteKeyValueSafe {
            inner: Mutex::new(None),
            name: name.to_string(),
            safe_connection,
        }
    }

    /// `SqliteKeyValueSafe::get` — the C++ returns a mutable reference via
    /// lsls; the shared-mutex equivalent runs the closure under the lock.
    pub fn with_kv<R>(&self, f: impl FnOnce(&mut SqliteKeyValue) -> R) -> R {
        let mut inner = self.inner.lock().unwrap();
        if inner.is_none() {
            let mut kv = SqliteKeyValue::new();
            kv.init_with_connection(self.safe_connection.get().clone(), &self.name)
                .ensure();
            *inner = Some(kv);
        }
        f(inner.as_mut().unwrap())
    }

    /// `SqliteKeyValueSafe::close`
    pub fn close(&self) {
        self.inner.lock().unwrap().take();
    }
}
