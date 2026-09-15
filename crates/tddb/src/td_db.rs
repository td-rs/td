//! Port of `td/telegram/TdDb.cpp` `init_sqlite` (schema skeleton): opens the
//! sqlite database, creates the tdlib schema (dialogs, messages, stories,
//! files) and the `user_version`-based migration scaffolding.
//!
//! SQL strings are byte-identical with the C++ source, including the FTS5
//! tokenizer's embedded 0x07 `tokenchars` byte.

use std::sync::Arc;

use crate::binlog::concurrent::ConcurrentBinlog;
use crate::binlog::event::BinlogEvent;
use crate::binlog_key_value::BinlogKeyValue;
use crate::db_key::DbKey;
use crate::sqlite_connection_safe::SqliteConnectionSafe;
use crate::sqlite_db::SqliteDb;
use crate::sqlite_key_value::SqliteKeyValue;
use tdutils::random;
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

/// `TdDb::Parameters`
#[derive(Clone, Debug)]
pub struct Parameters {
    pub encryption_key: DbKey,
    pub database_directory: String,
    pub files_directory: String,
    pub is_test_dc: bool,
    pub use_file_database: bool,
    pub use_chat_info_database: bool,
    pub use_message_database: bool,
}

impl Default for Parameters {
    fn default() -> Self {
        Parameters {
            encryption_key: DbKey::empty(),
            database_directory: String::new(),
            files_directory: String::new(),
            is_test_dc: false,
            use_file_database: true,
            use_chat_info_database: true,
            use_message_database: true,
        }
    }
}

/// `LogEvent::HandlerType::BinlogPmcMagic` (logevent/LogEvent.h).
pub const BINLOG_PMC_MAGIC: i32 = 0x4327;
/// `LogEvent::HandlerType::ConfigPmcMagic` (logevent/LogEvent.h).
pub const CONFIG_PMC_MAGIC: i32 = 0x1f18;
/// `LogEvent::HandlerType::Users` (logevent/LogEvent.h:65) — the raw
/// user-cache log events the `UserManager` mirror writes while a user is
/// unsaved (`UserLogEvent`). `SecretChats` = 1 arrives with its manager.
pub const LOG_EVENT_HANDLER_TYPE_USERS: i32 = 2;
/// `LogEvent::HandlerType::SecretChatInfos` (logevent/LogEvent.h:68) — the
/// raw secret-chat-cache log events the `UserManager` mirror writes while a
/// secret chat is unsaved (`SecretChatLogEvent`). Distinct from
/// `SecretChats` = 1, which routes to the Wave-3 `SecretChatsManager`.
pub const LOG_EVENT_HANDLER_TYPE_SECRET_CHAT_INFOS: i32 = 5;

/// `TdDb::OpenedDatabase` — the database plus the open metadata the Td actor
/// consumes (`since_last_open`; the per-manager binlog event vectors arrive
/// with the managers that own them).
pub struct OpenedDatabase {
    pub database: TdDb,
    pub since_last_open: i64,
    /// The `user_events` replay vector (TdDb.cpp:76-78) — every binlog event
    /// with the Users handler type, in binlog order, for
    /// `UserManager::on_binlog_user_event`.
    pub user_events: Vec<BinlogEvent>,
    /// The `secret_chat_events` replay vector (TdDb.cpp:79-87) — every
    /// binlog event with the SecretChatInfos handler type, in binlog order,
    /// for `UserManager::on_binlog_secret_chat_event`.
    pub secret_chat_events: Vec<BinlogEvent>,
}

/// `get_binlog_path` — `{database_directory}td{_test}.binlog`.
fn get_binlog_path(parameters: &Parameters) -> String {
    format!(
        "{}td{}.binlog",
        parameters.database_directory,
        if parameters.is_test_dc { "_test" } else { "" }
    )
}

/// `get_sqlite_path` — `{database_directory}db{_test}.sqlite`.
fn get_sqlite_path(parameters: &Parameters) -> String {
    format!(
        "{}db{}.sqlite",
        parameters.database_directory,
        if parameters.is_test_dc { "_test" } else { "" }
    )
}

/// `TdDb::check_parameters`: default directory, the use-database cascade,
/// and directory preparation (`mkpath` + `realpath`; the 0750 mode is a
/// documented divergence — mkdir modes are not portable).
fn check_parameters(parameters: &mut Parameters) -> Status {
    if parameters.database_directory.is_empty() {
        parameters.database_directory = ".".to_string();
    }
    if parameters.use_message_database && !parameters.use_chat_info_database {
        parameters.use_chat_info_database = true;
    }
    if parameters.use_chat_info_database && !parameters.use_file_database {
        parameters.use_file_database = true;
    }

    fn prepare_dir(dir: &str) -> Result<String> {
        let mut dir = dir.to_string();
        if !dir.ends_with(std::path::MAIN_SEPARATOR) {
            dir.push(std::path::MAIN_SEPARATOR);
        }
        if let Err(error) = std::fs::create_dir_all(&dir) {
            return Result::from_status(Status::error_message(&format!(
                "Failed to create directory: {error}"
            )));
        }
        let real_dir = match std::fs::canonicalize(&dir) {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(error) => {
                return Result::from_status(Status::error_message(&format!(
                    "Failed to get realpath: {error}"
                )));
            }
        };
        if real_dir.is_empty() {
            return Result::from_status(Status::error_message(
                "Failed to get realpath for empty path",
            ));
        }
        let mut real_dir = real_dir;
        if !real_dir.ends_with(std::path::MAIN_SEPARATOR) {
            real_dir.push(std::path::MAIN_SEPARATOR);
        }
        Result::Ok(real_dir)
    }

    let original_database_directory = parameters.database_directory.clone();
    match prepare_dir(&original_database_directory) {
        Result::Ok(dir) => parameters.database_directory = dir,
        Result::Err(error) => {
            tracing::warn!("Invalid database directory");
            return Status::error(
                400,
                &format!(
                    "Can't init database in the directory \"{original_database_directory}\": {}",
                    error.message()
                ),
            );
        }
    }

    if parameters.files_directory.is_empty() {
        parameters.files_directory = parameters.database_directory.clone();
    } else {
        let original_files_directory = parameters.files_directory.clone();
        match prepare_dir(&original_files_directory) {
            Result::Ok(dir) => parameters.files_directory = dir,
            Result::Err(error) => {
                tracing::warn!("Invalid files directory");
                return Status::error(
                    400,
                    &format!(
                        "Can't init files directory \"{original_files_directory}\": {}",
                        error.message()
                    ),
                );
            }
        }
    }

    Status::ok()
}

/// `TdDb` — the schema skeleton: owns the safe connection and the file
/// key-value table; manager-specific handles attach in later steps.
pub struct TdDb {
    parameters: Option<Parameters>,
    sql_connection: Option<Arc<SqliteConnectionSafe>>,
    files_kv: Option<SqliteKeyValue>,
    /// `TdDb::common_kv_safe_` — the "common" table behind the C++
    /// `get_sqlite_pmc`/`get_sqlite_sync_pmc` (the user/secret-chat store).
    common_kv: Option<SqliteKeyValue>,
    binlog_pmc: Option<BinlogKeyValue>,
    config_pmc: Option<BinlogKeyValue>,
    binlog: Option<Arc<ConcurrentBinlog>>,
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
            parameters: None,
            sql_connection: None,
            files_kv: None,
            common_kv: None,
            binlog_pmc: None,
            config_pmc: None,
            binlog: None,
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

        // the "common" table (`TdDb::common_kv_safe_`, TdDb.cpp:468) is
        // NOT created here: the C++ `SqliteKeyValueSafe` is a lazy
        // scheduler-local storage, so the table appears on first use
        // (`common_kv()` below) — the schema must stay byte-identical to
        // a C++ database that never touched the sync pmc.

        Status::ok()
    }

    /// `TdDb::get_binlog_pmc` — the pmc holding the auth state and the
    /// other non-option key-value state.
    pub fn binlog_pmc(&mut self) -> &mut BinlogKeyValue {
        self.binlog_pmc
            .as_mut()
            .expect("TdDb is not initialized: binlog pmc is missing")
    }

    /// `TdDb::get_config_pmc` — the option store.
    pub fn config_pmc(&mut self) -> &mut BinlogKeyValue {
        self.config_pmc
            .as_mut()
            .expect("TdDb is not initialized: config pmc is missing")
    }

    /// Move the config pmc out to its long-lived owner (the OptionManager);
    /// the underlying binlog handle stays shared through the `Arc`.
    pub fn take_config_pmc(&mut self) -> BinlogKeyValue {
        self.config_pmc
            .take()
            .expect("TdDb is not initialized: config pmc is missing")
    }

    /// The parameters the database was opened with (`TdDb::parameters_`).
    pub fn parameters(&self) -> &Parameters {
        self.parameters
            .as_ref()
            .expect("TdDb is not initialized: parameters are missing")
    }

    /// `TdDb::get_sqlite_sync_pmc` — the synchronous view of the "common"
    /// table. The C++ also runs an async wrapper (`get_sqlite_pmc`) on a
    /// database scheduler; the database opens synchronously in the actor
    /// here, so both collapse into this handle (the established M.1
    /// divergence). Like the C++ `SqliteKeyValueSafe` (a lazy
    /// scheduler-local storage), the table materializes on FIRST use —
    /// a database opened and closed untouched must not carry it (the
    /// schema-parity fixture).
    pub fn common_kv(&mut self) -> &mut SqliteKeyValue {
        if self.common_kv.is_none() {
            let mut common_kv = SqliteKeyValue::new();
            let connection = self
                .sql_connection
                .as_ref()
                .expect("TdDb is not initialized: sqlite connection is missing")
                .get();
            common_kv
                .init_with_connection(connection, "common")
                .ensure();
            self.common_kv = Some(common_kv);
        }
        self.common_kv
            .as_mut()
            .expect("the common kv was just initialized")
    }

    /// `TdDb::get_binlog` — the raw log-event surface (`binlog_add` /
    /// `binlog_rewrite` / `binlog_erase` with a `LogEvent::HandlerType`).
    /// A shared handle: `ConcurrentBinlog` serializes internally.
    pub fn binlog(&mut self) -> std::sync::Arc<ConcurrentBinlog> {
        self.binlog
            .clone()
            .expect("TdDb is not initialized: binlog is missing")
    }

    pub fn use_file_database(&self) -> bool {
        self.parameters().use_file_database
    }

    /// `TdDb::open` — the C++ `open_impl` in-process (the database-
    /// scheduler hop is a documented divergence until the network actors
    /// land): open the shared `td[._test].binlog` with both pmcs routed by
    /// magic, derive the sqlite key, then init the sqlite schema.
    pub fn open(mut parameters: Parameters) -> Result<OpenedDatabase> {
        let check_status = check_parameters(&mut parameters);
        if check_status.is_error() {
            return Result::from_status(check_status);
        }

        let binlog_path = get_binlog_path(&parameters);
        let sqlite_path = get_sqlite_path(&parameters);

        // `init_binlog`: the mtime of an existing binlog gives
        // `since_last_open`.
        let mut since_last_open: i64 = 0;
        let since = std::fs::metadata(&binlog_path)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|mtime| std::time::SystemTime::now().duration_since(mtime).ok())
            .map(|elapsed| elapsed.as_secs_f64());
        if let Some(since) = since {
            if since >= 86400.0 {
                tracing::warn!("Binlog wasn't opened for {since} seconds");
            }
            if since > 0.0 && since < 1e12 {
                since_last_open = since as i64;
            }
        }

        let mut binlog_pmc = BinlogKeyValue::new();
        let mut config_pmc = BinlogKeyValue::new();
        binlog_pmc.external_init_begin(BINLOG_PMC_MAGIC);
        config_pmc.external_init_begin(CONFIG_PMC_MAGIC);

        // The combined replay callback: each pmc consumes only the events
        // written with its own magic; the Users and SecretChatInfos events
        // collect for the user manager (the remaining handlers wait for
        // their owners).
        let mut user_events: Vec<BinlogEvent> = Vec::new();
        let mut secret_chat_events: Vec<BinlogEvent> = Vec::new();
        let mut binlog = ConcurrentBinlog::new();
        {
            let binlog_pmc = &mut binlog_pmc;
            let config_pmc = &mut config_pmc;
            let user_events = &mut user_events;
            let secret_chat_events = &mut secret_chat_events;
            let callback = |event: &BinlogEvent| match event.type_ {
                BINLOG_PMC_MAGIC => binlog_pmc.external_init_handle(event),
                CONFIG_PMC_MAGIC => config_pmc.external_init_handle(event),
                LOG_EVENT_HANDLER_TYPE_USERS => user_events.push(event.clone()),
                LOG_EVENT_HANDLER_TYPE_SECRET_CHAT_INFOS => secret_chat_events.push(event.clone()),
                _ => {}
            };
            let init_status = binlog.init(
                &binlog_path,
                callback,
                parameters.encryption_key.clone(),
                DbKey::empty(),
            );
            if let Result::Err(init_status) = init_status {
                return Result::from_status(init_status);
            }
        }
        let binlog = Arc::new(binlog);
        binlog_pmc.external_init_finish(binlog.clone());
        config_pmc.external_init_finish(binlog.clone());

        // Destroy an sqlite database from a previous (never authorized) run.
        if parameters.use_file_database && binlog_pmc.get("auth").is_empty() {
            tracing::info!("Destroy SQLite database, because wasn't authorized yet");
            let _ = crate::sqlite_db::destroy(&sqlite_path);
        }

        // The sqlite key is a random 32-byte secret living in the binlog
        // pmc; encryption follows the binlog key. The value is arbitrary
        // binary, so it goes through the raw (non-UTF-8) accessors.
        let encrypt_sqlite = !parameters.encryption_key.is_empty();
        let mut new_sqlite_key = DbKey::empty();
        let mut old_sqlite_key = DbKey::empty();
        let mut sqlite_key = binlog_pmc.get_raw(b"sqlite_key");
        if encrypt_sqlite {
            if sqlite_key.is_empty() {
                sqlite_key = vec![b' '; 32];
                random::secure_bytes(&mut sqlite_key);
                binlog_pmc.set_raw(b"sqlite_key", &sqlite_key);
                if parameters.use_file_database {
                    binlog_pmc.force_sync(None);
                }
            }
            new_sqlite_key = DbKey::raw_key(sqlite_key);
        } else if !sqlite_key.is_empty() {
            old_sqlite_key = DbKey::raw_key(sqlite_key);
            // drop_sqlite_key: the Rust sqlite port re-keys on open, so the
            // pmc erase happens right away (the C++ erases after the re-key
            // succeeds).
            binlog_pmc.erase("sqlite_key");
        }

        let mut db = TdDb::new();
        db.parameters = Some(parameters.clone());
        let mut init_status =
            db.init_sqlite(&sqlite_path, &parameters, &new_sqlite_key, &old_sqlite_key);
        if init_status.is_error() {
            // C++ destroys the bad database and retries once.
            tracing::error!("Destroy bad SQLite database because of {init_status}");
            let _ = crate::sqlite_db::destroy(&sqlite_path);
            init_status =
                db.init_sqlite(&sqlite_path, &parameters, &new_sqlite_key, &old_sqlite_key);
            if init_status.is_error() {
                return Result::from_status(init_status);
            }
        }
        db.binlog_pmc = Some(binlog_pmc);
        db.config_pmc = Some(config_pmc);
        db.binlog = Some(binlog);

        Result::Ok(OpenedDatabase {
            database: db,
            since_last_open,
            user_events,
            secret_chat_events,
        })
    }

    /// `TdDb::close_all` — drop the pmc views, then close (or destroy) the
    /// binlog and the sqlite connection.
    pub fn close_all(&mut self, destroy_flag: bool) {
        self.binlog_pmc.take();
        self.config_pmc.take();
        if let Some(binlog) = self.binlog.take() {
            if destroy_flag {
                binlog.close_and_destroy(None);
            } else {
                binlog.close(None);
            }
        }
        self.files_kv.take();
        self.common_kv.take();
        if let Some(connection) = self.sql_connection.take() {
            connection.close();
        }
        self.parameters = None;
    }

    pub fn close(&mut self) {
        self.files_kv.take();
        self.common_kv.take();
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
