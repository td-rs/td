//! Port of `td/telegram/BotVerification.{h,cpp,hpp}` (Wave 3, M.45 —
//! Bots I): the message-sender bot verification value plane with the
//! byte-exact store/parse.
//!
//! Seams: `BotVerification::add_dependencies(Dependencies &)`
//! (BotVerification.cpp:52-54) is [blocked: the Dependencies machinery] —
//! the dependency registry has not landed; the method joins it with the
//! user/chat dependency steps. The `get_bot_verification_object` entity
//! conversion passes the `()` [`crate::message_entity::UserManagerLike`]
//! stand-in like the synchronous surfaces (the custom-emoji resolver rides
//! the entity step).

use crate::emoji_status::CustomEmojiId;
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;

/// `td::BotVerification` (BotVerification.h:22-53).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BotVerification {
    bot_user_id: UserId,
    icon: CustomEmojiId,
    description: String,
}

impl BotVerification {
    /// `BotVerification::BotVerification(object_ptr<botVerification> &&)`
    /// (BotVerification.cpp:18-25): the null input stays the default value.
    pub fn from_telegram(
        bot_verification: Option<telegram_api::BotVerification>,
    ) -> BotVerification {
        let mut result = BotVerification::default();
        let Some(bot_verification) = bot_verification else {
            return result;
        };
        result.bot_user_id = UserId::new(bot_verification.bot_id);
        result.icon = CustomEmojiId::new(bot_verification.icon);
        result.description = bot_verification.description;
        result
    }

    /// `BotVerification::get_bot_verification`
    /// (BotVerification.cpp:27-38): `None` is the C++ `nullptr`.
    pub fn get_bot_verification(
        bot_verification: Option<telegram_api::BotVerification>,
    ) -> Option<BotVerification> {
        let result = BotVerification::from_telegram(bot_verification);
        if !result.is_valid() {
            tdutils::log_error!("Receive invalid {result}");
            return None;
        }
        Some(result)
    }

    /// `BotVerification::get_bot_verification_object`
    /// (BotVerification.cpp:40-50): `None` is the C++ `nullptr`.
    pub fn get_bot_verification_object(
        &self,
        env: &mut impl crate::bot_info_manager::BotInfoManagerEnv,
    ) -> Option<td_api::BotVerification> {
        if !self.is_valid() {
            return None;
        }
        let mut text = crate::message_entity::FormattedText::default();
        text.text = self.description.clone();
        text.entities = crate::message_entity::find_entities(&text.text, true, true);
        let description =
            crate::message_entity::get_formatted_text_object::<()>(None, &text, true, -1);
        Some(td_api::BotVerification {
            bot_user_id: env.get_user_id_object(self.bot_user_id, "botVerification"),
            icon_custom_emoji_id: self.icon.get(),
            custom_description: Some(description),
        })
    }

    /// `BotVerification::is_valid` (BotVerification.h:33-35).
    pub fn is_valid(&self) -> bool {
        self.bot_user_id.is_valid() && self.icon.is_valid()
    }

    /// `BotVerification::store` (BotVerification.hpp:15-22).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags::<0>(out, []);
        store_parse::store_i64(out, self.bot_user_id.get());
        store_parse::store_i64(out, self.icon.get());
        store_parse::store_string(out, &self.description);
    }

    /// `BotVerification::parse` (BotVerification.hpp:24-31).
    pub fn parse(parser: &mut Parser) -> Result<BotVerification, &'static str> {
        let mut result = BotVerification::default();
        parser.parse_flags::<0>()?;
        result.bot_user_id = UserId::new(parser.i64_()?);
        result.icon = CustomEmojiId::new(parser.i64_()?);
        result.description = parser.string()?;
        Result::Ok(result)
    }
}

/// `operator<<(StringBuilder &, const BotVerification &)`
/// (BotVerification.cpp:60-63).
impl std::fmt::Display for BotVerification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "verified by {} with {} and {}",
            self.bot_user_id.get(),
            self.icon.get(),
            self.description
        )
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

    fn sample(icon: i64) -> telegram_api::BotVerification {
        telegram_api::BotVerification {
            bot_id: 42,
            icon,
            description: "Visit https://t.me/example and https://example.org".to_owned(),
        }
    }

    #[test]
    fn bot_verification_validity_and_getter() {
        // the icon-less verification is invalid (BotVerification.cpp:27-38)
        assert!(BotVerification::get_bot_verification(Some(sample(0))).is_none());
        // the bot-less verification is invalid
        let mut invalid = sample(7);
        invalid.bot_id = 0;
        assert!(BotVerification::get_bot_verification(Some(invalid)).is_none());
        // the null is the null
        assert!(BotVerification::get_bot_verification(None).is_none());

        let verification = BotVerification::get_bot_verification(Some(sample(7))).expect("valid");
        assert_eq!(
            verification,
            BotVerification {
                bot_user_id: UserId::new(42),
                icon: CustomEmojiId::new(7),
                description: "Visit https://t.me/example and https://example.org".to_owned(),
            }
        );
        assert_eq!(
            format!("{verification}"),
            "verified by 42 with 7 and Visit https://t.me/example and https://example.org"
        );
    }

    #[test]
    fn bot_verification_object_builds_the_description_text() {
        let (mut global, mut options, mut user_manager) = env_pair();
        let mut env: TestBotEnv =
            test_env::fixture(&mut global, Some(&mut options), &mut user_manager, false, 1);

        let verification = BotVerification::get_bot_verification(Some(sample(7))).expect("valid");
        let object = verification
            .get_bot_verification_object(&mut env)
            .expect("object");
        assert_eq!(object.bot_user_id, 42);
        assert_eq!(object.icon_custom_emoji_id, 7);
        let description = object.custom_description.expect("description");
        assert!(description.text.contains("https://t.me/example"));
        assert!(!description.entities.is_empty(), "urls are extracted");

        // the invalid verification converts to the C++ nullptr
        assert!(
            BotVerification::default()
                .get_bot_verification_object(&mut env)
                .is_none()
        );
    }

    #[test]
    fn bot_verification_store_parse_round_trip() {
        let verification = BotVerification::from_telegram(Some(sample(1 << 40)));
        let mut out = Vec::new();
        store_parse::store_log_event_version(&mut out);
        verification.store(&mut out);
        let mut parser = Parser::new_log_event(&out).expect("framed");
        assert_eq!(
            BotVerification::parse(&mut parser).expect("parse"),
            verification
        );
    }
}
