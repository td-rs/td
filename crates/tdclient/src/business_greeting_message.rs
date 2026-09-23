//! `td::BusinessGreetingMessage` (BusinessGreetingMessage.h/.cpp/.hpp, M.48).

use crate::business_manager::BusinessEnv;
use crate::business_recipients::BusinessRecipients;
use crate::files::QuickReplyShortcutId;
use crate::store_parse::{self, Parser};
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::BusinessGreetingMessage` (BusinessGreetingMessage.h:14-49).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusinessGreetingMessage {
    shortcut_id: QuickReplyShortcutId,
    recipients: BusinessRecipients,
    inactivity_days: i32,
}

impl BusinessGreetingMessage {
    /// `BusinessGreetingMessage(businessGreetingMessage)`
    /// (BusinessGreetingMessage.cpp:10-16) — the no-activity days clamp to
    /// a whole week in [7, 28].
    pub fn from_telegram(
        greeting_message: Option<telegram_api::BusinessGreetingMessage>,
    ) -> BusinessGreetingMessage {
        let mut result = BusinessGreetingMessage::default();
        let Some(greeting_message) = greeting_message else {
            return result;
        };
        result.shortcut_id = QuickReplyShortcutId::new(greeting_message.shortcut_id);
        result.recipients =
            BusinessRecipients::from_telegram(greeting_message.recipients.unwrap_or_default());
        result.inactivity_days = (greeting_message.no_activity_days / 7 * 7).clamp(7, 28);
        result
    }

    /// `BusinessGreetingMessage(businessGreetingMessageSettings)`
    /// (BusinessGreetingMessage.cpp:18-29) — the td_api ctor rejects
    /// invalid inactivity days (not a whole week, or out of [7, 28]).
    pub fn from_td_api(
        greeting_message: Option<td_api::BusinessGreetingMessageSettings>,
    ) -> BusinessGreetingMessage {
        let mut result = BusinessGreetingMessage::default();
        let Some(greeting_message) = greeting_message else {
            return result;
        };
        let inactivity_days = greeting_message.inactivity_days;
        if !(7..=28).contains(&inactivity_days) || inactivity_days % 7 != 0 {
            return result;
        }
        result.shortcut_id = QuickReplyShortcutId::new(greeting_message.shortcut_id);
        result.recipients = BusinessRecipients::from_td_api(greeting_message.recipients, false);
        result.inactivity_days = inactivity_days;
        result
    }

    /// `get_business_greeting_message_settings_object`
    /// (BusinessGreetingMessage.cpp:31-37); `None` for the C++ nullptr.
    pub fn get_business_greeting_message_settings_object(
        &self,
        env: &mut impl BusinessEnv,
    ) -> Option<td_api::BusinessGreetingMessageSettings> {
        if self.is_empty() {
            return None;
        }
        Some(td_api::BusinessGreetingMessageSettings {
            shortcut_id: self.shortcut_id.get(),
            recipients: Some(self.recipients.get_business_recipients_object(env)),
            inactivity_days: self.inactivity_days,
        })
    }

    /// `get_input_business_greeting_message` (BusinessGreetingMessage.cpp:39-42).
    pub fn get_input_business_greeting_message(
        &self,
        env: &mut impl BusinessEnv,
    ) -> telegram_api::InputBusinessGreetingMessage {
        telegram_api::InputBusinessGreetingMessage {
            shortcut_id: self.shortcut_id.get(),
            recipients: Some(self.recipients.get_input_business_recipients(env)),
            no_activity_days: self.inactivity_days,
        }
    }

    /// `BusinessGreetingMessage::is_empty` (BusinessGreetingMessage.h:32-34).
    pub fn is_empty(&self) -> bool {
        !self.is_valid()
    }

    /// `BusinessGreetingMessage::is_valid` (BusinessGreetingMessage.h:36-38)
    /// — `QuickReplyShortcutId::is_server`, inlined (the files.rs stand-in
    /// carries `new`/`get`).
    pub fn is_valid(&self) -> bool {
        self.shortcut_id.get() > 0
    }

    /// `add_dependencies` (BusinessGreetingMessage.cpp:44-46).
    pub fn add_dependencies(&self, env: &mut impl BusinessEnv) {
        self.recipients.add_dependencies(env);
    }

    /// `store` (BusinessGreetingMessage.hpp:13-18).
    pub fn store(&self, out: &mut Vec<u8>) {
        store_parse::store_flags(out, []);
        store_parse::store_i32(out, self.shortcut_id.get());
        self.recipients.store(out);
        store_parse::store_i32(out, self.inactivity_days);
    }

    /// `parse` (BusinessGreetingMessage.hpp:19-24).
    pub fn parse(
        parser: &mut Parser,
    ) -> std::result::Result<BusinessGreetingMessage, &'static str> {
        parser.parse_flags::<0>()?;
        Ok(BusinessGreetingMessage {
            shortcut_id: QuickReplyShortcutId::new(parser.i32_()?),
            recipients: BusinessRecipients::parse(parser)?,
            inactivity_days: parser.i32_()?,
        })
    }
}

impl std::fmt::Display for BusinessGreetingMessage {
    /// `operator<<` (BusinessGreetingMessage.cpp:56-60).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "greeting message {} {} after {} inactivity days",
            self.shortcut_id.get(),
            self.recipients,
            self.inactivity_days
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn td_api_ctor_rejects_invalid_inactivity_days() {
        // BusinessGreetingMessage.cpp:23-26.
        let value =
            BusinessGreetingMessage::from_td_api(Some(td_api::BusinessGreetingMessageSettings {
                shortcut_id: 10,
                recipients: None,
                inactivity_days: 6,
            }));
        assert!(value.is_empty());

        let value =
            BusinessGreetingMessage::from_td_api(Some(td_api::BusinessGreetingMessageSettings {
                shortcut_id: 10,
                recipients: None,
                inactivity_days: 14,
            }));
        assert!(!value.is_empty());
        assert_eq!(value.inactivity_days, 14);
    }

    #[test]
    fn telegram_ctor_clamps_no_activity_days_to_whole_weeks() {
        // BusinessGreetingMessage.cpp:15.
        let value =
            BusinessGreetingMessage::from_telegram(Some(telegram_api::BusinessGreetingMessage {
                shortcut_id: 7,
                recipients: Some(telegram_api::BusinessRecipients {
                    flags: 0,
                    existing_chats: false,
                    new_chats: false,
                    contacts: false,
                    non_contacts: false,
                    exclude_selected: false,
                    users: vec![],
                }),
                no_activity_days: 20,
            }));
        assert_eq!(value.inactivity_days, 14);
    }

    #[test]
    fn store_parse_round_trip() {
        let value = BusinessGreetingMessage {
            shortcut_id: QuickReplyShortcutId::new(5),
            recipients: BusinessRecipients::default(),
            inactivity_days: 21,
        };
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(BusinessGreetingMessage::parse(&mut parser).unwrap(), value);
    }
}
