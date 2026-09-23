//! `td::BusinessBotManageBar` (BusinessBotManageBar.h/.cpp, M.48).

use crate::business_manager::BusinessEnv;
use crate::dialog_id::{DialogId, DialogType};
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::UserId;

/// `td::BusinessBotManageBar` (BusinessBotManageBar.h:14-47).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusinessBotManageBar {
    business_bot_user_id: UserId,
    business_bot_manage_url: String,
    is_business_bot_paused: bool,
    can_business_bot_reply: bool,
}

impl BusinessBotManageBar {
    /// `BusinessBotManageBar::create` (BusinessBotManageBar.cpp:12-23);
    /// `None` for the C++ nullptr (an empty bar).
    pub fn create(
        is_business_bot_paused: bool,
        can_business_bot_reply: bool,
        business_bot_user_id: UserId,
        business_bot_manage_url: String,
    ) -> Option<BusinessBotManageBar> {
        let action_bar = BusinessBotManageBar {
            is_business_bot_paused,
            can_business_bot_reply,
            business_bot_user_id,
            business_bot_manage_url,
        };
        if action_bar.is_empty() {
            return None;
        }
        Some(action_bar)
    }

    /// `BusinessBotManageBar::is_empty` (BusinessBotManageBar.cpp:25-27).
    pub fn is_empty(&self) -> bool {
        !self.business_bot_user_id.is_valid()
    }

    /// `BusinessBotManageBar::fix` (BusinessBotManageBar.cpp:29-38) — the
    /// dialog-shape validation; the invalid bar resets like the C++ `{}`
    /// assignment.
    pub fn fix(&mut self, dialog_id: DialogId) {
        let is_valid = if self.business_bot_user_id.is_valid() {
            dialog_id.get_type() == DialogType::User && !self.business_bot_manage_url.is_empty()
        } else {
            self.business_bot_manage_url.is_empty()
                && !self.is_business_bot_paused
                && !self.can_business_bot_reply
        };
        if !is_valid {
            tdutils::log_error!(
                "Receive business bot {} in {} with manage URL {}",
                self.business_bot_user_id.get(),
                dialog_id.get(),
                self.business_bot_manage_url
            );
            *self = BusinessBotManageBar::default();
        }
    }

    /// `get_business_bot_manage_bar_object` (BusinessBotManageBar.cpp:40-47);
    /// `None` for the C++ nullptr.
    pub fn get_business_bot_manage_bar_object(
        &self,
        env: &mut impl BusinessEnv,
    ) -> Option<td_api::BusinessBotManageBar> {
        if self.is_empty() {
            return None;
        }
        Some(td_api::BusinessBotManageBar {
            bot_user_id: env.get_user_id_object(self.business_bot_user_id, "businessBotManageBar"),
            manage_url: self.business_bot_manage_url.clone(),
            is_bot_paused: self.is_business_bot_paused,
            can_bot_reply: self.can_business_bot_reply,
        })
    }

    /// `BusinessBotManageBar::on_user_deleted` (BusinessBotManageBar.cpp:49-54).
    pub fn on_user_deleted(&mut self) -> bool {
        if self.is_empty() {
            return false;
        }
        *self = BusinessBotManageBar::default();
        true
    }

    /// `BusinessBotManageBar::set_business_bot_is_paused`
    /// (BusinessBotManageBar.cpp:56-60).
    pub fn set_business_bot_is_paused(&mut self, is_paused: bool) -> bool {
        if !self.business_bot_user_id.is_valid() || self.is_business_bot_paused == is_paused {
            return false;
        }
        self.is_business_bot_paused = is_paused;
        true
    }

    /// `store` (BusinessBotManageBar.h:33-48).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_business_bot_user_id = self.business_bot_user_id.is_valid();
        let has_business_bot_manage_url = !self.business_bot_manage_url.is_empty();
        store_parse::store_flags(
            out,
            [
                self.is_business_bot_paused,
                self.can_business_bot_reply,
                has_business_bot_user_id,
                has_business_bot_manage_url,
            ],
        );
        if has_business_bot_user_id {
            store_parse::store_i64(out, self.business_bot_user_id.get());
        }
        if has_business_bot_manage_url {
            store_parse::store_string(out, &self.business_bot_manage_url);
        }
    }

    /// `parse` (BusinessBotManageBar.h:49-64).
    pub fn parse(parser: &mut Parser) -> std::result::Result<BusinessBotManageBar, &'static str> {
        let [
            is_business_bot_paused,
            can_business_bot_reply,
            has_business_bot_user_id,
            has_business_bot_manage_url,
        ] = parser.parse_flags()?;
        let mut result = BusinessBotManageBar {
            is_business_bot_paused,
            can_business_bot_reply,
            ..BusinessBotManageBar::default()
        };
        if has_business_bot_user_id {
            result.business_bot_user_id = UserId::new(parser.i64_()?);
        }
        if has_business_bot_manage_url {
            result.business_bot_manage_url = parser.string()?;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_returns_none_for_an_empty_bar() {
        assert!(
            BusinessBotManageBar::create(false, false, UserId::default(), String::new()).is_none()
        );
        assert!(
            BusinessBotManageBar::create(
                false,
                false,
                UserId::new(5),
                "https://t.me/bot".to_owned()
            )
            .is_some()
        );
    }

    #[test]
    fn store_parse_round_trip() {
        let value = BusinessBotManageBar {
            business_bot_user_id: UserId::new(9),
            business_bot_manage_url: "https://t.me/manage".to_owned(),
            is_business_bot_paused: true,
            can_business_bot_reply: false,
        };
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        assert_eq!(BusinessBotManageBar::parse(&mut parser).unwrap(), value);
    }

    #[test]
    fn set_paused_returns_false_on_no_change() {
        let mut value = BusinessBotManageBar {
            business_bot_user_id: UserId::new(9),
            is_business_bot_paused: false,
            ..BusinessBotManageBar::default()
        };
        assert!(value.set_business_bot_is_paused(true));
        assert!(!value.set_business_bot_is_paused(true));
        assert!(!BusinessBotManageBar::default().set_business_bot_is_paused(true));
    }
}
