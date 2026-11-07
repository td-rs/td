//! The `MessageImportManager` port (MessageImportManager.{h,cpp}) — the
//! history-import eligibility checks, the confirmation text and the
//! import intake over the input-file id seam (M.26). The two head
//! queries (`CheckHistoryImportQuery`/`CheckHistoryImportPeerQuery` —
//! MessageImportManager.cpp:30-98) dispatch through the
//! `MessageImportNetKind` plane over the Td-held
//! `crate::td::MessageImportNetHandle` (the net-drain 58 families; the
//! answers drain through `Td::on_message_import_net_answer`); the
//! upload tails (the `InitHistoryImportQuery` chain) stay `[blocked:
//! net/Files]` — they resolve the uploaded InputFile through the
//! FileManager upload chain (cpp:119-150).

use crate::chat_manager::ChannelId;
use crate::dialog_id::{DialogId, DialogType};
use crate::files::FileId;
use crate::td_api;
use crate::user_manager::UserId;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// The `Td *td` slice of the collapsed MessageImportManager.
pub trait MessageImportManagerEnv {
    fn close_flag(&self) -> bool;

    /// `dialog_manager_->check_dialog_access(dialog_id, false,
    /// AccessRights::Write, source)`.
    fn check_dialog_access(&mut self, dialog_id: DialogId, source: &str) -> Result<(), Status>;

    /// `user_manager_->is_user_contact(user_id, true)`.
    fn is_user_contact(&self, user_id: UserId) -> bool;

    /// `dialog_manager_->is_broadcast_channel(dialog_id)`.
    fn is_broadcast_channel(&self, dialog_id: DialogId) -> bool;

    /// `chat_manager_->get_channel_permissions(channel_id)
    /// .can_change_info_and_settings()`.
    fn can_change_info_and_settings(&self, channel_id: ChannelId) -> bool;

    /// `file_manager_->get_input_file_id(FileType::Document,
    /// message_file, dialog_id, false, false)` — the [blocked: Files]
    /// input-file id seam.
    fn get_input_file_id(
        &mut self,
        message_file: &td_api::InputFile,
        dialog_id: DialogId,
    ) -> Result<FileId, Status>;

    /// `CheckHistoryImportQuery` (MessageImportManager.cpp:30-64) — the
    /// request promise has already parked in the family lot by the
    /// caller (the C++ query carries it on `promise_`, cpp:33-36).
    fn send_check_history_import_query(&mut self, message_file_head: String);

    /// `CheckHistoryImportPeerQuery` (MessageImportManager.cpp:67-98) —
    /// the request promise has already parked in the family lot by the
    /// caller (the C++ query carries it on `promise_`, cpp:72-74); the
    /// write peer resolves at this send leg (cpp:75-81).
    fn send_check_history_import_peer_query(&mut self, dialog_id: DialogId);

    /// `InitHistoryImportQuery` → `upload_imported_messages` — the
    /// [blocked: net/Files] upload tail; the promise parks (the C++ send
    /// resolves the uploaded InputFile through the FileManager upload
    /// chain, MessageImportManager.cpp:368-386 — un-gates with the Files
    /// plane).
    fn start_history_import(
        &mut self,
        dialog_id: DialogId,
        file_id: FileId,
        attached_files: Vec<td_api::InputFile>,
        promise: Promise<()>,
    );
}

/// The message-import plane in-flight query kinds — the C++
/// `Td::ResultHandler` query classes the manager's two head queries
/// create (MessageImportManager.cpp:30-98). The completions park by
/// kind behind the Td-held `crate::td::MessageImportNetHandle`;
/// `Td::on_message_import_net_answer` routes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageImportNetKind {
    /// `CheckHistoryImportQuery` (cpp:30-64) —
    /// `messages.checkHistoryImport`; the request promise parks in the
    /// manager's `check_history_import` FIFO lot (the C++ query carries
    /// it on `promise_`; §5's FIFO-per-family shape).
    CheckHistoryImport,
    /// `CheckHistoryImportPeerQuery` (cpp:67-98) —
    /// `messages.checkHistoryImportPeer`; the kind carries the
    /// handler's `dialog_id_` for the `on_get_dialog_error` head the
    /// C++ `on_error` runs (cpp:94), the request promise parks in the
    /// `check_history_import_peer` FIFO lot.
    CheckHistoryImportPeer { dialog_id: DialogId },
}

/// The parked head-query promises — the ResultHandler members the
/// `Copy` [`MessageImportNetKind`]s can't carry. FIFO-paired per family
/// (§5; the C++ carries each promise on its query).
#[derive(Default)]
pub(crate) struct PendingMessageImportQueries {
    /// `CheckHistoryImportQuery::promise_` (cpp:33).
    pub(crate) check_history_import: Vec<Promise<td_api::MessageFileType>>,
    /// `CheckHistoryImportPeerQuery::promise_` (cpp:69).
    pub(crate) check_history_import_peer: Vec<Promise<String>>,
}

/// `td::MessageImportManager` — stateless offline (every in-flight piece
/// rides the upload/net machinery of the seams).
#[derive(Default)]
pub struct MessageImportManager {
    /// The parked head-query promises (the
    /// [`PendingMessageImportQueries`] lots).
    pub(crate) pending: PendingMessageImportQueries,
}

impl MessageImportManager {
    pub fn new() -> MessageImportManager {
        MessageImportManager::default()
    }

    /// The check-history-import promise pop for the
    /// `Td::on_message_import_net_answer` drain — the FIFO head the
    /// answer pairs with (the C++ promise rides the query).
    pub(crate) fn pop_check_history_import_promise(
        &mut self,
    ) -> Option<Promise<td_api::MessageFileType>> {
        pop_head(&mut self.pending.check_history_import)
    }

    /// `MessageImportManager::get_message_file_type`
    /// (MessageImportManager.cpp:277-280) — the wire check: the promise
    /// parks in the family lot, the env send leg dispatches the
    /// `messages.checkHistoryImport` query (cpp:38-40).
    pub fn get_message_file_type(
        &mut self,
        env: &mut impl MessageImportManagerEnv,
        message_file_head: &str,
        promise: Promise<td_api::MessageFileType>,
    ) {
        self.pending.check_history_import.push(promise);
        env.send_check_history_import_query(message_file_head.to_owned());
    }

    /// `MessageImportManager::can_import_messages`
    /// (MessageImportManager.cpp:282-308) — the full local port.
    pub fn can_import_messages(
        &mut self,
        env: &mut impl MessageImportManagerEnv,
        dialog_id: DialogId,
    ) -> Result<(), Status> {
        env.check_dialog_access(dialog_id, "can_import_messages")?;

        match dialog_id.get_type() {
            DialogType::User => {
                if !env.is_user_contact(dialog_id.get_user_id()) {
                    return Err(Status::error(400, "User must be a mutual contact"));
                }
            }
            DialogType::Chat => {
                return Err(Status::error(
                    400,
                    "Basic groups must be upgraded to supergroups first",
                ));
            }
            DialogType::Channel => {
                if env.is_broadcast_channel(dialog_id) {
                    return Err(Status::error(400, "Can't import messages to channels"));
                }
                if !env.can_change_info_and_settings(dialog_id.get_channel_id()) {
                    return Err(Status::error(400, "Not enough rights to import messages"));
                }
            }
            DialogType::SecretChat | DialogType::None => {
                return Err(Status::error(400, "Chat not found"));
            }
        }
        Ok(())
    }

    /// The check-history-import-promise pop for the
    /// `Td::on_message_import_net_answer` drain — the FIFO head the
    /// answer pairs with (the C++ promise rides the query).
    pub(crate) fn pop_check_history_import_peer_promise(&mut self) -> Option<Promise<String>> {
        pop_head(&mut self.pending.check_history_import_peer)
    }

    /// `MessageImportManager::get_message_import_confirmation_text`
    /// (MessageImportManager.cpp:310-314) — the can_import_messages
    /// bands first, then the promise parks in the family lot and the
    /// env send leg resolves the write peer (cpp:75-81).
    pub fn get_message_import_confirmation_text(
        &mut self,
        env: &mut impl MessageImportManagerEnv,
        dialog_id: DialogId,
        mut promise: Promise<String>,
    ) {
        if let Err(status) = self.can_import_messages(env, dialog_id) {
            return promise.set_error(status);
        }
        self.pending.check_history_import_peer.push(promise);
        env.send_check_history_import_peer_query(dialog_id);
    }

    /// `MessageImportManager::import_messages`
    /// (MessageImportManager.cpp:316-342) — the can_import_messages port
    /// plus the input-file id half; the attached-file filter + the upload
    /// tail ride the env seam.
    pub fn import_messages(
        &mut self,
        env: &mut impl MessageImportManagerEnv,
        dialog_id: DialogId,
        message_file: Option<td_api::InputFile>,
        attached_files: Vec<td_api::InputFile>,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = self.can_import_messages(env, dialog_id) {
            return promise.set_error(status);
        }
        let Some(message_file) = message_file else {
            return promise.set_error(Status::error(400, "Message file must be non-empty"));
        };
        let file_id = match env.get_input_file_id(&message_file, dialog_id) {
            Ok(file_id) => file_id,
            Err(status) => return promise.set_error(status),
        };
        env.start_history_import(dialog_id, file_id, attached_files, promise);
    }
}

/// Pop the oldest parked promise — the §5 FIFO-per-family convention
/// (the C++ promise rides its own ResultHandler; the net completes the
/// same-family queries in dispatch order, so the head pairs).
fn pop_head<T: Send>(queue: &mut Vec<Promise<T>>) -> Option<Promise<T>> {
    if queue.is_empty() {
        None
    } else {
        Some(queue.remove(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_manager::ChannelId;
    use crate::files::FileId;
    use std::cell::RefCell;
    use std::collections::HashSet;

    struct TestEnv {
        contacts: HashSet<i64>,
        broadcast_channels: HashSet<i64>,
        can_change: HashSet<i64>,
        denied_dialogs: HashSet<i64>,
        checked_access: RefCell<Vec<DialogId>>,
        imported: RefCell<Vec<(DialogId, FileId)>>,
        /// the recorded `messages.checkHistoryImport` sends (the head
        /// strings — the offline stand-in for the net plane)
        sent_import_heads: RefCell<Vec<String>>,
        /// the recorded `messages.checkHistoryImportPeer` sends (the
        /// dialog ids — the offline stand-in for the net plane)
        sent_import_peer_dialogs: RefCell<Vec<DialogId>>,
    }

    impl MessageImportManagerEnv for TestEnv {
        fn close_flag(&self) -> bool {
            false
        }

        fn check_dialog_access(
            &mut self,
            dialog_id: DialogId,
            _source: &str,
        ) -> Result<(), Status> {
            self.checked_access.borrow_mut().push(dialog_id);
            if self.denied_dialogs.contains(&dialog_id.get()) {
                return Err(Status::error(400, "Chat not found"));
            }
            Ok(())
        }

        fn is_user_contact(&self, user_id: UserId) -> bool {
            self.contacts.contains(&user_id.get())
        }

        fn is_broadcast_channel(&self, dialog_id: DialogId) -> bool {
            self.broadcast_channels.contains(&dialog_id.get())
        }

        fn can_change_info_and_settings(&self, channel_id: ChannelId) -> bool {
            self.can_change.contains(&channel_id.get())
        }

        fn get_input_file_id(
            &mut self,
            _message_file: &td_api::InputFile,
            _dialog_id: DialogId,
        ) -> Result<FileId, Status> {
            Ok(FileId::new(7, 0))
        }

        fn send_check_history_import_query(&mut self, message_file_head: String) {
            self.sent_import_heads.borrow_mut().push(message_file_head);
        }

        fn send_check_history_import_peer_query(&mut self, dialog_id: DialogId) {
            self.sent_import_peer_dialogs.borrow_mut().push(dialog_id);
        }

        fn start_history_import(
            &mut self,
            dialog_id: DialogId,
            file_id: FileId,
            _attached_files: Vec<td_api::InputFile>,
            _promise: Promise<()>,
        ) {
            self.imported.borrow_mut().push((dialog_id, file_id));
        }
    }

    fn input_file() -> td_api::InputFile {
        td_api::InputFile::inputFileLocal(td_api::inputFileLocal {
            path: "/tmp/x.json".to_owned(),
        })
    }

    /// The recorded promise outcome (the `record_unit` shape — the
    /// td_api enums have no Default).
    type OutcomeSlot<T> = std::sync::Arc<std::sync::Mutex<Option<tdutils::status::Result<T>>>>;
    fn outcome_promise<T: Send + 'static>() -> (Promise<T>, OutcomeSlot<T>) {
        struct Rec<T>(OutcomeSlot<T>);
        impl<T: Send + 'static> tdutils::promise::PromiseInterface<T> for Rec<T> {
            fn set_value(&mut self, value: T) {
                *self.0.lock().unwrap() = Some(tdutils::status::Result::Ok(value));
            }
            fn set_error(&mut self, error: Status) {
                *self.0.lock().unwrap() = Some(tdutils::status::Result::Err(error));
            }
        }
        let slot: OutcomeSlot<T> = std::sync::Arc::new(std::sync::Mutex::new(None));
        (Promise::from_interface(Rec(slot.clone())), slot)
    }

    fn empty_env() -> TestEnv {
        TestEnv {
            contacts: HashSet::new(),
            broadcast_channels: HashSet::new(),
            can_change: HashSet::new(),
            denied_dialogs: HashSet::new(),
            checked_access: RefCell::new(Vec::new()),
            imported: RefCell::new(Vec::new()),
            sent_import_heads: RefCell::new(Vec::new()),
            sent_import_peer_dialogs: RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn check_history_import_parks_the_promise_and_sends_the_head() {
        // `CheckHistoryImportQuery` carries the request promise on the
        // query (`promise_`, MessageImportManager.cpp:33-36) — the collapse
        // parks it in the family lot before the env send, so the answer
        // drain pairs with the head (§5's FIFO-per-family shape).
        let mut env = empty_env();
        let mut manager = MessageImportManager::new();
        let (promise, slot) = outcome_promise::<td_api::MessageFileType>();
        manager.get_message_file_type(&mut env, "{\"chat_title\":1}", promise);
        assert_eq!(
            env.sent_import_heads.borrow().clone(),
            vec!["{\"chat_title\":1}".to_owned()],
            "the head string is the whole wire form (cpp:38-40)"
        );
        assert!(slot.lock().unwrap().is_none(), "the promise parks");

        let mut head = manager
            .pop_check_history_import_promise()
            .expect("the parked promise");
        head.set_value(td_api::MessageFileType::messageFileTypeUnknown(
            td_api::messageFileTypeUnknown,
        ));
        assert!(matches!(
            slot.lock().unwrap().take(),
            Some(tdutils::status::Result::Ok(_))
        ));
        assert!(
            manager.pop_check_history_import_promise().is_none(),
            "the lot drained"
        );
    }

    #[test]
    fn check_history_import_promise_lot_is_fifo() {
        // Two parks pair with two answers in order, and a bare answer
        // (no park) settles nothing (§5's FIFO-per-family shape).
        let mut env = empty_env();
        let mut manager = MessageImportManager::new();
        assert!(manager.pop_check_history_import_promise().is_none());

        let (first, first_slot) = outcome_promise::<td_api::MessageFileType>();
        manager.get_message_file_type(&mut env, "one", first);
        let (second, second_slot) = outcome_promise::<td_api::MessageFileType>();
        manager.get_message_file_type(&mut env, "two", second);
        assert_eq!(
            env.sent_import_heads.borrow().clone(),
            vec!["one".to_owned(), "two".to_owned()]
        );

        let mut head = manager.pop_check_history_import_promise().unwrap();
        head.set_error(Status::error(420, "FLOOD_WAIT_9"));
        assert!(matches!(
            first_slot.lock().unwrap().take(),
            Some(tdutils::status::Result::Err(status))
                if (status.code(), status.message()) == (420, "FLOOD_WAIT_9")
        ));
        assert!(second_slot.lock().unwrap().is_none(), "FIFO: only the head");

        let mut head = manager.pop_check_history_import_promise().unwrap();
        head.set_value(td_api::MessageFileType::messageFileTypeGroup(
            td_api::messageFileTypeGroup {
                title: "T".to_owned(),
            },
        ));
        assert!(matches!(
            second_slot.lock().unwrap().take(),
            Some(tdutils::status::Result::Ok(
                td_api::MessageFileType::messageFileTypeGroup(_)
            ))
        ));
        assert!(
            manager.pop_check_history_import_promise().is_none(),
            "the lot drained"
        );
    }

    #[test]
    fn check_history_import_peer_parks_after_the_access_bands() {
        // `get_message_import_confirmation_text`
        // (MessageImportManager.cpp:310-314) — the can_import_messages
        // bands fail the promise before any query (cpp:312), then the
        // promise parks in the family lot and the send leg carries the
        // dialog (the write peer resolves there, cpp:75-81).
        let mut env = empty_env();
        env.contacts.insert(42);
        let contact_dialog = DialogId::from_user_id(UserId::new(42));
        let stranger_dialog = DialogId::from_user_id(UserId::new(43));
        let mut manager = MessageImportManager::new();

        // the access bands fail before the send (no seam call)
        let (promise, slot) = outcome_promise::<String>();
        manager.get_message_import_confirmation_text(&mut env, stranger_dialog, promise);
        assert!(matches!(
            slot.lock().unwrap().take(),
            Some(tdutils::status::Result::Err(status))
                if (status.code(), status.message()) == (400, "User must be a mutual contact")
        ));
        assert!(
            env.sent_import_peer_dialogs.borrow().is_empty(),
            "the failed band never sends"
        );

        // the accessible dialog parks the promise and sends the dialog
        let (first, first_slot) = outcome_promise::<String>();
        manager.get_message_import_confirmation_text(&mut env, contact_dialog, first);
        let (second, second_slot) = outcome_promise::<String>();
        manager.get_message_import_confirmation_text(&mut env, contact_dialog, second);
        assert_eq!(
            env.sent_import_peer_dialogs.borrow().clone(),
            vec![contact_dialog, contact_dialog]
        );
        assert!(first_slot.lock().unwrap().is_none(), "the promises park");

        // FIFO: the answers pair with the head
        let mut head = manager
            .pop_check_history_import_peer_promise()
            .expect("the first park");
        head.set_value("Import this chat?".to_owned());
        assert!(matches!(
            first_slot.lock().unwrap().take(),
            Some(tdutils::status::Result::Ok(text)) if text == "Import this chat?"
        ));
        assert!(second_slot.lock().unwrap().is_none(), "FIFO: only the head");

        let mut head = manager
            .pop_check_history_import_peer_promise()
            .expect("the second park");
        head.set_error(Status::error(420, "FLOOD_WAIT_9"));
        assert!(matches!(
            second_slot.lock().unwrap().take(),
            Some(tdutils::status::Result::Err(status))
                if (status.code(), status.message()) == (420, "FLOOD_WAIT_9")
        ));
        assert!(
            manager.pop_check_history_import_peer_promise().is_none(),
            "the lot drained"
        );
    }

    #[test]
    fn user_must_be_a_mutual_contact() {
        let mut env = TestEnv {
            contacts: HashSet::new(),
            broadcast_channels: HashSet::new(),
            can_change: HashSet::new(),
            denied_dialogs: HashSet::new(),
            checked_access: RefCell::new(Vec::new()),
            imported: RefCell::new(Vec::new()),
            sent_import_heads: RefCell::new(Vec::new()),
            sent_import_peer_dialogs: RefCell::new(Vec::new()),
        };
        let mut manager = MessageImportManager::new();
        let dialog_id = DialogId::from_user_id(UserId::new(42));
        let status = manager
            .can_import_messages(&mut env, dialog_id)
            .unwrap_err();
        assert_eq!(status.code(), 400);
        assert!(status.message().contains("mutual contact"));

        env.contacts.insert(42);
        assert!(manager.can_import_messages(&mut env, dialog_id).is_ok());
    }

    #[test]
    fn basic_groups_are_rejected() {
        let mut env = TestEnv {
            contacts: HashSet::new(),
            broadcast_channels: HashSet::new(),
            can_change: HashSet::new(),
            denied_dialogs: HashSet::new(),
            checked_access: RefCell::new(Vec::new()),
            imported: RefCell::new(Vec::new()),
            sent_import_heads: RefCell::new(Vec::new()),
            sent_import_peer_dialogs: RefCell::new(Vec::new()),
        };
        let mut manager = MessageImportManager::new();
        let dialog_id = DialogId::from_chat_id(crate::chat_manager::ChatId::new(5));
        let status = manager
            .can_import_messages(&mut env, dialog_id)
            .unwrap_err();
        assert!(status.message().contains("upgraded"));
    }

    #[test]
    fn channels_need_rights_and_must_not_broadcast() {
        let mut env = TestEnv {
            contacts: HashSet::new(),
            broadcast_channels: HashSet::new(),
            can_change: HashSet::new(),
            denied_dialogs: HashSet::new(),
            checked_access: RefCell::new(Vec::new()),
            imported: RefCell::new(Vec::new()),
            sent_import_heads: RefCell::new(Vec::new()),
            sent_import_peer_dialogs: RefCell::new(Vec::new()),
        };
        let mut manager = MessageImportManager::new();
        let dialog_id = DialogId::from_channel_id(ChannelId::new(10));
        let status = manager
            .can_import_messages(&mut env, dialog_id)
            .unwrap_err();
        assert!(status.message().contains("Not enough rights"));

        env.can_change.insert(10);
        // the broadcast probe takes the dialog id (the C++
        // dialog_manager_->is_broadcast_channel(dialog_id))
        let broadcast_dialog = dialog_id.get();
        env.broadcast_channels.insert(broadcast_dialog);
        let status = manager
            .can_import_messages(&mut env, dialog_id)
            .unwrap_err();
        assert!(status.message().contains("channels"));

        env.broadcast_channels.remove(&broadcast_dialog);
        assert!(manager.can_import_messages(&mut env, dialog_id).is_ok());
    }

    #[test]
    fn import_routes_through_the_input_file_seam() {
        let mut env = TestEnv {
            contacts: HashSet::from([42]),
            broadcast_channels: HashSet::new(),
            can_change: HashSet::new(),
            denied_dialogs: HashSet::new(),
            checked_access: RefCell::new(Vec::new()),
            imported: RefCell::new(Vec::new()),
            sent_import_heads: RefCell::new(Vec::new()),
            sent_import_peer_dialogs: RefCell::new(Vec::new()),
        };
        let mut manager = MessageImportManager::new();
        let dialog_id = DialogId::from_user_id(UserId::new(42));
        let (promise, fulfilled, failed, _) = flag_promise::<()>();
        manager.import_messages(&mut env, dialog_id, Some(input_file()), Vec::new(), promise);
        assert!(!fulfilled.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!failed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(env.imported.borrow().len(), 1);
        assert_eq!(env.imported.borrow()[0].0, dialog_id);

        // a denied dialog fails before the file seam
        env.denied_dialogs.insert(dialog_id.get());
        let (promise, _, failed, _) = flag_promise::<()>();
        manager.import_messages(&mut env, dialog_id, Some(input_file()), Vec::new(), promise);
        assert!(failed.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// A minimal promise + flag factory (the tdutils Promise has no
    /// introspection surface).
    type FlagPromiseViews<T> = (
        Promise<T>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<parking_lot::Mutex<Option<T>>>,
    );
    fn flag_promise<T: Send + Default + 'static>() -> FlagPromiseViews<T> {
        struct FlagPromise<T> {
            value_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
            error_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
            value: std::sync::Arc<parking_lot::Mutex<Option<T>>>,
        }
        impl<T: Send + 'static> tdutils::promise::PromiseInterface<T> for FlagPromise<T> {
            fn set_value(&mut self, value: T) {
                *self.value.lock() = Some(value);
                self.value_flag
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            fn set_error(&mut self, _error: Status) {
                self.error_flag
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let value_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let error_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let value = std::sync::Arc::new(parking_lot::Mutex::new(None));
        (
            Promise::from_interface(FlagPromise::<T> {
                value_flag: value_flag.clone(),
                error_flag: error_flag.clone(),
                value: value.clone(),
            }),
            value_flag,
            error_flag,
            value,
        )
    }
}
