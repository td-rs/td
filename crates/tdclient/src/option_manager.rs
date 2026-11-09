//! Port of `td/telegram/OptionManager.{h,cpp}` (S69): the string-keyed
//! option store — `TsSeqKeyValue` in-memory + the binlog config pmc —
//! with the `setOption` validation table, the constructor default table,
//! the premium-derived limits and `getCurrentState`.
//!
//! Divergences (all documented inline):
//! - `is_bot` is `false` until the AuthManager lands (S71) — matches an
//!   unauthorized client, where the C++ also evaluates the non-bot branches.
//! - The `on_option_updated` manager fan-out stays partial (the manager
//!   call sites drive their halves directly); the internal-option updates
//!   (`default_reaction` → updateDefaultReactionType, `otherwise_relogin_days`
//!   → updateSuggestedActions, `whitelisted_bots` → updateTrustedMiniAppBots)
//!   are implemented at both emission sites (set_option + the pmc-restore
//!   loop) — surfaced by the S75 live user-path gate 2026-09-28.
//! - `online` is served from the store-less `OnlineManager` and `unix_time`
//!   from the server-time-corrected `G()->unix_time()`; both report the
//!   interim local values until those managers land.
//! - `set_option`'s `updateOption` broadcasts are queued in
//!   [`OptionManager::take_updates`] for the Td actor (S70) to drain.
//! - The ConfigManager branches of `get_option`/`set_option` (the
//!   `*_sensitive_content_restrictions` names) are routed through the
//!   ConfigManager by the `requests` dispatch (like the C++
//!   `send_closure_later`); `set_option_from_api` returns the
//!   [`SetOptionAction`] that triggers the handoff.

use crate::td_api;
use crate::td_api::OptionValue;
use tddb::binlog_key_value::BinlogKeyValue;
use tddb::seq_key_value::SeqKeyValue;

/// `full_split` + `to_integer<int64>` over a comma-joined id list — the
/// C++ `transform(full_split(...), to_integer<int64>)` in
/// `get_internal_option_update`. `to_integer` stops at the first
/// non-digit and yields 0 for garbage, never panics.
fn full_split_ids(value: &str) -> Vec<i64> {
    fn to_integer_i64(s: &str) -> i64 {
        let s = s.strip_prefix('-').unwrap_or(s);
        let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return 0;
        }
        digits.parse::<i64>().unwrap_or(0)
    }
    value
        .split(',')
        .filter(|part| !part.is_empty())
        .map(to_integer_i64)
        .collect()
}

/// The `set_option_from_api` outcomes: the store write completed, or the
/// ConfigManager must take over (the C++ `OptionManager::set_option`
/// `send_closure_later` to `ConfigManager::set_content_settings`).
#[derive(Debug, PartialEq, Eq)]
pub enum SetOptionAction {
    Done,
    SetContentSettings { value: bool },
}

/// The API version, hardcoded like `OptionManager.cpp` ("1.8.70" — pinned
/// to the reference C++ version for JSON parity).
pub const API_VERSION: &str = "1.8.70";

/// `OptionManager::get_synchronous_options`: `{"version", "commit_hash"}`.
pub const VERSION: &str = "1.8.70";

pub fn get_synchronous_options() -> &'static [&'static str] {
    &["version", "commit_hash"]
}

/// `OptionManager::is_synchronous_option`.
pub fn is_synchronous_option(name: &str) -> bool {
    get_synchronous_options().contains(&name)
}

/// `td::get_git_commit_hash` — the C++ bakes its td checkout's
/// `git rev-parse HEAD` into the generated `git_version.h` at build time;
/// the port reports the tracked upstream td commit instead (the same pin
/// the tl-codegen schemes carry — update both in the sync step). The
/// build-d1 C++ oracle reports its own older commit (`d1085f9c…`), so the
/// values legitimately differ; the shape must not.
const UPSTREAM_TD_COMMIT: &str = "c15d3f5a5de6e3ba5839822c451152e5e18bb700";

pub fn get_git_commit_hash() -> &'static str {
    UPSTREAM_TD_COMMIT
}

/// `OptionManager::get_option_synchronously` (the C++ `UNREACHABLE` on an
/// unknown name becomes a debug assert + a `None`).
pub fn get_option_synchronously(name: &str) -> Option<td_api::OptionValue> {
    debug_assert!(!name.is_empty());
    match name {
        "commit_hash" => Some(option_value_string(get_git_commit_hash())),
        "version" => Some(option_value_string(API_VERSION)),
        _ => None,
    }
}

fn option_value_string(value: &str) -> OptionValue {
    OptionValue::optionValueString(td_api::optionValueString {
        value: value.to_string(),
    })
}

/// `UserManager::get_*_user_id` — the bot-id constants behind the
/// constructor's first six defaults (test-DC variants like the C++).
const SERVICE_NOTIFICATIONS_USER_ID: i64 = 777000;
const REPLIES_BOT_USER_ID: i64 = 1271266957;
const REPLIES_BOT_USER_ID_TEST_DC: i64 = 708513;
const VERIFICATION_CODES_BOT_USER_ID: i64 = 489000;
const ANONYMOUS_BOT_USER_ID: i64 = 1087968824;
const ANONYMOUS_BOT_USER_ID_TEST_DC: i64 = 552888;
const CHANNEL_BOT_USER_ID: i64 = 136817688;
const CHANNEL_BOT_USER_ID_TEST_DC: i64 = 936174;
const ANTI_SPAM_BOT_USER_ID: i64 = 5434988373;
const ANTI_SPAM_BOT_USER_ID_TEST_DC: i64 = 2200583762;

/// `NotificationManager` bounds (NotificationManager.h:51-54).
const MIN_NOTIFICATION_GROUP_COUNT_MAX: i64 = 0;
const MAX_NOTIFICATION_GROUP_COUNT_MAX: i64 = 25;
const MIN_NOTIFICATION_GROUP_SIZE_MAX: i64 = 1;
const MAX_NOTIFICATION_GROUP_SIZE_MAX: i64 = 25;

/// `OptionManager::is_internal_option` — the exact C++ name set.
fn is_internal_option(name: &str) -> bool {
    const INTERNAL: &[&str] = &[
        "about_length_limit_default",
        "about_length_limit_premium",
        "aggressive_anti_spam_supergroup_member_count_min",
        "aicompose_tone_saved_limit_default",
        "aicompose_tone_saved_limit_premium",
        "animated_emoji_zoom",
        "animation_search_emojis",
        "animation_search_provider",
        "authorization_autoconfirm_period",
        "base_language_pack_version",
        "bot_allowed_suffixes",
        "bots_create_limit_default",
        "bots_create_limit_premium",
        "business_features",
        "call_receive_timeout_ms",
        "call_ring_timeout_ms",
        "can_edit_fact_check",
        "caption_length_limit_default",
        "caption_length_limit_premium",
        "channel_autotranslation_level_min",
        "channel_bg_icon_level_min",
        "channel_custom_wallpaper_level_min",
        "channel_emoji_status_level_min",
        "channel_profile_bg_icon_level_min",
        "channel_restrict_sponsored_level_min",
        "channel_wallpaper_level_min",
        "channels_limit_default",
        "channels_limit_premium",
        "channels_public_limit_default",
        "channels_public_limit_premium",
        "channels_read_media_period",
        "chat_read_mark_expire_period",
        "chat_read_mark_size_threshold",
        "chatlist_invites_limit_default",
        "chatlist_invites_limit_premium",
        "chatlists_joined_limit_default",
        "chatlists_joined_limit_premium",
        "dc_txt_domain_name",
        "default_reaction",
        "default_reaction_needs_sync",
        "dialog_filters_chats_limit_default",
        "dialog_filters_chats_limit_premium",
        "dialog_filters_limit_default",
        "dialog_filters_limit_premium",
        "dialogs_folder_pinned_limit_default",
        "dialogs_folder_pinned_limit_premium",
        "dialogs_pinned_limit_default",
        "dialogs_pinned_limit_premium",
        "dice_emojis",
        "dice_success_values",
        "dismiss_birthday_contact_today",
        "edit_time_limit",
        "emoji_sounds",
        "fragment_prefixes",
        "group_call_message_show_time_max",
        "group_custom_wallpaper_level_min",
        "group_emoji_status_level_min",
        "group_emoji_stickers_level_min",
        "group_profile_bg_icon_level_min",
        "group_transcribe_level_min",
        "group_wallpaper_level_min",
        "has_protected_content_disable_request_duration",
        "hidden_members_group_size_min",
        "ignored_restriction_reasons",
        "language_pack_version",
        "message_length_limit_default",
        "message_length_limit_premium",
        "my_phone_number",
        "need_premium_for_new_chat_privacy",
        "need_premium_for_story_caption_entities",
        "need_synchronize_archive_all_stories",
        "notification_cloud_delay_ms",
        "notification_default_delay_ms",
        "online_cloud_timeout_ms",
        "online_update_period_ms",
        "otherwise_relogin_days",
        "phone_country_iso2",
        "pm_read_date_expire_period",
        "poll_answer_delete_period",
        "premium_bot_username",
        "premium_features",
        "premium_invoice_slug",
        "premium_manage_subscription_url",
        "rating_e_decay",
        "reactions_uniq_max",
        "reactions_user_max_default",
        "reactions_user_max_premium",
        "recent_stickers_limit",
        "recommended_channels_limit_default",
        "recommended_channels_limit_premium",
        "restriction_add_platforms",
        "revoke_pm_inbox",
        "revoke_pm_time_limit",
        "revoke_time_limit",
        "saved_animations_limit",
        "saved_dialogs_pinned_limit_default",
        "saved_dialogs_pinned_limit_premium",
        "saved_gifs_limit_default",
        "saved_gifs_limit_premium",
        "session_count",
        "since_last_open",
        "stargifts_craft_attribute_permilles",
        "starref_start_param_prefixes",
        "stickers_faved_limit_default",
        "stickers_faved_limit_premium",
        "stickers_normal_by_emoji_per_premium_num",
        "stickers_premium_by_emoji_num",
        "stories_changelog_user_id",
        "stories_sent_monthly_limit_default",
        "stories_sent_monthly_limit_premium",
        "stories_sent_weekly_limit_default",
        "stories_sent_weekly_limit_premium",
        "stories_suggested_reactions_limit_default",
        "stories_suggested_reactions_limit_premium",
        "story_caption_length_limit_default",
        "story_caption_length_limit_premium",
        "story_expiring_limit_default",
        "story_expiring_limit_premium",
        "ton_proxy_address",
        "ton_stakedice_stake_suggested_amounts",
        "upload_premium_speedup_notify_period",
        "video_ignore_alt_documents",
        "video_note_size_max",
        "weather_bot_username",
        "webfile_dc_id",
        "whitelisted_bots",
    ];
    INTERNAL.contains(&name)
}

/// The option store: the C++ `OptionManager` fields minus the Td pointer.
/// `options_` (TsSeqKeyValue) + `option_pmc_` (config pmc) + the pending
/// pre-init getters + the queued `updateOption` broadcasts.
pub struct OptionManager {
    options: SeqKeyValue,
    option_pmc: Option<BinlogKeyValue>,
    is_test_dc: bool,
    is_td_inited: bool,
    /// The queued pre-init `getOption` requests (the C++
    /// `pending_get_options_`), with their request ids.
    pending_get_options: Vec<(u64, String)>,
    pending_updates: Vec<td_api::Object>,
    /// `last_sent_server_time_difference_` (OptionManager.h:95) — the
    /// `unix_time` notification is suppressed while the difference moved
    /// less than 0.5 since the last sent update.
    last_sent_server_time_difference: f64,
}

impl OptionManager {
    /// The C++ constructor: load the pmc (skipping `utc_time_offset`),
    /// apply the default table and the erase list, then the premium-derived
    /// limits when server options are present.
    pub fn new(is_test_dc: bool) -> Self {
        let mut manager = Self {
            options: SeqKeyValue::default(),
            option_pmc: None,
            is_test_dc,
            is_td_inited: false,
            pending_get_options: Vec::new(),
            pending_updates: Vec::new(),
            last_sent_server_time_difference: 1e100,
        };
        manager.set_option_integer(
            "utc_time_offset",
            // `Clocks::tz_offset()`: seconds east of UTC for the local zone.
            tz_offset() as i64,
        );
        manager.apply_defaults();
        manager
    }

    /// `OptionManager` constructor pmc replay (the C++
    /// `option_pmc_->for_each` over `get_config_pmc()`, OptionManager.cpp
    /// :59-160): rebuild in the C++ constructor's exact order — the
    /// opening `unix_time` update, the pmc replay (each persisted option
    /// re-emitted directly), `utc_time_offset`, then the default table
    /// with its isset skip — so a persisted value suppresses its ctor
    /// default exactly like the C++ (a reopen never re-emits the defaults
    /// the earlier run overrode, and a fresh run persists the applied
    /// defaults to the pmc like the C++ default lambda's
    /// `option_pmc_->set`). The `new()`-time defaults were premature:
    /// they ran before the replay could fill the store, so both the
    /// store and the pending queue start over here. `unix_time` is the
    /// Global-corrected time for the opening update.
    pub fn attach_pmc(&mut self, pmc: BinlogKeyValue, unix_time: i32, server_time_difference: f64) {
        self.options = SeqKeyValue::default();
        self.pending_updates.clear();
        self.send_unix_time_update(unix_time, server_time_difference);
        for (name, value) in pmc.get_all() {
            if name == "utc_time_offset" {
                continue;
            }
            self.options.set(&name, &value);
            if !is_internal_option(&name) {
                self.pending_updates
                    .push(td_api::Object::updateOption(td_api::updateOption {
                        name,
                        value: Some(Self::get_option_value_object(&value)),
                    }));
            } else if let Some(update) = self.get_internal_option_update(&name) {
                self.pending_updates.push(update.into());
            }
        }
        let utc_time_offset = format!("I{}", tz_offset() as i64);
        self.options.set("utc_time_offset", &utc_time_offset);
        self.pending_updates
            .push(td_api::Object::updateOption(td_api::updateOption {
                name: "utc_time_offset".to_string(),
                value: Some(Self::get_option_value_object(&utc_time_offset)),
            }));
        self.option_pmc = Some(pmc);
        self.apply_defaults();
    }

    /// `OptionManager::on_update_server_time_difference`
    /// (OptionManager.cpp:431-438): re-send the `unix_time` option only
    /// when the difference moved by 0.5 or more.
    pub fn on_update_server_time_difference(
        &mut self,
        server_time_difference: f64,
        unix_time: i32,
    ) {
        if (server_time_difference - self.last_sent_server_time_difference).abs() < 0.5 {
            return;
        }
        self.send_unix_time_update(unix_time, server_time_difference);
    }

    /// `OptionManager::send_unix_time_update` (OptionManager.cpp:426-429):
    /// every send re-anchors `last_sent_server_time_difference_` — without
    /// it the constructor's opening update doesn't count, and the first
    /// post-init time-diff notification re-sends `unix_time` no matter how
    /// small the move. The corrected time arrives from the Global instead
    /// of `G()`.
    fn send_unix_time_update(&mut self, unix_time: i32, server_time_difference: f64) {
        self.last_sent_server_time_difference = server_time_difference;
        self.pending_updates
            .push(td_api::Object::updateOption(td_api::updateOption {
                name: "unix_time".to_string(),
                value: Some(OptionValue::optionValueInteger(
                    td_api::optionValueInteger {
                        value: unix_time as i64,
                    },
                )),
            }));
    }

    /// `OptionManager::set_option` (the internal Slice form): write the
    /// store, mirror to the pmc, then queue the `updateOption` broadcast.
    /// Returns whether the value changed (the C++ SeqKeyValue early-return).
    pub fn set_option(&mut self, name: &str, value: &str) -> bool {
        debug_assert!(!name.is_empty());
        let changed = if value.is_empty() {
            self.options.erase(name) != 0
        } else {
            self.options.set(name, value) != 0
        };
        if !changed {
            return false;
        }
        if let Some(pmc) = &mut self.option_pmc {
            if value.is_empty() {
                pmc.erase(name);
            } else {
                pmc.set(name, value);
            }
        }
        // The C++ also calls on_option_updated(name) here — all its manager
        // fan-outs are TODO until the managers land (see the module docs).
        if !is_internal_option(name) {
            self.pending_updates
                .push(td_api::Object::updateOption(td_api::updateOption {
                    name: name.to_string(),
                    value: Some(Self::get_option_value_object(&self.get_option(name))),
                }));
        } else if let Some(update) = self.get_internal_option_update(name) {
            self.pending_updates.push(update.into());
        }
        true
    }

    /// `get_internal_option_update` (OptionManager.cpp:569-585): the
    /// dedicated update an internal option emits instead of `updateOption`
    /// — `updateTrustedMiniAppBots` for `whitelisted_bots` (always, even
    /// empty), `updateDefaultReactionType` for `default_reaction`, and the
    /// SetPassword suggested action for a positive `otherwise_relogin_days`.
    fn get_internal_option_update(&self, name: &str) -> Option<td_api::Update> {
        match name {
            "bot_allowed_suffixes" => {
                // OptionManager.cpp:574-577 — full_split on the space-
                // joined option (an empty option answers no suffixes).
                let suffixes: Vec<String> = self
                    .get_option_string(name, "")
                    .split(' ')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                Some(td_api::Update::updateAllowedBotUsernamePremiumSuffixes(
                    td_api::updateAllowedBotUsernamePremiumSuffixes { suffixes },
                ))
            }
            "default_reaction" => {
                crate::reaction_type::ReactionType::new(self.get_option_string(name, ""))
                    .get_update_default_reaction_type()
            }
            "otherwise_relogin_days" => {
                // `narrow_cast<int32>` — the option never carries a value
                // beyond i32 in practice.
                let days = self.get_option_integer(name, 0) as i32;
                if days > 0 {
                    Some(
                        crate::suggested_action::get_update_suggested_actions_object(
                            None,
                            &[crate::suggested_action::SuggestedAction {
                                type_: crate::suggested_action::SuggestedActionType::SetPassword,
                                otherwise_relogin_days: days,
                                ..Default::default()
                            }],
                            &[],
                            "get_internal_option_update",
                            &self.get_option_string("premium_manage_subscription_url", ""),
                        ),
                    )
                } else {
                    None
                }
            }
            "whitelisted_bots" => Some(td_api::Update::updateTrustedMiniAppBots(
                td_api::updateTrustedMiniAppBots {
                    bot_user_ids: full_split_ids(&self.get_option_string(name, "")),
                },
            )),
            _ => None,
        }
    }

    pub fn set_option_boolean(&mut self, name: &str, value: bool) -> bool {
        self.set_option(name, if value { "Btrue" } else { "Bfalse" })
    }

    pub fn set_option_empty(&mut self, name: &str) -> bool {
        self.set_option(name, "")
    }

    pub fn set_option_integer(&mut self, name: &str, value: i64) -> bool {
        self.set_option(name, &format!("I{value}"))
    }

    pub fn set_option_string(&mut self, name: &str, value: &str) -> bool {
        self.set_option(name, &format!("S{value}"))
    }

    /// `OptionManager::have_option`.
    pub fn have_option(&self, name: &str) -> bool {
        self.options.isset(name)
    }

    /// The whole in-memory store, sorted — the snapshot tests read the
    /// store the way the C++ `TsSeqKeyValue::foreach` would.
    #[cfg(test)]
    pub fn all_options(&self) -> Vec<(String, String)> {
        let mut all: Vec<(String, String)> = self.options.get_all().into_iter().collect();
        all.sort();
        all
    }

    /// The raw encoded value (`get_option` returns a Slice in the C++).
    pub fn get_option(&self, name: &str) -> String {
        self.options.get(name)
    }

    /// `OptionManager::get_option_boolean`.
    pub fn get_option_boolean(&self, name: &str, default_value: bool) -> bool {
        match self.get_option(name).as_str() {
            "" => default_value,
            "Btrue" => true,
            "Bfalse" => false,
            value => {
                tdutils::log_error!("Found \"{value}\" instead of boolean option {name}");
                default_value
            }
        }
    }

    /// `OptionManager::get_option_integer`.
    pub fn get_option_integer(&self, name: &str, default_value: i64) -> i64 {
        let value = self.get_option(name);
        match value.strip_prefix('I') {
            Some(digits) => digits.parse().unwrap_or_else(|_| {
                tdutils::log_error!("Found \"{value}\" instead of integer option {name}");
                default_value
            }),
            None => {
                if !value.is_empty() {
                    tdutils::log_error!("Found \"{value}\" instead of integer option {name}");
                }
                default_value
            }
        }
    }

    /// `OptionManager::get_option_string`.
    pub fn get_option_string(&self, name: &str, default_value: &str) -> String {
        let value = self.get_option(name);
        match value.strip_prefix('S') {
            Some(payload) => payload.to_string(),
            None => {
                if !value.is_empty() {
                    tdutils::log_error!("Found \"{value}\" instead of string option {name}");
                }
                default_value.to_string()
            }
        }
    }

    /// `OptionManager::get_option_value_object` (Slice → td_api).
    pub fn get_option_value_object(value: &str) -> OptionValue {
        if value.is_empty() {
            return OptionValue::optionValueEmpty(td_api::optionValueEmpty);
        }
        match value.as_bytes()[0] {
            b'B' => match value {
                "Btrue" => {
                    OptionValue::optionValueBoolean(td_api::optionValueBoolean { value: true })
                }
                "Bfalse" => {
                    OptionValue::optionValueBoolean(td_api::optionValueBoolean { value: false })
                }
                _ => unreachable!(),
            },
            b'I' => OptionValue::optionValueInteger(td_api::optionValueInteger {
                value: value[1..].parse().unwrap(),
            }),
            b'S' => option_value_string(&value[1..]),
            _ => unreachable!(),
        }
    }

    /// `OptionManager::get_option(name, promise)`: the store lookup with the
    /// async/live special cases. The ConfigManager branch for the
    /// `*_sensitive_content_restrictions` names is intercepted by the
    /// `requests` dispatch first (the C++ `send_closure_later`); the
    /// remaining NotificationManager/OnlineManager branches report the
    /// interim values until their managers land (Wave 1). `unix_time` is
    /// the Global-corrected `G()->unix_time()` for the live option.
    pub fn get_option_value(&self, name: &str, unix_time: i32) -> OptionValue {
        match name {
            "can_ignore_sensitive_content_restrictions"
            | "ignore_sensitive_content_restrictions" => {
                // Routed through ConfigManager::get_content_settings by the
                // getOption dispatch; this arm answers the plain store
                // value for any non-td_api caller.
                Self::get_option_value_object(&self.get_option(name))
            }
            "disable_contact_registered_notifications" => {
                // The NotificationManager getOption routes through the
                // contact-sign-up wire family at the `requests` dispatch
                // (the net-drain 61 slice, OptionManager.cpp:775-782);
                // the value itself is the plain store read (the wrap_promise
                // re-read)
                Self::get_option_value_object(&self.get_option(name))
            }
            "online" => {
                // OnlineManager::is_online — TODO (Wave 1).
                OptionValue::optionValueEmpty(td_api::optionValueEmpty)
            }
            "unix_time" => OptionValue::optionValueInteger(td_api::optionValueInteger {
                // `get_unix_time_option_value_object` → `G()->unix_time()`.
                value: unix_time as i64,
            }),
            _ => Self::get_option_value_object(&self.get_option(name)),
        }
    }

    /// `OptionManager::set_option(name, value, promise)` — the `setOption`
    /// td_api dispatch with the C++ validation table and error strings.
    /// `is_bot` is `false` until the AuthManager lands (S71), exactly like
    /// an unauthorized client.
    pub fn set_option_from_api(
        &mut self,
        name: &str,
        value: &OptionValue,
    ) -> std::result::Result<SetOptionAction, String> {
        let is_bot = false;
        let value_id = value_constructor_id(value);

        // The three C++ lambda helpers, inlined for the Rust match below.
        // set_integer_option: OptionValueEmpty erases; else must be Integer
        // in [min, max].
        macro_rules! set_integer_option {
            ($option_name:literal, $min:expr, $max:expr) => {
                if name == $option_name {
                    match value {
                        OptionValue::optionValueEmpty(_) => {
                            self.set_option_empty(name);
                            return Ok(SetOptionAction::Done);
                        }
                        OptionValue::optionValueInteger(v) => {
                            if v.value < $min || v.value > $max {
                                return Err(format!(
                                    "Option's \"{name}\" value {} is outside of the valid range [{}, {}]",
                                    v.value, $min, $max
                                ));
                            }
                            self.set_option_integer(name, v.value);
                            return Ok(SetOptionAction::Done);
                        }
                        _ => {
                            return Err(format!(
                                "Option \"{name}\" must have integer value"
                            ));
                        }
                    }
                }
            };
        }
        macro_rules! set_boolean_option {
            ($option_name:literal) => {
                if name == $option_name {
                    match value {
                        OptionValue::optionValueEmpty(_) => {
                            self.set_option_empty(name);
                            return Ok(SetOptionAction::Done);
                        }
                        OptionValue::optionValueBoolean(v) => {
                            self.set_option_boolean(name, v.value);
                            return Ok(SetOptionAction::Done);
                        }
                        _ => {
                            return Err(format!("Option \"{name}\" must have boolean value"));
                        }
                    }
                }
            };
        }
        macro_rules! set_string_option {
            ($option_name:literal, $check:expr) => {
                if name == $option_name {
                    match value {
                        OptionValue::optionValueEmpty(_) => {
                            self.set_option_empty(name);
                            return Ok(SetOptionAction::Done);
                        }
                        OptionValue::optionValueString(v) => {
                            if v.value.is_empty() {
                                self.set_option_empty(name);
                            } else if $check(&v.value) {
                                self.set_option_string(name, &v.value);
                            } else {
                                return Err(format!(
                                    "Option \"{name}\" can't have specified value"
                                ));
                            }
                            return Ok(SetOptionAction::Done);
                        }
                        _ => {
                            return Err(format!("Option \"{name}\" must have string value"));
                        }
                    }
                }
            };
        }

        match name.chars().next().unwrap_or('\0') {
            'a' => {
                set_boolean_option!("always_parse_markdown");
            }
            'c' if !is_bot => {
                set_string_option!("connection_parameters", |value: &str| {
                    // Must be a valid JSON object
                    // (`get_json_value` + jsonValueObject check).
                    match tdutils::json::json_decode(value) {
                        tdutils::status::Result::Ok(json_value) => {
                            json_value.ty() == tdutils::json::JsonValueType::Object
                        }
                        tdutils::status::Result::Err(_) => false,
                    }
                });
            }
            'd' => {
                if !is_bot {
                    set_boolean_option!("disable_animated_emoji");
                    set_boolean_option!("disable_contact_registered_notifications");
                }
                set_boolean_option!("disable_network_statistics");
                set_boolean_option!("disable_persistent_network_statistics");
                if !is_bot {
                    set_boolean_option!("disable_sent_scheduled_message_notifications");
                }
                set_boolean_option!("disable_time_adjustment_protection");
                if !is_bot {
                    set_boolean_option!("disable_top_chats");
                }
                if name == "drop_notification_ids" {
                    // Any value; erases the binlog pmc notification ids.
                    // TODO (TdDb binlog pmc, S44 wiring): the C++ erases
                    // "notification_id_current"/"notification_group_id_current".
                    return Ok(SetOptionAction::Done);
                }
            }
            'i' => {
                set_boolean_option!("ignore_background_updates");
                set_boolean_option!("ignore_default_disable_notification");
                set_boolean_option!("ignore_file_names");
                set_boolean_option!("ignore_inline_thumbnails");
                set_boolean_option!("ignore_platform_restrictions");
                set_boolean_option!("is_emulator");
                if !is_bot && name == "ignore_sensitive_content_restrictions" {
                    if !self.get_option_boolean("can_ignore_sensitive_content_restrictions", false)
                    {
                        return Err(
                            "Option \"ignore_sensitive_content_restrictions\" can't be changed by the user"
                                .to_string(),
                        );
                    }
                    if value_id != CONSTRUCTOR_BOOL && value_id != CONSTRUCTOR_EMPTY {
                        return Err(
                            "Option \"ignore_sensitive_content_restrictions\" must have boolean value"
                                .to_string(),
                        );
                    }
                    // ConfigManager::set_content_settings — the setOption
                    // answers when the account.setContentSettings query
                    // completes (requests.rs hands the action off).
                    let value = value_id == CONSTRUCTOR_BOOL
                        && matches!(value, OptionValue::optionValueBoolean(v) if v.value);
                    return Ok(SetOptionAction::SetContentSettings { value });
                }
            }
            'l' if !is_bot => {
                set_string_option!("language_pack_database_path", |_| true);
                // LanguagePackManager::check_language_code_name (M.24)
                set_string_option!(
                    "language_pack_id",
                    crate::language_pack_manager::check_language_code_name
                );
                // LanguagePackManager::check_language_pack_name (M.24)
                set_string_option!(
                    "localization_target",
                    crate::language_pack_manager::check_language_pack_name
                );
            }
            'm' => {
                set_integer_option!("message_unload_delay", 60, 86400);
            }
            'n' if !is_bot => {
                set_integer_option!(
                    "notification_group_count_max",
                    MIN_NOTIFICATION_GROUP_COUNT_MAX,
                    MAX_NOTIFICATION_GROUP_COUNT_MAX
                );
                set_integer_option!(
                    "notification_group_size_max",
                    MIN_NOTIFICATION_GROUP_SIZE_MAX,
                    MAX_NOTIFICATION_GROUP_SIZE_MAX
                );
            }
            'o' if name == "online" => {
                if value_id != CONSTRUCTOR_BOOL && value_id != CONSTRUCTOR_EMPTY {
                    return Err("Option \"online\" must have boolean value".to_string());
                }
                // OnlineManager::set_is_online — TODO (Wave 1).
                return Ok(SetOptionAction::Done);
            }
            'p' => {
                set_boolean_option!("prefer_ipv6");
                set_boolean_option!("process_pinned_messages_as_mentions");
            }
            'r' => {
                // temporary option
                set_boolean_option!("reuse_uploaded_photos_by_hash");
            }
            's' => {
                set_integer_option!("storage_max_files_size", 0, i32::MAX as i64);
                set_integer_option!("storage_max_time_from_last_access", 0, i32::MAX as i64);
                set_integer_option!("storage_max_file_count", 0, i32::MAX as i64);
                set_integer_option!("storage_immunity_delay", 0, i32::MAX as i64);
                set_boolean_option!("store_all_files_in_files_directory");
            }
            't' => {
                set_boolean_option!("test_flood_wait");
            }
            'u' => {
                set_boolean_option!("use_pfs");
                set_boolean_option!("use_quick_ack");
                set_boolean_option!("use_storage_optimizer");
                set_integer_option!("utc_time_offset", -12 * 60 * 60, 14 * 60 * 60);
            }
            'X' | 'x' => {
                if name.len() > 255 {
                    return Err("Option name is too long".to_string());
                }
                match value {
                    OptionValue::optionValueBoolean(v) => {
                        self.set_option_boolean(name, v.value);
                    }
                    OptionValue::optionValueEmpty(_) => {
                        self.set_option_empty(name);
                    }
                    OptionValue::optionValueInteger(v) => {
                        self.set_option_integer(name, v.value);
                    }
                    OptionValue::optionValueString(v) => {
                        self.set_option_string(name, &v.value);
                    }
                }
                return Ok(SetOptionAction::Done);
            }
            _ => {}
        }

        Err("Option can't be set".to_string())
    }

    /// `OptionManager::update_premium_options` — the derived user-facing
    /// limits from the server `*_limit_default`/`*_limit_premium` options.
    pub fn update_premium_options(&mut self) {
        let is_premium = self.get_option_boolean("is_premium", false);
        if is_premium {
            self.set_option_integer(
                "saved_animations_limit",
                self.get_option_integer("saved_gifs_limit_premium", 400),
            );
            self.set_option_integer(
                "favorite_stickers_limit",
                self.get_option_integer("stickers_faved_limit_premium", 10),
            );
            self.set_option_integer(
                "chat_folder_count_max",
                self.get_option_integer("dialog_filters_limit_premium", 20),
            );
            self.set_option_integer(
                "chat_folder_chosen_chat_count_max",
                self.get_option_integer("dialog_filters_chats_limit_premium", 200),
            );
            self.set_option_integer(
                "pinned_chat_count_max",
                self.get_option_integer("dialogs_pinned_limit_premium", 100),
            );
            self.set_option_integer(
                "pinned_archived_chat_count_max",
                self.get_option_integer("dialogs_folder_pinned_limit_premium", 200),
            );
            self.set_option_integer(
                "pinned_saved_messages_topic_count_max",
                self.get_option_integer("saved_dialogs_pinned_limit_premium", 100),
            );
            self.set_option_integer(
                "bio_length_max",
                self.get_option_integer("about_length_limit_premium", 140),
            );
            self.set_option_integer(
                "chat_folder_invite_link_count_max",
                self.get_option_integer("chatlist_invites_limit_premium", 20),
            );
            self.set_option_integer(
                "added_shareable_chat_folder_count_max",
                self.get_option_integer("chatlists_joined_limit_premium", 20),
            );
            self.set_option_integer(
                "active_story_count_max",
                self.get_option_integer("story_expiring_limit_premium", 100),
            );
            self.set_option_integer(
                "story_caption_length_max",
                self.get_option_integer("story_caption_length_limit_premium", 2048),
            );
            self.set_option_integer(
                "weekly_sent_story_count_max",
                self.get_option_integer("stories_sent_weekly_limit_premium", 700),
            );
            self.set_option_integer(
                "monthly_sent_story_count_max",
                self.get_option_integer("stories_sent_monthly_limit_premium", 3000),
            );
            self.set_option_integer(
                "story_suggested_reaction_area_count_max",
                self.get_option_integer("stories_suggested_reactions_limit_premium", 5),
            );
            self.set_option_integer(
                "owned_bot_count_max",
                self.get_option_integer("bots_create_limit_premium", 40),
            );
            self.set_option_integer(
                "added_text_composition_style_count_max",
                self.get_option_integer("aicompose_tone_saved_limit_premium", 20),
            );
            self.set_option_boolean("can_set_new_chat_privacy_settings", true);
            self.set_option_boolean("can_use_text_entities_in_story_caption", true);
        } else {
            self.set_option_integer(
                "saved_animations_limit",
                self.get_option_integer("saved_gifs_limit_default", 200),
            );
            self.set_option_integer(
                "favorite_stickers_limit",
                self.get_option_integer("stickers_faved_limit_default", 5),
            );
            self.set_option_integer(
                "chat_folder_count_max",
                self.get_option_integer("dialog_filters_limit_default", 10),
            );
            self.set_option_integer(
                "chat_folder_chosen_chat_count_max",
                self.get_option_integer("dialog_filters_chats_limit_default", 100),
            );
            self.set_option_integer(
                "pinned_chat_count_max",
                self.get_option_integer("dialogs_pinned_limit_default", 5),
            );
            self.set_option_integer(
                "pinned_archived_chat_count_max",
                self.get_option_integer("dialogs_folder_pinned_limit_default", 100),
            );
            self.set_option_integer(
                "pinned_saved_messages_topic_count_max",
                self.get_option_integer("saved_dialogs_pinned_limit_default", 5),
            );
            self.set_option_integer(
                "bio_length_max",
                self.get_option_integer("about_length_limit_default", 70),
            );
            self.set_option_integer(
                "chat_folder_invite_link_count_max",
                self.get_option_integer("chatlist_invites_limit_default", 3),
            );
            self.set_option_integer(
                "added_shareable_chat_folder_count_max",
                self.get_option_integer("chatlists_joined_limit_default", 2),
            );
            self.set_option_integer(
                "active_story_count_max",
                self.get_option_integer("story_expiring_limit_default", 3),
            );
            self.set_option_integer(
                "story_caption_length_max",
                self.get_option_integer("story_caption_length_limit_default", 200),
            );
            self.set_option_integer(
                "weekly_sent_story_count_max",
                self.get_option_integer("stories_sent_weekly_limit_default", 7),
            );
            self.set_option_integer(
                "monthly_sent_story_count_max",
                self.get_option_integer("stories_sent_monthly_limit_default", 30),
            );
            self.set_option_integer(
                "story_suggested_reaction_area_count_max",
                self.get_option_integer("stories_suggested_reactions_limit_default", 1),
            );
            self.set_option_integer(
                "owned_bot_count_max",
                self.get_option_integer("bots_create_limit_default", 20),
            );
            self.set_option_integer(
                "added_text_composition_style_count_max",
                self.get_option_integer("aicompose_tone_saved_limit_default", 5),
            );
            self.set_option_boolean(
                "can_set_new_chat_privacy_settings",
                !self.get_option_boolean("need_premium_for_new_chat_privacy", false),
            );
            self.set_option_boolean(
                "can_use_text_entities_in_story_caption",
                !self.get_option_boolean("need_premium_for_story_caption_entities", false),
            );
        }
    }

    /// The constructor default table (cpp:104-226) and the erase list
    /// (cpp:232-255).
    fn apply_defaults(&mut self) {
        let is_test_dc = self.is_test_dc;
        let options = &mut self.options;
        let pmc = &mut self.option_pmc;
        let updates = &mut self.pending_updates;

        let mut set_default_integer_option = |name: &str, value: i64| {
            if options.isset(name) {
                return;
            }
            let str_value = format!("I{value}");
            options.set(name, &str_value);
            if let Some(pmc) = pmc.as_mut() {
                pmc.set(name, &str_value);
            }
            if !is_internal_option(name) {
                updates.push(td_api::Object::updateOption(td_api::updateOption {
                    name: name.to_string(),
                    value: Some(OptionValue::optionValueInteger(
                        td_api::optionValueInteger { value },
                    )),
                }));
            }
        };
        set_default_integer_option(
            "telegram_service_notifications_chat_id",
            SERVICE_NOTIFICATIONS_USER_ID,
        );
        set_default_integer_option(
            "replies_bot_chat_id",
            if is_test_dc {
                REPLIES_BOT_USER_ID_TEST_DC
            } else {
                REPLIES_BOT_USER_ID
            },
        );
        set_default_integer_option(
            "verification_codes_bot_chat_id",
            VERIFICATION_CODES_BOT_USER_ID,
        );
        set_default_integer_option(
            "group_anonymous_bot_user_id",
            if is_test_dc {
                ANONYMOUS_BOT_USER_ID_TEST_DC
            } else {
                ANONYMOUS_BOT_USER_ID
            },
        );
        set_default_integer_option(
            "channel_bot_user_id",
            if is_test_dc {
                CHANNEL_BOT_USER_ID_TEST_DC
            } else {
                CHANNEL_BOT_USER_ID
            },
        );
        set_default_integer_option(
            "anti_spam_bot_user_id",
            if is_test_dc {
                ANTI_SPAM_BOT_USER_ID_TEST_DC
            } else {
                ANTI_SPAM_BOT_USER_ID
            },
        );
        set_default_integer_option("message_caption_length_max", 1024);
        set_default_integer_option("message_reply_quote_length_max", 1024);
        set_default_integer_option("story_caption_length_max", 200);
        set_default_integer_option("bio_length_max", 70);
        set_default_integer_option("suggested_video_note_length", 384);
        set_default_integer_option("suggested_video_note_video_bitrate", 1000);
        set_default_integer_option("suggested_video_note_audio_bitrate", 64);
        set_default_integer_option("notification_sound_duration_max", 5);
        set_default_integer_option("notification_sound_size_max", 307200);
        set_default_integer_option(
            "notification_sound_count_max",
            if is_test_dc { 5 } else { 100 },
        );
        set_default_integer_option("chat_folder_count_max", if is_test_dc { 3 } else { 10 });
        set_default_integer_option(
            "chat_folder_chosen_chat_count_max",
            if is_test_dc { 5 } else { 100 },
        );
        set_default_integer_option(
            "aggressive_anti_spam_supergroup_member_count_min",
            if is_test_dc { 1 } else { 200 },
        );
        set_default_integer_option(
            "pinned_forum_topic_count_max",
            if is_test_dc { 3 } else { 5 },
        );
        set_default_integer_option("story_stealth_mode_past_period", 300);
        set_default_integer_option("story_stealth_mode_future_period", 1500);
        set_default_integer_option("story_stealth_mode_cooldown_period", 3 * 3600);
        set_default_integer_option(
            "giveaway_additional_chat_count_max",
            if is_test_dc { 3 } else { 10 },
        );
        set_default_integer_option(
            "giveaway_country_count_max",
            if is_test_dc { 3 } else { 10 },
        );
        set_default_integer_option("giveaway_boost_count_per_premium", 4);
        set_default_integer_option("giveaway_duration_max", 31 * 86400);
        set_default_integer_option("premium_gift_boost_count", 3);
        set_default_integer_option("chat_boost_level_max", if is_test_dc { 10 } else { 100 });
        set_default_integer_option("chat_available_reaction_count_max", 100);
        set_default_integer_option("channel_bg_icon_level_min", if is_test_dc { 1 } else { 4 });
        set_default_integer_option(
            "channel_custom_wallpaper_level_min",
            if is_test_dc { 4 } else { 10 },
        );
        set_default_integer_option(
            "channel_emoji_status_level_min",
            if is_test_dc { 2 } else { 8 },
        );
        set_default_integer_option(
            "channel_profile_bg_icon_level_min",
            if is_test_dc { 1 } else { 7 },
        );
        set_default_integer_option(
            "channel_restrict_sponsored_level_min",
            if is_test_dc { 5 } else { 50 },
        );
        set_default_integer_option(
            "channel_wallpaper_level_min",
            if is_test_dc { 3 } else { 9 },
        );
        set_default_integer_option("pm_read_date_expire_period", 604800);
        set_default_integer_option("group_transcribe_level_min", if is_test_dc { 4 } else { 6 });
        set_default_integer_option(
            "group_emoji_stickers_level_min",
            if is_test_dc { 1 } else { 4 },
        );
        set_default_integer_option(
            "group_profile_bg_icon_level_min",
            if is_test_dc { 1 } else { 5 },
        );
        set_default_integer_option(
            "group_emoji_status_level_min",
            if is_test_dc { 2 } else { 8 },
        );
        set_default_integer_option("group_wallpaper_level_min", if is_test_dc { 3 } else { 9 });
        set_default_integer_option(
            "group_custom_wallpaper_level_min",
            if is_test_dc { 4 } else { 10 },
        );
        set_default_integer_option(
            "quick_reply_shortcut_count_max",
            if is_test_dc { 10 } else { 100 },
        );
        set_default_integer_option("quick_reply_shortcut_message_count_max", 20);
        set_default_integer_option("business_start_page_title_length_max", 32);
        set_default_integer_option("business_start_page_message_length_max", 70);
        set_default_integer_option("premium_download_speedup", 10);
        set_default_integer_option("premium_upload_speedup", 10);
        set_default_integer_option(
            "upload_premium_speedup_notify_period",
            if is_test_dc { 30 } else { 3600 },
        );
        set_default_integer_option(
            "business_chat_link_count_max",
            if is_test_dc { 5 } else { 100 },
        );
        set_default_integer_option("pinned_story_count_max", 3);
        set_default_integer_option("fact_check_length_max", 1024);
        set_default_integer_option(
            "star_withdrawal_count_min",
            if is_test_dc { 10 } else { 1000 },
        );
        set_default_integer_option("story_link_area_count_max", 3);
        set_default_integer_option("paid_media_message_star_count_max", 25000);
        set_default_integer_option("bot_media_preview_count_max", 12);
        set_default_integer_option("paid_reaction_star_count_max", 10000);
        set_default_integer_option("subscription_star_count_max", 10000);
        set_default_integer_option("usd_to_thousand_star_rate", 1410);
        set_default_integer_option("thousand_star_to_usd_rate", 1300);
        set_default_integer_option("gift_text_length_max", 128);
        set_default_integer_option(
            "gift_sell_period",
            if is_test_dc { 300 } else { 90 * 86400 },
        );
        set_default_integer_option("affiliate_program_commission_per_mille_min", 1);
        set_default_integer_option("affiliate_program_commission_per_mille_max", 800);
        set_default_integer_option("bot_verification_custom_description_length_max", 70);
        set_default_integer_option("paid_message_star_count_max", 10000);
        set_default_integer_option("paid_message_earnings_per_mille", 850);
        set_default_integer_option("pinned_gift_count_max", 6);
        set_default_integer_option(
            "group_call_participant_count_max",
            if is_test_dc { 5 } else { 200 },
        );
        set_default_integer_option(
            "channel_autotranslation_level_min",
            if is_test_dc { 1 } else { 3 },
        );
        set_default_integer_option("gift_resale_star_count_min", 125);
        set_default_integer_option("gift_resale_star_count_max", 100000);
        set_default_integer_option("gift_resale_star_earnings_per_mille", 800);
        set_default_integer_option("poll_answer_count_max", 12);
        set_default_integer_option("direct_channel_message_star_count_default", 10);
        set_default_integer_option("checklist_task_text_length_max", 100);
        set_default_integer_option("checklist_title_length_max", 255);
        set_default_integer_option("checklist_task_count_max", if is_test_dc { 10 } else { 30 });
        set_default_integer_option("suggested_post_star_count_min", 5);
        set_default_integer_option("suggested_post_star_count_max", 100000);
        set_default_integer_option("suggested_post_gram_cent_count_min", 1);
        set_default_integer_option("suggested_post_gram_cent_count_max", 1000000);
        set_default_integer_option("suggested_post_star_earnings_per_mille", 850);
        set_default_integer_option("suggested_post_gram_earnings_per_mille", 850);
        set_default_integer_option("million_gram_to_usd_rate", 3000000);
        set_default_integer_option(
            "suggested_post_lifetime_min",
            if is_test_dc { 120 } else { 86400 },
        );
        set_default_integer_option("suggested_post_send_delay_min", 300);
        set_default_integer_option("suggested_post_send_delay_max", 2678400);
        set_default_integer_option(
            "star_withdrawal_count_max",
            if is_test_dc { 100 } else { 25000000 },
        );
        set_default_integer_option("gift_collection_count_max", 10);
        set_default_integer_option("gift_collection_size_max", 500);
        set_default_integer_option(
            "gift_resale_gram_cent_count_min",
            if is_test_dc { 5000 } else { 700 },
        );
        set_default_integer_option("gift_resale_gram_cent_count_max", 10000000);
        set_default_integer_option("gift_resale_gram_earnings_per_mille", 900);
        set_default_integer_option("story_album_count_max", if is_test_dc { 20 } else { 100 });
        set_default_integer_option("story_album_size_max", if is_test_dc { 200 } else { 1000 });
        set_default_integer_option("pending_text_message_period", 30);
        set_default_integer_option("user_note_text_length_max", 128);
        set_default_integer_option("group_call_message_show_time_max", 10);
        set_default_integer_option("group_call_message_text_length_max", 128);
        set_default_integer_option("paid_group_call_message_star_count_max", 10000);
        set_default_integer_option("login_passkey_count_max", 5);
        set_default_integer_option("stake_dice_stake_amount_max", 50000000000);
        set_default_integer_option("stake_dice_stake_amount_min", 100000000);
        set_default_integer_option(
            "has_protected_content_disable_request_duration",
            if is_test_dc { 300 } else { 86400 },
        );
        set_default_integer_option("poll_open_period_max", 730 * 3600);
        set_default_integer_option("owned_bot_count_max", 20);
        set_default_integer_option("text_composition_style_example_count", 3);
        set_default_integer_option("text_composition_style_title_length_max", 12);
        set_default_integer_option("text_composition_style_prompt_length_max", 1024);
        set_default_integer_option("poll_country_count_max", 12);
        set_default_integer_option("message_text_length_max", 4096);
        set_default_integer_option("rich_message_text_length_max", 32768);
        set_default_integer_option("rich_message_block_count_max", 500);
        set_default_integer_option("rich_message_depth_max", 16);
        set_default_integer_option("rich_message_media_count_max", 50);
        set_default_integer_option("rich_message_table_column_count_max", 20);
        set_default_integer_option("welcome_message_count_max", if is_test_dc { 5 } else { 3 });
        set_default_integer_option(
            "community_chat_count_max",
            if is_test_dc { 10 } else { 100 },
        );
        set_default_integer_option("community_bot_count_max", if is_test_dc { 10 } else { 100 });
        set_default_integer_option("ton_wallet_transfer_amount_min", 100000000);
        set_default_integer_option("ton_wallet_gasless_transfer_amount_min", 100000000);
        set_default_integer_option("ton_wallet_gasless_transfer_daily_count_max", 0);
        set_default_integer_option("ton_wallet_gasless_transfer_daily_count", 0);
        set_default_integer_option("secondary_bot_username_count_max", 2);

        if self.options.isset("my_phone_number") || !self.options.isset("my_id") {
            self.update_premium_options();
        }

        for name in [
            "archive_and_mute_new_chats_from_unknown_users",
            "business_intro_title_length_max",
            "business_intro_message_length_max",
            "channel_custom_accent_color_boost_level_min",
            "chat_filter_count_max",
            "chat_filter_chosen_chat_count_max",
            "forum_member_count_min",
            "themed_emoji_statuses_sticker_set_id",
            "themed_premium_statuses_sticker_set_id",
            "usd_to_1000_star_rate",
            "1000_star_to_usd_rate",
            "is_location_visible",
            "gift_resale_earnings_per_mille",
            "user_rating_learn_more_url",
            "gift_collection_gift_count_max",
            "story_album_story_count_max",
            "suggested_post_toncoin_cent_count_min",
            "suggested_post_toncoin_cent_count_max",
            "suggested_post_toncoin_earnings_per_mille",
            "million_toncoin_to_usd_rate",
            "gift_resale_toncoin_cent_count_min",
            "gift_resale_toncoin_cent_count_max",
            "gift_resale_toncoin_earnings_per_mille",
            "toncoin_top_up_url",
        ] {
            self.set_option_empty(name);
        }
    }

    /// `OptionManager::is_td_inited`.
    pub fn is_td_inited(&self) -> bool {
        self.is_td_inited
    }

    /// `OptionManager::get_option(name, promise)` pre-init arm: the getter
    /// waits until `on_td_inited`.
    pub fn queue_get_option(&mut self, request_id: u64, name: String) {
        self.pending_get_options.push((request_id, name));
    }

    /// `OptionManager::on_td_inited`: mark inited and return the pending
    /// pre-init getters for the Td actor to answer.
    pub fn on_td_inited(&mut self) -> Vec<(u64, String)> {
        self.is_td_inited = true;
        std::mem::take(&mut self.pending_get_options)
    }

    /// The queued `updateOption` broadcasts, drained by the Td actor.
    pub fn take_updates(&mut self) -> Vec<td_api::Object> {
        std::mem::take(&mut self.pending_updates)
    }

    /// `OptionManager::get_common_state`.
    pub fn get_common_state(updates: &mut Vec<td_api::Object>) {
        for option_name in get_synchronous_options() {
            if let Some(value) = get_option_synchronously(option_name) {
                updates.push(td_api::Object::updateOption(td_api::updateOption {
                    name: (*option_name).to_string(),
                    value: Some(value),
                }));
            }
        }
    }

    /// `OptionManager::get_current_state`: common + online + unix_time +
    /// every non-internal store entry. `unix_time` is the Global-corrected
    /// time.
    pub fn get_current_state(&self, updates: &mut Vec<td_api::Object>, unix_time: i32) {
        Self::get_common_state(updates);

        // `td_->online_manager_->is_online()` — TODO (Wave 1), interim false.
        updates.push(td_api::Object::updateOption(td_api::updateOption {
            name: "online".to_string(),
            value: Some(OptionValue::optionValueBoolean(
                td_api::optionValueBoolean { value: false },
            )),
        }));
        updates.push(td_api::Object::updateOption(td_api::updateOption {
            name: "unix_time".to_string(),
            value: Some(OptionValue::optionValueInteger(
                td_api::optionValueInteger {
                    value: unix_time as i64,
                },
            )),
        }));

        for (name, value) in self.options.get_all() {
            if !is_internal_option(&name) {
                updates.push(td_api::Object::updateOption(td_api::updateOption {
                    name: name.clone(),
                    value: Some(Self::get_option_value_object(&value)),
                }));
            } else if let Some(update) = self.get_internal_option_update(&name) {
                // Internal options emit their dedicated updates instead
                // (get_internal_option_update) — the pmc-restore loop of
                // OptionManager.cpp:73-78.
                updates.push(update.into());
            }
        }
    }
}

/// The `td_api::OptionValue` constructor ids used by the C++ switch.
const CONSTRUCTOR_BOOL: i32 = td_api::optionValueBoolean::ID;
const CONSTRUCTOR_EMPTY: i32 = td_api::optionValueEmpty::ID;

fn value_constructor_id(value: &OptionValue) -> i32 {
    match value {
        OptionValue::optionValueBoolean(_) => td_api::optionValueBoolean::ID,
        OptionValue::optionValueEmpty(_) => td_api::optionValueEmpty::ID,
        OptionValue::optionValueInteger(_) => td_api::optionValueInteger::ID,
        OptionValue::optionValueString(_) => td_api::optionValueString::ID,
    }
}

/// `Clocks::tz_offset()`: the local UTC offset in seconds (east positive),
/// computed once per manager like the C++ `Clocks::update`.
#[cfg(unix)]
fn tz_offset() -> i32 {
    // the `tm_gmtoff` field straight from `localtime_r` (the C++ reads the
    // same glibc field).
    let now = std::time::SystemTime::now();
    let unix = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&unix, &mut tm);
    }
    tm.tm_gmtoff as i32
}

/// The Windows half: `std::localtime` on Windows resolves through the same
/// registry timezone info `GetTimeZoneInformation` reports, so the C++
/// localtime−gmtime arithmetic collapses onto the bias fields (east-positive
/// offset = −(Bias [+ DaylightBias]) × 60).
#[cfg(windows)]
fn tz_offset() -> i32 {
    #[repr(C)]
    #[allow(non_snake_case)]
    struct SystemTimeT {
        wYear: u16,
        wMonth: u16,
        wDayOfWeek: u16,
        wDay: u16,
        wHour: u16,
        wMinute: u16,
        wSecond: u16,
        wMilliseconds: u16,
    }
    #[repr(C)]
    #[allow(non_snake_case)]
    struct TimeZoneInformation {
        Bias: i32,
        StandardName: [u16; 32],
        StandardDate: SystemTimeT,
        DaylightName: [u16; 32],
        DaylightDate: SystemTimeT,
        DaylightBias: i32,
    }
    unsafe extern "system" {
        fn GetTimeZoneInformation(lptimezoneinformation: *mut TimeZoneInformation) -> u32;
    }
    const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
    let mut tzi: TimeZoneInformation = unsafe { std::mem::zeroed() };
    let rc = unsafe { GetTimeZoneInformation(&mut tzi) };
    let bias = if rc == TIME_ZONE_ID_DAYLIGHT {
        tzi.Bias + tzi.DaylightBias
    } else {
        tzi.Bias
    };
    -bias * 60
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> OptionValue {
        OptionValue::optionValueEmpty(td_api::optionValueEmpty)
    }

    fn boolean(value: bool) -> OptionValue {
        OptionValue::optionValueBoolean(td_api::optionValueBoolean { value })
    }

    fn integer(value: i64) -> OptionValue {
        OptionValue::optionValueInteger(td_api::optionValueInteger { value })
    }

    fn string(value: &str) -> OptionValue {
        option_value_string(value)
    }

    #[test]
    fn defaults_are_applied_with_test_dc_variants() {
        let manager = OptionManager::new(false);
        assert_eq!(
            manager.get_option_integer("telegram_service_notifications_chat_id", 0),
            777000
        );
        assert_eq!(
            manager.get_option_integer("replies_bot_chat_id", 0),
            1271266957
        );
        assert_eq!(
            manager.get_option_integer("message_caption_length_max", 0),
            1024
        );
        assert_eq!(
            manager.get_option_integer("notification_sound_count_max", 0),
            100
        );
        assert_eq!(manager.get_option_integer("chat_folder_count_max", 0), 10);

        let test_dc = OptionManager::new(true);
        assert_eq!(test_dc.get_option_integer("replies_bot_chat_id", 0), 708513);
        assert_eq!(
            test_dc.get_option_integer("notification_sound_count_max", 0),
            5
        );
        // update_premium_options() runs in the constructor (my_id is unset)
        // and overwrites this from the server-limit default, like the C++.
        assert_eq!(test_dc.get_option_integer("chat_folder_count_max", 0), 10);
        assert_eq!(
            test_dc.get_option_integer("anti_spam_bot_user_id", 0),
            ANTI_SPAM_BOT_USER_ID_TEST_DC
        );
    }

    #[test]
    fn premium_options_derive_limits() {
        let mut manager = OptionManager::new(false);
        // Not premium, no server limits: the default tier applies. The
        // privacy gate defaults to allowed (need_premium_* unset = false).
        assert_eq!(manager.get_option_integer("saved_animations_limit", 0), 200);
        assert_eq!(manager.get_option_integer("bio_length_max", 0), 70);
        assert!(manager.get_option_boolean("can_set_new_chat_privacy_settings", false));

        // The gate flips when the server requires premium.
        manager.set_option_boolean("need_premium_for_new_chat_privacy", true);
        manager.update_premium_options();
        assert!(!manager.get_option_boolean("can_set_new_chat_privacy_settings", true));
        manager.set_option_boolean("need_premium_for_new_chat_privacy", false);

        // Premium with server limits.
        manager.set_option_boolean("is_premium", true);
        manager.set_option_integer("saved_gifs_limit_premium", 500);
        manager.update_premium_options();
        assert_eq!(manager.get_option_integer("saved_animations_limit", 0), 500);
        assert_eq!(manager.get_option_integer("bio_length_max", 0), 140);
        assert!(manager.get_option_boolean("can_set_new_chat_privacy_settings", false));
    }

    #[test]
    fn typed_set_get_round_trip() {
        let mut manager = OptionManager::new(false);
        manager.set_option_string("x_test_string", "hello");
        manager.set_option_integer("x_test_int", 42);
        manager.set_option_boolean("x_test_bool", true);
        assert_eq!(manager.get_option_string("x_test_string", ""), "hello");
        assert_eq!(manager.get_option_integer("x_test_int", 0), 42);
        assert!(manager.get_option_boolean("x_test_bool", false));
        // Erase via empty.
        manager.set_option_empty("x_test_string");
        assert_eq!(manager.get_option_string("x_test_string", ""), "");
        assert!(!manager.have_option("x_test_string"));
    }

    #[test]
    fn set_option_validation_errors_match_cpp() {
        let mut manager = OptionManager::new(false);
        assert_eq!(
            manager
                .set_option_from_api("message_unload_delay", &string("x"))
                .unwrap_err(),
            "Option \"message_unload_delay\" must have integer value"
        );
        assert_eq!(
            manager
                .set_option_from_api("message_unload_delay", &integer(59))
                .unwrap_err(),
            "Option's \"message_unload_delay\" value 59 is outside of the valid range [60, 86400]"
        );
        assert_eq!(
            manager
                .set_option_from_api("message_unload_delay", &integer(86401))
                .unwrap_err(),
            "Option's \"message_unload_delay\" value 86401 is outside of the valid range [60, 86400]"
        );
        assert_eq!(
            manager
                .set_option_from_api("always_parse_markdown", &integer(1))
                .unwrap_err(),
            "Option \"always_parse_markdown\" must have boolean value"
        );
        assert_eq!(
            manager
                .set_option_from_api("unknown_option_name", &empty())
                .unwrap_err(),
            "Option can't be set"
        );
        assert_eq!(
            manager
                .set_option_from_api("online", &integer(1))
                .unwrap_err(),
            "Option \"online\" must have boolean value"
        );
    }

    #[test]
    fn set_option_accepts_valid_values() {
        let mut manager = OptionManager::new(false);
        manager
            .set_option_from_api("message_unload_delay", &integer(120))
            .unwrap();
        assert_eq!(manager.get_option_integer("message_unload_delay", 0), 120);
        manager
            .set_option_from_api("always_parse_markdown", &boolean(true))
            .unwrap();
        assert!(manager.get_option_boolean("always_parse_markdown", false));
        // The empty form erases.
        manager
            .set_option_from_api("always_parse_markdown", &empty())
            .unwrap();
        assert!(!manager.have_option("always_parse_markdown"));
    }

    #[test]
    fn custom_x_options_accept_any_type() {
        let mut manager = OptionManager::new(false);
        manager
            .set_option_from_api("x_custom_flag", &boolean(true))
            .unwrap();
        manager
            .set_option_from_api("x_custom_count", &integer(7))
            .unwrap();
        manager
            .set_option_from_api("x_custom_text", &string("v"))
            .unwrap();
        assert!(manager.get_option_boolean("x_custom_flag", false));
        assert_eq!(manager.get_option_integer("x_custom_count", 0), 7);
        assert_eq!(manager.get_option_string("x_custom_text", ""), "v");
        let long_name = "x".repeat(256);
        assert_eq!(
            manager
                .set_option_from_api(&long_name, &empty())
                .unwrap_err(),
            "Option name is too long"
        );
    }

    #[test]
    fn connection_parameters_must_be_a_json_object() {
        let mut manager = OptionManager::new(false);
        assert_eq!(
            manager
                .set_option_from_api("connection_parameters", &string("[1,2]"))
                .unwrap_err(),
            "Option \"connection_parameters\" can't have specified value"
        );
        manager
            .set_option_from_api("connection_parameters", &string(r#"{"a":1}"#))
            .unwrap();
        assert_eq!(
            manager.get_option_string("connection_parameters", ""),
            r#"{"a":1}"#
        );
    }

    #[test]
    fn get_option_value_decodes_prefixes() {
        let mut manager = OptionManager::new(false);
        manager.set_option_boolean("x_b", true);
        manager.set_option_integer("x_i", -5);
        manager.set_option_string("x_s", "text");
        assert_eq!(manager.get_option_value("x_b", 0), boolean(true));
        assert_eq!(manager.get_option_value("x_i", 0), integer(-5));
        assert_eq!(manager.get_option_value("x_s", 0), string("text"));
        assert_eq!(manager.get_option_value("x_missing", 0), empty());
        assert_eq!(manager.get_option_value("unix_time", 777), integer(777));
    }

    #[test]
    fn update_broadcasts_are_queued_and_drained() {
        let mut manager = OptionManager::new(false);
        let initial = manager.take_updates();
        assert!(
            !initial.is_empty(),
            "constructor defaults broadcast updates"
        );
        assert!(initial.iter().any(|update| {
            matches!(
                update,
                td_api::Object::updateOption(u) if u.name == "message_caption_length_max"
            )
        }));
        assert!(manager.take_updates().is_empty());
    }

    #[test]
    fn current_state_contains_common_and_user_options() {
        let mut manager = OptionManager::new(false);
        manager
            .set_option_from_api("x_user_opt", &integer(3))
            .unwrap();
        let mut updates = Vec::new();
        manager.get_current_state(&mut updates, 777);
        let names: Vec<&str> = updates
            .iter()
            .filter_map(|update| match update {
                td_api::Object::updateOption(u) => Some(u.name.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"version"));
        assert!(names.contains(&"commit_hash"));
        assert!(names.contains(&"online"));
        assert!(names.contains(&"unix_time"));
        assert!(names.contains(&"x_user_opt"));
        assert!(
            !names.contains(&"session_count"),
            "internal options are hidden"
        );
    }
}
