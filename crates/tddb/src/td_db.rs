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
use tdutils::try_result;
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
/// `MESSAGE_DB_INDEX_COUNT_OLD` (td/telegram/MessageDb.cpp:42) — the nine
/// indices the `AddMessageDbMediaIndex` migration adds.
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
/// `LogEvent::HandlerType::UpdateScopeNotificationSettingsOnServer`
/// (logevent/LogEvent.h:89) — the un-acked scope settings server update.
pub const LOG_EVENT_HANDLER_TYPE_UPDATE_SCOPE_NOTIFICATION_SETTINGS_ON_SERVER: i32 = 0x110;
/// `LogEvent::HandlerType::ResetAllNotificationSettingsOnServer`
/// (logevent/LogEvent.h:90) — the un-acked reset-all server update.
pub const LOG_EVENT_HANDLER_TYPE_RESET_ALL_NOTIFICATION_SETTINGS_ON_SERVER: i32 = 0x111;
/// `LogEvent::HandlerType::UpdateReactionNotificationSettingsOnServer`
/// (logevent/LogEvent.h:108) — the un-acked reaction settings server
/// update.
pub const LOG_EVENT_HANDLER_TYPE_UPDATE_REACTION_NOTIFICATION_SETTINGS_ON_SERVER: i32 = 0x129;
/// `LogEvent::HandlerType::Chats` (logevent/LogEvent.h:66) — the raw
/// basic-group log events the `ChatManager` mirror writes while a chat is
/// unsaved (`ChatLogEvent`).
pub const LOG_EVENT_HANDLER_TYPE_CHATS: i32 = 3;
/// `LogEvent::HandlerType::Channels` (logevent/LogEvent.h:67) — the raw
/// supergroup log events the `ChatManager` mirror writes while a channel
/// is unsaved (`ChannelLogEvent`).
pub const LOG_EVENT_HANDLER_TYPE_CHANNELS: i32 = 4;
/// `LogEvent::HandlerType::ToggleDialogIsPinnedOnServer` (LogEvent.h:85) —
/// the DialogManager server-sync mirror events (M.6.18). All seven collect
/// into one replay vector for `DialogManager::on_binlog_dialog_events`.
pub const LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_IS_PINNED_ON_SERVER: i32 = 0x10c;
/// `LogEvent::HandlerType::ReorderPinnedDialogsOnServer` (LogEvent.h:86).
pub const LOG_EVENT_HANDLER_TYPE_REORDER_PINNED_DIALOGS_ON_SERVER: i32 = 0x10d;
/// `LogEvent::HandlerType::ToggleDialogReportSpamStateOnServer` (LogEvent.h:91).
pub const LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_REPORT_SPAM_STATE_ON_SERVER: i32 = 0x112;
/// `LogEvent::HandlerType::ToggleDialogIsMarkedAsUnreadOnServer` (LogEvent.h:94).
pub const LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_IS_MARKED_AS_UNREAD_ON_SERVER: i32 = 0x115;
/// `LogEvent::HandlerType::ToggleDialogIsBlockedOnServer` (LogEvent.h:97).
pub const LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_IS_BLOCKED_ON_SERVER: i32 = 0x118;
/// `LogEvent::HandlerType::ToggleDialogIsTranslatableOnServer` (LogEvent.h:105).
pub const LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_IS_TRANSLATABLE_ON_SERVER: i32 = 0x126;
/// `LogEvent::HandlerType::ToggleDialogViewAsMessagesOnServer` (LogEvent.h:106).
pub const LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_VIEW_AS_MESSAGES_ON_SERVER: i32 = 0x127;
/// `LogEvent::HandlerType::DeleteMessage` (logevent/LogEvent.h:74).
pub const LOG_EVENT_HANDLER_TYPE_DELETE_MESSAGE: i32 = 0x101;
/// `LogEvent::HandlerType::SendMessage` (logevent/LogEvent.h:73) — the
/// half-sent outgoing message events (`SendMessageLogEvent`, M.10.8); the
/// messages-manager owner erases or replays them on open.
pub const LOG_EVENT_HANDLER_TYPE_SEND_MESSAGE: i32 = 0x100;

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
    /// The on-server notification settings log events (Td.cpp:1034) — the
    /// UpdateScopeNotificationSettingsOnServer / ResetAllNotificationSettings
    /// OnServer / UpdateReactionNotificationSettingsOnServer events, in
    /// binlog order, for `NotificationSettingsManager::on_binlog_events`.
    pub notification_settings_events: Vec<BinlogEvent>,
    /// The `chat_events` replay vector (TdDb.cpp:79-87) — every binlog
    /// event with the Chats handler type, in binlog order, for
    /// `ChatManager::on_binlog_chat_event`.
    pub chat_events: Vec<BinlogEvent>,
    /// The `channel_events` replay vector (TdDb.cpp:79-87) — every binlog
    /// event with the Channels handler type, in binlog order, for
    /// `ChatManager::on_binlog_channel_event`.
    pub channel_events: Vec<BinlogEvent>,
    /// The `dialog_manager_events` replay vector (TdDb.cpp `dialog_events`
    /// shape) — every binlog event with one of the seven DialogManager
    /// server-sync handler types, in binlog order, for
    /// `DialogManager::on_binlog_dialog_events`.
    pub dialog_manager_events: Vec<BinlogEvent>,
    /// The `to_messages_manager` replay vector (TdDb.cpp:122-136) — the
    /// messages-manager handler types collected at open; of the C++ vector's
    /// members `DeleteMessage` (MessagesManager.cpp 36248-36264) and
    /// `SendMessage` (35951-35991) are ported so far, the rest arrive with
    /// their slices.
    pub messages_manager_events: Vec<BinlogEvent>,
}

/// `get_binlog_path` — `{database_directory}td{_test}.binlog`.
fn get_binlog_path(parameters: &Parameters) -> String {
    format!(
        "{}td{}.binlog",
        parameters.database_directory,
        if parameters.is_test_dc { "_test" } else { "" }
    )
}

/// `format::as_size` (format.h:185-208) — the B/KB/MB/GB formatter the
/// stats string uses (`size / unit` after the 100000-of-unit step-up).
fn format_size(size: i64) -> String {
    const SIZES: [(&str, u64); 4] = [("B", 1), ("KB", 1 << 10), ("MB", 1 << 20), ("GB", 1 << 30)];
    let mut i = 0;
    while i + 1 < SIZES.len() && size.unsigned_abs() >= 100000 * SIZES[i].1 {
        i += 1;
    }
    format!("{}{}", size / SIZES[i].1 as i64, SIZES[i].0)
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
    /// `TdDb::was_dialog_db_created_` — set by `init_dialog_db` when the
    /// `dialogs` table was created from scratch (DialogDb.cpp feeds
    /// `DialogManager::have_old_message_database` through it).
    was_dialog_db_created: bool,
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

fn create_notification_group_table(db: &SqliteDb) -> Status {
    db.exec(
        "CREATE TABLE IF NOT EXISTS notification_groups (notification_group_id INT4 PRIMARY KEY, dialog_id \
         INT8, last_notification_date INT4)",
    )
}

fn create_last_notification_date_index(db: &SqliteDb) -> Status {
    db.exec(
        "CREATE INDEX IF NOT EXISTS notification_group_by_last_notification_date ON notification_groups \
         (last_notification_date, dialog_id, notification_group_id) WHERE last_notification_date IS NOT NULL",
    )
}

fn add_dialogs_in_folder_index(db: &SqliteDb) -> Status {
    db.exec(
        "CREATE INDEX IF NOT EXISTS dialog_in_folder_by_dialog_order ON dialogs (folder_id, dialog_order, dialog_id) \
         WHERE folder_id IS NOT NULL",
    )
}

/// `init_dialog_db` (DialogDb.cpp:28-100) — the fresh-DB path plus the
/// `DbVersion`-gated migration ladder. The pinned-dialogs migration stores
/// its results into the binlog pmc; `was_created` tells TdDb to reset the
/// dialog-counters pmc keys afterwards (DialogDb.cpp feeds
/// `DialogManager::have_old_message_database` through this flag).
pub fn init_dialog_db(
    db: &SqliteDb,
    version: &mut i32,
    binlog_pmc: &mut BinlogKeyValue,
    was_created: &mut bool,
) -> Status {
    tracing::info!("Init dialog database version={version}");
    *was_created = false;

    // Check if database exists
    let has_table = db.has_table("dialogs").unwrap();
    let mut version_local = *version;
    if !has_table {
        version_local = 0;
    } else if version_local > CURRENT_DB_VERSION {
        try_status!(drop_dialog_db(db, version_local));
        version_local = 0;
    }

    if version_local == 0 {
        tracing::info!("Create new dialog database");
        *was_created = true;
        try_status!(
            db.exec("CREATE TABLE IF NOT EXISTS dialogs (dialog_id INT8 PRIMARY KEY, dialog_order INT8, data BLOB, \
                     folder_id INT4)")
        );
        try_status!(create_notification_group_table(db));
        try_status!(create_last_notification_date_index(db));
        try_status!(add_dialogs_in_folder_index(db));
        version_local = CURRENT_DB_VERSION;
    }
    if version_local < db_version::ADD_NOTIFICATIONS_SUPPORT {
        try_status!(create_notification_group_table(db));
        try_status!(create_last_notification_date_index(db));
    }
    if version_local < db_version::ADD_FOLDERS {
        try_status!(db.exec("DROP INDEX IF EXISTS dialog_by_dialog_order"));
        try_status!(db.exec("ALTER TABLE dialogs ADD COLUMN folder_id INT4"));
        try_status!(add_dialogs_in_folder_index(db));
        try_status!(db.exec(
            "UPDATE dialogs SET folder_id = 0 WHERE dialog_id < -1500000000000 AND dialog_order > 0"
        ));
    }
    if version_local < db_version::STORE_PINNED_DIALOGS_IN_BINLOG {
        // 9221294780217032704 == get_dialog_order(Auto(), MIN_PINNED_DIALOG_DATE - 1)
        try_result!(
            get_pinned_dialogs_stmt,
            db.get_statement(
                "SELECT dialog_id FROM dialogs WHERE folder_id = ?1 AND dialog_order > \
                 9221294780217032704 ORDER BY dialog_order DESC, dialog_id DESC"
            )
        );
        // the try_result! binding is immutable and the statement is stepped
        let mut get_pinned_dialogs_stmt = get_pinned_dialogs_stmt;
        for folder_id in 0..2i32 {
            let mut pinned_dialog_ids: Vec<String> = Vec::new();
            get_pinned_dialogs_stmt.bind_int32(1, folder_id).ensure();
            get_pinned_dialogs_stmt.step().ensure();
            while get_pinned_dialogs_stmt.has_row() {
                pinned_dialog_ids.push(get_pinned_dialogs_stmt.view_int64(0).to_string());
                get_pinned_dialogs_stmt.step().ensure();
            }
            get_pinned_dialogs_stmt.reset();

            binlog_pmc.set(
                &format!("pinned_dialog_ids{folder_id}"),
                &pinned_dialog_ids.join(","),
            );
        }
    }

    *version = version_local;
    Status::ok()
}

/// `drop_dialog_db` (DialogDb.cpp:103-110). The C++ keeps the first exec's
/// status and still runs the second drop, returning the first error —
/// ported verbatim.
pub fn drop_dialog_db(db: &SqliteDb, version: i32) -> Status {
    if version != 0 {
        tracing::warn!(
            "Drop chat database version={version} current_db_version={CURRENT_DB_VERSION}"
        );
    }
    let status = db.exec("DROP TABLE IF EXISTS dialogs");
    try_status!(db.exec("DROP TABLE IF EXISTS notification_groups"));
    status
}

/// `init_message_db` (MessageDb.cpp:45-171) — the fresh-DB path plus the
/// `DbVersion`-gated ALTER ladder.
pub fn init_message_db(db: &SqliteDb, version: &mut i32) -> Status {
    tracing::info!("Init message database version={version}");

    // Check if database exists
    let has_table = db.has_table("messages").unwrap();
    let mut version_local = *version;
    if !has_table {
        version_local = 0;
    } else if version_local > CURRENT_DB_VERSION {
        try_status!(drop_message_db(db, version_local));
        version_local = 0;
    }

    if version_local == 0 {
        tracing::info!("Create new message database");
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
    if version_local < db_version::ADD_MESSAGE_DB_MEDIA_INDEX {
        try_status!(db.exec("ALTER TABLE messages ADD COLUMN index_mask INT4"));
        try_status!(add_media_indices(db, 0, MESSAGE_DB_INDEX_COUNT_OLD));
    }
    if version_local < db_version::ADD_MESSAGE_DB30_MEDIA_INDEX {
        try_status!(add_media_indices(
            db,
            MESSAGE_DB_INDEX_COUNT_OLD,
            MESSAGE_DB_INDEX_COUNT
        ));
    }
    if version_local < db_version::ADD_MESSAGE_DB_FTS {
        try_status!(db.exec("ALTER TABLE messages ADD COLUMN search_id INT8"));
        try_status!(db.exec("ALTER TABLE messages ADD COLUMN text STRING"));
        try_status!(add_fts(db));
    }
    if version_local < db_version::ADD_MESSAGES_CALL_INDEX {
        try_status!(add_call_index(db));
    }
    if version_local < db_version::ADD_NOTIFICATIONS_SUPPORT {
        try_status!(db.exec("ALTER TABLE messages ADD COLUMN notification_id INT4"));
        try_status!(add_notification_id_index(db));
    }
    if version_local < db_version::ADD_SCHEDULED_MESSAGES {
        try_status!(add_scheduled_messages_table(db));
    }
    if version_local < db_version::ADD_MESSAGE_THREAD_SUPPORT {
        try_status!(db.exec("ALTER TABLE messages ADD COLUMN top_thread_message_id INT8"));
    }
    *version = version_local;
    Status::ok()
}

/// `drop_message_db` (MessageDb.cpp:174-178) — drops only the `messages`
/// table; the FTS and scheduled tables are left to the recreate path,
/// like the C++.
pub fn drop_message_db(db: &SqliteDb, version: i32) -> Status {
    tracing::warn!(
        "Drop message database version={version} current_db_version={CURRENT_DB_VERSION}"
    );
    db.exec("DROP TABLE IF EXISTS messages")
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

/// `init_file_db` (files/FileDb.cpp:40-57)
fn init_file_db(db: &SqliteDb, version: &mut i32) -> Status {
    let has_table = db.has_table("files").unwrap();
    let mut version_local = *version;
    if !has_table {
        version_local = 0;
    } else if version_local < db_version::FIX_FILE_REMOTE_LOCATION_KEY_BUG {
        // `drop_file_db` (FileDb.cpp:34-38): a pre-fix database is
        // dropped instead of migrated.
        try_status!(SqliteKeyValue::drop_table(db, "files"));
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
            was_dialog_db_created: false,
        }
    }

    /// `TdDb::init_sqlite` (skeleton): open, WAL, secure_delete, create the
    /// schema inside one transaction, set `user_version`. The binlog pmc is
    /// passed in because the C++ `init_dialog_db` writes the pinned-dialogs
    /// migration results into it and the pmc-erase tail runs against it.
    pub fn init_sqlite(
        &mut self,
        sql_database_path: &str,
        parameters: &Parameters,
        key: &DbKey,
        old_key: &DbKey,
        binlog_pmc: &mut BinlogKeyValue,
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

        let mut was_dialog_db_created = false;
        if use_dialog_db {
            let mut version = user_version;
            try_status!(init_dialog_db(
                &db,
                &mut version,
                binlog_pmc,
                &mut was_dialog_db_created
            ));
        } else {
            try_status!(drop_dialog_db(&db, user_version));
        }
        if use_message_thread_db {
            let mut version = user_version;
            try_status!(init_message_thread_db_skeleton(&db, &mut version));
        }
        if parameters.use_message_database {
            let mut version = user_version;
            try_status!(init_message_db(&db, &mut version));
        } else {
            // the database is being replaced by the binlog-only storage
            try_status!(drop_message_db(&db, user_version));
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

        // TdDb.cpp:441-457 — a freshly created dialog database resets every
        // dialog-counter key the managers persist (their owners re-fetch
        // everything from the server).
        if was_dialog_db_created {
            binlog_pmc.erase_by_prefix(b"pinned_dialog_ids");
            binlog_pmc.erase_by_prefix(b"last_server_dialog_date");
            binlog_pmc.erase_by_prefix(b"unread_message_count");
            binlog_pmc.erase_by_prefix(b"unread_dialog_count");
            binlog_pmc.erase("sponsored_dialog_id");
            binlog_pmc.erase_by_prefix(b"top_dialogs#");
            binlog_pmc.erase("dlds_counter");
            binlog_pmc.erase_by_prefix(b"dlds#");
            binlog_pmc.erase("fetched_marks_as_unread");
            binlog_pmc.erase_by_prefix(b"public_channels");
            binlog_pmc.erase("channels_to_send_stories");
            binlog_pmc.erase_by_prefix(b"saved_messages_tags");
        }
        if user_version == 0 {
            binlog_pmc.erase("next_contacts_sync_date");
            binlog_pmc.erase("saved_contact_count");
            binlog_pmc.erase("old_featured_sticker_set_count");
            binlog_pmc.erase("invalidate_old_featured_sticker_sets");
            // AttachMenuManager::get_attach_menu_bots_database_key()
            // (AttachMenuManager.cpp:372).
            binlog_pmc.erase("attach_bots");
        }
        binlog_pmc.force_sync(None);

        try_status!(db.exec("COMMIT TRANSACTION"));

        self.sql_connection = Some(connection.clone());

        // TdDb.cpp:469-476 — the same fresh-dialog-db reset against the
        // sqlite pmc (this materializes the "common" table, like the C++
        // `get_sqlite_sync_pmc()->erase` calls do).
        if was_dialog_db_created {
            let common_kv = self.common_kv();
            common_kv.erase(b"calls_db_state");
            common_kv.erase(b"di_active_live_location_messages");
            common_kv.erase_by_prefix(b"channel_recommendations");
        }
        self.was_dialog_db_created = was_dialog_db_created;

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

    /// `TdDb::sql_connection_` — a cloned handle to the shared SQLite
    /// connection (`create_message_db_sync(sql_connection_)` consumers).
    pub fn sql_connection(&self) -> Arc<SqliteConnectionSafe> {
        self.sql_connection
            .as_ref()
            .expect("TdDb is not initialized: sql connection is missing")
            .clone()
    }

    /// `TdDb::was_dialog_db_created()` (TdDb.h) — whether this open created
    /// the `dialogs` table from scratch.
    pub fn was_dialog_db_created(&self) -> bool {
        self.was_dialog_db_created
    }

    /// `TdDb::with_db_path` (TdDb.cpp:687-692) — the binlog and sqlite
    /// paths every consumer iterates (the FileManager `bad_paths_`, the
    /// storage fast-stat database size).
    pub fn db_paths(&self) -> Vec<String> {
        vec![
            get_binlog_path(self.parameters()),
            get_sqlite_path(self.parameters()),
        ]
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

    /// `TdDb::file_db_`'s key-value handle — the "files" table behind the
    /// C++ `SqliteKeyValueSafe` (created by `init_file_db` at open). The
    /// FileDb access layer of the Files step (tdclient) wraps this; the
    /// C++ actor marshaling collapses into the single scheduler thread.
    pub fn files_kv(&mut self) -> &mut SqliteKeyValue {
        self.files_kv
            .as_mut()
            .expect("TdDb is not initialized: the files key-value is missing")
    }

    /// `TdDb::get_stats` (TdDb.cpp:694-773) — the per-table size/count
    /// aggregates plus the file-db `@@` reference-chain depth audit,
    /// behind the `getDatabaseStatistics` request.
    pub fn get_stats(&mut self) -> tdutils::status::Result<String> {
        let sql = match self.sql_connection.as_ref() {
            Some(connection) => connection.get(),
            None => return Result::Err(Status::error_message("Database has no sqlite connection")),
        };
        let mut sb = String::new();
        // `as_size` (format.h) — the human-readable <size> form
        let as_size = |value: i64| format_size(value);
        let run_query = |query: &str, desc: &str, sb: &mut String| -> Status {
            let mut stmt = match sql.get_statement(query) {
                Result::Ok(stmt) => stmt,
                Result::Err(status) => return status,
            };
            let step_status = stmt.step();
            if step_status.is_error() {
                return step_status;
            }
            if !stmt.has_row() {
                return Status::error_message("stats query returned no row");
            }
            let key_size = stmt.view_int64(0);
            let value_size = stmt.view_int64(1);
            let count = stmt.view_int64(2);
            sb.push_str(query);
            sb.push('\n');
            sb.push_str(desc);
            sb.push_str(":\n");
            sb.push_str(&as_size(key_size + value_size));
            sb.push('\t');
            sb.push_str(&as_size(key_size));
            sb.push('\t');
            sb.push_str(&as_size(value_size));
            sb.push('\t');
            sb.push_str(&as_size((key_size + value_size) / count.max(1)));
            sb.push_str("\n\n");
            Status::ok()
        };
        for (query, desc) in [
            (
                "SELECT 0, SUM(length(data)), COUNT(*) FROM stories WHERE 1",
                "stories",
            ),
            (
                "SELECT 0, SUM(length(data)), COUNT(*) FROM messages WHERE 1",
                "messages",
            ),
            (
                "SELECT 0, SUM(length(data)), COUNT(*) FROM dialogs WHERE 1",
                "dialogs",
            ),
        ] {
            let status = run_query(query, desc, &mut sb);
            if status.is_error() {
                return Result::Err(status);
            }
        }
        for (mask, table) in [
            ("%", "common"),
            ("%", "files"),
            ("wp%", "common"),
            ("wpurl%", "common"),
            ("wpiv%", "common"),
            ("us%", "common"),
            ("ch%", "common"),
            ("ss%", "common"),
            ("gr%", "common"),
        ] {
            let query = format!(
                "SELECT SUM(length(k)), SUM(length(v)), COUNT(*) FROM {table} WHERE k like '{mask}'"
            );
            let desc = format!("{table}:{mask}");
            let status = run_query(&query, &desc, &mut sb);
            if status.is_error() {
                return Result::Err(status);
            }
        }

        // the file-db `@@` reference-chain depth audit
        let mut prev: Vec<i32> = vec![0];
        let mut count: usize = 0;
        let mut max_bad_to: i32 = 0;
        let mut bad_count: usize = 0;
        self.files_kv()
            .get_by_range(b"file0", b"file:", &mut |key: &[u8], value: &[u8]| {
                if !value.starts_with(b"@@") {
                    return true;
                }
                count += 1;
                // the key is "file<id>", the value "@@<id>" — decimal ids
                let from = std::str::from_utf8(&key[4..])
                    .ok()
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(0);
                let to = std::str::from_utf8(&value[2..])
                    .ok()
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(0);
                if from <= to {
                    if to > max_bad_to {
                        max_bad_to = to;
                    }
                    bad_count += 1;
                    return true;
                }
                if from as usize >= prev.len() {
                    prev.resize(from as usize + 1, 0);
                }
                if to as usize >= prev.len() {
                    prev.resize(to as usize + 1, 0);
                }
                prev[from as usize] = to;
                true
            });
        for i in 1..prev.len() {
            if prev[i] == 0 {
                continue;
            }
            prev[i] = prev[prev[i] as usize] + 1;
        }
        sb.push_str(&format!(
            "Max file database depth out of {}/{} elements: {}\n",
            prev.len(),
            count,
            prev.iter().copied().max().unwrap_or(0)
        ));
        sb.push_str(&format!(
            "Have {} forward references with maximum reference to {}",
            bad_count, max_bad_to
        ));
        Result::Ok(sb)
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
        // written with its own magic; the Users, SecretChatInfos, Chats and
        // Channels events collect for their managers, the
        // notification-settings on-server events for the settings manager
        // (the remaining handlers wait for their owners).
        let mut user_events: Vec<BinlogEvent> = Vec::new();
        let mut secret_chat_events: Vec<BinlogEvent> = Vec::new();
        let mut notification_settings_events: Vec<BinlogEvent> = Vec::new();
        let mut chat_events: Vec<BinlogEvent> = Vec::new();
        let mut channel_events: Vec<BinlogEvent> = Vec::new();
        let mut dialog_manager_events: Vec<BinlogEvent> = Vec::new();
        let mut messages_manager_events: Vec<BinlogEvent> = Vec::new();
        let mut binlog = ConcurrentBinlog::new();
        {
            let binlog_pmc = &mut binlog_pmc;
            let config_pmc = &mut config_pmc;
            let user_events = &mut user_events;
            let secret_chat_events = &mut secret_chat_events;
            let notification_settings_events = &mut notification_settings_events;
            let chat_events = &mut chat_events;
            let channel_events = &mut channel_events;
            let dialog_manager_events = &mut dialog_manager_events;
            let messages_manager_events = &mut messages_manager_events;
            let callback = |event: &BinlogEvent| match event.type_ {
                BINLOG_PMC_MAGIC => binlog_pmc.external_init_handle(event),
                CONFIG_PMC_MAGIC => config_pmc.external_init_handle(event),
                LOG_EVENT_HANDLER_TYPE_USERS => user_events.push(event.clone()),
                LOG_EVENT_HANDLER_TYPE_SECRET_CHAT_INFOS => secret_chat_events.push(event.clone()),
                LOG_EVENT_HANDLER_TYPE_UPDATE_SCOPE_NOTIFICATION_SETTINGS_ON_SERVER
                | LOG_EVENT_HANDLER_TYPE_RESET_ALL_NOTIFICATION_SETTINGS_ON_SERVER
                | LOG_EVENT_HANDLER_TYPE_UPDATE_REACTION_NOTIFICATION_SETTINGS_ON_SERVER => {
                    notification_settings_events.push(event.clone())
                }
                LOG_EVENT_HANDLER_TYPE_CHATS => chat_events.push(event.clone()),
                LOG_EVENT_HANDLER_TYPE_CHANNELS => channel_events.push(event.clone()),
                LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_IS_PINNED_ON_SERVER
                | LOG_EVENT_HANDLER_TYPE_REORDER_PINNED_DIALOGS_ON_SERVER
                | LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_REPORT_SPAM_STATE_ON_SERVER
                | LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_IS_MARKED_AS_UNREAD_ON_SERVER
                | LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_IS_BLOCKED_ON_SERVER
                | LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_IS_TRANSLATABLE_ON_SERVER
                | LOG_EVENT_HANDLER_TYPE_TOGGLE_DIALOG_VIEW_AS_MESSAGES_ON_SERVER => {
                    dialog_manager_events.push(event.clone())
                }
                LOG_EVENT_HANDLER_TYPE_DELETE_MESSAGE | LOG_EVENT_HANDLER_TYPE_SEND_MESSAGE => {
                    messages_manager_events.push(event.clone())
                }
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
        let mut init_status = db.init_sqlite(
            &sqlite_path,
            &parameters,
            &new_sqlite_key,
            &old_sqlite_key,
            &mut binlog_pmc,
        );
        if init_status.is_error() {
            // C++ destroys the bad database and retries once.
            tracing::error!("Destroy bad SQLite database because of {init_status}");
            let _ = crate::sqlite_db::destroy(&sqlite_path);
            init_status = db.init_sqlite(
                &sqlite_path,
                &parameters,
                &new_sqlite_key,
                &old_sqlite_key,
                &mut binlog_pmc,
            );
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
            notification_settings_events,
            chat_events,
            channel_events,
            dialog_manager_events,
            messages_manager_events,
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

    /// `init_sqlite` writes the pinned-dialogs migration results (and the
    /// fresh-database pmc resets) into the binlog pmc, so the tests hand it
    /// a real (throwaway) binlog-backed pmc.
    fn test_binlog_pmc(name: &str) -> BinlogKeyValue {
        let path = std::env::temp_dir()
            .join(name)
            .to_string_lossy()
            .into_owned();
        let _ = BinlogKeyValue::destroy(&path);
        let mut pmc = BinlogKeyValue::new();
        pmc.init(&path, DbKey::empty()).ensure();
        pmc
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
        let mut pmc = test_binlog_pmc("m6_tddb_rs_pmc");
        tddb.init_sqlite(
            &rs_path,
            &Parameters::default(),
            &DbKey::empty(),
            &DbKey::empty(),
            &mut pmc,
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
        let mut pmc = test_binlog_pmc("m6_tddb_enc_pmc");
        tddb.init_sqlite(
            &path,
            &Parameters::default(),
            &DbKey::password("cucumber"),
            &DbKey::empty(),
            &mut pmc,
        )
        .ensure();
        tddb.close();

        // reopen with the right key works, wrong key fails
        let mut tddb = TdDb::new();
        let mut pmc = test_binlog_pmc("m6_tddb_enc_pmc");
        tddb.init_sqlite(
            &path,
            &Parameters::default(),
            &DbKey::password("cucumber"),
            &DbKey::empty(),
            &mut pmc,
        )
        .ensure();
        tddb.close();
        let mut tddb = TdDb::new();
        let mut pmc = test_binlog_pmc("m6_tddb_enc_pmc");
        assert!(
            tddb.init_sqlite(
                &path,
                &Parameters::default(),
                &DbKey::password("tomato"),
                &DbKey::empty(),
                &mut pmc,
            )
            .is_error()
        );
        let _ = crate::sqlite_db::destroy(&path);
    }

    /// The pytdbot/echobot live blocker (2026-11-08): `TdDb::open` on a
    /// FRESH directory with a non-empty `database_encryption_key` must
    /// create the database (binlog + sqlite, AesCtr-encrypted from the
    /// first event) and reopen cleanly — it reported WRONG_PASSWORD
    /// instead. The key is the raw KDF fast path (`TdDb::as_db_key`), and
    /// an empty JSON key defaults to the "cucumber" raw key, so both arms
    /// are covered.
    #[test]
    fn tddb_fresh_open_with_key_roundtrip() {
        for (name, key) in [
            ("m6_tddb_fresh_key", b"1234echobot$".to_vec()),
            ("m6_tddb_fresh_key_empty", Vec::new()),
        ] {
            let dir = std::env::temp_dir().join(name);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let parameters = Parameters {
                encryption_key: if key.is_empty() {
                    DbKey::raw_key(b"cucumber".to_vec())
                } else {
                    DbKey::raw_key(key)
                },
                database_directory: dir.join("database").to_string_lossy().into_owned(),
                files_directory: dir.join("files").to_string_lossy().into_owned(),
                ..Parameters::default()
            };

            // 1. fresh open with the key creates the database
            let mut opened = match TdDb::open(parameters.clone()) {
                Result::Ok(ok) => ok,
                Result::Err(status) => panic!("fresh open failed: {status}"),
            };
            opened.database.close_all(false);

            // 2. reopen with the same key reads the created binlog
            let mut opened = match TdDb::open(parameters.clone()) {
                Result::Ok(ok) => ok,
                Result::Err(status) => panic!("reopen failed: {status}"),
            };
            opened.database.close_all(false);

            // 3. reopen with a wrong key must fail with WRONG_PASSWORD
            let mut wrong = parameters.clone();
            wrong.encryption_key = DbKey::raw_key(b"other-key".to_vec());
            assert!(TdDb::open(wrong).is_error());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// M.6.13: the fresh path sets `was_created` and creates both tables;
    /// the pinned-dialogs migration (a database at version 11) collects the
    /// pinned rows per folder into the pmc, newest order first.
    #[test]
    fn dialog_db_ladder_and_pinned_migration() {
        fn open_sqlite(name: &str) -> SqliteDb {
            let path = std::env::temp_dir()
                .join(name)
                .to_string_lossy()
                .into_owned();
            let _ = crate::sqlite_db::destroy(&path);
            SqliteDb::open_with_key(&path, true, &DbKey::empty(), None).unwrap()
        }

        // 1. fresh database: was_created, both tables exist
        let db = open_sqlite("m6_dialogdb_fresh");
        let mut pmc = test_binlog_pmc("m6_dialogdb_fresh_pmc");
        let mut was_created = false;
        let mut version = 0;
        init_dialog_db(&db, &mut version, &mut pmc, &mut was_created).ensure();
        assert!(was_created);
        assert_eq!(version, CURRENT_DB_VERSION);
        assert!(db.has_table("dialogs").unwrap());
        assert!(db.has_table("notification_groups").unwrap());
        // a second run over the same database is not a creation
        let mut version = CURRENT_DB_VERSION;
        init_dialog_db(&db, &mut version, &mut pmc, &mut was_created).ensure();
        assert!(!was_created);
        drop(db);
        let _ = crate::sqlite_db::destroy(
            std::env::temp_dir()
                .join("m6_dialogdb_fresh")
                .to_str()
                .unwrap(),
        );

        // 2. an old (version 11) database migrates its pinned dialogs into
        // the pmc, ordered by dialog_order DESC per folder
        let db = open_sqlite("m6_dialogdb_old");
        db.exec(
            "CREATE TABLE dialogs (dialog_id INT8 PRIMARY KEY, dialog_order INT8, data BLOB, folder_id INT4)",
        )
        .ensure();
        db.exec(
            "CREATE TABLE notification_groups (notification_group_id INT4 PRIMARY KEY, dialog_id INT8, \
             last_notification_date INT4)",
        )
        .ensure();
        for (dialog_id, order, folder) in [
            (-1000i64, 9221294780217032705i64, 0i32),
            (-1001, 9221294780217032706, 0),
            (-1002, 100, 0),
            (-1003, 9221294780217032707, 1),
        ] {
            let mut stmt = db
                .get_statement("INSERT INTO dialogs VALUES(?1, ?2, ?3, ?4)")
                .unwrap();
            stmt.bind_int64(1, dialog_id).ensure();
            stmt.bind_int64(2, order).ensure();
            stmt.bind_blob(3, b"x").ensure();
            stmt.bind_int32(4, folder).ensure();
            stmt.step().ensure();
        }
        let mut pmc = test_binlog_pmc("m6_dialogdb_old_pmc");
        let mut was_created = true;
        let mut version = db_version::ADD_SCHEDULED_MESSAGES;
        init_dialog_db(&db, &mut version, &mut pmc, &mut was_created).ensure();
        assert!(!was_created);
        assert_eq!(pmc.get("pinned_dialog_ids0"), "-1001,-1000");
        assert_eq!(pmc.get("pinned_dialog_ids1"), "-1003");
        assert_eq!(pmc.get("last_server_dialog_date"), "");
        drop(db);
        let _ = crate::sqlite_db::destroy(
            std::env::temp_dir()
                .join("m6_dialogdb_old")
                .to_str()
                .unwrap(),
        );
    }
}
