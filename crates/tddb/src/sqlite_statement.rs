//! Port of `td/db/SqliteStatement`: a prepared statement driven through the
//! sqlite C API, with tdlib's state machine (`Start`/`HaveRow`/`Finish`),
//! unchecked typed `view_*` accessors, and error mapping through the shared
//! [`crate::sqlite_db::RawSqliteDb`] (including corrupted-db deletion).

use std::ffi::CStr;
use std::sync::Arc;
use std::sync::MutexGuard;

use rusqlite::ffi;

use crate::sqlite_db::RawSqliteDb;
use tdutils::status::Result;
use tdutils::status::Status;

/// `SqliteStatement::Datatype`
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Datatype {
    Integer,
    Float,
    Blob,
    Null,
    Text,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Start,
    HaveRow,
    Finish,
}

/// `td::SqliteStatement`
pub struct SqliteStatement {
    // Invariant: non-null, valid while `db` keeps the connection alive;
    // finalized exactly once in Drop.
    stmt: *mut ffi::sqlite3_stmt,
    db: Arc<RawSqliteDb>,
    state: State,
}

fn datatype_of(type_code: i32) -> Datatype {
    match type_code {
        ffi::SQLITE_INTEGER => Datatype::Integer,
        ffi::SQLITE_FLOAT => Datatype::Float,
        ffi::SQLITE_BLOB => Datatype::Blob,
        ffi::SQLITE_NULL => Datatype::Null,
        ffi::SQLITE3_TEXT => Datatype::Text,
        _ => unreachable!("unknown sqlite column type {type_code}"),
    }
}

impl SqliteStatement {
    /// `SqliteDb::get_statement` tail: take ownership of a prepared stmt.
    pub(crate) fn new(stmt: *mut ffi::sqlite3_stmt, db: Arc<RawSqliteDb>) -> SqliteStatement {
        assert!(!stmt.is_null());
        SqliteStatement {
            stmt,
            db,
            state: State::Start,
        }
    }

    fn connection(&self) -> MutexGuard<'_, rusqlite::Connection> {
        self.db.conn()
    }

    /// `SqliteStatement::last_error`
    fn last_error(&self) -> Status {
        self.db.last_error()
    }

    /// `SqliteStatement::bind_blob` — sqlite keeps the slice (SQLITE_STATIC),
    /// so it must stay valid until the next `step()`, like the C++ contract.
    pub fn bind_blob(&mut self, id: i32, blob: &[u8]) -> Status {
        // SAFETY: stmt is valid; blob outlives the call and, per the contract
        // above, until the next step.
        let rc = unsafe {
            ffi::sqlite3_bind_blob(self.stmt, id, blob.as_ptr().cast(), blob.len() as i32, None)
        };
        if rc != ffi::SQLITE_OK {
            return self.last_error();
        }
        Status::ok()
    }

    /// `SqliteStatement::bind_string`
    pub fn bind_string(&mut self, id: i32, str: &str) -> Status {
        // SAFETY: same contract as bind_blob.
        let rc = unsafe {
            ffi::sqlite3_bind_text(self.stmt, id, str.as_ptr().cast(), str.len() as i32, None)
        };
        if rc != ffi::SQLITE_OK {
            return self.last_error();
        }
        Status::ok()
    }

    /// `SqliteStatement::bind_int32`
    pub fn bind_int32(&mut self, id: i32, value: i32) -> Status {
        // SAFETY: stmt is valid.
        let rc = unsafe { ffi::sqlite3_bind_int(self.stmt, id, value) };
        if rc != ffi::SQLITE_OK {
            return self.last_error();
        }
        Status::ok()
    }

    /// `SqliteStatement::bind_int64`
    pub fn bind_int64(&mut self, id: i32, value: i64) -> Status {
        // SAFETY: stmt is valid.
        let rc = unsafe { ffi::sqlite3_bind_int64(self.stmt, id, value) };
        if rc != ffi::SQLITE_OK {
            return self.last_error();
        }
        Status::ok()
    }

    /// `SqliteStatement::bind_null`
    pub fn bind_null(&mut self, id: i32) -> Status {
        // SAFETY: stmt is valid.
        let rc = unsafe { ffi::sqlite3_bind_null(self.stmt, id) };
        if rc != ffi::SQLITE_OK {
            return self.last_error();
        }
        Status::ok()
    }

    /// `SqliteStatement::step`
    pub fn step(&mut self) -> Status {
        if self.state == State::Finish {
            return Status::error_message("One has to reset statement");
        }
        let rc = {
            let _connection = self.connection();
            // SAFETY: stmt is valid.
            unsafe { ffi::sqlite3_step(self.stmt) }
        };
        if rc == ffi::SQLITE_ROW {
            self.state = State::HaveRow;
            return Status::ok();
        }

        self.state = State::Finish;
        if rc == ffi::SQLITE_DONE {
            return Status::ok();
        }
        self.last_error()
    }

    /// `SqliteStatement::view_string` — raw bytes of a TEXT column; the slice
    /// is invalidated by the next `step()`/`reset()`, as in C++.
    pub fn view_string(&self, id: i32) -> &[u8] {
        if self.view_datatype(id) != Datatype::Text {
            tracing::error!("{:?}", self.view_datatype(id));
        }
        // SAFETY: stmt is valid and positioned on a row.
        unsafe {
            let data = ffi::sqlite3_column_text(self.stmt, id);
            let size = ffi::sqlite3_column_bytes(self.stmt, id);
            if data.is_null() {
                return &[];
            }
            std::slice::from_raw_parts(data.cast(), size as usize)
        }
    }

    /// `SqliteStatement::view_blob` — raw bytes of a BLOB column.
    pub fn view_blob(&self, id: i32) -> &[u8] {
        if self.view_datatype(id) != Datatype::Blob {
            tracing::error!("{:?}", self.view_datatype(id));
        }
        // SAFETY: stmt is valid and positioned on a row.
        unsafe {
            let data = ffi::sqlite3_column_blob(self.stmt, id);
            let size = ffi::sqlite3_column_bytes(self.stmt, id);
            if data.is_null() {
                return &[];
            }
            std::slice::from_raw_parts(data.cast(), size as usize)
        }
    }

    /// `SqliteStatement::view_int32`
    pub fn view_int32(&self, id: i32) -> i32 {
        if self.view_datatype(id) != Datatype::Integer {
            tracing::error!("{:?}", self.view_datatype(id));
        }
        // SAFETY: stmt is valid and positioned on a row.
        unsafe { ffi::sqlite3_column_int(self.stmt, id) }
    }

    /// `SqliteStatement::view_int64`
    pub fn view_int64(&self, id: i32) -> i64 {
        if self.view_datatype(id) != Datatype::Integer {
            tracing::error!("{:?}", self.view_datatype(id));
        }
        // SAFETY: stmt is valid and positioned on a row.
        unsafe { ffi::sqlite3_column_int64(self.stmt, id) }
    }

    /// `SqliteStatement::view_datatype`
    pub fn view_datatype(&self, id: i32) -> Datatype {
        // SAFETY: stmt is valid and positioned on a row.
        datatype_of(unsafe { ffi::sqlite3_column_type(self.stmt, id) })
    }

    /// `SqliteStatement::explain` — formatted `EXPLAIN QUERY PLAN` output.
    pub fn explain(&mut self) -> Result<String> {
        let connection = self.connection();
        // SAFETY: stmt and connection are valid; all returned strings are
        // copied before the guard is dropped and the pointers are freed.
        unsafe {
            let z_sql = ffi::sqlite3_sql(self.stmt);
            if z_sql.is_null() {
                return Result::from_status(Status::error_message("No statement"));
            }
            let mut result = format!("Explain query {}", CStr::from_ptr(z_sql).to_string_lossy());

            let z_explain = ffi::sqlite3_mprintf(c"EXPLAIN QUERY PLAN %s".as_ptr(), z_sql);
            if z_explain.is_null() {
                return Result::from_status(self.last_error());
            }
            let mut p_explain: *mut ffi::sqlite3_stmt = std::ptr::null_mut();
            let rc = ffi::sqlite3_prepare_v2(
                connection.handle(),
                z_explain,
                -1,
                &mut p_explain,
                std::ptr::null_mut(),
            );
            ffi::sqlite3_free(z_explain.cast());
            if rc != ffi::SQLITE_OK {
                return Result::from_status(self.last_error());
            }

            loop {
                match ffi::sqlite3_step(p_explain) {
                    ffi::SQLITE_ROW => {
                        let i_selectid = ffi::sqlite3_column_int(p_explain, 0);
                        let i_order = ffi::sqlite3_column_int(p_explain, 1);
                        let i_from = ffi::sqlite3_column_int(p_explain, 2);
                        let z_detail = ffi::sqlite3_column_text(p_explain, 3);
                        let detail = if z_detail.is_null() {
                            std::borrow::Cow::Borrowed("")
                        } else {
                            CStr::from_ptr(z_detail.cast()).to_string_lossy()
                        };
                        result.push_str(&format!("\n{i_selectid} {i_order} {i_from} {detail}"));
                    }
                    ffi::SQLITE_DONE => break,
                    _ => {
                        let status = self.last_error();
                        ffi::sqlite3_finalize(p_explain);
                        return Result::from_status(status);
                    }
                }
            }
            let rc = ffi::sqlite3_finalize(p_explain);
            if rc != ffi::SQLITE_OK {
                return Result::from_status(self.last_error());
            }
            Result::Ok(result)
        }
    }

    /// `SqliteStatement::can_step`
    pub fn can_step(&self) -> bool {
        self.state != State::Finish
    }

    /// `SqliteStatement::has_row`
    pub fn has_row(&self) -> bool {
        self.state == State::HaveRow
    }

    /// `SqliteStatement::reset`
    pub fn reset(&mut self) {
        // SAFETY: stmt is valid.
        unsafe { ffi::sqlite3_reset(self.stmt) };
        self.state = State::Start;
    }
}

impl Drop for SqliteStatement {
    fn drop(&mut self) {
        // SAFETY: stmt is valid and finalized exactly once.
        unsafe { ffi::sqlite3_finalize(self.stmt) };
    }
}

// SAFETY: the stmt pointer is only used under the crate's mutexes against a
// SQLITE_THREADSAFE=1 (serialized) connection, which internally serializes
// every call on a connection — the same contract C++ tdlib relies on.
unsafe impl Send for SqliteStatement {}
