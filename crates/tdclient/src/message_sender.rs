//! The M.45-needed slice of `td/telegram/MessageSender.{h,cpp}`:
//! `get_message_sender_dialog_id` (MessageSender.cpp:173-215), the
//! td_api `MessageSender` → `DialogId` resolution the bot verification
//! request arms use. The rest of the file (the senders object builders)
//! lives in `dialog_manager.rs` (`get_message_sender_object`).
//!
//! Seams: `td_->user_manager_->have_user_force` rides the env, and
//! `td_->dialog_manager_->have_dialog_force` is the env seam whose stub
//! inventory answers `false` (unknown dialogs force-load nothing yet).

use crate::bot_info_manager::BotInfoManagerEnv;
use crate::dialog_id::{DialogId, DialogType};
use crate::td_api;
use crate::user_manager::UserId;
use tdutils::status::{Result, Status};

/// `get_message_sender_dialog_id` (MessageSender.cpp:173-215): `None` is
/// never returned — the C++ returns `Result<DialogId>`; the Rust mirrors
/// the errors.
pub fn get_message_sender_dialog_id(
    env: &mut impl BotInfoManagerEnv,
    message_sender_id: Option<&td_api::MessageSender>,
    check_access: bool,
    allow_empty: bool,
) -> Result<DialogId> {
    let Some(message_sender_id) = message_sender_id else {
        if allow_empty {
            return Result::Ok(DialogId::default());
        }
        return Result::from_status(Status::error(400, "Message sender must be non-empty"));
    };
    match message_sender_id {
        td_api::MessageSender::messageSenderUser(sender) => {
            let user_id = UserId::new(sender.user_id);
            if !user_id.is_valid() {
                if allow_empty && user_id == UserId::default() {
                    return Result::Ok(DialogId::default());
                }
                return Result::from_status(Status::error(
                    400,
                    "Invalid user identifier specified",
                ));
            }
            let know_user = env.have_user_force(user_id, "get_message_sender_dialog_id");
            if check_access && !know_user {
                return Result::from_status(Status::error(
                    400,
                    "Unknown user identifier specified",
                ));
            }
            Result::Ok(DialogId::new(user_id.get()))
        }
        td_api::MessageSender::messageSenderChat(sender) => {
            let dialog_id = DialogId::new(sender.chat_id);
            if !dialog_id.is_valid() {
                if allow_empty && dialog_id.get() == 0 {
                    return Result::Ok(DialogId::default());
                }
                return Result::from_status(Status::error(
                    400,
                    "Invalid chat identifier specified",
                ));
            }
            let know_dialog = if dialog_id.get_type() == DialogType::User {
                env.have_user_force(dialog_id.get_user_id(), "get_message_sender_dialog_id 2")
            } else {
                env.have_dialog_force(dialog_id, "get_message_sender_dialog_id")
            };
            if check_access && !know_dialog {
                return Result::from_status(Status::error(
                    400,
                    "Unknown chat identifier specified",
                ));
            }
            Result::Ok(dialog_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot_info_manager::test_env::{self, TestBotEnv};
    use crate::global::Global;
    use crate::option_manager::OptionManager;
    use crate::user_manager::UserManager;

    #[test]
    fn message_sender_dialog_id_halves() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();
        let mut env: TestBotEnv =
            test_env::fixture(&mut global, Some(&mut options), &mut user_manager, true, 1);

        // the null sender with allow_empty
        assert_eq!(
            get_message_sender_dialog_id(&mut env, None, false, true).unwrap(),
            DialogId::default()
        );
        // the null sender without allow_empty
        let error = get_message_sender_dialog_id(&mut env, None, false, false)
            .error()
            .clone();
        assert_eq!(error.message(), "Message sender must be non-empty");

        // the invalid user id without the empty allowance
        let error = get_message_sender_dialog_id(
            &mut env,
            Some(&td_api::MessageSender::messageSenderUser(
                td_api::messageSenderUser { user_id: -1 },
            )),
            true,
            false,
        )
        .error()
        .clone();
        assert_eq!(error.message(), "Invalid user identifier specified");

        // the invalid user id with the empty allowance returns the empty dialog
        assert_eq!(
            get_message_sender_dialog_id(
                &mut env,
                Some(&td_api::MessageSender::messageSenderUser(
                    td_api::messageSenderUser { user_id: 0 },
                )),
                true,
                true,
            )
            .unwrap(),
            DialogId::default()
        );

        // a valid user resolves to the user dialog
        assert_eq!(
            get_message_sender_dialog_id(
                &mut env,
                Some(&td_api::MessageSender::messageSenderUser(
                    td_api::messageSenderUser { user_id: 42 },
                )),
                false,
                false,
            )
            .unwrap(),
            DialogId::new(42)
        );

        // the check_access gate with an unknown user (the have_user_force
        // seam backs the manager, where 42 is unknown)
        let error = get_message_sender_dialog_id(
            &mut env,
            Some(&td_api::MessageSender::messageSenderUser(
                td_api::messageSenderUser { user_id: 42 },
            )),
            true,
            false,
        )
        .error()
        .clone();
        assert_eq!(error.message(), "Unknown user identifier specified");

        // the invalid chat id
        let error = get_message_sender_dialog_id(
            &mut env,
            Some(&td_api::MessageSender::messageSenderChat(
                td_api::messageSenderChat { chat_id: 0 },
            )),
            false,
            false,
        )
        .error()
        .clone();
        assert_eq!(error.message(), "Invalid chat identifier specified");

        // a valid chat id resolves through the dialog force-create seam —
        // the stub inventory answers false, so the access gate fails
        let error = get_message_sender_dialog_id(
            &mut env,
            Some(&td_api::MessageSender::messageSenderChat(
                td_api::messageSenderChat { chat_id: -1000005 },
            )),
            true,
            false,
        )
        .error()
        .clone();
        assert_eq!(error.message(), "Unknown chat identifier specified");
    }
}
