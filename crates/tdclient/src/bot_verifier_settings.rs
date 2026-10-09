//! Port of `td/telegram/BotVerifierSettings.{h,cpp,hpp}` (Wave 3, M.45 —
//! Bots I): the bot verifier settings value plane with the byte-exact
//! store/parse.
//!
//! Seams: the `get_bot_verification_parameters_object` entity conversion
//! passes the `()` [`crate::message_entity::UserManagerLike`] stand-in
//! like the synchronous surfaces.

use crate::emoji_status::CustomEmojiId;
use crate::store_parse::{self, Parser};
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::BotVerifierSettings` (BotVerifierSettings.h:20-50).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BotVerifierSettings {
    icon: CustomEmojiId,
    company: String,
    description: crate::message_entity::FormattedText,
    can_modify_custom_description: bool,
}

impl BotVerifierSettings {
    /// `BotVerifierSettings::BotVerifierSettings(object_ptr<
    /// botVerifierSettings> &&)` (BotVerifierSettings.cpp:16-25): the null
    /// input stays the default value.
    pub fn from_telegram(
        bot_verifier_settings: Option<telegram_api::BotVerifierSettings>,
    ) -> BotVerifierSettings {
        let mut result = BotVerifierSettings::default();
        let Some(bot_verifier_settings) = bot_verifier_settings else {
            return result;
        };
        result.icon = CustomEmojiId::new(bot_verifier_settings.icon);
        result.company = bot_verifier_settings.company;
        result.description = crate::message_entity::get_formatted_text_of_text_with_entities::<()>(
            None,
            bot_verifier_settings.custom_description,
            true,
            false,
            "BotVerifierSettings",
        );
        result.can_modify_custom_description = bot_verifier_settings.can_modify_custom_description;
        result
    }

    /// `BotVerifierSettings::get_bot_verifier_settings`
    /// (BotVerifierSettings.cpp:27-38): `None` is the C++ `nullptr`.
    pub fn get_bot_verifier_settings(
        bot_verifier_settings: Option<telegram_api::BotVerifierSettings>,
    ) -> Option<BotVerifierSettings> {
        let result = BotVerifierSettings::from_telegram(bot_verifier_settings);
        if !result.is_valid() {
            tdutils::log_error!("Receive invalid {result}");
            return None;
        }
        Some(result)
    }

    /// `BotVerifierSettings::get_bot_verification_parameters_object`
    /// (BotVerifierSettings.cpp:40-54): `None` is the C++ `nullptr`.
    pub fn get_bot_verification_parameters_object(
        &self,
    ) -> Option<td_api::BotVerificationParameters> {
        if !self.is_valid() {
            return None;
        }
        let mut description: Option<td_api::FormattedText> = None;
        if !self.description.text.is_empty() || self.can_modify_custom_description {
            description = Some(crate::message_entity::get_formatted_text_object::<()>(
                None,
                &self.description,
                true,
                -1,
            ));
        }
        Some(td_api::BotVerificationParameters {
            icon_custom_emoji_id: self.icon.get(),
            organization_name: self.company.clone(),
            default_custom_description: description,
            can_set_custom_description: self.can_modify_custom_description,
        })
    }

    /// `BotVerifierSettings::is_valid` (BotVerifierSettings.h:31-33).
    pub fn is_valid(&self) -> bool {
        self.icon.is_valid()
    }

    /// `BotVerifierSettings::store` (BotVerifierSettings.hpp:15-31): the
    /// description presence flags gate the optional text and entities.
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_description = !self.description.text.is_empty();
        let has_description_entities = !self.description.entities.is_empty();
        store_parse::store_flags(
            out,
            [
                has_description,
                self.can_modify_custom_description,
                has_description_entities,
            ],
        );
        store_parse::store_i64(out, self.icon.get());
        store_parse::store_string(out, &self.company);
        if has_description {
            store_parse::store_string(out, &self.description.text);
        }
        if has_description_entities {
            for entity in &self.description.entities {
                entity.store(out);
            }
        }
    }

    /// `BotVerifierSettings::parse` (BotVerifierSettings.hpp:33-49).
    pub fn parse(parser: &mut Parser) -> Result<BotVerifierSettings, &'static str> {
        let mut result = BotVerifierSettings::default();
        let [
            has_description,
            can_modify_custom_description,
            has_description_entities,
        ] = parser.parse_flags()?;
        result.can_modify_custom_description = can_modify_custom_description;
        result.icon = CustomEmojiId::new(parser.i64_()?);
        result.company = parser.string()?;
        if has_description {
            result.description.text = parser.string()?;
        }
        if has_description_entities {
            let len = parser.i32_()?;
            if len < 0 || parser.left_len() < len as usize {
                return Err("Wrong vector length");
            }
            for _ in 0..len {
                result
                    .description
                    .entities
                    .push(crate::message_entity::MessageEntity::parse(parser)?);
            }
        }
        Result::Ok(result)
    }
}

/// `operator<<(StringBuilder &, const BotVerifierSettings &)`
/// (BotVerifierSettings.cpp:61-64).
impl std::fmt::Display for BotVerifierSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "VerificationSettings[{} by {}]",
            self.icon.get(),
            self.company
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(icon: i64) -> telegram_api::BotVerifierSettings {
        telegram_api::BotVerifierSettings {
            flags: 0,
            can_modify_custom_description: true,
            icon,
            company: "ACME".to_owned(),
            custom_description: Some(tdmtproto::telegram_api::TextWithEntities {
                text: "We verify things".to_owned(),
                entities: Vec::new(),
            }),
        }
    }

    #[test]
    fn verifier_settings_validity_and_getter() {
        // the icon-less settings are invalid
        assert!(BotVerifierSettings::get_bot_verifier_settings(Some(sample(0))).is_none());
        // the null is the null
        assert!(BotVerifierSettings::get_bot_verifier_settings(None).is_none());

        let settings =
            BotVerifierSettings::get_bot_verifier_settings(Some(sample(9))).expect("valid");
        assert_eq!(format!("{settings}"), "VerificationSettings[9 by ACME]");
    }

    #[test]
    fn verifier_settings_build_verification_parameters() {
        let settings =
            BotVerifierSettings::get_bot_verifier_settings(Some(sample(9))).expect("valid");
        let object = settings
            .get_bot_verification_parameters_object()
            .expect("object");
        assert_eq!(object.icon_custom_emoji_id, 9);
        assert_eq!(object.organization_name, "ACME");
        assert!(object.can_set_custom_description);
        let description = object.default_custom_description.expect("description");
        assert_eq!(description.text, "We verify things");

        // an empty description with the modify flag still builds the
        // (empty-text) description object (BotVerifierSettings.cpp:45-51)
        let mut bare = sample(9);
        bare.custom_description = None;
        let settings = BotVerifierSettings::get_bot_verifier_settings(Some(bare)).expect("valid");
        let object = settings
            .get_bot_verification_parameters_object()
            .expect("object");
        let description = object
            .default_custom_description
            .expect("empty-text description");
        assert_eq!(description.text, "");
        assert!(object.can_set_custom_description);

        // without the modify flag the description stays null
        let mut bare = sample(9);
        bare.custom_description = None;
        bare.can_modify_custom_description = false;
        let settings = BotVerifierSettings::get_bot_verifier_settings(Some(bare)).expect("valid");
        let object = settings
            .get_bot_verification_parameters_object()
            .expect("object");
        assert!(object.default_custom_description.is_none());
        assert!(!object.can_set_custom_description);
    }

    #[test]
    fn verifier_settings_store_parse_round_trip() {
        // both description-presence halves
        for custom_description in [Some("We verify things".to_owned()), None] {
            let mut raw = sample(1 << 40);
            raw.custom_description =
                custom_description.map(|text| tdmtproto::telegram_api::TextWithEntities {
                    text,
                    entities: Vec::new(),
                });
            let settings = BotVerifierSettings::from_telegram(Some(raw));
            let mut out = Vec::new();
            store_parse::store_log_event_version(&mut out);
            settings.store(&mut out);
            let mut parser = Parser::new_log_event(&out).expect("framed");
            assert_eq!(
                BotVerifierSettings::parse(&mut parser).expect("parse"),
                settings
            );
        }
    }
}
