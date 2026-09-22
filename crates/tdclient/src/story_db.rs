//! `td::StoryDb` (StoryDb.h/.cpp, M.47): the `stories` / `active_stories` /
//! `active_story_lists` tables. The schema and query texts are D1 pins,
//! copied verbatim from the C++ (StoryDb.cpp:44-62). The C++
//! sync/async/actor ladder (`StoryDbSyncInterface` → `StoryDbAsync` with the
//! 50-query/10ms write batching, StoryDb.cpp:356-567) collapses here: the
//! Rust base has no per-track db actors, so the manager calls the sync
//! statements directly (the batching seam lands with the TdDb
//! integration).

use tddb::sqlite_db::SqliteDb;
use tddb::sqlite_statement::SqliteStatement;
use tdutils::status::{Result, Status};
use tdutils::{try_result, try_status};

use crate::dialog_id::DialogId;
use crate::notification::NotificationId;
use crate::story_ids::{StoryFullId, StoryId, StoryListId, story_list_id_to_int};

/// `StoryDbStory` (StoryDb.h:27-33).
#[derive(Clone, Debug, PartialEq)]
pub struct StoryDbStory {
    pub story_full_id: StoryFullId,
    pub data: Vec<u8>,
}

/// `StoryDbGetActiveStoryListResult` (StoryDb.h:35-39).
#[derive(Clone, Debug, PartialEq)]
pub struct StoryDbGetActiveStoryListResult {
    pub active_stories: Vec<(DialogId, Vec<u8>)>,
    pub next_order: i64,
    pub next_dialog_id: DialogId,
}

/// `init_story_db` (StoryDb.cpp:30-67) — the schema text is verbatim.
pub fn init_schema(db: &SqliteDb) -> Status {
    try_status!(db.exec(
        "CREATE TABLE IF NOT EXISTS stories (dialog_id INT8, story_id INT4, expires_at INT4, notification_id \
         INT4, data BLOB, PRIMARY KEY (dialog_id, story_id))",
    ));
    try_status!(db.exec(
        "CREATE INDEX IF NOT EXISTS story_by_ttl ON stories (expires_at) WHERE expires_at IS NOT NULL",
    ));
    try_status!(db.exec(
        "CREATE INDEX IF NOT EXISTS story_by_notification_id ON stories (dialog_id, notification_id) WHERE \
         notification_id IS NOT NULL",
    ));
    try_status!(db.exec(
        "CREATE TABLE IF NOT EXISTS active_stories (dialog_id INT8 PRIMARY KEY, story_list_id INT4, \
         dialog_order INT8, data BLOB)",
    ));
    try_status!(db.exec(
        "CREATE INDEX IF NOT EXISTS active_stories_by_order ON active_stories (story_list_id, dialog_order, \
         dialog_id) WHERE story_list_id IS NOT NULL",
    ));
    try_status!(db.exec(
        "CREATE TABLE IF NOT EXISTS active_story_lists (story_list_id INT4 PRIMARY KEY, data BLOB)",
    ));
    Status::ok()
}

/// The prepared statements of the story family (StoryDb.cpp:87-126).
pub struct StoryDb {
    db: SqliteDb,
    add_story_stmt: SqliteStatement,
    delete_story_stmt: SqliteStatement,
    get_story_stmt: SqliteStatement,
    get_expiring_stories_stmt: SqliteStatement,
    get_stories_from_notification_id_stmt: SqliteStatement,
    add_active_stories_stmt: SqliteStatement,
    delete_active_stories_stmt: SqliteStatement,
    get_active_story_list_stmt: SqliteStatement,
    get_active_stories_stmt: SqliteStatement,
    add_active_story_list_state_stmt: SqliteStatement,
    get_active_story_list_state_stmt: SqliteStatement,
}

impl StoryDb {
    /// `StoryDbImpl::init` (StoryDb.cpp:87-126) — the statement texts are
    /// verbatim.
    pub fn open(db: SqliteDb) -> Result<StoryDb> {
        try_result!(
            add_story_stmt,
            db.get_statement("INSERT OR REPLACE INTO stories VALUES(?1, ?2, ?3, ?4, ?5)")
        );
        try_result!(
            delete_story_stmt,
            db.get_statement("DELETE FROM stories WHERE dialog_id = ?1 AND story_id = ?2")
        );
        try_result!(
            get_story_stmt,
            db.get_statement("SELECT data FROM stories WHERE dialog_id = ?1 AND story_id = ?2")
        );
        try_result!(
            get_expiring_stories_stmt,
            db.get_statement(
                "SELECT dialog_id, story_id, data FROM stories WHERE expires_at <= ?1 LIMIT ?2"
            )
        );
        try_result!(
            get_stories_from_notification_id_stmt,
            db.get_statement(
                "SELECT story_id, data FROM stories WHERE dialog_id = ?1 AND notification_id < ?2 \
                 ORDER BY notification_id DESC LIMIT ?3"
            )
        );
        try_result!(
            add_active_stories_stmt,
            db.get_statement("INSERT OR REPLACE INTO active_stories VALUES(?1, ?2, ?3, ?4)")
        );
        try_result!(
            delete_active_stories_stmt,
            db.get_statement("DELETE FROM active_stories WHERE dialog_id = ?1")
        );
        try_result!(
            get_active_story_list_stmt,
            db.get_statement(
                "SELECT data, dialog_id, dialog_order FROM active_stories WHERE story_list_id = ?1 AND \
                 (dialog_order < ?2 OR (dialog_order = ?2 AND dialog_id < ?3)) ORDER BY dialog_order DESC, \
                 dialog_id DESC LIMIT ?4"
            )
        );
        try_result!(
            get_active_stories_stmt,
            db.get_statement("SELECT data FROM active_stories WHERE dialog_id = ?1")
        );
        try_result!(
            add_active_story_list_state_stmt,
            db.get_statement("INSERT OR REPLACE INTO active_story_lists VALUES(?1, ?2)")
        );
        try_result!(
            get_active_story_list_state_stmt,
            db.get_statement("SELECT data FROM active_story_lists WHERE story_list_id = ?1")
        );
        Result::Ok(StoryDb {
            db,
            add_story_stmt,
            delete_story_stmt,
            get_story_stmt,
            get_expiring_stories_stmt,
            get_stories_from_notification_id_stmt,
            add_active_stories_stmt,
            delete_active_stories_stmt,
            get_active_story_list_stmt,
            get_active_stories_stmt,
            add_active_story_list_state_stmt,
            get_active_story_list_state_stmt,
        })
    }

    /// `StoryDbImpl::add_story` (StoryDb.cpp:128-152) — a zero `expires_at`
    /// and an invalid notification id bind SQL NULL.
    pub fn add_story(
        &mut self,
        story_full_id: StoryFullId,
        expires_at: i32,
        notification_id: NotificationId,
        data: &[u8],
    ) -> Status {
        let stmt = &mut self.add_story_stmt;
        try_status!(stmt.bind_int64(1, story_full_id.get_dialog_id().get()));
        try_status!(stmt.bind_int32(2, story_full_id.get_story_id().get()));
        if expires_at != 0 {
            try_status!(stmt.bind_int32(3, expires_at));
        } else {
            try_status!(stmt.bind_null(3));
        }
        if notification_id.is_valid() {
            try_status!(stmt.bind_int32(4, notification_id.get()));
        } else {
            try_status!(stmt.bind_null(4));
        }
        try_status!(stmt.bind_blob(5, data));
        try_status!(stmt.step());
        stmt.reset();
        Status::ok()
    }

    /// `StoryDbImpl::delete_story` (StoryDb.cpp:154-165).
    pub fn delete_story(&mut self, story_full_id: StoryFullId) -> Status {
        let stmt = &mut self.delete_story_stmt;
        try_status!(stmt.bind_int64(1, story_full_id.get_dialog_id().get()));
        try_status!(stmt.bind_int32(2, story_full_id.get_story_id().get()));
        try_status!(stmt.step());
        stmt.reset();
        Status::ok()
    }

    /// `StoryDbImpl::get_story` (StoryDb.cpp:167-182) — "Not found" when
    /// the row is absent.
    pub fn get_story(&mut self, story_full_id: StoryFullId) -> Result<Vec<u8>> {
        let stmt = &mut self.get_story_stmt;
        try_status!(stmt.bind_int64(1, story_full_id.get_dialog_id().get()));
        try_status!(stmt.bind_int32(2, story_full_id.get_story_id().get()));
        try_status!(stmt.step());
        if !stmt.has_row() {
            stmt.reset();
            return Result::Err(Status::error_message("Not found"));
        }
        let result = stmt.view_blob(0).to_vec();
        stmt.reset();
        Result::Ok(result)
    }

    /// `StoryDbImpl::get_expiring_stories` (StoryDb.cpp:184-204).
    pub fn get_expiring_stories(&mut self, expires_till: i32, limit: i32) -> Vec<StoryDbStory> {
        let stmt = &mut self.get_expiring_stories_stmt;
        let _ = stmt.bind_int32(1, expires_till);
        let _ = stmt.bind_int32(2, limit);
        let _ = stmt.step();

        let mut stories = Vec::new();
        while stmt.has_row() {
            let dialog_id = DialogId::new(stmt.view_int64(0));
            let story_id = StoryId::new(stmt.view_int32(1));
            let data = stmt.view_blob(2).to_vec();
            stories.push(StoryDbStory {
                story_full_id: StoryFullId::new(dialog_id, story_id),
                data,
            });
            let _ = stmt.step();
        }
        stmt.reset();
        stories
    }

    /// `StoryDbImpl::get_stories_from_notification_id` (StoryDb.cpp:206-225).
    pub fn get_stories_from_notification_id(
        &mut self,
        dialog_id: DialogId,
        from_notification_id: NotificationId,
        limit: i32,
    ) -> Vec<StoryDbStory> {
        let stmt = &mut self.get_stories_from_notification_id_stmt;
        let _ = stmt.bind_int64(1, dialog_id.get());
        let _ = stmt.bind_int32(2, from_notification_id.get());
        let _ = stmt.bind_int32(3, limit);
        let _ = stmt.step();

        let mut stories = Vec::new();
        while stmt.has_row() {
            let story_id = StoryId::new(stmt.view_int32(0));
            let data = stmt.view_blob(1).to_vec();
            stories.push(StoryDbStory {
                story_full_id: StoryFullId::new(dialog_id, story_id),
                data,
            });
            let _ = stmt.step();
        }
        stmt.reset();
        stories
    }

    /// `StoryDbImpl::add_active_stories` (StoryDb.cpp:227-240).
    pub fn add_active_stories(
        &mut self,
        dialog_id: DialogId,
        story_list_id: StoryListId,
        dialog_order: i64,
        data: &[u8],
    ) -> Status {
        let stmt = &mut self.add_active_stories_stmt;
        try_status!(stmt.bind_int64(1, dialog_id.get()));
        if story_list_id.is_valid() {
            try_status!(stmt.bind_int32(2, story_list_id_to_int(story_list_id)));
        } else {
            try_status!(stmt.bind_null(2));
        }
        try_status!(stmt.bind_int64(3, dialog_order));
        try_status!(stmt.bind_blob(4, data));
        try_status!(stmt.step());
        stmt.reset();
        Status::ok()
    }

    /// `StoryDbImpl::delete_active_stories` (StoryDb.cpp:242-248).
    pub fn delete_active_stories(&mut self, dialog_id: DialogId) -> Status {
        let stmt = &mut self.delete_active_stories_stmt;
        try_status!(stmt.bind_int64(1, dialog_id.get()));
        try_status!(stmt.step());
        stmt.reset();
        Status::ok()
    }

    /// `StoryDbImpl::get_active_stories` (StoryDb.cpp:250-261).
    pub fn get_active_stories(&mut self, dialog_id: DialogId) -> Result<Vec<u8>> {
        let stmt = &mut self.get_active_stories_stmt;
        try_status!(stmt.bind_int64(1, dialog_id.get()));
        try_status!(stmt.step());
        if !stmt.has_row() {
            stmt.reset();
            return Result::Err(Status::error_message("Not found"));
        }
        let result = stmt.view_blob(0).to_vec();
        stmt.reset();
        Result::Ok(result)
    }

    /// `StoryDbImpl::get_active_story_list` (StoryDb.cpp:263-288).
    pub fn get_active_story_list(
        &mut self,
        story_list_id: StoryListId,
        order: i64,
        dialog_id: DialogId,
        limit: i32,
    ) -> StoryDbGetActiveStoryListResult {
        let stmt = &mut self.get_active_story_list_stmt;
        if story_list_id.is_valid() {
            let _ = stmt.bind_int32(1, story_list_id_to_int(story_list_id));
        } else {
            // The C++ binds 0 here (StoryDb.cpp:269) but never queries an
            // invalid list — the manager rejects it first. Binding NULL
            // keeps the invalid-list query empty instead of aliasing the
            // main list.
            let _ = stmt.bind_null(1);
        }
        let _ = stmt.bind_int64(2, order);
        let _ = stmt.bind_int64(3, dialog_id.get());
        let _ = stmt.bind_int32(4, limit);

        let mut result = StoryDbGetActiveStoryListResult {
            active_stories: Vec::new(),
            next_order: order,
            next_dialog_id: dialog_id,
        };
        let _ = stmt.step();
        while stmt.has_row() {
            let data = stmt.view_blob(0).to_vec();
            result.next_dialog_id = DialogId::new(stmt.view_int64(1));
            result.next_order = stmt.view_int64(2);
            result.active_stories.push((result.next_dialog_id, data));
            let _ = stmt.step();
        }
        stmt.reset();
        result
    }

    /// `StoryDbImpl::add_active_story_list_state` (StoryDb.cpp:290-297).
    pub fn add_active_story_list_state(
        &mut self,
        story_list_id: StoryListId,
        data: &[u8],
    ) -> Status {
        let stmt = &mut self.add_active_story_list_state_stmt;
        try_status!(stmt.bind_int32(1, story_list_id_to_int(story_list_id)));
        try_status!(stmt.bind_blob(2, data));
        try_status!(stmt.step());
        stmt.reset();
        Status::ok()
    }

    /// `StoryDbImpl::get_active_story_list_state` (StoryDb.cpp:299-310).
    pub fn get_active_story_list_state(&mut self, story_list_id: StoryListId) -> Result<Vec<u8>> {
        let stmt = &mut self.get_active_story_list_state_stmt;
        try_status!(stmt.bind_int32(1, story_list_id_to_int(story_list_id)));
        try_status!(stmt.step());
        if !stmt.has_row() {
            stmt.reset();
            return Result::Err(Status::error_message("Not found"));
        }
        let result = stmt.view_blob(0).to_vec();
        stmt.reset();
        Result::Ok(result)
    }

    /// `StoryDbImpl::begin_write_transaction` (StoryDb.cpp:312-314).
    pub fn begin_write_transaction(&self) -> Status {
        self.db.begin_write_transaction()
    }

    /// `StoryDbImpl::commit_transaction` (StoryDb.cpp:315-317).
    pub fn commit_transaction(&self) -> Status {
        self.db.commit_transaction()
    }

    pub fn db(&self) -> &SqliteDb {
        &self.db
    }
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
                .join(format!("td-rs-m47-story-db-{name}-{}", std::process::id()))
                .to_string_lossy()
                .into_owned();
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDb { dir }
        }

        fn path(&self) -> String {
            std::path::Path::new(&self.dir)
                .join("story")
                .to_string_lossy()
                .into_owned()
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn open_db(temp: &TempDb) -> StoryDb {
        let db = SqliteDb::open_with_key(&temp.path(), true, &DbKey::empty(), None).unwrap();
        init_schema(&db).ensure();
        StoryDb::open(db).unwrap()
    }

    fn sid(id: i32) -> StoryId {
        StoryId::new(id)
    }

    fn sfull(dialog_id: i64, story_id: i32) -> StoryFullId {
        StoryFullId::new(DialogId::new(dialog_id), sid(story_id))
    }

    #[test]
    fn story_rows_round_trip() {
        let temp = TempDb::new("rows");
        let mut db = open_db(&temp);

        db.add_story(sfull(100, 5), 1234, NotificationId::new(77), b"payload-5")
            .ensure();
        // the zero expiry binds NULL; the invalid notification id binds NULL
        db.add_story(sfull(100, 6), 0, NotificationId::default(), b"payload-6")
            .ensure();

        assert_eq!(db.get_story(sfull(100, 5)).unwrap(), b"payload-5".to_vec());
        assert_eq!(db.get_story(sfull(100, 6)).unwrap(), b"payload-6".to_vec());
        assert_eq!(db.get_story(sfull(100, 7)).error().message(), "Not found");
        assert_eq!(db.get_story(sfull(101, 5)).error().message(), "Not found");

        // upsert replaces
        db.add_story(sfull(100, 5), 4321, NotificationId::new(78), b"payload-5b")
            .ensure();
        assert_eq!(db.get_story(sfull(100, 5)).unwrap(), b"payload-5b".to_vec());

        db.delete_story(sfull(100, 5)).ensure();
        assert!(db.get_story(sfull(100, 5)).is_error());
    }

    #[test]
    fn expiring_stories_scan_orders_by_ttl() {
        let temp = TempDb::new("expiring");
        let mut db = open_db(&temp);

        db.add_story(sfull(1, 10), 100, NotificationId::default(), b"a")
            .ensure();
        db.add_story(sfull(2, 20), 200, NotificationId::default(), b"b")
            .ensure();
        db.add_story(sfull(3, 30), 300, NotificationId::default(), b"c")
            .ensure();
        // no expiry → never expires
        db.add_story(sfull(4, 40), 0, NotificationId::default(), b"d")
            .ensure();

        let stories = db.get_expiring_stories(250, 10);
        assert_eq!(stories.len(), 2);
        assert_eq!(stories[0].story_full_id, sfull(1, 10));
        assert_eq!(stories[1].story_full_id, sfull(2, 20));

        let stories = db.get_expiring_stories(250, 1);
        assert_eq!(stories.len(), 1);
        assert_eq!(stories[0].story_full_id, sfull(1, 10));
    }

    #[test]
    fn active_stories_round_trip_and_paging() {
        let temp = TempDb::new("active");
        let mut db = open_db(&temp);

        db.add_active_stories(DialogId::new(10), StoryListId::Main, 500, b"list-10")
            .ensure();
        db.add_active_stories(DialogId::new(20), StoryListId::Archive, 700, b"list-20")
            .ensure();
        db.add_active_stories(DialogId::new(30), StoryListId::None, 900, b"list-30")
            .ensure();

        assert_eq!(
            db.get_active_stories(DialogId::new(20)).unwrap(),
            b"list-20".to_vec()
        );

        // from the top: both valid rows, descending order
        let result =
            db.get_active_story_list(StoryListId::Main, i64::MAX, DialogId::new(i64::MAX), 10);
        assert_eq!(result.active_stories.len(), 1);
        assert_eq!(result.active_stories[0].0, DialogId::new(10));
        assert_eq!(result.active_stories[0].1, b"list-10".to_vec());
        assert_eq!(result.next_order, 500);
        assert_eq!(result.next_dialog_id, DialogId::new(10));

        // paging after (500, 10) sees nothing in the main list
        let result = db.get_active_story_list(StoryListId::Main, 500, DialogId::new(10), 10);
        assert!(result.active_stories.is_empty());

        // the archive list is separate
        let result =
            db.get_active_story_list(StoryListId::Archive, i64::MAX, DialogId::new(i64::MAX), 10);
        assert_eq!(result.active_stories.len(), 1);
        assert_eq!(result.active_stories[0].0, DialogId::new(20));

        // the invalid list is in neither
        let result =
            db.get_active_story_list(StoryListId::None, i64::MAX, DialogId::new(i64::MAX), 10);
        assert!(result.active_stories.is_empty());

        db.delete_active_stories(DialogId::new(20)).ensure();
        assert!(db.get_active_stories(DialogId::new(20)).is_error());
    }

    #[test]
    fn story_list_state_round_trip() {
        let temp = TempDb::new("list-state");
        let mut db = open_db(&temp);

        assert_eq!(
            db.get_active_story_list_state(StoryListId::Main)
                .error()
                .message(),
            "Not found"
        );
        db.add_active_story_list_state(StoryListId::Main, b"state-main")
            .ensure();
        db.add_active_story_list_state(StoryListId::Archive, b"state-archive")
            .ensure();
        assert_eq!(
            db.get_active_story_list_state(StoryListId::Main).unwrap(),
            b"state-main".to_vec()
        );
        assert_eq!(
            db.get_active_story_list_state(StoryListId::Archive)
                .unwrap(),
            b"state-archive".to_vec()
        );
        // upsert replaces
        db.add_active_story_list_state(StoryListId::Main, b"state-main-2")
            .ensure();
        assert_eq!(
            db.get_active_story_list_state(StoryListId::Main).unwrap(),
            b"state-main-2".to_vec()
        );
    }

    #[test]
    fn transactions_commit_writes() {
        let temp = TempDb::new("txn");
        let mut db = open_db(&temp);

        db.begin_write_transaction().ensure();
        db.add_story(sfull(9, 1), 0, NotificationId::default(), b"txn-payload")
            .ensure();
        db.commit_transaction().ensure();
        assert_eq!(db.get_story(sfull(9, 1)).unwrap(), b"txn-payload".to_vec());
    }
}
