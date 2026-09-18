//! Port of `td/telegram/DialogDb.{h,cpp}` — the dialog database engine
//! (M.6.13/M.6.14): the sync interface and result structs (DialogDb.h:25-62),
//! `DialogDbImpl` (DialogDb.cpp:112-308), the safe wrapper (:310-327) and the
//! batching `DialogDbAsync` actor (:329-487). The schema ladder itself lives
//! in `tddb::td_db` (`init_dialog_db`/`drop_dialog_db`), like the C++ free
//! functions at DialogDb.cpp:28-110.
//!
//! `NotificationGroupKey` (NotificationGroupKey.h) is defined here until the
//! NotificationManager slice moves it beside the other notification types.

use std::sync::{Arc, Mutex};

use crate::dialog_id::{DialogId, FolderId, NotificationGroupId};

use tdactor::{
    Actor, ActorOwn, actor_messages, create_actor_on_scheduler, send_closure, send_closure_later,
};
use tddb::sqlite_connection_safe::SqliteConnectionSafe;
use tddb::sqlite_db::SqliteDb;
use tddb::sqlite_statement::{Datatype, SqliteStatement};
use tdutils::promise::Promise;
use tdutils::status::{Result, Status};
use tdutils::time::now_cached;
use tdutils::try_result;
use tdutils::try_status;

/// `NotificationGroupKey` (NotificationGroupKey.h:18-40).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NotificationGroupKey {
    pub group_id: NotificationGroupId,
    pub dialog_id: DialogId,
    pub last_notification_date: i32,
}

impl NotificationGroupKey {
    pub fn new(
        group_id: NotificationGroupId,
        dialog_id: DialogId,
        last_notification_date: i32,
    ) -> NotificationGroupKey {
        NotificationGroupKey {
            group_id,
            dialog_id,
            last_notification_date,
        }
    }
}

impl Ord for NotificationGroupKey {
    /// The C++ `operator<` orders newest first (NotificationGroupKey.h:32-39).
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self.last_notification_date != other.last_notification_date {
            return other
                .last_notification_date
                .cmp(&self.last_notification_date);
        }
        if self.dialog_id != other.dialog_id {
            return other.dialog_id.get().cmp(&self.dialog_id.get());
        }
        other.group_id.get().cmp(&self.group_id.get())
    }
}

impl PartialOrd for NotificationGroupKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// `DialogDbGetDialogsResult` (DialogDb.h:23-27).
#[derive(Clone, Debug, Default)]
pub struct DialogDbGetDialogsResult {
    pub dialogs: Vec<Vec<u8>>,
    pub next_order: i64,
    pub next_dialog_id: DialogId,
}

/// `DialogDbSyncInterface` (DialogDb.h:33-56).
pub trait DialogDbSyncInterface {
    fn add_dialog(
        &mut self,
        dialog_id: DialogId,
        folder_id: FolderId,
        order: i64,
        data: Vec<u8>,
        notification_groups: Vec<NotificationGroupKey>,
    );

    fn get_dialog(&mut self, dialog_id: DialogId) -> Result<Vec<u8>>;

    fn get_dialogs(
        &mut self,
        folder_id: FolderId,
        order: i64,
        dialog_id: DialogId,
        limit: i32,
    ) -> DialogDbGetDialogsResult;

    fn get_notification_groups_by_last_notification_date(
        &mut self,
        notification_group_key: NotificationGroupKey,
        limit: i32,
    ) -> Vec<NotificationGroupKey>;

    fn get_notification_group(
        &mut self,
        notification_group_id: NotificationGroupId,
    ) -> Result<NotificationGroupKey>;

    fn get_secret_chat_count(&mut self, folder_id: FolderId) -> i32;

    fn begin_read_transaction(&mut self) -> Status;
    fn begin_write_transaction(&mut self) -> Status;
    fn commit_transaction(&mut self) -> Status;
}

/// `td::DialogDbImpl` (DialogDb.cpp:112). The C++ default-constructs the
/// statement members and fills them in `init()`; the statements are created
/// in [`DialogDbImpl::new`] here instead, before the connection is moved
/// into the struct.
pub struct DialogDbImpl {
    db: SqliteDb,
    add_dialog_stmt: SqliteStatement,
    add_notification_group_stmt: SqliteStatement,
    delete_notification_group_stmt: SqliteStatement,
    get_dialog_stmt: SqliteStatement,
    get_dialogs_stmt: SqliteStatement,
    get_notification_groups_by_last_notification_date_stmt: SqliteStatement,
    get_notification_group_stmt: SqliteStatement,
    get_secret_chat_count_stmt: SqliteStatement,
}

impl DialogDbImpl {
    /// `DialogDbImpl(SqliteDb)` + `init` (DialogDb.cpp:114-154).
    pub fn new(db: SqliteDb) -> Result<DialogDbImpl> {
        try_result!(
            add_dialog_stmt,
            db.get_statement("INSERT OR REPLACE INTO dialogs VALUES(?1, ?2, ?3, ?4)")
        );
        try_result!(
            add_notification_group_stmt,
            db.get_statement("INSERT OR REPLACE INTO notification_groups VALUES(?1, ?2, ?3)")
        );
        try_result!(
            delete_notification_group_stmt,
            db.get_statement("DELETE FROM notification_groups WHERE notification_group_id = ?1")
        );
        try_result!(
            get_dialog_stmt,
            db.get_statement("SELECT data FROM dialogs WHERE dialog_id = ?1")
        );
        try_result!(
            get_dialogs_stmt,
            db.get_statement(
                "SELECT data, dialog_id, dialog_order FROM dialogs WHERE \
                 folder_id = ?1 AND (dialog_order < ?2 OR (dialog_order = ?2 AND dialog_id < ?3)) ORDER \
                 BY dialog_order DESC, dialog_id DESC LIMIT ?4"
            )
        );
        // The C++ keeps the row-value comparison variant of the WHERE clause
        // commented out (DialogDb.cpp:136-137); the live form is ported.
        try_result!(
            get_notification_groups_by_last_notification_date_stmt,
            db.get_statement(
                "SELECT notification_group_id, dialog_id, last_notification_date FROM notification_groups \
                 WHERE last_notification_date < ?1 OR (last_notification_date = ?1 \
                 AND (dialog_id < ?2 OR (dialog_id = ?2 AND notification_group_id < ?3))) ORDER BY \
                 last_notification_date DESC, dialog_id DESC LIMIT ?4"
            )
        );
        try_result!(
            get_notification_group_stmt,
            db.get_statement(
                "SELECT dialog_id, last_notification_date FROM notification_groups WHERE notification_group_id = ?1"
            )
        );
        try_result!(
            get_secret_chat_count_stmt,
            db.get_statement(
                "SELECT COUNT(*) FROM dialogs WHERE folder_id = ?1 AND dialog_order > 0 AND dialog_id < -1500000000000"
            )
        );

        Result::Ok(DialogDbImpl {
            db,
            add_dialog_stmt,
            add_notification_group_stmt,
            delete_notification_group_stmt,
            get_dialog_stmt,
            get_dialogs_stmt,
            get_notification_groups_by_last_notification_date_stmt,
            get_notification_group_stmt,
            get_secret_chat_count_stmt,
        })
    }

    /// `DialogDbImpl::get_last_notification_date` (DialogDb.cpp:302-307).
    fn get_last_notification_date(stmt: &SqliteStatement, id: i32) -> i32 {
        if stmt.view_datatype(id) == Datatype::Null {
            return 0;
        }
        stmt.view_int32(id)
    }
}

impl DialogDbImpl {
    /// `DialogDbImpl::add_dialog` (DialogDb.cpp:156-193). A notification
    /// group with a valid dialog is written (the date is NULL-ed at 0, like
    /// the C++ `bind_null`); with an invalid dialog the group row is deleted.
    pub fn add_dialog(
        &mut self,
        dialog_id: DialogId,
        folder_id: FolderId,
        order: i64,
        data: Vec<u8>,
        notification_groups: Vec<NotificationGroupKey>,
    ) {
        self.add_dialog_stmt.bind_int64(1, dialog_id.get()).ensure();
        self.add_dialog_stmt.bind_int64(2, order).ensure();
        self.add_dialog_stmt.bind_blob(3, &data).ensure();
        if order > 0 {
            self.add_dialog_stmt.bind_int32(4, folder_id.get()).ensure();
        } else {
            self.add_dialog_stmt.bind_null(4).ensure();
        }

        self.add_dialog_stmt.step().ensure();
        self.add_dialog_stmt.reset();

        for to_add in notification_groups {
            if to_add.dialog_id.is_valid() {
                self.add_notification_group_stmt
                    .bind_int32(1, to_add.group_id.get())
                    .ensure();
                self.add_notification_group_stmt
                    .bind_int64(2, to_add.dialog_id.get())
                    .ensure();
                if to_add.last_notification_date != 0 {
                    self.add_notification_group_stmt
                        .bind_int32(3, to_add.last_notification_date)
                        .ensure();
                } else {
                    self.add_notification_group_stmt.bind_null(3).ensure();
                }
                self.add_notification_group_stmt.step().ensure();
                self.add_notification_group_stmt.reset();
            } else {
                self.delete_notification_group_stmt
                    .bind_int32(1, to_add.group_id.get())
                    .ensure();
                self.delete_notification_group_stmt.step().ensure();
                self.delete_notification_group_stmt.reset();
            }
        }
    }

    /// `DialogDbImpl::get_dialog` (DialogDb.cpp:195-206).
    pub fn get_dialog(&mut self, dialog_id: DialogId) -> Result<Vec<u8>> {
        self.get_dialog_stmt.bind_int64(1, dialog_id.get()).ensure();
        try_status!(self.get_dialog_stmt.step());
        if !self.get_dialog_stmt.has_row() {
            self.get_dialog_stmt.reset();
            return Result::from_status(Status::error_message("Not found"));
        }
        let result = self.get_dialog_stmt.view_blob(0).to_vec();
        self.get_dialog_stmt.reset();
        Result::Ok(result)
    }

    /// `DialogDbImpl::get_notification_group` (DialogDb.cpp:208-219).
    pub fn get_notification_group(
        &mut self,
        notification_group_id: NotificationGroupId,
    ) -> Result<NotificationGroupKey> {
        self.get_notification_group_stmt
            .bind_int32(1, notification_group_id.get())
            .ensure();
        try_status!(self.get_notification_group_stmt.step());
        if !self.get_notification_group_stmt.has_row() {
            self.get_notification_group_stmt.reset();
            return Result::from_status(Status::error_message("Not found"));
        }
        let result = NotificationGroupKey::new(
            notification_group_id,
            DialogId::new(self.get_notification_group_stmt.view_int64(0)),
            Self::get_last_notification_date(&self.get_notification_group_stmt, 1),
        );
        self.get_notification_group_stmt.reset();
        Result::Ok(result)
    }

    /// `DialogDbImpl::get_secret_chat_count` (DialogDb.cpp:221-229).
    pub fn get_secret_chat_count(&mut self, folder_id: FolderId) -> i32 {
        self.get_secret_chat_count_stmt
            .bind_int32(1, folder_id.get())
            .ensure();
        self.get_secret_chat_count_stmt.step().ensure();
        assert!(
            self.get_secret_chat_count_stmt.has_row(),
            "COUNT(*) must return a row"
        );
        let count = self.get_secret_chat_count_stmt.view_int32(0);
        self.get_secret_chat_count_stmt.reset();
        count
    }

    /// `DialogDbImpl::get_dialogs` (DialogDb.cpp:231-255) — keyset paging by
    /// `(dialog_order, dialog_id)` descending; the returned cursor is the
    /// last seen row (the caller resumes from it).
    pub fn get_dialogs(
        &mut self,
        folder_id: FolderId,
        order: i64,
        dialog_id: DialogId,
        limit: i32,
    ) -> DialogDbGetDialogsResult {
        self.get_dialogs_stmt
            .bind_int32(1, folder_id.get())
            .ensure();
        self.get_dialogs_stmt.bind_int64(2, order).ensure();
        self.get_dialogs_stmt
            .bind_int64(3, dialog_id.get())
            .ensure();
        self.get_dialogs_stmt.bind_int32(4, limit).ensure();

        let mut result = DialogDbGetDialogsResult {
            next_dialog_id: dialog_id,
            next_order: order,
            ..DialogDbGetDialogsResult::default()
        };
        self.get_dialogs_stmt.step().ensure();
        while self.get_dialogs_stmt.has_row() {
            let data = self.get_dialogs_stmt.view_blob(0).to_vec();
            result.next_dialog_id = DialogId::new(self.get_dialogs_stmt.view_int64(1));
            result.next_order = self.get_dialogs_stmt.view_int64(2);
            tracing::info!(
                "Load {} with order {}",
                result.next_dialog_id.get(),
                result.next_order
            );
            result.dialogs.push(data);
            self.get_dialogs_stmt.step().ensure();
        }
        self.get_dialogs_stmt.reset();

        result
    }

    /// `DialogDbImpl::get_notification_groups_by_last_notification_date`
    /// (DialogDb.cpp:257-278).
    pub fn get_notification_groups_by_last_notification_date(
        &mut self,
        notification_group_key: NotificationGroupKey,
        limit: i32,
    ) -> Vec<NotificationGroupKey> {
        let stmt = &mut self.get_notification_groups_by_last_notification_date_stmt;
        stmt.bind_int32(1, notification_group_key.last_notification_date)
            .ensure();
        stmt.bind_int64(2, notification_group_key.dialog_id.get())
            .ensure();
        stmt.bind_int32(3, notification_group_key.group_id.get())
            .ensure();
        stmt.bind_int32(4, limit).ensure();

        let mut notification_groups = Vec::new();
        stmt.step().ensure();
        while stmt.has_row() {
            notification_groups.push(NotificationGroupKey::new(
                NotificationGroupId::new(stmt.view_int32(0)),
                DialogId::new(stmt.view_int64(1)),
                Self::get_last_notification_date(stmt, 2),
            ));
            stmt.step().ensure();
        }
        stmt.reset();

        notification_groups
    }
}

impl DialogDbSyncInterface for DialogDbImpl {
    fn add_dialog(
        &mut self,
        dialog_id: DialogId,
        folder_id: FolderId,
        order: i64,
        data: Vec<u8>,
        notification_groups: Vec<NotificationGroupKey>,
    ) {
        DialogDbImpl::add_dialog(self, dialog_id, folder_id, order, data, notification_groups)
    }

    fn get_dialog(&mut self, dialog_id: DialogId) -> Result<Vec<u8>> {
        DialogDbImpl::get_dialog(self, dialog_id)
    }

    fn get_dialogs(
        &mut self,
        folder_id: FolderId,
        order: i64,
        dialog_id: DialogId,
        limit: i32,
    ) -> DialogDbGetDialogsResult {
        DialogDbImpl::get_dialogs(self, folder_id, order, dialog_id, limit)
    }

    fn get_notification_groups_by_last_notification_date(
        &mut self,
        notification_group_key: NotificationGroupKey,
        limit: i32,
    ) -> Vec<NotificationGroupKey> {
        DialogDbImpl::get_notification_groups_by_last_notification_date(
            self,
            notification_group_key,
            limit,
        )
    }

    fn get_notification_group(
        &mut self,
        notification_group_id: NotificationGroupId,
    ) -> Result<NotificationGroupKey> {
        DialogDbImpl::get_notification_group(self, notification_group_id)
    }

    fn get_secret_chat_count(&mut self, folder_id: FolderId) -> i32 {
        DialogDbImpl::get_secret_chat_count(self, folder_id)
    }

    fn begin_read_transaction(&mut self) -> Status {
        self.db.begin_read_transaction()
    }
    fn begin_write_transaction(&mut self) -> Status {
        self.db.begin_write_transaction()
    }
    fn commit_transaction(&mut self) -> Status {
        self.db.commit_transaction()
    }
}

/// `create_dialog_db_sync` + `DialogDbSyncSafe` (DialogDb.cpp:310-327): the
/// port follows the shared connection subset of [`SqliteConnectionSafe`]
/// (see that module's docs) — one lazily-created [`DialogDbImpl`] behind a
/// mutex instead of a per-scheduler-thread lsls instance.
pub struct DialogDbSyncSafe {
    inner: Mutex<Option<DialogDbImpl>>,
    safe_connection: Arc<SqliteConnectionSafe>,
}

impl DialogDbSyncSafe {
    /// `create_dialog_db_sync`.
    pub fn new(safe_connection: Arc<SqliteConnectionSafe>) -> DialogDbSyncSafe {
        DialogDbSyncSafe {
            inner: Mutex::new(None),
            safe_connection,
        }
    }

    /// `DialogDbSyncSafeInterface::get()` — the closure runs against the
    /// (lazily created, on first use) implementation.
    pub fn with_db<R>(&self, f: impl FnOnce(&mut dyn DialogDbSyncInterface) -> R) -> R {
        let mut inner = self.inner.lock().unwrap();
        if inner.is_none() {
            let db = DialogDbImpl::new(self.safe_connection.get().clone()).unwrap();
            *inner = Some(db);
        }
        f(inner.as_mut().unwrap())
    }

    /// dropping the last reference closes the implementation.
    pub fn close(&self) {
        *self.inner.lock().unwrap() = None;
    }
}

/// `DialogDbAsyncInterface` (DialogDb.h:70-94).
pub trait DialogDbAsyncInterface: Send {
    fn add_dialog(
        &self,
        dialog_id: DialogId,
        folder_id: FolderId,
        order: i64,
        data: Vec<u8>,
        notification_groups: Vec<NotificationGroupKey>,
        promise: Promise<()>,
    );

    fn get_dialog(&self, dialog_id: DialogId, promise: Promise<Vec<u8>>);

    fn get_dialogs(
        &self,
        folder_id: FolderId,
        order: i64,
        dialog_id: DialogId,
        limit: i32,
        promise: Promise<DialogDbGetDialogsResult>,
    );

    fn get_notification_groups_by_last_notification_date(
        &self,
        notification_group_key: NotificationGroupKey,
        limit: i32,
        promise: Promise<Vec<NotificationGroupKey>>,
    );

    fn get_notification_group(
        &self,
        notification_group_id: NotificationGroupId,
        promise: Promise<NotificationGroupKey>,
    );

    fn get_secret_chat_count(&self, folder_id: FolderId, promise: Promise<i32>);

    fn close(&self, promise: Promise<()>);
    fn force_flush(&self);
}

/// `DialogDbAsync` (DialogDb.cpp:329-482): the forwarding wrapper around the
/// [`DialogDbAsyncImpl`] actor. The C++ forwards `add_dialog` and the
/// notification queries with `send_closure` and `get_dialog`/`get_dialogs`/
/// `close`/`force_flush` with `send_closure_later` (DialogDb.cpp:335-370) —
/// the split is ported.
pub struct DialogDbAsync {
    impl_: ActorOwn<DialogDbAsyncImpl>,
}

impl DialogDbAsync {
    /// `DialogDbAsync(sync_db, scheduler_id)` + `create_dialog_db_async`
    /// (DialogDb.cpp:484-487); the C++ default scheduler id is -1.
    pub fn new(sync_db: Arc<DialogDbSyncSafe>, scheduler_id: i32) -> DialogDbAsync {
        DialogDbAsync {
            impl_: create_actor_on_scheduler(
                "DialogDbActor",
                scheduler_id,
                DialogDbAsyncImpl::new(sync_db),
            ),
        }
    }
}

impl DialogDbAsyncInterface for DialogDbAsync {
    fn add_dialog(
        &self,
        dialog_id: DialogId,
        folder_id: FolderId,
        order: i64,
        data: Vec<u8>,
        notification_groups: Vec<NotificationGroupKey>,
        promise: Promise<()>,
    ) {
        send_closure(
            self.impl_.get(),
            DialogDbAsyncMessage::AddDialog {
                dialog_id,
                folder_id,
                order,
                data,
                notification_groups,
                promise,
            },
        );
    }

    fn get_dialog(&self, dialog_id: DialogId, promise: Promise<Vec<u8>>) {
        send_closure_later(
            self.impl_.get(),
            DialogDbAsyncMessage::GetDialog { dialog_id, promise },
        );
    }

    fn get_dialogs(
        &self,
        folder_id: FolderId,
        order: i64,
        dialog_id: DialogId,
        limit: i32,
        promise: Promise<DialogDbGetDialogsResult>,
    ) {
        send_closure_later(
            self.impl_.get(),
            DialogDbAsyncMessage::GetDialogs {
                folder_id,
                order,
                dialog_id,
                limit,
                promise,
            },
        );
    }

    fn get_notification_groups_by_last_notification_date(
        &self,
        notification_group_key: NotificationGroupKey,
        limit: i32,
        promise: Promise<Vec<NotificationGroupKey>>,
    ) {
        send_closure(
            self.impl_.get(),
            DialogDbAsyncMessage::GetNotificationGroupsByLastNotificationDate {
                notification_group_key,
                limit,
                promise,
            },
        );
    }

    fn get_notification_group(
        &self,
        notification_group_id: NotificationGroupId,
        promise: Promise<NotificationGroupKey>,
    ) {
        send_closure(
            self.impl_.get(),
            DialogDbAsyncMessage::GetNotificationGroup {
                notification_group_id,
                promise,
            },
        );
    }

    fn get_secret_chat_count(&self, folder_id: FolderId, promise: Promise<i32>) {
        send_closure(
            self.impl_.get(),
            DialogDbAsyncMessage::GetSecretChatCount { folder_id, promise },
        );
    }

    fn close(&self, promise: Promise<()>) {
        send_closure_later(self.impl_.get(), DialogDbAsyncMessage::Close { promise });
    }

    /// `force_flush` — the actor message carries no payload, so it is sent
    /// as a plain actor thunk (the C++ `send_closure_later` of a
    /// payload-less method).
    fn force_flush(&self) {
        tdactor::send_thunk_later(
            tdactor::ActorRef::from(self.impl_.get()),
            tdactor::event::ActorThunk::from_boxed(Box::new(move |actor| {
                // SAFETY: the `ActorId<DialogDbAsyncImpl>` marker names the
                // actor's real type (the C++ `static_cast` in send_closure)
                let actor = unsafe { &mut *(actor as *mut dyn Actor as *mut DialogDbAsyncImpl) };
                actor.on_force_flush();
            })),
        );
    }
}

/// `DialogDbAsync::Impl` (DialogDb.cpp:373-480). The C++ buffers write
/// queries as `PromiseCreator::lambda`s capturing `this`; here they are
/// buffered as the [`AddDialogWrite`] struct, which keeps the exact batching
/// order (writes run inside one transaction, promises fire after commit)
/// without the raw-pointer capture. The database has exactly one write
/// query family (`add_dialog`), so the buffer is a vector of that struct.
struct AddDialogWrite {
    dialog_id: DialogId,
    folder_id: FolderId,
    order: i64,
    data: Vec<u8>,
    notification_groups: Vec<NotificationGroupKey>,
    promise: Promise<()>,
}

/// `MAX_PENDING_QUERY_COUNT` / `MAX_PENDING_QUERY_DELAY`
/// (DialogDb.cpp:436-437).
const MAX_PENDING_QUERY_COUNT: usize = 50;
const MAX_PENDING_QUERY_DELAY: f64 = 0.01;

pub struct DialogDbAsyncImpl {
    sync_db_safe: Option<Arc<DialogDbSyncSafe>>,
    finished_writes: Vec<Promise<()>>,
    pending_writes: Vec<AddDialogWrite>,
    wakeup_at: f64,
}

impl DialogDbAsyncImpl {
    fn new(sync_db_safe: Arc<DialogDbSyncSafe>) -> DialogDbAsyncImpl {
        DialogDbAsyncImpl {
            sync_db_safe: Some(sync_db_safe),
            finished_writes: Vec::new(),
            pending_writes: Vec::new(),
            wakeup_at: 0.0,
        }
    }

    /// `Impl::sync_db_->` — run a closure against the implementation.
    fn run_sync<R>(&mut self, f: impl FnOnce(&mut dyn DialogDbSyncInterface) -> R) -> R {
        let sync_db_safe = self
            .sync_db_safe
            .as_ref()
            .expect("DialogDbAsyncImpl used after close");
        sync_db_safe.with_db(f)
    }

    /// `Impl::on_write_result` (DialogDb.cpp:387-390): we are inside a
    /// transaction and don't know how to handle errors.
    fn on_write_result(&mut self, promise: Promise<()>) {
        self.finished_writes.push(promise);
    }

    /// `Impl::add_write_query` (DialogDb.cpp:444-456).
    fn add_write_query(&mut self, query: AddDialogWrite) {
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

    /// `Impl::add_read_query` (DialogDb.cpp:458-460).
    fn add_read_query(&mut self) {
        self.do_flush();
    }

    /// `Impl::do_flush` (DialogDb.cpp:462-471): begin transaction, run the
    /// buffered writes, commit, then settle the finished-write promises.
    fn do_flush(&mut self) {
        if self.pending_writes.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_writes);
        self.run_sync(|db| db.begin_write_transaction().ensure());
        for query in pending {
            let AddDialogWrite {
                dialog_id,
                folder_id,
                order,
                data,
                notification_groups,
                promise,
            } = query;
            self.run_sync(|db| {
                db.add_dialog(dialog_id, folder_id, order, data, notification_groups)
            });
            self.on_write_result(promise);
        }
        self.run_sync(|db| db.commit_transaction().ensure());
        for mut promise in std::mem::take(&mut self.finished_writes) {
            promise.set_value(());
        }
        self.cancel_timeout();
    }

    /// `Impl::close` (DialogDb.cpp:419-425).
    fn on_close(&mut self, mut promise: Promise<()>) {
        self.do_flush();
        self.sync_db_safe = None;
        promise.set_value(());
        self.stop();
    }

    /// `Impl::force_flush` (DialogDb.cpp:427-430).
    fn on_force_flush(&mut self) {
        self.do_flush();
        tracing::info!("DialogDb flushed");
    }
}

impl Actor for DialogDbAsyncImpl {
    /// `Impl::start_up` (DialogDb.cpp:477-479) — create the
    /// implementation eagerly.
    fn start_up(&mut self) {
        self.run_sync(|_| {});
    }

    /// `Impl::timeout_expired` (DialogDb.cpp:473-475).
    fn timeout_expired(&mut self) {
        self.do_flush();
    }
}

actor_messages! {
    pub enum DialogDbAsyncMessage for DialogDbAsyncImpl {
        AddDialog {
            dialog_id: DialogId,
            folder_id: FolderId,
            order: i64,
            data: Vec<u8>,
            notification_groups: Vec<NotificationGroupKey>,
            promise: Promise<()>,
        } => on_add_dialog(dialog_id, folder_id, order, data, notification_groups, promise),
        GetDialog {
            dialog_id: DialogId,
            promise: Promise<Vec<u8>>,
        } => on_get_dialog(dialog_id, promise),
        GetDialogs {
            folder_id: FolderId,
            order: i64,
            dialog_id: DialogId,
            limit: i32,
            promise: Promise<DialogDbGetDialogsResult>,
        } => on_get_dialogs(folder_id, order, dialog_id, limit, promise),
        GetNotificationGroupsByLastNotificationDate {
            notification_group_key: NotificationGroupKey,
            limit: i32,
            promise: Promise<Vec<NotificationGroupKey>>,
        } => on_get_notification_groups_by_last_notification_date(notification_group_key, limit, promise),
        GetNotificationGroup {
            notification_group_id: NotificationGroupId,
            promise: Promise<NotificationGroupKey>,
        } => on_get_notification_group(notification_group_id, promise),
        GetSecretChatCount {
            folder_id: FolderId,
            promise: Promise<i32>,
        } => on_get_secret_chat_count(folder_id, promise),
        Close {
            promise: Promise<()>,
        } => on_close(promise),
    }
}

impl DialogDbAsyncImpl {
    /// `Impl::add_dialog` (DialogDb.cpp:378-385).
    fn on_add_dialog(
        &mut self,
        dialog_id: DialogId,
        folder_id: FolderId,
        order: i64,
        data: Vec<u8>,
        notification_groups: Vec<NotificationGroupKey>,
        promise: Promise<()>,
    ) {
        self.add_write_query(AddDialogWrite {
            dialog_id,
            folder_id,
            order,
            data,
            notification_groups,
            promise,
        });
    }

    /// `Impl::get_dialog` (DialogDb.cpp:408-411).
    fn on_get_dialog(&mut self, dialog_id: DialogId, mut promise: Promise<Vec<u8>>) {
        self.add_read_query();
        let result = self.run_sync(|db| db.get_dialog(dialog_id));
        promise.set_result(result);
    }

    /// `Impl::get_dialogs` (DialogDb.cpp:413-417).
    fn on_get_dialogs(
        &mut self,
        folder_id: FolderId,
        order: i64,
        dialog_id: DialogId,
        limit: i32,
        mut promise: Promise<DialogDbGetDialogsResult>,
    ) {
        self.add_read_query();
        let result = self.run_sync(|db| db.get_dialogs(folder_id, order, dialog_id, limit));
        promise.set_value(result);
    }

    /// `Impl::get_notification_groups_by_last_notification_date`
    /// (DialogDb.cpp:392-396).
    fn on_get_notification_groups_by_last_notification_date(
        &mut self,
        notification_group_key: NotificationGroupKey,
        limit: i32,
        mut promise: Promise<Vec<NotificationGroupKey>>,
    ) {
        self.add_read_query();
        let result = self.run_sync(|db| {
            db.get_notification_groups_by_last_notification_date(notification_group_key, limit)
        });
        promise.set_value(result);
    }

    /// `Impl::get_notification_group` (DialogDb.cpp:398-401).
    fn on_get_notification_group(
        &mut self,
        notification_group_id: NotificationGroupId,
        mut promise: Promise<NotificationGroupKey>,
    ) {
        self.add_read_query();
        let result = self.run_sync(|db| db.get_notification_group(notification_group_id));
        promise.set_result(result);
    }

    /// `Impl::get_secret_chat_count` (DialogDb.cpp:403-406).
    fn on_get_secret_chat_count(&mut self, folder_id: FolderId, mut promise: Promise<i32>) {
        self.add_read_query();
        let result = self.run_sync(|db| db.get_secret_chat_count(folder_id));
        promise.set_value(result);
    }
}

/// `create_dialog_db_async` (DialogDb.cpp:484-487).
pub fn create_dialog_db_async(sync_db: Arc<DialogDbSyncSafe>, scheduler_id: i32) -> DialogDbAsync {
    DialogDbAsync::new(sync_db, scheduler_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tddb::binlog_key_value::BinlogKeyValue;
    use tddb::db_key::DbKey;
    use tddb::td_db::init_dialog_db;

    fn temp_path(name: &str) -> String {
        std::env::temp_dir()
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    /// A throwaway binlog-backed pmc (the init ladder writes the
    /// pinned-dialogs migration results into it).
    fn test_binlog_pmc(name: &str) -> BinlogKeyValue {
        let path = temp_path(name);
        let _ = BinlogKeyValue::destroy(&path);
        let mut pmc = BinlogKeyValue::new();
        pmc.init(&path, DbKey::empty()).ensure();
        pmc
    }

    fn open_db(name: &str) -> (String, SqliteDb) {
        let path = temp_path(name);
        let _ = tddb::sqlite_db::destroy(&path);
        let db = SqliteDb::open_with_key(&path, true, &DbKey::empty(), None).unwrap();
        let mut version = 0;
        let mut pmc = test_binlog_pmc(&format!("{name}_pmc"));
        let mut was_created = false;
        init_dialog_db(&db, &mut version, &mut pmc, &mut was_created).ensure();
        (path, db)
    }

    fn raw_count(db: &SqliteDb, sql: &str) -> i32 {
        let mut stmt = db.get_statement(sql).unwrap();
        stmt.step().ensure();
        stmt.view_int32(0)
    }

    /// M.6.13: add → the raw row carries the byte-identical blob; the
    /// folder column is bound for a positive order and NULL-ed otherwise.
    #[test]
    fn add_dialog_round_trip_and_folder_nulling() {
        let (path, db) = open_db("m6_dialogdb_add");
        let raw_db = db.clone();
        let mut db = DialogDbImpl::new(db).unwrap();

        let dialog_id = DialogId::new(-1001);
        db.add_dialog(
            dialog_id,
            FolderId::new(0),
            9221294780217032705,
            b"blob-one".to_vec(),
            Vec::new(),
        );
        let dialog_id_null = DialogId::new(2001);
        db.add_dialog(
            dialog_id_null,
            FolderId::new(0),
            0,
            b"blob-null".to_vec(),
            Vec::new(),
        );

        assert_eq!(
            db.get_dialog(dialog_id).unwrap(),
            b"blob-one".to_vec(),
            "the blob must survive the round trip byte-identically"
        );
        assert_eq!(
            db.get_dialog(dialog_id_null).unwrap(),
            b"blob-null".to_vec()
        );
        // "Not found" for a missing dialog
        assert!(db.get_dialog(DialogId::new(9999)).is_error());

        assert_eq!(
            raw_count(
                &raw_db,
                "SELECT COUNT(*) FROM dialogs WHERE folder_id IS NULL"
            ),
            1
        );
        assert_eq!(
            raw_count(&raw_db, "SELECT COUNT(*) FROM dialogs WHERE folder_id = 0"),
            1
        );

        drop(db);
        drop(raw_db);
        let _ = tddb::sqlite_db::destroy(&path);
    }

    /// M.6.13: get_dialogs pages by (dialog_order, dialog_id) DESC and the
    /// returned cursor continues the enumeration.
    #[test]
    fn get_dialogs_keyset_paging() {
        let (path, db) = open_db("m6_dialogdb_paging");
        let mut db = DialogDbImpl::new(db).unwrap();

        // (order, dialog_id): the DESC enumeration of the fixture is
        // (104, -4), (103, -3), (102, -2), (101, -1)
        for (order, dialog_id) in [(101i64, -1i64), (103, -3), (102, -2), (104, -4)] {
            db.add_dialog(
                DialogId::new(dialog_id),
                FolderId::new(0),
                order,
                format!("blob-{order}").into_bytes(),
                Vec::new(),
            );
        }

        let page1 = db.get_dialogs(FolderId::new(0), i64::MAX, DialogId::new(i64::MAX), 2);
        assert_eq!(page1.dialogs.len(), 2);
        assert_eq!(page1.dialogs[0], b"blob-104".to_vec());
        assert_eq!(page1.dialogs[1], b"blob-103".to_vec());
        assert_eq!(page1.next_order, 103);
        assert_eq!(page1.next_dialog_id.get(), -3);

        let page2 = db.get_dialogs(FolderId::new(0), page1.next_order, page1.next_dialog_id, 2);
        assert_eq!(page2.dialogs.len(), 2);
        assert_eq!(page2.dialogs[0], b"blob-102".to_vec());
        assert_eq!(page2.dialogs[1], b"blob-101".to_vec());
        assert_eq!(page2.next_order, 101);
        assert_eq!(page2.next_dialog_id.get(), -1);

        // the end of the enumeration is an empty page keeping the cursor
        let page3 = db.get_dialogs(FolderId::new(0), page2.next_order, page2.next_dialog_id, 2);
        assert!(page3.dialogs.is_empty());
        assert_eq!(page3.next_order, 101);
        assert_eq!(page3.next_dialog_id.get(), -1);

        // rows of another folder stay out (NULL folder rows never match)
        assert!(
            db.get_dialogs(FolderId::new(1), i64::MAX, DialogId::new(i64::MAX), 10)
                .dialogs
                .is_empty()
        );

        drop(db);
        let _ = tddb::sqlite_db::destroy(&path);
    }

    /// M.6.14: valid-dialog groups are written (INSERT OR REPLACE, date 0
    /// NULL-ed and read back as 0); an invalid dialog deletes the group.
    #[test]
    fn notification_groups_add_replace_delete() {
        let (path, db) = open_db("m6_dialogdb_notif");
        let mut db = DialogDbImpl::new(db).unwrap();

        let group_id = NotificationGroupId::new(7);
        // first add with date 0 -> NULL
        db.add_dialog(
            DialogId::new(-100),
            FolderId::new(0),
            5,
            b"d".to_vec(),
            vec![NotificationGroupKey::new(group_id, DialogId::new(-100), 0)],
        );
        assert_eq!(
            db.get_notification_group(group_id).unwrap(),
            NotificationGroupKey::new(group_id, DialogId::new(-100), 0),
            "a NULL date reads back as 0"
        );

        // replace with a real date
        db.add_dialog(
            DialogId::new(-100),
            FolderId::new(0),
            5,
            b"d".to_vec(),
            vec![NotificationGroupKey::new(
                group_id,
                DialogId::new(-100),
                777,
            )],
        );
        assert_eq!(
            db.get_notification_group(group_id).unwrap(),
            NotificationGroupKey::new(group_id, DialogId::new(-100), 777)
        );

        // a second group to exercise the by-date enumeration
        let group_id2 = NotificationGroupId::new(8);
        db.add_dialog(
            DialogId::new(-101),
            FolderId::new(0),
            6,
            b"e".to_vec(),
            vec![NotificationGroupKey::new(
                group_id2,
                DialogId::new(-101),
                888,
            )],
        );
        let groups = db.get_notification_groups_by_last_notification_date(
            NotificationGroupKey::new(
                NotificationGroupId::new(i32::MAX),
                DialogId::new(i64::MAX),
                i32::MAX,
            ),
            10,
        );
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].group_id, group_id2);
        assert_eq!(groups[1].group_id, group_id);

        // deleting a notification (invalid dialog key) removes the row
        db.add_dialog(
            DialogId::new(0),
            FolderId::new(0),
            0,
            b"f".to_vec(),
            vec![NotificationGroupKey::new(group_id, DialogId::new(0), 0)],
        );
        assert!(db.get_notification_group(group_id).is_error());
        assert!(db.get_notification_group(group_id2).is_ok());

        drop(db);
        let _ = tddb::sqlite_db::destroy(&path);
    }

    /// M.6.14: `get_secret_chat_count` counts only rows in the secret-chat
    /// id band with a positive order, per folder.
    #[test]
    fn secret_chat_count_counts_secret_chats_per_folder() {
        let (path, db) = open_db("m6_dialogdb_secret");
        let mut db = DialogDbImpl::new(db).unwrap();

        // in-band secret chats (dialog_id < -1500000000000), one archived
        for (dialog_id, order, folder) in [
            (-1500000000001i64, 10i64, 0i32),
            (-1500000000002, 20, 0),
            (-1500000000003, 30, 1),
        ] {
            db.add_dialog(
                DialogId::new(dialog_id),
                FolderId::new(folder),
                order,
                b"s".to_vec(),
                Vec::new(),
            );
        }
        // out-of-band rows never count: a user dialog, a zero-order secret
        // chat and a channel in the same band... (the C++ WHERE filters all)
        for (dialog_id, order, folder) in [
            (2001i64, 5i64, 0i32),
            (-1500000000004, 0, 0),
            (-1000, 50, 0),
        ] {
            db.add_dialog(
                DialogId::new(dialog_id),
                FolderId::new(folder),
                order,
                b"o".to_vec(),
                Vec::new(),
            );
        }

        assert_eq!(db.get_secret_chat_count(FolderId::new(0)), 2);
        assert_eq!(db.get_secret_chat_count(FolderId::new(1)), 1);

        drop(db);
        let _ = tddb::sqlite_db::destroy(&path);
    }

    /// M.6.14 stress: writes batch into one transaction, a read flushes
    /// first (and sees every write), every promise fires exactly once, and
    /// close drains everything.
    #[test]
    fn dialog_db_async_batches_and_drains() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tdactor::{ConcurrentScheduler, Scheduler};
        use tdutils::promise::PromiseCreator;

        const WRITES: usize = 120; // > 2 * MAX_PENDING_QUERY_COUNT

        let path = temp_path("m6_dialogdb_async");
        let _ = tddb::sqlite_db::destroy(&path);
        let safe = StdArc::new(SqliteConnectionSafe::new(&path, DbKey::empty()));
        safe.init(&path, DbKey::empty()).ensure();
        let mut version = 0;
        let mut pmc = test_binlog_pmc("m6_dialogdb_async_pmc");
        let mut was_created = false;
        init_dialog_db(&safe.get(), &mut version, &mut pmc, &mut was_created).ensure();

        struct Choreography {
            // held in a slot so the `ActorOwn` is dropped on the main thread
            // after `finish()` — the actor stops itself in `on_close`, and the
            // C++ owner (TdDb) likewise releases the interface object later
            db_slot: StdArc<Mutex<Option<DialogDbAsync>>>,
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
                    db.add_dialog(
                        DialogId::new(dialog_id.get() - i),
                        FolderId::new(0),
                        i + 1,
                        vec![i as u8],
                        Vec::new(),
                        promise,
                    );
                }
                // the read flushes the batch first, then sees every write
                let read_count = self.read_count.clone();
                let done = self.done.clone();
                let read_promise: Promise<DialogDbGetDialogsResult> =
                    PromiseCreator::lambda(move |result: DialogDbGetDialogsResult| {
                        read_count.store(result.dialogs.len(), Ordering::SeqCst);
                        // every write promise must have fired already: the
                        // read flush settles them before running the query
                        assert_eq!(done.load(Ordering::SeqCst), WRITES);
                    });
                db.get_dialogs(
                    FolderId::new(0),
                    i64::MAX,
                    DialogId::new(i64::MAX),
                    1000,
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
        let sync_safe = StdArc::new(DialogDbSyncSafe::new(safe.clone()));
        let db_slot: StdArc<Mutex<Option<DialogDbAsync>>> = StdArc::new(Mutex::new(None));
        let mut sched = ConcurrentScheduler::new(0);
        // `DialogDbAsync::new` needs a scheduler context; the test wires the
        // actor through `create_actor_unsafe`, the ConcurrentScheduler
        // pattern, instead
        *db_slot.lock().unwrap() = Some(DialogDbAsync {
            impl_: sched.create_actor_unsafe(
                0,
                "DialogDbActor",
                DialogDbAsyncImpl::new(sync_safe.clone()),
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
        let _ = tddb::sqlite_db::destroy(&path);
    }
}
