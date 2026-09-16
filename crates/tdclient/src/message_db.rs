//! Port of `td/telegram/MessageDb.{h,cpp}` — the message database engine
//! (M.6): the sync interface and query/result structs (MessageDb.h:29-149),
//! `MessageDbImpl` (MessageDb.cpp:180-967) and the `get_message_info` blob
//! header parser (:935-967).
//!
//! The async actor (`MessageDbAsync`, :989-1241), the LSLRS safe wrapper
//! (:972-988) and the TdDb wiring land in M.6.10-M.6.12. The trait impl for
//! [`MessageDbSyncInterface`] is written once the full sync surface exists
//! (M.6.10, like the C++ class which only completes at :967).

use std::sync::{Arc, Mutex};

use crate::chat_manager::MessageId;
use crate::dialog_id::{
    DialogId, MessageFullId, MessageSearchFilter, NotificationId, ServerMessageId,
    call_message_search_filter_index, message_search_filter_index,
};
use crate::store_parse::{self, Parser};

use tdactor::{Actor, ActorOwn, actor_messages, create_actor_on_scheduler, send_closure_later};
use tddb::sqlite_connection_safe::SqliteConnectionSafe;
use tddb::sqlite_db::SqliteDb;
use tddb::sqlite_statement::SqliteStatement;
use tdutils::promise::Promise;
use tdutils::status::{Result, Status};
use tdutils::time::now_cached;
use tdutils::try_result;
use tdutils::unicode::{UnicodeSimpleCategory, get_unicode_simple_category};
use tdutils::utf8::{next_utf8_unsafe, utf8_truncate};

/// `MESSAGE_DB_INDEX_COUNT` (MessageDb.cpp:41).
const MESSAGE_DB_INDEX_COUNT: i32 = 30;

/// `MessageDbMessagesQuery` (MessageDb.h:29).
#[derive(Clone, Copy, Debug, Default)]
pub struct MessageDbMessagesQuery {
    pub dialog_id: DialogId,
    pub filter: MessageSearchFilter,
    pub from_message_id: MessageId,
    pub offset: i32,
    pub limit: i32,
}

/// `MessageDbDialogMessage` (MessageDb.h:37); the C++ `BufferSlice` is an
/// owned byte vector here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageDbDialogMessage {
    pub message_id: MessageId,
    pub data: Vec<u8>,
}

/// `MessageDbMessage` (MessageDb.h:42).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageDbMessage {
    pub dialog_id: DialogId,
    pub message_id: MessageId,
    pub data: Vec<u8>,
}

/// `MessageDbDialogCalendarQuery` (MessageDb.h:48).
#[derive(Clone, Copy, Debug, Default)]
pub struct MessageDbDialogCalendarQuery {
    pub dialog_id: DialogId,
    pub filter: MessageSearchFilter,
    pub from_message_id: MessageId,
    pub tz_offset: i32,
}

/// `MessageDbCalendar` (MessageDb.h:55).
#[derive(Clone, Debug, Default)]
pub struct MessageDbCalendar {
    pub messages: Vec<MessageDbDialogMessage>,
    pub total_counts: Vec<i32>,
}

/// `MessageDbGetDialogSparseMessagePositionsQuery` (MessageDb.h:60).
#[derive(Clone, Copy, Debug, Default)]
pub struct MessageDbGetDialogSparseMessagePositionsQuery {
    pub dialog_id: DialogId,
    pub filter: MessageSearchFilter,
    pub from_message_id: MessageId,
    pub limit: i32,
}

/// `MessageDbMessagePosition` (MessageDb.h:67).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageDbMessagePosition {
    pub position: i32,
    pub date: i32,
    pub message_id: MessageId,
}

/// `MessageDbMessagePositions` (MessageDb.h:73).
#[derive(Clone, Debug, Default)]
pub struct MessageDbMessagePositions {
    pub total_count: i32,
    pub positions: Vec<MessageDbMessagePosition>,
}

/// `MessageDbFtsQuery` (MessageDb.h:78).
#[derive(Clone, Debug, Default)]
pub struct MessageDbFtsQuery {
    pub query: String,
    pub dialog_id: DialogId,
    pub filter: MessageSearchFilter,
    pub from_search_id: i64,
    pub limit: i32,
}

/// `MessageDbFtsResult` (MessageDb.h:85) — the C++ default is
/// `next_search_id{1}`.
#[derive(Clone, Debug)]
pub struct MessageDbFtsResult {
    pub messages: Vec<MessageDbMessage>,
    pub next_search_id: i64,
}

impl Default for MessageDbFtsResult {
    fn default() -> MessageDbFtsResult {
        MessageDbFtsResult {
            messages: Vec::new(),
            next_search_id: 1,
        }
    }
}

/// `MessageDbCallsQuery` (MessageDb.h:90).
#[derive(Clone, Copy, Debug, Default)]
pub struct MessageDbCallsQuery {
    pub filter: MessageSearchFilter,
    pub from_unique_message_id: i32,
    pub limit: i32,
}

/// `MessageDbCallsResult` (MessageDb.h:96).
#[derive(Clone, Debug, Default)]
pub struct MessageDbCallsResult {
    pub messages: Vec<MessageDbMessage>,
}

/// `MessageDbSyncInterface` (MessageDb.h:100-139). Read methods return
/// [`Result`]; the C++ signals absence through the default-constructed
/// result, which maps to `Ok` of a zeroed struct.
#[allow(clippy::too_many_arguments)]
pub trait MessageDbSyncInterface {
    fn add_message(
        &mut self,
        message_full_id: MessageFullId,
        unique_message_id: ServerMessageId,
        sender_dialog_id: DialogId,
        random_id: i64,
        ttl_expires_at: i32,
        index_mask: i32,
        search_id: i64,
        text: String,
        notification_id: NotificationId,
        top_thread_message_id: MessageId,
        data: Vec<u8>,
    );
    fn add_scheduled_message(&mut self, message_full_id: MessageFullId, data: Vec<u8>);

    fn delete_message(&mut self, message_full_id: MessageFullId);
    fn delete_all_dialog_messages(&mut self, dialog_id: DialogId, from_message_id: MessageId);
    fn delete_dialog_messages_by_sender(&mut self, dialog_id: DialogId, sender_dialog_id: DialogId);

    fn get_message(&mut self, message_full_id: MessageFullId) -> Result<MessageDbDialogMessage>;
    fn get_message_by_unique_message_id(
        &mut self,
        unique_message_id: ServerMessageId,
    ) -> Result<MessageDbMessage>;
    fn get_message_by_random_id(
        &mut self,
        dialog_id: DialogId,
        random_id: i64,
    ) -> Result<MessageDbDialogMessage>;
    fn get_dialog_message_by_date(
        &mut self,
        dialog_id: DialogId,
        first_message_id: MessageId,
        last_message_id: MessageId,
        date: i32,
    ) -> Result<MessageDbDialogMessage>;

    fn get_dialog_message_calendar(
        &mut self,
        query: MessageDbDialogCalendarQuery,
    ) -> MessageDbCalendar;

    fn get_dialog_sparse_message_positions(
        &mut self,
        query: MessageDbGetDialogSparseMessagePositionsQuery,
    ) -> Result<MessageDbMessagePositions>;

    fn get_messages(&mut self, query: MessageDbMessagesQuery) -> Vec<MessageDbDialogMessage>;
    fn get_scheduled_messages(
        &mut self,
        dialog_id: DialogId,
        limit: i32,
    ) -> Vec<MessageDbDialogMessage>;
    fn get_messages_from_notification_id(
        &mut self,
        dialog_id: DialogId,
        from_notification_id: NotificationId,
        limit: i32,
    ) -> Vec<MessageDbDialogMessage>;

    fn get_expiring_messages(&mut self, expires_till: i32, limit: i32) -> Vec<MessageDbMessage>;
    fn get_calls(&mut self, query: MessageDbCallsQuery) -> MessageDbCallsResult;
    fn get_messages_fts(&mut self, query: MessageDbFtsQuery) -> MessageDbFtsResult;

    fn begin_write_transaction(&mut self) -> Status;
    fn commit_transaction(&mut self) -> Status;
}

/// `MessageDbImpl::GetMessagesStmt` (MessageDb.cpp:840-843).
pub struct GetMessagesStmt {
    pub asc_stmt: SqliteStatement,
    pub desc_stmt: SqliteStatement,
}

/// `td::MessageDbImpl` (MessageDb.cpp:180). The C++ default-constructs the
/// statement members and fills them in `init()`; the statements are created
/// in [`MessageDbImpl::new`] here instead, before the connection is moved
/// into the struct. The fixed-size statement arrays are `Vec`s.
pub struct MessageDbImpl {
    db: SqliteDb,
    add_message_stmt: SqliteStatement,
    delete_message_stmt: SqliteStatement,
    delete_all_dialog_messages_stmt: SqliteStatement,
    delete_dialog_messages_by_sender_stmt: SqliteStatement,

    get_message_stmt: SqliteStatement,
    get_message_by_random_id_stmt: SqliteStatement,
    get_message_by_unique_message_id_stmt: SqliteStatement,

    get_expiring_messages_stmt: SqliteStatement,

    get_messages_stmt: GetMessagesStmt,
    get_scheduled_messages_stmt: SqliteStatement,
    get_messages_from_notification_id_stmt: SqliteStatement,
    get_messages_fts_stmt: SqliteStatement,

    get_message_ids_stmts: Vec<SqliteStatement>,
    get_messages_from_index_stmts: Vec<GetMessagesStmt>,
    get_calls_stmts: Vec<SqliteStatement>,

    add_scheduled_message_stmt: SqliteStatement,
    get_scheduled_message_stmt: SqliteStatement,
    get_scheduled_server_message_stmt: SqliteStatement,
    delete_scheduled_message_stmt: SqliteStatement,
    delete_scheduled_server_message_stmt: SqliteStatement,
}

impl MessageDbImpl {
    /// `MessageDbImpl(SqliteDb)` + `init` (MessageDb.cpp:182-288).
    pub fn new(db: SqliteDb) -> Result<MessageDbImpl> {
        try_result!(
            add_message_stmt,
            db.get_statement(
                "INSERT OR REPLACE INTO messages VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
            )
        );
        try_result!(
            delete_message_stmt,
            db.get_statement("DELETE FROM messages WHERE dialog_id = ?1 AND message_id = ?2")
        );
        try_result!(
            delete_all_dialog_messages_stmt,
            db.get_statement("DELETE FROM messages WHERE dialog_id = ?1 AND message_id <= ?2")
        );
        try_result!(
            delete_dialog_messages_by_sender_stmt,
            db.get_statement("DELETE FROM messages WHERE dialog_id = ?1 AND sender_user_id = ?2")
        );

        try_result!(
            get_message_stmt,
            db.get_statement(
                "SELECT message_id, data FROM messages WHERE dialog_id = ?1 AND message_id = ?2"
            )
        );
        try_result!(
            get_message_by_random_id_stmt,
            db.get_statement(
                "SELECT message_id, data FROM messages WHERE dialog_id = ?1 AND random_id = ?2"
            )
        );
        try_result!(
            get_message_by_unique_message_id_stmt,
            db.get_statement(
                "SELECT dialog_id, message_id, data FROM messages WHERE unique_message_id = ?1"
            )
        );

        try_result!(
            get_expiring_messages_stmt,
            db.get_statement("SELECT dialog_id, message_id, data FROM messages WHERE ttl_expires_at <= ?1 LIMIT ?2")
        );

        try_result!(
            get_messages_asc,
            db.get_statement(
                "SELECT data, message_id FROM messages WHERE dialog_id = ?1 AND message_id > \
                 ?2 ORDER BY message_id ASC LIMIT ?3"
            )
        );
        try_result!(
            get_messages_desc,
            db.get_statement(
                "SELECT data, message_id FROM messages WHERE dialog_id = ?1 AND message_id < \
                 ?2 ORDER BY message_id DESC LIMIT ?3"
            )
        );
        try_result!(
            get_scheduled_messages_stmt,
            db.get_statement(
                "SELECT data, message_id FROM scheduled_messages WHERE dialog_id = ?1 AND \
                 message_id < ?2 ORDER BY message_id DESC LIMIT ?3"
            )
        );
        try_result!(
            get_messages_from_notification_id_stmt,
            db.get_statement(
                "SELECT data, message_id FROM messages WHERE dialog_id = ?1 AND \
                 notification_id < ?2 ORDER BY notification_id DESC LIMIT ?3"
            )
        );
        try_result!(
            get_messages_fts_stmt,
            db.get_statement(
                "SELECT dialog_id, message_id, data, search_id FROM messages WHERE search_id \
                 IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?1 AND rowid < ?2 \
                 ORDER BY rowid DESC LIMIT ?3) ORDER BY search_id DESC"
            )
        );

        let mut get_message_ids_stmts = Vec::with_capacity(MESSAGE_DB_INDEX_COUNT as usize);
        let mut get_messages_from_index_stmts = Vec::with_capacity(MESSAGE_DB_INDEX_COUNT as usize);
        for i in 0..MESSAGE_DB_INDEX_COUNT {
            try_result!(
                get_message_ids_stmt,
                db.get_statement(&format!(
                    "SELECT message_id FROM messages WHERE dialog_id = ?1 AND message_id < ?2 AND (index_mask & \
                     {}) != 0 ORDER BY message_id DESC LIMIT 1000000",
                    1 << i
                ))
            );
            get_message_ids_stmts.push(get_message_ids_stmt);

            try_result!(
                from_index_desc,
                db.get_statement(&format!(
                    "SELECT data, message_id FROM messages WHERE dialog_id = ?1 AND message_id < ?2 AND (index_mask & \
                     {}) != 0 ORDER BY message_id DESC LIMIT ?3",
                    1 << i
                ))
            );
            try_result!(
                from_index_asc,
                db.get_statement(&format!(
                    "SELECT data, message_id FROM messages WHERE dialog_id = ?1 AND message_id > ?2 AND (index_mask & \
                     {}) != 0 ORDER BY message_id ASC LIMIT ?3",
                    1 << i
                ))
            );
            get_messages_from_index_stmts.push(GetMessagesStmt {
                asc_stmt: from_index_asc,
                desc_stmt: from_index_desc,
            });
        }

        let mut get_calls_stmts = Vec::new();
        let mut pos = 0;
        let mut i = MessageSearchFilter::Call.as_i32() - 1;
        while i < MessageSearchFilter::MissedCall.as_i32() {
            try_result!(
                get_calls_stmt,
                db.get_statement(&format!(
                    "SELECT dialog_id, message_id, data FROM messages WHERE unique_message_id < ?1 AND (index_mask & \
                     {}) != 0 ORDER BY unique_message_id DESC LIMIT ?2",
                    1 << i
                ))
            );
            get_calls_stmts.push(get_calls_stmt);
            pos += 1;
            i += 1;
        }
        debug_assert_eq!(
            pos,
            call_message_search_filter_index(MessageSearchFilter::MissedCall) + 1
        );

        try_result!(
            add_scheduled_message_stmt,
            db.get_statement("INSERT OR REPLACE INTO scheduled_messages VALUES(?1, ?2, ?3, ?4)")
        );
        try_result!(
            get_scheduled_message_stmt,
            db.get_statement(
                "SELECT message_id, data FROM scheduled_messages WHERE dialog_id = ?1 AND message_id = ?2"
            )
        );
        try_result!(
            get_scheduled_server_message_stmt,
            db.get_statement(
                "SELECT message_id, data FROM scheduled_messages WHERE dialog_id = ?1 AND server_message_id = ?2"
            )
        );
        try_result!(
            delete_scheduled_message_stmt,
            db.get_statement(
                "DELETE FROM scheduled_messages WHERE dialog_id = ?1 AND message_id = ?2"
            )
        );
        try_result!(
            delete_scheduled_server_message_stmt,
            db.get_statement(
                "DELETE FROM scheduled_messages WHERE dialog_id = ?1 AND server_message_id = ?2"
            )
        );

        Result::Ok(MessageDbImpl {
            db,
            add_message_stmt,
            delete_message_stmt,
            delete_all_dialog_messages_stmt,
            delete_dialog_messages_by_sender_stmt,
            get_message_stmt,
            get_message_by_random_id_stmt,
            get_message_by_unique_message_id_stmt,
            get_expiring_messages_stmt,
            get_messages_stmt: GetMessagesStmt {
                asc_stmt: get_messages_asc,
                desc_stmt: get_messages_desc,
            },
            get_scheduled_messages_stmt,
            get_messages_from_notification_id_stmt,
            get_messages_fts_stmt,
            get_message_ids_stmts,
            get_messages_from_index_stmts,
            get_calls_stmts,
            add_scheduled_message_stmt,
            get_scheduled_message_stmt,
            get_scheduled_server_message_stmt,
            delete_scheduled_message_stmt,
            delete_scheduled_server_message_stmt,
        })
    }

    /// `MessageDbImpl::add_message` (MessageDb.cpp:290-367).
    #[allow(clippy::too_many_arguments)]
    pub fn add_message(
        &mut self,
        message_full_id: MessageFullId,
        unique_message_id: ServerMessageId,
        sender_dialog_id: DialogId,
        random_id: i64,
        ttl_expires_at: i32,
        index_mask: i32,
        search_id: i64,
        mut text: String,
        notification_id: NotificationId,
        top_thread_message_id: MessageId,
        data: Vec<u8>,
    ) {
        let dialog_id = message_full_id.get_dialog_id();
        let message_id = message_full_id.get_message_id();
        assert!(dialog_id.is_valid(), "invalid {dialog_id:?} {message_id:?}");
        assert!(message_id.is_valid(), "invalid {message_id:?}");

        self.add_message_stmt.reset();
        self.add_message_stmt
            .bind_int64(1, dialog_id.get())
            .ensure();
        self.add_message_stmt
            .bind_int64(2, message_id.get())
            .ensure();

        if unique_message_id.is_valid() {
            self.add_message_stmt
                .bind_int32(3, unique_message_id.get())
                .ensure();
        } else {
            self.add_message_stmt.bind_null(3).ensure();
        }

        if sender_dialog_id.is_valid() {
            self.add_message_stmt
                .bind_int64(4, sender_dialog_id.get())
                .ensure();
        } else {
            self.add_message_stmt.bind_null(4).ensure();
        }

        if random_id != 0 {
            self.add_message_stmt.bind_int64(5, random_id).ensure();
        } else {
            self.add_message_stmt.bind_null(5).ensure();
        }

        self.add_message_stmt.bind_blob(6, &data).ensure();

        if ttl_expires_at != 0 {
            self.add_message_stmt.bind_int32(7, ttl_expires_at).ensure();
        } else {
            self.add_message_stmt.bind_null(7).ensure();
        }

        if index_mask != 0 {
            self.add_message_stmt.bind_int32(8, index_mask).ensure();
        } else {
            self.add_message_stmt.bind_null(8).ensure();
        }
        if search_id != 0 {
            // add dialog_id to text (the C++ `" \a"` literal is byte 0x07)
            text.push_str(&format!(" \u{07}{}", dialog_id.get()));
            if index_mask != 0 {
                for i in 0..MESSAGE_DB_INDEX_COUNT {
                    if (index_mask & (1 << i)) != 0 {
                        text.push_str(&format!(" \u{07}\u{07}{i}"));
                    }
                }
            }
            self.add_message_stmt.bind_int64(9, search_id).ensure();
        } else {
            text = String::new();
            self.add_message_stmt.bind_null(9).ensure();
        }
        if !text.is_empty() {
            self.add_message_stmt.bind_string(10, &text).ensure();
        } else {
            self.add_message_stmt.bind_null(10).ensure();
        }
        if notification_id.is_valid() {
            self.add_message_stmt
                .bind_int32(11, notification_id.get())
                .ensure();
        } else {
            self.add_message_stmt.bind_null(11).ensure();
        }
        if top_thread_message_id.is_valid() {
            self.add_message_stmt
                .bind_int64(12, top_thread_message_id.get())
                .ensure();
        } else {
            self.add_message_stmt.bind_null(12).ensure();
        }

        self.add_message_stmt.step().ensure();
        self.add_message_stmt.reset();
    }

    /// `MessageDbImpl::add_scheduled_message` (MessageDb.cpp:369-390).
    pub fn add_scheduled_message(&mut self, message_full_id: MessageFullId, data: Vec<u8>) {
        let dialog_id = message_full_id.get_dialog_id();
        let message_id = message_full_id.get_message_id();
        assert!(dialog_id.is_valid(), "invalid {dialog_id:?}");
        assert!(message_id.is_valid_scheduled(), "invalid {message_id:?}");

        self.add_scheduled_message_stmt.reset();
        self.add_scheduled_message_stmt
            .bind_int64(1, dialog_id.get())
            .ensure();
        self.add_scheduled_message_stmt
            .bind_int64(2, message_id.get())
            .ensure();

        if message_id.is_scheduled_server() {
            self.add_scheduled_message_stmt
                .bind_int32(3, message_id.get_scheduled_server_message_id().get())
                .ensure();
        } else {
            self.add_scheduled_message_stmt.bind_null(3).ensure();
        }

        self.add_scheduled_message_stmt.bind_blob(4, &data).ensure();

        self.add_scheduled_message_stmt.step().ensure();
        self.add_scheduled_message_stmt.reset();
    }

    /// `MessageDbImpl::delete_message` (MessageDb.cpp:392-413).
    pub fn delete_message(&mut self, message_full_id: MessageFullId) {
        let dialog_id = message_full_id.get_dialog_id();
        let message_id = message_full_id.get_message_id();
        assert!(dialog_id.is_valid(), "invalid {dialog_id:?}");
        assert!(
            message_id.is_valid() || message_id.is_valid_scheduled(),
            "invalid {message_id:?}"
        );
        let is_scheduled = message_id.is_scheduled();
        let is_scheduled_server = is_scheduled && message_id.is_scheduled_server();

        let stmt = if is_scheduled {
            if is_scheduled_server {
                &mut self.delete_scheduled_server_message_stmt
            } else {
                &mut self.delete_scheduled_message_stmt
            }
        } else {
            &mut self.delete_message_stmt
        };
        stmt.reset();
        stmt.bind_int64(1, dialog_id.get()).ensure();
        if is_scheduled_server {
            stmt.bind_int32(2, message_id.get_scheduled_server_message_id().get())
                .ensure();
        } else {
            stmt.bind_int64(2, message_id.get()).ensure();
        }
        stmt.step().ensure();
        stmt.reset();
    }

    /// `MessageDbImpl::delete_all_dialog_messages` (MessageDb.cpp:415-428);
    /// like the C++, a failing step is logged and swallowed.
    pub fn delete_all_dialog_messages(&mut self, dialog_id: DialogId, from_message_id: MessageId) {
        assert!(dialog_id.is_valid(), "invalid {dialog_id:?}");
        assert!(from_message_id.is_valid(), "invalid {from_message_id:?}");
        self.delete_all_dialog_messages_stmt.reset();
        self.delete_all_dialog_messages_stmt
            .bind_int64(1, dialog_id.get())
            .ensure();
        self.delete_all_dialog_messages_stmt
            .bind_int64(2, from_message_id.get())
            .ensure();
        let status = self.delete_all_dialog_messages_stmt.step();
        if status.is_error() {
            tracing::error!("{status}");
        }
        self.delete_all_dialog_messages_stmt.reset();
    }

    /// `MessageDbImpl::delete_dialog_messages_by_sender`
    /// (MessageDb.cpp:430-440); the `sender_user_id` column stores a dialog
    /// id, like the C++.
    pub fn delete_dialog_messages_by_sender(
        &mut self,
        dialog_id: DialogId,
        sender_dialog_id: DialogId,
    ) {
        assert!(dialog_id.is_valid(), "invalid {dialog_id:?}");
        assert!(sender_dialog_id.is_valid(), "invalid {sender_dialog_id:?}");
        self.delete_dialog_messages_by_sender_stmt.reset();
        self.delete_dialog_messages_by_sender_stmt
            .bind_int64(1, dialog_id.get())
            .ensure();
        self.delete_dialog_messages_by_sender_stmt
            .bind_int64(2, sender_dialog_id.get())
            .ensure();
        self.delete_dialog_messages_by_sender_stmt.step().ensure();
        self.delete_dialog_messages_by_sender_stmt.reset();
    }

    /// `MessageDbImpl::get_message` (MessageDb.cpp:442-476).
    pub fn get_message(
        &mut self,
        message_full_id: MessageFullId,
    ) -> Result<MessageDbDialogMessage> {
        let dialog_id = message_full_id.get_dialog_id();
        let message_id = message_full_id.get_message_id();
        assert!(dialog_id.is_valid(), "invalid {dialog_id:?}");
        assert!(
            message_id.is_valid() || message_id.is_valid_scheduled(),
            "invalid {message_id:?}"
        );
        let is_scheduled = message_id.is_scheduled();
        let is_scheduled_server = is_scheduled && message_id.is_scheduled_server();

        let stmt = if is_scheduled {
            if is_scheduled_server {
                &mut self.get_scheduled_server_message_stmt
            } else {
                &mut self.get_scheduled_message_stmt
            }
        } else {
            &mut self.get_message_stmt
        };
        stmt.bind_int64(1, dialog_id.get()).ensure();
        if is_scheduled_server {
            stmt.bind_int32(2, message_id.get_scheduled_server_message_id().get())
                .ensure();
        } else {
            stmt.bind_int64(2, message_id.get()).ensure();
        }
        stmt.step().ensure();
        if !stmt.has_row() {
            stmt.reset();
            return Result::Err(Status::error_message("Not found"));
        }
        let received_message_id = MessageId::new(stmt.view_int64(0));
        let data = stmt.view_blob(1).to_vec();
        if is_scheduled_server {
            assert!(
                received_message_id.is_scheduled(),
                "invalid {received_message_id:?}"
            );
            assert!(
                received_message_id.is_scheduled_server(),
                "invalid {received_message_id:?}"
            );
            assert_eq!(
                received_message_id.get_scheduled_server_message_id(),
                message_id.get_scheduled_server_message_id()
            );
        } else {
            assert!(
                received_message_id == message_id,
                "{received_message_id:?} != {message_id:?} (embedded {:?})",
                MessageDbImpl::get_message_info(received_message_id, &data, true).0
            );
        }
        stmt.reset();
        Result::Ok(MessageDbDialogMessage {
            message_id: received_message_id,
            data,
        })
    }

    /// `MessageDbImpl::get_message_by_unique_message_id`
    /// (MessageDb.cpp:478-493).
    pub fn get_message_by_unique_message_id(
        &mut self,
        unique_message_id: ServerMessageId,
    ) -> Result<MessageDbMessage> {
        if !unique_message_id.is_valid() {
            return Result::Err(Status::error_message("Invalid unique_message_id"));
        }
        self.get_message_by_unique_message_id_stmt.reset();
        self.get_message_by_unique_message_id_stmt
            .bind_int32(1, unique_message_id.get())
            .ensure();
        self.get_message_by_unique_message_id_stmt.step().ensure();
        if !self.get_message_by_unique_message_id_stmt.has_row() {
            self.get_message_by_unique_message_id_stmt.reset();
            return Result::Err(Status::error_message("Not found"));
        }
        let dialog_id = DialogId::new(self.get_message_by_unique_message_id_stmt.view_int64(0));
        let message_id = MessageId::new(self.get_message_by_unique_message_id_stmt.view_int64(1));
        let data = self
            .get_message_by_unique_message_id_stmt
            .view_blob(2)
            .to_vec();
        self.get_message_by_unique_message_id_stmt.reset();
        Result::Ok(MessageDbMessage {
            dialog_id,
            message_id,
            data,
        })
    }

    /// `MessageDbImpl::get_message_by_random_id` (MessageDb.cpp:495-507).
    pub fn get_message_by_random_id(
        &mut self,
        dialog_id: DialogId,
        random_id: i64,
    ) -> Result<MessageDbDialogMessage> {
        self.get_message_by_random_id_stmt.reset();
        self.get_message_by_random_id_stmt
            .bind_int64(1, dialog_id.get())
            .ensure();
        self.get_message_by_random_id_stmt
            .bind_int64(2, random_id)
            .ensure();
        self.get_message_by_random_id_stmt.step().ensure();
        if !self.get_message_by_random_id_stmt.has_row() {
            self.get_message_by_random_id_stmt.reset();
            return Result::Err(Status::error_message("Not found"));
        }
        let message_id = MessageId::new(self.get_message_by_random_id_stmt.view_int64(0));
        let data = self.get_message_by_random_id_stmt.view_blob(1).to_vec();
        self.get_message_by_random_id_stmt.reset();
        Result::Ok(MessageDbDialogMessage { message_id, data })
    }

    /// `MessageDbImpl::get_dialog_message_by_date`
    /// (MessageDb.cpp:509-569): the binary search walks the asc statement
    /// and reads dates through the blob-header parser.
    pub fn get_dialog_message_by_date(
        &mut self,
        dialog_id: DialogId,
        first_message_id: MessageId,
        last_message_id: MessageId,
        date: i32,
    ) -> Result<MessageDbDialogMessage> {
        let mut left_message_id = first_message_id.get();
        let mut right_message_id = last_message_id.get();
        assert!(
            left_message_id <= right_message_id,
            "{first_message_id:?} {last_message_id:?}"
        );

        let first_messages = MessageDbImpl::get_messages_inner(
            &mut self.get_messages_stmt.asc_stmt,
            dialog_id,
            left_message_id - 1,
            1,
        );
        if !first_messages.is_empty() {
            let (real_first_message_id, real_first_message_date) =
                MessageDbImpl::get_message_info_of(&first_messages[0], false);
            if real_first_message_date <= date {
                // we definitely have at least one suitable message, let's do a binary search
                left_message_id = real_first_message_id.get();

                let mut prev_found_message_id = MessageId::new(0);
                while left_message_id <= right_message_id {
                    let middle_message_id =
                        left_message_id + ((right_message_id - left_message_id) >> 1);
                    let messages = MessageDbImpl::get_messages_inner(
                        &mut self.get_messages_stmt.asc_stmt,
                        dialog_id,
                        middle_message_id,
                        1,
                    );

                    let mut message_id = MessageId::new(0);
                    let mut message_date = i32::MAX;
                    if !messages.is_empty() {
                        let (found_id, found_date) =
                            MessageDbImpl::get_message_info_of(&messages[0], false);
                        message_id = found_id;
                        message_date = found_date;
                    }
                    if message_date <= date {
                        left_message_id = message_id.get();
                    } else {
                        right_message_id = middle_message_id - 1;
                    }

                    if prev_found_message_id == message_id {
                        // we may be very close to the result, let's check
                        let left_messages = MessageDbImpl::get_messages_inner(
                            &mut self.get_messages_stmt.asc_stmt,
                            dialog_id,
                            left_message_id - 1,
                            2,
                        );
                        assert!(!left_messages.is_empty());
                        if left_messages.len() == 1 {
                            // only one message has left, result is found
                            break;
                        }

                        let (next_message_id, next_message_date) =
                            MessageDbImpl::get_message_info_of(&left_messages[1], false);
                        if next_message_date <= date {
                            // next message has lesser date, adjusting left message
                            left_message_id = next_message_id.get();
                        } else {
                            // next message has bigger date, result is found
                            break;
                        }
                    }

                    prev_found_message_id = message_id;
                }

                // left_message_id is always an identifier of suitable message, let's return it
                return self.get_message(MessageFullId::new(
                    dialog_id,
                    MessageId::new(left_message_id),
                ));
            }
        }

        Result::Err(Status::error_message("Not found"))
    }

    /// `MessageDbImpl::get_expiring_messages` (MessageDb.cpp:571-590).
    pub fn get_expiring_messages(
        &mut self,
        expires_till: i32,
        limit: i32,
    ) -> Vec<MessageDbMessage> {
        self.get_expiring_messages_stmt.reset();
        let mut messages = Vec::new();
        self.get_expiring_messages_stmt
            .bind_int32(1, expires_till)
            .ensure();
        self.get_expiring_messages_stmt
            .bind_int32(2, limit)
            .ensure();
        self.get_expiring_messages_stmt.step().ensure();

        while self.get_expiring_messages_stmt.has_row() {
            let dialog_id = DialogId::new(self.get_expiring_messages_stmt.view_int64(0));
            let message_id = MessageId::new(self.get_expiring_messages_stmt.view_int64(1));
            let data = self.get_expiring_messages_stmt.view_blob(2).to_vec();
            messages.push(MessageDbMessage {
                dialog_id,
                message_id,
                data,
            });
            self.get_expiring_messages_stmt.step().ensure();
        }
        self.get_expiring_messages_stmt.reset();
        messages
    }

    /// `MessageDbImpl::get_dialog_message_calendar`
    /// (MessageDb.cpp:592-622).
    pub fn get_dialog_message_calendar(
        &mut self,
        query: MessageDbDialogCalendarQuery,
    ) -> MessageDbCalendar {
        let limit = 1000;
        let stmt = &mut self.get_messages_from_index_stmts
            [message_search_filter_index(query.filter) as usize]
            .desc_stmt;
        stmt.bind_int64(1, query.dialog_id.get()).ensure();
        stmt.bind_int64(2, query.from_message_id.get()).ensure();
        stmt.bind_int32(3, limit).ensure();

        let mut messages = Vec::new();
        let mut total_counts: Vec<i32> = Vec::new();
        stmt.step().ensure();
        let mut current_day = i32::MAX;
        while stmt.has_row() {
            let data = stmt.view_blob(0).to_vec();
            let message_id = MessageId::new(stmt.view_int64(1));
            let info = MessageDbImpl::get_message_info(message_id, &data, false);
            let day = (query.tz_offset + info.1) / 86400;
            if day >= current_day {
                assert!(!total_counts.is_empty());
                *total_counts.last_mut().unwrap() += 1;
            } else {
                current_day = day;
                messages.push(MessageDbDialogMessage { message_id, data });
                total_counts.push(1);
            }
            stmt.step().ensure();
        }
        stmt.reset();
        MessageDbCalendar {
            messages,
            total_counts,
        }
    }

    /// `MessageDbImpl::get_dialog_sparse_message_positions`
    /// (MessageDb.cpp:624-655).
    pub fn get_dialog_sparse_message_positions(
        &mut self,
        query: MessageDbGetDialogSparseMessagePositionsQuery,
    ) -> Result<MessageDbMessagePositions> {
        let stmt =
            &mut self.get_message_ids_stmts[message_search_filter_index(query.filter) as usize];
        stmt.bind_int64(1, query.dialog_id.get()).ensure();
        stmt.bind_int64(2, query.from_message_id.get()).ensure();

        let mut message_ids = Vec::new();
        stmt.step().ensure();
        while stmt.has_row() {
            message_ids.push(MessageId::new(stmt.view_int64(0)));
            stmt.step().ensure();
        }
        stmt.reset();

        let mut positions = MessageDbMessagePositions::default();
        let limit = i32::min(query.limit, message_ids.len() as i32);
        if limit > 0 {
            let delta = message_ids.len() as f64 / limit as f64;
            positions.total_count = message_ids.len() as i32;
            for i in 0..limit {
                let position = ((i as f64 + 0.5) * delta) as i32;
                let message_id = message_ids[position as usize];
                try_result!(
                    message,
                    self.get_message(MessageFullId::new(query.dialog_id, message_id))
                );
                let date = MessageDbImpl::get_message_info_of(&message, false).1;
                positions.positions.push(MessageDbMessagePosition {
                    position,
                    date,
                    message_id,
                });
            }
        }
        Result::Ok(positions)
    }

    /// `MessageDbImpl::get_messages` (MessageDb.cpp:657-662).
    pub fn get_messages(&mut self, query: MessageDbMessagesQuery) -> Vec<MessageDbDialogMessage> {
        if query.filter != MessageSearchFilter::Empty {
            return MessageDbImpl::get_messages_from_index(
                self,
                query.dialog_id,
                query.from_message_id,
                query.filter,
                query.offset,
                query.limit,
            );
        }
        MessageDbImpl::get_messages_impl(
            &mut self.get_messages_stmt,
            query.dialog_id,
            query.from_message_id,
            query.offset,
            query.limit,
        )
    }

    /// `MessageDbImpl::get_scheduled_messages` (MessageDb.cpp:664-666).
    pub fn get_scheduled_messages(
        &mut self,
        dialog_id: DialogId,
        limit: i32,
    ) -> Vec<MessageDbDialogMessage> {
        MessageDbImpl::get_messages_inner(
            &mut self.get_scheduled_messages_stmt,
            dialog_id,
            i64::MAX,
            limit,
        )
    }

    /// `MessageDbImpl::get_messages_from_notification_id`
    /// (MessageDb.cpp:668-689).
    pub fn get_messages_from_notification_id(
        &mut self,
        dialog_id: DialogId,
        from_notification_id: NotificationId,
        limit: i32,
    ) -> Vec<MessageDbDialogMessage> {
        let stmt = &mut self.get_messages_from_notification_id_stmt;
        stmt.bind_int64(1, dialog_id.get()).ensure();
        stmt.bind_int32(2, from_notification_id.get()).ensure();
        stmt.bind_int32(3, limit).ensure();

        let mut result = Vec::new();
        stmt.step().ensure();
        while stmt.has_row() {
            let data = stmt.view_blob(0).to_vec();
            let message_id = MessageId::new(stmt.view_int64(1));
            result.push(MessageDbDialogMessage { message_id, data });
            stmt.step().ensure();
        }
        stmt.reset();
        result
    }

    /// `MessageDbImpl::get_messages_from_index` (MessageDb.cpp:783-787).
    fn get_messages_from_index(
        &mut self,
        dialog_id: DialogId,
        from_message_id: MessageId,
        filter: MessageSearchFilter,
        offset: i32,
        limit: i32,
    ) -> Vec<MessageDbDialogMessage> {
        let stmt =
            &mut self.get_messages_from_index_stmts[message_search_filter_index(filter) as usize];
        MessageDbImpl::get_messages_impl(stmt, dialog_id, from_message_id, offset, limit)
    }

    /// `MessageDbImpl::get_messages_impl` (MessageDb.cpp:860-910). The C++
    /// `&& false` dead branches (:884-893) are not ported; only the live
    /// semantics remain.
    fn get_messages_impl(
        stmt: &mut GetMessagesStmt,
        dialog_id: DialogId,
        from_message_id: MessageId,
        offset: i32,
        limit: i32,
    ) -> Vec<MessageDbDialogMessage> {
        assert!(dialog_id.is_valid(), "invalid {dialog_id:?}");
        assert!(from_message_id.is_valid(), "invalid {from_message_id:?}");

        let mut message_id = from_message_id.get();
        if message_id >= MessageId::MAX.get() {
            message_id -= 1;
        }

        let left_message_id = message_id;
        let left_cnt = limit + offset;

        let right_message_id = message_id - 1;
        let right_cnt = -offset;

        let mut left = Vec::new();
        let mut right = Vec::new();

        if left_cnt != 0 {
            left = MessageDbImpl::get_messages_inner(
                &mut stmt.desc_stmt,
                dialog_id,
                left_message_id,
                left_cnt,
            );
        }
        if right_cnt != 0 {
            right = MessageDbImpl::get_messages_inner(
                &mut stmt.asc_stmt,
                dialog_id,
                right_message_id,
                right_cnt,
            );
            right.reverse();
        }
        if left.is_empty() {
            return right;
        }
        if right.is_empty() {
            return left;
        }

        right.reserve(right.len() + left.len());
        right.extend(left);
        right
    }

    /// `MessageDbImpl::get_messages_inner` (MessageDb.cpp:912-933).
    fn get_messages_inner(
        stmt: &mut SqliteStatement,
        dialog_id: DialogId,
        from_message_id: i64,
        limit: i32,
    ) -> Vec<MessageDbDialogMessage> {
        stmt.reset();
        stmt.bind_int64(1, dialog_id.get()).ensure();
        stmt.bind_int64(2, from_message_id).ensure();
        stmt.bind_int32(3, limit).ensure();

        let mut result = Vec::new();
        stmt.step().ensure();
        while stmt.has_row() {
            let data = stmt.view_blob(0).to_vec();
            let message_id = MessageId::new(stmt.view_int64(1));
            result.push(MessageDbDialogMessage { message_id, data });
            stmt.step().ensure();
        }
        stmt.reset();
        result
    }

    /// `MessageDbImpl::prepare_query` (MessageDb.cpp:691-736): shape the
    /// raw query into FTS5 quoted words. The C++ StringBuilder overflow
    /// branch is unreachable with a growable `String`.
    fn prepare_query(query: &str) -> String {
        let max_len = utf8_truncate(query.as_bytes(), 1024);
        let query = &query[..max_len];

        let mut sb = String::with_capacity(query.len() * 4 + 100);
        let mut in_word = false;
        let bytes = query.as_bytes();
        let mut pos = 0usize;
        while pos < bytes.len() {
            let code_start = pos;
            let (code, used) = next_utf8_unsafe(&bytes[pos..]);
            pos += used;
            if MessageDbImpl::is_fts_word_character(code) {
                if !in_word {
                    in_word = true;
                    sb.push('"');
                }
                sb.push_str(&query[code_start..pos]);
            } else if in_word {
                in_word = false;
                sb.push_str("\" ");
            }
        }
        if in_word {
            sb.push_str("\" ");
        }
        sb
    }

    /// the `is_word_character` lambda (MessageDb.cpp:692-701).
    fn is_fts_word_character(a: u32) -> bool {
        match get_unicode_simple_category(a) {
            UnicodeSimpleCategory::Letter
            | UnicodeSimpleCategory::DecimalNumber
            | UnicodeSimpleCategory::Number => true,
            _ => a == b'_' as u32,
        }
    }

    /// `MessageDbImpl::get_messages_fts` (MessageDb.cpp:738-781).
    pub fn get_messages_fts(&mut self, mut query: MessageDbFtsQuery) -> MessageDbFtsResult {
        let mut words = MessageDbImpl::prepare_query(&query.query);

        // dialog_id kludge
        if query.dialog_id.is_valid() {
            words.push_str(&format!(" \"\u{07}{}\"", query.dialog_id.get()));
        }

        // index_mask kludge
        if query.filter != MessageSearchFilter::Empty {
            words.push_str(&format!(
                " \"\u{07}\u{07}{}\"",
                message_search_filter_index(query.filter)
            ));
        }

        let stmt = &mut self.get_messages_fts_stmt;
        stmt.reset();
        stmt.bind_string(1, &words).ensure();
        if query.from_search_id == 0 {
            query.from_search_id = i64::MAX;
        }
        stmt.bind_int64(2, query.from_search_id).ensure();
        stmt.bind_int32(3, query.limit).ensure();
        let mut result = MessageDbFtsResult::default();
        let status = stmt.step();
        if status.is_error() {
            tracing::error!("{status}");
            stmt.reset();
            return result;
        }
        while stmt.has_row() {
            let dialog_id = DialogId::new(stmt.view_int64(0));
            let message_id = MessageId::new(stmt.view_int64(1));
            let data = stmt.view_blob(2).to_vec();
            let search_id = stmt.view_int64(3);
            result.next_search_id = search_id;
            result.messages.push(MessageDbMessage {
                dialog_id,
                message_id,
                data,
            });
            stmt.step().ensure();
        }
        stmt.reset();
        result
    }

    /// `MessageDbImpl::get_calls` (MessageDb.cpp:789-817).
    pub fn get_calls(&mut self, query: MessageDbCallsQuery) -> MessageDbCallsResult {
        let pos = if query.filter == MessageSearchFilter::Call {
            0
        } else if query.filter == MessageSearchFilter::MissedCall {
            1
        } else {
            unreachable!()
        };

        let stmt = &mut self.get_calls_stmts[pos];
        stmt.reset();
        stmt.bind_int32(1, query.from_unique_message_id).ensure();
        stmt.bind_int32(2, query.limit).ensure();

        let mut result = MessageDbCallsResult::default();
        stmt.step().ensure();
        while stmt.has_row() {
            let dialog_id = DialogId::new(stmt.view_int64(0));
            let message_id = MessageId::new(stmt.view_int64(1));
            let data = stmt.view_blob(2).to_vec();
            result.messages.push(MessageDbMessage {
                dialog_id,
                message_id,
                data,
            });
            stmt.step().ensure();
        }
        stmt.reset();
        result
    }

    /// `MessageDbImpl::begin_write_transaction` (MessageDb.cpp:819-821).
    pub fn begin_write_transaction(&mut self) -> Status {
        self.db.begin_write_transaction()
    }

    /// `MessageDbImpl::commit_transaction` (MessageDb.cpp:822-824).
    pub fn commit_transaction(&mut self) -> Status {
        self.db.commit_transaction()
    }

    /// `MessageDbImpl::get_message_info` (MessageDb.cpp:935-967): parse the
    /// log-event header — the flags/flags2/flags3/flags4 `1<<29` chain, the
    /// `1<<10` `has_sender` bit, the embedded message id, the optional
    /// sender and the date. C++ leaves parser errors unchecked (an
    /// under-long blob yields zeroed fields); `unwrap_or(0)` matches that.
    pub(crate) fn get_message_info(
        message_id: MessageId,
        data: &[u8],
        from_data: bool,
    ) -> (MessageId, i32) {
        let mut parser = match Parser::new_log_event(data) {
            Ok(parser) => parser,
            Err(_) => {
                // the C++ LOG_CHECK(version() < Next) aborts on a bad
                // version; a truncated blob has no version at all and the
                // parse chain below sees only zeros
                return (
                    if from_data {
                        MessageId::new(0)
                    } else {
                        message_id
                    },
                    0,
                );
            }
        };
        let flags = parser.i32_().unwrap_or(0);
        if (flags & (1 << 29)) != 0 {
            let flags2 = parser.i32_().unwrap_or(0);
            if (flags2 & (1 << 29)) != 0 {
                let flags3 = parser.i32_().unwrap_or(0);
                if (flags3 & (1 << 29)) != 0 {
                    let _flags4 = parser.i32_().unwrap_or(0);
                }
            }
        }
        let has_sender = (flags & (1 << 10)) != 0;
        let data_message_id = MessageId::parse_from(&mut parser).unwrap_or(MessageId::new(0));
        if has_sender {
            // `td::parse(UserId, parser)` is version-gated (UserId.h:57-64)
            if parser.version() >= store_parse::version::SUPPORT_64_BIT_IDS {
                let _ = parser.i64_().unwrap_or(0);
            } else {
                let _ = parser.i32_().unwrap_or(0);
            }
        }
        let date = parser.i32_().unwrap_or(0);
        (
            if from_data {
                data_message_id
            } else {
                message_id
            },
            date,
        )
    }

    /// `MessageDbImpl::get_message_info(message)` (MessageDb.cpp:935-937).
    pub(crate) fn get_message_info_of(
        message: &MessageDbDialogMessage,
        from_data: bool,
    ) -> (MessageId, i32) {
        MessageDbImpl::get_message_info(message.message_id, &message.data, from_data)
    }
}

/// The `MessageDbImpl final` overrides (MessageDb.cpp:290-824) — every trait
/// method forwards to the inherent method of the same name.
#[allow(clippy::too_many_arguments)]
impl MessageDbSyncInterface for MessageDbImpl {
    fn add_message(
        &mut self,
        message_full_id: MessageFullId,
        unique_message_id: ServerMessageId,
        sender_dialog_id: DialogId,
        random_id: i64,
        ttl_expires_at: i32,
        index_mask: i32,
        search_id: i64,
        text: String,
        notification_id: NotificationId,
        top_thread_message_id: MessageId,
        data: Vec<u8>,
    ) {
        MessageDbImpl::add_message(
            self,
            message_full_id,
            unique_message_id,
            sender_dialog_id,
            random_id,
            ttl_expires_at,
            index_mask,
            search_id,
            text,
            notification_id,
            top_thread_message_id,
            data,
        )
    }

    fn add_scheduled_message(&mut self, message_full_id: MessageFullId, data: Vec<u8>) {
        MessageDbImpl::add_scheduled_message(self, message_full_id, data)
    }

    fn delete_message(&mut self, message_full_id: MessageFullId) {
        MessageDbImpl::delete_message(self, message_full_id)
    }

    fn delete_all_dialog_messages(&mut self, dialog_id: DialogId, from_message_id: MessageId) {
        MessageDbImpl::delete_all_dialog_messages(self, dialog_id, from_message_id)
    }

    fn delete_dialog_messages_by_sender(
        &mut self,
        dialog_id: DialogId,
        sender_dialog_id: DialogId,
    ) {
        MessageDbImpl::delete_dialog_messages_by_sender(self, dialog_id, sender_dialog_id)
    }

    fn get_message(&mut self, message_full_id: MessageFullId) -> Result<MessageDbDialogMessage> {
        MessageDbImpl::get_message(self, message_full_id)
    }

    fn get_message_by_unique_message_id(
        &mut self,
        unique_message_id: ServerMessageId,
    ) -> Result<MessageDbMessage> {
        MessageDbImpl::get_message_by_unique_message_id(self, unique_message_id)
    }

    fn get_message_by_random_id(
        &mut self,
        dialog_id: DialogId,
        random_id: i64,
    ) -> Result<MessageDbDialogMessage> {
        MessageDbImpl::get_message_by_random_id(self, dialog_id, random_id)
    }

    fn get_dialog_message_by_date(
        &mut self,
        dialog_id: DialogId,
        first_message_id: MessageId,
        last_message_id: MessageId,
        date: i32,
    ) -> Result<MessageDbDialogMessage> {
        MessageDbImpl::get_dialog_message_by_date(
            self,
            dialog_id,
            first_message_id,
            last_message_id,
            date,
        )
    }

    fn get_dialog_message_calendar(
        &mut self,
        query: MessageDbDialogCalendarQuery,
    ) -> MessageDbCalendar {
        MessageDbImpl::get_dialog_message_calendar(self, query)
    }

    fn get_dialog_sparse_message_positions(
        &mut self,
        query: MessageDbGetDialogSparseMessagePositionsQuery,
    ) -> Result<MessageDbMessagePositions> {
        MessageDbImpl::get_dialog_sparse_message_positions(self, query)
    }

    fn get_messages(&mut self, query: MessageDbMessagesQuery) -> Vec<MessageDbDialogMessage> {
        MessageDbImpl::get_messages(self, query)
    }

    fn get_scheduled_messages(
        &mut self,
        dialog_id: DialogId,
        limit: i32,
    ) -> Vec<MessageDbDialogMessage> {
        MessageDbImpl::get_scheduled_messages(self, dialog_id, limit)
    }

    fn get_messages_from_notification_id(
        &mut self,
        dialog_id: DialogId,
        from_notification_id: NotificationId,
        limit: i32,
    ) -> Vec<MessageDbDialogMessage> {
        MessageDbImpl::get_messages_from_notification_id(
            self,
            dialog_id,
            from_notification_id,
            limit,
        )
    }

    fn get_expiring_messages(&mut self, expires_till: i32, limit: i32) -> Vec<MessageDbMessage> {
        MessageDbImpl::get_expiring_messages(self, expires_till, limit)
    }

    fn get_calls(&mut self, query: MessageDbCallsQuery) -> MessageDbCallsResult {
        MessageDbImpl::get_calls(self, query)
    }

    fn get_messages_fts(&mut self, query: MessageDbFtsQuery) -> MessageDbFtsResult {
        MessageDbImpl::get_messages_fts(self, query)
    }

    fn begin_write_transaction(&mut self) -> Status {
        MessageDbImpl::begin_write_transaction(self)
    }

    fn commit_transaction(&mut self) -> Status {
        MessageDbImpl::commit_transaction(self)
    }
}

/// `MessageDbSyncSafe` (MessageDb.cpp:970-987): the port follows the shared
/// connection subset of [`SqliteConnectionSafe`] (see that module's docs) —
/// one lazily-created [`MessageDbImpl`] behind a mutex instead of a
/// per-scheduler-thread lsls instance.
pub struct MessageDbSyncSafe {
    inner: Mutex<Option<MessageDbImpl>>,
    safe_connection: Arc<SqliteConnectionSafe>,
}

impl MessageDbSyncSafe {
    /// `create_message_db_sync`.
    pub fn new(safe_connection: Arc<SqliteConnectionSafe>) -> MessageDbSyncSafe {
        MessageDbSyncSafe {
            inner: Mutex::new(None),
            safe_connection,
        }
    }

    /// `MessageDbSyncSafeInterface::get()` — the closure runs against the
    /// (lazily created, on first use) implementation.
    pub fn with_db<R>(&self, f: impl FnOnce(&mut MessageDbImpl) -> R) -> R {
        let mut inner = self.inner.lock().unwrap();
        if inner.is_none() {
            let db = MessageDbImpl::new(self.safe_connection.get().clone()).unwrap();
            *inner = Some(db);
        }
        f(inner.as_mut().unwrap())
    }

    /// dropping the last reference closes the implementation.
    pub fn close(&self) {
        *self.inner.lock().unwrap() = None;
    }
}

/// `td::store_log_event`-style helper used by the tests: prefix the version
/// int the way `LogEventStorerUnsafe` does.
#[cfg(test)]
fn store_message_blob(flags: i32, rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    store_parse::store_log_event_version(&mut out);
    store_parse::store_i32(&mut out, flags);
    out.extend_from_slice(rest);
    out
}

/// `MessageDbAsyncInterface` (MessageDb.h:151-194).
pub trait MessageDbAsyncInterface: Send {
    #[allow(clippy::too_many_arguments)]
    fn add_message(
        &self,
        message_full_id: MessageFullId,
        unique_message_id: ServerMessageId,
        sender_dialog_id: DialogId,
        random_id: i64,
        ttl_expires_at: i32,
        index_mask: i32,
        search_id: i64,
        text: String,
        notification_id: NotificationId,
        top_thread_message_id: MessageId,
        data: Vec<u8>,
        promise: Promise<()>,
    );
    fn add_scheduled_message(
        &self,
        message_full_id: MessageFullId,
        data: Vec<u8>,
        promise: Promise<()>,
    );

    fn delete_message(&self, message_full_id: MessageFullId, promise: Promise<()>);
    fn delete_all_dialog_messages(
        &self,
        dialog_id: DialogId,
        from_message_id: MessageId,
        promise: Promise<()>,
    );
    fn delete_dialog_messages_by_sender(
        &self,
        dialog_id: DialogId,
        sender_dialog_id: DialogId,
        promise: Promise<()>,
    );

    fn get_message(&self, message_full_id: MessageFullId, promise: Promise<MessageDbDialogMessage>);
    fn get_message_by_unique_message_id(
        &self,
        unique_message_id: ServerMessageId,
        promise: Promise<MessageDbMessage>,
    );
    fn get_message_by_random_id(
        &self,
        dialog_id: DialogId,
        random_id: i64,
        promise: Promise<MessageDbDialogMessage>,
    );
    fn get_dialog_message_by_date(
        &self,
        dialog_id: DialogId,
        first_message_id: MessageId,
        last_message_id: MessageId,
        date: i32,
        promise: Promise<MessageDbDialogMessage>,
    );

    fn get_dialog_message_calendar(
        &self,
        query: MessageDbDialogCalendarQuery,
        promise: Promise<MessageDbCalendar>,
    );

    fn get_dialog_sparse_message_positions(
        &self,
        query: MessageDbGetDialogSparseMessagePositionsQuery,
        promise: Promise<MessageDbMessagePositions>,
    );

    fn get_messages(
        &self,
        query: MessageDbMessagesQuery,
        promise: Promise<Vec<MessageDbDialogMessage>>,
    );
    fn get_scheduled_messages(
        &self,
        dialog_id: DialogId,
        limit: i32,
        promise: Promise<Vec<MessageDbDialogMessage>>,
    );
    fn get_messages_from_notification_id(
        &self,
        dialog_id: DialogId,
        from_notification_id: NotificationId,
        limit: i32,
        promise: Promise<Vec<MessageDbDialogMessage>>,
    );

    fn get_calls(&self, query: MessageDbCallsQuery, promise: Promise<MessageDbCallsResult>);
    fn get_messages_fts(&self, query: MessageDbFtsQuery, promise: Promise<MessageDbFtsResult>);

    fn get_expiring_messages(
        &self,
        expires_till: i32,
        limit: i32,
        promise: Promise<Vec<MessageDbMessage>>,
    );

    fn close(&self, promise: Promise<()>);
    fn force_flush(&self);
}

/// `MessageDbAsync` (MessageDb.cpp:989-1234): the forwarding wrapper around
/// the [`MessageDbAsyncImpl`] actor, created with
/// `create_actor_on_scheduler` (MessageDb.cpp:991-993).
pub struct MessageDbAsync {
    impl_: ActorOwn<MessageDbAsyncImpl>,
}

impl MessageDbAsync {
    /// `MessageDbAsync(sync_db, scheduler_id)` + `create_message_db_async`
    /// (MessageDb.cpp:1236-1239); the C++ default scheduler id is -1.
    pub fn new(sync_db: Arc<MessageDbSyncSafe>, scheduler_id: i32) -> MessageDbAsync {
        MessageDbAsync {
            impl_: create_actor_on_scheduler(
                "MessageDbActor",
                scheduler_id,
                MessageDbAsyncImpl::new(sync_db),
            ),
        }
    }
}

/// `MessageDbAsync::Impl` (MessageDb.cpp:1071-1232). The C++ buffers write
/// queries as `PromiseCreator::lambda`s capturing `this`; here they are
/// buffered as the [`WriteQuery`] enum, which keeps the exact batching
/// order (writes run inside one transaction, promises fire after commit)
/// without the raw-pointer capture.
enum WriteQuery {
    Add {
        message_full_id: MessageFullId,
        unique_message_id: ServerMessageId,
        sender_dialog_id: DialogId,
        random_id: i64,
        ttl_expires_at: i32,
        index_mask: i32,
        search_id: i64,
        text: String,
        notification_id: NotificationId,
        top_thread_message_id: MessageId,
        data: Vec<u8>,
        promise: Promise<()>,
    },
    AddScheduled {
        message_full_id: MessageFullId,
        data: Vec<u8>,
        promise: Promise<()>,
    },
    Delete {
        message_full_id: MessageFullId,
        promise: Promise<()>,
    },
}

/// `MAX_PENDING_QUERY_COUNT` / `MAX_PENDING_QUERY_DELAY`
/// (MessageDb.cpp:1191-1192).
const MAX_PENDING_QUERY_COUNT: usize = 50;
const MAX_PENDING_QUERY_DELAY: f64 = 0.01;

pub struct MessageDbAsyncImpl {
    sync_db_safe: Option<Arc<MessageDbSyncSafe>>,
    finished_writes: Vec<Promise<()>>,
    pending_writes: Vec<WriteQuery>,
    wakeup_at: f64,
}

impl MessageDbAsyncImpl {
    fn new(sync_db_safe: Arc<MessageDbSyncSafe>) -> MessageDbAsyncImpl {
        MessageDbAsyncImpl {
            sync_db_safe: Some(sync_db_safe),
            finished_writes: Vec::new(),
            pending_writes: Vec::new(),
            wakeup_at: 0.0,
        }
    }

    /// `Impl::sync_db_->` — run a closure against the implementation.
    fn run_sync<R>(&mut self, f: impl FnOnce(&mut dyn MessageDbSyncInterface) -> R) -> R {
        let sync_db_safe = self
            .sync_db_safe
            .as_ref()
            .expect("MessageDbAsyncImpl used after close");
        sync_db_safe.with_db(|db| f(db))
    }

    /// `Impl::on_write_result` (MessageDb.cpp:1102-1105): we are inside a
    /// transaction and don't know how to handle errors.
    fn on_write_result(&mut self, promise: Promise<()>) {
        self.finished_writes.push(promise);
    }

    /// `Impl::add_write_query` (MessageDb.cpp:1199-1211).
    fn add_write_query(&mut self, query: WriteQuery) {
        self.pending_writes.push(query);
        if self.pending_writes.len() > MAX_PENDING_QUERY_COUNT {
            self.do_flush();
            self.wakeup_at = 0.0;
        } else if self.wakeup_at == 0.0 {
            self.wakeup_at = now_cached() + MAX_PENDING_QUERY_DELAY;
        }
        if self.wakeup_at != 0.0 {
            self.set_timeout_at(self.wakeup_at);
        }
    }

    /// `Impl::add_read_query` (MessageDb.cpp:1212-1214).
    fn add_read_query(&mut self) {
        self.do_flush();
    }

    /// `Impl::do_flush` (MessageDb.cpp:1215-1224): begin transaction, run
    /// the buffered writes, commit, then settle the finished-write promises.
    fn do_flush(&mut self) {
        if self.pending_writes.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_writes);
        self.run_sync(|db| db.begin_write_transaction().ensure());
        for query in pending {
            match query {
                WriteQuery::Add {
                    message_full_id,
                    unique_message_id,
                    sender_dialog_id,
                    random_id,
                    ttl_expires_at,
                    index_mask,
                    search_id,
                    text,
                    notification_id,
                    top_thread_message_id,
                    data,
                    promise,
                } => {
                    self.run_sync(|db| {
                        db.add_message(
                            message_full_id,
                            unique_message_id,
                            sender_dialog_id,
                            random_id,
                            ttl_expires_at,
                            index_mask,
                            search_id,
                            text,
                            notification_id,
                            top_thread_message_id,
                            data,
                        )
                    });
                    self.on_write_result(promise);
                }
                WriteQuery::AddScheduled {
                    message_full_id,
                    data,
                    promise,
                } => {
                    self.run_sync(|db| db.add_scheduled_message(message_full_id, data));
                    self.on_write_result(promise);
                }
                WriteQuery::Delete {
                    message_full_id,
                    promise,
                } => {
                    self.run_sync(|db| db.delete_message(message_full_id));
                    self.on_write_result(promise);
                }
            }
        }
        self.run_sync(|db| db.commit_transaction().ensure());
        for mut promise in std::mem::take(&mut self.finished_writes) {
            promise.set_value(());
        }
        self.cancel_timeout();
    }
}

impl Actor for MessageDbAsyncImpl {
    /// `Impl::start_up` (MessageDb.cpp:1229-1231) — create the
    /// implementation eagerly.
    fn start_up(&mut self) {
        self.run_sync(|_| {});
    }

    /// `Impl::timeout_expired` (MessageDb.cpp:1225-1227).
    fn timeout_expired(&mut self) {
        self.do_flush();
    }
}

actor_messages! {
    pub enum MessageDbAsyncMessage for MessageDbAsyncImpl {
        AddMessage {
            message_full_id: MessageFullId,
            unique_message_id: ServerMessageId,
            sender_dialog_id: DialogId,
            random_id: i64,
            ttl_expires_at: i32,
            index_mask: i32,
            search_id: i64,
            text: String,
            notification_id: NotificationId,
            top_thread_message_id: MessageId,
            data: Vec<u8>,
            promise: Promise<()>,
        } => on_add_message(message_full_id, unique_message_id, sender_dialog_id, random_id, ttl_expires_at, index_mask, search_id, text, notification_id, top_thread_message_id, data, promise),
        AddScheduledMessage {
            message_full_id: MessageFullId,
            data: Vec<u8>,
            promise: Promise<()>,
        } => on_add_scheduled_message(message_full_id, data, promise),
        DeleteMessage {
            message_full_id: MessageFullId,
            promise: Promise<()>,
        } => on_delete_message(message_full_id, promise),
        DeleteAllDialogMessages {
            dialog_id: DialogId,
            from_message_id: MessageId,
            promise: Promise<()>,
        } => on_delete_all_dialog_messages(dialog_id, from_message_id, promise),
        DeleteDialogMessagesBySender {
            dialog_id: DialogId,
            sender_dialog_id: DialogId,
            promise: Promise<()>,
        } => on_delete_dialog_messages_by_sender(dialog_id, sender_dialog_id, promise),
        GetMessage {
            message_full_id: MessageFullId,
            promise: Promise<MessageDbDialogMessage>,
        } => on_get_message(message_full_id, promise),
        GetMessageByUniqueMessageId {
            unique_message_id: ServerMessageId,
            promise: Promise<MessageDbMessage>,
        } => on_get_message_by_unique_message_id(unique_message_id, promise),
        GetMessageByRandomId {
            dialog_id: DialogId,
            random_id: i64,
            promise: Promise<MessageDbDialogMessage>,
        } => on_get_message_by_random_id(dialog_id, random_id, promise),
        GetDialogMessageByDate {
            dialog_id: DialogId,
            first_message_id: MessageId,
            last_message_id: MessageId,
            date: i32,
            promise: Promise<MessageDbDialogMessage>,
        } => on_get_dialog_message_by_date(dialog_id, first_message_id, last_message_id, date, promise),
        GetDialogMessageCalendar {
            query: MessageDbDialogCalendarQuery,
            promise: Promise<MessageDbCalendar>,
        } => on_get_dialog_message_calendar(query, promise),
        GetDialogSparseMessagePositions {
            query: MessageDbGetDialogSparseMessagePositionsQuery,
            promise: Promise<MessageDbMessagePositions>,
        } => on_get_dialog_sparse_message_positions(query, promise),
        GetMessages {
            query: MessageDbMessagesQuery,
            promise: Promise<Vec<MessageDbDialogMessage>>,
        } => on_get_messages(query, promise),
        GetScheduledMessages {
            dialog_id: DialogId,
            limit: i32,
            promise: Promise<Vec<MessageDbDialogMessage>>,
        } => on_get_scheduled_messages(dialog_id, limit, promise),
        GetMessagesFromNotificationId {
            dialog_id: DialogId,
            from_notification_id: NotificationId,
            limit: i32,
            promise: Promise<Vec<MessageDbDialogMessage>>,
        } => on_get_messages_from_notification_id(dialog_id, from_notification_id, limit, promise),
        GetCalls {
            query: MessageDbCallsQuery,
            promise: Promise<MessageDbCallsResult>,
        } => on_get_calls(query, promise),
        GetMessagesFts {
            query: MessageDbFtsQuery,
            promise: Promise<MessageDbFtsResult>,
        } => on_get_messages_fts(query, promise),
        GetExpiringMessages {
            expires_till: i32,
            limit: i32,
            promise: Promise<Vec<MessageDbMessage>>,
        } => on_get_expiring_messages(expires_till, limit, promise),
        Close {
            promise: Promise<()>,
        } => on_close(promise),
    }
}

impl MessageDbAsyncImpl {
    /// `Impl::add_message` (MessageDb.cpp:1075-1087).
    #[allow(clippy::too_many_arguments)]
    fn on_add_message(
        &mut self,
        message_full_id: MessageFullId,
        unique_message_id: ServerMessageId,
        sender_dialog_id: DialogId,
        random_id: i64,
        ttl_expires_at: i32,
        index_mask: i32,
        search_id: i64,
        text: String,
        notification_id: NotificationId,
        top_thread_message_id: MessageId,
        data: Vec<u8>,
        promise: Promise<()>,
    ) {
        self.add_write_query(WriteQuery::Add {
            message_full_id,
            unique_message_id,
            sender_dialog_id,
            random_id,
            ttl_expires_at,
            index_mask,
            search_id,
            text,
            notification_id,
            top_thread_message_id,
            data,
            promise,
        });
    }

    /// `Impl::add_scheduled_message` (MessageDb.cpp:1088-1093).
    fn on_add_scheduled_message(
        &mut self,
        message_full_id: MessageFullId,
        data: Vec<u8>,
        promise: Promise<()>,
    ) {
        self.add_write_query(WriteQuery::AddScheduled {
            message_full_id,
            data,
            promise,
        });
    }

    /// `Impl::delete_message` (MessageDb.cpp:1095-1100).
    fn on_delete_message(&mut self, message_full_id: MessageFullId, promise: Promise<()>) {
        self.add_write_query(WriteQuery::Delete {
            message_full_id,
            promise,
        });
    }

    /// `Impl::delete_all_dialog_messages` (MessageDb.cpp:1107-1111) — an
    /// immediate (read-path) query.
    fn on_delete_all_dialog_messages(
        &mut self,
        dialog_id: DialogId,
        from_message_id: MessageId,
        mut promise: Promise<()>,
    ) {
        self.add_read_query();
        self.run_sync(|db| db.delete_all_dialog_messages(dialog_id, from_message_id));
        promise.set_value(());
    }

    /// `Impl::delete_dialog_messages_by_sender` (MessageDb.cpp:1113-1117).
    fn on_delete_dialog_messages_by_sender(
        &mut self,
        dialog_id: DialogId,
        sender_dialog_id: DialogId,
        mut promise: Promise<()>,
    ) {
        self.add_read_query();
        self.run_sync(|db| db.delete_dialog_messages_by_sender(dialog_id, sender_dialog_id));
        promise.set_value(());
    }

    /// `Impl::get_message` (MessageDb.cpp:1119-1122).
    fn on_get_message(
        &mut self,
        message_full_id: MessageFullId,
        mut promise: Promise<MessageDbDialogMessage>,
    ) {
        self.add_read_query();
        let result = self.run_sync(|db| db.get_message(message_full_id));
        promise.set_result(result);
    }

    /// `Impl::get_message_by_unique_message_id` (MessageDb.cpp:1123-1126).
    fn on_get_message_by_unique_message_id(
        &mut self,
        unique_message_id: ServerMessageId,
        mut promise: Promise<MessageDbMessage>,
    ) {
        self.add_read_query();
        let result = self.run_sync(|db| db.get_message_by_unique_message_id(unique_message_id));
        promise.set_result(result);
    }

    /// `Impl::get_message_by_random_id` (MessageDb.cpp:1127-1130).
    fn on_get_message_by_random_id(
        &mut self,
        dialog_id: DialogId,
        random_id: i64,
        mut promise: Promise<MessageDbDialogMessage>,
    ) {
        self.add_read_query();
        let result = self.run_sync(|db| db.get_message_by_random_id(dialog_id, random_id));
        promise.set_result(result);
    }

    /// `Impl::get_dialog_message_by_date` (MessageDb.cpp:1131-1135).
    fn on_get_dialog_message_by_date(
        &mut self,
        dialog_id: DialogId,
        first_message_id: MessageId,
        last_message_id: MessageId,
        date: i32,
        mut promise: Promise<MessageDbDialogMessage>,
    ) {
        self.add_read_query();
        let result = self.run_sync(|db| {
            db.get_dialog_message_by_date(dialog_id, first_message_id, last_message_id, date)
        });
        promise.set_result(result);
    }

    /// `Impl::get_dialog_message_calendar` (MessageDb.cpp:1137-1140).
    fn on_get_dialog_message_calendar(
        &mut self,
        query: MessageDbDialogCalendarQuery,
        mut promise: Promise<MessageDbCalendar>,
    ) {
        self.add_read_query();
        let calendar = self.run_sync(|db| db.get_dialog_message_calendar(query));
        promise.set_value(calendar);
    }

    /// `Impl::get_dialog_sparse_message_positions`
    /// (MessageDb.cpp:1142-1146).
    fn on_get_dialog_sparse_message_positions(
        &mut self,
        query: MessageDbGetDialogSparseMessagePositionsQuery,
        mut promise: Promise<MessageDbMessagePositions>,
    ) {
        self.add_read_query();
        let result = self.run_sync(|db| db.get_dialog_sparse_message_positions(query));
        promise.set_result(result);
    }

    /// `Impl::get_messages` (MessageDb.cpp:1148-1151).
    fn on_get_messages(
        &mut self,
        query: MessageDbMessagesQuery,
        mut promise: Promise<Vec<MessageDbDialogMessage>>,
    ) {
        self.add_read_query();
        let messages = self.run_sync(|db| db.get_messages(query));
        promise.set_value(messages);
    }

    /// `Impl::get_scheduled_messages` (MessageDb.cpp:1152-1155).
    fn on_get_scheduled_messages(
        &mut self,
        dialog_id: DialogId,
        limit: i32,
        mut promise: Promise<Vec<MessageDbDialogMessage>>,
    ) {
        self.add_read_query();
        let messages = self.run_sync(|db| db.get_scheduled_messages(dialog_id, limit));
        promise.set_value(messages);
    }

    /// `Impl::get_messages_from_notification_id`
    /// (MessageDb.cpp:1156-1160).
    fn on_get_messages_from_notification_id(
        &mut self,
        dialog_id: DialogId,
        from_notification_id: NotificationId,
        limit: i32,
        mut promise: Promise<Vec<MessageDbDialogMessage>>,
    ) {
        self.add_read_query();
        let messages = self.run_sync(|db| {
            db.get_messages_from_notification_id(dialog_id, from_notification_id, limit)
        });
        promise.set_value(messages);
    }

    /// `Impl::get_calls` (MessageDb.cpp:1161-1164).
    fn on_get_calls(
        &mut self,
        query: MessageDbCallsQuery,
        mut promise: Promise<MessageDbCallsResult>,
    ) {
        self.add_read_query();
        let calls = self.run_sync(|db| db.get_calls(query));
        promise.set_value(calls);
    }

    /// `Impl::get_messages_fts` (MessageDb.cpp:1165-1168).
    fn on_get_messages_fts(
        &mut self,
        query: MessageDbFtsQuery,
        mut promise: Promise<MessageDbFtsResult>,
    ) {
        self.add_read_query();
        let fts = self.run_sync(|db| db.get_messages_fts(query));
        promise.set_value(fts);
    }

    /// `Impl::get_expiring_messages` (MessageDb.cpp:1169-1172).
    fn on_get_expiring_messages(
        &mut self,
        expires_till: i32,
        limit: i32,
        mut promise: Promise<Vec<MessageDbMessage>>,
    ) {
        self.add_read_query();
        let messages = self.run_sync(|db| db.get_expiring_messages(expires_till, limit));
        promise.set_value(messages);
    }

    /// `Impl::close` (MessageDb.cpp:1174-1180).
    fn on_close(&mut self, mut promise: Promise<()>) {
        self.do_flush();
        self.sync_db_safe = None;
        promise.set_value(());
        self.stop();
    }

    /// `Impl::force_flush` (MessageDb.cpp:1182-1185).
    fn on_force_flush(&mut self) {
        self.do_flush();
        tracing::info!("MessageDb flushed");
    }
}

impl MessageDbAsyncInterface for MessageDbAsync {
    fn add_message(
        &self,
        message_full_id: MessageFullId,
        unique_message_id: ServerMessageId,
        sender_dialog_id: DialogId,
        random_id: i64,
        ttl_expires_at: i32,
        index_mask: i32,
        search_id: i64,
        text: String,
        notification_id: NotificationId,
        top_thread_message_id: MessageId,
        data: Vec<u8>,
        promise: Promise<()>,
    ) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::AddMessage {
                message_full_id,
                unique_message_id,
                sender_dialog_id,
                random_id,
                ttl_expires_at,
                index_mask,
                search_id,
                text,
                notification_id,
                top_thread_message_id,
                data,
                promise,
            },
        );
    }

    fn add_scheduled_message(
        &self,
        message_full_id: MessageFullId,
        data: Vec<u8>,
        promise: Promise<()>,
    ) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::AddScheduledMessage {
                message_full_id,
                data,
                promise,
            },
        );
    }

    fn delete_message(&self, message_full_id: MessageFullId, promise: Promise<()>) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::DeleteMessage {
                message_full_id,
                promise,
            },
        );
    }

    fn delete_all_dialog_messages(
        &self,
        dialog_id: DialogId,
        from_message_id: MessageId,
        promise: Promise<()>,
    ) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::DeleteAllDialogMessages {
                dialog_id,
                from_message_id,
                promise,
            },
        );
    }

    fn delete_dialog_messages_by_sender(
        &self,
        dialog_id: DialogId,
        sender_dialog_id: DialogId,
        promise: Promise<()>,
    ) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::DeleteDialogMessagesBySender {
                dialog_id,
                sender_dialog_id,
                promise,
            },
        );
    }

    fn get_message(
        &self,
        message_full_id: MessageFullId,
        promise: Promise<MessageDbDialogMessage>,
    ) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::GetMessage {
                message_full_id,
                promise,
            },
        );
    }

    fn get_message_by_unique_message_id(
        &self,
        unique_message_id: ServerMessageId,
        promise: Promise<MessageDbMessage>,
    ) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::GetMessageByUniqueMessageId {
                unique_message_id,
                promise,
            },
        );
    }

    fn get_message_by_random_id(
        &self,
        dialog_id: DialogId,
        random_id: i64,
        promise: Promise<MessageDbDialogMessage>,
    ) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::GetMessageByRandomId {
                dialog_id,
                random_id,
                promise,
            },
        );
    }

    fn get_dialog_message_by_date(
        &self,
        dialog_id: DialogId,
        first_message_id: MessageId,
        last_message_id: MessageId,
        date: i32,
        promise: Promise<MessageDbDialogMessage>,
    ) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::GetDialogMessageByDate {
                dialog_id,
                first_message_id,
                last_message_id,
                date,
                promise,
            },
        );
    }

    fn get_dialog_message_calendar(
        &self,
        query: MessageDbDialogCalendarQuery,
        promise: Promise<MessageDbCalendar>,
    ) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::GetDialogMessageCalendar { query, promise },
        );
    }

    fn get_dialog_sparse_message_positions(
        &self,
        query: MessageDbGetDialogSparseMessagePositionsQuery,
        promise: Promise<MessageDbMessagePositions>,
    ) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::GetDialogSparseMessagePositions { query, promise },
        );
    }

    fn get_messages(
        &self,
        query: MessageDbMessagesQuery,
        promise: Promise<Vec<MessageDbDialogMessage>>,
    ) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::GetMessages { query, promise },
        );
    }

    fn get_scheduled_messages(
        &self,
        dialog_id: DialogId,
        limit: i32,
        promise: Promise<Vec<MessageDbDialogMessage>>,
    ) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::GetScheduledMessages {
                dialog_id,
                limit,
                promise,
            },
        );
    }

    fn get_messages_from_notification_id(
        &self,
        dialog_id: DialogId,
        from_notification_id: NotificationId,
        limit: i32,
        promise: Promise<Vec<MessageDbDialogMessage>>,
    ) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::GetMessagesFromNotificationId {
                dialog_id,
                from_notification_id,
                limit,
                promise,
            },
        );
    }

    fn get_calls(&self, query: MessageDbCallsQuery, promise: Promise<MessageDbCallsResult>) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::GetCalls { query, promise },
        );
    }

    fn get_messages_fts(&self, query: MessageDbFtsQuery, promise: Promise<MessageDbFtsResult>) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::GetMessagesFts { query, promise },
        );
    }

    fn get_expiring_messages(
        &self,
        expires_till: i32,
        limit: i32,
        promise: Promise<Vec<MessageDbMessage>>,
    ) {
        send_closure_later(
            self.impl_.get(),
            MessageDbAsyncMessage::GetExpiringMessages {
                expires_till,
                limit,
                promise,
            },
        );
    }

    fn close(&self, promise: Promise<()>) {
        send_closure_later(self.impl_.get(), MessageDbAsyncMessage::Close { promise });
    }

    fn force_flush(&self) {
        // the fieldless `force_flush` has no `actor_messages!` variant (the
        // macro requires at least one field); the raw-thunk send is the
        // same building block the tdactor tests use
        tdactor::send_thunk_later(
            tdactor::ActorRef::from(self.impl_.get()),
            tdactor::event::ActorThunk::from_boxed(Box::new(move |actor| {
                // SAFETY: the `ActorId<MessageDbAsyncImpl>` marker names the
                // actor's real type (the C++ `static_cast` in send_closure)
                let actor = unsafe { &mut *(actor as *mut dyn Actor as *mut MessageDbAsyncImpl) };
                actor.on_force_flush();
            })),
        );
    }
}

/// `create_message_db_async` (MessageDb.cpp:1236-1239).
pub fn create_message_db_async(
    sync_db: Arc<MessageDbSyncSafe>,
    scheduler_id: i32,
) -> MessageDbAsync {
    MessageDbAsync::new(sync_db, scheduler_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialog_id::ScheduledServerMessageId;
    use tddb::db_key::DbKey;
    use tddb::sqlite_db::destroy;
    use tddb::sqlite_statement::Datatype;
    use tddb::td_db::init_message_db;

    fn open_db(name: &str) -> (String, SqliteDb) {
        let path = std::env::temp_dir()
            .join(name)
            .to_string_lossy()
            .into_owned();
        let _ = destroy(&path);
        let db = SqliteDb::open_with_key(&path, true, &DbKey::empty(), None).unwrap();
        let mut version = 0;
        init_message_db(&db, &mut version).ensure();
        (path, db)
    }

    /// M.6.3: add → the raw row carries the byte-identical blob and every
    /// index column; the nullable columns are NULL-ed exactly as in C++.
    #[test]
    fn add_message_round_trip_preserves_blob_and_columns() {
        let (_path, db) = open_db("m6_msgdb_add_full");
        let mut db_impl = MessageDbImpl::new(db).unwrap();

        let dialog_id = DialogId::new(-42);
        let message_id = MessageId::new(1 << 20);
        let data = vec![1u8, 2, 3, 4, 5];
        db_impl.add_message(
            MessageFullId::new(dialog_id, message_id),
            ServerMessageId::new(7),
            DialogId::new(777000),
            12345,
            999,
            (1 << 0) | (1 << 2),
            42,
            "hello".to_string(),
            NotificationId::new(5),
            MessageId::new(2 << 20),
            data.clone(),
        );

        let mut stmt = db_impl
            .db
            .get_statement("SELECT * FROM messages WHERE dialog_id = ?1")
            .unwrap();
        stmt.bind_int64(1, dialog_id.get()).ensure();
        stmt.step().ensure();
        assert!(stmt.has_row());
        assert_eq!(stmt.view_int64(0), -42);
        assert_eq!(stmt.view_int64(1), 1 << 20);
        assert_eq!(stmt.view_int32(2), 7);
        assert_eq!(stmt.view_int64(3), 777000);
        assert_eq!(stmt.view_int64(4), 12345);
        assert_eq!(stmt.view_blob(5), &data[..]);
        assert_eq!(stmt.view_int32(6), 999);
        assert_eq!(stmt.view_int32(7), (1 << 0) | (1 << 2));
        assert_eq!(stmt.view_int64(8), 42);
        // the C++ text enrichment: " \a<dialog_id>" then " \a\a<i>" per set bit
        assert_eq!(stmt.view_string(9), b"hello \x07-42 \x07\x070 \x07\x072");
        assert_eq!(stmt.view_int32(10), 5);
        assert_eq!(stmt.view_int64(11), 2 << 20);
        stmt.step().ensure();
        assert!(!stmt.has_row());
        stmt.reset();

        // all-nullable variant: every optional column binds NULL
        db_impl.add_message(
            MessageFullId::new(dialog_id, MessageId::new(3 << 20)),
            ServerMessageId::new(0),
            DialogId::new(0),
            0,
            0,
            0,
            0,
            String::new(),
            NotificationId::new(0),
            MessageId::new(0),
            b"tiny".to_vec(),
        );
        let mut stmt = db_impl
            .db
            .get_statement("SELECT * FROM messages WHERE message_id = ?2")
            .unwrap();
        stmt.bind_int64(2, 3 << 20).ensure();
        stmt.step().ensure();
        assert!(stmt.has_row());
        for col in [2, 3, 4, 6, 7, 8, 9, 10, 11] {
            assert_eq!(
                stmt.view_datatype(col),
                Datatype::Null,
                "column {col} should be NULL"
            );
        }
        assert_eq!(stmt.view_datatype(5), Datatype::Blob);
        stmt.step().ensure();
        assert!(!stmt.has_row());
        stmt.reset();
    }

    /// M.6.4: the three delete statements + the FTS trigger consistency —
    /// deleting a searched message removes its `messages_fts` row.
    #[test]
    fn delete_paths_and_fts_trigger_consistency() {
        let (_path, db) = open_db("m6_msgdb_delete");
        let mut db_impl = MessageDbImpl::new(db).unwrap();

        let dialog_id = DialogId::new(-42);
        let add = |db_impl: &mut MessageDbImpl, message_id: i64, sender: i64, search_id: i64| {
            db_impl.add_message(
                MessageFullId::new(dialog_id, MessageId::new(message_id)),
                ServerMessageId::new(0),
                DialogId::new(sender),
                0,
                0,
                0,
                search_id,
                if search_id != 0 {
                    "searchable unicorn text".to_string()
                } else {
                    String::new()
                },
                NotificationId::new(0),
                MessageId::new(0),
                vec![b'x'],
            );
        };

        let count = |db_impl: &MessageDbImpl, sql: &str, bind: i64| -> i64 {
            let mut stmt = db_impl.db.get_statement(sql).unwrap();
            stmt.bind_int64(1, bind).ensure();
            stmt.step().ensure();
            assert!(stmt.has_row());
            let value = stmt.view_int64(0);
            stmt.reset();
            value
        };

        add(&mut db_impl, 1 << 20, 777000, 91);
        add(&mut db_impl, 2 << 20, 42, 0);
        add(&mut db_impl, 3 << 20, 777000, 0);

        // ordinary delete
        db_impl.delete_message(MessageFullId::new(dialog_id, MessageId::new(2 << 20)));
        assert_eq!(
            count(
                &db_impl,
                "SELECT count(*) FROM messages WHERE dialog_id = ?1",
                -42
            ),
            2
        );

        // the FTS insert trigger fired for search_id 91...
        let mut match_stmt = db_impl
            .db
            .get_statement("SELECT count(*) FROM messages_fts WHERE messages_fts MATCH ?1")
            .unwrap();
        match_stmt.bind_string(1, "unicorn").ensure();
        match_stmt.step().ensure();
        assert_eq!(match_stmt.view_int64(0), 1);
        match_stmt.reset();

        // ...and the delete trigger removes the FTS row
        db_impl.delete_message(MessageFullId::new(dialog_id, MessageId::new(1 << 20)));
        match_stmt.bind_string(1, "unicorn").ensure();
        match_stmt.step().ensure();
        assert_eq!(match_stmt.view_int64(0), 0);
        match_stmt.reset();

        let mut integrity = db_impl.db.get_statement("PRAGMA integrity_check").unwrap();
        integrity.step().ensure();
        assert_eq!(integrity.view_string(0), b"ok");
        integrity.reset();

        // delete_all_dialog_messages: `message_id <= ?2` only
        add(&mut db_impl, 4 << 20, 1, 0);
        add(&mut db_impl, 5 << 20, 1, 0);
        db_impl.delete_all_dialog_messages(dialog_id, MessageId::new(4 << 20));
        assert_eq!(
            count(
                &db_impl,
                "SELECT count(*) FROM messages WHERE dialog_id = ?1",
                -42
            ),
            1
        );
        db_impl.delete_all_dialog_messages(dialog_id, MessageId::new(6 << 20));
        assert_eq!(
            count(
                &db_impl,
                "SELECT count(*) FROM messages WHERE dialog_id = ?1",
                -42
            ),
            0
        );

        // delete_dialog_messages_by_sender (the sender_user_id column)
        add(&mut db_impl, 7 << 20, 777000, 0);
        add(&mut db_impl, 8 << 20, 42, 0);
        db_impl.delete_dialog_messages_by_sender(dialog_id, DialogId::new(777000));
        let mut remaining = db_impl
            .db
            .get_statement("SELECT sender_user_id FROM messages WHERE dialog_id = ?1")
            .unwrap();
        remaining.bind_int64(1, -42).ensure();
        remaining.step().ensure();
        assert!(remaining.has_row());
        assert_eq!(remaining.view_int64(0), 42);
        remaining.step().ensure();
        assert!(!remaining.has_row());
        remaining.reset();

        // scheduled deletes: the server variant goes through
        // `server_message_id`, the local one through `message_id`
        let send_date = (1 << 30) + 1000;
        let server_scheduled =
            MessageId::from_scheduled(ScheduledServerMessageId::new(5), send_date, false);
        assert!(server_scheduled.is_scheduled_server());
        db_impl.add_scheduled_message(
            MessageFullId::new(dialog_id, server_scheduled),
            b"s".to_vec(),
        );
        let local_scheduled = MessageId::new(4 | 2); // SCHEDULED_MASK | TYPE_LOCAL
        assert!(local_scheduled.is_valid_scheduled() && !local_scheduled.is_scheduled_server());
        db_impl.add_scheduled_message(
            MessageFullId::new(dialog_id, local_scheduled),
            b"l".to_vec(),
        );
        assert_eq!(
            count(
                &db_impl,
                "SELECT count(*) FROM scheduled_messages WHERE dialog_id = ?1",
                -42
            ),
            2
        );
        db_impl.delete_message(MessageFullId::new(dialog_id, server_scheduled));
        assert_eq!(
            count(
                &db_impl,
                "SELECT count(*) FROM scheduled_messages WHERE dialog_id = ?1",
                -42
            ),
            1
        );
        db_impl.delete_message(MessageFullId::new(dialog_id, local_scheduled));
        assert_eq!(
            count(
                &db_impl,
                "SELECT count(*) FROM scheduled_messages WHERE dialog_id = ?1",
                -42
            ),
            0
        );
    }

    /// M.6.5: the blob header parser — flag chain, sender presence, and the
    /// `from_data` embedded-id switch.
    #[test]
    fn get_message_info_parses_the_cpp_blob_header() {
        // plain header: no flags chain, no sender
        let mut rest = Vec::new();
        store_parse::store_i64(&mut rest, 1 << 20); // the embedded message id
        store_parse::store_i32(&mut rest, 12345); // date
        let blob = store_message_blob(0, &rest);

        let (message_id, date) = MessageDbImpl::get_message_info(MessageId::new(55), &blob, false);
        assert_eq!(message_id.get(), 55);
        assert_eq!(date, 12345);
        let (message_id, date) = MessageDbImpl::get_message_info(MessageId::new(55), &blob, true);
        assert_eq!(message_id.get(), 1 << 20);
        assert_eq!(date, 12345);

        // full chain: flags(1<<29) → flags2(1<<29) → flags3(1<<29) → flags4,
        // then has_sender(1<<10)
        let mut rest = Vec::new();
        store_parse::store_i32(&mut rest, 1 << 29); // flags2
        store_parse::store_i32(&mut rest, 1 << 29); // flags3
        store_parse::store_i32(&mut rest, 7); // flags4
        store_parse::store_i64(&mut rest, (3 << 20) | 2); // local message id
        store_parse::store_i64(&mut rest, 777); // the sender (64-bit version)
        store_parse::store_i32(&mut rest, 88888); // date
        let blob = store_message_blob((1 << 29) | (1 << 10), &rest);
        let (message_id, date) = MessageDbImpl::get_message_info(MessageId::new(56), &blob, true);
        assert_eq!(message_id.get(), (3 << 20) | 2);
        assert_eq!(date, 88888);

        // the chain stops at the first missing 1<<29 bit: one extra i32 is
        // the flags2 payload, then the header continues
        let mut rest = Vec::new();
        store_parse::store_i32(&mut rest, 3); // flags2 payload
        store_parse::store_i64(&mut rest, 9 << 20);
        store_parse::store_i32(&mut rest, 4242);
        let blob = store_message_blob(1 << 29, &rest);
        let (message_id, date) = MessageDbImpl::get_message_info(MessageId::new(57), &blob, true);
        assert_eq!(message_id.get(), 9 << 20);
        assert_eq!(date, 4242);

        // a truncated blob sees zeros, like the unchecked C++ parse
        let blob = store_message_blob(0, &[1, 2, 3]);
        let (message_id, date) = MessageDbImpl::get_message_info(MessageId::new(58), &blob, true);
        assert_eq!(message_id.get(), 0);
        assert_eq!(date, 0);
    }

    /// a blob header carrying only the message id and the date (flags = 0)
    fn dated_blob(message_id: i64, date: i32) -> Vec<u8> {
        let mut rest = Vec::new();
        store_parse::store_i64(&mut rest, message_id);
        store_parse::store_i32(&mut rest, date);
        store_message_blob(0, &rest)
    }

    /// M.6.7: the two-sided get_messages offset semantics, including the
    /// `MessageId::max()` guard and the per-filter index path.
    #[test]
    fn get_messages_offset_semantics_match_the_cpp_merge() {
        let (_path, db) = open_db("m6_msgdb_get_messages");
        let mut db_impl = MessageDbImpl::new(db).unwrap();

        let dialog_id = DialogId::new(-42);
        for id in 1..=10 {
            db_impl.add_message(
                MessageFullId::new(dialog_id, MessageId::new(id << 20)),
                ServerMessageId::new(0),
                DialogId::new(0),
                0,
                0,
                0,
                0,
                String::new(),
                NotificationId::new(0),
                MessageId::new(0),
                vec![id as u8],
            );
        }

        let ids = |messages: &[MessageDbDialogMessage]| -> Vec<i64> {
            messages.iter().map(|m| m.message_id.get()).collect()
        };

        // offset 0: plain descending page
        let query = MessageDbMessagesQuery {
            dialog_id,
            from_message_id: MessageId::new(8 << 20),
            offset: 0,
            limit: 3,
            ..MessageDbMessagesQuery::default()
        };
        assert_eq!(
            ids(&db_impl.get_messages(query)),
            vec![7 << 20, 6 << 20, 5 << 20]
        );

        // negative offset: `-offset` ascending rows first (right ++ left),
        // then `limit + offset` descending rows — the C++ merge order
        let query = MessageDbMessagesQuery {
            dialog_id,
            from_message_id: MessageId::new(6 << 20),
            offset: -2,
            limit: 4,
            ..MessageDbMessagesQuery::default()
        };
        assert_eq!(
            ids(&db_impl.get_messages(query)),
            vec![7 << 20, 6 << 20, 5 << 20, 4 << 20]
        );

        // from_message_id == max(): the C++ decrements first, so the
        // descending side reads everything below max — here the newest
        // messages
        let query = MessageDbMessagesQuery {
            dialog_id,
            from_message_id: MessageId::MAX,
            offset: 0,
            limit: 3,
            ..MessageDbMessagesQuery::default()
        };
        assert_eq!(
            ids(&db_impl.get_messages(query)),
            vec![10 << 20, 9 << 20, 8 << 20]
        );

        // ascending reach beyond the newest message: [10, 9] reversed-asc
        // first, then the descending page from 8
        let query = MessageDbMessagesQuery {
            dialog_id,
            from_message_id: MessageId::new(9 << 20),
            offset: -3,
            limit: 10,
            ..MessageDbMessagesQuery::default()
        };
        assert_eq!(
            ids(&db_impl.get_messages(query)),
            vec![
                10 << 20,
                9 << 20,
                8 << 20,
                7 << 20,
                6 << 20,
                5 << 20,
                4 << 20,
                3 << 20,
                2 << 20
            ]
        );
    }

    /// M.6.7: the per-filter (index_mask) statement path.
    #[test]
    fn get_messages_by_filter_uses_the_index_mask() {
        let (_path, db) = open_db("m6_msgdb_filter");
        let mut db_impl = MessageDbImpl::new(db).unwrap();

        let dialog_id = DialogId::new(-7);
        // Photo is the 5th filter: ordinal 4, statement slot 3, mask bit
        // 1 << 3
        let photo_mask = 1 << message_search_filter_index(MessageSearchFilter::Photo);
        for id in [1, 3, 5, 7] {
            db_impl.add_message(
                MessageFullId::new(dialog_id, MessageId::new(id << 20)),
                ServerMessageId::new(0),
                DialogId::new(0),
                0,
                0,
                photo_mask,
                0,
                String::new(),
                NotificationId::new(0),
                MessageId::new(0),
                vec![id as u8],
            );
        }
        // a message without the bit is invisible to the filtered query
        db_impl.add_message(
            MessageFullId::new(dialog_id, MessageId::new(2 << 20)),
            ServerMessageId::new(0),
            DialogId::new(0),
            0,
            0,
            photo_mask << 1,
            0,
            String::new(),
            NotificationId::new(0),
            MessageId::new(0),
            vec![2],
        );

        let query = MessageDbMessagesQuery {
            dialog_id,
            filter: MessageSearchFilter::Photo,
            from_message_id: MessageId::new(10 << 20),
            offset: 0,
            limit: 3,
        };
        let ids: Vec<i64> = db_impl
            .get_messages(query)
            .iter()
            .map(|m| m.message_id.get())
            .collect();
        assert_eq!(ids, vec![7 << 20, 5 << 20, 3 << 20]);
    }

    /// M.6.6: scheduled reads, by-unique, by-random and expiring reads.
    #[test]
    fn single_message_reads_cover_all_paths() {
        let (_path, db) = open_db("m6_msgdb_single_reads");
        let mut db_impl = MessageDbImpl::new(db).unwrap();

        let dialog_id = DialogId::new(-9);
        let blob = vec![9u8, 8, 7];

        // ordinary get_message
        db_impl.add_message(
            MessageFullId::new(dialog_id, MessageId::new(1 << 20)),
            ServerMessageId::new(500),
            DialogId::new(0),
            4242,
            0,
            0,
            0,
            String::new(),
            NotificationId::new(0),
            MessageId::new(0),
            blob.clone(),
        );
        let message = db_impl
            .get_message(MessageFullId::new(dialog_id, MessageId::new(1 << 20)))
            .unwrap();
        assert_eq!(message.data, blob);
        assert!(
            db_impl
                .get_message(MessageFullId::new(dialog_id, MessageId::new(2 << 20)))
                .is_error()
        );

        // by unique message id
        let message = db_impl
            .get_message_by_unique_message_id(ServerMessageId::new(500))
            .unwrap();
        assert_eq!(message.dialog_id, dialog_id);
        assert_eq!(message.message_id.get(), 1 << 20);
        assert!(
            db_impl
                .get_message_by_unique_message_id(ServerMessageId::new(0))
                .is_error()
        ); // invalid
        assert!(
            db_impl
                .get_message_by_unique_message_id(ServerMessageId::new(501))
                .is_error()
        ); // not found

        // by random id
        let message = db_impl.get_message_by_random_id(dialog_id, 4242).unwrap();
        assert_eq!(message.message_id.get(), 1 << 20);
        assert!(db_impl.get_message_by_random_id(dialog_id, 1).is_error());

        // scheduled: server + local variants
        let server_scheduled =
            MessageId::from_scheduled(ScheduledServerMessageId::new(6), (1 << 30) + 5000, false);
        db_impl.add_scheduled_message(
            MessageFullId::new(dialog_id, server_scheduled),
            b"s".to_vec(),
        );
        let got = db_impl
            .get_message(MessageFullId::new(dialog_id, server_scheduled))
            .unwrap();
        assert_eq!(got.data, b"s".to_vec());
        let local_scheduled = MessageId::new(4 | 2);
        db_impl.add_scheduled_message(
            MessageFullId::new(dialog_id, local_scheduled),
            b"l".to_vec(),
        );
        let got = db_impl
            .get_message(MessageFullId::new(dialog_id, local_scheduled))
            .unwrap();
        assert_eq!(got.data, b"l".to_vec());

        // scheduled listing: newest first, everything (limit = MAX)
        let scheduled = db_impl.get_scheduled_messages(dialog_id, i32::MAX);
        let ids: Vec<i64> = scheduled.iter().map(|m| m.message_id.get()).collect();
        assert_eq!(ids.len(), 2);

        // expiring
        db_impl.add_message(
            MessageFullId::new(dialog_id, MessageId::new(5 << 20)),
            ServerMessageId::new(0),
            DialogId::new(0),
            0,
            7777,
            0,
            0,
            String::new(),
            NotificationId::new(0),
            MessageId::new(0),
            b"e".to_vec(),
        );
        db_impl.add_message(
            MessageFullId::new(dialog_id, MessageId::new(6 << 20)),
            ServerMessageId::new(0),
            DialogId::new(0),
            0,
            9999,
            0,
            0,
            String::new(),
            NotificationId::new(0),
            MessageId::new(0),
            b"f".to_vec(),
        );
        let expiring = db_impl.get_expiring_messages(7777, 10);
        assert_eq!(expiring.len(), 1);
        assert_eq!(expiring[0].message_id.get(), 5 << 20);
    }

    /// M.6.6: the by-date binary search over blob-header dates.
    #[test]
    fn get_dialog_message_by_date_finds_the_nearest_older_message() {
        let (_path, db) = open_db("m6_msgdb_by_date");
        let mut db_impl = MessageDbImpl::new(db).unwrap();

        let dialog_id = DialogId::new(-11);
        for id in 1..=10 {
            db_impl.add_message(
                MessageFullId::new(dialog_id, MessageId::new(id << 20)),
                ServerMessageId::new(0),
                DialogId::new(0),
                0,
                0,
                0,
                0,
                String::new(),
                NotificationId::new(0),
                MessageId::new(0),
                dated_blob(id << 20, 100 * id as i32),
            );
        }

        // the newest message with date <= 550 is message 5 (date 500)
        let found = db_impl
            .get_dialog_message_by_date(
                dialog_id,
                MessageId::new(1 << 20),
                MessageId::new(10 << 20),
                550,
            )
            .unwrap();
        assert_eq!(found.message_id.get(), 5 << 20);

        // exactly on a date boundary
        let found = db_impl
            .get_dialog_message_by_date(
                dialog_id,
                MessageId::new(1 << 20),
                MessageId::new(10 << 20),
                700,
            )
            .unwrap();
        assert_eq!(found.message_id.get(), 7 << 20);

        // a date before every message
        assert!(
            db_impl
                .get_dialog_message_by_date(
                    dialog_id,
                    MessageId::new(1 << 20),
                    MessageId::new(10 << 20),
                    50,
                )
                .is_error()
        );
    }

    /// M.6.8: the calendar day grouping and the sparse position sampling.
    #[test]
    fn calendar_and_sparse_positions_match_the_cpp_arithmetic() {
        // Photo: the 5th filter → statement slot 3, mask bit 1 << 3
        let photo_mask = 1 << message_search_filter_index(MessageSearchFilter::Photo);
        let (_path, db) = open_db("m6_msgdb_calendar");
        let mut db_impl = MessageDbImpl::new(db).unwrap();

        let dialog_id = DialogId::new(-13);
        // three messages on day D = date 86400 (day 1), tz 0
        for date in [86_400, 86_401, 86_799] {
            let id = (date as i64) << 20;
            db_impl.add_message(
                MessageFullId::new(dialog_id, MessageId::new(id)),
                ServerMessageId::new(0),
                DialogId::new(0),
                0,
                0,
                photo_mask, // Photo
                0,
                String::new(),
                NotificationId::new(0),
                MessageId::new(0),
                dated_blob(id, date),
            );
        }
        // one message on the previous day and one further back
        for date in [300, 86_399] {
            let id = (date as i64) << 20;
            db_impl.add_message(
                MessageFullId::new(dialog_id, MessageId::new(id)),
                ServerMessageId::new(0),
                DialogId::new(0),
                0,
                0,
                photo_mask,
                0,
                String::new(),
                NotificationId::new(0),
                MessageId::new(0),
                dated_blob(id, date),
            );
        }

        let calendar = db_impl.get_dialog_message_calendar(MessageDbDialogCalendarQuery {
            dialog_id,
            filter: MessageSearchFilter::Photo,
            from_message_id: MessageId::MAX,
            tz_offset: 0,
        });
        // descending: day groups — {86799, 86401, 86400} = day 1, then
        // {86399, 300} = day 0
        assert_eq!(calendar.total_counts, vec![3, 2]);
        assert_eq!(calendar.messages.len(), 2);
        assert_eq!(calendar.messages[0].message_id.get(), 86_799 << 20);
        assert_eq!(calendar.messages[1].message_id.get(), 86_399 << 20);

        // tz_offset shifts the day boundaries: with tz +3600 the 86_399
        // message joins day 1 (89999/86400 == 1)
        let calendar = db_impl.get_dialog_message_calendar(MessageDbDialogCalendarQuery {
            dialog_id,
            filter: MessageSearchFilter::Photo,
            from_message_id: MessageId::MAX,
            tz_offset: 3600,
        });
        assert_eq!(calendar.total_counts, vec![4, 1]);

        // sparse positions over the same 5 messages, limit 3: delta = 5/3,
        // position = (i + 0.5) * delta truncated → [0, 2, 4] into the
        // message_id-descending list
        let positions = db_impl
            .get_dialog_sparse_message_positions(MessageDbGetDialogSparseMessagePositionsQuery {
                dialog_id,
                filter: MessageSearchFilter::Photo,
                from_message_id: MessageId::MAX,
                limit: 3,
            })
            .unwrap();
        assert_eq!(positions.total_count, 5);
        let indexes: Vec<i32> = positions.positions.iter().map(|p| p.position).collect();
        assert_eq!(indexes, vec![0, 2, 4]);
        // the descending list is [86799, 86401, 86400, 86399, 300] << 20 and
        // each position carries its blob-header date
        assert_eq!(positions.positions[0].message_id.get(), 86_799 << 20);
        assert_eq!(positions.positions[0].date, 86_799);
        assert_eq!(positions.positions[2].message_id.get(), 300 << 20);
        assert_eq!(positions.positions[2].date, 300);
    }

    /// M.6.9: FTS query shaping, dialog/filter kludges and keyset paging.
    #[test]
    fn fts_query_and_calls_match_the_cpp_semantics() {
        // Photo: statement slot 3, mask bit 1 << 3
        let photo_mask = 1 << message_search_filter_index(MessageSearchFilter::Photo);
        let (_path, db) = open_db("m6_msgdb_fts");
        let mut db_impl = MessageDbImpl::new(db).unwrap();

        let dialog_id = DialogId::new(-17);
        let other_dialog = DialogId::new(-18);
        // three searchable messages in dialog -17, one in -18
        for (i, (dialog, search_id)) in [
            (dialog_id, 100),
            (dialog_id, 200),
            (dialog_id, 300),
            (other_dialog, 400),
        ]
        .iter()
        .enumerate()
        {
            db_impl.add_message(
                MessageFullId::new(*dialog, MessageId::new((i as i64 + 1) << 20)),
                ServerMessageId::new(0),
                DialogId::new(0),
                0,
                0,
                if i == 3 { photo_mask } else { 0 },
                *search_id,
                "searchable unicorn text".to_string(),
                NotificationId::new(0),
                MessageId::new(0),
                vec![i as u8],
            );
        }

        // plain query: the newest match first, next_search_id = its search_id
        let result = db_impl.get_messages_fts(MessageDbFtsQuery {
            query: "unicorn".to_string(),
            dialog_id: DialogId::new(0),
            filter: MessageSearchFilter::Empty,
            from_search_id: 0,
            limit: 10,
        });
        assert_eq!(result.messages.len(), 4);
        assert_eq!(result.next_search_id, 100);
        assert_eq!(result.messages[0].dialog_id, other_dialog);

        // keyset paging is strict `rowid < from_search_id`: from 200 only
        // search_id 100 remains
        let result = db_impl.get_messages_fts(MessageDbFtsQuery {
            query: "unicorn".to_string(),
            dialog_id: DialogId::new(0),
            filter: MessageSearchFilter::Empty,
            from_search_id: 200,
            limit: 10,
        });
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.next_search_id, 100);

        // dialog kludge: only dialog -17 matches
        let result = db_impl.get_messages_fts(MessageDbFtsQuery {
            query: "unicorn".to_string(),
            dialog_id,
            filter: MessageSearchFilter::Empty,
            from_search_id: 0,
            limit: 10,
        });
        assert_eq!(result.messages.len(), 3);

        // filter kludge: the only indexed (1<<4) message lives in -18
        let result = db_impl.get_messages_fts(MessageDbFtsQuery {
            query: "unicorn".to_string(),
            dialog_id: DialogId::new(0),
            filter: MessageSearchFilter::Photo,
            from_search_id: 0,
            limit: 10,
        });
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].dialog_id, other_dialog);

        // query shaping: multi-word input becomes quoted FTS words
        assert_eq!(
            MessageDbImpl::prepare_query("Hello, big world! __x__ 123"),
            "\"Hello\" \"big\" \"world\" \"__x__\" \"123\" "
        );
        // empty/separator-only queries shape to the empty string
        assert_eq!(MessageDbImpl::prepare_query("   ,.! "), "");

        // calls: Call (1<<9) and MissedCall (1<<10) rows by unique id
        for (id, unique, mask) in [
            (1, 11, 1 << 9),
            (2, 22, 1 << 9),
            (3, 33, 1 << 10),
            (4, 44, 1 << 10),
        ] {
            db_impl.add_message(
                MessageFullId::new(dialog_id, MessageId::new(id << 20)),
                ServerMessageId::new(unique),
                DialogId::new(0),
                0,
                0,
                mask,
                0,
                String::new(),
                NotificationId::new(0),
                MessageId::new(0),
                vec![id as u8],
            );
        }
        let calls = db_impl.get_calls(MessageDbCallsQuery {
            filter: MessageSearchFilter::Call,
            from_unique_message_id: 100,
            limit: 10,
        });
        assert_eq!(calls.messages.len(), 2);
        assert_eq!(calls.messages[0].message_id.get(), 2 << 20);
        let missed = db_impl.get_calls(MessageDbCallsQuery {
            filter: MessageSearchFilter::MissedCall,
            from_unique_message_id: 100,
            limit: 1,
        });
        assert_eq!(missed.messages.len(), 1);
        assert_eq!(missed.messages[0].message_id.get(), 4 << 20);

        // notification-id paging
        for (id, notification) in [(1, 5), (2, 7), (3, 9)] {
            db_impl.add_message(
                MessageFullId::new(other_dialog, MessageId::new((id + 10) << 20)),
                ServerMessageId::new(0),
                DialogId::new(0),
                0,
                0,
                0,
                0,
                String::new(),
                NotificationId::new(notification),
                MessageId::new(0),
                vec![],
            );
        }
        let page =
            db_impl.get_messages_from_notification_id(other_dialog, NotificationId::new(9), 10);
        let ids: Vec<i64> = page.iter().map(|m| m.message_id.get()).collect();
        assert_eq!(ids, vec![12 << 20, 11 << 20]);
    }

    /// M.6.10: the sync-safe wrapper — lazy creation, add/get through the
    /// trait object, close.
    #[test]
    fn message_db_sync_safe_round_trip() {
        use std::sync::Arc as StdArc;

        let path = std::env::temp_dir()
            .join("m6_msgdb_sync_safe")
            .to_string_lossy()
            .into_owned();
        let _ = tddb::sqlite_db::destroy(&path);
        let safe = StdArc::new(SqliteConnectionSafe::new(&path, DbKey::empty()));
        safe.init(&path, DbKey::empty()).ensure();
        // the schema is TdDb's job (MessageDb.cpp is called from TdDb::init)
        let mut version = 0;
        init_message_db(&safe.get(), &mut version).ensure();

        let sync_safe = MessageDbSyncSafe::new(safe.clone());
        let dialog_id = DialogId::new(-77);
        sync_safe.with_db(|db| {
            db.add_message(
                MessageFullId::new(dialog_id, MessageId::new(1 << 20)),
                ServerMessageId::new(1),
                DialogId::new(0),
                0,
                0,
                0,
                0,
                String::new(),
                NotificationId::new(0),
                MessageId::new(0),
                b"safe".to_vec(),
            );
        });
        let got = sync_safe
            .with_db(|db| db.get_message(MessageFullId::new(dialog_id, MessageId::new(1 << 20))));
        assert_eq!(got.unwrap().data, b"safe".to_vec());

        sync_safe.close();
        safe.close_and_destroy();
    }

    /// M.6.11 stress: N buffered writes burst through the batching bound,
    /// a read flushes first, every promise fires exactly once, close drains
    /// everything and the data lands.
    #[test]
    fn message_db_async_batches_and_drains() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tdactor::{ConcurrentScheduler, Scheduler};
        use tdutils::promise::PromiseCreator;

        const WRITES: usize = 120; // > 2 * MAX_PENDING_QUERY_COUNT

        let path = std::env::temp_dir()
            .join("m6_msgdb_async")
            .to_string_lossy()
            .into_owned();
        let _ = tddb::sqlite_db::destroy(&path);
        let safe = StdArc::new(SqliteConnectionSafe::new(&path, DbKey::empty()));
        safe.init(&path, DbKey::empty()).ensure();
        let mut version = 0;
        init_message_db(&safe.get(), &mut version).ensure();

        struct Choreography {
            // held in a slot so the `ActorOwn` is dropped on the main thread
            // after `finish()` — the actor stops itself in `on_close`, and the
            // C++ owner (TdDb) likewise releases the interface object later
            db_slot: StdArc<Mutex<Option<MessageDbAsync>>>,
            done: StdArc<AtomicUsize>,
            read_count: StdArc<AtomicUsize>,
        }
        impl Actor for Choreography {
            fn start_up(&mut self) {
                let db_slot = self.db_slot.clone();
                let db_guard = db_slot.lock().unwrap();
                let db = db_guard.as_ref().expect("db");
                let dialog_id = DialogId::new(-99);
                for i in 0..WRITES as i64 {
                    let done = self.done.clone();
                    let promise: Promise<()> = PromiseCreator::lambda(move |()| {
                        done.fetch_add(1, Ordering::SeqCst);
                    });
                    db.add_message(
                        MessageFullId::new(dialog_id, MessageId::new((i + 1) << 20)),
                        ServerMessageId::new((i + 1) as i32),
                        DialogId::new(0),
                        0,
                        0,
                        0,
                        0,
                        String::new(),
                        NotificationId::new(0),
                        MessageId::new(0),
                        vec![i as u8],
                        promise,
                    );
                }
                // the read flushes the batch first, then sees every write
                let read_count = self.read_count.clone();
                let done = self.done.clone();
                let read_promise: Promise<Vec<MessageDbDialogMessage>> =
                    PromiseCreator::lambda(move |messages: Vec<MessageDbDialogMessage>| {
                        read_count.store(messages.len(), Ordering::SeqCst);
                        // every write promise must have fired already: the
                        // read flush settles them before running the query
                        assert_eq!(done.load(Ordering::SeqCst), WRITES);
                    });
                db.get_messages(
                    MessageDbMessagesQuery {
                        dialog_id,
                        from_message_id: MessageId::MAX,
                        offset: 0,
                        limit: 1000,
                        ..MessageDbMessagesQuery::default()
                    },
                    read_promise,
                );
                // close drains the (already empty) batch and stops the actor;
                // its promise is the last event, so finishing there is safe
                let close_promise: Promise<()> =
                    PromiseCreator::lambda(|()| Scheduler::instance().finish());
                db.close(close_promise);
            }
        }

        let done = StdArc::new(AtomicUsize::new(0));
        let read_count = StdArc::new(AtomicUsize::new(0));
        let sync_safe = StdArc::new(MessageDbSyncSafe::new(safe.clone()));
        let db_slot: StdArc<Mutex<Option<MessageDbAsync>>> = StdArc::new(Mutex::new(None));
        let mut sched = ConcurrentScheduler::new(0);
        // `MessageDbAsync::new` needs a scheduler context; the test wires the
        // actor through `create_actor_unsafe`, the ConcurrentScheduler
        // pattern, instead
        *db_slot.lock().unwrap() = Some(MessageDbAsync {
            impl_: sched.create_actor_unsafe(
                0,
                "MessageDbActor",
                MessageDbAsyncImpl::new(sync_safe.clone()),
            ),
        });
        sched.create_actor_unsafe(
            0,
            "Choreography",
            Choreography {
                db_slot: db_slot.clone(),
                done: done.clone(),
                read_count: read_count.clone(),
            },
        );
        sched.start();
        while sched.run_main(10.0) {}
        sched.finish();

        assert_eq!(done.load(Ordering::SeqCst), WRITES);
        assert_eq!(read_count.load(Ordering::SeqCst), WRITES);

        // release the interface object outside any scheduler context
        *db_slot.lock().unwrap() = None;

        sync_safe.close();
        safe.close_and_destroy();
    }
}
