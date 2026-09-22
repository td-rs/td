//! Port of `td/telegram/JoinChatBotResult.{h,cpp}` (Wave 3, M.45 — Bots I):
//! the chat-join-bot request result discriminator. The consumer is
//! `UpdatesManager::on_update` for `updateChatJoinBotRequest` (the M.7
//! updates integration wires the call, UpdatesManager.cpp:4534).

use crate::td_api;
use tdmtproto::telegram_api;

/// `JoinChatBotResult::Type` (JoinChatBotResult.h:28).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Type {
    Approved,
    Declined,
    #[default]
    Queued,
}

/// `td::JoinChatBotResult` (JoinChatBotResult.h:17-33).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JoinChatBotResult {
    type_: Type,
}

impl JoinChatBotResult {
    /// `JoinChatBotResult::JoinChatBotResult(object_ptr<
    /// JoinChatBotResult> &&)` (JoinChatBotResult.cpp:13-30): the
    /// WebView variant logs the error and keeps the queued default; the
    /// generated enum is exhaustive, so the C++ UNREACHABLE default is
    /// unreachable here too.
    pub fn from_telegram(result: telegram_api::JoinChatBotResult) -> JoinChatBotResult {
        let mut result = JoinChatBotResult::default();
        match result {
            telegram_api::JoinChatBotResult::joinChatBotResultApproved(_) => {
                result.type_ = Type::Approved;
            }
            telegram_api::JoinChatBotResult::joinChatBotResultDeclined(_) => {
                result.type_ = Type::Declined;
            }
            telegram_api::JoinChatBotResult::joinChatBotResultQueued(_) => {
                result.type_ = Type::Queued;
            }
            telegram_api::JoinChatBotResult::joinChatBotResultWebView(web_view) => {
                tdutils::log_error!("Receive {:?}", web_view);
            }
        }
        result
    }

    /// `JoinChatBotResult::JoinChatBotResult(const object_ptr<
    /// ChatJoinRequestResult> &)` (JoinChatBotResult.cpp:32-49).
    pub fn from_td_api(result: Option<&td_api::ChatJoinRequestResult>) -> JoinChatBotResult {
        let mut result = JoinChatBotResult::default();
        let Some(result) = result else {
            return result;
        };
        match result {
            td_api::ChatJoinRequestResult::chatJoinRequestResultApproved(_) => {
                result.type_ = Type::Approved;
            }
            td_api::ChatJoinRequestResult::chatJoinRequestResultDeclined(_) => {
                result.type_ = Type::Declined;
            }
            td_api::ChatJoinRequestResult::chatJoinRequestResultQueued(_) => {
                result.type_ = Type::Queued;
            }
        }
        result
    }

    /// `JoinChatBotResult::get_chat_join_request_result_object`
    /// (JoinChatBotResult.cpp:51-63).
    pub fn get_chat_join_request_result_object(&self) -> td_api::ChatJoinRequestResult {
        match self.type_ {
            Type::Approved => td_api::ChatJoinRequestResult::chatJoinRequestResultApproved(
                td_api::chatJoinRequestResultApproved {},
            ),
            Type::Declined => td_api::ChatJoinRequestResult::chatJoinRequestResultDeclined(
                td_api::chatJoinRequestResultDeclined {},
            ),
            Type::Queued => td_api::ChatJoinRequestResult::chatJoinRequestResultQueued(
                td_api::chatJoinRequestResultQueued {},
            ),
        }
    }

    /// `JoinChatBotResult::get_input_join_chat_bot_result`
    /// (JoinChatBotResult.cpp:65-77).
    pub fn get_input_join_chat_bot_result(&self) -> telegram_api::JoinChatBotResult {
        match self.type_ {
            Type::Approved => telegram_api::JoinChatBotResult::joinChatBotResultApproved(
                telegram_api::joinChatBotResultApproved {},
            ),
            Type::Declined => telegram_api::JoinChatBotResult::joinChatBotResultDeclined(
                telegram_api::joinChatBotResultDeclined {},
            ),
            Type::Queued => telegram_api::JoinChatBotResult::joinChatBotResultQueued(
                telegram_api::joinChatBotResultQueued {},
            ),
        }
    }
}

/// `operator<<(StringBuilder &, const JoinChatBotResult &)`
/// (JoinChatBotResult.cpp:79-91).
impl std::fmt::Display for JoinChatBotResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.type_ {
            Type::Approved => f.write_str("JoinRequestApproved"),
            Type::Declined => f.write_str("JoinRequestDeclined"),
            Type::Queued => f.write_str("JoinRequestQueued"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_chat_bot_result_round_trips() {
        // the telegram intake keeps only the discriminator
        let approved = JoinChatBotResult::from_telegram(
            telegram_api::JoinChatBotResult::joinChatBotResultApproved(
                telegram_api::joinChatBotResultApproved {},
            ),
        );
        assert_eq!(
            approved,
            JoinChatBotResult {
                type_: super::Type::Approved
            }
        );
        assert!(matches!(
            approved.get_chat_join_request_result_object(),
            td_api::ChatJoinRequestResult::chatJoinRequestResultApproved(_)
        ));
        assert!(matches!(
            approved.get_input_join_chat_bot_result(),
            telegram_api::JoinChatBotResult::joinChatBotResultApproved(_)
        ));
        assert_eq!(format!("{approved}"), "JoinRequestApproved");

        let declined = JoinChatBotResult::from_telegram(
            telegram_api::JoinChatBotResult::joinChatBotResultDeclined(
                telegram_api::joinChatBotResultDeclined {},
            ),
        );
        assert_eq!(format!("{declined}"), "JoinRequestDeclined");

        // the queued default (JoinChatBotResult.cpp:21-23)
        let queued = JoinChatBotResult::from_telegram(
            telegram_api::JoinChatBotResult::joinChatBotResultQueued(
                telegram_api::joinChatBotResultQueued {},
            ),
        );
        assert_eq!(queued, JoinChatBotResult::default());
        assert_eq!(format!("{queued}"), "JoinRequestQueued");
    }

    #[test]
    fn join_chat_bot_result_from_td_api() {
        let approved = JoinChatBotResult::from_td_api(Some(
            &td_api::ChatJoinRequestResult::chatJoinRequestResultApproved(
                td_api::chatJoinRequestResultApproved {},
            ),
        ));
        assert!(matches!(
            approved.get_input_join_chat_bot_result(),
            telegram_api::JoinChatBotResult::joinChatBotResultApproved(_)
        ));

        // the null is the queued default (JoinChatBotResult.cpp:32-35)
        assert_eq!(JoinChatBotResult::from_td_api(None), JoinChatBotResult::default());
    }
}
