//! Port of `td/telegram/BotCommandScope.{h,cpp}` (Wave 3, M.45 — Bots I):
//! the bot command scope conversion between the td_api surface and the
//! `bots.*BotCommands` constructors.
//!
//! Seams: the dialog access halves ride
//! [`BotInfoManagerEnv::check_dialog_access`]/
//! [`BotInfoManagerEnv::is_broadcast_channel`]/[`BotInfoManagerEnv::
//! get_input_peer`]/[`BotInfoManagerEnv::get_input_user`] — the M.6/Chat
//! manager reads documented on the env.

use crate::bot_info_manager::BotInfoManagerEnv;
use crate::dialog_id::{DialogId, DialogType};
use crate::td_api;
use crate::user_manager::{AccessRights, UserId};
use tdmtproto::telegram_api;
use tdutils::status::{Result, Status};

/// `BotCommandScope::Type` (BotCommandScope.h:22-30).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScopeType {
    #[default]
    Default,
    AllUsers,
    AllChats,
    AllChatAdministrators,
    Dialog,
    DialogAdministrators,
    DialogParticipant,
}

/// `td::BotCommandScope` (BotCommandScope.h:21-41).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BotCommandScope {
    type_: ScopeType,
    dialog_id: DialogId,
    user_id: UserId,
}

impl BotCommandScope {
    /// `BotCommandScope::BotCommandScope(Type, DialogId, UserId)`
    /// (BotCommandScope.cpp:18-20).
    fn new(type_: ScopeType, dialog_id: DialogId, user_id: UserId) -> BotCommandScope {
        BotCommandScope {
            type_,
            dialog_id,
            user_id,
        }
    }

    /// `BotCommandScope::get_bot_command_scope`
    /// (BotCommandScope.cpp:22-88).
    pub fn get_bot_command_scope(
        env: &mut impl BotInfoManagerEnv,
        scope_ptr: Option<td_api::BotCommandScope>,
    ) -> Result<BotCommandScope> {
        let Some(scope_ptr) = scope_ptr else {
            return Result::Ok(BotCommandScope::new(
                ScopeType::Default,
                DialogId::default(),
                UserId::default(),
            ));
        };

        assert!(env.is_bot(), "CHECK failed: td->auth_manager_->is_bot()");
        let type_: ScopeType;
        let dialog_id;
        let mut user_id = UserId::default();
        match scope_ptr {
            td_api::BotCommandScope::botCommandScopeDefault(_) => {
                return Result::Ok(BotCommandScope::new(
                    ScopeType::Default,
                    DialogId::default(),
                    UserId::default(),
                ));
            }
            td_api::BotCommandScope::botCommandScopeAllPrivateChats(_) => {
                return Result::Ok(BotCommandScope::new(
                    ScopeType::AllUsers,
                    DialogId::default(),
                    UserId::default(),
                ));
            }
            td_api::BotCommandScope::botCommandScopeAllGroupChats(_) => {
                return Result::Ok(BotCommandScope::new(
                    ScopeType::AllChats,
                    DialogId::default(),
                    UserId::default(),
                ));
            }
            td_api::BotCommandScope::botCommandScopeAllChatAdministrators(_) => {
                return Result::Ok(BotCommandScope::new(
                    ScopeType::AllChatAdministrators,
                    DialogId::default(),
                    UserId::default(),
                ));
            }
            td_api::BotCommandScope::botCommandScopeChat(scope) => {
                type_ = ScopeType::Dialog;
                dialog_id = DialogId::new(scope.chat_id);
            }
            td_api::BotCommandScope::botCommandScopeChatAdministrators(scope) => {
                type_ = ScopeType::DialogAdministrators;
                dialog_id = DialogId::new(scope.chat_id);
            }
            td_api::BotCommandScope::botCommandScopeChatMember(scope) => {
                type_ = ScopeType::DialogParticipant;
                dialog_id = DialogId::new(scope.chat_id);
                user_id = UserId::new(scope.user_id);
                // TRY_STATUS(td->user_manager_->get_input_user(user_id))
                if let Err(status) = env.get_input_user(user_id) {
                    return Result::from_status(status);
                }
            }
        }

        if let tdutils::status::Result::Err(status) = env.check_dialog_access(
            dialog_id,
            false,
            AccessRights::Read,
            "get_bot_command_scope",
        ) {
            return Result::from_status(status);
        }

        match dialog_id.get_type() {
            DialogType::User => {
                if type_ != ScopeType::Dialog {
                    return Result::from_status(Status::error(
                        400,
                        "Can't use specified scope in private chats",
                    ));
                }
            }
            // ok
            DialogType::Chat => {}
            DialogType::Channel => {
                if env.is_broadcast_channel(dialog_id) {
                    return Result::from_status(Status::error(
                        400,
                        "Can't change commands in channel chats",
                    ));
                }
            }
            // UNREACHABLE in the C++
            DialogType::SecretChat | DialogType::None => {
                unreachable!("UNREACHABLE: unexpected bot command scope dialog type");
            }
        }

        Result::Ok(BotCommandScope::new(type_, dialog_id, user_id))
    }

    /// `BotCommandScope::get_input_bot_command_scope`
    /// (BotCommandScope.cpp:90-120).
    pub fn get_input_bot_command_scope(
        &self,
        env: &mut impl BotInfoManagerEnv,
    ) -> telegram_api::BotCommandScope {
        let input_peer = if self.dialog_id.is_valid() {
            env.get_input_peer(self.dialog_id, AccessRights::Read)
        } else {
            None
        };
        let input_user = env.get_input_user(self.user_id).ok();
        match self.type_ {
            ScopeType::Default => telegram_api::BotCommandScope::botCommandScopeDefault(
                telegram_api::botCommandScopeDefault {},
            ),
            ScopeType::AllUsers => telegram_api::BotCommandScope::botCommandScopeUsers(
                telegram_api::botCommandScopeUsers {},
            ),
            ScopeType::AllChats => telegram_api::BotCommandScope::botCommandScopeChats(
                telegram_api::botCommandScopeChats {},
            ),
            ScopeType::AllChatAdministrators => {
                telegram_api::BotCommandScope::botCommandScopeChatAdmins(
                    telegram_api::botCommandScopeChatAdmins {},
                )
            }
            ScopeType::Dialog => {
                assert!(input_peer.is_some(), "CHECK failed: input_peer != nullptr");
                telegram_api::BotCommandScope::botCommandScopePeer(
                    telegram_api::botCommandScopePeer {
                        peer: input_peer.map(Box::new),
                    },
                )
            }
            ScopeType::DialogAdministrators => {
                assert!(input_peer.is_some(), "CHECK failed: input_peer != nullptr");
                telegram_api::BotCommandScope::botCommandScopePeerAdmins(
                    telegram_api::botCommandScopePeerAdmins {
                        peer: input_peer.map(Box::new),
                    },
                )
            }
            ScopeType::DialogParticipant => {
                assert!(input_peer.is_some(), "CHECK failed: input_peer != nullptr");
                assert!(input_user.is_some(), "CHECK failed: input_user != nullptr");
                telegram_api::BotCommandScope::botCommandScopePeerUser(
                    telegram_api::botCommandScopePeerUser {
                        peer: input_peer.map(Box::new),
                        user_id: input_user,
                    },
                )
            }
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

    fn bot_env<'a>(
        global: &'a mut Global,
        options: &'a mut OptionManager,
        user_manager: &'a mut UserManager,
    ) -> TestBotEnv<'a> {
        test_env::fixture(global, Some(options), user_manager, true, 1)
    }

    #[test]
    fn scope_kinds_convert_to_the_input_ctors() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();
        let mut env = bot_env(&mut global, &mut options, &mut user_manager);

        // the null scope is the default scope (BotCommandScope.cpp:24-26)
        let scope = BotCommandScope::get_bot_command_scope(&mut env, None).unwrap();
        assert!(matches!(
            scope.get_input_bot_command_scope(&mut env),
            telegram_api::BotCommandScope::botCommandScopeDefault(_)
        ));

        for (ctor, expected) in [
            (
                td_api::BotCommandScope::botCommandScopeAllPrivateChats(
                    td_api::botCommandScopeAllPrivateChats {},
                ),
                "botCommandScopeUsers",
            ),
            (
                td_api::BotCommandScope::botCommandScopeAllGroupChats(
                    td_api::botCommandScopeAllGroupChats {},
                ),
                "botCommandScopeChats",
            ),
            (
                td_api::BotCommandScope::botCommandScopeAllChatAdministrators(
                    td_api::botCommandScopeAllChatAdministrators {},
                ),
                "botCommandScopeChatAdmins",
            ),
        ] {
            let scope = BotCommandScope::get_bot_command_scope(&mut env, Some(ctor)).unwrap();
            let input = scope.get_input_bot_command_scope(&mut env);
            assert_eq!(
                input,
                match expected {
                    "botCommandScopeUsers" => {
                        telegram_api::BotCommandScope::botCommandScopeUsers(
                            telegram_api::botCommandScopeUsers {},
                        )
                    }
                    "botCommandScopeChats" => {
                        telegram_api::BotCommandScope::botCommandScopeChats(
                            telegram_api::botCommandScopeChats {},
                        )
                    }
                    _ => telegram_api::BotCommandScope::botCommandScopeChatAdmins(
                        telegram_api::botCommandScopeChatAdmins {},
                    ),
                }
            );
        }
    }

    #[test]
    fn scope_chat_kinds_resolve_peers() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();
        let mut env = bot_env(&mut global, &mut options, &mut user_manager);

        // the bot scope with the self dialog resolves the Dialog kind; the
        // env models the empty input peer, so the CHECK holds
        let scope = BotCommandScope::get_bot_command_scope(
            &mut env,
            Some(td_api::BotCommandScope::botCommandScopeChat(
                td_api::botCommandScopeChat { chat_id: 1 },
            )),
        )
        .unwrap();
        assert_eq!(
            scope,
            BotCommandScope::new(ScopeType::Dialog, DialogId::new(1), UserId::default())
        );
        assert!(matches!(
            scope.get_input_bot_command_scope(&mut env),
            telegram_api::BotCommandScope::botCommandScopePeer(_)
        ));

        // the ChatMember kind validates the user first: the invalid user id
        // fails the input-user gate (the C++ TRY_STATUS)
        let error = BotCommandScope::get_bot_command_scope(
            &mut env,
            Some(td_api::BotCommandScope::botCommandScopeChatMember(
                td_api::botCommandScopeChatMember {
                    chat_id: 1,
                    user_id: -1,
                },
            )),
        )
        .error()
        .clone();
        assert_eq!(error.message(), "User not found");
    }

    #[test]
    fn scope_private_chat_rejects_administrator_kinds() {
        let mut global = Global::new();
        let mut options = OptionManager::new(false);
        let mut user_manager = UserManager::new();
        let mut env = bot_env(&mut global, &mut options, &mut user_manager);

        // a user dialog with the DialogAdministrators kind is the C++
        // "Can't use specified scope in private chats" (cpp:68-73)
        let error = BotCommandScope::get_bot_command_scope(
            &mut env,
            Some(td_api::BotCommandScope::botCommandScopeChatAdministrators(
                td_api::botCommandScopeChatAdministrators { chat_id: 1 },
            )),
        )
        .error()
        .clone();
        assert_eq!(
            error.message(),
            "Can't use specified scope in private chats"
        );

        // the broadcast channel kind is the C++ channel rejection (the
        // channel-band dialog id)
        env.broadcast_channels.push(-1000000000001);
        let error = BotCommandScope::get_bot_command_scope(
            &mut env,
            Some(td_api::BotCommandScope::botCommandScopeChat(
                td_api::botCommandScopeChat {
                    chat_id: -1000000000001,
                },
            )),
        )
        .error()
        .clone();
        assert_eq!(error.message(), "Can't change commands in channel chats");

        // the inaccessible dialog fails the access check first
        env.inaccessible_dialogs.push(4);
        let error = BotCommandScope::get_bot_command_scope(
            &mut env,
            Some(td_api::BotCommandScope::botCommandScopeChat(
                td_api::botCommandScopeChat { chat_id: 4 },
            )),
        )
        .error()
        .clone();
        assert_eq!(error.message(), "Can't access the chat");
    }
}
