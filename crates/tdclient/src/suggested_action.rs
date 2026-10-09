//! `SuggestedAction` (td/telegram/SuggestedAction.{h,cpp,hpp}) — the M.33
//! Wave-3 suggested-action value class + the list helpers the manager
//! uses.

use crate::chat_manager::ChannelId;
use crate::dialog_id::DialogId;
use crate::message_entity::{FormattedText, get_formatted_text, get_formatted_text_object};
use crate::misc::clean_input_string;
use crate::store_parse::{Parser, store_i32, store_string};
use crate::td_api;
use tdmtproto::telegram_api;

/// `SuggestedAction::Type` (SuggestedAction.h:13-33).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SuggestedActionType {
    #[default]
    Empty,
    EnableArchiveAndMuteNewChats,
    CheckPhoneNumber,
    ViewChecksHint,
    ConvertToGigagroup,
    CheckPassword,
    SetPassword,
    UpgradePremium,
    SubscribeToAnnualPremium,
    RestorePremium,
    GiftPremiumForChristmas,
    BirthdaySetup,
    PremiumGrace,
    StarsSubscriptionLowBalance,
    UserpicSetup,
    Custom,
    SetupLoginEmail,
    SetupLoginEmailNoskip,
    SetupPasskey,
    WalletFirstIncomingTransfer,
}

impl SuggestedActionType {
    fn from_i32(value: i32) -> SuggestedActionType {
        match value {
            0 => SuggestedActionType::Empty,
            1 => SuggestedActionType::EnableArchiveAndMuteNewChats,
            2 => SuggestedActionType::CheckPhoneNumber,
            3 => SuggestedActionType::ViewChecksHint,
            4 => SuggestedActionType::ConvertToGigagroup,
            5 => SuggestedActionType::CheckPassword,
            6 => SuggestedActionType::SetPassword,
            7 => SuggestedActionType::UpgradePremium,
            8 => SuggestedActionType::SubscribeToAnnualPremium,
            9 => SuggestedActionType::RestorePremium,
            10 => SuggestedActionType::GiftPremiumForChristmas,
            11 => SuggestedActionType::BirthdaySetup,
            12 => SuggestedActionType::PremiumGrace,
            13 => SuggestedActionType::StarsSubscriptionLowBalance,
            14 => SuggestedActionType::UserpicSetup,
            15 => SuggestedActionType::Custom,
            16 => SuggestedActionType::SetupLoginEmail,
            17 => SuggestedActionType::SetupLoginEmailNoskip,
            18 => SuggestedActionType::SetupPasskey,
            19 => SuggestedActionType::WalletFirstIncomingTransfer,
            _ => unreachable!("invalid suggested action type {value}"),
        }
    }
}

/// `td::SuggestedAction` — the action + its dialog context. The equality
/// and hash cover the C++ `operator==`/`SuggestedActionHash` fields only
/// (type, dialog, custom type, url) — the title/description are excluded
/// (SuggestedAction.h:88-99).
#[derive(Clone, Debug, Default)]
pub struct SuggestedAction {
    pub type_: SuggestedActionType,
    pub dialog_id: DialogId,
    pub otherwise_relogin_days: i32,
    pub custom_type: String,
    pub title: FormattedText,
    pub description: FormattedText,
    pub url: String,
}

/// `SuggestedActionHash` + `operator==` (SuggestedAction.h:88-99) — the
/// equality ignores the title/description (the hash does too).
impl PartialEq for SuggestedAction {
    fn eq(&self, other: &Self) -> bool {
        self.type_ == other.type_
            && self.dialog_id == other.dialog_id
            && self.custom_type == other.custom_type
            && self.url == other.url
    }
}

impl Eq for SuggestedAction {}

impl std::hash::Hash for SuggestedAction {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.dialog_id.hash(state);
        self.type_.hash(state);
        self.custom_type.hash(state);
        self.url.hash(state);
    }
}

impl PartialOrd for SuggestedAction {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SuggestedAction {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.dialog_id
            .get()
            .cmp(&other.dialog_id.get())
            .then(self.type_.cmp(&other.type_))
            .then(self.custom_type.cmp(&other.custom_type))
            .then(self.url.cmp(&other.url))
    }
}

impl SuggestedAction {
    /// `init` (SuggestedAction.cpp:25-27).
    pub fn init(&mut self, type_: SuggestedActionType) {
        *self = SuggestedAction::default();
        self.type_ = type_;
    }

    /// `SuggestedAction(Slice action_str)` (SuggestedAction.cpp:29-56).
    pub fn from_action_str(action_str: &str) -> SuggestedAction {
        let type_ = match action_str {
            "AUTOARCHIVE_POPULAR" => SuggestedActionType::EnableArchiveAndMuteNewChats,
            "VALIDATE_PASSWORD" => SuggestedActionType::CheckPassword,
            "VALIDATE_PHONE_NUMBER" => SuggestedActionType::CheckPhoneNumber,
            "NEWCOMER_TICKS" => SuggestedActionType::ViewChecksHint,
            "SETUP_PASSWORD" => SuggestedActionType::SetPassword,
            "PREMIUM_UPGRADE" => SuggestedActionType::UpgradePremium,
            "PREMIUM_ANNUAL" => SuggestedActionType::SubscribeToAnnualPremium,
            "PREMIUM_RESTORE" => SuggestedActionType::RestorePremium,
            "PREMIUM_CHRISTMAS" => SuggestedActionType::GiftPremiumForChristmas,
            "BIRTHDAY_SETUP" => SuggestedActionType::BirthdaySetup,
            "PREMIUM_GRACE" => SuggestedActionType::PremiumGrace,
            "STARS_SUBSCRIPTION_LOW_BALANCE" => SuggestedActionType::StarsSubscriptionLowBalance,
            "USERPIC_SETUP" => SuggestedActionType::UserpicSetup,
            "SETUP_LOGIN_EMAIL" => SuggestedActionType::SetupLoginEmail,
            "SETUP_LOGIN_EMAIL_NOSKIP" => SuggestedActionType::SetupLoginEmailNoskip,
            "SETUP_PASSKEY" => SuggestedActionType::SetupPasskey,
            "WALLET_FIRST_INCOMING_TRANSFER" => SuggestedActionType::WalletFirstIncomingTransfer,
            _ => SuggestedActionType::Empty,
        };
        SuggestedAction {
            type_,
            ..SuggestedAction::default()
        }
    }

    /// `SuggestedAction(Slice action_str, DialogId)` (SuggestedAction.cpp:58-63).
    pub fn from_action_str_and_dialog(action_str: &str, dialog_id: DialogId) -> SuggestedAction {
        debug_assert!(dialog_id.is_valid());
        if action_str == "CONVERT_GIGAGROUP" {
            SuggestedAction {
                type_: SuggestedActionType::ConvertToGigagroup,
                dialog_id,
                ..SuggestedAction::default()
            }
        } else {
            SuggestedAction::default()
        }
    }

    /// `SuggestedAction(const UserManager *, pendingSuggestion)`
    /// (SuggestedAction.cpp:65-76) — the Custom ctor.
    pub fn from_pending_suggestion(
        user_manager: Option<&mut ()>,
        action: &telegram_api::PendingSuggestion,
    ) -> SuggestedAction {
        if action.suggestion.is_empty() || action.url.is_empty() {
            return SuggestedAction::default();
        }
        let get_text = |text: &Option<telegram_api::TextWithEntities>| {
            text.as_ref()
                .map_or(String::new(), |text| text.text.clone())
        };
        SuggestedAction {
            type_: SuggestedActionType::Custom,
            custom_type: action.suggestion.clone(),
            // the ()-resolver is a no-op, so the two conversions share it
            // through one use; the real resolver arrives when UserManager
            // implements UserManagerLike (the M.8 integration note)
            title: get_formatted_text(
                user_manager,
                get_text(&action.title),
                Vec::new(),
                true,
                false,
                "SuggestedAction",
            ),
            description: get_formatted_text(
                None::<&mut ()>,
                get_text(&action.description),
                Vec::new(),
                true,
                false,
                "SuggestedAction",
            ),
            url: action.url.clone(),
            ..SuggestedAction::default()
        }
    }

    /// `SuggestedAction(td_api::SuggestedAction&&)` (SuggestedAction.cpp:79-159).
    pub fn from_td_api(suggested_action: &td_api::SuggestedAction) -> SuggestedAction {
        let mut result = SuggestedAction::default();
        match suggested_action {
            td_api::SuggestedAction::suggestedActionEnableArchiveAndMuteNewChats(_) => {
                result.init(SuggestedActionType::EnableArchiveAndMuteNewChats);
            }
            td_api::SuggestedAction::suggestedActionCheckPassword(_) => {
                result.init(SuggestedActionType::CheckPassword);
            }
            td_api::SuggestedAction::suggestedActionCheckPhoneNumber(_) => {
                result.init(SuggestedActionType::CheckPhoneNumber);
            }
            td_api::SuggestedAction::suggestedActionViewChecksHint(_) => {
                result.init(SuggestedActionType::ViewChecksHint);
            }
            td_api::SuggestedAction::suggestedActionConvertToBroadcastGroup(action) => {
                let channel_id = ChannelId::new(action.supergroup_id);
                if channel_id.is_valid() {
                    result.type_ = SuggestedActionType::ConvertToGigagroup;
                    result.dialog_id = DialogId::from_channel_id(channel_id);
                }
            }
            td_api::SuggestedAction::suggestedActionSetPassword(action) => {
                result.type_ = SuggestedActionType::SetPassword;
                result.otherwise_relogin_days = action.authorization_delay;
            }
            td_api::SuggestedAction::suggestedActionUpgradePremium(_) => {
                result.init(SuggestedActionType::UpgradePremium);
            }
            td_api::SuggestedAction::suggestedActionSubscribeToAnnualPremium(_) => {
                result.init(SuggestedActionType::SubscribeToAnnualPremium);
            }
            td_api::SuggestedAction::suggestedActionRestorePremium(_) => {
                result.init(SuggestedActionType::RestorePremium);
            }
            td_api::SuggestedAction::suggestedActionGiftPremiumForChristmas(_) => {
                result.init(SuggestedActionType::GiftPremiumForChristmas);
            }
            td_api::SuggestedAction::suggestedActionSetBirthdate(_) => {
                result.init(SuggestedActionType::BirthdaySetup);
            }
            td_api::SuggestedAction::suggestedActionExtendPremium(_) => {
                result.init(SuggestedActionType::PremiumGrace);
            }
            td_api::SuggestedAction::suggestedActionExtendStarSubscriptions(_) => {
                result.init(SuggestedActionType::StarsSubscriptionLowBalance);
            }
            td_api::SuggestedAction::suggestedActionSetProfilePhoto(_) => {
                result.init(SuggestedActionType::UserpicSetup);
            }
            td_api::SuggestedAction::suggestedActionCustom(action) => {
                if clean_input_string(&action.name).is_some()
                    && clean_input_string(&action.url).is_some()
                {
                    result.type_ = SuggestedActionType::Custom;
                    result.custom_type = action.name.clone();
                    result.url = action.url.clone();
                }
            }
            td_api::SuggestedAction::suggestedActionSetLoginEmailAddress(action) => {
                result.init(if action.can_be_hidden {
                    SuggestedActionType::SetupLoginEmail
                } else {
                    SuggestedActionType::SetupLoginEmailNoskip
                });
            }
            td_api::SuggestedAction::suggestedActionAddLoginPasskey(_) => {
                result.init(SuggestedActionType::SetupPasskey);
            }
            td_api::SuggestedAction::suggestedActionViewFirstTonWalletTransferHint(_) => {
                result.init(SuggestedActionType::WalletFirstIncomingTransfer);
            }
        }
        result
    }

    /// `is_empty` (SuggestedAction.h:58-60).
    pub fn is_empty(&self) -> bool {
        self.type_ == SuggestedActionType::Empty
    }

    /// `is_persistent` (SuggestedAction.h:61-63).
    pub fn is_persistent(&self) -> bool {
        !self.is_empty()
            && self.type_ != SuggestedActionType::SetupLoginEmail
            && self.type_ != SuggestedActionType::SetupLoginEmailNoskip
    }

    /// `get_suggested_action_str` (SuggestedAction.cpp:161-193).
    pub fn get_suggested_action_str(&self) -> String {
        match self.type_ {
            SuggestedActionType::EnableArchiveAndMuteNewChats => "AUTOARCHIVE_POPULAR".to_string(),
            SuggestedActionType::CheckPassword => "VALIDATE_PASSWORD".to_string(),
            SuggestedActionType::CheckPhoneNumber => "VALIDATE_PHONE_NUMBER".to_string(),
            SuggestedActionType::ViewChecksHint => "NEWCOMER_TICKS".to_string(),
            SuggestedActionType::ConvertToGigagroup => "CONVERT_GIGAGROUP".to_string(),
            SuggestedActionType::SetPassword => "SETUP_PASSWORD".to_string(),
            SuggestedActionType::UpgradePremium => "PREMIUM_UPGRADE".to_string(),
            SuggestedActionType::SubscribeToAnnualPremium => "PREMIUM_ANNUAL".to_string(),
            SuggestedActionType::RestorePremium => "PREMIUM_RESTORE".to_string(),
            SuggestedActionType::GiftPremiumForChristmas => "PREMIUM_CHRISTMAS".to_string(),
            SuggestedActionType::BirthdaySetup => "BIRTHDAY_SETUP".to_string(),
            SuggestedActionType::PremiumGrace => "PREMIUM_GRACE".to_string(),
            SuggestedActionType::StarsSubscriptionLowBalance => {
                "STARS_SUBSCRIPTION_LOW_BALANCE".to_string()
            }
            SuggestedActionType::UserpicSetup => "USERPIC_SETUP".to_string(),
            SuggestedActionType::Custom => self.custom_type.clone(),
            SuggestedActionType::SetupLoginEmail => "SETUP_LOGIN_EMAIL".to_string(),
            SuggestedActionType::SetupLoginEmailNoskip => "SETUP_LOGIN_EMAIL_NOSKIP".to_string(),
            SuggestedActionType::SetupPasskey => "SETUP_PASSKEY".to_string(),
            SuggestedActionType::WalletFirstIncomingTransfer => {
                "WALLET_FIRST_INCOMING_TRANSFER".to_string()
            }
            SuggestedActionType::Empty => String::new(),
        }
    }

    /// `get_suggested_action_object` (SuggestedAction.cpp:195-241) — the
    /// `premium_manage_subscription_url` option is the env seam (the C++
    /// reads `G()->get_option_string`).
    pub fn get_suggested_action_object(
        &self,
        user_manager: Option<&mut ()>,
        premium_manage_subscription_url: &str,
    ) -> Option<td_api::SuggestedAction> {
        let object = match self.type_ {
            SuggestedActionType::Empty => return None,
            SuggestedActionType::EnableArchiveAndMuteNewChats => {
                td_api::SuggestedAction::suggestedActionEnableArchiveAndMuteNewChats(
                    td_api::suggestedActionEnableArchiveAndMuteNewChats,
                )
            }
            SuggestedActionType::CheckPassword => {
                td_api::SuggestedAction::suggestedActionCheckPassword(
                    td_api::suggestedActionCheckPassword,
                )
            }
            SuggestedActionType::CheckPhoneNumber => {
                td_api::SuggestedAction::suggestedActionCheckPhoneNumber(
                    td_api::suggestedActionCheckPhoneNumber,
                )
            }
            SuggestedActionType::ViewChecksHint => {
                td_api::SuggestedAction::suggestedActionViewChecksHint(
                    td_api::suggestedActionViewChecksHint,
                )
            }
            SuggestedActionType::WalletFirstIncomingTransfer => {
                td_api::SuggestedAction::suggestedActionViewFirstTonWalletTransferHint(
                    td_api::suggestedActionViewFirstTonWalletTransferHint,
                )
            }
            SuggestedActionType::ConvertToGigagroup => {
                td_api::SuggestedAction::suggestedActionConvertToBroadcastGroup(
                    td_api::suggestedActionConvertToBroadcastGroup {
                        supergroup_id: self.dialog_id.get_channel_id().get(),
                    },
                )
            }
            SuggestedActionType::SetPassword => {
                td_api::SuggestedAction::suggestedActionSetPassword(
                    td_api::suggestedActionSetPassword {
                        authorization_delay: self.otherwise_relogin_days,
                    },
                )
            }
            SuggestedActionType::UpgradePremium => {
                td_api::SuggestedAction::suggestedActionUpgradePremium(
                    td_api::suggestedActionUpgradePremium,
                )
            }
            SuggestedActionType::SubscribeToAnnualPremium => {
                td_api::SuggestedAction::suggestedActionSubscribeToAnnualPremium(
                    td_api::suggestedActionSubscribeToAnnualPremium,
                )
            }
            SuggestedActionType::RestorePremium => {
                td_api::SuggestedAction::suggestedActionRestorePremium(
                    td_api::suggestedActionRestorePremium,
                )
            }
            SuggestedActionType::GiftPremiumForChristmas => {
                td_api::SuggestedAction::suggestedActionGiftPremiumForChristmas(
                    td_api::suggestedActionGiftPremiumForChristmas,
                )
            }
            SuggestedActionType::BirthdaySetup => {
                td_api::SuggestedAction::suggestedActionSetBirthdate(
                    td_api::suggestedActionSetBirthdate,
                )
            }
            SuggestedActionType::PremiumGrace => {
                td_api::SuggestedAction::suggestedActionExtendPremium(
                    td_api::suggestedActionExtendPremium {
                        manage_premium_subscription_url: premium_manage_subscription_url
                            .to_string(),
                    },
                )
            }
            SuggestedActionType::StarsSubscriptionLowBalance => {
                td_api::SuggestedAction::suggestedActionExtendStarSubscriptions(
                    td_api::suggestedActionExtendStarSubscriptions,
                )
            }
            SuggestedActionType::UserpicSetup => {
                td_api::SuggestedAction::suggestedActionSetProfilePhoto(
                    td_api::suggestedActionSetProfilePhoto,
                )
            }
            SuggestedActionType::Custom => {
                td_api::SuggestedAction::suggestedActionCustom(td_api::suggestedActionCustom {
                    name: self.custom_type.clone(),
                    title: Some(get_formatted_text_object(
                        user_manager,
                        &self.title,
                        true,
                        -1,
                    )),
                    description: Some(get_formatted_text_object(
                        None::<&mut ()>,
                        &self.description,
                        true,
                        -1,
                    )),
                    url: self.url.clone(),
                })
            }
            SuggestedActionType::SetupLoginEmail => {
                td_api::SuggestedAction::suggestedActionSetLoginEmailAddress(
                    td_api::suggestedActionSetLoginEmailAddress {
                        can_be_hidden: true,
                    },
                )
            }
            SuggestedActionType::SetupLoginEmailNoskip => {
                td_api::SuggestedAction::suggestedActionSetLoginEmailAddress(
                    td_api::suggestedActionSetLoginEmailAddress {
                        can_be_hidden: false,
                    },
                )
            }
            SuggestedActionType::SetupPasskey => {
                td_api::SuggestedAction::suggestedActionAddLoginPasskey(
                    td_api::suggestedActionAddLoginPasskey,
                )
            }
        };
        Some(object)
    }

    /// `SuggestedAction::store` (SuggestedAction.hpp).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_dialog_id = self.dialog_id != DialogId::default();
        let has_otherwise_relogin_days = self.otherwise_relogin_days != 0;
        crate::store_parse::store_flags(out, [has_dialog_id, has_otherwise_relogin_days]);
        store_i32(out, self.type_ as i32);
        if has_dialog_id {
            self.dialog_id.store(out);
        }
        if has_otherwise_relogin_days {
            store_i32(out, self.otherwise_relogin_days);
        }
        if self.type_ == SuggestedActionType::Custom {
            store_string(out, &self.custom_type);
            self.title.store(out);
            self.description.store(out);
            store_string(out, &self.url);
        }
    }

    /// `SuggestedAction::parse` (SuggestedAction.hpp).
    pub fn parse(parser: &mut Parser) -> Result<SuggestedAction, &'static str> {
        let [has_dialog_id, has_otherwise_relogin_days] = parser.parse_flags()?;
        let type_ = SuggestedActionType::from_i32(parser.i32_()?);
        let dialog_id = if has_dialog_id {
            DialogId::parse_from(parser)?
        } else {
            DialogId::default()
        };
        let otherwise_relogin_days = if has_otherwise_relogin_days {
            parser.i32_()?
        } else {
            0
        };
        let (custom_type, title, description, url) = if type_ == SuggestedActionType::Custom {
            (
                parser.string()?,
                FormattedText::parse(parser)?,
                FormattedText::parse(parser)?,
                parser.string()?,
            )
        } else {
            (
                String::new(),
                FormattedText::default(),
                FormattedText::default(),
                String::new(),
            )
        };
        Ok(SuggestedAction {
            type_,
            dialog_id,
            otherwise_relogin_days,
            custom_type,
            title,
            description,
            url,
        })
    }
}

/// `get_update_suggested_actions_object` (SuggestedAction.cpp:243-251).
pub fn get_update_suggested_actions_object(
    user_manager: Option<&mut ()>,
    added_actions: &[SuggestedAction],
    removed_actions: &[SuggestedAction],
    source: &str,
    premium_manage_subscription_url: &str,
) -> td_api::Update {
    tdutils::log_info!("Get updateSuggestedActions from {source}");
    let _ = user_manager;
    let mut added_objects = Vec::with_capacity(added_actions.len());
    for action in added_actions {
        added_objects.push(
            action
                .get_suggested_action_object(None, premium_manage_subscription_url)
                .expect("a suggested action always converts"),
        );
    }
    let mut removed_objects = Vec::with_capacity(removed_actions.len());
    for action in removed_actions {
        removed_objects.push(
            action
                .get_suggested_action_object(None, premium_manage_subscription_url)
                .expect("a suggested action always converts"),
        );
    }
    td_api::Update::updateSuggestedActions(td_api::updateSuggestedActions {
        added_actions: added_objects,
        removed_actions: removed_objects,
    })
}

/// `update_suggested_actions` (SuggestedAction.cpp:253-277) — the sorted
/// diff walk; `true` when the list changed.
pub fn update_suggested_actions(
    suggested_actions: &mut Vec<SuggestedAction>,
    mut new_suggested_actions: Vec<SuggestedAction>,
) -> bool {
    new_suggested_actions.sort();
    new_suggested_actions.dedup();
    suggested_actions.sort();
    suggested_actions.dedup();
    if new_suggested_actions == *suggested_actions {
        return false;
    }
    *suggested_actions = new_suggested_actions;
    true
}

/// `remove_suggested_action` (SuggestedAction.cpp:279-283).
pub fn remove_suggested_action(
    suggested_actions: &mut Vec<SuggestedAction>,
    suggested_action: SuggestedAction,
) -> bool {
    let before = suggested_actions.len();
    suggested_actions.retain(|action| *action != suggested_action);
    suggested_actions.len() != before
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_str_forms_round_trip() {
        let action = SuggestedAction::from_action_str("PREMIUM_UPGRADE");
        assert_eq!(action.type_, SuggestedActionType::UpgradePremium);
        assert_eq!(action.get_suggested_action_str(), "PREMIUM_UPGRADE");

        let action = SuggestedAction::from_action_str("CONVERT_GIGAGROUP");
        assert!(action.is_empty(), "the dialog form needs the dialog ctor");

        let action =
            SuggestedAction::from_action_str_and_dialog("CONVERT_GIGAGROUP", DialogId::new(3));
        assert_eq!(action.type_, SuggestedActionType::ConvertToGigagroup);
        assert_eq!(action.dialog_id, DialogId::new(3));
    }

    #[test]
    fn persistence_excludes_the_login_email_pair() {
        assert!(SuggestedAction::from_action_str("PREMIUM_UPGRADE").is_persistent());
        assert!(!SuggestedAction::from_action_str("SETUP_LOGIN_EMAIL").is_persistent());
        assert!(!SuggestedAction::from_action_str("SETUP_LOGIN_EMAIL_NOSKIP").is_persistent());
    }

    #[test]
    fn td_api_conversion_covers_the_variants() {
        let action =
            SuggestedAction::from_td_api(&td_api::SuggestedAction::suggestedActionSetPassword(
                td_api::suggestedActionSetPassword {
                    authorization_delay: 14,
                },
            ));
        assert_eq!(action.type_, SuggestedActionType::SetPassword);
        assert_eq!(action.otherwise_relogin_days, 14);
        let object = action
            .get_suggested_action_object(None, "https://t.me/premiumbot")
            .unwrap();
        match object {
            td_api::SuggestedAction::suggestedActionSetPassword(object) => {
                assert_eq!(object.authorization_delay, 14);
            }
            other => panic!("unexpected object: {other:?}"),
        }
    }

    #[test]
    fn custom_actions_carry_the_strings() {
        let action = SuggestedAction::from_pending_suggestion(
            None,
            &telegram_api::PendingSuggestion {
                suggestion: "custom_name".to_string(),
                title: Some(telegram_api::TextWithEntities {
                    text: "t".to_string(),
                    entities: Vec::new(),
                }),
                description: Some(telegram_api::TextWithEntities {
                    text: "d".to_string(),
                    entities: Vec::new(),
                }),
                url: "https://u".to_string(),
            },
        );
        assert_eq!(action.type_, SuggestedActionType::Custom);
        assert_eq!(action.get_suggested_action_str(), "custom_name");
        assert_eq!(action.url, "https://u");
    }

    #[test]
    fn store_parse_round_trips() {
        for action in [
            SuggestedAction::from_action_str("PREMIUM_UPGRADE"),
            SuggestedAction::from_action_str_and_dialog("CONVERT_GIGAGROUP", DialogId::new(3)),
            SuggestedAction::from_pending_suggestion(
                None,
                &telegram_api::PendingSuggestion {
                    suggestion: "custom_name".to_string(),
                    title: Some(telegram_api::TextWithEntities {
                        text: "t".to_string(),
                        entities: Vec::new(),
                    }),
                    description: Some(telegram_api::TextWithEntities {
                        text: "d".to_string(),
                        entities: Vec::new(),
                    }),
                    url: "https://u".to_string(),
                },
            ),
        ] {
            let mut data = Vec::new();
            crate::store_parse::store_log_event_version(&mut data);
            action.store(&mut data);
            let mut parser = Parser::new_log_event(&data).unwrap();
            let parsed = SuggestedAction::parse(&mut parser).unwrap();
            parser.fetch_end().unwrap();
            assert_eq!(parsed, action);
        }
    }

    #[test]
    fn update_and_remove_helpers_work() {
        let mut actions = vec![
            SuggestedAction::from_action_str("PREMIUM_UPGRADE"),
            SuggestedAction::from_action_str("PREMIUM_RESTORE"),
        ];
        assert!(!update_suggested_actions(
            &mut actions,
            vec![
                SuggestedAction::from_action_str("PREMIUM_RESTORE"),
                SuggestedAction::from_action_str("PREMIUM_UPGRADE"),
            ],
        ));
        assert!(update_suggested_actions(
            &mut actions,
            vec![SuggestedAction::from_action_str("PREMIUM_UPGRADE")],
        ));
        assert_eq!(actions.len(), 1);
        assert!(remove_suggested_action(
            &mut actions,
            SuggestedAction::from_action_str("PREMIUM_UPGRADE")
        ));
        assert!(actions.is_empty());
        assert!(!remove_suggested_action(
            &mut actions,
            SuggestedAction::from_action_str("PREMIUM_UPGRADE")
        ));
    }
}
