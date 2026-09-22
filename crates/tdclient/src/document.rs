//! Placeholder `Document` shim for the NotificationManager slice (M.13.1a).
//!
//! The full `Document.{h,cpp}` port arrives with the MessageContent slice
//! (M.9); this shim carries only what the notification model reads —
//! `is_empty()` and `file_id` (`NotificationType.cpp` uses
//! `document.is_empty()` for file ids and `document.file_id` for the push
//! media lookups). It is replaced by the real port at integration, at which
//! point `NotificationType::get_file_ids` grows the full file-id set.

use crate::user_manager::FileId;

/// `td::Document` (Document.h) — see the module doc: a minimal stand-in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Document {
    pub file_id: FileId,
}

impl Document {
    /// `Document::is_empty` (Document.h) — an empty document has no file id.
    pub fn is_empty(&self) -> bool {
        !self.file_id.is_valid()
    }
}
