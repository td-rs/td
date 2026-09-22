//! Port of the `Document` value record (`Document.h:22-46`) with the
//! `Document.hpp` store/parse framing — the typed file-id payloads ride
//! the [`crate::photo::PhotoStoreEnv`] codec (the M.40 icon-plane
//! encoding; the C++ per-type manager framings
//! `store_animation`/`store_audio`/`store_document`/`store_sticker`/
//! `store_video`/`store_video_note`/`store_voice_note` collapse to the
//! bare FileManager FileId, M.16's documented divergence).
//!
//! First consumer: the NotificationManager push slice (M.13.1a); the full
//! record landed with WebPagesManager (M.17.2). The typed thumbnail
//! planes of `Document::append_file_ids` arrive with the media-metadata
//! managers (M.28).

use crate::files::FileId;
use crate::store_parse::{Parser, store_i32};

/// `Document::Type` (Document.h:24) — append only; the stored i32 order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum DocumentType {
    #[default]
    Unknown = 0,
    Animation,
    Audio,
    General,
    Sticker,
    Video,
    VideoNote,
    VoiceNote,
}

/// `td::Document` (Document.h:25).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Document {
    pub type_: DocumentType,
    pub file_id: FileId,
}

impl Document {
    /// `Document(Type, FileId)` (Document.h:28-30).
    pub fn new(type_: DocumentType, file_id: FileId) -> Document {
        Document { type_, file_id }
    }

    /// `is_empty` (Document.h:32-34).
    pub fn is_empty(&self) -> bool {
        self.type_ == DocumentType::Unknown
    }

    /// `append_file_ids` (Document.cpp) — the per-type thumbnail planes
    /// are the M.28 seam; the typed file id itself appends locally.
    pub fn append_file_ids(&self, file_ids: &mut Vec<FileId>) {
        if self.file_id.is_valid() {
            file_ids.push(self.file_id);
        }
    }

    /// `Document::get_file_id` (Document.hpp) — the main file id.
    pub fn get_file_id(&self) -> FileId {
        self.file_id
    }

    /// `store(Document)` (Document.hpp:27-57) — the type discriminant,
    /// then the (collapsed) typed file-id framing.
    pub fn store(&self, env: &mut impl crate::photo::PhotoStoreEnv, out: &mut Vec<u8>) {
        store_i32(out, self.type_ as i32);
        env.store_file_id(self.file_id, out);
    }

    /// `parse(Document)` (Document.hpp:59-100).
    pub fn parse(
        env: &mut impl crate::photo::PhotoStoreEnv,
        parser: &mut Parser<'_>,
    ) -> Result<Document, &'static str> {
        let type_ = parser.i32_()?;
        let type_ = match type_ {
            0 => DocumentType::Unknown,
            1 => DocumentType::Animation,
            2 => DocumentType::Audio,
            3 => DocumentType::General,
            4 => DocumentType::Sticker,
            5 => DocumentType::Video,
            6 => DocumentType::VideoNote,
            7 => DocumentType::VoiceNote,
            _ => return Err("Unknown Document type"),
        };
        let file_id = env.parse_file_id(parser)?;
        Ok(Document { type_, file_id })
    }
}
