//! `td::BusinessBotRights` (BusinessBotRights.h/.cpp/.hpp, M.48).
//!
//! The 14-flag business-bot rights record with the td_api/telegram_api
//! conversions and the byte-exact store/parse. The `legacy(can_reply)`
//! ctor matches the pre-business-connection form.

use crate::store_parse::{self, Parser};
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::BusinessBotRights` (BusinessBotRights.h:13-56).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BusinessBotRights {
    can_reply: bool,
    can_read_messages: bool,
    can_delete_sent_messages: bool,
    can_delete_received_messages: bool,
    can_edit_name: bool,
    can_edit_bio: bool,
    can_edit_profile_photo: bool,
    can_edit_username: bool,
    can_view_gifts: bool,
    can_sell_gifts: bool,
    can_change_gift_settings: bool,
    can_transfer_and_upgrade_gifts: bool,
    can_transfer_stars: bool,
    can_manage_stories: bool,
}

impl BusinessBotRights {
    /// `BusinessBotRights(businessBotRights)` (BusinessBotRights.cpp:10-27).
    pub fn from_telegram(
        bot_rights: Option<&telegram_api::BusinessBotRights>,
    ) -> BusinessBotRights {
        let mut result = BusinessBotRights::default();
        let Some(bot_rights) = bot_rights else {
            return result;
        };
        result.can_reply = bot_rights.reply;
        result.can_read_messages = bot_rights.read_messages;
        result.can_delete_sent_messages = bot_rights.delete_sent_messages;
        result.can_delete_received_messages = bot_rights.delete_received_messages;
        result.can_edit_name = bot_rights.edit_name;
        result.can_edit_bio = bot_rights.edit_bio;
        result.can_edit_profile_photo = bot_rights.edit_profile_photo;
        result.can_edit_username = bot_rights.edit_username;
        result.can_view_gifts = bot_rights.view_gifts;
        result.can_sell_gifts = bot_rights.sell_gifts;
        result.can_change_gift_settings = bot_rights.change_gift_settings;
        result.can_transfer_and_upgrade_gifts = bot_rights.transfer_and_upgrade_gifts;
        result.can_transfer_stars = bot_rights.transfer_stars;
        result.can_manage_stories = bot_rights.manage_stories;
        result
    }

    /// `BusinessBotRights(businessBotRights)` (BusinessBotRights.cpp:29-46) —
    /// the td_api `can_delete_all_messages` reads the
    /// `can_delete_received_messages` slot and `can_view_gifts_and_stars`
    /// the `can_view_gifts` slot.
    pub fn from_td_api(bot_rights: Option<&td_api::BusinessBotRights>) -> BusinessBotRights {
        let mut result = BusinessBotRights::default();
        let Some(bot_rights) = bot_rights else {
            return result;
        };
        result.can_reply = bot_rights.can_reply;
        result.can_read_messages = bot_rights.can_read_messages;
        result.can_delete_sent_messages = bot_rights.can_delete_sent_messages;
        result.can_delete_received_messages = bot_rights.can_delete_all_messages;
        result.can_edit_name = bot_rights.can_edit_name;
        result.can_edit_bio = bot_rights.can_edit_bio;
        result.can_edit_profile_photo = bot_rights.can_edit_profile_photo;
        result.can_edit_username = bot_rights.can_edit_username;
        result.can_view_gifts = bot_rights.can_view_gifts_and_stars;
        result.can_sell_gifts = bot_rights.can_sell_gifts;
        result.can_change_gift_settings = bot_rights.can_change_gift_settings;
        result.can_transfer_and_upgrade_gifts = bot_rights.can_transfer_and_upgrade_gifts;
        result.can_transfer_stars = bot_rights.can_transfer_stars;
        result.can_manage_stories = bot_rights.can_manage_stories;
        result
    }

    /// `BusinessBotRights::legacy` (BusinessBotRights.cpp:48-52).
    pub fn legacy(can_reply: bool) -> BusinessBotRights {
        BusinessBotRights {
            can_reply,
            ..BusinessBotRights::default()
        }
    }

    /// `get_business_bot_rights_object` (BusinessBotRights.cpp:54-59).
    pub fn get_business_bot_rights_object(self) -> td_api::BusinessBotRights {
        td_api::BusinessBotRights {
            can_reply: self.can_reply,
            can_read_messages: self.can_read_messages,
            can_delete_sent_messages: self.can_delete_sent_messages,
            can_delete_all_messages: self.can_delete_received_messages,
            can_edit_name: self.can_edit_name,
            can_edit_bio: self.can_edit_bio,
            can_edit_profile_photo: self.can_edit_profile_photo,
            can_edit_username: self.can_edit_username,
            can_view_gifts_and_stars: self.can_view_gifts,
            can_sell_gifts: self.can_sell_gifts,
            can_change_gift_settings: self.can_change_gift_settings,
            can_transfer_and_upgrade_gifts: self.can_transfer_and_upgrade_gifts,
            can_transfer_stars: self.can_transfer_stars,
            can_manage_stories: self.can_manage_stories,
        }
    }

    /// `get_input_business_bot_rights` (BusinessBotRights.cpp:61-66).
    pub fn get_input_business_bot_rights(self) -> telegram_api::BusinessBotRights {
        telegram_api::BusinessBotRights {
            flags: 0,
            reply: self.can_reply,
            read_messages: self.can_read_messages,
            delete_sent_messages: self.can_delete_sent_messages,
            delete_received_messages: self.can_delete_received_messages,
            edit_name: self.can_edit_name,
            edit_bio: self.can_edit_bio,
            edit_profile_photo: self.can_edit_profile_photo,
            edit_username: self.can_edit_username,
            view_gifts: self.can_view_gifts,
            sell_gifts: self.can_sell_gifts,
            change_gift_settings: self.can_change_gift_settings,
            transfer_and_upgrade_gifts: self.can_transfer_and_upgrade_gifts,
            transfer_stars: self.can_transfer_stars,
            manage_stories: self.can_manage_stories,
        }
    }

    /// `store` (BusinessBotRights.hpp:13-30).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_flags(
            out,
            [
                self.can_reply,
                self.can_read_messages,
                self.can_delete_sent_messages,
                self.can_delete_received_messages,
                self.can_edit_name,
                self.can_edit_bio,
                self.can_edit_profile_photo,
                self.can_edit_username,
                self.can_view_gifts,
                self.can_sell_gifts,
                self.can_change_gift_settings,
                self.can_transfer_and_upgrade_gifts,
                self.can_transfer_stars,
                self.can_manage_stories,
            ],
        );
    }

    /// `parse` (BusinessBotRights.hpp:31-48).
    pub fn parse(parser: &mut Parser) -> std::result::Result<BusinessBotRights, &'static str> {
        let [
            can_reply,
            can_read_messages,
            can_delete_sent_messages,
            can_delete_received_messages,
            can_edit_name,
            can_edit_bio,
            can_edit_profile_photo,
            can_edit_username,
            can_view_gifts,
            can_sell_gifts,
            can_change_gift_settings,
            can_transfer_and_upgrade_gifts,
            can_transfer_stars,
            can_manage_stories,
        ] = parser.parse_flags()?;
        Ok(BusinessBotRights {
            can_reply,
            can_read_messages,
            can_delete_sent_messages,
            can_delete_received_messages,
            can_edit_name,
            can_edit_bio,
            can_edit_profile_photo,
            can_edit_username,
            can_view_gifts,
            can_sell_gifts,
            can_change_gift_settings,
            can_transfer_and_upgrade_gifts,
            can_transfer_stars,
            can_manage_stories,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_parse_round_trip() {
        let value = BusinessBotRights {
            can_reply: true,
            can_read_messages: true,
            can_delete_sent_messages: false,
            can_delete_received_messages: true,
            can_edit_name: false,
            can_edit_bio: false,
            can_edit_profile_photo: true,
            can_edit_username: false,
            can_view_gifts: true,
            can_sell_gifts: false,
            can_change_gift_settings: false,
            can_transfer_and_upgrade_gifts: true,
            can_transfer_stars: false,
            can_manage_stories: true,
        };
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(BusinessBotRights::parse(&mut parser).unwrap(), value);
    }

    #[test]
    fn legacy_only_sets_can_reply() {
        let value = BusinessBotRights::legacy(true);
        assert!(value.can_reply);
        assert!(!value.can_read_messages);
        assert!(!value.can_transfer_stars);
    }

    #[test]
    fn td_api_conversion_renames_the_two_slots() {
        // BusinessBotRights.cpp:29-46.
        let value = BusinessBotRights::from_td_api(Some(&td_api::BusinessBotRights {
            can_reply: true,
            can_read_messages: false,
            can_delete_sent_messages: false,
            can_delete_all_messages: true,
            can_edit_name: false,
            can_edit_bio: false,
            can_edit_profile_photo: false,
            can_edit_username: false,
            can_view_gifts_and_stars: true,
            can_sell_gifts: false,
            can_change_gift_settings: false,
            can_transfer_and_upgrade_gifts: false,
            can_transfer_stars: false,
            can_manage_stories: false,
        }));
        assert!(value.can_delete_received_messages);
        assert!(value.can_view_gifts);
    }
}
