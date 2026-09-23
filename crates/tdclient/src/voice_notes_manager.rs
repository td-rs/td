use crate::files::{FileId, FileManager};
use crate::store_parse::{Parser, store_flags, store_string};
use crate::td_api;
use std::collections::HashMap;
use std::sync::Arc;

/// The `Td *td` slice of the collapsed VoiceNotesManager.
pub trait VoiceNotesManagerEnv: crate::photo::DialogPhotoStoreEnv {
    fn file_manager(&self) -> Arc<parking_lot::Mutex<FileManager>>;
}

/// `VoiceNotesManager::VoiceNote` (VoiceNotesManager.h:60-68).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VoiceNote {
    pub mime_type: String,
    pub duration: i32,
    pub waveform: String,
    pub file_id: FileId,
}

/// `VoiceNotesManager` (VoiceNotesManager.h:24-84).
#[derive(Default)]
pub struct VoiceNotesManager {
    pub(crate) voice_notes: HashMap<FileId, VoiceNote>,
}

impl VoiceNotesManager {
    pub fn new() -> VoiceNotesManager {
        VoiceNotesManager::default()
    }

    /// `VoiceNotesManager::get_voice_note` (VoiceNotesManager.cpp:77-81).
    pub fn get_voice_note(&self, file_id: FileId) -> Option<&VoiceNote> {
        self.voice_notes.get(&file_id)
    }

    /// `VoiceNotesManager::get_voice_note_duration`
    /// (VoiceNotesManager.cpp:83-87).
    pub fn get_voice_note_duration(&self, file_id: FileId) -> i32 {
        self.get_voice_note(file_id).map_or(0, |note| note.duration)
    }

    /// `VoiceNotesManager::create_voice_note`
    /// (VoiceNotesManager.cpp:132-151).
    pub fn create_voice_note(
        &mut self,
        file_id: FileId,
        mime_type: String,
        duration: i32,
        waveform: String,
        replace: bool,
    ) {
        let note = VoiceNote {
            mime_type,
            duration,
            waveform,
            file_id,
        };
        if replace || !self.voice_notes.contains_key(&file_id) {
            self.voice_notes.insert(file_id, note);
        }
    }

    /// `VoiceNotesManager::merge_voice_notes`
    /// (VoiceNotesManager.cpp:115-130).
    pub fn merge_voice_notes(&mut self, new_id: FileId, old_id: FileId) {
        if new_id == old_id {
            return;
        }
        if let Some(old_note) = self.voice_notes.remove(&old_id) {
            self.voice_notes.entry(new_id).or_insert(old_note);
        }
    }

    /// `VoiceNotesManager::get_voice_note_object`
    /// (VoiceNotesManager.cpp:153-177).
    pub fn get_voice_note_object(
        &self,
        env: &impl VoiceNotesManagerEnv,
        file_id: FileId,
    ) -> Option<td_api::VoiceNote> {
        if !file_id.is_valid() {
            return None;
        }
        let note = self.get_voice_note(file_id)?;
        Some(td_api::VoiceNote {
            duration: note.duration,
            waveform: note.waveform.as_bytes().to_vec(),
            mime_type: note.mime_type.clone(),
            speech_recognition_result: None,
            voice: Some(env.file_manager().lock().get_file_object(file_id)),
        })
    }

    /// `VoiceNotesManager::store_voice_note`
    /// (VoiceNotesManager.hpp:20-43).
    pub fn store_voice_note(
        &self,
        env: &mut impl VoiceNotesManagerEnv,
        file_id: FileId,
        out: &mut Vec<u8>,
    ) {
        let note = self.get_voice_note(file_id).expect("voice note not found");
        let has_mime_type = !note.mime_type.is_empty();
        let has_duration = note.duration != 0;
        let has_waveform = !note.waveform.is_empty();
        // the TranscriptionInfo `is_transcribed` flag plane — the record
        // rides the M.25 transcription seam and is always false here
        store_flags(out, [has_mime_type, has_duration, has_waveform, false]);
        if has_mime_type {
            store_string(out, &note.mime_type);
        }
        if has_duration {
            crate::store_parse::store_i32(out, note.duration);
        }
        if has_waveform {
            store_string(out, &note.waveform);
        }
        env.store_file_id(file_id, out);
    }

    /// `VoiceNotesManager::parse_voice_note`
    /// (VoiceNotesManager.hpp:47-90).
    pub fn parse_voice_note(
        &mut self,
        env: &mut impl VoiceNotesManagerEnv,
        parser: &mut Parser<'_>,
    ) -> Result<FileId, &'static str> {
        let (has_mime_type, has_duration, has_waveform, is_transcribed) =
            if parser.version() >= crate::store_parse::version::ADD_VOICE_NOTE_FLAGS {
                let flags = parser.parse_flags::<4>()?;
                (flags[0], flags[1], flags[2], flags[3])
            } else {
                (true, true, true, false)
            };
        if is_transcribed {
            // the TranscriptionInfo record rides the M.25 transcription
            // seam; this port never writes the flag
            return Err("Transcribed voice notes ride the M.25 transcription seam");
        }
        let mime_type = if has_mime_type {
            parser.string()?
        } else {
            String::new()
        };
        let duration = if has_duration { parser.i32_()? } else { 0 };
        let waveform = if has_waveform {
            parser.string()?
        } else {
            String::new()
        };
        let file_id = env.parse_file_id(parser)?;
        self.voice_notes.insert(
            file_id,
            VoiceNote {
                mime_type,
                duration,
                waveform,
                file_id,
            },
        );
        Ok(file_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::FileManager;
    use std::sync::Arc;

    struct TestEnv {
        files: Arc<parking_lot::Mutex<FileManager>>,
    }

    impl crate::photo::DialogPhotoStoreEnv for TestEnv {
        fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
            self.files.lock().store_file(file_id, out);
        }

        fn parse_file_id(&mut self, parser: &mut Parser<'_>) -> Result<FileId, &'static str> {
            self.files.lock().parse_file(parser)
        }
    }

    impl VoiceNotesManagerEnv for TestEnv {
        fn file_manager(&self) -> Arc<parking_lot::Mutex<FileManager>> {
            Arc::clone(&self.files)
        }
    }

    impl TestEnv {
        /// The store/parse plane round-trips only registered file ids
        /// (the C++ records always carry FileManager-backed ids).
        fn register_file(&self, remote_document_id: i64) -> FileId {
            self.files.lock().register_remote(
                crate::files::FullRemoteFileLocation::from_document(
                    crate::files::FileType::VoiceNote,
                    remote_document_id,
                    42,
                    crate::chat_manager::DcId::internal(2),
                    String::new(),
                ),
                crate::files::FileLocationSource::FromServer,
                crate::message_id::DialogId::default(),
                1000,
                0,
                format!("doc{remote_document_id}.bin"),
            )
        }
    }

    fn env() -> TestEnv {
        TestEnv {
            files: Arc::new(parking_lot::Mutex::new(FileManager::new())),
        }
    }

    #[test]
    fn create_and_merge() {
        let mut manager = VoiceNotesManager::new();
        manager.create_voice_note(
            FileId::new(1, 0),
            "audio/ogg".to_owned(),
            5,
            "wv".to_owned(),
            false,
        );
        assert_eq!(manager.get_voice_note_duration(FileId::new(1, 0)), 5);
        manager.merge_voice_notes(FileId::new(2, 0), FileId::new(1, 0));
        assert_eq!(manager.get_voice_note_duration(FileId::new(2, 0)), 5);
        assert_eq!(manager.get_voice_note_duration(FileId::new(1, 0)), 0);
    }

    #[test]
    fn store_parse_round_trips() {
        let mut manager = VoiceNotesManager::new();
        let mut env = env();
        let file_id = env.register_file(4);
        manager.create_voice_note(file_id, "audio/ogg".to_owned(), 8, "wav".to_owned(), false);
        let mut out = Vec::new();
        manager.store_voice_note(&mut env, file_id, &mut out);

        let mut manager2 = VoiceNotesManager::new();
        let mut parser = Parser::new(&out);
        let parsed_file_id = manager2.parse_voice_note(&mut env, &mut parser).unwrap();
        assert_eq!(parsed_file_id, file_id);
        assert_eq!(
            manager2.get_voice_note(parsed_file_id),
            manager.get_voice_note(file_id)
        );
    }
}
