//! `td::BusinessConnectedBot` (BusinessConnectedBot.h/.cpp/.hpp, M.48).

use crate::business_bot_rights::BusinessBotRights;
use crate::business_manager::BusinessEnv;
use crate::business_recipients::BusinessRecipients;
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;

/// `td::BusinessConnectedBot` (BusinessConnectedBot.h:13-48).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusinessConnectedBot {
    user_id: UserId,
    recipients: BusinessRecipients,
    rights: BusinessBotRights,
    device: String,
    location: String,
    date: i32,
}

impl BusinessConnectedBot {
    /// `BusinessConnectedBot(connectedBot)` (BusinessConnectedBot.cpp:10-17).
    pub fn from_telegram(connected_bot: telegram_api::ConnectedBot) -> BusinessConnectedBot {
        BusinessConnectedBot {
            user_id: UserId::new(connected_bot.bot_id),
            recipients: BusinessRecipients::from_telegram_bot(
                connected_bot.recipients.unwrap_or_default(),
            ),
            rights: BusinessBotRights::from_telegram(connected_bot.rights.as_ref()),
            device: connected_bot.device,
            location: connected_bot.location,
            date: connected_bot.date,
        }
    }

    /// `BusinessConnectedBot(businessConnectedBot)`
    /// (BusinessConnectedBot.cpp:19-24).
    pub fn from_td_api(
        connected_bot: Option<td_api::BusinessConnectedBot>,
    ) -> BusinessConnectedBot {
        let mut result = BusinessConnectedBot::default();
        let Some(connected_bot) = connected_bot else {
            return result;
        };
        result.user_id = UserId::new(connected_bot.bot_user_id);
        result.recipients = BusinessRecipients::from_td_api(connected_bot.recipients, true);
        result.rights = BusinessBotRights::from_td_api(connected_bot.rights.as_ref());
        result
    }

    /// `BusinessConnectedBot::is_valid` (BusinessConnectedBot.h:23-25).
    pub fn is_valid(&self) -> bool {
        self.user_id.is_valid()
    }

    /// `BusinessConnectedBot::get_user_id` (BusinessConnectedBot.h:26-28).
    pub fn get_user_id(&self) -> UserId {
        self.user_id
    }

    /// `BusinessConnectedBot::get_recipients` (BusinessConnectedBot.h:29-31).
    pub fn get_recipients(&self) -> &BusinessRecipients {
        &self.recipients
    }

    /// `BusinessConnectedBot::get_rights` (BusinessConnectedBot.h:32-34).
    pub fn get_rights(&self) -> BusinessBotRights {
        self.rights
    }

    /// `get_business_connected_bot_object` (BusinessConnectedBot.cpp:26-30).
    pub fn get_business_connected_bot_object(
        &self,
        env: &mut impl BusinessEnv,
    ) -> td_api::BusinessConnectedBot {
        debug_assert!(self.is_valid());
        td_api::BusinessConnectedBot {
            bot_user_id: env.get_user_id_object(self.user_id, "businessConnectedBot"),
            recipients: Some(self.recipients.get_business_recipients_object(env)),
            rights: Some(self.rights.get_business_bot_rights_object()),
        }
    }

    /// `get_business_connected_bot_info_object` (BusinessConnectedBot.cpp:32-35).
    pub fn get_business_connected_bot_info_object(
        &self,
        env: &mut impl BusinessEnv,
    ) -> td_api::BusinessConnectedBotInfo {
        td_api::BusinessConnectedBotInfo {
            bot: Some(self.get_business_connected_bot_object(env)),
            connection_date: self.date,
            device_model: self.device.clone(),
            location: self.location.clone(),
        }
    }

    /// `store` (BusinessConnectedBot.hpp:14-22).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_device = !self.device.is_empty();
        let has_location = !self.location.is_empty();
        store_parse::store_flags(out, [has_device, has_location]);
        store_parse::store_i64(out, self.user_id.get());
        self.recipients.store(out);
        self.rights.store(out);
        if has_device {
            store_parse::store_string(out, &self.device);
        }
        if has_location {
            store_parse::store_string(out, &self.location);
        }
        store_parse::store_i32(out, self.date);
    }

    /// `parse` (BusinessConnectedBot.hpp:23-33).
    pub fn parse(parser: &mut Parser) -> std::result::Result<BusinessConnectedBot, &'static str> {
        let [has_device, has_location] = parser.parse_flags()?;
        let mut result = BusinessConnectedBot {
            user_id: UserId::new(parser.i64_()?),
            recipients: BusinessRecipients::parse(parser)?,
            rights: BusinessBotRights::parse(parser)?,
            ..BusinessConnectedBot::default()
        };
        if has_device {
            result.device = parser.string()?;
        }
        if has_location {
            result.location = parser.string()?;
        }
        result.date = parser.i32_()?;
        Ok(result)
    }
}

impl std::fmt::Display for BusinessConnectedBot {
    /// `operator<<` (BusinessConnectedBot.cpp:44-47).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "connected bot {} {} with {:?}",
            self.user_id.get(),
            self.recipients,
            self.rights
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_parse_round_trip() {
        let value = BusinessConnectedBot {
            user_id: UserId::new(11),
            recipients: BusinessRecipients::default(),
            rights: BusinessBotRights::legacy(true),
            device: "ThinkPad".to_owned(),
            location: "Berlin".to_owned(),
            date: 123,
        };
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(BusinessConnectedBot::parse(&mut parser).unwrap(), value);
    }

    #[test]
    fn default_bot_is_invalid() {
        assert!(!BusinessConnectedBot::default().is_valid());
    }
}
