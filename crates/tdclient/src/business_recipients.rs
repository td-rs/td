//! `td::BusinessRecipients` (BusinessRecipients.h/.cpp/.hpp, M.48).
//!
//! The business-message recipient selection: the user-id sets with the
//! seven-flag store/parse (existing/new/contacts/non-contacts/
//! exclude-selected + the two has-vector flags), the td_api/telegram_api
//! conversions and the dialog force-create rides through the env seam.

use crate::business_manager::BusinessEnv;
use crate::dialog_id::DialogId;
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;

/// The `inputBusinessRecipients.users` bit (telegram_api.tl:1616).
const INPUT_BUSINESS_RECIPIENTS_USERS_MASK: i32 = 1 << 4;
/// The `inputBusinessBotRecipients.exclude_users` bit (telegram_api.tl:1617).
const INPUT_BUSINESS_BOT_RECIPIENTS_EXCLUDE_USERS_MASK: i32 = 1 << 6;

/// `UserId::get_user_ids` (UserId.cpp:13-23) — the valid-id filter.
fn get_user_ids(input_user_ids: &[i64]) -> Vec<UserId> {
    input_user_ids
        .iter()
        .copied()
        .map(UserId::new)
        .filter(|user_id| user_id.is_valid())
        .collect()
}

/// `td::BusinessRecipients` (BusinessRecipients.h:17-54).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusinessRecipients {
    user_ids: Vec<UserId>,
    excluded_user_ids: Vec<UserId>,
    existing_chats: bool,
    new_chats: bool,
    contacts: bool,
    non_contacts: bool,
    exclude_selected: bool,
}

impl BusinessRecipients {
    /// `BusinessRecipients(businessRecipients)` (BusinessRecipients.cpp:11-19).
    pub fn from_telegram(recipients: telegram_api::BusinessRecipients) -> BusinessRecipients {
        BusinessRecipients {
            user_ids: get_user_ids(&recipients.users),
            excluded_user_ids: Vec::new(),
            existing_chats: recipients.existing_chats,
            new_chats: recipients.new_chats,
            contacts: recipients.contacts,
            non_contacts: recipients.non_contacts,
            exclude_selected: recipients.exclude_selected,
        }
    }

    /// `BusinessRecipients(businessBotRecipients)` (BusinessRecipients.cpp:21-30).
    pub fn from_telegram_bot(
        recipients: telegram_api::BusinessBotRecipients,
    ) -> BusinessRecipients {
        BusinessRecipients {
            user_ids: get_user_ids(&recipients.users),
            excluded_user_ids: get_user_ids(&recipients.exclude_users),
            existing_chats: recipients.existing_chats,
            new_chats: recipients.new_chats,
            contacts: recipients.contacts,
            non_contacts: recipients.non_contacts,
            exclude_selected: recipients.exclude_selected,
        }
    }

    /// `BusinessRecipients(businessRecipients, allow_excluded)`
    /// (BusinessRecipients.cpp:32-57).
    pub fn from_td_api(
        recipients: Option<td_api::BusinessRecipients>,
        allow_excluded: bool,
    ) -> BusinessRecipients {
        let mut result = BusinessRecipients::default();
        let Some(recipients) = recipients else {
            return result;
        };
        for chat_id in recipients.chat_ids {
            let dialog_id = DialogId::new(chat_id);
            if dialog_id.get_type() == crate::dialog_id::DialogType::User {
                result.user_ids.push(dialog_id.get_user_id());
            }
        }
        if allow_excluded {
            for chat_id in recipients.excluded_chat_ids {
                let dialog_id = DialogId::new(chat_id);
                if dialog_id.get_type() == crate::dialog_id::DialogType::User {
                    result.excluded_user_ids.push(dialog_id.get_user_id());
                }
            }
            if recipients.exclude_selected {
                result.user_ids.append(&mut result.excluded_user_ids);
            }
        }
        result.existing_chats = recipients.select_existing_chats;
        result.new_chats = recipients.select_new_chats;
        result.contacts = recipients.select_contacts;
        result.non_contacts = recipients.select_non_contacts;
        result.exclude_selected = recipients.exclude_selected;
        result
    }

    /// `get_business_recipients_object` (BusinessRecipients.cpp:59-83).
    pub fn get_business_recipients_object(
        &self,
        env: &mut impl BusinessEnv,
    ) -> td_api::BusinessRecipients {
        let mut chat_ids = Vec::new();
        for &user_id in &self.user_ids {
            let dialog_id = DialogId::from_user_id(user_id);
            env.force_create_dialog(dialog_id, "get_business_recipients_object 1", true);
            chat_ids.push(env.get_chat_id_object(dialog_id, "businessRecipients 1"));
        }
        let mut excluded_chat_ids = Vec::new();
        for &user_id in &self.excluded_user_ids {
            let dialog_id = DialogId::from_user_id(user_id);
            env.force_create_dialog(dialog_id, "get_business_recipients_object 2", true);
            excluded_chat_ids.push(env.get_chat_id_object(dialog_id, "businessRecipients 2"));
        }
        td_api::BusinessRecipients {
            chat_ids,
            excluded_chat_ids,
            select_existing_chats: self.existing_chats,
            select_new_chats: self.new_chats,
            select_contacts: self.contacts,
            select_non_contacts: self.non_contacts,
            exclude_selected: self.exclude_selected,
        }
    }

    /// `get_input_business_recipients` (BusinessRecipients.cpp:85-93).
    pub fn get_input_business_recipients(
        &self,
        env: &mut impl BusinessEnv,
    ) -> telegram_api::InputBusinessRecipients {
        let input_users = env.get_input_users_force(&self.user_ids);
        let users_mask = !input_users.is_empty();
        telegram_api::InputBusinessRecipients {
            flags: if users_mask {
                INPUT_BUSINESS_RECIPIENTS_USERS_MASK
            } else {
                0
            },
            existing_chats: self.existing_chats,
            new_chats: self.new_chats,
            contacts: self.contacts,
            non_contacts: self.non_contacts,
            exclude_selected: self.exclude_selected,
            users: input_users,
        }
    }

    /// `get_input_business_bot_recipients` (BusinessRecipients.cpp:95-108).
    pub fn get_input_business_bot_recipients(
        &self,
        env: &mut impl BusinessEnv,
    ) -> telegram_api::InputBusinessBotRecipients {
        let input_users = env.get_input_users_force(&self.user_ids);
        let users_mask = !input_users.is_empty();
        let excluded_input_users = env.get_input_users_force(&self.excluded_user_ids);
        let exclude_users_mask = !excluded_input_users.is_empty();
        telegram_api::InputBusinessBotRecipients {
            flags: (if users_mask {
                INPUT_BUSINESS_RECIPIENTS_USERS_MASK
            } else {
                0
            }) | (if exclude_users_mask {
                INPUT_BUSINESS_BOT_RECIPIENTS_EXCLUDE_USERS_MASK
            } else {
                0
            }),
            existing_chats: self.existing_chats,
            new_chats: self.new_chats,
            contacts: self.contacts,
            non_contacts: self.non_contacts,
            exclude_selected: self.exclude_selected,
            users: input_users,
            exclude_users: excluded_input_users,
        }
    }

    /// `add_dependencies` (BusinessRecipients.cpp:110-117) — the
    /// force-create seam for the user sets.
    pub fn add_dependencies(&self, env: &mut impl BusinessEnv) {
        for &user_id in self.user_ids.iter().chain(self.excluded_user_ids.iter()) {
            env.force_create_dialog(DialogId::from_user_id(user_id), "BusinessRecipients", true);
        }
    }

    /// `store` (BusinessRecipients.hpp:13-30).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_user_ids = !self.user_ids.is_empty();
        let has_excluded_user_ids = !self.excluded_user_ids.is_empty();
        store_parse::store_flags(
            out,
            [
                self.existing_chats,
                self.new_chats,
                self.contacts,
                self.non_contacts,
                self.exclude_selected,
                has_user_ids,
                has_excluded_user_ids,
            ],
        );
        if has_user_ids {
            store_parse::store_i32(out, self.user_ids.len() as i32);
            for user_id in &self.user_ids {
                store_parse::store_i64(out, user_id.get());
            }
        }
        if has_excluded_user_ids {
            store_parse::store_i32(out, self.excluded_user_ids.len() as i32);
            for user_id in &self.excluded_user_ids {
                store_parse::store_i64(out, user_id.get());
            }
        }
    }

    /// `parse` (BusinessRecipients.hpp:31-50).
    pub fn parse(parser: &mut Parser) -> std::result::Result<BusinessRecipients, &'static str> {
        let [
            existing_chats,
            new_chats,
            contacts,
            non_contacts,
            exclude_selected,
            has_user_ids,
            has_excluded_user_ids,
        ] = parser.parse_flags()?;
        let mut result = BusinessRecipients {
            existing_chats,
            new_chats,
            contacts,
            non_contacts,
            exclude_selected,
            ..BusinessRecipients::default()
        };
        if has_user_ids {
            let len = parser.i32_()?;
            for _ in 0..len {
                result.user_ids.push(UserId::new(parser.i64_()?));
            }
        }
        if has_excluded_user_ids {
            let len = parser.i32_()?;
            for _ in 0..len {
                result.excluded_user_ids.push(UserId::new(parser.i64_()?));
            }
        }
        Ok(result)
    }
}

impl std::fmt::Display for BusinessRecipients {
    /// `operator<<` (BusinessRecipients.cpp:119-133).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "business recipients")?;
        if self.existing_chats {
            write!(f, " existing chats")?;
        }
        if self.new_chats {
            write!(f, " new chats")?;
        }
        if self.contacts {
            write!(f, " contacts")?;
        }
        if self.non_contacts {
            write!(f, " non-contacts")?;
        }
        if !self.user_ids.is_empty() {
            write!(f, " {} users", self.user_ids.len())?;
        }
        if !self.excluded_user_ids.is_empty() {
            write!(f, " excluding {} users", self.excluded_user_ids.len())?;
        }
        if self.exclude_selected {
            write!(f, " excluding selected")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_td_api_folds_the_excluded_set_when_selected() {
        // BusinessRecipients.cpp:47-56 — exclude_selected merges the
        // excluded user ids into the selected set.
        let recipients = BusinessRecipients::from_td_api(
            Some(td_api::BusinessRecipients {
                chat_ids: vec![1, 2],
                excluded_chat_ids: vec![3],
                select_existing_chats: true,
                select_new_chats: false,
                select_contacts: true,
                select_non_contacts: false,
                exclude_selected: true,
            }),
            true,
        );
        assert_eq!(
            recipients.user_ids,
            vec![UserId::new(1), UserId::new(2), UserId::new(3)]
        );
        assert!(recipients.excluded_user_ids.is_empty());
        assert!(recipients.existing_chats);
        assert!(recipients.contacts);
        assert!(!recipients.new_chats);
    }

    #[test]
    fn from_td_api_keeps_excluded_when_not_selected() {
        let recipients = BusinessRecipients::from_td_api(
            Some(td_api::BusinessRecipients {
                chat_ids: vec![1],
                excluded_chat_ids: vec![3],
                select_existing_chats: false,
                select_new_chats: false,
                select_contacts: false,
                select_non_contacts: false,
                exclude_selected: false,
            }),
            true,
        );
        assert_eq!(recipients.user_ids, vec![UserId::new(1)]);
        assert_eq!(recipients.excluded_user_ids, vec![UserId::new(3)]);
    }

    #[test]
    fn store_parse_round_trip_covers_every_flag_shape() {
        let value = BusinessRecipients {
            user_ids: vec![UserId::new(1), UserId::new(2)],
            excluded_user_ids: vec![UserId::new(3)],
            existing_chats: true,
            new_chats: true,
            contacts: false,
            non_contacts: true,
            exclude_selected: false,
        };
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(BusinessRecipients::parse(&mut parser).unwrap(), value);

        let value = BusinessRecipients::default();
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(BusinessRecipients::parse(&mut parser).unwrap(), value);
    }
}
