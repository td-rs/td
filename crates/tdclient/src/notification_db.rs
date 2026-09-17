//! The notification-group database layer (M.13.2): the `notification_groups`
//! table of the dialog database (DialogDb.cpp:42-52, 108, 121-142, 157-176,
//! 257-280, 302-307). The schema text and the query shapes are D1 pins,
//! copied verbatim from the C++.
//!
//! Standalone for now — the [`crate::notification_manager`] env seams
//! (`get_message_notification_group_keys_from_database` and the
//! `get_group_force` database arm) flip onto these queries when the TdDb
//! dialog-db plumbing lands with M.6; the M.6 dialog_db copies are the
//! integration dedupe candidates.
//!
//! Ported quirks:
//! - a zero `last_notification_date` is stored as SQL NULL and reads back
//!   as 0 (`get_last_notification_date`, DialogDb.cpp:302-307);
//! - the active paging query keeps the OR-chain key comparison and the
//!   `ORDER BY last_notification_date DESC, dialog_id DESC` *without* the
//!   group-id tiebreak — the C++ commented the tuple form out
//!   (DialogDb.cpp:131-136), so date-0 (NULL) rows are unreachable through
//!   paging exactly like the C++.

use tddb::sqlite_db::SqliteDb;
use tddb::sqlite_statement::{Datatype, SqliteStatement};
use tdutils::status::{Result, Status};
use tdutils::{try_result, try_status};

use crate::message_id::DialogId;
use crate::notification::{NotificationGroupId, NotificationGroupKey};

/// `create_notification_group_table` (DialogDb.cpp:42-46) + the partial
/// index (DialogDb.cpp:48-52), verbatim.
pub fn init_schema(db: &SqliteDb) -> Status {
    try_status!(db.exec(
        "CREATE TABLE IF NOT EXISTS notification_groups (notification_group_id INT4 PRIMARY KEY, dialog_id \
         INT8, last_notification_date INT4)",
    ));
    try_status!(db.exec(
        "CREATE INDEX IF NOT EXISTS notification_group_by_last_notification_date ON notification_groups \
         (last_notification_date, dialog_id, notification_group_id) WHERE last_notification_date IS NOT NULL",
    ));
    Status::ok()
}

/// The prepared statements of the notification-group family
/// (DialogDb.cpp:121-142).
pub struct NotificationGroupsDb {
    db: SqliteDb,
    add_notification_group_stmt: SqliteStatement,
    delete_notification_group_stmt: SqliteStatement,
    get_notification_groups_by_last_notification_date_stmt: SqliteStatement,
    get_notification_group_stmt: SqliteStatement,
}

impl NotificationGroupsDb {
    /// Opens the group statements over an existing database (the schema is
    /// the dialog-db init ladder's job, `init_schema` for the standalone
    /// form).
    pub fn open(db: SqliteDb) -> Result<NotificationGroupsDb> {
        try_result!(
            add_notification_group_stmt,
            db.get_statement("INSERT OR REPLACE INTO notification_groups VALUES(?1, ?2, ?3)")
        );
        try_result!(
            delete_notification_group_stmt,
            db.get_statement("DELETE FROM notification_groups WHERE notification_group_id = ?1")
        );
        try_result!(
            get_notification_groups_by_last_notification_date_stmt,
            db.get_statement(
                "SELECT notification_group_id, dialog_id, last_notification_date FROM notification_groups \
                 WHERE last_notification_date < ?1 OR (last_notification_date = ?1 \
                 AND (dialog_id < ?2 OR (dialog_id = ?2 AND notification_group_id < ?3))) ORDER BY \
                 last_notification_date DESC, dialog_id DESC LIMIT ?4",
            )
        );
        try_result!(
            get_notification_group_stmt,
            db.get_statement(
                "SELECT dialog_id, last_notification_date FROM notification_groups WHERE notification_group_id = ?1",
            )
        );
        Result::Ok(NotificationGroupsDb {
            db,
            add_notification_group_stmt,
            delete_notification_group_stmt,
            get_notification_groups_by_last_notification_date_stmt,
            get_notification_group_stmt,
        })
    }

    /// The add-dialog notification-group upsert (DialogDb.cpp:170-184) — a
    /// zero date binds NULL.
    pub fn add_notification_group(&mut self, group: NotificationGroupKey) -> Status {
        let stmt = &mut self.add_notification_group_stmt;
        try_status!(stmt.bind_int32(1, group.group_id.get()));
        try_status!(stmt.bind_int64(2, group.dialog_id.get()));
        if group.last_notification_date != 0 {
            try_status!(stmt.bind_int32(3, group.last_notification_date));
        } else {
            try_status!(stmt.bind_null(3));
        }
        try_status!(stmt.step());
        stmt.reset();
        Status::ok()
    }

    /// The reuse-path delete (DialogDb.cpp:185-190).
    pub fn delete_notification_group(&mut self, group_id: NotificationGroupId) -> Status {
        let stmt = &mut self.delete_notification_group_stmt;
        try_status!(stmt.bind_int32(1, group_id.get()));
        try_status!(stmt.step());
        stmt.reset();
        Status::ok()
    }

    /// `get_notification_groups_by_last_notification_date` (DialogDb.cpp:257-280).
    pub fn get_notification_groups_by_last_notification_date(
        &mut self,
        notification_group_key: NotificationGroupKey,
        limit: i32,
    ) -> Result<Vec<NotificationGroupKey>> {
        let stmt = &mut self.get_notification_groups_by_last_notification_date_stmt;
        try_status!(stmt.bind_int32(1, notification_group_key.last_notification_date));
        try_status!(stmt.bind_int64(2, notification_group_key.dialog_id.get()));
        try_status!(stmt.bind_int32(3, notification_group_key.group_id.get()));
        try_status!(stmt.bind_int32(4, limit));

        let mut notification_groups = Vec::new();
        try_status!(stmt.step());
        while stmt.has_row() {
            notification_groups.push(NotificationGroupKey::new(
                NotificationGroupId::new(stmt.view_int32(0)),
                DialogId::new(stmt.view_int64(1)),
                get_last_notification_date(stmt, 2),
            ));
            try_status!(stmt.step());
        }
        stmt.reset();
        Result::Ok(notification_groups)
    }

    /// `get_notification_group` (DialogDb.cpp:207-217) — "Not found" when
    /// the row is absent.
    pub fn get_notification_group(
        &mut self,
        notification_group_id: NotificationGroupId,
    ) -> Result<NotificationGroupKey> {
        let stmt = &mut self.get_notification_group_stmt;
        try_status!(stmt.bind_int32(1, notification_group_id.get()));
        try_status!(stmt.step());
        if !stmt.has_row() {
            stmt.reset();
            return Result::Err(Status::error_message("Not found"));
        }
        let result = NotificationGroupKey::new(
            notification_group_id,
            DialogId::new(stmt.view_int64(0)),
            get_last_notification_date(stmt, 1),
        );
        stmt.reset();
        Result::Ok(result)
    }

    pub fn db(&self) -> &SqliteDb {
        &self.db
    }
}

/// `get_last_notification_date` (DialogDb.cpp:302-307) — NULL reads as 0.
fn get_last_notification_date(stmt: &SqliteStatement, id: i32) -> i32 {
    if stmt.view_datatype(id) == Datatype::Null {
        return 0;
    }
    stmt.view_int32(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tddb::db_key::DbKey;

    struct TempDb {
        dir: String,
    }

    impl TempDb {
        fn new(name: &str) -> TempDb {
            let dir = std::env::temp_dir()
                .join(format!(
                    "td-rs-m13-notification-db-{name}-{}",
                    std::process::id()
                ))
                .to_string_lossy()
                .into_owned();
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDb { dir }
        }

        fn path(&self) -> String {
            std::path::Path::new(&self.dir)
                .join("dialog")
                .to_string_lossy()
                .into_owned()
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn open_db(temp: &TempDb) -> NotificationGroupsDb {
        let db = SqliteDb::open_with_key(&temp.path(), true, &DbKey::empty(), None).unwrap();
        init_schema(&db).ensure();
        NotificationGroupsDb::open(db).unwrap()
    }

    fn key(group_id: i32, dialog_id: i64, date: i32) -> NotificationGroupKey {
        NotificationGroupKey::new(
            NotificationGroupId::new(group_id),
            DialogId::new(dialog_id),
            date,
        )
    }

    #[test]
    fn group_round_trip_with_null_dates() {
        let temp = TempDb::new("roundtrip");
        let mut db = open_db(&temp);

        db.add_notification_group(key(1, 10, 100)).ensure();
        db.add_notification_group(key(2, 20, 0)).ensure();

        assert_eq!(
            db.get_notification_group(NotificationGroupId::new(1))
                .unwrap(),
            key(1, 10, 100)
        );
        assert_eq!(
            db.get_notification_group(NotificationGroupId::new(2))
                .unwrap(),
            key(2, 20, 0)
        );
        assert_eq!(
            db.get_notification_group(NotificationGroupId::new(3))
                .error()
                .message(),
            "Not found"
        );

        // upsert replaces the row
        db.add_notification_group(key(1, 11, 200)).ensure();
        assert_eq!(
            db.get_notification_group(NotificationGroupId::new(1))
                .unwrap(),
            key(1, 11, 200)
        );

        // delete
        db.delete_notification_group(NotificationGroupId::new(1))
            .ensure();
        assert!(
            db.get_notification_group(NotificationGroupId::new(1))
                .is_error()
        );
    }

    #[test]
    fn paging_query_orders_by_last_notification_date() {
        let temp = TempDb::new("paging");
        let mut db = open_db(&temp);

        for group in [
            key(1, 10, 100),
            key(2, 20, 300),
            key(3, 30, 200),
            key(4, 40, 200),
            key(5, 50, 0),
        ] {
            db.add_notification_group(group).ensure();
        }

        // from the top: descending dates; the date-0 (NULL) row is excluded
        // by the key comparison (NULL compares to nothing)
        let groups = db
            .get_notification_groups_by_last_notification_date(
                NotificationGroupKey::new(
                    NotificationGroupId::new(i32::MAX),
                    DialogId::new(i64::MAX),
                    i32::MAX,
                ),
                10,
            )
            .unwrap();
        let dates: Vec<i32> = groups
            .iter()
            .map(|group| group.last_notification_date)
            .collect();
        assert_eq!(dates, vec![300, 200, 200, 100]);
        assert_eq!(groups[1].group_id, NotificationGroupId::new(4)); // same date: higher dialog first
        assert_eq!(groups[2].group_id, NotificationGroupId::new(3));

        // paging after (200, 30, 3): only the strictly lower rows
        let groups = db
            .get_notification_groups_by_last_notification_date(key(3, 30, 200), 10)
            .unwrap();
        let ids: Vec<i32> = groups.iter().map(|group| group.group_id.get()).collect();
        assert_eq!(ids, vec![1]);

        // the limit is honored
        let groups = db
            .get_notification_groups_by_last_notification_date(
                NotificationGroupKey::new(
                    NotificationGroupId::new(i32::MAX),
                    DialogId::new(i64::MAX),
                    i32::MAX,
                ),
                2,
            )
            .unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].group_id, NotificationGroupId::new(2));
        assert_eq!(groups[1].group_id, NotificationGroupId::new(4));
    }
}
