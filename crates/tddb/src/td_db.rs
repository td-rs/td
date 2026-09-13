//! Port of `td/telegram/TdDb.cpp` `init_sqlite` (schema skeleton): opens the
//! sqlite database, creates the tdlib schema (dialogs, messages, stories,
//! files) and the `user_version`-based migration scaffolding.
//!
//! SQL strings are byte-identical with the C++ source, including the FTS5
//! tokenizer's embedded 0x07 `tokenchars` byte.

use std::sync::Arc;

use crate::db_key::DbKey;
use crate::sqlite_connection_safe::SqliteConnectionSafe;
use crate::sqlite_db::SqliteDb;
use crate::sqlite_key_value::SqliteKeyValue;
use tdutils::status::Result;
use tdutils::status::Status;
use tdutils::try_status;

/// `DbVersion` (td/telegram/Version.h)
pub mod db_version {
    pub const CREATE_DIALOG_DB: i32 = 3;
    pub const ADD_MESSAGE_DB_MEDIA_INDEX: i32 = 4;
    pub const ADD_MESSAGE_DB30_MEDIA_INDEX: i32 = 5;
    pub const ADD_MESSAGE_DB_FTS: i32 = 6;
    pub const ADD_MESSAGES_CALL_INDEX: i32 = 7;
    pub const FIX_FILE_REMOTE_LOCATION_KEY_BUG: i32 = 8;
    pub const ADD_NOTIFICATIONS_SUPPORT: i32 = 9;
    pub const ADD_FOLDERS: i32 = 10;
    pub const ADD_SCHEDULED_MESSAGES: i32 = 11;
    pub const STORE_PINNED_DIALOGS_IN_BINLOG: i32 = 12;
    pub const ADD_MESSAGE_THREAD_SUPPORT: i32 = 13;
    pub const ADD_MESSAGE_THREAD_DATABASE: i32 = 14;
    pub const NEXT: i32 = 15;
}

/// `current_db_version()`
pub const CURRENT_DB_VERSION: i32 = db_version::NEXT - 1;

/// `MESSAGE_DB_INDEX_COUNT` (td/telegram/MessageDb.cpp)
const MESSAGE_DB_INDEX_COUNT: i32 = 30;
#[allow(dead_code)]
const MESSAGE_DB_INDEX_COUNT_OLD: i32 = 9;

/// `MessageSearchFilter::Call` / `MissedCall`
const MESSAGE_SEARCH_FILTER_CALL: i32 = 10;
const MESSAGE_SEARCH_FILTER_MISSED_CALL: i32 = 11;

/// `TdDb::Parameters` (the database-relevant subset)
#[derive(Clone, Debug)]
pub struct Parameters {
    pub use_file_database: bool,
    pub use_chat_info_database: bool,
    pub use_message_database: bool,
}

impl Default for Parameters {
    fn default() -> Self {
        Parameters {
            use_file_database: true,
            use_chat_info_database: true,
            use_message_database: true,
        }
    }
}

/// `TdDb` — the schema skeleton: owns the safe connection and the file
/// key-value table; manager-specific handles attach in later steps.
pub struct TdDb {
    sql_connection: Option<Arc<SqliteConnectionSafe>>,
    files_kv: Option<SqliteKeyValue>,
}

fn add_media_indices(db: &SqliteDb, begin: i32, end: i32) -> Status {
    for i in begin..end {
        try_status!(
            db.exec(&format!(
                "CREATE INDEX IF NOT EXISTS message_index_{i} ON messages (dialog_id, message_id) WHERE (index_mask & {}) != 0",
                1 << i
            ))
        );
    }
    Status::ok()
}

fn add_fts(db: &SqliteDb) -> Status {
    try_status!(
        db.exec("CREATE INDEX IF NOT EXISTS message_by_search_id ON messages (search_id) WHERE search_id IS NOT NULL")
    );
    // C++ embeds a literal 0x07 byte in the tokenizer options
    try_status!(
        db.exec("CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(text, content='messages', \
                 content_rowid='search_id', tokenize = \"unicode61 remove_diacritics 0 tokenchars '\u{07}'\")")
    );
    try_status!(
        db.exec("CREATE TRIGGER IF NOT EXISTS trigger_fts_delete BEFORE DELETE ON messages WHEN OLD.search_id IS NOT NULL BEGIN INSERT INTO messages_fts(messages_fts, rowid, text) VALUES('delete', OLD.search_id, OLD.text); END")
    );
    try_status!(
        db.exec("CREATE TRIGGER IF NOT EXISTS trigger_fts_insert AFTER INSERT ON messages WHEN NEW.search_id IS NOT NULL BEGIN INSERT INTO messages_fts(rowid, text) VALUES(NEW.search_id, NEW.text); END")
    );
    Status::ok()
}

fn add_call_index(db: &SqliteDb) -> Status {
    for i in MESSAGE_SEARCH_FILTER_CALL - 1..MESSAGE_SEARCH_FILTER_MISSED_CALL {
        try_status!(
            db.exec(&format!(
                "CREATE INDEX IF NOT EXISTS full_message_index_{i} ON messages (unique_message_id) WHERE (index_mask & {}) != 0",
                1 << i
            ))
        );
    }
    Status::ok()
}

fn add_notification_id_index(db: &SqliteDb) -> Status {
    db.exec(
        "CREATE INDEX IF NOT EXISTS message_by_notification_id ON messages (dialog_id, notification_id) WHERE \
         notification_id IS NOT NULL",
    )
}

fn add_scheduled_messages_table(db: &SqliteDb) -> Status {
    try_status!(db.exec(
        "CREATE TABLE IF NOT EXISTS scheduled_messages (dialog_id INT8, message_id INT8, \
         server_message_id INT4, data BLOB, PRIMARY KEY (dialog_id, message_id))"
    ));
    db.exec(
        "CREATE INDEX IF NOT EXISTS message_by_server_message_id ON scheduled_messages \
         (dialog_id, server_message_id) WHERE server_message_id IS NOT NULL",
    )
}

/// `init_dialog_db` (fresh-database path; C++ migrations are version-gated)
fn init_dialog_db(db: &SqliteDb, version: &mut i32) -> Status {
    let has_table = db.has_table("dialogs").unwrap();
    let mut version_local = *version;
    if !has_table || version_local > CURRENT_DB_VERSION {
        // drop_dialog_db (td/telegram/DialogDb.cpp) — future migrations
        version_local = 0;
    }

    if version_local == 0 {
        try_status!(
            db.exec("CREATE TABLE IF NOT EXISTS dialogs (dialog_id INT8 PRIMARY KEY, dialog_order INT8, data BLOB, \
                     folder_id INT4)")
        );
        try_status!(
            db.exec("CREATE TABLE IF NOT EXISTS notification_groups (notification_group_id INT4 PRIMARY KEY, dialog_id \
                     INT8, last_notification_date INT4)")
        );
        try_status!(
            db.exec("CREATE INDEX IF NOT EXISTS notification_group_by_last_notification_date ON notification_groups \
                     (last_notification_date, dialog_id, notification_group_id) WHERE last_notification_date IS NOT NULL")
        );
        try_status!(
            db.exec("CREATE INDEX IF NOT EXISTS dialog_in_folder_by_dialog_order ON dialogs (folder_id, dialog_order, dialog_id) \
                     WHERE folder_id IS NOT NULL")
        );
        version_local = CURRENT_DB_VERSION;
    }
    *version = version_local;
    Status::ok()
}

/// `init_message_db`
fn init_message_db(db: &SqliteDb, version: &mut i32) -> Status {
    let has_table = db.has_table("messages").unwrap();
    let mut version_local = *version;
    if !has_table || version_local > CURRENT_DB_VERSION {
        version_local = 0;
    }

    if version_local == 0 {
        try_status!(
            db.exec("CREATE TABLE IF NOT EXISTS messages (dialog_id INT8, message_id INT8, unique_message_id INT4, \
                     sender_user_id INT8, random_id INT8, data BLOB, ttl_expires_at INT4, index_mask INT4, search_id INT8, \
                     text STRING, notification_id INT4, top_thread_message_id INT8, PRIMARY KEY (dialog_id, message_id))")
        );
        try_status!(db.exec(
            "CREATE INDEX IF NOT EXISTS message_by_random_id ON messages (dialog_id, random_id) \
                     WHERE random_id IS NOT NULL"
        ));
        try_status!(db.exec(
            "CREATE INDEX IF NOT EXISTS message_by_unique_message_id ON messages \
                     (unique_message_id) WHERE unique_message_id IS NOT NULL"
        ));
        try_status!(db.exec(
            "CREATE INDEX IF NOT EXISTS message_by_ttl ON messages \
                     (ttl_expires_at) WHERE ttl_expires_at IS NOT NULL"
        ));
        try_status!(add_media_indices(db, 0, MESSAGE_DB_INDEX_COUNT));
        try_status!(add_fts(db));
        try_status!(add_call_index(db));
        try_status!(add_notification_id_index(db));
        try_status!(add_scheduled_messages_table(db));
        version_local = CURRENT_DB_VERSION;
    }
    *version = version_local;
    Status::ok()
}

/// `init_story_db`
fn init_story_db(db: &SqliteDb, version: &mut i32) -> Status {
    let has_stories_table = db.has_table("stories").unwrap();
    let has_active_stories_table = db.has_table("active_stories").unwrap();
    let has_story_list_table = db.has_table("active_story_lists").unwrap();
    let mut version_local = *version;
    if (!has_stories_table || !has_active_stories_table || !has_story_list_table)
        || version_local > CURRENT_DB_VERSION
    {
        // drop_story_db (td/telegram/StoryDb.cpp) — future migrations
        version_local = 0;
    }

    if version_local == 0 {
        try_status!(
            db.exec("CREATE TABLE IF NOT EXISTS stories (dialog_id INT8, story_id INT4, expires_at INT4, notification_id \
                     INT4, data BLOB, PRIMARY KEY (dialog_id, story_id))")
        );
        try_status!(
            db.exec("CREATE INDEX IF NOT EXISTS story_by_ttl ON stories (expires_at) WHERE expires_at IS NOT NULL")
        );
        try_status!(
            db.exec("CREATE INDEX IF NOT EXISTS story_by_notification_id ON stories (dialog_id, notification_id) WHERE \
                     notification_id IS NOT NULL")
        );
        try_status!(
            db.exec("CREATE TABLE IF NOT EXISTS active_stories (dialog_id INT8 PRIMARY KEY, story_list_id INT4, \
                     dialog_order INT8, data BLOB)")
        );
        try_status!(
            db.exec("CREATE INDEX IF NOT EXISTS active_stories_by_order ON active_stories (story_list_id, dialog_order, \
                     dialog_id) WHERE story_list_id IS NOT NULL")
        );
        try_status!(
            db.exec("CREATE TABLE IF NOT EXISTS active_story_lists (story_list_id INT4 PRIMARY KEY, data BLOB)")
        );
        version_local = CURRENT_DB_VERSION;
    }
    *version = version_local;
    Status::ok()
}

/// `init_file_db`
fn init_file_db(db: &SqliteDb, version: &mut i32) -> Status {
    let has_table = db.has_table("files").unwrap();
    let mut version_local = *version;
    if !has_table || version_local < db_version::FIX_FILE_REMOTE_LOCATION_KEY_BUG {
        version_local = 0;
    }
    if version_local == 0 {
        try_status!(SqliteKeyValue::init(db, "files"));
    }
    *version = version_local;
    Status::ok()
}

impl TdDb {
    pub fn new() -> TdDb {
        TdDb {
            sql_connection: None,
            files_kv: None,
        }
    }

    /// `TdDb::init_sqlite` (skeleton): open, WAL, secure_delete, create the
    /// schema inside one transaction, set `user_version`.
    pub fn init_sqlite(
        &mut self,
        sql_database_path: &str,
        parameters: &Parameters,
        key: &DbKey,
        old_key: &DbKey,
    ) -> Status {
        let use_sqlite = parameters.use_file_database;
        let use_dialog_db = parameters.use_message_database;
        // C++: parameters.use_message_database && false — intentionally off
        let use_message_thread_db = false;
        let use_story_database = parameters.use_message_database;

        if !use_sqlite {
            let _ = crate::sqlite_db::destroy(sql_database_path);
            return Status::ok();
        }

        let db_instance = match SqliteDb::change_key(sql_database_path, true, key, old_key) {
            Result::Ok(db) => db,
            Result::Err(status) => return status,
        };
        let cipher_version = db_instance.get_cipher_version();
        let connection = Arc::new(SqliteConnectionSafe::new(sql_database_path, key.clone()));
        connection.set(db_instance);
        let db = connection.get();
        let _ = cipher_version;
        try_status!(db.exec("PRAGMA journal_mode=WAL"));
        try_status!(db.exec("PRAGMA secure_delete=1"));

        // Must be in a transaction
        try_status!(db.exec("BEGIN TRANSACTION"));

        let user_version = db.user_version().unwrap();

        if use_dialog_db {
            let mut version = user_version;
            try_status!(init_dialog_db(&db, &mut version));
        }
        if use_message_thread_db {
            let mut version = user_version;
            try_status!(init_message_thread_db_skeleton(&db, &mut version));
        }
        if parameters.use_message_database {
            let mut version = user_version;
            try_status!(init_message_db(&db, &mut version));
        }
        if use_story_database {
            let mut version = user_version;
            try_status!(init_story_db(&db, &mut version));
        }
        if parameters.use_file_database {
            let mut version = user_version;
            try_status!(init_file_db(&db, &mut version));
        }

        // Update 'PRAGMA user_version'
        if CURRENT_DB_VERSION != user_version {
            try_status!(db.set_user_version(CURRENT_DB_VERSION));
        }

        try_status!(db.exec("COMMIT TRANSACTION"));

        self.sql_connection = Some(connection.clone());

        // files key-value handle (used by the file manager in later steps)
        let mut files_kv = SqliteKeyValue::new();
        try_status!(files_kv.init_with_connection(connection.get(), "files"));
        self.files_kv = Some(files_kv);

        Status::ok()
    }

    pub fn close(&mut self) {
        self.files_kv.take();
        if let Some(connection) = self.sql_connection.take() {
            connection.close();
        }
    }
}

impl Default for TdDb {
    fn default() -> Self {
        Self::new()
    }
}

/// `init_message_thread_db` — kept for parity; `use_message_thread_db` is
/// disabled in current tdlib (`&& false`), so this never runs there either.
fn init_message_thread_db_skeleton(db: &SqliteDb, version: &mut i32) -> Status {
    let has_table = db.has_table("threads").unwrap();
    let mut version_local = *version;
    if !has_table || version_local > CURRENT_DB_VERSION {
        version_local = 0;
    }
    if version_local == 0 {
        try_status!(
            db.exec("CREATE TABLE IF NOT EXISTS threads (dialog_id INT8, thread_id INT8, thread_order INT8, data BLOB, \
                     PRIMARY KEY (dialog_id, thread_id))")
        );
        try_status!(
            db.exec("CREATE INDEX IF NOT EXISTS dialog_threads_by_thread_order ON threads (dialog_id, thread_order)")
        );
        version_local = CURRENT_DB_VERSION;
    }
    *version = version_local;
    Status::ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_dir() -> String {
        format!("{}/tests/data", env!("CARGO_MANIFEST_DIR"))
    }

    /// S44 verify: the schema created by the Rust port is identical to the
    /// schema created by the C++ implementation (conformance/cpp/
    /// td_db_create_sample.cpp, db version 14, message threads disabled).
    #[test]
    fn tddb_schema_matches_cpp() {
        fn dump_schema(path: &str) -> Vec<(String, String)> {
            let db = SqliteDb::open_with_key(path, false, &DbKey::empty(), None).unwrap();
            let mut stmt = db
                .get_statement(
                    "SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type, name",
                )
                .unwrap();
            let mut schema = Vec::new();
            loop {
                stmt.step().ensure();
                if !stmt.has_row() {
                    break;
                }
                schema.push((
                    String::from_utf8_lossy(stmt.view_blob(0)).into_owned(),
                    String::from_utf8_lossy(stmt.view_blob(1)).into_owned()
                        + "\u{1}"
                        + String::from_utf8_lossy(stmt.view_blob(2)).as_ref(),
                ));
            }
            schema
        }

        // 1. C++-created fixture
        let cpp_path = format!("{}/sample_cpp_tddb.db", data_dir());
        let cpp_schema = dump_schema(&cpp_path);
        assert!(
            cpp_schema.len() > 40,
            "fixture must contain the full tdlib schema, got {} entries",
            cpp_schema.len()
        );

        // 2. Rust-created database
        let rs_path = format!("{}/test_tddb_rs", data_dir());
        let _ = crate::sqlite_db::destroy(&rs_path);
        let mut tddb = TdDb::new();
        tddb.init_sqlite(
            &rs_path,
            &Parameters::default(),
            &DbKey::empty(),
            &DbKey::empty(),
        )
        .ensure();
        tddb.close();
        let rs_schema = dump_schema(&rs_path);

        assert_eq!(cpp_schema, rs_schema, "schemas must be identical");

        // user_version must match too
        let db = SqliteDb::open_with_key(&rs_path, false, &DbKey::empty(), None).unwrap();
        assert_eq!(db.user_version().unwrap(), 14);
        let _ = crate::sqlite_db::destroy(&rs_path);
    }

    #[test]
    fn tddb_opens_with_encryption() {
        let path = format!("{}/test_tddb_enc", data_dir());
        let _ = crate::sqlite_db::destroy(&path);
        let mut tddb = TdDb::new();
        tddb.init_sqlite(
            &path,
            &Parameters::default(),
            &DbKey::password("cucumber"),
            &DbKey::empty(),
        )
        .ensure();
        tddb.close();

        // reopen with the right key works, wrong key fails
        let mut tddb = TdDb::new();
        tddb.init_sqlite(
            &path,
            &Parameters::default(),
            &DbKey::password("cucumber"),
            &DbKey::empty(),
        )
        .ensure();
        tddb.close();
        let mut tddb = TdDb::new();
        assert!(
            tddb.init_sqlite(
                &path,
                &Parameters::default(),
                &DbKey::password("tomato"),
                &DbKey::empty(),
            )
            .is_error()
        );
        let _ = crate::sqlite_db::destroy(&path);
    }
}
