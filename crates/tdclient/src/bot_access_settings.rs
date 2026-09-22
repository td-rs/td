//! Port of `td/telegram/BotAccessSettings.{h,cpp}` (Wave 3, M.45 — Bots I):
//! the managed-bot access settings value plane.

use crate::bot_info_manager::BotInfoManagerEnv;
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;

/// `td::BotAccessSettings` (BotAccessSettings.h:19-37).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BotAccessSettings {
    is_restricted: bool,
    added_user_ids: Vec<UserId>,
}

impl BotAccessSettings {
    /// `BotAccessSettings::BotAccessSettings(Td *, object_ptr<
    /// bots_accessSettings> &&)` (BotAccessSettings.cpp:16-29).
    pub fn from_telegram(
        env: &mut impl BotInfoManagerEnv,
        settings: telegram_api::bots_AccessSettings,
    ) -> BotAccessSettings {
        let mut result = BotAccessSettings {
            is_restricted: settings.restricted,
            ..Default::default()
        };
        if result.is_restricted {
            for user in settings.add_users {
                let user_id = env.on_get_user(user, "BotAccessSettings");
                if user_id.is_valid() {
                    result.added_user_ids.push(user_id);
                }
            }
        } else if !settings.add_users.is_empty() {
            tdutils::log_error!("Receive added users in a non-restricted bot");
        }
        result
    }

    /// `BotAccessSettings::BotAccessSettings(object_ptr<
    /// botAccessSettings> &&)` (BotAccessSettings.cpp:31-41).
    pub fn from_td_api(settings: Option<td_api::BotAccessSettings>) -> BotAccessSettings {
        let mut result = BotAccessSettings::default();
        if let Some(settings) = settings {
            result.is_restricted = settings.is_restricted;
            for added_user_id in settings.added_user_ids {
                let user_id = UserId::new(added_user_id);
                if user_id.is_valid() {
                    result.added_user_ids.push(user_id);
                }
            }
        }
        result
    }

    /// `BotAccessSettings::get_bot_access_settings_object`
    /// (BotAccessSettings.cpp:43-46).
    pub fn get_bot_access_settings_object(
        &self,
        env: &mut impl BotInfoManagerEnv,
    ) -> td_api::BotAccessSettings {
        td_api::BotAccessSettings {
            is_restricted: self.is_restricted,
            added_user_ids: env.get_user_ids_object(&self.added_user_ids, "botAccessSettings"),
        }
    }

    /// `BotAccessSettings::is_restricted` (BotAccessSettings.h:30-32).
    pub fn is_restricted(&self) -> bool {
        self.is_restricted
    }

    /// `BotAccessSettings::get_added_user_ids`
    /// (BotAccessSettings.h:34-36).
    pub fn get_added_user_ids(&self) -> &[UserId] {
        &self.added_user_ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot_info_manager::test_env::{self, TestBotEnv};
    use crate::global::Global;
    use crate::option_manager::OptionManager;
    use crate::user_manager::UserManager;

    fn env_pair() -> (Global, OptionManager, UserManager) {
        (Global::new(), OptionManager::new(false), UserManager::new())
    }

    #[test]
    fn access_settings_round_trip_through_the_object() {
        let (mut global, mut options, mut user_manager) = env_pair();
        let mut env: TestBotEnv =
            test_env::fixture(&mut global, Some(&mut options), &mut user_manager, true, 1);

        // the td_api half filters the invalid ids
        let settings = BotAccessSettings::from_td_api(Some(td_api::BotAccessSettings {
            is_restricted: true,
            added_user_ids: vec![1, 0, 2],
        }));
        assert!(settings.is_restricted());
        assert_eq!(
            settings.get_added_user_ids(),
            &[UserId::new(1), UserId::new(2)]
        );

        let object = settings.get_bot_access_settings_object(&mut env);
        assert_eq!(
            object,
            td_api::BotAccessSettings {
                is_restricted: true,
                added_user_ids: vec![1, 2],
            }
        );

        // the null input is the default (BotAccessSettings.cpp:31-41)
        assert!(!BotAccessSettings::from_td_api(None).is_restricted());
        assert!(
            BotAccessSettings::from_td_api(None)
                .get_added_user_ids()
                .is_empty()
        );
    }

    #[test]
    fn access_settings_from_telegram_intakes_users() {
        let (mut global, mut options, mut user_manager) = env_pair();
        let mut env: TestBotEnv =
            test_env::fixture(&mut global, Some(&mut options), &mut user_manager, true, 1);

        let settings = BotAccessSettings::from_telegram(
            &mut env,
            telegram_api::bots_AccessSettings {
                flags: 1,
                restricted: true,
                add_users: vec![make_user(9), make_user(10)],
            },
        );
        assert!(settings.is_restricted());
        assert_eq!(
            settings.get_added_user_ids(),
            &[UserId::new(9), UserId::new(10)]
        );
        assert_eq!(env.received_users.len(), 2);

        // the non-restricted variant with added users logs and drops them
        let settings = BotAccessSettings::from_telegram(
            &mut env,
            telegram_api::bots_AccessSettings {
                flags: 0,
                restricted: false,
                add_users: vec![make_user(11)],
            },
        );
        assert!(!settings.is_restricted());
        assert!(settings.get_added_user_ids().is_empty());
    }

    fn make_user(id: i64) -> telegram_api::User {
        telegram_api::User::userEmpty(telegram_api::userEmpty { id })
    }
}
