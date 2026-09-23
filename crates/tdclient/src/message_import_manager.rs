//! The `MessageImportManager` port (MessageImportManager.{h,cpp}) — the
//! history-import eligibility checks, the confirmation text and the
//! import intake over the input-file id seam (M.26).

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

    /// `CheckHistoryImportQuery` — [blocked: net]; the promise parks.
    fn send_check_history_import_query(
        &mut self,
        message_file_head: String,
        promise: Promise<td_api::MessageFileType>,
    );

    /// `CheckHistoryImportPeerQuery` — [blocked: net].
    fn send_check_history_import_peer_query(
        &mut self,
        dialog_id: DialogId,
        promise: Promise<String>,
    );

    /// `InitHistoryImportQuery` → `upload_imported_messages` — the
    /// [blocked: Files upload] + [blocked: net] tail; the promise parks.
    fn start_history_import(
        &mut self,
        dialog_id: DialogId,
        file_id: FileId,
        attached_files: Vec<td_api::InputFile>,
        promise: Promise<()>,
    );
}

/// `td::MessageImportManager` — stateless offline (every in-flight piece
/// rides the upload/net machinery of the seams).
#[derive(Default)]
pub struct MessageImportManager {}

impl MessageImportManager {
    pub fn new() -> MessageImportManager {
        MessageImportManager::default()
    }

    /// `MessageImportManager::get_message_file_type`
    /// (MessageImportManager.cpp:277-280) — the wire check; the query is
    /// [blocked: net].
    pub fn get_message_file_type(
        &mut self,
        env: &mut impl MessageImportManagerEnv,
        message_file_head: &str,
        promise: Promise<td_api::MessageFileType>,
    ) {
        env.send_check_history_import_query(message_file_head.to_owned(), promise);
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

    /// `MessageImportManager::get_message_import_confirmation_text`
    /// (MessageImportManager.cpp:310-314).
    pub fn get_message_import_confirmation_text(
        &mut self,
        env: &mut impl MessageImportManagerEnv,
        dialog_id: DialogId,
        mut promise: Promise<String>,
    ) {
        if let Err(status) = self.can_import_messages(env, dialog_id) {
            return promise.set_error(status);
        }
        env.send_check_history_import_peer_query(dialog_id, promise);
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

        fn send_check_history_import_query(
            &mut self,
            _message_file_head: String,
            _promise: Promise<td_api::MessageFileType>,
        ) {
        }

        fn send_check_history_import_peer_query(
            &mut self,
            _dialog_id: DialogId,
            _promise: Promise<String>,
        ) {
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

    #[test]
    fn user_must_be_a_mutual_contact() {
        let mut env = TestEnv {
            contacts: HashSet::new(),
            broadcast_channels: HashSet::new(),
            can_change: HashSet::new(),
            denied_dialogs: HashSet::new(),
            checked_access: RefCell::new(Vec::new()),
            imported: RefCell::new(Vec::new()),
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
