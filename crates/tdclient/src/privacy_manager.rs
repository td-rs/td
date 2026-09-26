//! The port of `td/telegram/PrivacyManager.{h,cpp}` +
//! `UserPrivacySetting.{h,cpp}` + `UserPrivacySettingRule.{h,cpp}` and the
//! new-chat/read-date plane of `GlobalPrivacySettings.{h,cpp}` — the glue
//! slice behind `getUserPrivacySettingRules`/`setUserPrivacySettingRules`
//! and `getReadDatePrivacySettings`/`setReadDatePrivacySettings` +
//! `getNewChatPrivacySettings`/`setNewChatPrivacySettings`.
//!
//! The `account.getPrivacy`/`account.setPrivacy` query tails and the
//! `account.getGlobalPrivacySettings` → `apply_changes` →
//! `account.setGlobalPrivacySettings` chain park at the named
//! [`PrivacyManagerEnv`] net seams (the M.31 convention — the promise
//! queues ride the [`PrivacyManager`] state exactly like the C++
//! `PrivacyInfo`, and the query result handlers
//! `on_get_user_privacy_settings`/`on_set_user_privacy_settings`
//! (PrivacyManager.cpp:176-229) are the seam's unreachable consumers).
//! Everything the C++ answers from local state is real: the privacy-key
//! model, the rules intake with the 400 bands, the rules→td_api builders,
//! the `do_update_privacy` cache flip + the `updateUserPrivacySettingRules`
//! emission, the new-chat star-count clamp and the null-settings bands.
//!
//! The `gift_settings_` field, the `StarGiftSettings` constructor and the
//! `get_input_global_privacy_settings` telegram builder
//! (GlobalPrivacySettings.cpp:139-154) landed with the `setGiftSettings`
//! slice (the models live in [`crate::star_gift_settings`]).

use tdutils::promise::Promise;
use tdutils::status::Status;

use crate::chat_manager::ChannelId;
use crate::dialog_id::DialogId;
use crate::messages_manager::DialogType;
use crate::td_api;
use crate::user_manager::UserId;

/// `UserPrivacySetting::Type` (UserPrivacySetting.h:19-34) — the array
/// index the [`PrivacyManager`] cache is keyed by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UserPrivacySettingType {
    UserStatus,
    ChatInvite,
    Call,
    PeerToPeerCall,
    LinkInForwardedMessages,
    UserProfilePhoto,
    UserPhoneNumber,
    FindByPhoneNumber,
    VoiceMessages,
    UserBio,
    UserBirthdate,
    StarGiftAutosave,
    NoPaidMessages,
    SavedMusic,
}

impl UserPrivacySettingType {
    /// `UserPrivacySetting::Type::Size`.
    pub(crate) const SIZE: usize = 14;

    /// The cache index (`static_cast<size_t>(key.type())`).
    fn index(self) -> usize {
        self as usize
    }

    /// `UserPrivacySetting::get_user_privacy_setting`
    /// (UserPrivacySetting.cpp:11-17) — the 400 band for the null key.
    pub(crate) fn get_user_privacy_setting(
        key: Option<&td_api::UserPrivacySetting>,
    ) -> Result<UserPrivacySettingType, Status> {
        let Some(key) = key else {
            return Err(Status::error(400, "UserPrivacySetting must be non-empty"));
        };
        Ok(match key {
            td_api::UserPrivacySetting::userPrivacySettingShowStatus(_) => Self::UserStatus,
            td_api::UserPrivacySetting::userPrivacySettingAllowChatInvites(_) => Self::ChatInvite,
            td_api::UserPrivacySetting::userPrivacySettingAllowCalls(_) => Self::Call,
            td_api::UserPrivacySetting::userPrivacySettingAllowPeerToPeerCalls(_) => {
                Self::PeerToPeerCall
            }
            td_api::UserPrivacySetting::userPrivacySettingShowLinkInForwardedMessages(_) => {
                Self::LinkInForwardedMessages
            }
            td_api::UserPrivacySetting::userPrivacySettingShowProfilePhoto(_) => {
                Self::UserProfilePhoto
            }
            td_api::UserPrivacySetting::userPrivacySettingShowPhoneNumber(_) => {
                Self::UserPhoneNumber
            }
            td_api::UserPrivacySetting::userPrivacySettingAllowFindingByPhoneNumber(_) => {
                Self::FindByPhoneNumber
            }
            td_api::UserPrivacySetting::userPrivacySettingAllowPrivateVoiceAndVideoNoteMessages(
                _,
            ) => Self::VoiceMessages,
            td_api::UserPrivacySetting::userPrivacySettingShowBio(_) => Self::UserBio,
            td_api::UserPrivacySetting::userPrivacySettingShowBirthdate(_) => Self::UserBirthdate,
            td_api::UserPrivacySetting::userPrivacySettingAutosaveGifts(_) => {
                Self::StarGiftAutosave
            }
            td_api::UserPrivacySetting::userPrivacySettingAllowUnpaidMessages(_) => {
                Self::NoPaidMessages
            }
            td_api::UserPrivacySetting::userPrivacySettingShowProfileAudio(_) => Self::SavedMusic,
        })
    }

    /// `UserPrivacySetting::get_user_privacy_setting_object`
    /// (UserPrivacySetting.cpp:69-103).
    #[allow(dead_code)] // the do_update_privacy emission chain (the intake plane)
    pub(crate) fn get_user_privacy_setting_object(self) -> td_api::UserPrivacySetting {
        match self {
            Self::UserStatus => td_api::UserPrivacySetting::userPrivacySettingShowStatus(
                td_api::userPrivacySettingShowStatus {},
            ),
            Self::ChatInvite => td_api::UserPrivacySetting::userPrivacySettingAllowChatInvites(
                td_api::userPrivacySettingAllowChatInvites {},
            ),
            Self::Call => td_api::UserPrivacySetting::userPrivacySettingAllowCalls(
                td_api::userPrivacySettingAllowCalls {},
            ),
            Self::PeerToPeerCall => {
                td_api::UserPrivacySetting::userPrivacySettingAllowPeerToPeerCalls(
                    td_api::userPrivacySettingAllowPeerToPeerCalls {},
                )
            }
            Self::LinkInForwardedMessages => {
                td_api::UserPrivacySetting::userPrivacySettingShowLinkInForwardedMessages(
                    td_api::userPrivacySettingShowLinkInForwardedMessages {},
                )
            }
            Self::UserProfilePhoto => {
                td_api::UserPrivacySetting::userPrivacySettingShowProfilePhoto(
                    td_api::userPrivacySettingShowProfilePhoto {},
                )
            }
            Self::UserPhoneNumber => td_api::UserPrivacySetting::userPrivacySettingShowPhoneNumber(
                td_api::userPrivacySettingShowPhoneNumber {},
            ),
            Self::FindByPhoneNumber => {
                td_api::UserPrivacySetting::userPrivacySettingAllowFindingByPhoneNumber(
                    td_api::userPrivacySettingAllowFindingByPhoneNumber {},
                )
            }
            Self::VoiceMessages => {
                td_api::UserPrivacySetting::userPrivacySettingAllowPrivateVoiceAndVideoNoteMessages(
                    td_api::userPrivacySettingAllowPrivateVoiceAndVideoNoteMessages {},
                )
            }
            Self::UserBio => td_api::UserPrivacySetting::userPrivacySettingShowBio(
                td_api::userPrivacySettingShowBio {},
            ),
            Self::UserBirthdate => td_api::UserPrivacySetting::userPrivacySettingShowBirthdate(
                td_api::userPrivacySettingShowBirthdate {},
            ),
            Self::StarGiftAutosave => td_api::UserPrivacySetting::userPrivacySettingAutosaveGifts(
                td_api::userPrivacySettingAutosaveGifts {},
            ),
            Self::NoPaidMessages => {
                td_api::UserPrivacySetting::userPrivacySettingAllowUnpaidMessages(
                    td_api::userPrivacySettingAllowUnpaidMessages {},
                )
            }
            Self::SavedMusic => td_api::UserPrivacySetting::userPrivacySettingShowProfileAudio(
                td_api::userPrivacySettingShowProfileAudio {},
            ),
        }
    }
}

/// `UserPrivacySettingRule::Type` (UserPrivacySettingRule.h:80-92) — the
/// i32 discriminant order.
mod rule_type {
    pub(crate) const ALLOW_CONTACTS: i32 = 0;
    pub(crate) const ALLOW_CLOSE_FRIENDS: i32 = 1;
    pub(crate) const ALLOW_ALL: i32 = 2;
    pub(crate) const ALLOW_USERS: i32 = 3;
    pub(crate) const ALLOW_CHAT_PARTICIPANTS: i32 = 4;
    pub(crate) const RESTRICT_CONTACTS: i32 = 5;
    pub(crate) const RESTRICT_ALL: i32 = 6;
    pub(crate) const RESTRICT_USERS: i32 = 7;
    pub(crate) const RESTRICT_CHAT_PARTICIPANTS: i32 = 8;
    pub(crate) const ALLOW_PREMIUM: i32 = 9;
    pub(crate) const ALLOW_BOTS: i32 = 10;
    pub(crate) const RESTRICT_BOTS: i32 = 11;
}

/// `td::UserPrivacySettingRule` (UserPrivacySettingRule.h:23-107) — the
/// full-plane rule; the story-plane subset lives in
/// [`crate::story_manager`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct UserPrivacySettingRule {
    r#type: i32,
    user_ids: Vec<UserId>,
    dialog_ids: Vec<DialogId>,
}

impl UserPrivacySettingRule {
    /// The bare constructor (the C++ in-class `type_ = RestrictAll`
    /// default is the parse fallback, the C++ ctors set both vectors).
    fn bare(r#type: i32) -> Self {
        UserPrivacySettingRule {
            r#type,
            user_ids: Vec::new(),
            dialog_ids: Vec::new(),
        }
    }

    /// `UserId::get_user_ids` (UserId.cpp:13-23) — the valid-id filter.
    fn get_user_ids(input_user_ids: &[i64]) -> Vec<UserId> {
        input_user_ids
            .iter()
            .copied()
            .map(UserId::new)
            .filter(|user_id| user_id.is_valid())
            .collect()
    }

    /// `UserPrivacySettingRule::set_dialog_ids`
    /// (UserPrivacySettingRule.cpp:24-50) — the dialog walk: only the
    /// loaded basic groups and megagroup channels are kept.
    fn set_dialog_ids(env: &mut impl PrivacyManagerEnv, chat_ids: &[i64]) -> Vec<DialogId> {
        let mut dialog_ids = Vec::new();
        for chat_id in chat_ids {
            let dialog_id = DialogId::new(*chat_id);
            if !env.have_dialog_force(dialog_id, "UserPrivacySettingRule::set_dialog_ids") {
                tdutils::log_info!("Ignore not found {dialog_id:?}");
                continue;
            }
            match dialog_id.get_type() {
                DialogType::Chat => dialog_ids.push(dialog_id),
                DialogType::Channel => {
                    let channel_id = dialog_id.get_channel_id();
                    if !env.is_megagroup_channel(channel_id) {
                        tdutils::log_info!("Ignore broadcast {channel_id:?}");
                        continue;
                    }
                    dialog_ids.push(dialog_id);
                }
                dialog_type => {
                    tdutils::log_info!("Ignore dialog of type {dialog_type:?}");
                }
            }
        }
        dialog_ids
    }

    /// `UserPrivacySettingRule::UserPrivacySettingRule(Td *, const
    /// td_api::UserPrivacySettingRule &)` (UserPrivacySettingRule.cpp:52-95).
    pub(crate) fn from_td_api(
        env: &mut impl PrivacyManagerEnv,
        rule: &td_api::UserPrivacySettingRule,
    ) -> Self {
        match rule {
            td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowContacts(_) => {
                Self::bare(rule_type::ALLOW_CONTACTS)
            }
            td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowBots(_) => {
                Self::bare(rule_type::ALLOW_BOTS)
            }
            td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowPremiumUsers(_) => {
                Self::bare(rule_type::ALLOW_PREMIUM)
            }
            td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowAll(_) => {
                Self::bare(rule_type::ALLOW_ALL)
            }
            td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowUsers(ids) => Self {
                r#type: rule_type::ALLOW_USERS,
                user_ids: Self::get_user_ids(&ids.user_ids),
                dialog_ids: Vec::new(),
            },
            td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowChatMembers(ids) => Self {
                r#type: rule_type::ALLOW_CHAT_PARTICIPANTS,
                user_ids: Vec::new(),
                dialog_ids: Self::set_dialog_ids(env, &ids.chat_ids),
            },
            td_api::UserPrivacySettingRule::userPrivacySettingRuleRestrictContacts(_) => {
                Self::bare(rule_type::RESTRICT_CONTACTS)
            }
            td_api::UserPrivacySettingRule::userPrivacySettingRuleRestrictBots(_) => {
                Self::bare(rule_type::RESTRICT_BOTS)
            }
            td_api::UserPrivacySettingRule::userPrivacySettingRuleRestrictAll(_) => {
                Self::bare(rule_type::RESTRICT_ALL)
            }
            td_api::UserPrivacySettingRule::userPrivacySettingRuleRestrictUsers(ids) => Self {
                r#type: rule_type::RESTRICT_USERS,
                user_ids: Self::get_user_ids(&ids.user_ids),
                dialog_ids: Vec::new(),
            },
            td_api::UserPrivacySettingRule::userPrivacySettingRuleRestrictChatMembers(ids) => {
                Self {
                    r#type: rule_type::RESTRICT_CHAT_PARTICIPANTS,
                    user_ids: Vec::new(),
                    dialog_ids: Self::set_dialog_ids(env, &ids.chat_ids),
                }
            }
        }
    }

    /// `UserPrivacySettingRule::get_user_privacy_setting_rule_object`
    /// (UserPrivacySettingRule.cpp:173-209).
    pub(crate) fn get_user_privacy_setting_rule_object(
        &self,
        env: &mut impl PrivacyManagerEnv,
    ) -> td_api::UserPrivacySettingRule {
        match self.r#type {
            rule_type::ALLOW_CONTACTS => {
                td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowContacts(
                    td_api::userPrivacySettingRuleAllowContacts {},
                )
            }
            rule_type::ALLOW_BOTS => {
                td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowBots(
                    td_api::userPrivacySettingRuleAllowBots {},
                )
            }
            rule_type::ALLOW_PREMIUM => {
                td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowPremiumUsers(
                    td_api::userPrivacySettingRuleAllowPremiumUsers {},
                )
            }
            rule_type::ALLOW_CLOSE_FRIENDS => {
                // UserPrivacySettingRule.cpp:182-184 — the server-only
                // rule degrades to an empty AllowUsers.
                tdutils::log_error!("Have AllowCloseFriends rule");
                td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowUsers(
                    td_api::userPrivacySettingRuleAllowUsers {
                        user_ids: Vec::new(),
                    },
                )
            }
            rule_type::ALLOW_ALL => td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowAll(
                td_api::userPrivacySettingRuleAllowAll {},
            ),
            rule_type::ALLOW_USERS => {
                td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowUsers(
                    td_api::userPrivacySettingRuleAllowUsers {
                        user_ids: env.get_user_ids_object(
                            &self.user_ids,
                            "userPrivacySettingRuleAllowUsers",
                        ),
                    },
                )
            }
            rule_type::ALLOW_CHAT_PARTICIPANTS => {
                td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowChatMembers(
                    td_api::userPrivacySettingRuleAllowChatMembers {
                        chat_ids: env
                            .get_chat_ids_object(&self.dialog_ids, "UserPrivacySettingRule"),
                    },
                )
            }
            rule_type::RESTRICT_CONTACTS => {
                td_api::UserPrivacySettingRule::userPrivacySettingRuleRestrictContacts(
                    td_api::userPrivacySettingRuleRestrictContacts {},
                )
            }
            rule_type::RESTRICT_BOTS => {
                td_api::UserPrivacySettingRule::userPrivacySettingRuleRestrictBots(
                    td_api::userPrivacySettingRuleRestrictBots {},
                )
            }
            rule_type::RESTRICT_ALL => {
                td_api::UserPrivacySettingRule::userPrivacySettingRuleRestrictAll(
                    td_api::userPrivacySettingRuleRestrictAll {},
                )
            }
            rule_type::RESTRICT_USERS => {
                td_api::UserPrivacySettingRule::userPrivacySettingRuleRestrictUsers(
                    td_api::userPrivacySettingRuleRestrictUsers {
                        user_ids: env.get_user_ids_object(
                            &self.user_ids,
                            "userPrivacySettingRuleRestrictUsers",
                        ),
                    },
                )
            }
            rule_type::RESTRICT_CHAT_PARTICIPANTS => {
                td_api::UserPrivacySettingRule::userPrivacySettingRuleRestrictChatMembers(
                    td_api::userPrivacySettingRuleRestrictChatMembers {
                        chat_ids: env
                            .get_chat_ids_object(&self.dialog_ids, "UserPrivacySettingRule"),
                    },
                )
            }
            _ => unreachable!("the C++ UNREACHABLE default"),
        }
    }
}

/// `td::UserPrivacySettingRules` (UserPrivacySettingRule.h:109-155).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct UserPrivacySettingRules {
    rules: Vec<UserPrivacySettingRule>,
}

impl UserPrivacySettingRules {
    /// `UserPrivacySettingRules::get_user_privacy_setting_rules(td_api)`
    /// (UserPrivacySettingRule.cpp:298-311). The null-element band
    /// ("UserPrivacySettingRule must be non-empty") is inexpressible —
    /// the Rust `Vec` carries no nulls.
    pub(crate) fn get_user_privacy_setting_rules(
        env: &mut impl PrivacyManagerEnv,
        rules: Option<&td_api::UserPrivacySettingRules>,
    ) -> Result<UserPrivacySettingRules, Status> {
        let Some(rules) = rules else {
            return Err(Status::error(
                400,
                "UserPrivacySettingRules must be non-empty",
            ));
        };
        let mut result = UserPrivacySettingRules::default();
        for rule in &rules.rules {
            result
                .rules
                .push(UserPrivacySettingRule::from_td_api(env, rule));
        }
        Ok(result)
    }

    /// `UserPrivacySettingRules::get_user_privacy_setting_rules_object`
    /// (UserPrivacySettingRule.cpp:353-357).
    pub(crate) fn get_user_privacy_setting_rules_object(
        &self,
        env: &mut impl PrivacyManagerEnv,
    ) -> td_api::UserPrivacySettingRules {
        td_api::UserPrivacySettingRules {
            rules: self
                .rules
                .iter()
                .map(|rule| rule.get_user_privacy_setting_rule_object(env))
                .collect(),
        }
    }

    /// `UserPrivacySettingRules::get_restricted_user_ids`
    /// (UserPrivacySettingRule.cpp:399-407) — the sorted dedup of the
    /// RestrictUsers id sets.
    #[allow(dead_code)] // the do_update_privacy user fan-outs (the intake plane)
    fn get_restricted_user_ids(&self) -> Vec<UserId> {
        let mut result: Vec<UserId> = self
            .rules
            .iter()
            .filter(|rule| rule.r#type == rule_type::RESTRICT_USERS)
            .flat_map(|rule| rule.user_ids.iter().copied())
            .collect();
        result.sort_by_key(|user_id| user_id.get());
        result.dedup();
        result
    }
}

/// `PrivacyManager::PrivacyInfo` (PrivacyManager.h:42-49) — the per-key
/// cached rules and the parked promise queues.
#[derive(Default)]
struct PrivacyInfo {
    rules: UserPrivacySettingRules,
    pending_rules: Option<UserPrivacySettingRules>,
    get_promises: Vec<Promise<td_api::UserPrivacySettingRules>>,
    set_promises: Vec<Promise<()>>,
    has_set_query: bool,
    is_synchronized: bool,
}

/// `td::PrivacyManager` (PrivacyManager.h:27-73) — the collapsed actor
/// state: the per-key [`PrivacyInfo`] array over the pmc-free in-memory
/// cache.
pub(crate) struct PrivacyManager {
    info: Vec<PrivacyInfo>,
}

impl Default for PrivacyManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PrivacyManager {
    pub(crate) fn new() -> PrivacyManager {
        PrivacyManager {
            info: (0..UserPrivacySettingType::SIZE)
                .map(|_| PrivacyInfo::default())
                .collect(),
        }
    }

    /// `PrivacyManager::get_info` (PrivacyManager.h:53-55).
    fn get_info(&mut self, key: UserPrivacySettingType) -> &mut PrivacyInfo {
        &mut self.info[key.index()]
    }

    /// `PrivacyManager::get_privacy` (PrivacyManager.cpp:120-138) — the
    /// synchronized cache answers the builder; otherwise the promise
    /// queues and the first request sends `account.getPrivacy` — the
    /// `[blocked: net]` seam.
    pub(crate) fn get_privacy(
        &mut self,
        env: &mut impl PrivacyManagerEnv,
        key: Option<&td_api::UserPrivacySetting>,
        mut promise: Promise<td_api::UserPrivacySettingRules>,
    ) -> Result<(), Status> {
        let user_privacy_setting = UserPrivacySettingType::get_user_privacy_setting(key)?;
        let info = self.get_info(user_privacy_setting);
        if info.is_synchronized {
            promise.set_value(info.rules.get_user_privacy_setting_rules_object(env));
            return Ok(());
        }
        info.get_promises.push(promise);
        if info.get_promises.len() > 1 {
            // query has already been sent, just wait for the result
            return Ok(());
        }
        env.get_privacy_via_net(user_privacy_setting);
        Ok(())
    }

    /// `PrivacyManager::set_privacy` (PrivacyManager.cpp:140-155) — the
    /// rules intake is real; the first request sends `account.setPrivacy`
    /// through the `[blocked: net]` seam (the
    /// `set_privacy_impl`/`on_set_user_privacy_settings` chain rides it),
    /// the dedup path parks the rules in `pending_rules`.
    pub(crate) fn set_privacy(
        &mut self,
        env: &mut impl PrivacyManagerEnv,
        key: Option<&td_api::UserPrivacySetting>,
        rules: Option<&td_api::UserPrivacySettingRules>,
        promise: Promise<()>,
    ) -> Result<(), Status> {
        let user_privacy_setting = UserPrivacySettingType::get_user_privacy_setting(key)?;
        let privacy_rules = UserPrivacySettingRules::get_user_privacy_setting_rules(env, rules)?;

        let info = self.get_info(user_privacy_setting);
        if info.has_set_query {
            info.pending_rules = Some(privacy_rules);
            info.set_promises.push(promise);
            return Ok(());
        }
        info.has_set_query = true;

        env.set_privacy_via_net(user_privacy_setting, privacy_rules, promise);
        Ok(())
    }

    /// `PrivacyManager::do_update_privacy` (PrivacyManager.cpp:231-273) —
    /// the cache flip, the equality dedup, the user-manager fan-outs and
    /// the `updateUserPrivacySettingRules` emission. Reached from the net
    /// seams' result handlers and `on_update_privacy` (the updates-plane
    /// arm), and from the tests.
    #[allow(dead_code)] // the result-handler/updates-plane callers land with the M.7/M.net wiring
    fn do_update_privacy(
        &mut self,
        env: &mut impl PrivacyManagerEnv,
        user_privacy_setting: UserPrivacySettingType,
        privacy_rules: UserPrivacySettingRules,
        from_update: bool,
    ) {
        let info = self.get_info(user_privacy_setting);
        let was_synchronized = info.is_synchronized;
        info.is_synchronized = true;

        if info.rules != privacy_rules {
            if from_update || was_synchronized {
                match user_privacy_setting {
                    UserPrivacySettingType::UserStatus => {
                        env.on_update_online_status_privacy();

                        let old_restricted = info.rules.get_restricted_user_ids();
                        let new_restricted = privacy_rules.get_restricted_user_ids();
                        if old_restricted != new_restricted {
                            // if a user was unrestricted, it is not received from the server
                            // anymore; we need to reget their online status manually
                            let unrestricted: Vec<UserId> = old_restricted
                                .iter()
                                .copied()
                                .filter(|user_id| !new_restricted.contains(user_id))
                                .collect();
                            for user_id in unrestricted {
                                env.reload_user(user_id, "do_update_privacy");
                            }
                        }
                    }
                    UserPrivacySettingType::UserPhoneNumber => {
                        env.on_update_phone_number_privacy();
                    }
                    _ => {}
                }
            }

            info.rules = privacy_rules;
            let key_object = user_privacy_setting.get_user_privacy_setting_object();
            let info = self.get_info(user_privacy_setting);
            let rules_object = info.rules.get_user_privacy_setting_rules_object(env);
            env.send_update_privacy_rules(td_api::updateUserPrivacySettingRules {
                setting: Some(key_object),
                rules: Some(rules_object),
            });
        }
    }
}

/// `GlobalPrivacySettings::SetType` (GlobalPrivacySettings.h:21).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SetType {
    #[default]
    None,
    #[allow(dead_code)] // the Archive set plane (setArchiveChatListSettings) lands with its slice
    Archive,
    ReadDate,
    NewChat,
    Gift,
}

/// `td::GlobalPrivacySettings` (GlobalPrivacySettings.h:20-55) — the
/// new-chat/read-date plane plus the gift settings; the Archive
/// constructor lands with its slice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GlobalPrivacySettings {
    set_type: SetType,
    archive_and_mute_new_noncontact_peers: bool,
    #[allow(dead_code)] // the Archive set-type plane (setArchiveChatListSettings)
    keep_archived_unmuted: bool,
    #[allow(dead_code)] // the Archive set-type plane (setArchiveChatListSettings)
    keep_archived_folders: bool,
    hide_read_marks: bool,
    new_noncontact_peers_require_premium: bool,
    noncontact_peers_paid_star_count: i64,
    /// `GlobalPrivacySettings.h:29` — the `setGiftSettings` slice.
    gift_settings: crate::star_gift_settings::StarGiftSettings,
}

impl GlobalPrivacySettings {
    /// `GlobalPrivacySettings(td_api::readDatePrivacySettings *)`
    /// (GlobalPrivacySettings.cpp:97-100).
    pub(crate) fn from_read_date_privacy_settings(
        settings: Option<td_api::ReadDatePrivacySettings>,
    ) -> GlobalPrivacySettings {
        GlobalPrivacySettings {
            set_type: SetType::ReadDate,
            hide_read_marks: settings.as_ref().is_none_or(|settings| {
                // the C++ reads `!settings->show_read_date_`
                !settings.show_read_date
            }),
            ..GlobalPrivacySettings::default()
        }
    }

    /// `GlobalPrivacySettings(td_api::newChatPrivacySettings *)`
    /// (GlobalPrivacySettings.cpp:102-108) — the star count clamps to
    /// `[0, 1000000]`.
    pub(crate) fn from_new_chat_privacy_settings(
        settings: Option<td_api::NewChatPrivacySettings>,
    ) -> GlobalPrivacySettings {
        GlobalPrivacySettings {
            set_type: SetType::NewChat,
            new_noncontact_peers_require_premium: settings.as_ref().is_none_or(|settings| {
                // the C++ reads `!settings->allow_new_chats_from_unknown_users_`
                !settings.allow_new_chats_from_unknown_users
            }),
            noncontact_peers_paid_star_count: settings
                .map(|settings| {
                    settings
                        .incoming_paid_message_star_count
                        .clamp(0, 1_000_000)
                })
                .unwrap_or(0),
            ..GlobalPrivacySettings::default()
        }
    }

    /// `GlobalPrivacySettings(td_api::giftSettings *)`
    /// (GlobalPrivacySettings.cpp:110-113).
    pub(crate) fn from_gift_settings(
        settings: Option<&td_api::GiftSettings>,
    ) -> GlobalPrivacySettings {
        GlobalPrivacySettings {
            set_type: SetType::Gift,
            gift_settings: crate::star_gift_settings::StarGiftSettings::from_gift_settings(
                settings,
            ),
            ..GlobalPrivacySettings::default()
        }
    }

    /// `GlobalPrivacySettings::apply_changes` (GlobalPrivacySettings.cpp:115-137).
    #[allow(dead_code)] // the get → apply_changes chain parks until the net result handlers land
    fn apply_changes(&mut self, set_settings: &GlobalPrivacySettings) {
        debug_assert_eq!(self.set_type, SetType::None);
        match set_settings.set_type {
            SetType::Archive => {
                self.archive_and_mute_new_noncontact_peers =
                    set_settings.archive_and_mute_new_noncontact_peers;
                self.keep_archived_unmuted = set_settings.keep_archived_unmuted;
                self.keep_archived_folders = set_settings.keep_archived_folders;
            }
            SetType::ReadDate => {
                self.hide_read_marks = set_settings.hide_read_marks;
            }
            SetType::NewChat => {
                self.new_noncontact_peers_require_premium =
                    set_settings.new_noncontact_peers_require_premium;
                self.noncontact_peers_paid_star_count =
                    set_settings.noncontact_peers_paid_star_count;
            }
            SetType::Gift => {
                self.gift_settings = set_settings.gift_settings.clone();
            }
            SetType::None => unreachable!("the C++ UNREACHABLE default"),
        }
    }

    /// `GlobalPrivacySettings::get_read_date_privacy_settings_object`
    /// (GlobalPrivacySettings.cpp:163-167).
    pub(crate) fn get_read_date_privacy_settings_object(&self) -> td_api::ReadDatePrivacySettings {
        debug_assert_eq!(self.set_type, SetType::None);
        td_api::ReadDatePrivacySettings {
            show_read_date: !self.hide_read_marks,
        }
    }

    /// `GlobalPrivacySettings::get_new_chat_privacy_settings_object`
    /// (GlobalPrivacySettings.cpp:169-173).
    pub(crate) fn get_new_chat_privacy_settings_object(&self) -> td_api::NewChatPrivacySettings {
        debug_assert_eq!(self.set_type, SetType::None);
        td_api::NewChatPrivacySettings {
            allow_new_chats_from_unknown_users: !self.new_noncontact_peers_require_premium,
            incoming_paid_message_star_count: self.noncontact_peers_paid_star_count,
        }
    }

    /// `GlobalPrivacySettings::get_input_global_privacy_settings`
    /// (GlobalPrivacySettings.cpp:139-154) — the telegram form the
    /// `SetGlobalPrivacySettingsQuery` sends (its sender rides the
    /// `[blocked: net]` result handlers). The masks are the C++
    /// `NONCONTACT_PEERS_PAID_STARS_MASK`/`DISALLOWED_GIFTS_MASK` (bits
    /// 6/7 of the `globalPrivacySettings` flags).
    #[allow(dead_code)] // the SetGlobalPrivacySettingsQuery sender (the [blocked: net] tail)
    pub(crate) fn get_input_global_privacy_settings(
        &self,
    ) -> tdmtproto::telegram_api::GlobalPrivacySettings {
        debug_assert_eq!(self.set_type, SetType::None);
        let mut flags = 0;
        if self.noncontact_peers_paid_star_count > 0 {
            flags |= 1 << 6; // NONCONTACT_PEERS_PAID_STARS_MASK
        }
        let gifts_settings = self
            .gift_settings
            .get_disallowed_gifts()
            .get_input_disallowed_gifts_settings();
        if gifts_settings.is_some() {
            flags |= 1 << 7; // DISALLOWED_GIFTS_MASK
        }
        tdmtproto::telegram_api::GlobalPrivacySettings {
            flags,
            archive_and_mute_new_noncontact_peers: self.archive_and_mute_new_noncontact_peers,
            keep_archived_unmuted: self.keep_archived_unmuted,
            keep_archived_folders: self.keep_archived_folders,
            hide_read_marks: self.hide_read_marks,
            new_noncontact_peers_require_premium: self.new_noncontact_peers_require_premium,
            display_gifts_button: self.gift_settings.get_display_gifts_button(),
            noncontact_peers_paid_stars: self.noncontact_peers_paid_star_count,
            disallowed_gifts: gifts_settings,
        }
    }

    /// `GlobalPrivacySettings::get_global_privacy_settings`
    /// (GlobalPrivacySettings.cpp:175-177) — the
    /// `account.getGlobalPrivacySettings` query parks at the
    /// `[blocked: net]` seam.
    pub(crate) fn get_global_privacy_settings(
        env: &mut impl PrivacyManagerEnv,
        promise: Promise<GlobalPrivacySettings>,
    ) {
        env.get_global_privacy_via_net(promise);
    }

    /// `GlobalPrivacySettings::set_global_privacy_settings`
    /// (GlobalPrivacySettings.cpp:179-198) — the archive-flag hide is
    /// real; the get → `apply_changes` → `account.setGlobalPrivacySettings`
    /// chain parks at the `[blocked: net]` seam with the caller promise
    /// riding the dropped query (the C++ LambdaPromise convention).
    pub(crate) fn set_global_privacy_settings(
        env: &mut impl PrivacyManagerEnv,
        set_settings: GlobalPrivacySettings,
        promise: Promise<()>,
    ) {
        debug_assert_ne!(set_settings.set_type, SetType::None);
        if set_settings.archive_and_mute_new_noncontact_peers {
            env.hide_archive_and_mute_suggested_action();
        }
        env.set_global_privacy_via_net(set_settings, promise);
    }
}

/// The [`PrivacyManager`]/[`GlobalPrivacySettings`] seams over the `Td`
/// fields — the env convention
/// ([`crate::connection_creator::ConnectionCreatorEnv`]).
pub(crate) trait PrivacyManagerEnv {
    /// `DialogManager::have_dialog_force`.
    fn have_dialog_force(&mut self, dialog_id: DialogId, source: &str) -> bool;

    /// `ChatManager::is_megagroup_channel`.
    fn is_megagroup_channel(&mut self, channel_id: ChannelId) -> bool;

    /// `UserManager::get_user_ids_object`.
    fn get_user_ids_object(&mut self, user_ids: &[UserId], source: &str) -> Vec<i64>;

    /// `DialogManager::get_chat_ids_object`.
    fn get_chat_ids_object(&mut self, dialog_ids: &[DialogId], source: &str) -> Vec<i64>;

    /// `Td::send_update(updateUserPrivacySettingRules)` — the collapsed
    /// update sink (the privacy plane's single update).
    #[allow(dead_code)] // do_update_privacy's emission (the intake plane)
    fn send_update_privacy_rules(&mut self, update: td_api::updateUserPrivacySettingRules);

    /// `UserManager::on_update_online_status_privacy`.
    #[allow(dead_code)] // do_update_privacy's fan-out (the intake plane)
    fn on_update_online_status_privacy(&mut self);

    /// `UserManager::on_update_phone_number_privacy`.
    #[allow(dead_code)] // do_update_privacy's fan-out (the intake plane)
    fn on_update_phone_number_privacy(&mut self);

    /// `UserManager::reload_user` — the user-reload plane (the
    /// unrestricted-user online-status refetch walk parks here).
    #[allow(dead_code)] // do_update_privacy's fan-out (the intake plane)
    fn reload_user(&mut self, user_id: UserId, source: &str);

    /// `SuggestedActionManager::hide_suggested_action(SuggestedAction{
    /// EnableArchiveAndMuteNewChats})` (GlobalPrivacySettings.cpp:182-185).
    fn hide_archive_and_mute_suggested_action(&mut self);

    /// `GetPrivacyQuery::send` (PrivacyManager.cpp:33-35) — the
    /// `account.getPrivacy` query; `[blocked: net]`, the queued caller
    /// promises ride the [`PrivacyManager`] state until the net step's
    /// `on_get_user_privacy_settings` drains them.
    fn get_privacy_via_net(&mut self, key: UserPrivacySettingType);

    /// `SetPrivacyQuery::send` (PrivacyManager.cpp:61-64) — the
    /// `account.setPrivacy` query with the input rules; `[blocked: net]`,
    /// the caller promise rides the seam (the C++ LambdaPromise drop)
    /// until the net step's `on_set_user_privacy_settings` answers it.
    fn set_privacy_via_net(
        &mut self,
        key: UserPrivacySettingType,
        rules: UserPrivacySettingRules,
        promise: Promise<()>,
    );

    /// `GetGlobalPrivacySettingsQuery::send`
    /// (GlobalPrivacySettings.cpp:33-35) — the
    /// `account.getGlobalPrivacySettings` query; `[blocked: net]`.
    fn get_global_privacy_via_net(&mut self, promise: Promise<GlobalPrivacySettings>);

    /// `SetGlobalPrivacySettingsQuery::send`
    /// (GlobalPrivacySettings.cpp:59-62) — the
    /// `account.setGlobalPrivacySettings` tail of the get →
    /// `apply_changes` → set chain; `[blocked: net]`.
    fn set_global_privacy_via_net(&mut self, settings: GlobalPrivacySettings, promise: Promise<()>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_parse::store_i32;

    /// The privacy env over split test halves: the dialog/user builders
    /// answer the id-equal collapse, the user-manager hooks count their
    /// calls, and the net seams park.
    struct TestPrivacyEnv {
        dialogs: Vec<DialogId>,
        megagroups: Vec<ChannelId>,
        online_status_privacy_updates: usize,
        phone_number_privacy_updates: usize,
        reloaded_users: Vec<(UserId, String)>,
        hidden_suggested_actions: usize,
        get_privacy_queries: Vec<UserPrivacySettingType>,
        set_privacy_queries: Vec<(UserPrivacySettingType, usize)>,
        get_global_privacy_queries: usize,
        set_global_privacy_settings: Vec<i64>,
        updates: Vec<td_api::Update>,
    }

    impl TestPrivacyEnv {
        fn new() -> TestPrivacyEnv {
            TestPrivacyEnv {
                dialogs: Vec::new(),
                megagroups: Vec::new(),
                online_status_privacy_updates: 0,
                phone_number_privacy_updates: 0,
                reloaded_users: Vec::new(),
                hidden_suggested_actions: 0,
                get_privacy_queries: Vec::new(),
                set_privacy_queries: Vec::new(),
                get_global_privacy_queries: 0,
                set_global_privacy_settings: Vec::new(),
                updates: Vec::new(),
            }
        }

        fn chat(dialog_id: i64) -> DialogId {
            DialogId::new(dialog_id)
        }
    }

    impl PrivacyManagerEnv for TestPrivacyEnv {
        fn have_dialog_force(&mut self, dialog_id: DialogId, _source: &str) -> bool {
            self.dialogs.contains(&dialog_id)
        }

        fn is_megagroup_channel(&mut self, channel_id: ChannelId) -> bool {
            self.megagroups.contains(&channel_id)
        }

        fn get_user_ids_object(&mut self, user_ids: &[UserId], _source: &str) -> Vec<i64> {
            user_ids.iter().map(|&user_id| user_id.get()).collect()
        }

        fn get_chat_ids_object(&mut self, dialog_ids: &[DialogId], _source: &str) -> Vec<i64> {
            dialog_ids
                .iter()
                .map(|&dialog_id| dialog_id.get())
                .collect()
        }

        fn send_update_privacy_rules(&mut self, update: td_api::updateUserPrivacySettingRules) {
            self.updates
                .push(td_api::Update::updateUserPrivacySettingRules(update));
        }

        fn on_update_online_status_privacy(&mut self) {
            self.online_status_privacy_updates += 1;
        }

        fn on_update_phone_number_privacy(&mut self) {
            self.phone_number_privacy_updates += 1;
        }

        fn reload_user(&mut self, user_id: UserId, source: &str) {
            self.reloaded_users.push((user_id, source.to_string()));
        }

        fn hide_archive_and_mute_suggested_action(&mut self) {
            self.hidden_suggested_actions += 1;
        }

        fn get_privacy_via_net(&mut self, key: UserPrivacySettingType) {
            self.get_privacy_queries.push(key);
        }

        fn set_privacy_via_net(
            &mut self,
            key: UserPrivacySettingType,
            rules: UserPrivacySettingRules,
            _promise: Promise<()>,
        ) {
            self.set_privacy_queries.push((key, rules.rules.len()));
        }

        fn get_global_privacy_via_net(&mut self, _promise: Promise<GlobalPrivacySettings>) {
            self.get_global_privacy_queries += 1;
        }

        fn set_global_privacy_via_net(
            &mut self,
            settings: GlobalPrivacySettings,
            _promise: Promise<()>,
        ) {
            self.set_global_privacy_settings
                .push(settings.noncontact_peers_paid_star_count);
        }
    }

    fn show_status() -> td_api::UserPrivacySetting {
        td_api::UserPrivacySetting::userPrivacySettingShowStatus(
            td_api::userPrivacySettingShowStatus {},
        )
    }

    fn rules_of(
        rules: Vec<td_api::UserPrivacySettingRule>,
    ) -> Option<td_api::UserPrivacySettingRules> {
        Some(td_api::UserPrivacySettingRules { rules })
    }

    /// The privacy-key intake table: every td_api key maps to the C++
    /// `Type` discriminant, and the null key answers the 400 band.
    #[test]
    fn user_privacy_setting_key_table_matches_the_cpp() {
        let cases = [
            (
                td_api::UserPrivacySetting::userPrivacySettingShowStatus(
                    td_api::userPrivacySettingShowStatus {},
                ),
                UserPrivacySettingType::UserStatus,
            ),
            (
                td_api::UserPrivacySetting::userPrivacySettingAllowChatInvites(
                    td_api::userPrivacySettingAllowChatInvites {},
                ),
                UserPrivacySettingType::ChatInvite,
            ),
            (
                td_api::UserPrivacySetting::userPrivacySettingAllowCalls(
                    td_api::userPrivacySettingAllowCalls {},
                ),
                UserPrivacySettingType::Call,
            ),
            (
                td_api::UserPrivacySetting::userPrivacySettingAllowPeerToPeerCalls(
                    td_api::userPrivacySettingAllowPeerToPeerCalls {},
                ),
                UserPrivacySettingType::PeerToPeerCall,
            ),
            (
                td_api::UserPrivacySetting::userPrivacySettingShowLinkInForwardedMessages(
                    td_api::userPrivacySettingShowLinkInForwardedMessages {},
                ),
                UserPrivacySettingType::LinkInForwardedMessages,
            ),
            (
                td_api::UserPrivacySetting::userPrivacySettingShowProfilePhoto(
                    td_api::userPrivacySettingShowProfilePhoto {},
                ),
                UserPrivacySettingType::UserProfilePhoto,
            ),
            (
                td_api::UserPrivacySetting::userPrivacySettingShowPhoneNumber(
                    td_api::userPrivacySettingShowPhoneNumber {},
                ),
                UserPrivacySettingType::UserPhoneNumber,
            ),
            (
                td_api::UserPrivacySetting::userPrivacySettingAllowFindingByPhoneNumber(
                    td_api::userPrivacySettingAllowFindingByPhoneNumber {},
                ),
                UserPrivacySettingType::FindByPhoneNumber,
            ),
            (
                td_api::UserPrivacySetting::userPrivacySettingAllowPrivateVoiceAndVideoNoteMessages(
                    td_api::userPrivacySettingAllowPrivateVoiceAndVideoNoteMessages {},
                ),
                UserPrivacySettingType::VoiceMessages,
            ),
            (
                td_api::UserPrivacySetting::userPrivacySettingShowBio(
                    td_api::userPrivacySettingShowBio {},
                ),
                UserPrivacySettingType::UserBio,
            ),
            (
                td_api::UserPrivacySetting::userPrivacySettingShowBirthdate(
                    td_api::userPrivacySettingShowBirthdate {},
                ),
                UserPrivacySettingType::UserBirthdate,
            ),
            (
                td_api::UserPrivacySetting::userPrivacySettingAutosaveGifts(
                    td_api::userPrivacySettingAutosaveGifts {},
                ),
                UserPrivacySettingType::StarGiftAutosave,
            ),
            (
                td_api::UserPrivacySetting::userPrivacySettingAllowUnpaidMessages(
                    td_api::userPrivacySettingAllowUnpaidMessages {},
                ),
                UserPrivacySettingType::NoPaidMessages,
            ),
            (
                td_api::UserPrivacySetting::userPrivacySettingShowProfileAudio(
                    td_api::userPrivacySettingShowProfileAudio {},
                ),
                UserPrivacySettingType::SavedMusic,
            ),
        ];
        assert_eq!(cases.len(), UserPrivacySettingType::SIZE);
        for (key, expected) in cases {
            let parsed = UserPrivacySettingType::get_user_privacy_setting(Some(&key)).unwrap();
            assert_eq!(parsed, expected);
            // the builder round-trips onto the same td_api shape
            let rebuilt = parsed.get_user_privacy_setting_object();
            assert_eq!(
                std::mem::discriminant(&rebuilt),
                std::mem::discriminant(&key)
            );
        }
        let error = UserPrivacySettingType::get_user_privacy_setting(None).unwrap_err();
        assert_eq!(error.code(), 400);
        assert_eq!(error.message(), "UserPrivacySetting must be non-empty");
    }

    /// The rules intake bands and the round-trip builder: the 400 band for
    /// the null rules, the valid-id filter, the dialog walk and the
    /// AllowCloseFriends degradation.
    #[test]
    fn rules_intake_and_builders_match_the_cpp() {
        let mut env = TestPrivacyEnv::new();
        env.dialogs = vec![
            TestPrivacyEnv::chat(-100),
            TestPrivacyEnv::chat(-1_000_000_000_002),
            TestPrivacyEnv::chat(-1_000_000_000_003),
        ];
        // -1000000000002 stays a broadcast: the C++ `Ignore broadcast`
        // filter; -500 exercises `Ignore not found` and 7 the `Ignore`
        // default (a user id carries no chat-member ids)
        env.megagroups = vec![DialogId::new(-1_000_000_000_003).get_channel_id()];

        assert_eq!(
            UserPrivacySettingRules::get_user_privacy_setting_rules(&mut env, None)
                .unwrap_err()
                .message(),
            "UserPrivacySettingRules must be non-empty"
        );

        let rules = rules_of(vec![
            td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowUsers(
                td_api::userPrivacySettingRuleAllowUsers {
                    user_ids: vec![1, 2, -3, 4],
                },
            ),
            td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowChatMembers(
                td_api::userPrivacySettingRuleAllowChatMembers {
                    chat_ids: vec![-100, -500, -1_000_000_000_002, -1_000_000_000_003, 7],
                },
            ),
            td_api::UserPrivacySettingRule::userPrivacySettingRuleRestrictUsers(
                td_api::userPrivacySettingRuleRestrictUsers {
                    user_ids: vec![9, 1, 9],
                },
            ),
            td_api::UserPrivacySettingRule::userPrivacySettingRuleRestrictAll(
                td_api::userPrivacySettingRuleRestrictAll {},
            ),
        ]);
        let parsed =
            UserPrivacySettingRules::get_user_privacy_setting_rules(&mut env, rules.as_ref())
                .unwrap();
        assert_eq!(parsed.rules.len(), 4);
        // the valid-id filter drops -3
        assert_eq!(
            parsed.rules[0]
                .user_ids
                .iter()
                .map(|&user_id| user_id.get())
                .collect::<Vec<_>>(),
            vec![1, 2, 4]
        );
        // the dialog walk keeps the basic group -100 and the megagroup
        // -1000000000003; the broadcast -1000000000002, the unloaded -500
        // and the user id 7 all drop
        assert_eq!(
            parsed.rules[1]
                .dialog_ids
                .iter()
                .map(|&dialog_id| dialog_id.get())
                .collect::<Vec<_>>(),
            vec![-100, -1_000_000_000_003]
        );
        // the restricted-id set is the sorted dedup
        assert_eq!(
            parsed
                .get_restricted_user_ids()
                .iter()
                .map(|&user_id| user_id.get())
                .collect::<Vec<_>>(),
            vec![1, 9]
        );

        // the builder round-trips: the ids ride the id-equal collapse, the
        // bare rules stay bare
        let built = parsed.get_user_privacy_setting_rules_object(&mut env);
        assert_eq!(built.rules.len(), 4);
        match &built.rules[3] {
            td_api::UserPrivacySettingRule::userPrivacySettingRuleRestrictAll(_) => {}
            other => panic!("wrong round-tripped rule {other:?}"),
        }

        // the AllowCloseFriends rule degrades to an empty AllowUsers with
        // the C++ error log
        let close_friends = UserPrivacySettingRule::bare(rule_type::ALLOW_CLOSE_FRIENDS);
        let built = close_friends.get_user_privacy_setting_rule_object(&mut env);
        match built {
            td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowUsers(ids) => {
                assert!(ids.user_ids.is_empty());
            }
            other => panic!("wrong degraded rule {other:?}"),
        }
    }

    /// The `get_privacy`/`set_privacy` decision table: the 400 band, the
    /// parked query, the promise-queue dedup and the synchronized
    /// cache-hit answering the builder.
    #[test]
    fn privacy_manager_decisions_match_the_cpp() {
        let mut env = TestPrivacyEnv::new();
        let mut manager = PrivacyManager::new();

        // the null key 400s before any state touch
        let promise = Promise::default();
        assert_eq!(
            manager
                .get_privacy(&mut env, None, promise)
                .unwrap_err()
                .message(),
            "UserPrivacySetting must be non-empty"
        );

        // the first get queues and sends the query
        let promise = Promise::default();
        manager
            .get_privacy(&mut env, Some(&show_status()), promise)
            .unwrap();
        assert_eq!(
            env.get_privacy_queries,
            vec![UserPrivacySettingType::UserStatus]
        );

        // the second get only queues
        let promise = Promise::default();
        manager
            .get_privacy(&mut env, Some(&show_status()), promise)
            .unwrap();
        assert_eq!(env.get_privacy_queries.len(), 1);

        // the set intake 400s on the null rules
        let promise = Promise::default();
        assert_eq!(
            manager
                .set_privacy(&mut env, Some(&show_status()), None, promise)
                .unwrap_err()
                .message(),
            "UserPrivacySettingRules must be non-empty"
        );

        // the first set sends the query with the parsed rules
        let rules = rules_of(vec![
            td_api::UserPrivacySettingRule::userPrivacySettingRuleAllowAll(
                td_api::userPrivacySettingRuleAllowAll {},
            ),
        ]);
        let promise = Promise::default();
        manager
            .set_privacy(&mut env, Some(&show_status()), rules.as_ref(), promise)
            .unwrap();
        assert_eq!(
            env.set_privacy_queries,
            vec![(UserPrivacySettingType::UserStatus, 1)]
        );

        // the dedup set parks the pending rules
        let promise = Promise::default();
        manager
            .set_privacy(&mut env, Some(&show_status()), rules.as_ref(), promise)
            .unwrap();
        assert_eq!(env.set_privacy_queries.len(), 1);
        assert!(
            manager
                .get_info(UserPrivacySettingType::UserStatus)
                .has_set_query
        );
        assert_eq!(
            manager
                .get_info(UserPrivacySettingType::UserStatus)
                .pending_rules
                .as_ref()
                .unwrap()
                .rules
                .len(),
            1
        );

        // a second key has its own info slot
        let allow_calls = td_api::UserPrivacySetting::userPrivacySettingAllowCalls(
            td_api::userPrivacySettingAllowCalls {},
        );
        let promise = Promise::default();
        manager
            .get_privacy(&mut env, Some(&allow_calls), promise)
            .unwrap();
        assert_eq!(
            env.get_privacy_queries,
            vec![
                UserPrivacySettingType::UserStatus,
                UserPrivacySettingType::Call
            ]
        );

        // do_update_privacy flips the synchronized bit and the cache-hit
        // get answers the builder without a new query
        let rules = UserPrivacySettingRules {
            rules: vec![UserPrivacySettingRule::bare(rule_type::ALLOW_ALL)],
        };
        manager.do_update_privacy(&mut env, UserPrivacySettingType::UserStatus, rules, false);
        assert_eq!(env.updates.len(), 1);
        match &env.updates[0] {
            td_api::Update::updateUserPrivacySettingRules(update) => {
                assert!(update.setting.is_some());
                assert_eq!(update.rules.as_ref().unwrap().rules.len(), 1);
            }
            other => panic!("wrong update {other:?}"),
        }

        let promise = Promise::default();
        manager
            .get_privacy(&mut env, Some(&show_status()), promise)
            .unwrap();
        assert_eq!(env.get_privacy_queries.len(), 2);
    }

    /// The GlobalPrivacySettings local plane: the null-settings bands, the
    /// star-count clamp, the builders and the apply_changes merge.
    #[test]
    fn global_privacy_settings_plane_matches_the_cpp() {
        // ReadDate: the null settings hide the read marks, the explicit
        // false too, the true keeps them
        assert!(GlobalPrivacySettings::from_read_date_privacy_settings(None).hide_read_marks);
        assert!(
            GlobalPrivacySettings::from_read_date_privacy_settings(Some(
                td_api::ReadDatePrivacySettings {
                    show_read_date: false
                }
            ))
            .hide_read_marks
        );
        let read_date = GlobalPrivacySettings::from_read_date_privacy_settings(Some(
            td_api::ReadDatePrivacySettings {
                show_read_date: true,
            },
        ));
        assert!(!read_date.hide_read_marks);
        assert_eq!(read_date.set_type, SetType::ReadDate);
        // the rewrap getters CHECK set_type == None
        // (GlobalPrivacySettings.cpp:165/170) — they run on the fetched
        // response-shaped settings, never on the set intake
        let mut fetched = GlobalPrivacySettings::default();
        assert_eq!(
            fetched.get_read_date_privacy_settings_object(),
            td_api::ReadDatePrivacySettings {
                show_read_date: true
            }
        );
        fetched.hide_read_marks = true;
        assert_eq!(
            fetched.get_read_date_privacy_settings_object(),
            td_api::ReadDatePrivacySettings {
                show_read_date: false
            }
        );

        // NewChat: the null settings require premium with a zero count;
        // the star count clamps to [0, 1000000]
        let new_chat = GlobalPrivacySettings::from_new_chat_privacy_settings(None);
        assert!(new_chat.new_noncontact_peers_require_premium);
        assert_eq!(new_chat.noncontact_peers_paid_star_count, 0);
        let fetched = GlobalPrivacySettings::default();
        assert_eq!(
            fetched.get_new_chat_privacy_settings_object(),
            td_api::NewChatPrivacySettings {
                allow_new_chats_from_unknown_users: true,
                incoming_paid_message_star_count: 0,
            }
        );

        let new_chat = GlobalPrivacySettings::from_new_chat_privacy_settings(Some(
            td_api::NewChatPrivacySettings {
                allow_new_chats_from_unknown_users: true,
                incoming_paid_message_star_count: 2_000_000,
            },
        ));
        assert!(!new_chat.new_noncontact_peers_require_premium);
        assert_eq!(new_chat.noncontact_peers_paid_star_count, 1_000_000);
        let fetched = GlobalPrivacySettings {
            new_noncontact_peers_require_premium: true,
            noncontact_peers_paid_star_count: 3,
            ..GlobalPrivacySettings::default()
        };
        assert_eq!(
            fetched.get_new_chat_privacy_settings_object(),
            td_api::NewChatPrivacySettings {
                allow_new_chats_from_unknown_users: false,
                incoming_paid_message_star_count: 3,
            }
        );

        // apply_changes merges only the NewChat slice into the fetched
        // settings (set_type stays None on the fetched side, so the pin
        // is per-field); the ReadDate slice merges too
        let mut fetched = GlobalPrivacySettings::default();
        fetched.apply_changes(&new_chat);
        assert!(!fetched.new_noncontact_peers_require_premium);
        assert_eq!(fetched.noncontact_peers_paid_star_count, 1_000_000);
        assert!(!fetched.archive_and_mute_new_noncontact_peers);
        assert!(!fetched.hide_read_marks);
        fetched.apply_changes(&GlobalPrivacySettings::from_read_date_privacy_settings(
            Some(td_api::ReadDatePrivacySettings {
                show_read_date: false,
            }),
        ));
        assert!(fetched.hide_read_marks);
        fetched.apply_changes(&read_date);
        assert!(!fetched.hide_read_marks);

        // the set flow: the archive hide is statically dead for the
        // ReadDate/NewChat set types, and the whole chain parks at the
        // seam with the caller promise
        let mut env = TestPrivacyEnv::new();
        let promise = Promise::default();
        GlobalPrivacySettings::set_global_privacy_settings(&mut env, read_date, promise);
        assert_eq!(env.hidden_suggested_actions, 0);
        assert_eq!(env.get_global_privacy_queries, 0);
        assert_eq!(env.set_global_privacy_settings.len(), 1);

        let promise = Promise::default();
        GlobalPrivacySettings::get_global_privacy_settings(&mut env, promise);
        assert_eq!(env.get_global_privacy_queries, 1);
    }

    /// The `get_privacy_via_net` seam stays net-shaped: the parked
    /// key-type flows out and the caller promises ride the manager state.
    /// The byte-pin half pins the rule-type discriminants the C++ enum
    /// fixes (the store form is the i32 value).
    #[test]
    fn rule_type_discriminants_match_the_cpp_order() {
        // UserPrivacySettingRule.h:80-92 — the store form is the i32
        // discriminant, pinned here against reordering.
        let mut out = Vec::new();
        store_i32(&mut out, rule_type::RESTRICT_USERS);
        assert_eq!(out, vec![7, 0, 0, 0]);
        let mut out = Vec::new();
        store_i32(&mut out, rule_type::ALLOW_CHAT_PARTICIPANTS);
        assert_eq!(out, vec![4, 0, 0, 0]);
        assert_eq!(rule_type::ALLOW_CONTACTS, 0);
        assert_eq!(rule_type::RESTRICT_BOTS, 11);
    }
}
