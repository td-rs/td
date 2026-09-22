//! `BoostManager` (td/telegram/BoostManager.{h,cpp}) — the M.38 Wave-3
//! chat-boost manager: the option-driven boost feature tables and the
//! boost-link builders (the value plane) plus the router over the query
//! seams. Recorded collapses: the theme counts ride the ThemeManager
//! (M.41, not started) — the seam answers zero; the five query handlers
//! are `[blocked: net]` env seams with parked promises; the link parse
//! (`LinkManager::get_dialog_boost_link_info`) and the username/dialog
//! resolution are the M.23/M.6 seams.

use crate::chat_manager::ChannelId;
use crate::dialog_id::DialogId;
use crate::td_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// `td::DialogBoostLinkInfo` (DialogBoostLinkInfo.h).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DialogBoostLinkInfo {
    pub username: String,
    pub channel_id: ChannelId,
}

/// The `G()`/`td_` surface of the manager.
pub trait BoostManagerEnv {
    fn close_flag(&self) -> bool;

    fn get_option_integer(&self, name: &str, default_value: i64) -> i64;

    fn send_update(&mut self, update: td_api::Update);

    /// `DialogManager::check_dialog_access` — the local gate.
    fn check_dialog_access(&mut self, dialog_id: DialogId) -> std::result::Result<(), Status>;

    /// `ChatManager::get_channel_first_username` — the link public half.
    fn get_channel_first_username(&mut self, channel_id: ChannelId) -> String;

    /// `ThemeManager::get_dialog_boost_available_count` — the M.41 seam
    /// (zero counts until the themes track lands).
    fn get_dialog_boost_available_count(&mut self, _level: i32, _for_megagroup: bool) -> (i32, i32, i32) {
        (0, 0, 0)
    }

    // ---- the query seams ([blocked: net]; the promises park) ----
    fn send_get_my_boosts_query(&mut self) {
        tdutils::log_info!("Send premium.getMyBoosts — [blocked: net]");
    }
    fn send_get_boosts_status_query(&mut self, dialog_id: DialogId) {
        tdutils::log_info!("Send premium.getBoostsStatus for {dialog_id:?} — [blocked: net]");
    }
    fn send_apply_boost_query(&mut self, dialog_id: DialogId, slot_ids: &[i32]) {
        tdutils::log_info!("Send premium.applyBoost for {dialog_id:?} — [blocked: net]");
        let _ = slot_ids;
    }
    fn send_get_boosts_list_query(&mut self, dialog_id: DialogId, only_gift_codes: bool, offset: &str, limit: i32) {
        tdutils::log_info!("Send premium.getBoostsList for {dialog_id:?} — [blocked: net]");
        let _ = (only_gift_codes, offset, limit);
    }
    fn send_get_user_boosts_query(&mut self, dialog_id: DialogId, user_id: i64) {
        tdutils::log_info!("Send premium.getUserBoosts for {dialog_id:?} — [blocked: net]");
        let _ = user_id;
    }
}

/// The collapsed `BoostManager`.
#[derive(Default)]
pub struct BoostManager {
    /// The `ChatBoostSlots`-typed promises parked at the slots seam.
    pub parked_slots: Vec<Promise<td_api::ChatBoostSlots>>,
    /// The `ChatBoostStatus`-typed promises parked at the status seam.
    pub parked_status: Vec<Promise<td_api::ChatBoostStatus>>,
    /// The `FoundChatBoosts`-typed promises parked at the list seams.
    pub parked_found_boosts: Vec<Promise<td_api::FoundChatBoosts>>,
}

impl BoostManager {
    pub fn new() -> BoostManager {
        BoostManager::default()
    }

    /// `get_chat_boost_level_features_object` (BoostManager.cpp:365-390) —
    /// the theme counts are the M.41 seam.
    pub fn get_chat_boost_level_features_object(
        &mut self,
        env: &mut impl BoostManagerEnv,
        for_megagroup: bool,
        level: i32,
    ) -> td_api::ChatBoostLevelFeatures {
        let max_level = env.get_option_integer("chat_boost_level_max", 10) as i32;
        let actual_level = level.clamp(0, max_level);
        let key_prefix = if for_megagroup { "group" } else { "channel" };
        let have_enough_boost_level = |name: &str| {
            let needed_boost_level =
                env.get_option_integer(&format!("{key_prefix}_{name}_level_min"), 0) as i32;
            needed_boost_level != 0 && actual_level >= needed_boost_level
        };
        let can_set_profile_background_custom_emoji = have_enough_boost_level("profile_bg_icon");
        let can_set_background_custom_emoji = have_enough_boost_level("bg_icon");
        let can_set_emoji_status = have_enough_boost_level("emoji_status");
        let can_set_custom_background = have_enough_boost_level("custom_wallpaper");
        let can_set_custom_emoji_sticker_set = have_enough_boost_level("emoji_stickers");
        let can_enable_automatic_translation = have_enough_boost_level("autotranslation");
        let can_recognize_speech = have_enough_boost_level("transcribe");
        let can_disable_sponsored_messages = have_enough_boost_level("restrict_sponsored");
        let theme_counts = env.get_dialog_boost_available_count(actual_level, for_megagroup);
        td_api::ChatBoostLevelFeatures {
            level,
            story_per_day_count: if for_megagroup { 0 } else { actual_level },
            custom_emoji_reaction_count: actual_level,
            title_color_count: theme_counts.0,
            profile_accent_color_count: theme_counts.1,
            can_set_profile_background_custom_emoji,
            accent_color_count: theme_counts.2,
            can_set_background_custom_emoji,
            can_set_emoji_status,
            chat_theme_background_count: 0,
            can_set_custom_background,
            can_set_custom_emoji_sticker_set,
            can_enable_automatic_translation,
            can_recognize_speech,
            can_disable_sponsored_messages,
        }
    }

    /// `get_chat_boost_features_object` (BoostManager.cpp:391-414).
    pub fn get_chat_boost_features_object(
        &mut self,
        env: &mut impl BoostManagerEnv,
        for_megagroup: bool,
    ) -> td_api::ChatBoostFeatures {
        let key_prefix = if for_megagroup { "group" } else { "channel" };
        let min = |name: &str| {
            env.get_option_integer(&format!("{key_prefix}_{name}_level_min"), 1_000_000_000) as i32
        };
        let min_profile_background_custom_emoji_boost_level = min("profile_bg_icon");
        let min_background_custom_emoji_boost_level = min("bg_icon");
        let min_emoji_status_boost_level = min("emoji_status");
        let min_chat_theme_background_boost_level = min("wallpaper");
        let min_custom_background_boost_level = min("custom_wallpaper");
        let min_custom_emoji_sticker_set_boost_level = min("emoji_stickers");
        let min_automatic_translation_boost_level = min("autotranslation");
        let min_speech_recognition_boost_level = min("transcribe");
        let min_sponsored_message_disable_boost_level = min("restrict_sponsored");
        let mut features = Vec::new();
        let max_level = env.get_option_integer("chat_boost_level_max", 10) as i32;
        for level in 1..=max_level {
            features.push(self.get_chat_boost_level_features_object(env, for_megagroup, level));
        }
        td_api::ChatBoostFeatures {
            features,
            min_profile_background_custom_emoji_boost_level,
            min_background_custom_emoji_boost_level,
            min_emoji_status_boost_level,
            min_chat_theme_background_boost_level,
            min_custom_background_boost_level,
            min_custom_emoji_sticker_set_boost_level,
            min_automatic_translation_boost_level,
            min_speech_recognition_boost_level,
            min_sponsored_message_disable_boost_level,
        }
    }

    /// `get_boost_slots` (cpp:416-419) — the query is the seam.
    pub fn get_boost_slots(&mut self, env: &mut impl BoostManagerEnv, promise: Promise<td_api::ChatBoostSlots>) {
        env.send_get_my_boosts_query();
        self.parked_slots.push(promise);
    }

    /// `get_dialog_boost_status` (cpp:420-426).
    pub fn get_dialog_boost_status(
        &mut self,
        env: &mut impl BoostManagerEnv,
        dialog_id: DialogId,
        promise: Promise<td_api::ChatBoostStatus>,
    ) {
        if !dialog_id.is_valid() {
            return;
        }
        env.send_get_boosts_status_query(dialog_id);
        self.parked_status.push(promise);
    }

    /// `boost_dialog` (cpp:427-433) — the empty-slot list answers the
    /// slots query.
    pub fn boost_dialog(
        &mut self,
        env: &mut impl BoostManagerEnv,
        dialog_id: DialogId,
        slot_ids: &[i32],
        mut promise: Promise<td_api::ChatBoostSlots>,
    ) {
        if let Err(status) = env.check_dialog_access(dialog_id) {
            return promise.set_error(status);
        }
        if slot_ids.is_empty() {
            return self.get_boost_slots(env, promise);
        }
        env.send_apply_boost_query(dialog_id, slot_ids);
        self.parked_slots.push(promise);
    }

    /// `get_dialog_boost_link` (cpp:434-457) — the local link build (the
    /// `get_t_me_url` default; the option gate joins M.23).
    pub fn get_dialog_boost_link(
        &mut self,
        env: &mut impl BoostManagerEnv,
        dialog_id: DialogId,
    ) -> std::result::Result<(String, bool), Status> {
        env.check_dialog_access(dialog_id)?;
        if dialog_id.get_type() != crate::dialog_id::DialogType::Channel {
            return Err(Status::error(400, "Can't boost the chat"));
        }
        let channel_id = dialog_id.get_channel_id();
        let username = env.get_channel_first_username(channel_id);
        let is_public = !username.is_empty();
        let link = if is_public {
            format!("https://t.me/boost/{username}")
        } else {
            format!("https://t.me/boost?c={}", channel_id.get())
        };
        Ok((link, is_public))
    }

    /// `get_chat_boost_link_info_object` (cpp:470-479) — the dialog
    /// resolution is the M.6 seam (the chat id answers the raw id).
    pub fn get_chat_boost_link_info_object(
        &self,
        info: &DialogBoostLinkInfo,
    ) -> td_api::ChatBoostLinkInfo {
        let is_public = !info.username.is_empty();
        let dialog_id = if is_public {
            DialogId::default()
        } else {
            DialogId::from_channel_id(info.channel_id)
        };
        td_api::ChatBoostLinkInfo {
            is_public,
            chat_id: dialog_id.get(),
        }
    }

    /// `get_dialog_boosts` (cpp:481-490).
    pub fn get_dialog_boosts(
        &mut self,
        env: &mut impl BoostManagerEnv,
        dialog_id: DialogId,
        only_gift_codes: bool,
        offset: &str,
        limit: i32,
        promise: Promise<td_api::FoundChatBoosts>,
    ) {
        env.send_get_boosts_list_query(dialog_id, only_gift_codes, offset, limit);
        self.parked_found_boosts.push(promise);
    }

    /// `get_user_dialog_boosts` (cpp:492-502).
    pub fn get_user_dialog_boosts(
        &mut self,
        env: &mut impl BoostManagerEnv,
        dialog_id: DialogId,
        user_id: i64,
        promise: Promise<td_api::FoundChatBoosts>,
    ) {
        env.send_get_user_boosts_query(dialog_id, user_id);
        self.parked_found_boosts.push(promise);
    }

    /// `on_update_dialog_boost` (cpp:503-521) — the boost intake; the
    /// C++ emits updateChatBoost via the update builders (recorded seam —
    /// the update joins the list-answer converters).
    pub fn on_update_dialog_boost(&mut self, env: &mut impl BoostManagerEnv, dialog_id: DialogId) {
        let _ = (env, dialog_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct TestEnv {
        options: BTreeMap<String, i64>,
        queries: Vec<String>,
        username: String,
    }

    impl BoostManagerEnv for TestEnv {
        fn close_flag(&self) -> bool {
            false
        }

        fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
            self.options.get(name).copied().unwrap_or(default_value)
        }

        fn send_update(&mut self, _update: td_api::Update) {}

        fn check_dialog_access(&mut self, dialog_id: DialogId) -> std::result::Result<(), Status> {
            if dialog_id.is_valid() {
                Ok(())
            } else {
                Err(Status::error(400, "Chat not found"))
            }
        }

        fn get_channel_first_username(&mut self, _channel_id: ChannelId) -> String {
            self.username.clone()
        }

        fn send_get_my_boosts_query(&mut self) {
            self.queries.push("getMyBoosts".to_string());
        }

        fn send_get_boosts_status_query(&mut self, dialog_id: DialogId) {
            self.queries.push(format!("getBoostsStatus {}", dialog_id.get()));
        }

        fn send_apply_boost_query(&mut self, dialog_id: DialogId, slot_ids: &[i32]) {
            self.queries
                .push(format!("applyBoost {} {:?}", dialog_id.get(), slot_ids));
        }

        fn send_get_boosts_list_query(
            &mut self,
            dialog_id: DialogId,
            only_gift_codes: bool,
            offset: &str,
            limit: i32,
        ) {
            self.queries
                .push(format!("getBoostsList {} {only_gift_codes} {offset} {limit}", dialog_id.get()));
        }

        fn send_get_user_boosts_query(&mut self, dialog_id: DialogId, user_id: i64) {
            self.queries
                .push(format!("getUserBoosts {} {user_id}", dialog_id.get()));
        }
    }

    fn env() -> TestEnv {
        let mut options = BTreeMap::new();
        options.insert("chat_boost_level_max".to_string(), 2);
        options.insert("channel_custom_wallpaper_level_min".to_string(), 1);
        options.insert("channel_transcribe_level_min".to_string(), 2);
        TestEnv {
            options,
            queries: Vec::new(),
            username: String::new(),
        }
    }

    #[test]
    fn level_features_read_the_options() {
        let mut env = env();
        let mut manager = BoostManager::new();
        let level_1 = manager.get_chat_boost_level_features_object(&mut env, false, 1);
        assert_eq!(level_1.level, 1);
        assert!(level_1.can_set_custom_background);
        assert!(!level_1.can_recognize_speech);
        let level_2 = manager.get_chat_boost_level_features_object(&mut env, false, 2);
        assert!(level_2.can_recognize_speech);
        let level_5 = manager.get_chat_boost_level_features_object(&mut env, false, 5);
        assert_eq!(level_5.story_per_day_count, 2, "the clamp to the max level");
    }

    #[test]
    fn features_build_the_level_list_and_mins() {
        let mut env = env();
        let mut manager = BoostManager::new();
        let features = manager.get_chat_boost_features_object(&mut env, false);
        assert_eq!(features.features.len(), 2);
        assert_eq!(features.min_custom_background_boost_level, 1);
        assert_eq!(features.min_speech_recognition_boost_level, 2);
    }

    #[test]
    fn links_build_public_and_private_forms() {
        let mut env = env();
        let mut manager = BoostManager::new();
        let channel_id = ChannelId::new(1000); // channel ids are the positive band
        let (link, is_public) = manager
            .get_dialog_boost_link(&mut env, DialogId::from_channel_id(channel_id))
            .unwrap();
        assert!(!is_public);
        assert_eq!(link, format!("https://t.me/boost?c={}", channel_id.get()));

        env.username = "durov".to_string();
        let (link, is_public) = manager
            .get_dialog_boost_link(&mut env, DialogId::from_channel_id(channel_id))
            .unwrap();
        assert!(is_public);
        assert_eq!(link, "https://t.me/boost/durov");
    }

    #[test]
    fn the_router_dispatches_through_the_seams() {
        let mut env = env();
        let mut manager = BoostManager::new();
        manager.get_boost_slots(&mut env, Promise::default());
        manager.get_dialog_boost_status(&mut env, DialogId::new(1), Promise::default());
        manager.boost_dialog(&mut env, DialogId::new(1), &[1, 2], Promise::default());
        manager.get_dialog_boosts(&mut env, DialogId::new(1), true, "off", 10, Promise::default());
        manager.get_user_dialog_boosts(&mut env, DialogId::new(1), 5, Promise::default());
        assert!(env.queries.iter().any(|q| q == "getMyBoosts"));
        assert!(env.queries.iter().any(|q| q == "getBoostsStatus 1"));
        assert!(env.queries.iter().any(|q| q == "applyBoost 1 [1, 2]"));
        assert!(env.queries.iter().any(|q| q == "getBoostsList 1 true off 10"));
        assert!(env.queries.iter().any(|q| q == "getUserBoosts 1 5"));
        assert_eq!(manager.parked_slots.len(), 2);
        assert_eq!(manager.parked_status.len(), 1);
        assert_eq!(manager.parked_found_boosts.len(), 2);

        // empty slots redirect to the slots query
        let count = env.queries.len();
        manager.boost_dialog(&mut env, DialogId::new(1), &[], Promise::default());
        assert_eq!(env.queries.len(), count + 1, "getMyBoosts again");
    }
}
