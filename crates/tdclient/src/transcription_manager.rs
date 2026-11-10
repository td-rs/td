//! Port of `td/telegram/TranscriptionManager.{h,cpp}` (M.25) plus the
//! model files that only serve it: `TranscriptionInfo.{h,cpp,hpp}`.
//!
//! The manager is the collapsed actor: `Td` owns the struct, the
//! [`TranscriptionManagerEnv`] trait carries the `G()`/auth/pmc/
//! cross-manager surfaces, and the two `create_handler<...>` network
//! tails (`TranscribeAudioQuery`/`RateTranscribedAudioQuery` —
//! TranscriptionInfo.cpp:21-91) dispatch through the
//! `TranscriptionNetKind` plane over the Td-held
//! `crate::td::TranscriptionNetHandle` (the net-drain 47 families; the
//! answers drain through `Td::on_transcription_net_answer`).
//!
//! Landed here: the trial-parameters state machine (moved in from
//! `manager_option_states`, the M.3.9.2 stand-in — the pmc codec is the
//! same bytes), the weekly-reset timeout plane the stand-in skipped, the
//! `transcription_id → FileInfo` subscription map with the 60-second
//! expiry (`MultiTimeout` → the deadline table + [`TranscriptionManager::
//! tick_timeouts`]), the voice-message registration maps, the
//! partial-results update machine (`TranscriptionInfo`), and the
//! `speechRecognitionResult`/`updateSpeechRecognitionTrial` builders.
//!
//! Env seams (the C++ callers that do not exist yet):
//! - `MessagesManager::have_message_force` /
//!   `on_external_update_message_content` / `on_update_message_content`
//!   — [blocked: M.10]; the collapsed probe answers `false`, so the
//!   C++ "Message not found" band is what `recognizeSpeech`/
//!   `rateSpeechRecognition` return offline.
//! - `VoiceNotesManager::get_voice_note_transcription_info` /
//!   `VideoNotesManager::get_video_note_transcription_info` —
//!   [blocked: M.28]; the per-content `TranscriptionInfo` storage. The
//!   type itself (state machine, builders, message-db store/parse) is
//!   landed and unit-tested here.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::dialog_id::{DialogId, DialogType, MessageFullId};
use crate::files::{FileId, FileType, FileUploadId};
use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `TranscriptionManager::AUDIO_TRANSCRIPTION_TIMEOUT`
/// (TranscriptionManager.h:61) — seconds a pending transcription id is
/// subscribed for.
const AUDIO_TRANSCRIPTION_TIMEOUT: f64 = 60.0;

/// The `MessageContentType` slice the manager distinguishes
/// (TranscriptionManager.cpp:252-262, the VideoNotesManager vs
/// VoiceNotesManager routing); the full enum lands with the message
/// content plane (M.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptionKind {
    VideoNote,
    VoiceNote,
}

/// `TranscriptionManager::FileInfo` (TranscriptionManager.h:104) — the
/// `std::pair<MessageContentType, FileId>` with the pair's first/second
/// named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub kind: TranscriptionKind,
    pub file_id: FileId,
}

/// The transcription-plane in-flight query kinds — the C++
/// `Td::ResultHandler` query classes the manager's two network tails
/// create (`TranscriptionInfo.cpp:21-91`). The completions park by kind
/// behind the Td-held `crate::td::TranscriptionNetHandle`;
/// `Td::on_transcription_net_answer` routes them. Both kinds carry the
/// query's `dialog_id_` for the `on_get_dialog_error` head the C++
/// `on_error`s run (cpp:52/:88).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptionNetKind {
    /// `TranscribeAudioQuery` (TranscriptionInfo.cpp:21-55) —
    /// `messages.transcribeAudio`; the kind carries the `FileInfo` the
    /// C++ handler closure captures over the actor hop (cpp:275-278),
    /// so the answer re-enters `on_transcribed_audio` for the same
    /// voice/video note.
    TranscribeAudio {
        dialog_id: DialogId,
        file_info: FileInfo,
    },
    /// `RateTranscribedAudioQuery` (TranscriptionInfo.cpp:57-91) —
    /// `messages.rateTranscribedAudio`; the request promise parks in the
    /// manager's `pending_rate_promises` FIFO lot (the C++ promise rides
    /// the query's `promise_` member; §5's FIFO-per-family shape).
    RateTranscribedAudio { dialog_id: DialogId },
    /// `TranscribeAudioFileQuery` (TranscriptionManager.cpp:71-100) —
    /// `messages.transcribeAudioFile`; the request promise parks in the
    /// manager's `audio_file_transcriptions` map keyed by the upload
    /// pair, so the kind carries the pair the C++ handler lambda closes
    /// over (cpp:532-535) and the answer re-enters
    /// `on_transcribed_audio_file`.
    TranscribeAudioFile { file_upload_id: FileUploadId },
    /// `GetTranscriptionLangsQuery` (TranscriptionManager.cpp:32-69) —
    /// `messages.getTranscriptionLangs`; the request promise parks in
    /// the manager's `pending_langs_promises` FIFO lot (the C++ promise
    /// rides the query's `promise_` member; §5). The kind carries the
    /// request's `dialog_id` for the `on_get_dialog_error` head the C++
    /// `on_error` runs (cpp:63-68).
    GetTranscriptionLangs { dialog_id: DialogId },
}

/// `TranscriptionManager::AudioFileTranscription`
/// (TranscriptionManager.h:162-165) — the in-flight
/// `recognizeSpeechInFile` park, keyed by the upload pair.
struct AudioFileTranscription {
    /// The request's `recognition_id` — nonzero requests get the
    /// partial-text `updateSpeechRecognitionInFile` stream.
    recognition_id: i64,
    /// The request promise; the final text arrives as the answer.
    promise: Promise<td_api::Text>,
}

/// The transcription-id subscription entries — the C++ handler closures'
/// only free variable, tagged by which continuation the answer drives
/// (the message-content machine vs the file-transcription finish).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingTranscription {
    /// The message-content continuation — the `FileInfo` the
    /// `on_transcribed_audio_update` lambda captures
    /// (TranscriptionManager.cpp:433-439).
    Message(FileInfo),
    /// The file-transcription continuation — the `FileUploadId` the
    /// `on_transcribed_audio_file_update` lambda captures
    /// (TranscriptionManager.cpp:564-569).
    File(FileUploadId),
}

/// `TranscriptionInfo` (TranscriptionInfo.h:23-31) — the per-message-
/// content speech-recognition state. The C++ type lives inside the
/// VoiceNotes/VideoNotes managers' message content records; the storage
/// is the M.28 seam, the machine is here. The parked promises make it
/// non-Clone (like the C++ deleted copy).
#[derive(Default)]
pub struct TranscriptionInfo {
    is_transcribed: bool,
    transcription_id: i64,
    text: String,

    // temporary state
    /// `Status last_transcription_error_` — `None` is `Status::OK()`.
    last_transcription_error: Option<Status>,
    speech_recognition_queries: Vec<Promise<()>>,
}

impl TranscriptionInfo {
    /// `TranscriptionInfo::is_transcribed` (TranscriptionInfo.h:33-35).
    pub fn is_transcribed(&self) -> bool {
        self.is_transcribed
    }

    /// `TranscriptionInfo::recognize_speech` (TranscriptionInfo.cpp:93-107)
    /// — the promise parks until a final transcription; the return value
    /// is the C++ "a new query was sent" flag (the caller sends the
    /// `TranscribeAudioQuery` and refreshes the message content).
    pub fn recognize_speech(&mut self, promise: Promise<()>) -> bool {
        if self.is_transcribed {
            let mut promise = promise;
            promise.set_value(());
            return false;
        }
        self.speech_recognition_queries.push(promise);
        if self.speech_recognition_queries.len() == 1 {
            self.last_transcription_error = None;
            return true;
        }
        false
    }

    /// `TranscriptionInfo::on_final_transcription`
    /// (TranscriptionInfo.cpp:109-123) — the parked promises complete
    /// with ok (the text arrives through the message-content updates).
    pub fn on_final_transcription(
        &mut self,
        text: String,
        transcription_id: i64,
    ) -> Vec<Promise<()>> {
        assert!(!self.is_transcribed, "CHECK(!is_transcribed_)");
        assert!(
            self.transcription_id == 0 || self.transcription_id == transcription_id,
            "CHECK(transcription_id_ == 0 || transcription_id_ == transcription_id)"
        );
        assert!(transcription_id != 0, "CHECK(transcription_id != 0)");
        self.transcription_id = transcription_id;
        self.is_transcribed = true;
        self.text = text;
        self.last_transcription_error = None;

        assert!(
            !self.speech_recognition_queries.is_empty(),
            "CHECK(!speech_recognition_queries_.empty())"
        );
        std::mem::take(&mut self.speech_recognition_queries)
    }

    /// `TranscriptionInfo::on_partial_transcription`
    /// (TranscriptionInfo.cpp:125-135) — returns whether the text
    /// changed.
    pub fn on_partial_transcription(&mut self, text: String, transcription_id: i64) -> bool {
        assert!(!self.is_transcribed, "CHECK(!is_transcribed_)");
        assert!(
            self.transcription_id == 0 || self.transcription_id == transcription_id,
            "CHECK(transcription_id_ == 0 || transcription_id_ == transcription_id)"
        );
        assert!(transcription_id != 0, "CHECK(transcription_id != 0)");
        let is_changed = self.text != text;
        self.transcription_id = transcription_id;
        self.text = text;
        self.last_transcription_error = None;

        is_changed
    }

    /// `TranscriptionInfo::on_failed_transcription`
    /// (TranscriptionInfo.cpp:137-147).
    pub fn on_failed_transcription(&mut self, error: Status) -> Vec<Promise<()>> {
        assert!(!self.is_transcribed, "CHECK(!is_transcribed_)");
        self.transcription_id = 0;
        self.text.clear();
        self.last_transcription_error = Some(error);

        assert!(
            !self.speech_recognition_queries.is_empty(),
            "CHECK(!speech_recognition_queries_.empty())"
        );
        std::mem::take(&mut self.speech_recognition_queries)
    }

    /// The outcome of `TranscriptionInfo::rate_speech_recognition`
    /// (TranscriptionInfo.cpp:149-156) — `None` is the immediate ok (the
    /// untranscribed content answers the request without a query); `Some`
    /// carries the `RateTranscribedAudioQuery` arguments.
    pub fn rate_speech_recognition(&self, promise: Promise<()>) -> Option<(i64, Promise<()>)> {
        if !self.is_transcribed {
            let mut promise = promise;
            promise.set_value(());
            return None;
        }
        assert!(self.transcription_id != 0, "CHECK(transcription_id_ != 0)");
        Some((self.transcription_id, promise))
    }

    /// `TranscriptionInfo::copy_if_transcribed`
    /// (TranscriptionInfo.cpp:158-167) — the message-copy helper; the
    /// VoiceNotes/VideoNotes callers land with M.10/M.28.
    pub fn copy_if_transcribed(info: Option<&TranscriptionInfo>) -> Option<TranscriptionInfo> {
        let info = info?;
        if !info.is_transcribed {
            return None;
        }
        Some(TranscriptionInfo {
            is_transcribed: true,
            transcription_id: info.transcription_id,
            text: info.text.clone(),
            last_transcription_error: None,
            speech_recognition_queries: Vec::new(),
        })
    }

    /// `TranscriptionInfo::update_from` (TranscriptionInfo.cpp:169-186) —
    /// adopts a parsed-from-database info unless live state would be
    /// clobbered; returns whether the content changed.
    pub fn update_from(
        old_info: &mut Option<TranscriptionInfo>,
        new_info: Option<TranscriptionInfo>,
    ) -> bool {
        let Some(new_info) = new_info else {
            return false;
        };
        if !new_info.is_transcribed {
            return false;
        }
        assert!(
            new_info.transcription_id != 0,
            "CHECK(new_info->transcription_id_ != 0)"
        );
        assert!(
            new_info.last_transcription_error.is_none(),
            "CHECK(new_info->last_transcription_error_.is_ok())"
        );
        assert!(
            new_info.speech_recognition_queries.is_empty(),
            "CHECK(new_info->speech_recognition_queries_.empty())"
        );
        match old_info {
            None => {
                *old_info = Some(new_info);
                true
            }
            Some(old) => {
                if old.transcription_id != 0 || !old.speech_recognition_queries.is_empty() {
                    return false;
                }
                assert!(!old.is_transcribed, "CHECK(!old_info->is_transcribed_)");
                *old = new_info;
                true
            }
        }
    }

    /// `TranscriptionInfo::get_speech_recognition_result_object`
    /// (TranscriptionInfo.cpp:188-200) — `None` is the C++ `nullptr`.
    pub fn get_speech_recognition_result_object(&self) -> Option<td_api::SpeechRecognitionResult> {
        if self.is_transcribed {
            return Some(
                td_api::SpeechRecognitionResult::speechRecognitionResultText(
                    td_api::speechRecognitionResultText {
                        text: self.text.clone(),
                    },
                ),
            );
        }
        if !self.speech_recognition_queries.is_empty() {
            return Some(
                td_api::SpeechRecognitionResult::speechRecognitionResultPending(
                    td_api::speechRecognitionResultPending {
                        partial_text: self.text.clone(),
                    },
                ),
            );
        }
        if let Some(error) = &self.last_transcription_error {
            return Some(
                td_api::SpeechRecognitionResult::speechRecognitionResultError(
                    td_api::speechRecognitionResultError {
                        error: Some(td_api::Error {
                            code: error.code(),
                            message: error.message().to_string(),
                        }),
                    },
                ),
            );
        }
        None
    }

    /// `TranscriptionInfo::store` (TranscriptionInfo.hpp:16-21) — the
    /// message-database form; only a finished transcription is stored.
    pub fn store(&self, out: &mut Vec<u8>) {
        assert!(self.is_transcribed, "CHECK(is_transcribed())");
        crate::store_parse::store_i64(out, self.transcription_id);
        crate::store_parse::store_string(out, &self.text);
    }

    /// `TranscriptionInfo::parse` (TranscriptionInfo.hpp:23-29).
    pub fn parse(
        input: &mut crate::store_parse::Parser<'_>,
    ) -> Result<TranscriptionInfo, &'static str> {
        let transcription_id = input.i64_()?;
        let text = input.string()?;
        assert!(transcription_id != 0, "CHECK(transcription_id_ != 0)");
        Ok(TranscriptionInfo {
            is_transcribed: true,
            transcription_id,
            text,
            last_transcription_error: None,
            speech_recognition_queries: Vec::new(),
        })
    }
}

/// `TranscriptionManager::TrialParameters` (TranscriptionManager.h:63-78) —
/// the speech-recognition trial state persisted in the binlog pmc under
/// `speech_recognition_trial` (TranscriptionManager.cpp:120-122). Moved
/// in from the M.3.9.2 `manager_option_states` stand-in; the pmc bytes
/// are the same.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrialParameters {
    weekly_number: i32,
    duration_max: i32,
    left_tries: i32,
    next_reset_date: i32,
}

impl TrialParameters {
    /// `TrialParameters::update_left_tries` (TranscriptionManager.cpp:24-31)
    /// — a passed reset date restarts the weekly counter, otherwise the
    /// left tries clamp at the weekly number.
    fn update_left_tries(&mut self, unix_time: i32) {
        if self.next_reset_date <= unix_time {
            self.next_reset_date = 0;
            self.left_tries = self.weekly_number;
        } else if self.left_tries > self.weekly_number {
            self.left_tries = self.weekly_number;
        }
    }

    /// `TrialParameters::get_update_speech_recognition_trial_object`
    /// (TranscriptionManager.cpp:212-216).
    fn get_update_speech_recognition_trial_object(&self) -> td_api::Update {
        td_api::Update::updateSpeechRecognitionTrial(td_api::updateSpeechRecognitionTrial {
            max_media_duration: self.duration_max,
            weekly_count: self.weekly_number,
            left_count: self.left_tries,
            next_reset_date: self.next_reset_date,
        })
    }

    /// `TrialParameters::store` (TranscriptionManager.cpp:33-57) — the
    /// log-event flags word plus the four conditional int32s.
    fn store(&self, out: &mut Vec<u8>) {
        let mut flags = 0i32;
        if self.weekly_number != 0 {
            flags |= 1;
        }
        if self.duration_max != 0 {
            flags |= 2;
        }
        if self.left_tries != 0 {
            flags |= 4;
        }
        if self.next_reset_date != 0 {
            flags |= 8;
        }
        crate::store_parse::store_i32(out, flags);
        if flags & 1 != 0 {
            crate::store_parse::store_i32(out, self.weekly_number);
        }
        if flags & 2 != 0 {
            crate::store_parse::store_i32(out, self.duration_max);
        }
        if flags & 4 != 0 {
            crate::store_parse::store_i32(out, self.left_tries);
        }
        if flags & 8 != 0 {
            crate::store_parse::store_i32(out, self.next_reset_date);
        }
    }

    /// `TrialParameters::parse` (TranscriptionManager.cpp:59-83).
    fn parse(input: &mut crate::store_parse::Parser<'_>) -> Result<TrialParameters, &'static str> {
        let flags = input.i32_()?;
        let mut result = TrialParameters::default();
        if flags & 1 != 0 {
            result.weekly_number = input.i32_()?;
        }
        if flags & 2 != 0 {
            result.duration_max = input.i32_()?;
        }
        if flags & 4 != 0 {
            result.left_tries = input.i32_()?;
        }
        if flags & 8 != 0 {
            result.next_reset_date = input.i32_()?;
        }
        Ok(result)
    }
}

/// The `G()`/manager slice the collapsed manager reads through (the C++
/// manager reads the same surfaces off its `Td *td_` directly). The
/// `send_update`s buffer into the Td-drained outbox — the collapsed
/// `send_closure(G()->td(), &Td::send_update, ...)` hop.
pub trait TranscriptionManagerEnv {
    // -- the G()/auth slice
    fn auth_is_authorized(&self) -> bool;
    fn auth_is_bot(&self) -> bool;
    fn close_flag(&self) -> bool;

    // -- the clock planes
    /// `G()->unix_time()`.
    fn unix_time(&self) -> i32;
    /// The scheduler clock the C++ `Timeout`/`MultiTimeout` deadlines
    /// arm against (real seconds, like `td::Time::now`).
    fn now(&self) -> f64;

    // -- the pmc slice
    /// `G()->td_db()->get_binlog_pmc()->get/set` — the raw form.
    fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8>;
    fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]);

    // -- the update plane
    /// `send_closure(G()->td(), &Td::send_update, ...)`.
    fn send_update(&mut self, update: td_api::Update);

    // -- the cross-manager surfaces
    /// `messages_manager_->have_message_force` — [blocked: M.10]; the
    /// collapsed line answers `false` (no message plane exists yet), so
    /// the C++ "Message not found" band is the offline answer.
    fn have_message_force(&self, message_full_id: MessageFullId, source: &str) -> bool;
    /// `messages_manager_->on_external_update_message_content` —
    /// [blocked: M.10].
    fn on_external_update_message_content(&mut self, message_full_id: MessageFullId, source: &str);
    /// `messages_manager_->on_update_message_content` — [blocked: M.10].
    fn on_update_message_content(&mut self, message_full_id: MessageFullId);
    /// `VideoNotesManager::get_video_note_transcription_info` /
    /// `VoiceNotesManager::get_voice_note_transcription_info`
    /// (TranscriptionManager.cpp:252-262) — the per-content info
    /// storage, [blocked: M.28]; the seam runs `f` against the stored
    /// info when the content plane exists and answers `None` while it
    /// does not.
    fn with_transcription_info<R, F: FnOnce(&mut TranscriptionInfo) -> R>(
        &mut self,
        file_info: FileInfo,
        allow_creation: bool,
        f: F,
    ) -> Option<R>;

    // -- the net query tails (the net-drain 47 families — the wire
    // -- construction lives at the Td env send legs)
    /// `TranscribeAudioQuery` (TranscriptionInfo.cpp:21-55) — the C++
    /// query carries the `on_transcribed_audio` handler closure, not the
    /// parked request promises; the closure's `FileInfo` capture rides
    /// the kind so the answer re-enters the manager for the same note.
    fn send_transcribe_audio_query(&mut self, message_full_id: MessageFullId, file_info: FileInfo);
    /// `RateTranscribedAudioQuery::send` (TranscriptionInfo.cpp:65-74) —
    /// the request promise has already parked in the family lot by the
    /// caller (the C++ query carries it on `promise_`).
    fn send_rate_transcribed_audio_query(
        &mut self,
        message_full_id: MessageFullId,
        transcription_id: i64,
        is_good: bool,
    );

    // -- the file-manager slice (the recognizeSpeechInFile flow, the
    // -- 1.8.70 family; each op is the one C++ line over the held
    // -- FileManager)
    /// `G()->close_status()` — the finish error while closing.
    fn close_status(&self) -> Status;
    /// `td_->file_manager_->get_input_file_id(FileType::
    /// SpeechRecognitionAudio, audio_file, DialogId(), false, false)`
    /// (TranscriptionManager.cpp:482-484) — the C++ five-argument
    /// overload (`is_encrypted` defaulted off; the
    /// `get_by_hash`/`is_secure`/`force_reuse`/`never_reuse` tail keeps
    /// the C++ defaults).
    fn fm_get_input_file_id(
        &mut self,
        audio_file: Option<td_api::InputFile>,
    ) -> tdutils::status::Result<FileId>;
    /// `FileManager::get_internal_upload_id()`
    /// (TranscriptionManager.cpp:485).
    fn fm_get_internal_upload_id(&mut self) -> i64;
    /// `file_view.is_encrypted()` (TranscriptionManager.cpp:492).
    fn fm_view_is_encrypted(&mut self, file_id: FileId) -> bool;
    /// `file_view.has_full_local_location() ||
    /// file_view.has_generate_location()`
    /// (TranscriptionManager.cpp:495); the empty view answers false.
    fn fm_view_has_upload_location(&mut self, file_id: FileId) -> bool;
    /// `file_view.get_type()` (TranscriptionManager.cpp:605); the empty
    /// view answers `FileType::None`.
    fn fm_view_file_type(&mut self, file_id: FileId) -> FileType;
    /// `td_->file_manager_->upload(file_upload_id, callback, 1, 0)`
    /// (TranscriptionManager.cpp:518) — the callback records the
    /// completions for the Td drain (the C++ callback hops through the
    /// actor id).
    fn fm_upload_audio_file(&mut self, file_upload_id: FileUploadId);
    /// `td_->file_manager_->delete_partial_remote_location(file_upload_id)`
    /// (TranscriptionManager.cpp:603).
    fn fm_delete_partial_remote_location(&mut self, file_upload_id: FileUploadId);
    /// `td_->file_manager_->delete_file(file_id, Promise<Unit>(),
    /// "finish_audio_file_transcription")`
    /// (TranscriptionManager.cpp:606).
    fn fm_delete_file(&mut self, file_id: FileId);
    /// `td_->dialog_manager_->check_dialog_access(dialog_id, false,
    /// AccessRights::Read, source)`
    /// (TranscriptionManager.cpp:617-619) — the optional head of
    /// `getSpeechRecognitionLanguages`.
    fn check_dialog_access_read(
        &mut self,
        dialog_id: DialogId,
        source: &str,
    ) -> tdutils::status::Result<()>;
    /// `TranscribeAudioFileQuery::send`
    /// (TranscriptionManager.cpp:80-84) — the uploaded `InputFile`
    /// rides the query; no dialog peer.
    fn send_transcribe_audio_file_query(
        &mut self,
        file_upload_id: FileUploadId,
        input_file: telegram_api::InputFile,
    );
    /// `GetTranscriptionLangsQuery::send`
    /// (TranscriptionManager.cpp:41-49) — the optional read peer
    /// resolves at the send leg; the promise has already parked in the
    /// family lot by the caller.
    fn send_get_transcription_langs_query(&mut self, dialog_id: DialogId);
}

/// The side effects `TranscriptionManager::on_transcribed_audio_update`
/// runs after the `TranscriptionInfo` mutation, in the C++ order.
enum InfoOutcome {
    /// `on_failed_transcription` — the content refresh, then the ok
    /// answers (TranscriptionManager.cpp:327-331).
    Failed(Vec<Promise<()>>),
    /// `on_final_transcription` — the content refresh, then the ok
    /// answers (TranscriptionManager.cpp:335-338).
    Final(Vec<Promise<()>>),
    /// `on_partial_transcription` — the content refresh only when the
    /// text changed, then the initial subscription
    /// (TranscriptionManager.cpp:340-353).
    Partial {
        is_changed: bool,
        transcription_id: i64,
    },
}

/// `TranscriptionManager` (TranscriptionManager.h:33-133).
#[derive(Default)]
pub struct TranscriptionManager {
    trial_parameters: TrialParameters,
    /// The C++ `Timeout trial_parameters_timeout_` — the armed deadline,
    /// `None` is the cancelled timeout.
    trial_parameters_timeout: Option<f64>,

    /// `pending_audio_transcriptions_` — the handler map collapsed to
    /// the closures' free variable (the message `FileInfo` or the file
    /// `FileUploadId`; the wire answer (`on_transcribed_audio`) and the
    /// updates intake both route back through the entry's continuation).
    pending_audio_transcriptions: HashMap<i64, PendingTranscription>,
    /// The C++ `MultiTimeout pending_audio_transcription_timeout_` — the
    /// per-transcription-id deadlines.
    pending_audio_transcription_timeouts: BTreeMap<i64, f64>,

    /// `voice_messages_` — file id → the message full ids carrying it
    /// (the C++ `FlatHashSet` is unordered; the BTreeSet pins the
    /// iteration order for the port's determinism).
    voice_messages: HashMap<FileId, BTreeSet<MessageFullId>>,
    /// `message_file_ids_`.
    message_file_ids: HashMap<MessageFullId, FileInfo>,
    /// The rate-family request promises — the C++
    /// `RateTranscribedAudioQuery` carries the promise on the query
    /// (`promise_`, TranscriptionInfo.cpp:57-62); the collapse parks it
    /// here and the answer pops the head (FIFO-paired per family, §5).
    pending_rate_promises: Vec<Promise<()>>,
    /// `audio_file_transcriptions_` — the in-flight
    /// `recognizeSpeechInFile` parks keyed by the upload pair.
    audio_file_transcriptions: HashMap<FileUploadId, AudioFileTranscription>,
    /// The `getSpeechRecognitionLanguages` request promises — the C++
    /// `GetTranscriptionLangsQuery` carries the promise on the query
    /// (`promise_`, TranscriptionManager.cpp:33-39); the collapse parks
    /// it here and the answer pops the head (FIFO-paired per family, §5).
    pending_langs_promises: Vec<Promise<td_api::SpeechRecognitionLanguages>>,
}

/// Pop the oldest parked entry — the §5 FIFO-per-family convention (the
/// C++ promise rides its own ResultHandler; the net completes the
/// same-family queries in dispatch order, so the head pairs).
fn pop_head<T: Send + 'static>(queue: &mut Vec<Promise<T>>) -> Option<Promise<T>> {
    if queue.is_empty() {
        None
    } else {
        Some(queue.remove(0))
    }
}

impl TranscriptionManager {
    /// `TranscriptionManager::get_trial_parameters_database_key`
    /// (TranscriptionManager.cpp:120-122).
    fn get_trial_parameters_database_key() -> &'static [u8] {
        b"speech_recognition_trial"
    }

    /// The constructor's restore (TranscriptionManager.cpp:90-95 — the
    /// `load_trial_parameters` call; the `MultiTimeout` callback wiring
    /// is the env's business).
    pub fn init(&mut self, env: &mut impl TranscriptionManagerEnv) {
        self.load_trial_parameters(env);
    }

    /// `TranscriptionManager::load_trial_parameters`
    /// (TranscriptionManager.cpp:124-142).
    fn load_trial_parameters(&mut self, env: &mut impl TranscriptionManagerEnv) {
        if !env.auth_is_authorized() || env.auth_is_bot() {
            return;
        }

        let log_event_bytes = env.binlog_pmc_get_raw(Self::get_trial_parameters_database_key());
        if !log_event_bytes.is_empty() {
            let mut parser = crate::store_parse::Parser::new(&log_event_bytes);
            match TrialParameters::parse(&mut parser) {
                Ok(mut parsed) => {
                    parsed.update_left_tries(env.unix_time());
                    self.trial_parameters = parsed;
                }
                Err(status) => {
                    tdutils::log_error!(
                        "Failed to parse speech recognition trial parameters from binlog: {status}"
                    );
                    self.trial_parameters = TrialParameters::default();
                    self.save_trial_parameters(env);
                }
            }
        }
        self.send_update_speech_recognition_trial(env);
        self.set_speech_recognition_trial_timeout(env);
    }

    /// `TranscriptionManager::save_trial_parameters`
    /// (TranscriptionManager.cpp:144-147).
    fn save_trial_parameters(&self, env: &mut impl TranscriptionManagerEnv) {
        let mut blob = Vec::new();
        self.trial_parameters.store(&mut blob);
        env.binlog_pmc_set_raw(Self::get_trial_parameters_database_key(), &blob);
    }

    /// `TranscriptionManager::on_update_trial_parameters`
    /// (TranscriptionManager.cpp:149-160).
    pub fn on_update_trial_parameters(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        weekly_number: i32,
        duration_max: i32,
        cooldown_until: i32,
    ) {
        if !env.auth_is_authorized() {
            return;
        }
        assert!(!env.auth_is_bot(), "CHECK(!td_->auth_manager_->is_bot())");
        let new_trial_parameters = TrialParameters {
            weekly_number: weekly_number.max(0),
            duration_max: duration_max.max(0),
            next_reset_date: if cooldown_until > 0 {
                cooldown_until
            } else {
                self.trial_parameters.next_reset_date
            },
            left_tries: self.trial_parameters.left_tries,
        };
        self.set_trial_parameters(env, new_trial_parameters);
    }

    /// `TranscriptionManager::set_trial_parameters`
    /// (TranscriptionManager.cpp:162-172).
    fn set_trial_parameters(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        mut new_trial_parameters: TrialParameters,
    ) {
        new_trial_parameters.update_left_tries(env.unix_time());
        if new_trial_parameters == self.trial_parameters {
            return;
        }

        self.trial_parameters = new_trial_parameters;
        self.send_update_speech_recognition_trial(env);
        self.set_speech_recognition_trial_timeout(env);
        self.save_trial_parameters(env);
    }

    /// `TranscriptionManager::set_speech_recognition_trial_timeout`
    /// (TranscriptionManager.cpp:174-182).
    fn set_speech_recognition_trial_timeout(&mut self, env: &mut impl TranscriptionManagerEnv) {
        if self.trial_parameters.next_reset_date == 0 {
            self.trial_parameters_timeout = None;
        } else {
            self.trial_parameters_timeout = Some(
                env.now() + (self.trial_parameters.next_reset_date - env.unix_time() + 1) as f64,
            );
        }
    }

    /// `TranscriptionManager::on_trial_parameters_timeout`
    /// (TranscriptionManager.cpp:193-199) — re-runs the clamps through
    /// `set_trial_parameters`.
    fn on_trial_parameters_timeout(&mut self, env: &mut impl TranscriptionManagerEnv) {
        if !env.auth_is_authorized() {
            return;
        }
        assert!(!env.auth_is_bot(), "CHECK(!td_->auth_manager_->is_bot())");
        let trial_parameters = self.trial_parameters;
        self.set_trial_parameters(env, trial_parameters);
    }

    /// `TranscriptionManager::send_update_speech_recognition_trial`
    /// (TranscriptionManager.cpp:201-203).
    fn send_update_speech_recognition_trial(&self, env: &mut impl TranscriptionManagerEnv) {
        env.send_update(self.get_update_speech_recognition_trial_object());
    }

    /// `TranscriptionManager::get_update_speech_recognition_trial_object`
    /// (TranscriptionManager.cpp:205-210 + 212-216).
    pub fn get_update_speech_recognition_trial_object(&self) -> td_api::Update {
        self.trial_parameters
            .get_update_speech_recognition_trial_object()
    }

    /// Park one rate-family promise and send the wire query — the
    /// `RateTranscribedAudioQuery` send wrapper
    /// (TranscriptionInfo.cpp:65-74): the request promise rides the
    /// query in the C++ (`promise_`), the collapse parks it in the family
    /// lot before the env send; FIFO order pairs with the answer drain
    /// (§5).
    pub(crate) fn send_rate_transcribed_audio_query(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        message_full_id: MessageFullId,
        transcription_id: i64,
        is_good: bool,
        promise: Promise<()>,
    ) {
        self.pending_rate_promises.push(promise);
        env.send_rate_transcribed_audio_query(message_full_id, transcription_id, is_good);
    }

    /// The rate-family promise pop for the
    /// `Td::on_transcription_net_answer` drain — the FIFO head the answer
    /// pairs with (the C++ promise rides the query).
    pub(crate) fn pop_rate_promise(&mut self) -> Option<Promise<()>> {
        pop_head(&mut self.pending_rate_promises)
    }

    /// `TranscriptionManager::register_voice` (TranscriptionManager.cpp:
    /// 218-231) — the MessagesManager/VoiceNotes/VideoNotes callers land
    /// with M.10/M.28.
    pub fn register_voice(
        &mut self,
        env: &impl TranscriptionManagerEnv,
        file_id: FileId,
        content_type: TranscriptionKind,
        message_full_id: MessageFullId,
        source: &str,
    ) {
        let message_id = message_full_id.get_message_id();
        if env.auth_is_bot()
            || message_id.is_scheduled()
            || !message_id.is_server()
            || message_full_id.get_dialog_id().get_type() == DialogType::SecretChat
        {
            return;
        }
        tdutils::log_info!("Register voice {file_id} from {message_full_id:?} from {source}");
        assert!(file_id.is_valid(), "CHECK(file_id.is_valid())");
        let message_full_ids = self.voice_messages.entry(file_id).or_default();
        assert!(
            message_full_ids.insert(message_full_id),
            "LOG_CHECK failed: {source} {file_id} {message_full_id:?}"
        );
        assert!(
            self.message_file_ids
                .insert(
                    message_full_id,
                    FileInfo {
                        kind: content_type,
                        file_id
                    }
                )
                .is_none(),
            "CHECK(is_inserted)"
        );
    }

    /// `TranscriptionManager::unregister_voice` (TranscriptionManager.cpp:
    /// 233-250).
    pub fn unregister_voice(
        &mut self,
        env: &impl TranscriptionManagerEnv,
        file_id: FileId,
        message_full_id: MessageFullId,
        source: &str,
    ) {
        let message_id = message_full_id.get_message_id();
        if env.auth_is_bot()
            || message_id.is_scheduled()
            || !message_id.is_server()
            || message_full_id.get_dialog_id().get_type() == DialogType::SecretChat
        {
            return;
        }
        tdutils::log_info!("Unregister voice {file_id} from {message_full_id:?} from {source}");
        assert!(file_id.is_valid(), "CHECK(file_id.is_valid())");
        let is_deleted = self
            .voice_messages
            .get_mut(&file_id)
            .is_some_and(|message_full_ids| message_full_ids.remove(&message_full_id));
        assert!(
            is_deleted,
            "LOG_CHECK failed: {source} {file_id} {message_full_id:?}"
        );
        if self
            .voice_messages
            .get(&file_id)
            .is_some_and(|message_full_ids| message_full_ids.is_empty())
        {
            self.voice_messages.remove(&file_id);
        }
        let is_deleted = self.message_file_ids.remove(&message_full_id).is_some();
        assert!(is_deleted, "CHECK(is_deleted)");
    }

    /// `TranscriptionManager::recognize_speech`
    /// (TranscriptionManager.cpp:264-282).
    pub fn recognize_speech(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        message_full_id: MessageFullId,
        promise: Promise<()>,
    ) {
        if !env.have_message_force(message_full_id, "recognize_speech") {
            let mut promise = promise;
            promise.set_error(Status::error(400, "Message not found"));
            return;
        }

        let Some(file_info) = self.message_file_ids.get(&message_full_id).copied() else {
            let mut promise = promise;
            promise.set_error(Status::error(400, "Message can't be transcribed"));
            return;
        };

        let should_send = env
            .with_transcription_info(file_info, true, |transcription_info| {
                transcription_info.recognize_speech(promise)
            })
            .unwrap_or(false);
        if should_send {
            env.send_transcribe_audio_query(message_full_id, file_info);
            self.on_transcription_updated(env, file_info.file_id);
        }
    }

    /// `TranscriptionManager::on_transcribed_audio`
    /// (TranscriptionManager.cpp:284-316) — the `TranscribeAudioQuery`
    /// answer intake.
    pub fn on_transcribed_audio(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        file_info: FileInfo,
        r_audio: tdutils::status::Result<telegram_api::messages_TranscribedAudio>,
    ) {
        if env.close_flag() || !env.auth_is_authorized() {
            return;
        }
        match r_audio {
            tdutils::status::Result::Err(error) => {
                self.on_transcribed_audio_update(
                    env,
                    file_info,
                    true,
                    tdutils::status::Result::Err(error.clone()),
                );
                self.on_transcription_error(env, &error);
            }
            tdutils::status::Result::Ok(audio) => {
                if audio.transcription_id == 0 {
                    return self.on_transcribed_audio_update(
                        env,
                        file_info,
                        true,
                        tdutils::status::Result::Err(Status::error(
                            500,
                            "Receive no transcription identifier",
                        )),
                    );
                }
                let update = telegram_api::updateTranscribedAudio {
                    text: audio.text.clone(),
                    transcription_id: audio.transcription_id,
                    pending: audio.pending,
                    ..Default::default()
                };
                self.on_transcribed_audio_update(
                    env,
                    file_info,
                    true,
                    tdutils::status::Result::Ok(update),
                );

                self.on_transcription_trial_parameters(env, &audio);
            }
        }
    }

    /// `TranscriptionManager::on_transcription_error`
    /// (TranscriptionManager.cpp:386-394) — the 429 retry-after band of
    /// the trial table, shared by the message and file intakes.
    fn on_transcription_error(&mut self, env: &mut impl TranscriptionManagerEnv, error: &Status) {
        let retry_after = get_retry_after(error.code(), error.message());
        if retry_after > 0 && self.trial_parameters.left_tries > 0 {
            let mut new_trial_parameters = self.trial_parameters;
            new_trial_parameters.next_reset_date = env.unix_time() + retry_after;
            new_trial_parameters.left_tries = 0;
            self.set_trial_parameters(env, new_trial_parameters);
        }
    }

    /// `TranscriptionManager::on_transcription_trial_parameters`
    /// (TranscriptionManager.cpp:396-403) — the
    /// `messages_transcribedAudio::TRIAL_REMAINS_NUM_MASK` band (the
    /// `flags.1` pair, trial_remains_num + trial_remains_until_date),
    /// shared by the message and file intakes.
    fn on_transcription_trial_parameters(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        audio: &telegram_api::messages_TranscribedAudio,
    ) {
        if audio.flags & (1 << 1) != 0 {
            let mut new_trial_parameters = self.trial_parameters;
            new_trial_parameters.next_reset_date = audio.trial_remains_until_date.max(0);
            new_trial_parameters.left_tries = audio.trial_remains_num;
            self.set_trial_parameters(env, new_trial_parameters);
        }
    }

    /// `TranscriptionManager::recognize_speech_in_file`
    /// (TranscriptionManager.cpp:479-519) — the 1.8.70 file flow: the
    /// input file resolves under the never-exposed
    /// `SpeechRecognitionAudio` type, the request parks by the upload
    /// pair, and the encrypted/location bands answer synchronously
    /// through the finish.
    pub fn recognize_speech_in_file(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        audio_file: Option<td_api::InputFile>,
        recognition_id: i64,
        promise: Promise<td_api::Text>,
    ) {
        let file_id = match env.fm_get_input_file_id(audio_file) {
            tdutils::status::Result::Ok(file_id) => file_id,
            tdutils::status::Result::Err(status) => {
                let mut promise = promise;
                promise.set_error(status);
                return;
            }
        };
        let file_upload_id = FileUploadId::new(file_id, env.fm_get_internal_upload_id());
        assert!(
            self.audio_file_transcriptions
                .insert(
                    file_upload_id,
                    AudioFileTranscription {
                        recognition_id,
                        promise,
                    },
                )
                .is_none(),
            "CHECK(!transcription.promise_)"
        );

        if env.fm_view_is_encrypted(file_id) {
            return self.finish_audio_file_transcription(
                env,
                file_upload_id,
                tdutils::status::Result::Err(Status::error(400, "Can't use encrypted file")),
            );
        }
        if !env.fm_view_has_upload_location(file_id) {
            return self.finish_audio_file_transcription(
                env,
                file_upload_id,
                tdutils::status::Result::Err(Status::error(
                    400,
                    "Need local or generate location to upload the file",
                )),
            );
        }

        tdutils::log_info!("Upload audio {file_upload_id:?} for speech recognition");
        env.fm_upload_audio_file(file_upload_id);
    }

    /// `TranscriptionManager::on_upload_audio_file`
    /// (TranscriptionManager.cpp:521-537) — the upload-callback ok hop.
    pub fn on_upload_audio_file(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        file_upload_id: FileUploadId,
        input_file: Option<telegram_api::InputFile>,
    ) {
        tdutils::log_info!("Audio {file_upload_id:?} has been uploaded");
        if env.close_flag() {
            return self.finish_audio_file_transcription(
                env,
                file_upload_id,
                tdutils::status::Result::Err(env.close_status()),
            );
        }
        let Some(input_file) = input_file else {
            return self.finish_audio_file_transcription(
                env,
                file_upload_id,
                tdutils::status::Result::Err(Status::error(500, "Failed to reupload the file")),
            );
        };
        env.send_transcribe_audio_file_query(file_upload_id, input_file);
    }

    /// `TranscriptionManager::on_upload_audio_file_error`
    /// (TranscriptionManager.cpp:539-544) — the upload-callback error
    /// hop; non-positive codes clamp at 500.
    pub fn on_upload_audio_file_error(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        file_upload_id: FileUploadId,
        status: Status,
    ) {
        tdutils::log_info!("Audio {file_upload_id:?} has upload error {status}");
        assert!(status.is_error(), "CHECK(status.is_error())");
        let code = if status.code() > 0 {
            status.code()
        } else {
            500
        };
        self.finish_audio_file_transcription(
            env,
            file_upload_id,
            tdutils::status::Result::Err(Status::error(code, status.message())),
        );
    }

    /// `TranscriptionManager::on_transcribed_audio_file`
    /// (TranscriptionManager.cpp:546-570) — the
    /// `TranscribeAudioFileQuery` answer intake.
    pub fn on_transcribed_audio_file(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        file_upload_id: FileUploadId,
        r_audio: tdutils::status::Result<telegram_api::messages_TranscribedAudio>,
    ) {
        if env.close_flag() {
            return self.finish_audio_file_transcription(
                env,
                file_upload_id,
                tdutils::status::Result::Err(env.close_status()),
            );
        }
        let audio = match r_audio {
            tdutils::status::Result::Err(error) => {
                self.on_transcription_error(env, &error);
                return self.finish_audio_file_transcription(
                    env,
                    file_upload_id,
                    tdutils::status::Result::Err(error),
                );
            }
            tdutils::status::Result::Ok(audio) => audio,
        };
        self.on_transcription_trial_parameters(env, &audio);
        if audio.transcription_id == 0 {
            return self.finish_audio_file_transcription(
                env,
                file_upload_id,
                tdutils::status::Result::Err(Status::error(
                    500,
                    "Receive no transcription identifier",
                )),
            );
        }
        if !audio.pending {
            return self.finish_audio_file_transcription(
                env,
                file_upload_id,
                tdutils::status::Result::Ok(audio.text),
            );
        }
        self.on_partial_audio_file_transcription(env, file_upload_id, audio.text);
        self.subscribe_to_transcribed_audio_updates(
            env,
            audio.transcription_id,
            PendingTranscription::File(file_upload_id),
        );
    }

    /// `TranscriptionManager::on_transcribed_audio_file_update`
    /// (TranscriptionManager.cpp:572-583) — the subscription
    /// continuation of the file flow.
    pub fn on_transcribed_audio_file_update(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        file_upload_id: FileUploadId,
        r_update: tdutils::status::Result<telegram_api::updateTranscribedAudio>,
    ) {
        let update = match r_update {
            tdutils::status::Result::Err(error) => {
                return self.finish_audio_file_transcription(
                    env,
                    file_upload_id,
                    tdutils::status::Result::Err(error),
                );
            }
            tdutils::status::Result::Ok(update) => update,
        };
        if !update.pending {
            self.finish_audio_file_transcription(
                env,
                file_upload_id,
                tdutils::status::Result::Ok(update.text),
            );
        } else {
            self.on_partial_audio_file_transcription(env, file_upload_id, update.text);
        }
    }

    /// `TranscriptionManager::on_partial_audio_file_transcription`
    /// (TranscriptionManager.cpp:585-595) — the partial-text update for
    /// the requests that passed a nonzero `recognition_id`.
    fn on_partial_audio_file_transcription(
        &self,
        env: &mut impl TranscriptionManagerEnv,
        file_upload_id: FileUploadId,
        partial_text: String,
    ) {
        let recognition_id = self
            .audio_file_transcriptions
            .get(&file_upload_id)
            .expect("CHECK(it != audio_file_transcriptions_.end())")
            .recognition_id;
        if recognition_id == 0 {
            return;
        }
        env.send_update(td_api::Update::updateSpeechRecognitionInFile(
            td_api::updateSpeechRecognitionInFile {
                recognition_id,
                partial_text,
            },
        ));
    }

    /// `TranscriptionManager::finish_audio_file_transcription`
    /// (TranscriptionManager.cpp:597-613) — the file flow's single
    /// completion: the partial remote location drops, the
    /// speech-recognition file deletes, the request answers.
    fn finish_audio_file_transcription(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        file_upload_id: FileUploadId,
        r_text: tdutils::status::Result<String>,
    ) {
        let transcription = self
            .audio_file_transcriptions
            .remove(&file_upload_id)
            .expect("CHECK(it != audio_file_transcriptions_.end())");

        env.fm_delete_partial_remote_location(file_upload_id);
        let file_id = file_upload_id.file_id;
        if env.fm_view_file_type(file_id) == FileType::SpeechRecognitionAudio {
            env.fm_delete_file(file_id);
        }

        match r_text {
            tdutils::status::Result::Err(status) => {
                let mut promise = transcription.promise;
                promise.set_error(status);
            }
            tdutils::status::Result::Ok(text) => {
                let mut promise = transcription.promise;
                promise.set_value(td_api::Text { text });
            }
        }
    }

    /// `TranscriptionManager::get_speech_recognition_languages`
    /// (TranscriptionManager.cpp:615-622) — the optional dialog-access
    /// head, then the langs query (the promise parks in the family lot).
    pub fn get_speech_recognition_languages(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        dialog_id: DialogId,
        promise: Promise<td_api::SpeechRecognitionLanguages>,
    ) {
        if dialog_id != DialogId::default()
            && let tdutils::status::Result::Err(status) =
                env.check_dialog_access_read(dialog_id, "get_speech_recognition_languages")
        {
            let mut promise = promise;
            promise.set_error(status);
            return;
        }
        self.pending_langs_promises.push(promise);
        env.send_get_transcription_langs_query(dialog_id);
    }

    /// The langs-family promise pop for the
    /// `Td::on_transcription_net_answer` drain — the FIFO head the
    /// answer pairs with (the C++ promise rides the query).
    pub(crate) fn pop_langs_promise(
        &mut self,
    ) -> Option<Promise<td_api::SpeechRecognitionLanguages>> {
        pop_head(&mut self.pending_langs_promises)
    }

    /// The side effects `on_transcribed_audio_update` runs after the
    /// `TranscriptionInfo` mutation, in the C++ order.
    fn on_transcribed_audio_update(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        file_info: FileInfo,
        is_initial: bool,
        r_update: tdutils::status::Result<telegram_api::updateTranscribedAudio>,
    ) {
        if env.close_flag() || !env.auth_is_authorized() {
            return;
        }

        let outcome =
            env.with_transcription_info(file_info, false, |transcription_info| match r_update {
                tdutils::status::Result::Err(error) => {
                    InfoOutcome::Failed(transcription_info.on_failed_transcription(error))
                }
                tdutils::status::Result::Ok(update) => {
                    let transcription_id = update.transcription_id;
                    if !update.pending {
                        InfoOutcome::Final(
                            transcription_info
                                .on_final_transcription(update.text, transcription_id),
                        )
                    } else {
                        InfoOutcome::Partial {
                            is_changed: transcription_info
                                .on_partial_transcription(update.text, transcription_id),
                            transcription_id,
                        }
                    }
                }
            });
        let Some(outcome) = outcome else {
            // [blocked: M.28] — the content plane owns no infos yet;
            // unreachable through the public intakes while the message
            // maps are empty (the M.10 seam).
            tdutils::log_info!("Transcription info missing — [blocked: M.28]");
            return;
        };
        match outcome {
            InfoOutcome::Failed(promises) => {
                self.on_transcription_updated(env, file_info.file_id);
                set_promises(promises);
            }
            InfoOutcome::Final(promises) => {
                self.on_transcription_completed(env, file_info.file_id);
                set_promises(promises);
            }
            InfoOutcome::Partial {
                is_changed,
                transcription_id,
            } => {
                if is_changed {
                    self.on_transcription_updated(env, file_info.file_id);
                }

                if is_initial {
                    self.subscribe_to_transcribed_audio_updates(
                        env,
                        transcription_id,
                        PendingTranscription::Message(file_info),
                    );
                }
            }
        }
    }

    /// `TranscriptionManager::on_transcription_updated`
    /// (TranscriptionManager.cpp:356-363).
    fn on_transcription_updated(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        file_id: FileId,
    ) {
        if let Some(message_full_ids) = self.voice_messages.get(&file_id) {
            for message_full_id in message_full_ids {
                env.on_external_update_message_content(
                    *message_full_id,
                    "on_transcription_updated",
                );
            }
        }
    }

    /// `TranscriptionManager::on_transcription_completed`
    /// (TranscriptionManager.cpp:365-372).
    fn on_transcription_completed(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        file_id: FileId,
    ) {
        if let Some(message_full_ids) = self.voice_messages.get(&file_id) {
            for message_full_id in message_full_ids {
                env.on_update_message_content(*message_full_id);
            }
        }
    }

    /// `TranscriptionManager::rate_speech_recognition`
    /// (TranscriptionManager.cpp:374-390).
    pub fn rate_speech_recognition(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        message_full_id: MessageFullId,
        is_good: bool,
        promise: Promise<()>,
    ) {
        if !env.have_message_force(message_full_id, "rate_speech_recognition") {
            let mut promise = promise;
            promise.set_error(Status::error(400, "Message not found"));
            return;
        }

        let Some(file_info) = self.message_file_ids.get(&message_full_id).copied() else {
            let mut promise = promise;
            promise.set_error(Status::error(400, "Message can't be transcribed"));
            return;
        };

        // `Option<Option<_>>` — the outer `None` is the [blocked: M.28]
        // missing-info seam (the C++ `CHECK(transcription_info != nullptr)`
        // site; the dropped promise keeps the C++ request parked), the
        // inner `None` is the C++ immediate ok, the inner `Some` carries
        // the `RateTranscribedAudioQuery` arguments.
        match env.with_transcription_info(file_info, false, |transcription_info| {
            transcription_info.rate_speech_recognition(promise)
        }) {
            None => {
                tdutils::log_info!("Transcription info missing — [blocked: M.28]");
            }
            Some(None) => {
                // the untranscribed content: the promise answered ok
            }
            Some(Some((transcription_id, promise))) => self.send_rate_transcribed_audio_query(
                env,
                message_full_id,
                transcription_id,
                is_good,
                promise,
            ),
        }
    }

    /// `TranscriptionManager::subscribe_to_transcribed_audio_updates`
    /// (TranscriptionManager.cpp:392-402) — the handler collapses to
    /// its free variable (the message `FileInfo` or the file
    /// `FileUploadId`).
    fn subscribe_to_transcribed_audio_updates(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        transcription_id: i64,
        handler: PendingTranscription,
    ) {
        assert!(transcription_id != 0, "CHECK(transcription_id != 0)");
        if self
            .pending_audio_transcriptions
            .contains_key(&transcription_id)
        {
            self.on_pending_audio_transcription_failed(
                env,
                transcription_id,
                Status::error(500, "Receive duplicate speech recognition identifier"),
            );
        }
        assert!(
            self.pending_audio_transcriptions
                .insert(transcription_id, handler)
                .is_none(),
            "CHECK(is_inserted)"
        );
        self.pending_audio_transcription_timeouts
            .insert(transcription_id, env.now() + AUDIO_TRANSCRIPTION_TIMEOUT);
    }

    /// `TranscriptionManager::on_update_transcribed_audio`
    /// (TranscriptionManager.cpp:404-419) — the UpdatesManager intake;
    /// `flags_`, `peer_` and `msg_id_` must not be used. The entry kind
    /// picks the continuation (the message machine vs the file finish).
    pub fn on_update_transcribed_audio(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        update: telegram_api::updateTranscribedAudio,
    ) {
        let Some(handler) = self
            .pending_audio_transcriptions
            .get(&update.transcription_id)
            .copied()
        else {
            return;
        };
        if !update.pending {
            self.pending_audio_transcriptions
                .remove(&update.transcription_id);
            self.pending_audio_transcription_timeouts
                .remove(&update.transcription_id);
        }
        self.dispatch_pending_transcription(env, handler, tdutils::status::Result::Ok(update));
    }

    /// `TranscriptionManager::on_pending_audio_transcription_failed`
    /// (TranscriptionManager.cpp:421-434).
    pub fn on_pending_audio_transcription_failed(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        transcription_id: i64,
        error: Status,
    ) {
        if env.close_flag() {
            return;
        }
        let Some(handler) = self.pending_audio_transcriptions.remove(&transcription_id) else {
            return;
        };
        self.pending_audio_transcription_timeouts
            .remove(&transcription_id);

        self.dispatch_pending_transcription(env, handler, tdutils::status::Result::Err(error));
    }

    /// The stored handler's call — the C++ invokes the popped closure
    /// with the result (TranscriptionManager.cpp:433-439/565-569/647/:674);
    /// the closure's free variable rides the enum.
    fn dispatch_pending_transcription(
        &mut self,
        env: &mut impl TranscriptionManagerEnv,
        handler: PendingTranscription,
        r_update: tdutils::status::Result<telegram_api::updateTranscribedAudio>,
    ) {
        match handler {
            PendingTranscription::Message(file_info) => {
                self.on_transcribed_audio_update(env, file_info, false, r_update);
            }
            PendingTranscription::File(file_upload_id) => {
                self.on_transcribed_audio_file_update(env, file_upload_id, r_update);
            }
        }
    }

    /// The C++ `on_pending_audio_transcription_timeout_callback`
    /// (TranscriptionManager.cpp:105-118) + the trial `Timeout` — the
    /// fired deadlines collapse into this pump. The C++ callback's
    /// close/authorized gates run here, before the failure is routed.
    pub fn tick_timeouts(&mut self, env: &mut impl TranscriptionManagerEnv) {
        // the MultiTimeout plane
        let now = env.now();
        let expired: Vec<i64> = self
            .pending_audio_transcription_timeouts
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for transcription_id in expired {
            if env.close_flag() {
                return;
            }
            if !env.auth_is_authorized() {
                self.pending_audio_transcription_timeouts
                    .remove(&transcription_id);
                continue;
            }
            self.on_pending_audio_transcription_failed(
                env,
                transcription_id,
                Status::error(500, "Timeout expired"),
            );
        }

        // the trial Timeout plane
        if let Some(deadline) = self.trial_parameters_timeout
            && deadline <= env.now()
        {
            self.trial_parameters_timeout = None;
            if !env.close_flag() {
                self.on_trial_parameters_timeout(env);
            }
        }
    }

    /// The earliest armed deadline, for the Td timeout slot.
    pub fn next_deadline(&self) -> Option<f64> {
        let pending = self
            .pending_audio_transcription_timeouts
            .values()
            .copied()
            .fold(None::<f64>, |acc, deadline| match acc {
                None => Some(deadline),
                Some(acc) => Some(acc.min(deadline)),
            });
        match (self.trial_parameters_timeout, pending) {
            (Some(trial), Some(pending)) => Some(trial.min(pending)),
            (Some(trial), None) => Some(trial),
            (None, pending) => pending,
        }
    }

    /// `TranscriptionManager::get_current_state`
    /// (TranscriptionManager.cpp:436-442).
    pub fn get_current_state(
        &self,
        env: &impl TranscriptionManagerEnv,
        updates: &mut Vec<td_api::Update>,
    ) {
        if !env.auth_is_authorized() || env.auth_is_bot() {
            return;
        }

        updates.push(self.get_update_speech_recognition_trial_object());
    }
}

/// `set_promises(promises)` (TranscriptionManager.cpp:330/338) — the
/// parked td_api requests answer ok; the outcome arrives through the
/// message-content updates.
fn set_promises(promises: Vec<Promise<()>>) {
    for mut promise in promises {
        promise.set_value(());
    }
}

/// `Global::get_retry_after` (Global.cpp:173-187) — the 429 band of the
/// `on_transcribed_audio` rate-limit table (and the device-token
/// Reregister retry, DeviceTokenManager.cpp:441-447).
pub(crate) fn get_retry_after(error_code: i32, error_message: &str) -> i32 {
    if error_code != 429 {
        return 0;
    }
    let retry_after_prefix = "Too Many Requests: retry after ";
    let Some(rest) = error_message.strip_prefix(retry_after_prefix) else {
        return 0;
    };
    match rest.parse::<i32>() {
        Ok(retry_after) if retry_after > 0 => retry_after,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::dialog_id::{DialogId, MessageFullId};
    use crate::files::FileId;
    use crate::messages_manager::MessageId;
    use tdutils::promise::{Promise, PromiseInterface};

    /// A [`PromiseInterface`] that flips flags instead of answering requests —
    /// the offline stand-in for the deferred outbox.
    struct FlagPromise<T: Send + 'static> {
        value_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
        error_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
        _type: std::marker::PhantomData<T>,
    }

    impl<T: Send + 'static> FlagPromise<T> {
        fn create() -> (
            (
                std::sync::Arc<std::sync::atomic::AtomicBool>,
                std::sync::Arc<std::sync::atomic::AtomicBool>,
            ),
            Promise<T>,
        ) {
            let value_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let error_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            (
                (value_flag.clone(), error_flag.clone()),
                Promise::from_interface(FlagPromise {
                    value_flag,
                    error_flag,
                    _type: std::marker::PhantomData,
                }),
            )
        }
    }

    impl<T: Send + 'static> PromiseInterface<T> for FlagPromise<T> {
        fn set_value(&mut self, _value: T) {
            self.value_flag
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn set_error(&mut self, _error: tdutils::status::Status) {
            self.error_flag
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// The offline env: the pmc/options state in plain maps, one recorded
    /// clock, the in-test transcription-info storage behind the M.28 seam,
    /// and the registered-message set behind the M.10 probe.
    struct TestTranscriptionEnv {
        pmc: HashMap<Vec<u8>, Vec<u8>>,
        authorized: bool,
        bot: bool,
        close_flag: bool,
        unix_time: i32,
        now: f64,
        /// the `messages_manager_->have_message_force` stand-in
        messages: Vec<MessageFullId>,
        /// the VoiceNotes/VideoNotes info storage ([blocked: M.28] seam)
        infos: HashMap<i32, TranscriptionInfo>,
        /// the recorded wire-query tags (the offline stand-in for the
        /// net plane)
        queries: Vec<&'static str>,
        updates: Vec<td_api::Update>,
        // -- the file-manager slice stand-ins
        /// the canned `get_input_file_id` result
        input_file_id_result: tdutils::status::Result<FileId>,
        internal_upload_id: i64,
        view_is_encrypted: bool,
        view_has_upload_location: bool,
        /// the canned view types by raw file id (the default is `None`)
        view_file_types: HashMap<i32, FileType>,
        uploads: Vec<FileUploadId>,
        deleted_partial: Vec<FileUploadId>,
        deleted_files: Vec<FileId>,
        /// the canned `check_dialog_access` failure (`None` is access)
        dialog_access_error: Option<Status>,
    }

    impl TestTranscriptionEnv {
        fn new() -> TestTranscriptionEnv {
            TestTranscriptionEnv {
                pmc: HashMap::new(),
                authorized: true,
                bot: false,
                close_flag: false,
                unix_time: 1000,
                now: 10_000.0,
                messages: Vec::new(),
                infos: HashMap::new(),
                queries: Vec::new(),
                updates: Vec::new(),
                input_file_id_result: tdutils::status::Result::Ok(FileId::new(3, 0)),
                internal_upload_id: 0,
                view_is_encrypted: false,
                view_has_upload_location: true,
                view_file_types: HashMap::new(),
                uploads: Vec::new(),
                deleted_partial: Vec::new(),
                deleted_files: Vec::new(),
                dialog_access_error: None,
            }
        }

        fn register_info(&mut self, file_id: i32, info: TranscriptionInfo) {
            self.infos.insert(file_id, info);
        }
    }

    impl TranscriptionManagerEnv for TestTranscriptionEnv {
        fn auth_is_authorized(&self) -> bool {
            self.authorized
        }

        fn auth_is_bot(&self) -> bool {
            self.bot
        }

        fn close_flag(&self) -> bool {
            self.close_flag
        }

        fn unix_time(&self) -> i32 {
            self.unix_time
        }

        fn now(&self) -> f64 {
            self.now
        }

        fn binlog_pmc_get_raw(&mut self, key: &[u8]) -> Vec<u8> {
            self.pmc.get(key).cloned().unwrap_or_default()
        }

        fn binlog_pmc_set_raw(&mut self, key: &[u8], value: &[u8]) {
            self.pmc.insert(key.to_vec(), value.to_vec());
        }

        fn send_update(&mut self, update: td_api::Update) {
            self.updates.push(update);
        }

        fn have_message_force(&self, message_full_id: MessageFullId, _source: &str) -> bool {
            self.messages.contains(&message_full_id)
        }

        fn on_external_update_message_content(
            &mut self,
            _message_full_id: MessageFullId,
            _source: &str,
        ) {
        }

        fn on_update_message_content(&mut self, _message_full_id: MessageFullId) {}

        fn with_transcription_info<R, F: FnOnce(&mut TranscriptionInfo) -> R>(
            &mut self,
            file_info: FileInfo,
            allow_creation: bool,
            f: F,
        ) -> Option<R> {
            if allow_creation {
                self.infos.entry(file_info.file_id.get()).or_default();
            }
            self.infos.get_mut(&file_info.file_id.get()).map(f)
        }

        fn send_transcribe_audio_query(
            &mut self,
            _message_full_id: MessageFullId,
            _file_info: FileInfo,
        ) {
            self.queries.push("transcribeAudio");
        }

        fn send_rate_transcribed_audio_query(
            &mut self,
            _message_full_id: MessageFullId,
            _transcription_id: i64,
            _is_good: bool,
        ) {
            self.queries.push("rateTranscribedAudio");
        }

        fn close_status(&self) -> Status {
            if self.close_flag {
                Status::error(500, "Request aborted")
            } else {
                Status::ok()
            }
        }

        fn fm_get_input_file_id(
            &mut self,
            _audio_file: Option<td_api::InputFile>,
        ) -> tdutils::status::Result<FileId> {
            self.input_file_id_result.clone()
        }

        fn fm_get_internal_upload_id(&mut self) -> i64 {
            self.internal_upload_id += 1;
            self.internal_upload_id
        }

        fn fm_view_is_encrypted(&mut self, _file_id: FileId) -> bool {
            self.view_is_encrypted
        }

        fn fm_view_has_upload_location(&mut self, _file_id: FileId) -> bool {
            self.view_has_upload_location
        }

        fn fm_view_file_type(&mut self, file_id: FileId) -> FileType {
            self.view_file_types
                .get(&file_id.get())
                .copied()
                .unwrap_or(FileType::None)
        }

        fn fm_upload_audio_file(&mut self, file_upload_id: FileUploadId) {
            self.uploads.push(file_upload_id);
            self.queries.push("uploadAudio");
        }

        fn fm_delete_partial_remote_location(&mut self, file_upload_id: FileUploadId) {
            self.deleted_partial.push(file_upload_id);
        }

        fn fm_delete_file(&mut self, file_id: FileId) {
            self.deleted_files.push(file_id);
        }

        fn check_dialog_access_read(
            &mut self,
            _dialog_id: DialogId,
            _source: &str,
        ) -> tdutils::status::Result<()> {
            match self.dialog_access_error.clone() {
                Some(status) => tdutils::status::Result::Err(status),
                None => tdutils::status::Result::Ok(()),
            }
        }

        fn send_transcribe_audio_file_query(
            &mut self,
            _file_upload_id: FileUploadId,
            _input_file: telegram_api::InputFile,
        ) {
            self.queries.push("transcribeAudioFile");
        }

        fn send_get_transcription_langs_query(&mut self, _dialog_id: DialogId) {
            self.queries.push("getTranscriptionLangs");
        }
    }

    fn full_id(chat_id: i64, message_id: i64) -> MessageFullId {
        MessageFullId::new(DialogId::new(chat_id), MessageId::new(message_id))
    }

    fn voice_file_info(file_id: i32) -> FileInfo {
        FileInfo {
            kind: TranscriptionKind::VoiceNote,
            file_id: FileId::new(file_id, 0),
        }
    }

    fn transcribed_info(text: &str) -> TranscriptionInfo {
        TranscriptionInfo {
            is_transcribed: true,
            transcription_id: 77,
            text: text.to_string(),
            last_transcription_error: None,
            speech_recognition_queries: Vec::new(),
        }
    }

    fn initial_pending_audio(
        transcription_id: i64,
        text: &str,
    ) -> telegram_api::messages_TranscribedAudio {
        telegram_api::messages_TranscribedAudio {
            flags: 1, // the `pending` wire bit + the derived bool
            pending: true,
            transcription_id,
            text: text.to_string(),
            trial_remains_num: 0,
            trial_remains_until_date: 0,
        }
    }

    fn stored_trial_blob(stored: TrialParameters) -> Vec<u8> {
        let mut blob = Vec::new();
        stored.store(&mut blob);
        blob
    }

    #[test]
    fn test_trial_parameters_pmc_round_trip() {
        for parameters in [
            TrialParameters::default(),
            TrialParameters {
                weekly_number: 10,
                duration_max: 300,
                left_tries: 7,
                next_reset_date: 1735689600,
            },
            TrialParameters {
                weekly_number: 5,
                duration_max: 0,
                left_tries: 5,
                next_reset_date: 0,
            },
        ] {
            let mut blob = Vec::new();
            parameters.store(&mut blob);
            let mut parser = crate::store_parse::Parser::new(&blob);
            assert_eq!(TrialParameters::parse(&mut parser), Ok(parameters));
        }
        // The all-zero form is exactly the 4-byte flags word.
        let mut blob = Vec::new();
        TrialParameters::default().store(&mut blob);
        assert_eq!(blob, vec![0, 0, 0, 0]);
    }

    #[test]
    fn test_update_left_tries() {
        let mut parameters = TrialParameters {
            weekly_number: 3,
            duration_max: 30,
            left_tries: 1,
            next_reset_date: 1000,
        };
        // A passed reset date restarts the weekly counter.
        parameters.update_left_tries(1000);
        assert_eq!(parameters.next_reset_date, 0);
        assert_eq!(parameters.left_tries, 3);
        // Otherwise the left tries clamp at the weekly number.
        let mut parameters = TrialParameters {
            weekly_number: 2,
            duration_max: 30,
            left_tries: 9,
            next_reset_date: 1000,
        };
        parameters.update_left_tries(999);
        assert_eq!(parameters.left_tries, 2);
        assert_eq!(parameters.next_reset_date, 1000);
    }

    #[test]
    fn test_load_trial_parameters() {
        // Unauthorized or bot starts read nothing and send nothing
        // (TranscriptionManager.cpp:125-127).
        let mut env = TestTranscriptionEnv::new();
        env.authorized = false;
        TranscriptionManager::default().init(&mut env);
        assert!(env.updates.is_empty());

        let mut env = TestTranscriptionEnv::new();
        env.bot = true;
        TranscriptionManager::default().init(&mut env);
        assert!(env.updates.is_empty());

        // An authorized restore: the stored blob parses, the passed reset
        // date clamps (does not pass), the trial update is sent and the
        // reset timeout arms at next_reset_date - unix_time + 1.
        let mut env = TestTranscriptionEnv::new();
        env.binlog_pmc_set_raw(
            b"speech_recognition_trial",
            &stored_trial_blob(TrialParameters {
                weekly_number: 5,
                duration_max: 300,
                left_tries: 2,
                next_reset_date: 1500,
            }),
        );
        let mut manager = TranscriptionManager::default();
        manager.init(&mut env);
        assert_eq!(env.updates.len(), 1);
        let td_api::Update::updateSpeechRecognitionTrial(update) = &env.updates[0] else {
            panic!("speech recognition trial update");
        };
        assert_eq!(update.weekly_count, 5);
        assert_eq!(update.max_media_duration, 300);
        assert_eq!(update.left_count, 2);
        assert_eq!(update.next_reset_date, 1500);
        assert_eq!(
            manager.next_deadline(),
            Some(env.now + (1500 - env.unix_time + 1) as f64)
        );

        // A passed reset date restarts the counter at load.
        let mut env = TestTranscriptionEnv::new();
        env.unix_time = 1500;
        env.binlog_pmc_set_raw(
            b"speech_recognition_trial",
            &stored_trial_blob(TrialParameters {
                weekly_number: 5,
                duration_max: 300,
                left_tries: 2,
                next_reset_date: 1500,
            }),
        );
        let mut manager = TranscriptionManager::default();
        manager.init(&mut env);
        let td_api::Update::updateSpeechRecognitionTrial(update) = &env.updates[0] else {
            panic!("speech recognition trial update");
        };
        assert_eq!(update.next_reset_date, 0);
        assert_eq!(update.left_count, 5);

        // A corrupt blob resets to the default and re-saves the default
        // bytes.
        let mut env = TestTranscriptionEnv::new();
        env.binlog_pmc_set_raw(b"speech_recognition_trial", b"garbage");
        let mut manager = TranscriptionManager::default();
        manager.init(&mut env);
        assert_eq!(env.updates.len(), 1);
        assert_eq!(
            env.pmc
                .get(b"speech_recognition_trial".as_slice())
                .cloned()
                .unwrap(),
            stored_trial_blob(TrialParameters::default())
        );
    }

    #[test]
    fn test_on_update_trial_parameters() {
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();

        // Unauthorized: dropped, like the C++ guard.
        env.authorized = false;
        manager.on_update_trial_parameters(&mut env, 10, 300, 2000);
        assert!(env.updates.is_empty());
        env.authorized = true;

        // First application: the negatives clamp at zero, the left tries
        // carry (and clamp), the pmc write lands, the update is carried and
        // the timeout arms at next_reset_date - unix_time + 1.
        manager.on_update_trial_parameters(&mut env, -1, 300, 2000);
        assert_eq!(env.updates.len(), 1);
        let td_api::Update::updateSpeechRecognitionTrial(update) = &env.updates[0] else {
            panic!("speech recognition trial update");
        };
        assert_eq!(update.weekly_count, 0);
        assert_eq!(update.max_media_duration, 300);
        assert_eq!(update.left_count, 0);
        assert_eq!(update.next_reset_date, 2000);
        assert!(
            !env.pmc
                .get(b"speech_recognition_trial".as_slice())
                .unwrap()
                .is_empty()
        );
        assert_eq!(manager.next_deadline(), Some(env.now + 1001.0));

        // Same values: no update.
        manager.on_update_trial_parameters(&mut env, -1, 300, 2000);
        assert_eq!(env.updates.len(), 1);

        // A cooldown of 0 keeps the old reset date; the weekly number grows
        // without touching the tries (they restart only at the reset date).
        manager.on_update_trial_parameters(&mut env, 5, 300, 0);
        let td_api::Update::updateSpeechRecognitionTrial(update) = &env.updates[1] else {
            panic!("speech recognition trial update");
        };
        assert_eq!(update.weekly_count, 5);
        assert_eq!(update.next_reset_date, 2000);
        assert_eq!(update.left_count, 0);
    }

    #[test]
    fn test_trial_parameters_timeout() {
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();
        manager.on_update_trial_parameters(&mut env, 5, 300, 2000);
        assert_eq!(env.updates.len(), 1);

        // Before the deadline nothing fires.
        env.now += 1000.0;
        manager.tick_timeouts(&mut env);
        assert_eq!(env.updates.len(), 1);

        // The fired timeout re-runs the clamps: the reset date has passed,
        // so the counter restarts, the update re-sends, the pmc re-saves
        // and the timeout cancels.
        env.unix_time = 2000;
        env.now += 1.0;
        manager.tick_timeouts(&mut env);
        assert_eq!(env.updates.len(), 2);
        let td_api::Update::updateSpeechRecognitionTrial(update) = &env.updates[1] else {
            panic!("speech recognition trial update");
        };
        assert_eq!(update.next_reset_date, 0);
        assert_eq!(update.left_count, 5);
        assert!(manager.next_deadline().is_none());
    }

    #[test]
    fn test_register_unregister_voice() {
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();
        env.messages.push(full_id(1, 1 << 21));

        // The plain server voice-note message registers into both maps.
        manager.register_voice(
            &env,
            FileId::new(3, 0),
            TranscriptionKind::VoiceNote,
            full_id(1, 1 << 21),
            "test",
        );
        assert!(manager.message_file_ids.contains_key(&full_id(1, 1 << 21)));

        // A second message over the same file joins the set (a clean
        // server id — bit 4 is the scheduled mark, not a counter).
        manager.register_voice(
            &env,
            FileId::new(3, 0),
            TranscriptionKind::VoiceNote,
            full_id(1, 3 << 21),
            "test",
        );
        assert_eq!(
            manager
                .voice_messages
                .get(&FileId::new(3, 0))
                .map(|ids| ids.len()),
            Some(2)
        );

        // Scheduled and non-server messages are skipped.
        manager.register_voice(
            &env,
            FileId::new(4, 0),
            TranscriptionKind::VoiceNote,
            full_id(1, (1 << 20) | 4), // scheduled
            "test",
        );
        manager.register_voice(
            &env,
            FileId::new(4, 0),
            TranscriptionKind::VoiceNote,
            full_id(1, (1 << 20) | 1), // yet unsent
            "test",
        );
        assert!(
            !manager
                .message_file_ids
                .contains_key(&full_id(1, (1 << 20) | 4))
        );
        assert!(
            !manager
                .message_file_ids
                .contains_key(&full_id(1, (1 << 20) | 1))
        );

        // Secret-chat messages are skipped.
        manager.register_voice(
            &env,
            FileId::new(4, 0),
            TranscriptionKind::VoiceNote,
            MessageFullId::new(
                DialogId::from_secret_chat_id(crate::user_manager::SecretChatId::new(1)),
                MessageId::new(1 << 21),
            ),
            "test",
        );
        assert!(!manager.message_file_ids.contains_key(&MessageFullId::new(
            DialogId::from_secret_chat_id(crate::user_manager::SecretChatId::new(1)),
            MessageId::new(1 << 21)
        )));

        // Bots are skipped (the C++ is_bot guard).
        env.bot = true;
        manager.register_voice(
            &env,
            FileId::new(4, 0),
            TranscriptionKind::VoiceNote,
            full_id(1, 1 << 22),
            "test",
        );
        assert!(!manager.message_file_ids.contains_key(&full_id(1, 1 << 22)));
        env.bot = false;

        // The unregister cleans both maps; the last registration removes
        // the file row entirely.
        manager.unregister_voice(&env, FileId::new(3, 0), full_id(1, 1 << 21), "test");
        assert_eq!(
            manager
                .voice_messages
                .get(&FileId::new(3, 0))
                .map(|ids| ids.len()),
            Some(1)
        );
        manager.unregister_voice(&env, FileId::new(3, 0), full_id(1, 3 << 21), "test");
        assert!(manager.voice_messages.is_empty());
        assert!(manager.message_file_ids.is_empty());

        // The double unregister is the LOG_CHECK crash.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let env = TestTranscriptionEnv::new();
            let mut manager = TranscriptionManager::default();
            manager.register_voice(
                &env,
                FileId::new(3, 0),
                TranscriptionKind::VoiceNote,
                full_id(1, 1 << 21),
                "test",
            );
            manager.unregister_voice(&env, FileId::new(3, 0), full_id(1, 1 << 21), "test");
            manager.unregister_voice(&env, FileId::new(3, 0), full_id(1, 1 << 21), "test");
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_recognize_speech_offline_bands() {
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();

        // An unknown message: the C++ "Message not found" band.
        let ((ok_flag, error_flag), promise) = FlagPromise::<()>::create();
        manager.recognize_speech(&mut env, full_id(1, 1 << 21), promise);
        assert!(!ok_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(error_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(env.queries.is_empty());

        // A registered message without a voice file: "Message can't be
        // transcribed".
        env.messages.push(full_id(1, 1 << 21));
        let ((ok_flag, error_flag), promise) = FlagPromise::<()>::create();
        manager.recognize_speech(&mut env, full_id(1, 1 << 21), promise);
        assert!(!ok_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(error_flag.load(std::sync::atomic::Ordering::SeqCst));

        // A registered voice message with a finished transcription answers
        // ok immediately, without a query.
        env.register_info(3, transcribed_info("done"));
        manager.register_voice(
            &env,
            FileId::new(3, 0),
            TranscriptionKind::VoiceNote,
            full_id(1, 1 << 21),
            "test",
        );
        let ((ok_flag, _), promise) = FlagPromise::<()>::create();
        manager.recognize_speech(&mut env, full_id(1, 1 << 21), promise);
        assert!(ok_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(env.queries.is_empty());
    }

    #[test]
    fn test_recognize_speech_parks_and_sends_once() {
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();
        env.messages.push(full_id(1, 1 << 21));
        env.register_info(3, TranscriptionInfo::default());
        manager.register_voice(
            &env,
            FileId::new(3, 0),
            TranscriptionKind::VoiceNote,
            full_id(1, 1 << 21),
            "test",
        );

        // The first request parks and sends the query; the second only
        // parks (the C++ single-query fan-in).
        let (_, promise) = FlagPromise::<()>::create();
        manager.recognize_speech(&mut env, full_id(1, 1 << 21), promise);
        assert_eq!(env.queries, vec!["transcribeAudio"]);
        let (_, promise) = FlagPromise::<()>::create();
        manager.recognize_speech(&mut env, full_id(1, 1 << 21), promise);
        assert_eq!(env.queries.len(), 1);

        // The initial partial answer subscribes the id for 60 seconds.
        manager.on_transcribed_audio(
            &mut env,
            voice_file_info(3),
            tdutils::status::Result::Ok(initial_pending_audio(77, "par")),
        );
        assert!(manager.pending_audio_transcriptions.contains_key(&77));
        assert_eq!(
            manager.pending_audio_transcription_timeouts.get(&77),
            Some(&(env.now + AUDIO_TRANSCRIPTION_TIMEOUT))
        );

        // The follow-up partial rides the subscription; meanwhile the rate
        // request for the still-untranscribed content answers ok without a
        // query.
        manager.on_update_transcribed_audio(
            &mut env,
            telegram_api::updateTranscribedAudio {
                flags: 1,
                pending: true,
                transcription_id: 77,
                text: "partial".to_string(),
                ..Default::default()
            },
        );
        assert!(manager.pending_audio_transcriptions.contains_key(&77));
        let ((ok_flag, _), promise) = FlagPromise::<()>::create();
        manager.rate_speech_recognition(&mut env, full_id(1, 1 << 21), true, promise);
        assert!(ok_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(env.queries.len(), 1);

        // The final answer completes the subscription, the parked requests
        // have already been answered ok and the info carries the text.
        manager.on_update_transcribed_audio(
            &mut env,
            telegram_api::updateTranscribedAudio {
                flags: 0,
                pending: false,
                transcription_id: 77,
                text: "partial text".to_string(),
                ..Default::default()
            },
        );
        assert!(!manager.pending_audio_transcriptions.contains_key(&77));
        assert_eq!(
            env.infos.get(&3).map(|info| info.is_transcribed()),
            Some(true)
        );

        // A further recognize on the finished content answers immediately.
        let ((ok_flag, _), promise) = FlagPromise::<()>::create();
        manager.recognize_speech(&mut env, full_id(1, 1 << 21), promise);
        assert!(ok_flag.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn test_pending_transcription_expiry() {
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();
        env.messages.push(full_id(1, 1 << 21));
        env.register_info(3, TranscriptionInfo::default());
        manager.register_voice(
            &env,
            FileId::new(3, 0),
            TranscriptionKind::VoiceNote,
            full_id(1, 1 << 21),
            "test",
        );
        let (_, promise) = FlagPromise::<()>::create();
        manager.recognize_speech(&mut env, full_id(1, 1 << 21), promise);
        manager.on_transcribed_audio(
            &mut env,
            voice_file_info(3),
            tdutils::status::Result::Ok(initial_pending_audio(77, "par")),
        );
        assert!(manager.next_deadline().is_some());

        // Before the deadline nothing fires.
        env.now += AUDIO_TRANSCRIPTION_TIMEOUT - 1.0;
        manager.tick_timeouts(&mut env);
        assert!(manager.pending_audio_transcriptions.contains_key(&77));

        // At the deadline the subscription fails with the timeout status
        // and the info carries the error.
        env.now += 1.0;
        manager.tick_timeouts(&mut env);
        assert!(!manager.pending_audio_transcriptions.contains_key(&77));
        assert!(manager.next_deadline().is_none());
        let info = env.infos.get(&3).unwrap();
        let Some(td_api::SpeechRecognitionResult::speechRecognitionResultError(error)) =
            info.get_speech_recognition_result_object()
        else {
            panic!("error result");
        };
        let error = error.error.expect("error object");
        assert_eq!(error.code, 500);
        assert_eq!(error.message, "Timeout expired");

        // An unauthorized tick drops only the deadline; the subscription
        // stays (the C++ callback returns before the failure routing).
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();
        env.messages.push(full_id(1, 1 << 21));
        env.register_info(3, TranscriptionInfo::default());
        manager.register_voice(
            &env,
            FileId::new(3, 0),
            TranscriptionKind::VoiceNote,
            full_id(1, 1 << 21),
            "test",
        );
        let (_, promise) = FlagPromise::<()>::create();
        manager.recognize_speech(&mut env, full_id(1, 1 << 21), promise);
        manager.on_transcribed_audio(
            &mut env,
            voice_file_info(3),
            tdutils::status::Result::Ok(initial_pending_audio(88, "")),
        );
        env.authorized = false;
        env.now += AUDIO_TRANSCRIPTION_TIMEOUT + 1.0;
        manager.tick_timeouts(&mut env);
        assert!(manager.pending_audio_transcriptions.contains_key(&88));
        assert!(manager.next_deadline().is_none());
    }

    #[test]
    fn test_rate_speech_recognition() {
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();
        env.messages.push(full_id(1, 1 << 21));

        // No voice file: "Message can't be transcribed".
        let ((_, error_flag), promise) = FlagPromise::<()>::create();
        manager.rate_speech_recognition(&mut env, full_id(1, 1 << 21), true, promise);
        assert!(error_flag.load(std::sync::atomic::Ordering::SeqCst));

        // The untranscribed content answers ok without a query.
        env.register_info(3, TranscriptionInfo::default());
        manager.register_voice(
            &env,
            FileId::new(3, 0),
            TranscriptionKind::VoiceNote,
            full_id(1, 1 << 21),
            "test",
        );
        let ((ok_flag, _), promise) = FlagPromise::<()>::create();
        manager.rate_speech_recognition(&mut env, full_id(1, 1 << 21), true, promise);
        assert!(ok_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(env.queries.is_empty());

        // The transcribed content sends the rate query with its id (the
        // promise parks in the family lot until the answer).
        env.register_info(3, transcribed_info("done"));
        let (_, promise) = FlagPromise::<()>::create();
        manager.rate_speech_recognition(&mut env, full_id(1, 1 << 21), false, promise);
        assert_eq!(env.queries, vec!["rateTranscribedAudio"]);
        assert!(manager.pop_rate_promise().is_some(), "the promise parked");

        // The [blocked: M.28] missing-info seam: no stored info, no crash,
        // no query.
        env.infos.clear();
        let (_, promise) = FlagPromise::<()>::create();
        manager.rate_speech_recognition(&mut env, full_id(1, 1 << 21), true, promise);
        assert_eq!(env.queries.len(), 1);
    }

    #[test]
    fn test_rate_promise_lot_is_fifo() {
        // `RateTranscribedAudioQuery` carries the request promise on the
        // query (`promise_`, TranscriptionInfo.cpp:57-62); the send
        // wrapper parks it in the manager's lot and the answer pops the
        // head — two parks pair with two answers in order, and a bare
        // answer (no park) settles nothing (§5's FIFO-per-family shape).
        let mut manager = TranscriptionManager::default();
        let mut env = TestTranscriptionEnv::new();
        assert!(manager.pop_rate_promise().is_none());

        let ((first_ok, first_error), promise) = FlagPromise::<()>::create();
        manager.send_rate_transcribed_audio_query(&mut env, full_id(1, 1 << 21), 77, true, promise);
        let ((second_ok, second_error), promise) = FlagPromise::<()>::create();
        manager.send_rate_transcribed_audio_query(
            &mut env,
            full_id(1, 1 << 21),
            78,
            false,
            promise,
        );
        assert_eq!(env.queries, vec!["rateTranscribedAudio"; 2]);

        let mut head = manager.pop_rate_promise().expect("the first park");
        head.set_value(());
        assert!(first_ok.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!first_error.load(std::sync::atomic::Ordering::SeqCst));

        let mut head = manager.pop_rate_promise().expect("the second park");
        head.set_error(Status::error(400, "VOICE_MESSAGES_FORBIDDEN"));
        assert!(!second_ok.load(std::sync::atomic::Ordering::SeqCst));
        assert!(second_error.load(std::sync::atomic::Ordering::SeqCst));

        assert!(manager.pop_rate_promise().is_none(), "the lot drained");
    }

    #[test]
    fn test_recognize_speech_in_file_bands() {
        // The input-file resolution failure answers its error before any
        // park (TranscriptionManager.cpp:482-484).
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();
        env.input_file_id_result =
            tdutils::status::Result::Err(Status::error(400, "File not found"));
        let ((ok_flag, error_flag), promise) = FlagPromise::<td_api::Text>::create();
        manager.recognize_speech_in_file(&mut env, None, 7, promise);
        assert!(!ok_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(error_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(manager.audio_file_transcriptions.is_empty());
        assert!(env.uploads.is_empty());
        assert!(env.deleted_partial.is_empty());

        // The encrypted band finishes through the shared completion: the
        // partial remote location drops even on the early exit (the
        // canned view type is not a speech file, so no file delete).
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();
        env.view_is_encrypted = true;
        let ((ok_flag, error_flag), promise) = FlagPromise::<td_api::Text>::create();
        manager.recognize_speech_in_file(&mut env, None, 7, promise);
        assert!(!ok_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(error_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(manager.audio_file_transcriptions.is_empty());
        assert_eq!(env.deleted_partial.len(), 1);
        assert!(env.deleted_files.is_empty());
        assert!(env.uploads.is_empty());

        // The missing-upload-location band.
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();
        env.view_has_upload_location = false;
        let ((ok_flag, error_flag), promise) = FlagPromise::<td_api::Text>::create();
        manager.recognize_speech_in_file(&mut env, None, 0, promise);
        assert!(!ok_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(error_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(env.deleted_partial.len(), 1);
        assert!(env.uploads.is_empty());
    }

    #[test]
    fn test_recognize_speech_in_file_happy_path() {
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();
        env.view_file_types
            .insert(3, FileType::SpeechRecognitionAudio);
        let file_upload_id = FileUploadId::new(FileId::new(3, 0), 1);
        let ((_, error_flag), promise) = FlagPromise::<td_api::Text>::create();
        manager.recognize_speech_in_file(&mut env, None, 7, promise);
        assert!(
            manager
                .audio_file_transcriptions
                .contains_key(&file_upload_id)
        );
        assert_eq!(env.uploads, vec![file_upload_id]);

        // The upload ok hop without an InputFile: the reupload band
        // fails the request (TranscriptionManager.cpp:527-529).
        manager.on_upload_audio_file(&mut env, file_upload_id, None);
        assert!(error_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(manager.audio_file_transcriptions.is_empty());
        assert_eq!(env.deleted_partial, vec![file_upload_id]);
        // the speech file deletes at the finish
        assert_eq!(env.deleted_files, vec![FileId::new(3, 0)]);

        // A full pass: the upload ok, the final answer, the text object.
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();
        env.view_file_types
            .insert(3, FileType::SpeechRecognitionAudio);
        let ((ok_flag, error_flag), promise) = FlagPromise::<td_api::Text>::create();
        manager.recognize_speech_in_file(&mut env, None, 7, promise);
        manager.on_upload_audio_file(
            &mut env,
            file_upload_id,
            Some(telegram_api::InputFile::inputFile(
                telegram_api::inputFile {
                    id: 1,
                    parts: 1,
                    name: String::new(),
                    md5_checksum: String::new(),
                },
            )),
        );
        assert_eq!(env.queries, vec!["uploadAudio", "transcribeAudioFile"]);
        manager.on_transcribed_audio_file(
            &mut env,
            file_upload_id,
            tdutils::status::Result::Ok(telegram_api::messages_TranscribedAudio {
                flags: 0,
                pending: false,
                transcription_id: 9,
                text: "hello".to_string(),
                trial_remains_num: 0,
                trial_remains_until_date: 0,
            }),
        );
        assert!(ok_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!error_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(manager.audio_file_transcriptions.is_empty());
        assert_eq!(env.deleted_partial, vec![file_upload_id]);
        assert_eq!(env.deleted_files, vec![FileId::new(3, 0)]);

        // A non-speech view type skips the file delete.
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();
        let ((ok_flag, _), promise) = FlagPromise::<td_api::Text>::create();
        manager.recognize_speech_in_file(&mut env, None, 0, promise);
        manager.on_transcribed_audio_file(
            &mut env,
            file_upload_id,
            tdutils::status::Result::Ok(telegram_api::messages_TranscribedAudio {
                flags: 0,
                pending: false,
                transcription_id: 9,
                text: String::new(),
                trial_remains_num: 0,
                trial_remains_until_date: 0,
            }),
        );
        assert!(ok_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(env.deleted_partial, vec![file_upload_id]);
        assert!(env.deleted_files.is_empty());
    }

    #[test]
    fn test_recognize_speech_in_file_partial_flow() {
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();
        let file_upload_id = FileUploadId::new(FileId::new(3, 0), 1);
        let ((ok_flag, error_flag), promise) = FlagPromise::<td_api::Text>::create();
        manager.recognize_speech_in_file(&mut env, None, 7, promise);
        manager.on_upload_audio_file(
            &mut env,
            file_upload_id,
            Some(telegram_api::InputFile::inputFile(
                telegram_api::inputFile {
                    id: 1,
                    parts: 1,
                    name: String::new(),
                    md5_checksum: String::new(),
                },
            )),
        );

        // The pending answer arms the subscription and emits the partial
        // update for the nonzero recognition id.
        manager.on_transcribed_audio_file(
            &mut env,
            file_upload_id,
            tdutils::status::Result::Ok(telegram_api::messages_TranscribedAudio {
                flags: 1,
                pending: true,
                transcription_id: 9,
                text: "par".to_string(),
                trial_remains_num: 0,
                trial_remains_until_date: 0,
            }),
        );
        assert!(manager.pending_audio_transcriptions.contains_key(&9));
        assert_eq!(
            manager.pending_audio_transcription_timeouts.get(&9),
            Some(&(env.now + AUDIO_TRANSCRIPTION_TIMEOUT))
        );
        assert_eq!(env.updates.len(), 1);
        let td_api::Update::updateSpeechRecognitionInFile(update) = &env.updates[0] else {
            panic!("speech recognition in file update");
        };
        assert_eq!(update.recognition_id, 7);
        assert_eq!(update.partial_text, "par");

        // The follow-up pending update rides the subscription.
        manager.on_update_transcribed_audio(
            &mut env,
            telegram_api::updateTranscribedAudio {
                flags: 1,
                pending: true,
                transcription_id: 9,
                text: "partial".to_string(),
                ..Default::default()
            },
        );
        assert_eq!(env.updates.len(), 2);

        // The final update finishes through the subscription: the
        // request answers the text, the cleanup runs.
        manager.on_update_transcribed_audio(
            &mut env,
            telegram_api::updateTranscribedAudio {
                flags: 0,
                pending: false,
                transcription_id: 9,
                text: "done".to_string(),
                ..Default::default()
            },
        );
        assert!(!manager.pending_audio_transcriptions.contains_key(&9));
        assert!(ok_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!error_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(manager.audio_file_transcriptions.is_empty());
        assert_eq!(env.deleted_partial, vec![file_upload_id]);
        assert_eq!(env.updates.len(), 2);

        // recognition_id 0 suppresses the partial updates (the request
        // only wants the final text).
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();
        let ((ok_flag, error_flag), promise) = FlagPromise::<td_api::Text>::create();
        manager.recognize_speech_in_file(&mut env, None, 0, promise);
        manager.on_transcribed_audio_file(
            &mut env,
            file_upload_id,
            tdutils::status::Result::Ok(telegram_api::messages_TranscribedAudio {
                flags: 1,
                pending: true,
                transcription_id: 9,
                text: "par".to_string(),
                trial_remains_num: 0,
                trial_remains_until_date: 0,
            }),
        );
        assert!(manager.pending_audio_transcriptions.contains_key(&9));
        assert!(env.updates.is_empty());

        // The subscription timeout fails the request through the file
        // continuation (the shared failed intake).
        env.now += AUDIO_TRANSCRIPTION_TIMEOUT + 1.0;
        manager.tick_timeouts(&mut env);
        assert!(!manager.pending_audio_transcriptions.contains_key(&9));
        assert!(!ok_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(error_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(manager.audio_file_transcriptions.is_empty());
        assert_eq!(env.deleted_partial.len(), 1);
    }

    #[test]
    fn test_get_speech_recognition_languages() {
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();

        // The dialog-access failure answers before the park (the C++
        // TRY_STATUS_PROMISE head).
        env.dialog_access_error = Some(Status::error(400, "Chat not found"));
        let ((ok_flag, error_flag), promise) =
            FlagPromise::<td_api::SpeechRecognitionLanguages>::create();
        manager.get_speech_recognition_languages(&mut env, DialogId::new(1), promise);
        assert!(error_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!ok_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(env.queries.is_empty());
        assert!(manager.pop_langs_promise().is_none());

        // dialog_id 0 skips the access check and parks the promise.
        let ((ok_flag, error_flag), promise) =
            FlagPromise::<td_api::SpeechRecognitionLanguages>::create();
        manager.get_speech_recognition_languages(&mut env, DialogId::default(), promise);
        assert_eq!(env.queries, vec!["getTranscriptionLangs"]);

        // A second request parks behind the first (the FIFO family lot).
        env.dialog_access_error = None;
        let ((second_ok, second_error), promise) =
            FlagPromise::<td_api::SpeechRecognitionLanguages>::create();
        manager.get_speech_recognition_languages(&mut env, DialogId::new(2), promise);
        assert_eq!(env.queries, vec!["getTranscriptionLangs"; 2]);

        let mut head = manager.pop_langs_promise().expect("the first park");
        head.set_value(td_api::SpeechRecognitionLanguages {
            language_codes: vec!["en".to_string()],
        });
        assert!(ok_flag.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!error_flag.load(std::sync::atomic::Ordering::SeqCst));

        let mut head = manager.pop_langs_promise().expect("the second park");
        head.set_error(Status::error(500, "Timeout expired"));
        assert!(!second_ok.load(std::sync::atomic::Ordering::SeqCst));
        assert!(second_error.load(std::sync::atomic::Ordering::SeqCst));

        assert!(manager.pop_langs_promise().is_none(), "the lot drained");
    }

    #[test]
    fn test_on_transcribed_audio_trial_table() {
        // The `flags.1` pair lands the trial numbers, clamped through
        // update_left_tries at the standing weekly number. The answer
        // flows through the final-transcription path first, so the info
        // must carry a pending query (the C++
        // CHECK(!speech_recognition_queries_.empty()) precondition): the
        // recognize step parks it.
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();
        env.messages.push(full_id(1, 1 << 21));
        env.register_info(3, TranscriptionInfo::default());
        manager.register_voice(
            &env,
            FileId::new(3, 0),
            TranscriptionKind::VoiceNote,
            full_id(1, 1 << 21),
            "test",
        );
        let (_, promise) = FlagPromise::<()>::create();
        manager.recognize_speech(&mut env, full_id(1, 1 << 21), promise);
        manager.on_update_trial_parameters(&mut env, 10, 300, 0);
        // Restart the counter so the carried tries are 10.
        env.unix_time += 1;
        env.now += 1.0;
        manager.tick_timeouts(&mut env);
        assert_eq!(manager.trial_parameters.left_tries, 10);

        manager.on_transcribed_audio(
            &mut env,
            voice_file_info(3),
            tdutils::status::Result::Ok(telegram_api::messages_TranscribedAudio {
                flags: 1 << 1,
                pending: false,
                transcription_id: 7,
                text: String::new(),
                trial_remains_num: 7,
                trial_remains_until_date: 5555,
            }),
        );
        let td_api::Update::updateSpeechRecognitionTrial(update) = &env.updates[1] else {
            panic!("speech recognition trial update");
        };
        assert_eq!(update.left_count, 7);
        assert_eq!(update.next_reset_date, 5555);

        // A zero transcription id fails the initial answer.
        let mut env = TestTranscriptionEnv::new();
        env.messages.push(full_id(1, 1 << 21));
        env.register_info(3, TranscriptionInfo::default());
        let mut manager = TranscriptionManager::default();
        manager.register_voice(
            &env,
            FileId::new(3, 0),
            TranscriptionKind::VoiceNote,
            full_id(1, 1 << 21),
            "test",
        );
        let (_, promise) = FlagPromise::<()>::create();
        manager.recognize_speech(&mut env, full_id(1, 1 << 21), promise);
        manager.on_transcribed_audio(
            &mut env,
            voice_file_info(3),
            tdutils::status::Result::Ok(telegram_api::messages_TranscribedAudio {
                flags: 0,
                pending: false,
                transcription_id: 0,
                text: "x".to_string(),
                trial_remains_num: 0,
                trial_remains_until_date: 0,
            }),
        );
        let info = env.infos.get(&3).unwrap();
        let Some(td_api::SpeechRecognitionResult::speechRecognitionResultError(error)) =
            info.get_speech_recognition_result_object()
        else {
            panic!("error result");
        };
        assert_eq!(
            error.error.expect("error object").message,
            "Receive no transcription identifier"
        );
    }

    #[test]
    fn test_on_transcribed_audio_429_resets_left_tries() {
        let mut env = TestTranscriptionEnv::new();
        let mut manager = TranscriptionManager::default();
        manager.on_update_trial_parameters(&mut env, 5, 300, 2000);
        assert_eq!(env.updates.len(), 1);
        assert_eq!(manager.trial_parameters.left_tries, 0);

        // The reset passes, the counter restarts at the weekly number.
        env.unix_time = 2000;
        env.now += 1001.0;
        manager.tick_timeouts(&mut env);
        assert_eq!(manager.trial_parameters.left_tries, 5);

        // The 429 band with left tries zeroes them and moves the reset
        // date (TranscriptionManager.cpp:289-298).
        env.messages.push(full_id(1, 1 << 21));
        env.register_info(3, TranscriptionInfo::default());
        manager.register_voice(
            &env,
            FileId::new(3, 0),
            TranscriptionKind::VoiceNote,
            full_id(1, 1 << 21),
            "test",
        );
        let (_, promise) = FlagPromise::<()>::create();
        manager.recognize_speech(&mut env, full_id(1, 1 << 21), promise);
        manager.on_transcribed_audio(
            &mut env,
            voice_file_info(3),
            tdutils::status::Result::Err(Status::error(429, "Too Many Requests: retry after 42")),
        );
        assert_eq!(env.updates.len(), 3);
        let td_api::Update::updateSpeechRecognitionTrial(update) = &env.updates[2] else {
            panic!("speech recognition trial update");
        };
        assert_eq!(update.left_count, 0);
        assert_eq!(update.next_reset_date, env.unix_time + 42);

        // A non-429 error does not touch the trial state; the parked
        // requests answer ok with the error carried by the info.
        let (_, promise) = FlagPromise::<()>::create();
        manager.recognize_speech(&mut env, full_id(1, 1 << 21), promise);
        manager.on_transcribed_audio(
            &mut env,
            voice_file_info(3),
            tdutils::status::Result::Err(Status::error(400, "CHAT_ID_INVALID")),
        );
        assert_eq!(env.updates.len(), 3);
        let info = env.infos.get(&3).unwrap();
        let Some(td_api::SpeechRecognitionResult::speechRecognitionResultError(error)) =
            info.get_speech_recognition_result_object()
        else {
            panic!("error result");
        };
        assert_eq!(
            error.error.expect("error object").message,
            "CHAT_ID_INVALID"
        );
    }

    #[test]
    fn test_get_retry_after() {
        assert_eq!(
            get_retry_after(429, "Too Many Requests: retry after 12"),
            12
        );
        assert_eq!(get_retry_after(400, "Too Many Requests: retry after 12"), 0);
        assert_eq!(get_retry_after(429, "Other"), 0);
        assert_eq!(get_retry_after(429, "Too Many Requests: retry after -3"), 0);
        assert_eq!(get_retry_after(429, "Too Many Requests: retry after x"), 0);
    }

    #[test]
    fn test_transcription_info_machine() {
        let mut info = TranscriptionInfo::default();

        // The first request starts the query, further ones only park.
        let ((a, _), promise) = FlagPromise::<()>::create();
        assert!(info.recognize_speech(promise));
        let ((b, _), promise) = FlagPromise::<()>::create();
        assert!(!info.recognize_speech(promise));
        let ((c, _), promise) = FlagPromise::<()>::create();
        assert!(!info.recognize_speech(promise));
        assert!(!a.load(std::sync::atomic::Ordering::SeqCst));

        // The partial result tracks the text without completing anything.
        assert!(info.on_partial_transcription("par".to_string(), 9));
        assert!(!info.on_partial_transcription("par".to_string(), 9));
        assert_eq!(
            info.get_speech_recognition_result_object(),
            Some(
                td_api::SpeechRecognitionResult::speechRecognitionResultPending(
                    td_api::speechRecognitionResultPending {
                        partial_text: "par".to_string()
                    }
                )
            )
        );

        // The failure keeps the promises' ok answers and stores the error.
        let promises = info.on_failed_transcription(Status::error(400, "CHAT_ID_INVALID"));
        assert_eq!(promises.len(), 3);
        set_promises(promises);
        assert!(a.load(std::sync::atomic::Ordering::SeqCst));
        assert!(b.load(std::sync::atomic::Ordering::SeqCst));
        assert!(c.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            info.get_speech_recognition_result_object(),
            Some(
                td_api::SpeechRecognitionResult::speechRecognitionResultError(
                    td_api::speechRecognitionResultError {
                        error: Some(td_api::Error {
                            code: 400,
                            message: "CHAT_ID_INVALID".to_string()
                        })
                    }
                )
            )
        );

        // The next request retries after the failure.
        let ((_, _), promise) = FlagPromise::<()>::create();
        assert!(info.recognize_speech(promise));

        // The final transcription completes everything.
        let promises = info.on_final_transcription("text".to_string(), 10);
        assert_eq!(promises.len(), 1);
        set_promises(promises);
        assert!(info.is_transcribed());
        assert_eq!(
            info.get_speech_recognition_result_object(),
            Some(
                td_api::SpeechRecognitionResult::speechRecognitionResultText(
                    td_api::speechRecognitionResultText {
                        text: "text".to_string()
                    }
                )
            )
        );

        // The finished transcription answers further requests immediately.
        let ((e, _), promise) = FlagPromise::<()>::create();
        assert!(!info.recognize_speech(promise));
        assert!(e.load(std::sync::atomic::Ordering::SeqCst));

        // And carries no result error.
        assert_eq!(
            info.rate_speech_recognition(Promise::default())
                .map(|(id, _)| id),
            Some(10)
        );
    }

    #[test]
    fn test_transcription_info_store_parse() {
        let info = transcribed_info("hello");
        let mut blob = Vec::new();
        info.store(&mut blob);
        let mut parser = crate::store_parse::Parser::new(&blob);
        let parsed = TranscriptionInfo::parse(&mut parser).unwrap();
        assert!(parsed.is_transcribed());
        assert_eq!(parsed.transcription_id, 77);
        assert_eq!(parsed.text, "hello");

        // An unfinished transcription cannot be stored (the C++ CHECK).
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut blob = Vec::new();
            TranscriptionInfo::default().store(&mut blob);
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_transcription_info_copy_and_update_from() {
        // copy_if_transcribed: only a finished transcription copies.
        assert!(TranscriptionInfo::copy_if_transcribed(None).is_none());
        assert!(
            TranscriptionInfo::copy_if_transcribed(Some(&TranscriptionInfo::default())).is_none()
        );
        let copy = TranscriptionInfo::copy_if_transcribed(Some(&transcribed_info("hello")))
            .expect("copied");
        assert!(copy.is_transcribed());
        assert_eq!(copy.transcription_id, 77);
        assert_eq!(copy.text, "hello");

        // update_from: the empty slot adopts, a `None` is a no-op, an
        // unfinished new info is a no-op.
        let mut old_info = None;
        assert!(TranscriptionInfo::update_from(
            &mut old_info,
            Some(transcribed_info("a"))
        ));
        assert!(!TranscriptionInfo::update_from(&mut old_info, None));
        assert!(!TranscriptionInfo::update_from(
            &mut old_info,
            Some(TranscriptionInfo::default())
        ));

        // A live slot with a transcription id keeps its info.
        let mut old_info = Some(transcribed_info("live"));
        assert!(!TranscriptionInfo::update_from(
            &mut old_info,
            Some(transcribed_info("new"))
        ));
        assert_eq!(old_info.expect("live").text, "live");

        // A live slot with parked queries keeps its info too.
        let mut old_info = Some(TranscriptionInfo::default());
        {
            let (_, promise) = FlagPromise::<()>::create();
            old_info
                .as_mut()
                .unwrap()
                .speech_recognition_queries
                .push(promise);
        }
        assert!(!TranscriptionInfo::update_from(
            &mut old_info,
            Some(transcribed_info("parsed"))
        ));
        assert_eq!(old_info.unwrap().speech_recognition_queries.len(), 1);

        // An empty live slot adopts the parsed one.
        let mut old_info = Some(TranscriptionInfo::default());
        assert!(TranscriptionInfo::update_from(
            &mut old_info,
            Some(transcribed_info("parsed"))
        ));
        assert_eq!(old_info.unwrap().text, "parsed");
    }

    #[test]
    fn test_get_current_state_gate() {
        let mut env = TestTranscriptionEnv::new();
        let manager = TranscriptionManager::default();

        // Bots and unauthorized sessions see no trial update.
        env.bot = true;
        let mut updates = Vec::new();
        manager.get_current_state(&env, &mut updates);
        assert!(updates.is_empty());

        env.bot = false;
        env.authorized = false;
        let mut updates = Vec::new();
        manager.get_current_state(&env, &mut updates);
        assert!(updates.is_empty());

        env.authorized = true;
        let mut updates = Vec::new();
        manager.get_current_state(&env, &mut updates);
        assert_eq!(updates.len(), 1);
        assert!(matches!(
            updates[0],
            td_api::Update::updateSpeechRecognitionTrial(_)
        ));
    }
}
