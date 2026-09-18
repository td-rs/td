//! The notification-settings value types (M.13S.1): `NotificationSound`
//! (NotificationSound.cpp/h), `NotificationSettingsScope`
//! (NotificationSettingsScope.cpp/h), `ScopeNotificationSettings`
//! (ScopeNotificationSettings.cpp/hpp), `ReactionNotificationsFrom`
//! (ReactionNotificationsFrom.cpp/hpp) and `ReactionNotificationSettings`
//! (ReactionNotificationSettings.cpp/hpp).
//!
//! The C++ `unique_ptr<NotificationSound>` "default" marker is `None` of
//! `Option<NotificationSound>`; every store/parse keeps the C++ byte
//! format (the pmc blobs must stay readable by both implementations), so
//! the flag blocks and the empty flag-block placeholders are reproduced
//! verbatim. `DialogNotificationSettings` stays with MessagesManager
//! (M.10) — the settings manager on this line holds no dialog state.

use crate::store_parse::{Parser, store_i32, store_i64, store_string, store_u32};
use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::status::Status;

/// `NotificationSoundType` (NotificationSoundType.h) — the stored
/// discriminant of a [`NotificationSound`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationSoundType {
    None,
    Local,
    Ringtone,
}

impl NotificationSoundType {
    fn as_i32(self) -> i32 {
        match self {
            NotificationSoundType::None => 0,
            NotificationSoundType::Local => 1,
            NotificationSoundType::Ringtone => 2,
        }
    }

    fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(NotificationSoundType::None),
            1 => Some(NotificationSoundType::Local),
            2 => Some(NotificationSoundType::Ringtone),
            _ => None,
        }
    }
}

/// `NotificationSound` (NotificationSound.h) — the "default" sound is the
/// absent [`Option`] value (`nullptr` in the C++).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotificationSound {
    /// `NotificationSoundNone`.
    NoSound,
    /// `NotificationSoundLocal` — a client-local sound, still "default"
    /// for the server-sync rules.
    Local { title: String, data: String },
    /// `NotificationSoundRingtone`.
    Ringtone { ringtone_id: i64 },
}

impl NotificationSound {
    fn get_type(&self) -> NotificationSoundType {
        match self {
            NotificationSound::NoSound => NotificationSoundType::None,
            NotificationSound::Local { .. } => NotificationSoundType::Local,
            NotificationSound::Ringtone { .. } => NotificationSoundType::Ringtone,
        }
    }

    /// `NotificationSound::store` — the type then the type-specific fields
    /// (NotificationSound.cpp:50-74).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_i32(out, self.get_type().as_i32());
        match self {
            NotificationSound::NoSound => {}
            NotificationSound::Local { title, data } => {
                store_string(out, title);
                store_string(out, data);
            }
            NotificationSound::Ringtone { ringtone_id } => {
                store_i64(out, *ringtone_id);
            }
        }
    }

    /// `parse_notification_sound` (NotificationSound.cpp:76-101); the
    /// unknown-discriminant `LOG(FATAL)` reads as a parse error here.
    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let sound_type = NotificationSoundType::from_i32(parser.i32_()?)
            .ok_or("Have unknown notification sound type")?;
        match sound_type {
            NotificationSoundType::None => Ok(NotificationSound::NoSound),
            NotificationSoundType::Local => {
                let title = parser.string()?;
                let data = parser.string()?;
                Ok(NotificationSound::Local { title, data })
            }
            NotificationSoundType::Ringtone => {
                let ringtone_id = parser.i64_()?;
                Ok(NotificationSound::Ringtone { ringtone_id })
            }
        }
    }
}

/// `is_notification_sound_default` (NotificationSound.cpp:136-141) — null
/// and `Local` sounds both count as "default".
pub fn is_notification_sound_default(notification_sound: &Option<NotificationSound>) -> bool {
    match notification_sound {
        None => true,
        Some(sound) => sound.get_type() == NotificationSoundType::Local,
    }
}

/// `are_equivalent_notification_sounds` (NotificationSound.cpp:143-169).
pub fn are_equivalent_notification_sounds(
    lhs: &Option<NotificationSound>,
    rhs: &Option<NotificationSound>,
) -> bool {
    if is_notification_sound_default(lhs) {
        return is_notification_sound_default(rhs);
    }
    if is_notification_sound_default(rhs) {
        return false;
    }
    let lhs_sound = lhs.as_ref().expect("non-default sound checked above");
    let rhs_sound = rhs.as_ref().expect("non-default sound checked above");
    match (lhs_sound, rhs_sound) {
        (NotificationSound::NoSound, NotificationSound::NoSound) => true,
        (
            NotificationSound::Ringtone {
                ringtone_id: lhs_id,
            },
            NotificationSound::Ringtone {
                ringtone_id: rhs_id,
            },
        ) => lhs_id == rhs_id,
        // `Local` sounds are default; the UNREACHABLE tail reads as false.
        _ => false,
    }
}

/// `are_different_equivalent_notification_sounds` (NotificationSound.cpp:
/// 171-186) — true when the sounds are equivalent but not identical, i.e.
/// two `Local`s with different title/data.
pub fn are_different_equivalent_notification_sounds(
    lhs: &Option<NotificationSound>,
    rhs: &Option<NotificationSound>,
) -> bool {
    let Some(lhs_sound) = lhs else {
        return matches!(rhs, Some(NotificationSound::Local { .. }));
    };
    let Some(rhs_sound) = rhs else {
        return matches!(lhs_sound, NotificationSound::Local { .. });
    };
    match (lhs_sound, rhs_sound) {
        (
            NotificationSound::Local {
                title: lhs_title,
                data: lhs_data,
            },
            NotificationSound::Local {
                title: rhs_title,
                data: rhs_data,
            },
        ) => lhs_title != rhs_title || lhs_data != rhs_data,
        _ => false,
    }
}

/// `get_notification_sound_ringtone_id` (NotificationSound.cpp:188-204).
pub fn get_notification_sound_ringtone_id(notification_sound: &Option<NotificationSound>) -> i64 {
    match notification_sound {
        None => -1,
        Some(NotificationSound::NoSound) => 0,
        Some(NotificationSound::Local { .. }) => -1,
        Some(NotificationSound::Ringtone { ringtone_id }) => *ringtone_id,
    }
}

/// `get_legacy_notification_sound` (NotificationSound.cpp:206-214) — the
/// pre-ringtone-support string form.
pub fn get_legacy_notification_sound(sound: &str) -> Option<NotificationSound> {
    if sound == "default" {
        return None;
    }
    if sound.is_empty() {
        return Some(NotificationSound::NoSound);
    }
    Some(NotificationSound::Local {
        title: sound.to_owned(),
        data: sound.to_owned(),
    })
}

/// `get_notification_sound(bool, int64)` (NotificationSound.cpp:216-224) —
/// the td_api `sound_id` intake.
pub fn get_notification_sound_from_id(
    use_default_sound: bool,
    ringtone_id: i64,
) -> Option<NotificationSound> {
    if use_default_sound || ringtone_id == -1 {
        return None;
    }
    if ringtone_id == 0 {
        return Some(NotificationSound::NoSound);
    }
    Some(NotificationSound::Ringtone { ringtone_id })
}

/// `get_notification_sound(telegram_api::NotificationSound *)`
/// (NotificationSound.cpp:226-252).
pub fn get_notification_sound_from_telegram(
    notification_sound: Option<&telegram_api::NotificationSound>,
) -> Option<NotificationSound> {
    let notification_sound = notification_sound?;
    match notification_sound {
        telegram_api::NotificationSound::notificationSoundDefault(_) => None,
        telegram_api::NotificationSound::notificationSoundNone(_) => {
            Some(NotificationSound::NoSound)
        }
        telegram_api::NotificationSound::notificationSoundLocal(sound) => {
            Some(NotificationSound::Local {
                title: sound.title.clone(),
                data: sound.data.clone(),
            })
        }
        telegram_api::NotificationSound::notificationSoundRingtone(sound) => {
            if sound.id == 0 || sound.id == -1 {
                tdutils::log_error!("Receive ringtone with ID = {}", sound.id);
                return Some(NotificationSound::NoSound);
            }
            Some(NotificationSound::Ringtone {
                ringtone_id: sound.id,
            })
        }
    }
}

/// `get_notification_sound(telegram_api::peerNotifySettings *, bool)`
/// (NotificationSound.cpp:254-265) — neither `TD_ANDROID` nor any
/// `TD_DARWIN` platform is defined for this build, so the `other_sound` /
/// `stories_other_sound` slots are the live ones.
pub fn get_notification_sound_from_peer_settings(
    settings: &telegram_api::PeerNotifySettings,
    for_stories: bool,
) -> Option<NotificationSound> {
    let sound = if for_stories {
        settings.stories_other_sound.as_ref()
    } else {
        settings.other_sound.as_ref()
    };
    get_notification_sound_from_telegram(sound)
}

/// `get_input_notification_sound` (NotificationSound.cpp:267-292) — a
/// non-default sound must never serialize as null, so `return_non_null`
/// upgrades the default answer to `notificationSoundDefault`.
pub fn get_input_notification_sound(
    notification_sound: &Option<NotificationSound>,
    return_non_null: bool,
) -> Option<telegram_api::NotificationSound> {
    let Some(sound) = notification_sound else {
        return if return_non_null {
            Some(telegram_api::NotificationSound::notificationSoundDefault(
                telegram_api::notificationSoundDefault,
            ))
        } else {
            None
        };
    };
    Some(match sound {
        NotificationSound::NoSound => telegram_api::NotificationSound::notificationSoundNone(
            telegram_api::notificationSoundNone,
        ),
        NotificationSound::Local { title, data } => {
            telegram_api::NotificationSound::notificationSoundLocal(
                telegram_api::notificationSoundLocal {
                    title: title.clone(),
                    data: data.clone(),
                },
            )
        }
        NotificationSound::Ringtone { ringtone_id } => {
            telegram_api::NotificationSound::notificationSoundRingtone(
                telegram_api::notificationSoundRingtone { id: *ringtone_id },
            )
        }
    })
}

/// `dup_notification_sound` — `NotificationSound` is a value type here,
/// so the C++ clone helper is a plain copy.
pub fn dup_notification_sound(
    notification_sound: &Option<NotificationSound>,
) -> Option<NotificationSound> {
    notification_sound.clone()
}

/// `NotificationSettingsScope` (NotificationSettingsScope.h).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationSettingsScope {
    Private,
    Group,
    Channel,
}

impl NotificationSettingsScope {
    /// The `enum class : int32` value, as stored in the on-server log
    /// events and used for the unmute timeout slot id.
    pub fn as_i32(self) -> i32 {
        match self {
            NotificationSettingsScope::Private => 0,
            NotificationSettingsScope::Group => 1,
            NotificationSettingsScope::Channel => 2,
        }
    }

    pub fn from_i32(value: i32) -> Self {
        match value {
            1 => NotificationSettingsScope::Group,
            2 => NotificationSettingsScope::Channel,
            _ => NotificationSettingsScope::Private,
        }
    }
}

/// `get_notification_settings_scope_object` (NotificationSettingsScope.cpp:
/// 16-29).
pub fn get_notification_settings_scope_object(
    scope: NotificationSettingsScope,
) -> td_api::NotificationSettingsScope {
    match scope {
        NotificationSettingsScope::Private => {
            td_api::NotificationSettingsScope::notificationSettingsScopePrivateChats(
                td_api::notificationSettingsScopePrivateChats,
            )
        }
        NotificationSettingsScope::Group => {
            td_api::NotificationSettingsScope::notificationSettingsScopeGroupChats(
                td_api::notificationSettingsScopeGroupChats,
            )
        }
        NotificationSettingsScope::Channel => {
            td_api::NotificationSettingsScope::notificationSettingsScopeChannelChats(
                td_api::notificationSettingsScopeChannelChats,
            )
        }
    }
}

/// `get_notification_settings_scope` (NotificationSettingsScope.cpp:50-67);
/// the C++ `CHECK` on the null pointer happens at the arm site (the
/// generated field is an `Option` here).
pub fn get_notification_settings_scope(
    scope: &td_api::NotificationSettingsScope,
) -> NotificationSettingsScope {
    match scope {
        td_api::NotificationSettingsScope::notificationSettingsScopePrivateChats(_) => {
            NotificationSettingsScope::Private
        }
        td_api::NotificationSettingsScope::notificationSettingsScopeGroupChats(_) => {
            NotificationSettingsScope::Group
        }
        td_api::NotificationSettingsScope::notificationSettingsScopeChannelChats(_) => {
            NotificationSettingsScope::Channel
        }
    }
}

/// `get_input_notify_peer(NotificationSettingsScope)`
/// (NotificationSettingsScope.cpp:31-44).
pub fn get_input_notify_peer_for_scope(
    scope: NotificationSettingsScope,
) -> telegram_api::InputNotifyPeer {
    match scope {
        NotificationSettingsScope::Private => {
            telegram_api::InputNotifyPeer::inputNotifyUsers(telegram_api::inputNotifyUsers)
        }
        NotificationSettingsScope::Group => {
            telegram_api::InputNotifyPeer::inputNotifyChats(telegram_api::inputNotifyChats)
        }
        NotificationSettingsScope::Channel => telegram_api::InputNotifyPeer::inputNotifyBroadcasts(
            telegram_api::inputNotifyBroadcasts,
        ),
    }
}

/// `ScopeNotificationSettings` (ScopeNotificationSettings.h:17-60) — the
/// field defaults are the C++ member initializers (`show_preview` and
/// `use_default_mute_stories` start `true`), not the zero form.
#[derive(Clone, Debug, PartialEq)]
pub struct ScopeNotificationSettings {
    pub mute_until: i32,
    pub sound: Option<NotificationSound>,
    pub story_sound: Option<NotificationSound>,
    pub show_preview: bool,
    pub use_default_mute_stories: bool,
    pub mute_stories: bool,
    pub hide_story_sender: bool,
    pub is_synchronized: bool,
    // local settings
    pub disable_pinned_message_notifications: bool,
    pub disable_mention_notifications: bool,
}

impl Default for ScopeNotificationSettings {
    fn default() -> Self {
        ScopeNotificationSettings {
            mute_until: 0,
            sound: None,
            story_sound: None,
            show_preview: true,
            use_default_mute_stories: true,
            mute_stories: false,
            hide_story_sender: false,
            is_synchronized: false,
            disable_pinned_message_notifications: false,
            disable_mention_notifications: false,
        }
    }
}

impl ScopeNotificationSettings {
    /// The C++ `Default` overwrites the `true` field defaults, so the
    /// 9-argument constructor (ScopeNotificationSettings.h:33-47) is a
    /// plain constructor here.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mute_until: i32,
        sound: Option<NotificationSound>,
        show_preview: bool,
        use_default_mute_stories: bool,
        mute_stories: bool,
        story_sound: Option<NotificationSound>,
        hide_story_sender: bool,
        disable_pinned_message_notifications: bool,
        disable_mention_notifications: bool,
    ) -> Self {
        ScopeNotificationSettings {
            mute_until,
            sound,
            story_sound,
            show_preview,
            use_default_mute_stories,
            mute_stories,
            hide_story_sender,
            is_synchronized: true,
            disable_pinned_message_notifications,
            disable_mention_notifications,
        }
    }

    /// `ScopeNotificationSettings::get_input_peer_notify_settings`
    /// (ScopeNotificationSettings.cpp:10-26); the mask bits are the
    /// generated `inputPeerNotifySettings` flag positions.
    pub fn get_input_peer_notify_settings(&self) -> telegram_api::InputPeerNotifySettings {
        let mut flags = (1 << 2) | (1 << 0) | (1 << 7);
        if self.sound.is_some() {
            flags |= 1 << 3;
        }
        if self.story_sound.is_some() {
            flags |= 1 << 8;
        }
        if !self.use_default_mute_stories {
            flags |= 1 << 6;
        }
        telegram_api::InputPeerNotifySettings {
            flags,
            show_previews: self.show_preview,
            silent: false,
            mute_until: self.mute_until,
            sound: get_input_notification_sound(&self.sound, false),
            stories_muted: self.mute_stories,
            stories_hide_sender: self.hide_story_sender,
            stories_sound: get_input_notification_sound(&self.story_sound, false),
        }
    }

    /// The `store` half of ScopeNotificationSettings.hpp — the flag block
    /// is `is_muted, has_sound, show_preview, false, is_synchronized,
    /// disable_pinned_message_notifications, disable_mention_notifications,
    /// has_ringtone_support, mute_stories, has_story_sound,
    /// hide_story_sender, use_mute_stories`; `is_muted` is evaluated
    /// against the current unix time like the C++.
    pub fn store(&self, out: &mut Vec<u8>, unix_time: i32) {
        let is_muted = self.mute_until != 0 && self.mute_until > unix_time;
        let has_sound = self.sound.is_some();
        let has_story_sound = self.story_sound.is_some();
        let use_mute_stories = !self.use_default_mute_stories;
        // has_ringtone_support is always true since Version 7; the flag
        // stays reserved (always written true).
        let has_ringtone_support = true;
        crate::store_parse::store_flags(
            out,
            [
                is_muted,
                has_sound,
                self.show_preview,
                false,
                self.is_synchronized,
                self.disable_pinned_message_notifications,
                self.disable_mention_notifications,
                has_ringtone_support,
                self.mute_stories,
                has_story_sound,
                self.hide_story_sender,
                use_mute_stories,
            ],
        );
        if is_muted {
            store_i32(out, self.mute_until);
        }
        if has_sound {
            self.sound.as_ref().expect("has_sound checked").store(out);
        }
        if has_story_sound {
            self.story_sound
                .as_ref()
                .expect("has_story_sound checked")
                .store(out);
        }
    }

    /// The `parse` half of ScopeNotificationSettings.hpp, over the
    /// log-event framing reader (the version prefix is consumed by the
    /// caller).
    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let [
            is_muted,
            has_sound,
            show_preview,
            _silent_send_message_ignored,
            is_synchronized,
            disable_pinned_message_notifications,
            disable_mention_notifications,
            has_ringtone_support,
            mute_stories,
            has_story_sound,
            hide_story_sender,
            use_mute_stories,
        ] = parser.parse_flags::<12>()?;

        let mute_until = if is_muted { parser.i32_()? } else { 0 };
        let sound = if has_sound {
            Some(if has_ringtone_support {
                NotificationSound::parse(parser)?
            } else {
                get_legacy_notification_sound(&parser.string()?)
                    .expect("legacy sound is only None for the literal \"default\"")
            })
        } else {
            None
        };
        let story_sound = if has_story_sound {
            Some(NotificationSound::parse(parser)?)
        } else {
            None
        };
        Ok(ScopeNotificationSettings {
            mute_until,
            sound,
            story_sound,
            show_preview,
            use_default_mute_stories: !use_mute_stories,
            mute_stories,
            hide_story_sender,
            is_synchronized,
            disable_pinned_message_notifications,
            disable_mention_notifications,
        })
    }
}

/// `get_scope_notification_settings_object` (ScopeNotificationSettings.cpp:
/// 28-43).
pub fn get_scope_notification_settings_object(
    notification_settings: &ScopeNotificationSettings,
    unix_time: i32,
) -> td_api::ScopeNotificationSettings {
    td_api::ScopeNotificationSettings {
        mute_for: (notification_settings.mute_until - unix_time).max(0),
        sound_id: get_notification_sound_ringtone_id(&notification_settings.sound),
        show_preview: notification_settings.show_preview,
        use_default_mute_stories: notification_settings.use_default_mute_stories,
        mute_stories: notification_settings.mute_stories,
        story_sound_id: get_notification_sound_ringtone_id(&notification_settings.story_sound),
        show_story_poster: !notification_settings.hide_story_sender,
        disable_pinned_message_notifications: notification_settings
            .disable_pinned_message_notifications,
        disable_mention_notifications: notification_settings.disable_mention_notifications,
    }
}

/// `get_mute_until(int32)` (ScopeNotificationSettings.cpp:45-55).
fn get_mute_until(mute_for: i32, unix_time: i32) -> i32 {
    if mute_for <= 0 {
        return 0;
    }
    const MAX_PRECISE_MUTE_FOR: i32 = 366 * 86400;
    if mute_for > MAX_PRECISE_MUTE_FOR || mute_for >= i32::MAX - unix_time {
        return i32::MAX;
    }
    mute_for.wrapping_add(unix_time)
}

/// `get_scope_notification_settings(td_api::object_ptr<...>)`
/// (ScopeNotificationSettings.cpp:57-77).
pub fn get_scope_notification_settings_from_td_api(
    notification_settings: Option<td_api::ScopeNotificationSettings>,
    unix_time: i32,
) -> Result<ScopeNotificationSettings, Status> {
    let Some(notification_settings) = notification_settings else {
        return Err(Status::error(
            400,
            "New notification settings must be non-empty",
        ));
    };
    let mute_until = get_mute_until(notification_settings.mute_for, unix_time);
    Ok(ScopeNotificationSettings::new(
        mute_until,
        get_notification_sound_from_id(false, notification_settings.sound_id),
        notification_settings.show_preview,
        notification_settings.use_default_mute_stories,
        notification_settings.mute_stories,
        get_notification_sound_from_id(false, notification_settings.story_sound_id),
        !notification_settings.show_story_poster,
        notification_settings.disable_pinned_message_notifications,
        notification_settings.disable_mention_notifications,
    ))
}

/// `get_scope_notification_settings(telegram_api::peerNotifySettings &&, ...)`
/// (ScopeNotificationSettings.cpp:79-101).
pub fn get_scope_notification_settings_from_telegram(
    settings: Option<telegram_api::PeerNotifySettings>,
    old_disable_pinned_message_notifications: bool,
    old_disable_mention_notifications: bool,
    unix_time: i32,
) -> ScopeNotificationSettings {
    let Some(settings) = settings else {
        return ScopeNotificationSettings::default();
    };
    let mut mute_until = settings.mute_until;
    if mute_until <= unix_time {
        mute_until = 0;
    }
    let show_preview = settings.show_previews;
    // `peerNotifySettings::STORIES_MUTED_MASK` — bit 6.
    let use_default_mute_stories = (settings.flags & (1 << 6)) == 0;
    ScopeNotificationSettings::new(
        mute_until,
        get_notification_sound_from_peer_settings(&settings, false),
        show_preview,
        use_default_mute_stories,
        settings.stories_muted,
        get_notification_sound_from_peer_settings(&settings, true),
        settings.stories_hide_sender,
        old_disable_pinned_message_notifications,
        old_disable_mention_notifications,
    )
}

/// `ReactionNotificationsFrom` (ReactionNotificationsFrom.h) — `None` is
/// the td_api `reactionNotificationSourceNone`; the Telegram-layer
/// `None` serializes as null.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReactionNotificationsFrom {
    #[default]
    None,
    Contacts,
    All,
}

impl ReactionNotificationsFrom {
    /// The td_api intake (ReactionNotificationsFrom.cpp:5-24).
    pub fn from_td_api(notification_source: Option<&td_api::ReactionNotificationSource>) -> Self {
        let Some(notification_source) = notification_source else {
            return ReactionNotificationsFrom::None;
        };
        match notification_source {
            td_api::ReactionNotificationSource::reactionNotificationSourceNone(_) => {
                ReactionNotificationsFrom::None
            }
            td_api::ReactionNotificationSource::reactionNotificationSourceContacts(_) => {
                ReactionNotificationsFrom::Contacts
            }
            td_api::ReactionNotificationSource::reactionNotificationSourceAll(_) => {
                ReactionNotificationsFrom::All
            }
        }
    }

    /// The telegram intake (ReactionNotificationsFrom.cpp:26-42).
    pub fn from_telegram(
        notifications_from: Option<&telegram_api::ReactionNotificationsFrom>,
    ) -> Self {
        let Some(notifications_from) = notifications_from else {
            return ReactionNotificationsFrom::None;
        };
        match notifications_from {
            telegram_api::ReactionNotificationsFrom::reactionNotificationsFromContacts(_) => {
                ReactionNotificationsFrom::Contacts
            }
            telegram_api::ReactionNotificationsFrom::reactionNotificationsFromAll(_) => {
                ReactionNotificationsFrom::All
            }
        }
    }

    /// `get_reaction_notification_source_object`
    /// (ReactionNotificationsFrom.cpp:44-60).
    pub fn get_reaction_notification_source_object(&self) -> td_api::ReactionNotificationSource {
        match self {
            ReactionNotificationsFrom::None => {
                td_api::ReactionNotificationSource::reactionNotificationSourceNone(
                    td_api::reactionNotificationSourceNone,
                )
            }
            ReactionNotificationsFrom::Contacts => {
                td_api::ReactionNotificationSource::reactionNotificationSourceContacts(
                    td_api::reactionNotificationSourceContacts,
                )
            }
            ReactionNotificationsFrom::All => {
                td_api::ReactionNotificationSource::reactionNotificationSourceAll(
                    td_api::reactionNotificationSourceAll,
                )
            }
        }
    }

    /// `get_input_reaction_notifications_from`
    /// (ReactionNotificationsFrom.cpp:62-78) — `None` stays null.
    pub fn get_input_reaction_notifications_from(
        &self,
    ) -> Option<telegram_api::ReactionNotificationsFrom> {
        match self {
            ReactionNotificationsFrom::None => None,
            ReactionNotificationsFrom::Contacts => Some(
                telegram_api::ReactionNotificationsFrom::reactionNotificationsFromContacts(
                    telegram_api::reactionNotificationsFromContacts,
                ),
            ),
            ReactionNotificationsFrom::All => Some(
                telegram_api::ReactionNotificationsFrom::reactionNotificationsFromAll(
                    telegram_api::reactionNotificationsFromAll,
                ),
            ),
        }
    }

    /// The empty flag block is part of the stored format
    /// (ReactionNotificationsFrom.hpp).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_u32(out, 0);
        store_i32(out, *self as i32);
    }

    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let _empty_flags = parser.parse_flags::<0>()?;
        Ok(match parser.i32_()? {
            0 => ReactionNotificationsFrom::None,
            1 => ReactionNotificationsFrom::Contacts,
            2 => ReactionNotificationsFrom::All,
            _ => return Err("Unknown ReactionNotificationsFrom type"),
        })
    }
}

/// `ReactionNotificationSettings` (ReactionNotificationSettings.h:17-62).
#[derive(Clone, Debug)]
pub struct ReactionNotificationSettings {
    pub message_reactions: ReactionNotificationsFrom,
    pub story_reactions: ReactionNotificationsFrom,
    pub poll_votes: ReactionNotificationsFrom,
    pub sound: Option<NotificationSound>,
    pub show_preview: bool,
}

impl Default for ReactionNotificationSettings {
    /// The C++ defaults: `ReactionNotificationsFrom()` is `Contacts`, the
    /// sound is default, previews are on.
    fn default() -> Self {
        ReactionNotificationSettings {
            message_reactions: ReactionNotificationsFrom::Contacts,
            story_reactions: ReactionNotificationsFrom::Contacts,
            poll_votes: ReactionNotificationsFrom::Contacts,
            sound: None,
            show_preview: true,
        }
    }
}

impl PartialEq for ReactionNotificationSettings {
    /// `operator==` (ReactionNotificationSettings.cpp:104-110) — the sound
    /// compares through the equivalence rules, not raw equality.
    fn eq(&self, other: &Self) -> bool {
        self.message_reactions == other.message_reactions
            && self.story_reactions == other.story_reactions
            && self.poll_votes == other.poll_votes
            && are_equivalent_notification_sounds(&self.sound, &other.sound)
            && self.show_preview == other.show_preview
    }
}

impl ReactionNotificationSettings {
    /// The td_api constructor (ReactionNotificationSettings.cpp:5-20).
    pub fn from_td_api(
        notification_settings: Option<td_api::ReactionNotificationSettings>,
    ) -> Self {
        let Some(notification_settings) = notification_settings else {
            return ReactionNotificationSettings::default();
        };
        ReactionNotificationSettings {
            message_reactions: ReactionNotificationsFrom::from_td_api(
                notification_settings.message_reaction_source.as_ref(),
            ),
            story_reactions: ReactionNotificationsFrom::from_td_api(
                notification_settings.story_reaction_source.as_ref(),
            ),
            poll_votes: ReactionNotificationsFrom::from_td_api(
                notification_settings.poll_vote_source.as_ref(),
            ),
            sound: get_notification_sound_from_id(false, notification_settings.sound_id),
            show_preview: notification_settings.show_preview,
        }
    }

    /// The telegram constructor (ReactionNotificationSettings.cpp:22-36).
    pub fn from_telegram(notify_settings: Option<telegram_api::ReactionsNotifySettings>) -> Self {
        let Some(notify_settings) = notify_settings else {
            return ReactionNotificationSettings::default();
        };
        ReactionNotificationSettings {
            message_reactions: ReactionNotificationsFrom::from_telegram(
                notify_settings.messages_notify_from.as_ref(),
            ),
            story_reactions: ReactionNotificationsFrom::from_telegram(
                notify_settings.stories_notify_from.as_ref(),
            ),
            poll_votes: ReactionNotificationsFrom::from_telegram(
                notify_settings.poll_votes_notify_from.as_ref(),
            ),
            sound: get_notification_sound_from_telegram(notify_settings.sound.as_ref()),
            show_preview: notify_settings.show_previews,
        }
    }

    /// `get_reaction_notification_settings_object`
    /// (ReactionNotificationSettings.cpp:38-47).
    pub fn get_reaction_notification_settings_object(
        &self,
    ) -> td_api::ReactionNotificationSettings {
        td_api::ReactionNotificationSettings {
            message_reaction_source: Some(
                self.message_reactions
                    .get_reaction_notification_source_object(),
            ),
            story_reaction_source: Some(
                self.story_reactions
                    .get_reaction_notification_source_object(),
            ),
            poll_vote_source: Some(self.poll_votes.get_reaction_notification_source_object()),
            sound_id: get_notification_sound_ringtone_id(&self.sound),
            show_preview: self.show_preview,
        }
    }

    /// `get_input_reactions_notify_settings`
    /// (ReactionNotificationSettings.cpp:49-69) — the wire flags follow
    /// the generated `reactionsNotifySettings` flag positions; the sound
    /// is always on the wire (`return_non_null`).
    pub fn get_input_reactions_notify_settings(&self) -> telegram_api::ReactionsNotifySettings {
        let mut flags = 0;
        let messages_notify_from = self
            .message_reactions
            .get_input_reaction_notifications_from();
        if messages_notify_from.is_some() {
            flags |= 1;
        }
        let stories_notify_from = self.story_reactions.get_input_reaction_notifications_from();
        if stories_notify_from.is_some() {
            flags |= 1 << 1;
        }
        let poll_votes_notify_from = self.poll_votes.get_input_reaction_notifications_from();
        if poll_votes_notify_from.is_some() {
            flags |= 1 << 2;
        }
        telegram_api::ReactionsNotifySettings {
            flags,
            messages_notify_from,
            stories_notify_from,
            poll_votes_notify_from,
            sound: get_input_notification_sound(&self.sound, true),
            show_previews: self.show_preview,
        }
    }

    /// `update_default_notification_sound`
    /// (ReactionNotificationSettings.cpp:71-77).
    pub fn update_default_notification_sound(&mut self, other: &ReactionNotificationSettings) {
        if is_notification_sound_default(&self.sound) && is_notification_sound_default(&other.sound)
        {
            self.sound = dup_notification_sound(&other.sound);
        }
    }

    /// The `store` half of ReactionNotificationSettings.hpp — flags are
    /// `has_sound, show_preview, has_poll_votes`.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_sound = self.sound.is_some();
        let has_poll_votes = self.poll_votes != ReactionNotificationsFrom::default();
        crate::store_parse::store_flags(out, [has_sound, self.show_preview, has_poll_votes]);
        self.message_reactions.store(out);
        self.story_reactions.store(out);
        if has_sound {
            self.sound.as_ref().expect("has_sound checked").store(out);
        }
        if has_poll_votes {
            self.poll_votes.store(out);
        }
    }

    /// The `parse` half of ReactionNotificationSettings.hpp.
    pub fn parse(parser: &mut Parser<'_>) -> Result<Self, &'static str> {
        let [has_sound, show_preview, has_poll_votes] = parser.parse_flags::<3>()?;
        let message_reactions = ReactionNotificationsFrom::parse(parser)?;
        let story_reactions = ReactionNotificationsFrom::parse(parser)?;
        let sound = if has_sound {
            Some(NotificationSound::parse(parser)?)
        } else {
            None
        };
        let poll_votes = if has_poll_votes {
            ReactionNotificationsFrom::parse(parser)?
        } else {
            ReactionNotificationsFrom::default()
        };
        Ok(ReactionNotificationSettings {
            message_reactions,
            story_reactions,
            poll_votes,
            sound,
            show_preview,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_parse::{Parser, store_log_event_version};

    fn round_trip_sound(sound: NotificationSound) -> NotificationSound {
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        sound.store(&mut out);
        let mut parser = Parser::new_log_event(&out).expect("version ok");
        NotificationSound::parse(&mut parser).expect("parse ok")
    }

    #[test]
    fn notification_sound_round_trip_and_bytes() {
        // Ringtone(42): version(4) + type i32 + int64 — the int64 is a
        // full 8-byte store, unlike the C++ `store(int32)` of the enum.
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        NotificationSound::Ringtone { ringtone_id: 42 }.store(&mut out);
        assert_eq!(out.len(), 4 + 4 + 8);
        // new_log_event consumed and checked the version prefix
        let mut parser = Parser::new_log_event(&out).unwrap();
        assert_eq!(parser.i32_().unwrap(), 2);
        assert_eq!(parser.i64_().unwrap(), 42);

        let sounds = vec![
            NotificationSound::NoSound,
            NotificationSound::Local {
                title: "a".to_owned(),
                data: "b".to_owned(),
            },
            NotificationSound::Ringtone { ringtone_id: -5 },
        ];
        for sound in sounds {
            assert_eq!(round_trip_sound(sound.clone()), sound);
        }
    }

    #[test]
    fn sound_equivalence_quirks() {
        let none: Option<NotificationSound> = None;
        let local_a = Some(NotificationSound::Local {
            title: "a".to_owned(),
            data: "a".to_owned(),
        });
        let local_b = Some(NotificationSound::Local {
            title: "b".to_owned(),
            data: "b".to_owned(),
        });
        let no_sound = Some(NotificationSound::NoSound);
        let ringtone = Some(NotificationSound::Ringtone { ringtone_id: 1 });

        // Local counts as default (NotificationSound.cpp:136-141).
        assert!(is_notification_sound_default(&none));
        assert!(is_notification_sound_default(&local_a));
        assert!(!is_notification_sound_default(&no_sound));
        assert!(!is_notification_sound_default(&ringtone));

        assert!(are_equivalent_notification_sounds(&none, &local_a));
        assert!(!are_equivalent_notification_sounds(&none, &no_sound));
        assert!(!are_equivalent_notification_sounds(&no_sound, &ringtone));
        assert!(are_equivalent_notification_sounds(&ringtone, &ringtone));

        assert!(are_different_equivalent_notification_sounds(
            &none, &local_a
        ));
        assert!(are_different_equivalent_notification_sounds(
            &local_a, &local_b
        ));
        assert!(!are_different_equivalent_notification_sounds(
            &local_a, &local_a
        ));
        // null vs Local: equivalent (both "default") but not identical —
        // the C++ answers true here too (NotificationSound.cpp:173-178)
        assert!(are_different_equivalent_notification_sounds(
            &local_a, &none
        ));
        assert!(!are_different_equivalent_notification_sounds(&none, &none));
        assert!(!are_different_equivalent_notification_sounds(
            &no_sound, &ringtone
        ));

        assert_eq!(get_notification_sound_ringtone_id(&none), -1);
        assert_eq!(get_notification_sound_ringtone_id(&no_sound), 0);
        assert_eq!(get_notification_sound_ringtone_id(&local_a), -1);
        assert_eq!(get_notification_sound_ringtone_id(&ringtone), 1);
    }

    #[test]
    fn sound_id_intake_matches_cpp() {
        // get_notification_sound(false, id) (NotificationSound.cpp:216-224).
        assert_eq!(get_notification_sound_from_id(false, -1), None);
        assert_eq!(get_notification_sound_from_id(true, 5), None);
        assert_eq!(
            get_notification_sound_from_id(false, 0),
            Some(NotificationSound::NoSound)
        );
        assert_eq!(
            get_notification_sound_from_id(false, 7),
            Some(NotificationSound::Ringtone { ringtone_id: 7 })
        );
        // legacy strings
        assert_eq!(get_legacy_notification_sound("default"), None);
        assert_eq!(
            get_legacy_notification_sound(""),
            Some(NotificationSound::NoSound)
        );
        assert_eq!(
            get_legacy_notification_sound("x"),
            Some(NotificationSound::Local {
                title: "x".to_owned(),
                data: "x".to_owned()
            })
        );
    }

    #[test]
    fn scope_settings_td_api_round_trip() {
        let unix_time = 1000;
        let from_api = get_scope_notification_settings_from_td_api(
            Some(td_api::ScopeNotificationSettings {
                mute_for: 100,
                sound_id: 5,
                show_preview: false,
                use_default_mute_stories: false,
                mute_stories: true,
                story_sound_id: 0,
                show_story_poster: false,
                disable_pinned_message_notifications: true,
                disable_mention_notifications: false,
            }),
            unix_time,
        )
        .expect("settings are non-empty");
        assert_eq!(from_api.mute_until, 1100);
        assert_eq!(
            from_api.sound,
            Some(NotificationSound::Ringtone { ringtone_id: 5 })
        );
        assert_eq!(from_api.story_sound, Some(NotificationSound::NoSound));
        assert!(from_api.hide_story_sender);
        assert!(from_api.is_synchronized);

        // the object form inverts back (mute_for is relative to now)
        let object = get_scope_notification_settings_object(&from_api, unix_time);
        assert_eq!(object.mute_for, 100);
        assert_eq!(object.sound_id, 5);
        assert!(!object.show_story_poster);
        assert_eq!(object.story_sound_id, 0);

        // the mute clamps (ScopeNotificationSettings.cpp:45-55)
        assert_eq!(get_mute_until(0, unix_time), 0);
        assert_eq!(get_mute_until(-5, unix_time), 0);
        assert_eq!(get_mute_until(366 * 86400 + 1, unix_time), i32::MAX);
        assert_eq!(
            get_mute_until(i32::MAX - unix_time + 1, unix_time),
            i32::MAX
        );
        assert_eq!(get_mute_until(50, unix_time), 1050);

        // the null settings answer
        let error = get_scope_notification_settings_from_td_api(None, unix_time)
            .expect_err("must be empty");
        assert_eq!(error.code(), 400);
    }

    #[test]
    fn scope_settings_store_bytes_and_round_trip() {
        let unix_time = 1000;
        let settings = ScopeNotificationSettings::new(
            1100,
            Some(NotificationSound::Ringtone { ringtone_id: 9 }),
            false,
            false,
            true,
            Some(NotificationSound::NoSound),
            false,
            true,
            false,
        );
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        settings.store(&mut out, unix_time);
        // flags u32 + mute_until (muted: 1100 > 1000) + sound (type + id)
        // + story_sound type
        assert_eq!(out.len(), 4 + 4 + 4 + (4 + 8) + 4);
        let restored = ScopeNotificationSettings::parse(&mut Parser::new_log_event(&out).unwrap())
            .expect("parse ok");
        assert_eq!(restored.mute_until, 1100);
        assert_eq!(
            restored.sound,
            Some(NotificationSound::Ringtone { ringtone_id: 9 })
        );
        assert_eq!(restored.story_sound, Some(NotificationSound::NoSound));
        assert!(!restored.show_preview);
        assert!(!restored.use_default_mute_stories);
        assert!(restored.mute_stories);
        assert!(restored.is_synchronized);
        assert!(restored.disable_pinned_message_notifications);
        assert!(!restored.disable_mention_notifications);

        // an unmuted settings keeps the mute_until off the wire
        let quiet = ScopeNotificationSettings {
            is_synchronized: true,
            ..ScopeNotificationSettings::default()
        };
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        quiet.store(&mut out, unix_time);
        assert_eq!(out.len(), 4 + 4);
        let restored =
            ScopeNotificationSettings::parse(&mut Parser::new_log_event(&out).unwrap()).unwrap();
        assert_eq!(restored.mute_until, 0);
        assert!(restored.sound.is_none());
        assert!(restored.story_sound.is_none());
    }

    #[test]
    fn scope_settings_legacy_sound_parse() {
        // has_ringtone_support = false: the sound is the legacy string.
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        // flags: is_muted=0, has_sound=1, show_preview=1, false,
        // is_synchronized=1, disable_pinned=0, disable_mention=0,
        // has_ringtone_support=0, ...
        let flags: u32 = (1 << 1) | (1 << 2) | (1 << 4);
        store_u32(&mut out, flags);
        store_string(&mut out, "custom");
        // new_log_event consumed and checked the version prefix
        let mut parser = Parser::new_log_event(&out).unwrap();
        let restored = ScopeNotificationSettings::parse(&mut parser).expect("parse ok");
        assert_eq!(
            restored.sound,
            Some(NotificationSound::Local {
                title: "custom".to_owned(),
                data: "custom".to_owned()
            })
        );
    }

    #[test]
    fn scope_settings_input_peer_notify_settings_flags() {
        let mut settings = ScopeNotificationSettings::default();
        let wire = settings.get_input_peer_notify_settings();
        // MUTE_UNTIL | SHOW_PREVIEWS | STORIES_HIDE_SENDER, no sounds
        assert_eq!(wire.flags, (1 << 2) | (1 << 0) | (1 << 7));
        assert!(wire.show_previews);
        assert!(!wire.silent);
        assert!(wire.sound.is_none());

        settings.sound = Some(NotificationSound::NoSound);
        settings.story_sound = Some(NotificationSound::NoSound);
        settings.use_default_mute_stories = false;
        settings.mute_stories = true;
        settings.hide_story_sender = true;
        settings.mute_until = 12345;
        let wire = settings.get_input_peer_notify_settings();
        assert_eq!(
            wire.flags,
            (1 << 2) | (1 << 0) | (1 << 7) | (1 << 3) | (1 << 8) | (1 << 6)
        );
        assert_eq!(wire.mute_until, 12345);
        assert!(wire.stories_muted);
        assert!(wire.stories_hide_sender);
        assert!(matches!(
            wire.sound,
            Some(telegram_api::NotificationSound::notificationSoundNone(_))
        ));
    }

    #[test]
    fn scope_settings_from_telegram_uses_other_sound_slots() {
        let settings = telegram_api::PeerNotifySettings {
            flags: (1 << 0) | (1 << 6) | (1 << 2),
            show_previews: true,
            silent: false,
            mute_until: 900, // <= unix_time 1000 -> 0
            ios_sound: Some(telegram_api::NotificationSound::notificationSoundRingtone(
                telegram_api::notificationSoundRingtone { id: 1 },
            )),
            android_sound: Some(telegram_api::NotificationSound::notificationSoundNone(
                telegram_api::notificationSoundNone,
            )),
            other_sound: Some(telegram_api::NotificationSound::notificationSoundRingtone(
                telegram_api::notificationSoundRingtone { id: 3 },
            )),
            stories_muted: true,
            stories_hide_sender: false,
            stories_ios_sound: None,
            stories_android_sound: None,
            stories_other_sound: Some(telegram_api::NotificationSound::notificationSoundLocal(
                telegram_api::notificationSoundLocal {
                    title: "t".to_owned(),
                    data: "d".to_owned(),
                },
            )),
        };
        let scope =
            get_scope_notification_settings_from_telegram(Some(settings), true, false, 1000);
        assert_eq!(scope.mute_until, 0);
        assert_eq!(
            scope.sound,
            Some(NotificationSound::Ringtone { ringtone_id: 3 })
        );
        assert_eq!(
            scope.story_sound,
            Some(NotificationSound::Local {
                title: "t".to_owned(),
                data: "d".to_owned()
            })
        );
        assert!(!scope.use_default_mute_stories);
        assert!(scope.mute_stories);
        assert_eq!(
            scope.get_input_peer_notify_settings().flags & (1 << 6),
            1 << 6
        );
        // the local-only settings are carried from the caller
        assert!(scope.disable_pinned_message_notifications);
        assert!(!scope.disable_mention_notifications);

        // null settings keep the defaults
        let scope = get_scope_notification_settings_from_telegram(None, true, false, 1000);
        assert_eq!(scope, ScopeNotificationSettings::default());
    }

    #[test]
    fn reaction_notifications_from_conversions() {
        assert_eq!(
            ReactionNotificationsFrom::from_td_api(Some(
                &td_api::ReactionNotificationSource::reactionNotificationSourceAll(
                    td_api::reactionNotificationSourceAll
                )
            )),
            ReactionNotificationsFrom::All
        );
        assert_eq!(
            ReactionNotificationsFrom::from_td_api(None),
            ReactionNotificationsFrom::None
        );
        assert_eq!(
            ReactionNotificationsFrom::from_telegram(Some(
                &telegram_api::ReactionNotificationsFrom::reactionNotificationsFromContacts(
                    telegram_api::reactionNotificationsFromContacts
                )
            )),
            ReactionNotificationsFrom::Contacts
        );
        // None never serializes to Telegram
        assert!(
            ReactionNotificationsFrom::None
                .get_input_reaction_notifications_from()
                .is_none()
        );

        // store/parse keeps the empty flag block
        let mut out = Vec::new();
        ReactionNotificationsFrom::All.store(&mut out);
        assert_eq!(out, {
            let mut expected = Vec::new();
            store_u32(&mut expected, 0);
            store_i32(&mut expected, 2);
            expected
        });
        assert_eq!(
            ReactionNotificationsFrom::parse(&mut Parser::new(&out)).unwrap(),
            ReactionNotificationsFrom::All
        );
    }

    #[test]
    fn reaction_settings_round_trip_and_equivalence() {
        let settings = ReactionNotificationSettings {
            message_reactions: ReactionNotificationsFrom::All,
            story_reactions: ReactionNotificationsFrom::None,
            poll_votes: ReactionNotificationsFrom::Contacts,
            sound: Some(NotificationSound::NoSound),
            show_preview: false,
        };
        let mut out = Vec::new();
        store_log_event_version(&mut out);
        settings.store(&mut out);
        let restored =
            ReactionNotificationSettings::parse(&mut Parser::new_log_event(&out).unwrap())
                .expect("parse ok");
        assert_eq!(restored, settings);
        assert!(are_equivalent_notification_sounds(
            &restored.sound,
            &settings.sound
        ));

        // the default is Contacts/Contacts/Contacts with previews on
        let default_settings = ReactionNotificationSettings::default();
        assert_eq!(
            default_settings.message_reactions,
            ReactionNotificationsFrom::Contacts
        );
        assert!(default_settings.show_preview);

        // equivalence-based equality: Local sounds with different data
        let lhs = ReactionNotificationSettings {
            sound: Some(NotificationSound::Local {
                title: "a".to_owned(),
                data: "a".to_owned(),
            }),
            ..default_settings.clone()
        };
        let mut rhs = lhs.clone();
        rhs.sound = Some(NotificationSound::Local {
            title: "b".to_owned(),
            data: "b".to_owned(),
        });
        assert_eq!(lhs, rhs);
    }

    #[test]
    fn reaction_settings_telegram_and_update_default_sound() {
        let wire = telegram_api::ReactionsNotifySettings {
            flags: 1 | 4,
            messages_notify_from: Some(
                telegram_api::ReactionNotificationsFrom::reactionNotificationsFromAll(
                    telegram_api::reactionNotificationsFromAll,
                ),
            ),
            stories_notify_from: None,
            poll_votes_notify_from: Some(
                telegram_api::ReactionNotificationsFrom::reactionNotificationsFromContacts(
                    telegram_api::reactionNotificationsFromContacts,
                ),
            ),
            sound: Some(telegram_api::NotificationSound::notificationSoundRingtone(
                telegram_api::notificationSoundRingtone { id: 11 },
            )),
            show_previews: false,
        };
        let settings = ReactionNotificationSettings::from_telegram(Some(wire));
        assert_eq!(settings.message_reactions, ReactionNotificationsFrom::All);
        assert_eq!(settings.story_reactions, ReactionNotificationsFrom::None);
        assert_eq!(settings.poll_votes, ReactionNotificationsFrom::Contacts);
        assert_eq!(
            settings.sound,
            Some(NotificationSound::Ringtone { ringtone_id: 11 })
        );
        assert!(!settings.show_preview);

        // the wire form carries the sound even when default (flag bit 3)
        let wire_back = settings.get_input_reactions_notify_settings();
        assert_eq!(wire_back.flags, 1 | 4);
        assert!(wire_back.sound.is_some());
        assert!(wire_back.stories_notify_from.is_none());

        // update_default_notification_sound copies only between defaults
        let mut target = ReactionNotificationSettings::default();
        let local = ReactionNotificationSettings {
            sound: Some(NotificationSound::Local {
                title: "x".to_owned(),
                data: "x".to_owned(),
            }),
            ..ReactionNotificationSettings::default()
        };
        target.update_default_notification_sound(&local);
        assert_eq!(
            target.sound,
            Some(NotificationSound::Local {
                title: "x".to_owned(),
                data: "x".to_owned()
            })
        );
        let mut non_default = ReactionNotificationSettings {
            sound: Some(NotificationSound::NoSound),
            ..ReactionNotificationSettings::default()
        };
        non_default.update_default_notification_sound(&local);
        assert_eq!(non_default.sound, Some(NotificationSound::NoSound));
    }

    #[test]
    fn scope_td_api_object_conversions() {
        // the scope enum round-trips through the td_api object form
        for scope in [
            NotificationSettingsScope::Private,
            NotificationSettingsScope::Group,
            NotificationSettingsScope::Channel,
        ] {
            let object = get_notification_settings_scope_object(scope);
            assert_eq!(get_notification_settings_scope(&object), scope);
        }
        // the i32 form feeds the log events and the timeout slot id
        assert_eq!(NotificationSettingsScope::Private.as_i32(), 0);
        assert_eq!(NotificationSettingsScope::Group.as_i32(), 1);
        assert_eq!(NotificationSettingsScope::Channel.as_i32(), 2);
        assert_eq!(
            NotificationSettingsScope::from_i32(2),
            NotificationSettingsScope::Channel
        );
        // the input notify peers follow the scope
        assert!(matches!(
            get_input_notify_peer_for_scope(NotificationSettingsScope::Channel),
            telegram_api::InputNotifyPeer::inputNotifyBroadcasts(_)
        ));
    }
}
