//! Port of `td/db/SqliteDb.h` + `td/db/detail/RawSqliteDb` on rusqlite.
//!
//! Encrypted databases use SQLCipher (rusqlite `bundled-sqlcipher`), the same
//! engine tdlib links, so `PRAGMA key` / `change_key` / the C++-generated
//! sample databases (SQLCipher 3 and 4 fixtures) stay byte-compatible.

use std::cell::Cell;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::OpenFlags;

use crate::db_key::DbKey;
use tdutils::status::Result;
use tdutils::status::Status;
use tdutils::{try_result, try_status};

static WAS_DATABASE_DESTROYED: AtomicBool = AtomicBool::new(false);

fn was_any_database_destroyed() -> bool {
    WAS_DATABASE_DESTROYED.load(Ordering::Relaxed)
}

/// `RawSqliteDb::with_db_path`: the main file plus the sidecar files sqlite
/// may leave around.
pub fn with_db_path(main_path: &str, mut f: impl FnMut(&str)) {
    f(main_path);
    f(&format!("{main_path}-journal"));
    f(&format!("{main_path}-wal"));
    f(&format!("{main_path}-shm"));
}

/// `RawSqliteDb::destroy`: unlink the database and its sidecars. Returns an
/// error only if a file other than `-shm` survived the removal attempt.
pub fn destroy(path: &str) -> Status {
    let mut error = Status::ok();
    with_db_path(path, |p| {
        let _ = std::fs::remove_file(p);
        if !p.ends_with("-shm") && Path::new(p).metadata().is_ok() {
            error = Status::error_message(&format!("Failed to delete file \"{p}\""));
        }
    });
    error
}

/// Status → Result<()> for the `try_result!` macro.
fn status_to_result(status: Status) -> Result<()> {
    if status.is_ok() {
        Result::Ok(())
    } else {
        Result::Err(status)
    }
}

fn last_error_for(err: &rusqlite::Error, path: &str) -> Status {
    Status::error_message(&format!("{err} for database \"{path}\""))
}

/// Port of the C++ `trace_v2_callback`: print expanded SQL (or the raw
/// statement when it is a comment).
unsafe extern "C" fn trace_v2_callback(
    _code: u32,
    _ctx: *mut std::ffi::c_void,
    p_raw: *mut std::ffi::c_void,
    x_raw: *mut std::ffi::c_void,
) -> i32 {
    unsafe {
        let x = x_raw as *const std::ffi::c_char;
        if *x == b'-' as std::ffi::c_char && *x.add(1) == b'-' as std::ffi::c_char {
            tracing::error!("{}", std::ffi::CStr::from_ptr(x).to_string_lossy());
        } else {
            let expanded =
                rusqlite::ffi::sqlite3_expanded_sql(p_raw as *mut rusqlite::ffi::sqlite3_stmt);
            if !expanded.is_null() {
                tracing::error!("{}", std::ffi::CStr::from_ptr(expanded).to_string_lossy());
                rusqlite::ffi::sqlite3_free(expanded.cast());
            }
        }
    }
    0
}

/// `td::detail::RawSqliteDb`: the raw connection plus the bookkeeping shared
/// between all `SqliteDb::clone`s of one connection.
pub(crate) struct RawSqliteDb {
    // C++ compiles sqlite in serialized mode; the mutex gives the same
    // all-access serialization over rusqlite's !Sync Connection.
    conn: Mutex<rusqlite::Connection>,
    path: String,
    begin_cnt: AtomicUsize,
    cipher_version: Mutex<Option<i32>>,
}

impl RawSqliteDb {
    /// `SqliteDb::init`: open READWRITE (+CREATE when allowed). If the file is
    /// missing and creation is disallowed, fail with tdlib's wording; if
    /// creation is allowed, first delete stale files of an old database.
    fn open(path: &str, allow_creation: bool) -> Result<RawSqliteDb> {
        if Path::new(path).metadata().is_err() {
            if !allow_creation {
                let reason = if was_any_database_destroyed() {
                    "was corrupted and deleted"
                } else {
                    "disappeared"
                };
                return Result::from_status(Status::error_message(&format!(
                    "Database {reason} during execution and can't be recreated: failed to stat"
                )));
            }
            try_status!(destroy(path));
        }

        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE;
        if allow_creation {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }
        let conn = match rusqlite::Connection::open_with_flags(path, flags) {
            Ok(conn) => conn,
            Err(err) => return Result::from_status(last_error_for(&err, path)),
        };
        if let Err(err) = conn.busy_timeout(Duration::from_secs(5)) {
            return Result::from_status(last_error_for(&err, path));
        }
        Result::Ok(RawSqliteDb {
            conn: Mutex::new(conn),
            path: path.to_string(),
            begin_cnt: AtomicUsize::new(0),
            cipher_version: Mutex::new(None),
        })
    }

    fn path(&self) -> &str {
        &self.path
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.conn.lock().unwrap()
    }

    /// `RawSqliteDb::last_error`: on `SQLITE_CORRUPT` mark the database as
    /// destroyed and delete it, exactly like the C++ implementation.
    // Used from S39 (SqliteStatement error mapping), as in C++ get_statement.
    #[allow(dead_code)]
    fn last_error(&self) -> Status {
        // SAFETY: the connection handle is valid for as long as `self` is.
        let code = unsafe { rusqlite::ffi::sqlite3_errcode(self.conn().handle()) };
        if code == rusqlite::ffi::SQLITE_CORRUPT {
            WAS_DATABASE_DESTROYED.store(true, Ordering::Relaxed);
            let _ = destroy(&self.path);
            return Status::error_message(&format!(
                "database disk image is malformed for database \"{}\"",
                self.path
            ));
        }
        // SAFETY: same handle validity as above.
        let message = unsafe {
            std::ffi::CStr::from_ptr(rusqlite::ffi::sqlite3_errmsg(self.conn().handle()))
                .to_string_lossy()
                .into_owned()
        };
        Status::error_message(&format!("{message} for database \"{}\"", self.path))
    }

    /// `RawSqliteDb::on_begin`: true only for the outermost begin.
    fn on_begin(&self) -> bool {
        self.begin_cnt.fetch_add(1, Ordering::SeqCst) == 0
    }

    /// `RawSqliteDb::on_commit`: true only when the outermost transaction
    /// commits; errors on unbalanced commit (without wrapping the counter).
    fn on_commit(&self) -> Result<bool> {
        let prev = self.begin_cnt.fetch_sub(1, Ordering::SeqCst);
        if prev == 0 {
            self.begin_cnt.store(0, Ordering::SeqCst);
            return Result::from_status(Status::error_message("No matching begin for commit"));
        }
        Result::Ok(prev == 1)
    }

    fn set_cipher_version(&self, cipher_version: i32) {
        *self.cipher_version.lock().unwrap() = Some(cipher_version);
    }

    fn get_cipher_version(&self) -> Option<i32> {
        *self.cipher_version.lock().unwrap()
    }
}

/// `td::SqliteDb`
#[derive(Clone)]
pub struct SqliteDb {
    raw: Option<Arc<RawSqliteDb>>,
    enable_logging: Cell<bool>,
}

impl Default for SqliteDb {
    fn default() -> Self {
        Self::new()
    }
}

fn quote_string(s: &str) -> String {
    s.replace('\'', "''")
}

/// `db_key_to_sqlcipher_key`: the value of `PRAGMA key` for a [`DbKey`].
fn db_key_to_sqlcipher_key(db_key: &DbKey) -> String {
    if db_key.is_empty() {
        return "''".to_string();
    }
    if db_key.is_password() {
        let password = String::from_utf8_lossy(db_key.data());
        return format!("'{}'", quote_string(&password));
    }
    assert!(db_key.is_raw_key());
    let raw_key = db_key.data();
    assert_eq!(raw_key.len(), 32);
    let hex: String = raw_key.iter().map(|b| format!("{b:02x}")).collect();
    format!("\"x'{hex}'\"")
}

impl SqliteDb {
    pub fn new() -> SqliteDb {
        SqliteDb {
            raw: None,
            enable_logging: Cell::new(false),
        }
    }

    pub fn empty(&self) -> bool {
        self.raw.is_none()
    }

    pub fn close(&mut self) {
        *self = SqliteDb::new();
    }

    fn raw(&self) -> &Arc<RawSqliteDb> {
        self.raw.as_ref().expect("SqliteDb is empty")
    }

    /// `SqliteDb::init` — open a raw connection into this (empty) database.
    fn init(&mut self, path: &str, allow_creation: bool) -> Status {
        match RawSqliteDb::open(path, allow_creation) {
            Result::Ok(raw) => {
                self.raw = Some(Arc::new(raw));
                Status::ok()
            }
            Result::Err(status) => status,
        }
    }

    /// `SqliteDb::exec`
    pub fn exec(&self, cmd: &str) -> Status {
        let raw = self.raw();
        if self.enable_logging.get() {
            tracing::debug!(query = cmd, database = raw.path(), "Start exec");
        }
        match raw.conn().execute_batch(cmd) {
            Ok(()) => {
                if self.enable_logging.get() {
                    tracing::debug!("Finish exec");
                }
                Status::ok()
            }
            Err(err) => {
                let message = err.to_string();
                if self.enable_logging.get() {
                    tracing::debug!(error = %message, "Finish exec with error");
                }
                Status::error_message(&format!(
                    "query {cmd:?} to database \"{}\" failed: {message}",
                    raw.path()
                ))
            }
        }
    }

    /// `SqliteDb::has_table`
    pub fn has_table(&self, table: &str) -> Result<bool> {
        let stmt =
            format!("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='{table}'");
        try_result!(cnt, self.query_row_i32(&stmt));
        Result::Ok(cnt == 1)
    }

    /// `SqliteDb::get_pragma`
    pub fn get_pragma(&self, name: &str) -> Result<String> {
        self.query_row_string(&format!("PRAGMA {name}"))
    }

    /// `SqliteDb::get_pragma_string`
    pub fn get_pragma_string(&self, name: &str) -> Result<String> {
        self.query_row_string(&format!("PRAGMA {name}"))
    }

    /// `SqliteDb::user_version`
    pub fn user_version(&self) -> Result<i32> {
        let raw = self.raw();
        match raw
            .conn()
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
        {
            Ok(version) => Result::Ok(version),
            Err(err) => Result::from_status(Status::error_message(&format!(
                "PRAGMA user_version failed for database \"{}\": {err}",
                raw.path()
            ))),
        }
    }

    /// `SqliteDb::set_user_version`
    pub fn set_user_version(&self, version: i32) -> Status {
        self.exec(&format!("PRAGMA user_version = {version}"))
    }

    /// `SqliteDb::begin_read_transaction`
    pub fn begin_read_transaction(&self) -> Status {
        if self.raw().on_begin() {
            return self.exec("BEGIN");
        }
        Status::ok()
    }

    /// `SqliteDb::begin_write_transaction`
    pub fn begin_write_transaction(&self) -> Status {
        if self.raw().on_begin() {
            return self.exec("BEGIN IMMEDIATE");
        }
        Status::ok()
    }

    /// `SqliteDb::commit_transaction`
    pub fn commit_transaction(&self) -> Status {
        match self.raw().on_commit() {
            Result::Ok(true) => self.exec("COMMIT"),
            Result::Ok(false) => Status::ok(),
            Result::Err(status) => status,
        }
    }

    /// `SqliteDb::check_encryption` — fails on an encrypted (keyed) database.
    fn check_encryption(&self) -> Status {
        let status = self.exec("SELECT count(*) FROM sqlite_master");
        if status.is_ok() {
            self.enable_logging.set(true);
        }
        status
    }

    /// `SqliteDb::trace` — log every statement like the C++ `trace_v2`
    /// callback (LOG(ERROR) per statement, expanded SQL for real statements).
    pub fn trace(&self, flag: bool) {
        // SAFETY: the connection handle is valid for as long as `self` is;
        // the callback uses no context.
        unsafe {
            rusqlite::ffi::sqlite3_trace_v2(
                self.raw().conn().handle(),
                rusqlite::ffi::SQLITE_TRACE_STMT,
                if flag { Some(trace_v2_callback) } else { None },
                std::ptr::null_mut(),
            );
        }
    }

    /// `SqliteDb::open_with_key`
    pub fn open_with_key(
        path: &str,
        allow_creation: bool,
        db_key: &DbKey,
        cipher_version: Option<i32>,
    ) -> Result<SqliteDb> {
        let res = do_open_with_key(path, allow_creation, db_key, cipher_version.unwrap_or(0));
        if res.is_error() && cipher_version.is_none() && !db_key.is_empty() {
            return do_open_with_key(path, false, db_key, 3);
        }
        res
    }

    fn set_cipher_version(&self, cipher_version: i32) {
        self.raw().set_cipher_version(cipher_version);
    }

    /// `SqliteDb::get_cipher_version`
    pub fn get_cipher_version(&self) -> Option<i32> {
        self.raw.as_ref().and_then(|raw| raw.get_cipher_version())
    }

    /// `SqliteDb::change_key`
    pub fn change_key(
        path: &str,
        allow_creation: bool,
        new_db_key: &DbKey,
        old_db_key: &DbKey,
    ) -> Result<SqliteDb> {
        // fast path
        if let Result::Ok(db) = Self::open_with_key(path, allow_creation, new_db_key, None) {
            return Result::Ok(db);
        }

        let create_database = |tmp_path: &str| -> Status {
            try_status!(destroy(tmp_path));
            let mut db = SqliteDb::new();
            db.init(tmp_path, true)
        };

        try_result!(db, Self::open_with_key(path, false, old_db_key, None));
        try_result!(user_version, db.user_version());
        let new_key = db_key_to_sqlcipher_key(new_db_key);
        if old_db_key.is_empty() && !new_db_key.is_empty() {
            tracing::debug!("ENCRYPT");
            let tmp_path = format!("{path}.encrypted");
            try_status!(create_database(&tmp_path));

            // make sure that database is not empty
            try_status!(
                db.exec("CREATE TABLE IF NOT EXISTS encryption_dummy_table(id INT PRIMARY KEY)")
            );
            try_status!(
                db.exec(&format!(
                    "ATTACH DATABASE '{}' AS encrypted KEY {new_key}",
                    quote_string(&tmp_path)
                )),
                "Failed to attach encrypted database: "
            );
            try_status!(db.exec("SELECT sqlcipher_export('encrypted')"));
            try_status!(db.exec(&format!("PRAGMA encrypted.user_version = {user_version}")));
            try_status!(db.exec("DETACH DATABASE encrypted"));
            let mut closed = db.clone();
            closed.close();
            if let Err(err) = std::fs::rename(&tmp_path, path) {
                return Result::from_status(Status::error_message(&format!(
                    "Failed to rename \"{tmp_path}\" to \"{path}\": {err}"
                )));
            }
        } else if !old_db_key.is_empty() && new_db_key.is_empty() {
            tracing::debug!("DECRYPT");
            let tmp_path = format!("{path}.encrypted");
            try_status!(create_database(&tmp_path));

            try_status!(
                db.exec(&format!(
                    "ATTACH DATABASE '{}' AS decrypted KEY ''",
                    quote_string(&tmp_path)
                )),
                "Failed to attach decrypted database: "
            );
            try_status!(db.exec("SELECT sqlcipher_export('decrypted')"));
            try_status!(db.exec(&format!("PRAGMA decrypted.user_version = {user_version}")));
            try_status!(db.exec("DETACH DATABASE decrypted"));
            let mut closed = db.clone();
            closed.close();
            if let Err(err) = std::fs::rename(&tmp_path, path) {
                return Result::from_status(Status::error_message(&format!(
                    "Failed to rename \"{tmp_path}\" to \"{path}\": {err}"
                )));
            }
        } else {
            tracing::debug!("REKEY");
            try_status!(db.exec(&format!("PRAGMA rekey = {new_key}")));
        }

        try_result!(new_db, Self::open_with_key(path, false, new_db_key, None));
        try_result!(got_version, new_db.user_version());
        assert_eq!(
            got_version, user_version,
            "user_version must survive change_key"
        );
        Result::Ok(new_db)
    }

    // -- internal single-row query helpers (replaced by SqliteStatement in S39)

    fn query_row_i32(&self, sql: &str) -> Result<i32> {
        let raw = self.raw();
        match raw.conn().query_row(sql, [], |row| row.get::<_, i32>(0)) {
            Ok(value) => Result::Ok(value),
            Err(err) => Result::from_status(last_error_for(&err, raw.path())),
        }
    }

    fn query_row_string(&self, sql: &str) -> Result<String> {
        let raw = self.raw();
        // sqlite3_column_blob semantics: accept TEXT as well as BLOB.
        match raw
            .conn()
            .query_row(sql, [], |row| row.get::<_, rusqlite::types::Value>(0))
        {
            Ok(value) => Result::Ok(match value {
                rusqlite::types::Value::Text(text) => text,
                rusqlite::types::Value::Blob(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                other => panic!("unexpected value type: {other:?}"),
            }),
            Err(err) => Result::from_status(last_error_for(&err, raw.path())),
        }
    }
}

/// `SqliteDb::do_open_with_key`
fn do_open_with_key(
    path: &str,
    allow_creation: bool,
    db_key: &DbKey,
    cipher_version: i32,
) -> Result<SqliteDb> {
    let mut db = SqliteDb::new();
    try_result!(
        init,
        status_to_result(db.init(path, allow_creation)),
        "Failed to open database: "
    );

    if !db_key.is_empty() {
        if db.check_encryption().is_ok() {
            return Result::from_status(Status::error_message(&format!(
                "No key is needed for database \"{path}\""
            )));
        }
        let key = db_key_to_sqlcipher_key(db_key);
        try_result!(
            keyed,
            status_to_result(db.exec(&format!("PRAGMA key = {key}")))
        );
        if cipher_version != 0 {
            tracing::debug!("Trying SQLCipher compatibility mode with version = {cipher_version}");
            try_result!(
                compat,
                status_to_result(
                    db.exec(&format!("PRAGMA cipher_compatibility = {cipher_version}"))
                )
            );
        }
        db.set_cipher_version(cipher_version);
    }
    try_result!(
        checked,
        status_to_result(db.check_encryption()),
        "Can't check database: "
    );
    Result::Ok(db)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_path(name: &str) -> String {
        format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    /// tdlib test `TEST(DB, sqlite_lfs)`.
    #[test]
    fn sqlite_lfs() {
        let path = db_path("test_sqlite_db_lfs");
        let _ = destroy(&path);
        let db = SqliteDb::open_with_key(&path, true, &DbKey::empty(), None).unwrap();
        db.exec("PRAGMA journal_mode=WAL").ensure();
        db.exec("PRAGMA user_version").ensure();
        let _ = destroy(&path);
    }

    /// tdlib test `TEST(DB, sqlite_encryption)`. The `SqliteKeyValue` usage
    /// (S42) is replaced by direct table statements; everything else,
    /// including both rekey directions, is a faithful port.
    #[test]
    fn sqlite_encryption() {
        let path = db_path("test_sqlite_db_enc");
        let _ = destroy(&path);

        let empty = DbKey::empty();
        let cucumber = DbKey::password("cucu'\"mb er");
        let tomato = DbKey::raw_key(vec![b'a'; 32]);

        let kv_set = |db: &SqliteDb| -> Status {
            try_status!(db.exec("CREATE TABLE IF NOT EXISTS kv(k TEXT PRIMARY KEY, v TEXT)"));
            db.exec("INSERT OR REPLACE INTO kv VALUES('a', 'b')")
        };
        let kv_get = |db: &SqliteDb| -> String {
            db.query_row_string("SELECT v FROM kv WHERE k = 'a'")
                .unwrap()
        };

        {
            let db = SqliteDb::open_with_key(&path, true, &empty, None).unwrap();
            db.set_user_version(123).ensure();
            kv_set(&db).ensure();
        }
        assert!(SqliteDb::open_with_key(&path, false, &cucumber, None).is_error());

        SqliteDb::change_key(&path, false, &cucumber, &empty).unwrap();
        SqliteDb::change_key(&path, false, &cucumber, &empty).unwrap();

        assert!(SqliteDb::open_with_key(&path, false, &tomato, None).is_error());
        {
            let db = SqliteDb::open_with_key(&path, false, &cucumber, None).unwrap();
            assert_eq!(kv_get(&db), "b");
            assert_eq!(db.user_version().unwrap(), 123);
        }

        SqliteDb::change_key(&path, false, &tomato, &cucumber).unwrap();
        SqliteDb::change_key(&path, false, &tomato, &cucumber).unwrap();

        assert!(SqliteDb::open_with_key(&path, false, &cucumber, None).is_error());
        {
            let db = SqliteDb::open_with_key(&path, false, &tomato, None).unwrap();
            assert_eq!(kv_get(&db), "b");
            assert_eq!(db.user_version().unwrap(), 123);
        }

        SqliteDb::change_key(&path, false, &empty, &tomato).unwrap();
        SqliteDb::change_key(&path, false, &empty, &tomato).unwrap();

        {
            let db = SqliteDb::open_with_key(&path, false, &empty, None).unwrap();
            assert_eq!(kv_get(&db), "b");
            assert_eq!(db.user_version().unwrap(), 123);
        }
        assert!(SqliteDb::open_with_key(&path, false, &cucumber, None).is_error());
        let _ = destroy(&path);
    }

    /// tdlib test `TEST(DB, sqlite_encryption_migrate_v3)`: the fixture is a
    /// database generated by the C++ code with SQLCipher based on
    /// SQLite 3.15.2 (SQLCipher 3), embedded verbatim from `test/data.cpp`.
    #[test]
    fn sqlite_encryption_migrate_v3() {
        migrate_fixture("sqlite_sample_db_v3", "cucumber");
    }

    /// tdlib test `TEST(DB, sqlite_encryption_migrate_v4)`: fixture generated
    /// with SQLCipher 4.4.0, embedded verbatim from `test/data.cpp`.
    #[test]
    fn sqlite_encryption_migrate_v4() {
        migrate_fixture("sqlite_sample_db_v4", "cucu'\"mb er");
    }

    fn migrate_fixture(fixture: &str, password: &str) {
        let data_dir = format!("{}/tests/data", env!("CARGO_MANIFEST_DIR"));
        let path = format!("{data_dir}/test_sqlite_db_{fixture}");
        let _ = destroy(&path);
        let cucumber = DbKey::password(password);
        std::fs::write(
            &path,
            std::fs::read(format!("{data_dir}/{fixture}.db")).unwrap(),
        )
        .unwrap();
        let db = SqliteDb::open_with_key(&path, true, &cucumber, None).unwrap();
        // The sample databases store kv keys as BLOBs (SqliteKeyValue binds
        // with bind_blob), so match with a blob literal like the C++ test.
        assert_eq!(
            db.query_row_string("SELECT v FROM kv WHERE k = x'68656C6C6F'")
                .unwrap(),
            "world"
        );
        assert_eq!(db.user_version().unwrap(), 123);
        let _ = destroy(&path);
    }

    #[test]
    fn sqlcipher_key_formats() {
        assert_eq!(db_key_to_sqlcipher_key(&DbKey::empty()), "''");
        assert_eq!(db_key_to_sqlcipher_key(&DbKey::password("a'b")), "'a''b'");
        assert_eq!(
            db_key_to_sqlcipher_key(&DbKey::raw_key([0u8; 32])),
            "\"x'0000000000000000000000000000000000000000000000000000000000000000'\""
        );
    }

    #[test]
    fn unbalanced_commit_is_an_error() {
        let path = db_path("test_sqlite_db_txn");
        let _ = destroy(&path);
        let db = SqliteDb::open_with_key(&path, true, &DbKey::empty(), None).unwrap();
        assert!(db.commit_transaction().is_error());
        db.begin_write_transaction().ensure();
        db.begin_write_transaction().ensure();
        // only the outermost begin issued SQL; both commits succeed, the
        // outermost one actually committing, and a third commit fails.
        assert!(db.commit_transaction().is_ok());
        assert!(db.commit_transaction().is_ok());
        assert!(db.commit_transaction().is_error());
        let _ = destroy(&path);
    }

    #[test]
    fn missing_database_without_creation_fails() {
        let path = db_path("test_sqlite_db_missing");
        let _ = destroy(&path);
        assert!(SqliteDb::open_with_key(&path, false, &DbKey::empty(), None).is_error());
    }

    #[test]
    fn destroy_removes_sidecars() {
        let path = db_path("test_sqlite_db_sidecars");
        let _ = destroy(&path);
        let mut db = SqliteDb::open_with_key(&path, true, &DbKey::empty(), None).unwrap();
        db.exec("PRAGMA journal_mode=WAL").ensure();
        db.close();
        for suffix in ["", "-journal", "-wal", "-shm"] {
            std::fs::write(format!("{path}{suffix}"), b"x").unwrap();
        }
        destroy(&path).ensure();
        for suffix in ["", "-journal", "-wal", "-shm"] {
            assert!(!Path::new(&format!("{path}{suffix}")).exists());
        }
    }
}
