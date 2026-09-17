//! The M.4.15-era id subset (`DialogId.h`, `MessageId.h`, `MessageFullId.h`).
//!
//! Since the M.10 union these types are defined once, in
//! [`crate::messages_manager`] (the full M.10.1 id-space port — the full
//! three-space `MessageId` layout, the C++-probe-pinned `DialogId`
//! band table and the `telegram_api` conversions); this module is a
//! compatibility re-export so the M.4/M.6-line import paths stay stable.
//! (`ChannelId` stays in [`crate::chat_manager`], where the C++
//! ChannelId.h port landed with M.5.)

pub use crate::messages_manager::{DialogId, DialogType, MessageFullId, MessageId};
