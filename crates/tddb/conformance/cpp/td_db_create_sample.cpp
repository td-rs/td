// Conformance helper (S44): creates a tdlib sqlite database using the exact
// schema SQL from td/telegram/{DialogDb,MessageDb,StoryDb,files/FileDb}.cpp
// (fresh-database path, db version 14, message-thread database disabled as in
// current tdlib). Used to diff the schema created by the Rust port.
//
// Build against a configured TDLib build tree (see binlog README.md).

#include "td/db/DbKey.h"
#include "td/db/SqliteConnectionSafe.h"
#include "td/db/SqliteDb.h"
#include "td/db/SqliteKeyValue.h"

#include "td/utils/Status.h"
#include "td/utils/common.h"
#include "td/utils/int_types.h"
#include "td/utils/logging.h"

// td/telegram/Version.h
enum class DbVersion : std::int32_t {
  CreateDialogDb = 3,
  AddMessageDbMediaIndex,
  AddMessageDb30MediaIndex,
  AddMessageDbFts,
  AddMessagesCallIndex,
  FixFileRemoteLocationKeyBug,
  AddNotificationsSupport,
  AddFolders,
  AddScheduledMessages,
  StorePinnedDialogsInBinlog,
  AddMessageThreadSupport,
  AddMessageThreadDatabase,
  Next
};

inline constexpr std::int32_t current_db_version() {
  return static_cast<std::int32_t>(DbVersion::Next) - 1;
}

// td/telegram/MessageDb.cpp
constexpr std::int32_t MESSAGE_DB_INDEX_COUNT = 30;
// MessageSearchFilter::Call = 10, MissedCall = 11

int main(int argc, char *argv[]) {
  SET_VERBOSITY_LEVEL(VERBOSITY_NAME(ERROR));
  if (argc < 2) {
    LOG(PLAIN) << "Usage: td_db_create_sample <output_path>";
    return 1;
  }
  td::string path = argv[1];
  td::SqliteDb::destroy(path).ignore();

  td::SqliteDb db;
  auto r_db = td::SqliteDb::change_key(path, true, td::DbKey::empty(),
                                       td::DbKey::empty());
  if (r_db.is_error()) {
    LOG(PLAIN) << "ERROR " << r_db.error().message();
    return 2;
  }
  db = r_db.move_as_ok();
  db.exec("PRAGMA journal_mode=WAL").ensure();
  db.exec("PRAGMA secure_delete=1").ensure();
  db.exec("BEGIN TRANSACTION").ensure();
  auto user_version = db.user_version().move_as_ok();
  (void)0;

  // init_dialog_db (use_dialog_db = true), fresh database
  {
    auto has_table = db.has_table("dialogs").move_as_ok();
    auto version = user_version;
    if (!has_table) {
      version = 0;
    }
    if (version == 0) {
      db.exec("CREATE TABLE IF NOT EXISTS dialogs (dialog_id INT8 PRIMARY KEY, "
              "dialog_order INT8, data BLOB, "
              "folder_id INT4)")
          .ensure();
      db.exec("CREATE TABLE IF NOT EXISTS notification_groups "
              "(notification_group_id INT4 PRIMARY KEY, dialog_id "
              "INT8, last_notification_date INT4)")
          .ensure();
      db.exec("CREATE INDEX IF NOT EXISTS "
              "notification_group_by_last_notification_date ON "
              "notification_groups "
              "(last_notification_date, dialog_id, notification_group_id) "
              "WHERE last_notification_date IS NOT NULL")
          .ensure();
      db.exec("CREATE INDEX IF NOT EXISTS dialog_in_folder_by_dialog_order ON "
              "dialogs (folder_id, dialog_order, dialog_id) "
              "WHERE folder_id IS NOT NULL")
          .ensure();
      version = current_db_version();
    }
    user_version = version;
  }

  // init_message_db (use_message_database = true), fresh database
  {
    auto has_table = db.has_table("messages").move_as_ok();
    auto version = user_version;
    if (!has_table) {
      version = 0;
    }
    if (version == 0) {
      db.exec("CREATE TABLE IF NOT EXISTS messages (dialog_id INT8, message_id "
              "INT8, unique_message_id INT4, "
              "sender_user_id INT8, random_id INT8, data BLOB, ttl_expires_at "
              "INT4, index_mask INT4, search_id INT8, "
              "text STRING, notification_id INT4, top_thread_message_id INT8, "
              "PRIMARY KEY (dialog_id, message_id))")
          .ensure();
      db.exec("CREATE INDEX IF NOT EXISTS message_by_random_id ON messages "
              "(dialog_id, random_id) "
              "WHERE random_id IS NOT NULL")
          .ensure();
      db.exec("CREATE INDEX IF NOT EXISTS message_by_unique_message_id ON "
              "messages "
              "(unique_message_id) WHERE unique_message_id IS NOT NULL")
          .ensure();
      db.exec("CREATE INDEX IF NOT EXISTS message_by_ttl ON messages "
              "(ttl_expires_at) WHERE ttl_expires_at IS NOT NULL")
          .ensure();
      for (int i = 0; i < MESSAGE_DB_INDEX_COUNT; i++) {
        db.exec(PSLICE()
                << "CREATE INDEX IF NOT EXISTS message_index_" << i
                << " ON messages (dialog_id, message_id) WHERE (index_mask & "
                << (1 << i) << ") != 0")
            .ensure();
      }
      db.exec("CREATE INDEX IF NOT EXISTS message_by_search_id ON messages "
              "(search_id) WHERE search_id IS NOT NULL")
          .ensure();
      db.exec("CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING "
              "fts5(text, content='messages', "
              "content_rowid='search_id', tokenize = \"unicode61 "
              "remove_diacritics 0 tokenchars '\a'\")")
          .ensure();
      db.exec("CREATE TRIGGER IF NOT EXISTS trigger_fts_delete BEFORE DELETE "
              "ON messages WHEN OLD.search_id IS NOT NULL"
              " BEGIN INSERT INTO messages_fts(messages_fts, rowid, text) "
              "VALUES(\'delete\', OLD.search_id, OLD.text); END")
          .ensure();
      db.exec("CREATE TRIGGER IF NOT EXISTS trigger_fts_insert AFTER INSERT ON "
              "messages WHEN NEW.search_id IS NOT NULL"
              " BEGIN INSERT INTO messages_fts(rowid, text) "
              "VALUES(NEW.search_id, NEW.text); END")
          .ensure();
      for (int i = static_cast<int>(td::int32{10}) - 1;
           i < static_cast<int>(td::int32{11}); i++) {
        db.exec(PSLICE()
                << "CREATE INDEX IF NOT EXISTS full_message_index_" << i
                << " ON messages (unique_message_id) WHERE (index_mask & "
                << (1 << i) << ") != 0")
            .ensure();
      }
      db.exec("CREATE INDEX IF NOT EXISTS message_by_notification_id ON "
              "messages (dialog_id, notification_id) WHERE "
              "notification_id IS NOT NULL")
          .ensure();
      db.exec("CREATE TABLE IF NOT EXISTS scheduled_messages (dialog_id INT8, "
              "message_id INT8, "
              "server_message_id INT4, data BLOB, PRIMARY KEY (dialog_id, "
              "message_id))")
          .ensure();
      db.exec("CREATE INDEX IF NOT EXISTS message_by_server_message_id ON "
              "scheduled_messages "
              "(dialog_id, server_message_id) WHERE server_message_id IS NOT "
              "NULL")
          .ensure();
      version = current_db_version();
    }
    user_version = version;
  }

  // init_story_db (use_story_database = true), fresh database
  {
    auto version = user_version;
    auto has_stories = db.has_table("stories").move_as_ok();
    auto has_active = db.has_table("active_stories").move_as_ok();
    auto has_lists = db.has_table("active_story_lists").move_as_ok();
    if ((!has_stories || !has_active || !has_lists) ||
        version > current_db_version()) {
      version = 0;
    }
    if (version == 0) {
      db.exec("CREATE TABLE IF NOT EXISTS stories (dialog_id INT8, story_id "
              "INT4, expires_at INT4, notification_id "
              "INT4, data BLOB, PRIMARY KEY (dialog_id, story_id))")
          .ensure();
      db.exec("CREATE INDEX IF NOT EXISTS story_by_ttl ON stories (expires_at) "
              "WHERE expires_at IS NOT NULL")
          .ensure();
      db.exec("CREATE INDEX IF NOT EXISTS story_by_notification_id ON stories "
              "(dialog_id, notification_id) WHERE "
              "notification_id IS NOT NULL")
          .ensure();
      db.exec("CREATE TABLE IF NOT EXISTS active_stories (dialog_id INT8 "
              "PRIMARY KEY, story_list_id INT4, "
              "dialog_order INT8, data BLOB)")
          .ensure();
      db.exec("CREATE INDEX IF NOT EXISTS active_stories_by_order ON "
              "active_stories (story_list_id, dialog_order, "
              "dialog_id) WHERE story_list_id IS NOT NULL")
          .ensure();
      db.exec("CREATE TABLE IF NOT EXISTS active_story_lists (story_list_id "
              "INT4 PRIMARY KEY, data BLOB)")
          .ensure();
      version = current_db_version();
    }
    user_version = version;
  }

  // init_file_db (use_file_database = true), fresh database
  {
    auto has_table = db.has_table("files").move_as_ok();
    auto version = user_version;
    if (!has_table) {
      version = 0;
    }
    if (version == 0) {
      td::SqliteKeyValue::init(db, "files").ensure();
    }
    user_version = version;
  }

  if (current_db_version() != user_version) {
    db.set_user_version(current_db_version()).ensure();
  }
  db.exec("COMMIT TRANSACTION").ensure();
  db.close();
  LOG(PLAIN) << "OK user_version=" << current_db_version();
  return 0;
}
