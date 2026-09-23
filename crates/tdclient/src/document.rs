//! `Document.{h,cpp}` — grown from the NotificationManager shim
//! (M.13.1a): the shim carried only `is_empty()`/`file_id` for the push
//! media lookups; the media-metadata slice (M.28) resolves the intake
//! side and grows the record with the append-only `Type` enum and the
//! constructors `on_get_document` produces. `Document::Type` is
//! append-only by contract (the C++ header comment pins the order);
//! `get_file_ids`/`append_file_ids` (Document.cpp:26-52 — the
//! per-type sibling-manager fan-out) stay with the M.9 MessageContent
//! integration, they need every sibling manager live at once.

use crate::user_manager::FileId;

/// `Document::Type` (Document.h:18) — append-only; the order is the
/// stored int32.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DocumentType {
    #[default]
    Unknown,
    Animation,
    Audio,
    General,
    Sticker,
    Video,
    VideoNote,
    VoiceNote,
}

/// `td::Document` (Document.h:16-36).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Document {
    pub r#type: DocumentType,
    pub file_id: FileId,
}

impl Document {
    /// `Document::Document(Type, FileId)` (Document.h:24-26).
    pub fn new(r#type: DocumentType, file_id: FileId) -> Document {
        Document { r#type, file_id }
    }

    /// `Document::is_empty` (Document.h:28-30) — an empty document has no
    /// type. The M.13.1a shim keyed this on the file id alone (its only
    /// carried field); the C++ keys it on the type.
    pub fn is_empty(&self) -> bool {
        self.r#type == DocumentType::Unknown
    }
}
