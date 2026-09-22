//! `td::StoryNotificationSettings` (StoryNotificationSettings.h, M.47) —
//! the immutable settings snapshot the notification-settings plane hands to
//! the story consumers. The C++ members are `const` with non-zero-default
//! booleans (`false`), so the derived default matches.

/// `td::StoryNotificationSettings` (StoryNotificationSettings.h:13-31).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoryNotificationSettings {
    /// `need_dialog_settings_` (StoryNotificationSettings.h:15).
    pub need_dialog_settings: bool,
    /// `need_top_dialogs_` (StoryNotificationSettings.h:16).
    pub need_top_dialogs: bool,
    /// `are_muted_` (StoryNotificationSettings.h:17).
    pub are_muted: bool,
    /// `hide_sender_` (StoryNotificationSettings.h:18).
    pub hide_sender: bool,
    /// `ringtone_id_` (StoryNotificationSettings.h:19).
    pub ringtone_id: i64,
}

impl StoryNotificationSettings {
    /// The C++ five-argument constructor (StoryNotificationSettings.h:21-28).
    pub fn new(
        need_dialog_settings: bool,
        need_top_dialogs: bool,
        are_muted: bool,
        hide_sender: bool,
        ringtone_id: i64,
    ) -> Self {
        StoryNotificationSettings {
            need_dialog_settings,
            need_top_dialogs,
            are_muted,
            hide_sender,
            ringtone_id,
        }
    }
}
