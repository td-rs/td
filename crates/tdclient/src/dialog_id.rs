//! The dialog/message id tail (the M.6 slice's `DialogId.{h,cpp}`,
//! `MessageFullId.h`, `ServerMessageId.h`, `ScheduledServerMessageId.h`,
//! `NotificationId.h`, `NotificationGroupId.h`, `FolderId.h` and
//! `MessageSearchFilter.{h,cpp}` surface).
//!
//! Since the M.10 union these types are defined once, in
//! [`crate::messages_manager`] (the full M.10.1 id-space port, with the
//! C++-probe-pinned `DialogId::get_type` sentinel holes and the
//! three-space `MessageId`); this module is a compatibility re-export so
//! the M.6-line import paths stay stable.
//!
//! `MessageId` also lives in [`crate::messages_manager`]; the M.5-era
//! `chat_manager` stand-in remains only for that module's internal use on
//! this line.

pub use crate::messages_manager::{
    DialogId, DialogType, FolderId, MessageFullId, MessageSearchFilter, NotificationGroupId,
    NotificationId, ScheduledServerMessageId, ServerMessageId, call_message_search_filter_index,
    message_search_filter_count, message_search_filter_index, message_search_filter_index_mask,
};
