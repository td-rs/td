//! The port of `td/telegram/AutoDownloadSettings.{h,cpp}` — the glue
//! slice behind `getAutoDownloadSettingsPresets`/`setAutoDownloadSettings`.
//!
//! The local halves are real: `get_auto_download_settings` (the td_api →
//! `AutoDownloadSettings` mapping, AutoDownloadSettings.cpp:98-111), the
//! `convert_auto_download_settings` presets builder with its clamps
//! (AutoDownloadSettings.cpp:21-32), the "New settings must be non-empty"
//! band (Requests.cpp:3126-3128) and the net-type-keyed
//! roaming/wifi flag derivation of `SaveAutoDownloadSettingsQuery::send`
//! (AutoDownloadSettings.cpp:78-81). The
//! `account.getAutoDownloadSettings` / `account.saveAutoDownloadSettings`
//! queries ride the client-side dispatcher under their
//! `AutoDownloadNetKind` — the answers drain through
//! `Td::on_auto_download_net_answer` (the presets triple converts
//! through the clamps; the save ok value drops), and the value → wire
//! `get_input_auto_download_settings` builder landed with the net
//! slice.

use tdutils::promise::Promise;

use crate::connection_creator::NetType;
use crate::td_api;

/// `td::AutoDownloadSettings` (AutoDownloadSettings.h:19-30) — the
/// collapsed settings record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AutoDownloadSettings {
    pub(crate) max_photo_file_size: i32,
    pub(crate) max_video_file_size: i64,
    pub(crate) max_other_file_size: i64,
    pub(crate) video_upload_bitrate: i32,
    pub(crate) is_enabled: bool,
    pub(crate) preload_large_videos: bool,
    pub(crate) preload_next_audio: bool,
    pub(crate) preload_stories: bool,
    pub(crate) use_less_data_for_calls: bool,
}

/// `convert_auto_download_settings` (AutoDownloadSettings.cpp:21-32) —
/// the telegram `autoDownloadSettings` → td_api presets builder with the
/// exact clamps (`MAX_PHOTO_SIZE` = 10 MB, `MAX_DOCUMENT_SIZE` = 2^52).
#[allow(dead_code)] // the presets builder's result half — the getAutoDownloadSettings net seam
pub(crate) fn convert_auto_download_settings(
    settings: &tdmtproto::telegram_api::AutoDownloadSettings,
) -> td_api::AutoDownloadSettings {
    const MAX_PHOTO_SIZE: i32 = 10 * (1 << 20); // 10 MB
    const MAX_DOCUMENT_SIZE: i64 = 1i64 << 52;
    td_api::AutoDownloadSettings {
        is_auto_download_enabled: !settings.disabled,
        max_photo_file_size: settings.photo_size_max.clamp(0, MAX_PHOTO_SIZE),
        max_video_file_size: settings.video_size_max.clamp(0, MAX_DOCUMENT_SIZE),
        max_other_file_size: settings.file_size_max.clamp(0, MAX_DOCUMENT_SIZE),
        video_upload_bitrate: settings.video_upload_maxbitrate,
        preload_large_videos: settings.video_preload_large,
        preload_next_audio: settings.audio_preload_next,
        preload_stories: settings.stories_preload,
        use_less_data_for_calls: settings.phonecalls_less_data,
    }
}

/// `get_auto_download_settings` (AutoDownloadSettings.cpp:98-111) — the
/// td_api input mapping; the C++ null-settings CHECK is the arm-level
/// "New settings must be non-empty" 400 band.
pub(crate) fn get_auto_download_settings(
    settings: &td_api::AutoDownloadSettings,
) -> AutoDownloadSettings {
    AutoDownloadSettings {
        max_photo_file_size: settings.max_photo_file_size,
        max_video_file_size: settings.max_video_file_size,
        max_other_file_size: settings.max_other_file_size,
        video_upload_bitrate: settings.video_upload_bitrate,
        is_enabled: settings.is_auto_download_enabled,
        preload_large_videos: settings.preload_large_videos,
        preload_next_audio: settings.preload_next_audio,
        preload_stories: settings.preload_stories,
        use_less_data_for_calls: settings.use_less_data_for_calls,
    }
}

/// `get_input_auto_download_settings` (AutoDownloadSettings.cpp:62-68)
/// — the value → wire form the `SaveAutoDownloadSettingsQuery` sends
/// (the C++ passes flags = 0 and leaves the queue caps at their
/// defaults).
pub(crate) fn get_input_auto_download_settings(
    settings: &AutoDownloadSettings,
) -> tdmtproto::telegram_api::AutoDownloadSettings {
    tdmtproto::telegram_api::AutoDownloadSettings {
        flags: 0,
        disabled: !settings.is_enabled,
        video_preload_large: settings.preload_large_videos,
        audio_preload_next: settings.preload_next_audio,
        phonecalls_less_data: settings.use_less_data_for_calls,
        stories_preload: settings.preload_stories,
        photo_size_max: settings.max_photo_file_size,
        video_size_max: settings.max_video_file_size,
        file_size_max: settings.max_other_file_size,
        video_upload_maxbitrate: settings.video_upload_bitrate,
        small_queue_active_operations_max: 0,
        large_queue_active_operations_max: 0,
    }
}

/// `get_auto_download_settings_presets` (AutoDownloadSettings.cpp:113-116)
/// — the `account.getAutoDownloadSettings` query parks at the
/// `[blocked: net]` seam; the presets builder above is its result half.
pub(crate) fn get_auto_download_settings_presets(
    env: &mut impl AutoDownloadSettingsEnv,
    promise: Promise<td_api::AutoDownloadSettingsPresets>,
) {
    env.get_auto_download_settings_via_net(promise);
}

/// `set_auto_download_settings` (AutoDownloadSettings.cpp:118-120) — the
/// `SaveAutoDownloadSettingsQuery::send` roaming/wifi flag derivation is
/// the net-type-keyed local half; the `account.saveAutoDownloadSettings`
/// tail parks at the `[blocked: net]` seam.
pub(crate) fn set_auto_download_settings(
    env: &mut impl AutoDownloadSettingsEnv,
    net_type: NetType,
    settings: AutoDownloadSettings,
    promise: Promise<()>,
) {
    env.set_auto_download_settings_via_net(
        net_type == NetType::MobileRoaming,
        net_type == NetType::WiFi,
        settings,
        promise,
    );
}

/// The [`crate::auto_download_settings`] seams over the `Td` — the env
/// convention ([`crate::connection_creator::ConnectionCreatorEnv`]).
pub(crate) trait AutoDownloadSettingsEnv {
    /// `GetAutoDownloadSettingsQuery::send`
    /// (AutoDownloadSettings.cpp:42-45) — the
    /// `account.getAutoDownloadSettings` query; `[blocked: net]`.
    fn get_auto_download_settings_via_net(
        &mut self,
        promise: Promise<td_api::AutoDownloadSettingsPresets>,
    );

    /// `SaveAutoDownloadSettingsQuery::send`
    /// (AutoDownloadSettings.cpp:78-81) — the
    /// `account.saveAutoDownloadSettings(flags, is_roaming, is_wifi,
    /// settings)` query; `[blocked: net]`.
    fn set_auto_download_settings_via_net(
        &mut self,
        is_roaming: bool,
        is_wifi: bool,
        settings: AutoDownloadSettings,
        promise: Promise<()>,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The presets builder clamps: the disabled flip, the 10 MB photo
    /// cap, the 2^52 document caps and the pass-through flags.
    #[test]
    fn convert_auto_download_settings_clamps_match_the_cpp() {
        let settings = tdmtproto::telegram_api::AutoDownloadSettings {
            flags: 0,
            disabled: true,
            video_preload_large: true,
            audio_preload_next: false,
            phonecalls_less_data: true,
            stories_preload: true,
            photo_size_max: -5,
            video_size_max: -1,
            file_size_max: 1i64 << 53,
            video_upload_maxbitrate: 4242,
            small_queue_active_operations_max: 0,
            large_queue_active_operations_max: 0,
        };
        let converted = convert_auto_download_settings(&settings);
        assert!(!converted.is_auto_download_enabled);
        assert_eq!(converted.max_photo_file_size, 0);
        assert_eq!(converted.max_video_file_size, 0);
        assert_eq!(converted.max_other_file_size, 1i64 << 52);
        assert_eq!(converted.video_upload_bitrate, 4242);
        assert!(converted.preload_large_videos);
        assert!(!converted.preload_next_audio);
        assert!(converted.preload_stories);
        assert!(converted.use_less_data_for_calls);

        // the pass-through band: a sane server answer rides unchanged
        let settings = tdmtproto::telegram_api::AutoDownloadSettings {
            photo_size_max: 100_000,
            video_size_max: 5_000_000,
            file_size_max: 3_000_000,
            ..settings
        };
        let converted = convert_auto_download_settings(&settings);
        assert_eq!(converted.max_photo_file_size, 100_000);
        assert_eq!(converted.max_video_file_size, 5_000_000);
        assert_eq!(converted.max_other_file_size, 3_000_000);
    }

    /// The td_api input mapping is the field-for-field
    /// `get_auto_download_settings` (AutoDownloadSettings.cpp:98-111).
    #[test]
    fn get_auto_download_settings_maps_field_for_field() {
        let settings = td_api::AutoDownloadSettings {
            is_auto_download_enabled: true,
            max_photo_file_size: 200_000,
            max_video_file_size: 6_000_000,
            max_other_file_size: 4_000_000,
            video_upload_bitrate: 5000,
            preload_large_videos: true,
            preload_next_audio: true,
            preload_stories: false,
            use_less_data_for_calls: true,
        };
        let mapped = get_auto_download_settings(&settings);
        assert_eq!(mapped.max_photo_file_size, 200_000);
        assert_eq!(mapped.max_video_file_size, 6_000_000);
        assert_eq!(mapped.max_other_file_size, 4_000_000);
        assert_eq!(mapped.video_upload_bitrate, 5000);
        assert!(mapped.is_enabled);
        assert!(mapped.preload_large_videos);
        assert!(mapped.preload_next_audio);
        assert!(!mapped.preload_stories);
        assert!(mapped.use_less_data_for_calls);
    }

    /// The env seams: the net-type-keyed flag derivation table and the
    /// parked promises.
    #[test]
    fn set_auto_download_settings_flags_match_the_cpp() {
        struct TestEnv {
            get_presets: usize,
            saves: Vec<(bool, bool, AutoDownloadSettings)>,
        }

        impl AutoDownloadSettingsEnv for TestEnv {
            fn get_auto_download_settings_via_net(
                &mut self,
                _promise: Promise<td_api::AutoDownloadSettingsPresets>,
            ) {
                self.get_presets += 1;
            }

            fn set_auto_download_settings_via_net(
                &mut self,
                is_roaming: bool,
                is_wifi: bool,
                settings: AutoDownloadSettings,
                _promise: Promise<()>,
            ) {
                self.saves.push((is_roaming, is_wifi, settings));
            }
        }

        let settings = AutoDownloadSettings {
            is_enabled: true,
            ..AutoDownloadSettings::default()
        };
        let cases = [
            (NetType::Other, (false, false)),
            (NetType::WiFi, (false, true)),
            (NetType::Mobile, (false, false)),
            (NetType::MobileRoaming, (true, false)),
            (NetType::None, (false, false)),
        ];
        for (net_type, expected_flags) in cases {
            let mut env = TestEnv {
                get_presets: 0,
                saves: Vec::new(),
            };
            set_auto_download_settings(&mut env, net_type, settings.clone(), Promise::default());
            assert_eq!(env.saves.len(), 1);
            assert_eq!((env.saves[0].0, env.saves[0].1), expected_flags);
            assert_eq!(env.saves[0].2, settings);
            assert_eq!(env.get_presets, 0);
        }

        // the presets fetch parks at its seam with the caller promise
        let mut env = TestEnv {
            get_presets: 0,
            saves: Vec::new(),
        };
        get_auto_download_settings_presets(&mut env, Promise::default());
        assert_eq!(env.get_presets, 1);
        assert!(env.saves.is_empty());
    }
}
