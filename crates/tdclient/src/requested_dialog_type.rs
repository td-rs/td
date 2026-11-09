//! Port of `td/telegram/RequestedDialogType.{h,cpp,hpp}` — the payload
//! of a `keyboardButtonTypeRequestUsers`/`RequestChat`/
//! `RequestManagedBot` button: which dialog classes the client may
//! attach to the bot and under which filters.
//!
//! `check_shared_dialog` dereferences `Td *` in the C++ (the
//! user/chat-manager probes); the port reads the same probes through the
//! [`RequestedDialogTypeEnv`] trait, the collapsed-Td-slice convention of
//! the M.16 manager plane.

use tdutils::status::Status;

use crate::chat_manager::{
    AdministratorRights, ChannelId, ChannelType, ChatId, DialogParticipantStatus,
};
use crate::dialog_id::{DialogId, DialogType};
use crate::misc::clean_input_string;
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;

/// The `Td *` slice `RequestedDialogType::check_shared_dialog`
/// (RequestedDialogType.cpp:236-311) needs: the user-manager bot/
/// premium probes and the chat-manager dialog state probes.
pub trait RequestedDialogTypeEnv {
    /// `td->user_manager_->is_user_bot(user_id)`.
    fn is_user_bot(&self, user_id: UserId) -> bool;
    /// `td->user_manager_->is_user_premium(user_id)`.
    fn is_user_premium(&self, user_id: UserId) -> bool;
    /// `td->chat_manager_->get_chat_is_active(chat_id)`.
    fn get_chat_is_active(&self, chat_id: ChatId) -> bool;
    /// `td->chat_manager_->get_chat_status(chat_id)`.
    fn get_chat_status(&self, chat_id: ChatId) -> DialogParticipantStatus;
    /// `td->chat_manager_->is_broadcast_channel(channel_id)`.
    fn is_broadcast_channel(&self, channel_id: ChannelId) -> bool;
    /// `td->chat_manager_->is_monoforum_channel(channel_id)`.
    fn is_monoforum_channel(&self, channel_id: ChannelId) -> bool;
    /// `td->chat_manager_->is_forum_channel(channel_id)`.
    fn is_forum_channel(&self, channel_id: ChannelId) -> bool;
    /// `td->chat_manager_->get_channel_first_username(channel_id)`.
    fn get_channel_first_username(&self, channel_id: ChannelId) -> String;
    /// `td->chat_manager_->get_channel_status(channel_id)`.
    fn get_channel_status(&self, channel_id: ChannelId) -> DialogParticipantStatus;
    /// `td->chat_manager_->is_channel_public(channel_id)`.
    fn is_channel_public(&self, channel_id: ChannelId) -> bool;
}

/// `td::RequestedDialogType::Type` (RequestedDialogType.h:15) — append
/// only; the discriminants ride the store/parse format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RequestedDialogTypeType {
    #[default]
    User,
    Group,
    Channel,
    CreateBot,
}

impl RequestedDialogTypeType {
    fn as_i32(self) -> i32 {
        match self {
            RequestedDialogTypeType::User => 0,
            RequestedDialogTypeType::Group => 1,
            RequestedDialogTypeType::Channel => 2,
            RequestedDialogTypeType::CreateBot => 3,
        }
    }

    fn from_i32(value: i32) -> RequestedDialogTypeType {
        match value {
            1 => RequestedDialogTypeType::Group,
            2 => RequestedDialogTypeType::Channel,
            3 => RequestedDialogTypeType::CreateBot,
            _ => RequestedDialogTypeType::User,
        }
    }
}

/// `td::RequestedDialogType` (RequestedDialogType.h:14).
#[derive(Clone, Debug, PartialEq)]
pub struct RequestedDialogType {
    pub(crate) type_: RequestedDialogTypeType,
    pub(crate) button_id: i32,
    pub(crate) max_quantity: i32,         // User only
    pub(crate) restrict_is_bot: bool,     // User only
    pub(crate) is_bot: bool,              // User only
    pub(crate) restrict_is_premium: bool, // User only
    pub(crate) is_premium: bool,          // User only

    pub(crate) request_name: bool,
    pub(crate) request_username: bool,
    pub(crate) request_photo: bool,

    pub(crate) restrict_is_forum: bool,     // Group only
    pub(crate) is_forum: bool,              // Group only
    pub(crate) bot_is_participant: bool,    // Group only
    pub(crate) restrict_has_username: bool, // Group and Channel only
    pub(crate) has_username: bool,          // Group and Channel only
    pub(crate) is_created: bool,            // Group and Channel only
    pub(crate) restrict_user_administrator_rights: bool, // Group and Channel only
    pub(crate) restrict_bot_administrator_rights: bool, // Group and Channel only
    pub(crate) user_administrator_rights: AdministratorRights, // Group and Channel only
    pub(crate) bot_administrator_rights: AdministratorRights, // Group and Channel only

    pub(crate) suggested_name: String,     // CreateBot only
    pub(crate) suggested_username: String, // CreateBot only
}

impl Default for RequestedDialogType {
    /// The C++ field defaults: `button_id = 0`, `max_quantity = 1`, a
    /// default (empty) user type and empty right sets.
    fn default() -> RequestedDialogType {
        RequestedDialogType {
            type_: RequestedDialogTypeType::User,
            button_id: 0,
            max_quantity: 1,
            restrict_is_bot: false,
            is_bot: false,
            restrict_is_premium: false,
            is_premium: false,
            request_name: false,
            request_username: false,
            request_photo: false,
            restrict_is_forum: false,
            is_forum: false,
            bot_is_participant: false,
            restrict_has_username: false,
            has_username: false,
            is_created: false,
            restrict_user_administrator_rights: false,
            restrict_bot_administrator_rights: false,
            user_administrator_rights: AdministratorRights::default(),
            bot_administrator_rights: AdministratorRights::default(),
            suggested_name: String::new(),
            suggested_username: String::new(),
        }
    }
}

impl RequestedDialogType {
    /// `RequestedDialogType(td_api::object_ptr<
    /// td_api::keyboardButtonTypeRequestUsers> &&)`
    /// (RequestedDialogType.cpp:13-24).
    pub fn from_td_api_request_users(
        request_users: td_api::keyboardButtonTypeRequestUsers,
    ) -> RequestedDialogType {
        RequestedDialogType {
            type_: RequestedDialogTypeType::User,
            button_id: request_users.id,
            max_quantity: std::cmp::max(request_users.max_quantity, 1),
            restrict_is_bot: request_users.restrict_user_is_bot,
            is_bot: request_users.user_is_bot,
            restrict_is_premium: request_users.restrict_user_is_premium,
            is_premium: request_users.user_is_premium,
            request_name: request_users.request_name,
            request_username: request_users.request_username,
            request_photo: request_users.request_photo,
            ..RequestedDialogType::default()
        }
    }

    /// `RequestedDialogType(td_api::object_ptr<
    /// td_api::keyboardButtonTypeRequestChat> &&)`
    /// (RequestedDialogType.cpp:26-44).
    pub fn from_td_api_request_chat(
        request_dialog: td_api::keyboardButtonTypeRequestChat,
    ) -> RequestedDialogType {
        let channel_type = if request_dialog.chat_is_channel {
            ChannelType::Broadcast
        } else {
            ChannelType::Megagroup
        };
        RequestedDialogType {
            type_: if request_dialog.chat_is_channel {
                RequestedDialogTypeType::Channel
            } else {
                RequestedDialogTypeType::Group
            },
            button_id: request_dialog.id,
            restrict_is_forum: request_dialog.restrict_chat_is_forum,
            is_forum: request_dialog.chat_is_forum,
            bot_is_participant: request_dialog.bot_is_member,
            restrict_has_username: request_dialog.restrict_chat_has_username,
            has_username: request_dialog.chat_has_username,
            is_created: request_dialog.chat_is_created,
            restrict_user_administrator_rights: request_dialog.user_administrator_rights.is_some(),
            restrict_bot_administrator_rights: request_dialog.bot_administrator_rights.is_some(),
            user_administrator_rights: AdministratorRights::from_chat_administrator_rights(
                request_dialog.user_administrator_rights.as_ref(),
                channel_type,
            ),
            bot_administrator_rights: AdministratorRights::from_chat_administrator_rights(
                request_dialog.bot_administrator_rights.as_ref(),
                channel_type,
            ),
            request_name: request_dialog.request_title,
            request_username: request_dialog.request_username,
            request_photo: request_dialog.request_photo,
            ..RequestedDialogType::default()
        }
    }

    /// `RequestedDialogType(td_api::object_ptr<
    /// td_api::keyboardButtonTypeRequestManagedBot> &&)`
    /// (RequestedDialogType.cpp:46-58).
    pub fn from_td_api_request_managed_bot(
        request: td_api::keyboardButtonTypeRequestManagedBot,
    ) -> RequestedDialogType {
        let mut suggested_name = request.suggested_name;
        if clean_input_string(&suggested_name).is_none() {
            suggested_name.clear();
        }
        let mut suggested_username = request.suggested_username;
        if clean_input_string(&suggested_username).is_none() {
            suggested_username.clear();
        }
        RequestedDialogType {
            type_: RequestedDialogTypeType::CreateBot,
            button_id: request.id,
            suggested_name,
            suggested_username,
            ..RequestedDialogType::default()
        }
    }

    /// `RequestedDialogType(telegram_api::object_ptr<
    /// telegram_api::RequestPeerType> &&, int32, int32)`
    /// (RequestedDialogType.cpp:60-114).
    pub fn from_telegram(
        peer_type: telegram_api::RequestPeerType,
        button_id: i32,
        max_quantity: i32,
    ) -> RequestedDialogType {
        let mut result = RequestedDialogType {
            button_id,
            max_quantity: std::cmp::max(max_quantity, 1),
            ..RequestedDialogType::default()
        };
        match peer_type {
            telegram_api::RequestPeerType::requestPeerTypeUser(type_) => {
                result.type_ = RequestedDialogTypeType::User;
                result.restrict_is_bot = type_.flags & (1 << 0) != 0; // BOT_MASK
                result.is_bot = type_.bot;
                result.restrict_is_premium = type_.flags & (1 << 1) != 0; // PREMIUM_MASK
                result.is_premium = type_.premium;
            }
            telegram_api::RequestPeerType::requestPeerTypeChat(type_) => {
                result.type_ = RequestedDialogTypeType::Group;
                result.restrict_is_forum = type_.flags & (1 << 4) != 0; // FORUM_MASK
                result.is_forum = type_.forum;
                result.bot_is_participant = type_.bot_participant;
                result.restrict_has_username = type_.flags & (1 << 3) != 0; // HAS_USERNAME_MASK
                result.has_username = type_.has_username;
                result.is_created = type_.creator;
                result.restrict_user_administrator_rights =
                    !type_.creator && type_.user_admin_rights.is_some();
                result.restrict_bot_administrator_rights = type_.bot_admin_rights.is_some();
                result.user_administrator_rights = AdministratorRights::from_chat_admin_rights(
                    type_.user_admin_rights.as_ref(),
                    ChannelType::Megagroup,
                );
                result.bot_administrator_rights = AdministratorRights::from_chat_admin_rights(
                    type_.bot_admin_rights.as_ref(),
                    ChannelType::Megagroup,
                );
            }
            telegram_api::RequestPeerType::requestPeerTypeBroadcast(type_) => {
                result.type_ = RequestedDialogTypeType::Channel;
                result.restrict_has_username = type_.flags & (1 << 3) != 0; // HAS_USERNAME_MASK
                result.has_username = type_.has_username;
                result.is_created = type_.creator;
                result.restrict_user_administrator_rights =
                    !type_.creator && type_.user_admin_rights.is_some();
                result.restrict_bot_administrator_rights = type_.bot_admin_rights.is_some();
                result.user_administrator_rights = AdministratorRights::from_chat_admin_rights(
                    type_.user_admin_rights.as_ref(),
                    ChannelType::Broadcast,
                );
                result.bot_administrator_rights = AdministratorRights::from_chat_admin_rights(
                    type_.bot_admin_rights.as_ref(),
                    ChannelType::Broadcast,
                );
            }
            telegram_api::RequestPeerType::requestPeerTypeCreateBot(type_) => {
                if type_.bot_managed {
                    result.type_ = RequestedDialogTypeType::CreateBot;
                    result.suggested_name = type_.suggested_name;
                    result.suggested_username = type_.suggested_username;
                } else {
                    tdutils::log_error!("Receive request to create an unmanaged bot");
                    result.type_ = RequestedDialogTypeType::User;
                    result.restrict_is_bot = true;
                    result.is_bot = true;
                }
            }
        }
        result
    }

    /// `get_keyboard_button_type_object`
    /// (RequestedDialogType.cpp:116-139).
    pub fn get_keyboard_button_type_object(&self) -> td_api::KeyboardButtonType {
        if self.type_ == RequestedDialogTypeType::User {
            td_api::KeyboardButtonType::keyboardButtonTypeRequestUsers(
                td_api::keyboardButtonTypeRequestUsers {
                    id: self.button_id,
                    restrict_user_is_bot: self.restrict_is_bot,
                    user_is_bot: self.is_bot,
                    restrict_user_is_premium: self.restrict_is_premium,
                    user_is_premium: self.is_premium,
                    max_quantity: self.max_quantity,
                    request_name: self.request_name,
                    request_username: self.request_username,
                    request_photo: self.request_photo,
                },
            )
        } else if self.type_ == RequestedDialogTypeType::CreateBot {
            td_api::KeyboardButtonType::keyboardButtonTypeRequestManagedBot(
                td_api::keyboardButtonTypeRequestManagedBot {
                    id: self.button_id,
                    suggested_name: self.suggested_name.clone(),
                    suggested_username: self.suggested_username.clone(),
                },
            )
        } else {
            let user_administrator_rights = if self.restrict_user_administrator_rights {
                Some(
                    self.user_administrator_rights
                        .get_chat_administrator_rights_object(),
                )
            } else {
                None
            };
            let bot_administrator_rights = if self.restrict_bot_administrator_rights {
                Some(
                    self.bot_administrator_rights
                        .get_chat_administrator_rights_object(),
                )
            } else {
                None
            };
            td_api::KeyboardButtonType::keyboardButtonTypeRequestChat(
                td_api::keyboardButtonTypeRequestChat {
                    id: self.button_id,
                    chat_is_channel: self.type_ == RequestedDialogTypeType::Channel,
                    restrict_chat_is_forum: self.restrict_is_forum,
                    chat_is_forum: self.is_forum,
                    restrict_chat_has_username: self.restrict_has_username,
                    chat_has_username: self.has_username,
                    chat_is_created: self.is_created,
                    user_administrator_rights,
                    bot_administrator_rights,
                    bot_is_member: self.bot_is_participant,
                    request_title: self.request_name,
                    request_username: self.request_username,
                    request_photo: self.request_photo,
                },
            )
        }
    }

    /// `get_input_request_peer_type_object`
    /// (RequestedDialogType.cpp:141-202).
    fn get_input_request_peer_type_object(&self) -> telegram_api::RequestPeerType {
        match self.type_ {
            RequestedDialogTypeType::User => {
                let mut flags = 0;
                if self.restrict_is_bot {
                    flags |= 1 << 0; // BOT_MASK
                }
                if self.restrict_is_premium {
                    flags |= 1 << 1; // PREMIUM_MASK
                }
                telegram_api::RequestPeerType::requestPeerTypeUser(
                    telegram_api::requestPeerTypeUser {
                        flags,
                        bot: self.is_bot,
                        premium: self.is_premium,
                    },
                )
            }
            RequestedDialogTypeType::Group => {
                let mut flags = 0;
                if self.restrict_is_forum {
                    flags |= 1 << 4; // FORUM_MASK
                }
                if self.restrict_has_username {
                    flags |= 1 << 3; // HAS_USERNAME_MASK
                }
                if self.restrict_user_administrator_rights {
                    flags |= 1 << 1; // USER_ADMIN_RIGHTS_MASK
                }
                if self.restrict_bot_administrator_rights {
                    flags |= 1 << 2; // BOT_ADMIN_RIGHTS_MASK
                }
                let user_admin_rights = if self.restrict_user_administrator_rights {
                    Some(self.user_administrator_rights.get_chat_admin_rights())
                } else {
                    None
                };
                let bot_admin_rights = if self.restrict_bot_administrator_rights {
                    Some(self.bot_administrator_rights.get_chat_admin_rights())
                } else {
                    None
                };
                telegram_api::RequestPeerType::requestPeerTypeChat(
                    telegram_api::requestPeerTypeChat {
                        flags,
                        creator: self.is_created,
                        bot_participant: self.bot_is_participant,
                        has_username: self.has_username,
                        forum: self.is_forum,
                        user_admin_rights,
                        bot_admin_rights,
                    },
                )
            }
            RequestedDialogTypeType::Channel => {
                let mut flags = 0;
                if self.restrict_has_username {
                    flags |= 1 << 3; // HAS_USERNAME_MASK
                }
                if self.restrict_user_administrator_rights {
                    flags |= 1 << 1; // USER_ADMIN_RIGHTS_MASK
                }
                if self.restrict_bot_administrator_rights {
                    flags |= 1 << 2; // BOT_ADMIN_RIGHTS_MASK
                }
                let user_admin_rights = if self.restrict_user_administrator_rights {
                    Some(self.user_administrator_rights.get_chat_admin_rights())
                } else {
                    None
                };
                let bot_admin_rights = if self.restrict_bot_administrator_rights {
                    Some(self.bot_administrator_rights.get_chat_admin_rights())
                } else {
                    None
                };
                telegram_api::RequestPeerType::requestPeerTypeBroadcast(
                    telegram_api::requestPeerTypeBroadcast {
                        flags,
                        creator: self.is_created,
                        has_username: self.has_username,
                        user_admin_rights,
                        bot_admin_rights,
                    },
                )
            }
            RequestedDialogTypeType::CreateBot => {
                let mut flags = 0;
                if !self.suggested_name.is_empty() {
                    flags |= 1 << 1; // SUGGESTED_NAME_MASK
                }
                if !self.suggested_username.is_empty() {
                    flags |= 1 << 2; // SUGGESTED_USERNAME_MASK
                }
                telegram_api::RequestPeerType::requestPeerTypeCreateBot(
                    telegram_api::requestPeerTypeCreateBot {
                        flags,
                        bot_managed: true,
                        suggested_name: self.suggested_name.clone(),
                        suggested_username: self.suggested_username.clone(),
                    },
                )
            }
        }
    }

    /// `get_input_button_type_request_peer`
    /// (RequestedDialogType.cpp:204-209).
    pub fn get_input_button_type_request_peer(&self) -> telegram_api::inputButtonTypeRequestPeer {
        telegram_api::inputButtonTypeRequestPeer {
            flags: 0,
            name_requested: self.request_name,
            username_requested: self.request_username,
            photo_requested: self.request_photo,
            button_id: self.button_id,
            peer_type: Some(self.get_input_request_peer_type_object()),
            max_quantity: self.max_quantity,
        }
    }

    /// `get_button_id` (RequestedDialogType.cpp:211-213).
    pub fn get_button_id(&self) -> i32 {
        self.button_id
    }

    /// `check_shared_dialog` (RequestedDialogType.cpp:215-311) — the
    /// synchronous acceptance table over the env probes.
    pub fn check_shared_dialog(
        &self,
        env: &dyn RequestedDialogTypeEnv,
        dialog_id: DialogId,
    ) -> Result<(), Status> {
        match dialog_id.get_type() {
            DialogType::User => {
                // 26766da73 — check_shared_user: a managed-bot
                // (CreateBot) request shares a user dialog too
                if self.type_ != RequestedDialogTypeType::User
                    && self.type_ != RequestedDialogTypeType::CreateBot
                {
                    return Err(Status::error(400, "Wrong chat type"));
                }
                let user_id = dialog_id.get_user_id();
                // The server verifies ownership and the manager of a
                // newly created bot; locally, never accept an ordinary
                // user for a managed-bot request.
                let is_bot = env.is_user_bot(user_id);
                if (self.type_ == RequestedDialogTypeType::CreateBot && !is_bot)
                    || (self.restrict_is_bot && is_bot != self.is_bot)
                {
                    return Err(Status::error(400, "Wrong is_bot value"));
                }
                if self.restrict_is_premium && env.is_user_premium(user_id) != self.is_premium {
                    return Err(Status::error(400, "Wrong is_premium value"));
                }
            }
            DialogType::Chat => {
                if self.type_ != RequestedDialogTypeType::Group {
                    return Err(Status::error(400, "Wrong chat type"));
                }
                if self.restrict_is_forum && self.is_forum {
                    return Err(Status::error(400, "Wrong is_forum value"));
                }
                if self.restrict_has_username && self.has_username {
                    return Err(Status::error(400, "Wrong has_username value"));
                }
                let chat_id = dialog_id.get_chat_id();
                if !env.get_chat_is_active(chat_id) {
                    return Err(Status::error(400, "Chat is deactivated"));
                }
                let status = env.get_chat_status(chat_id);
                if self.is_created && !status.is_creator() {
                    return Err(Status::error(
                        400,
                        "The chat must be created by the current user",
                    ));
                }
                if self.bot_is_participant {
                    // can't check that the bot is already participant, so
                    // check that the user can add it instead
                    if !status.can_invite_users() {
                        return Err(Status::error(400, "The bot can't be added to the chat"));
                    }
                }
                if self.restrict_user_administrator_rights
                    && !status.has_all_administrator_rights(self.user_administrator_rights)
                {
                    return Err(Status::error(400, "Not enough rights in the chat"));
                }
                if self.restrict_bot_administrator_rights {
                    // can't check that the bot is already an administrator,
                    // so check that the user can promote it instead
                    if !status.can_invite_users() || !status.can_promote_members() {
                        return Err(Status::error(400, "The bot can't be promoted in the chat"));
                    }
                }
            }
            DialogType::Channel => {
                let channel_id = dialog_id.get_channel_id();
                let is_broadcast = env.is_broadcast_channel(channel_id);
                let expected_type = if is_broadcast {
                    RequestedDialogTypeType::Channel
                } else {
                    RequestedDialogTypeType::Group
                };
                if self.type_ != expected_type {
                    return Err(Status::error(400, "Wrong chat type"));
                }
                if env.is_monoforum_channel(channel_id) {
                    return Err(Status::error(
                        400,
                        "Channel direct messages chats can't be shared with bots",
                    ));
                }
                if !is_broadcast
                    && self.restrict_is_forum
                    && env.is_forum_channel(channel_id) != self.is_forum
                {
                    return Err(Status::error(400, "Wrong is_forum value"));
                }
                if self.restrict_has_username
                    && env.get_channel_first_username(channel_id).is_empty() == self.has_username
                {
                    return Err(Status::error(400, "Wrong has_username value"));
                }
                let status = env.get_channel_status(channel_id);
                if self.is_created && !status.is_creator() {
                    return Err(Status::error(
                        400,
                        "The chat must be created by the current user",
                    ));
                }
                let can_invite_bot = status.can_invite_users()
                    && (status.is_administrator() || !env.is_channel_public(channel_id));
                if !is_broadcast && self.bot_is_participant {
                    // can't synchronously check that the bot is already
                    // participant
                    if !can_invite_bot {
                        // return Status::Error(400, "The bot can't be added to the chat");
                    }
                }
                if self.restrict_user_administrator_rights
                    && !status.has_all_administrator_rights(self.user_administrator_rights)
                {
                    return Err(Status::error(400, "Not enough rights in the chat"));
                }
                if self.restrict_bot_administrator_rights {
                    // can't synchronously check that the bot is already an
                    // administrator
                    if !can_invite_bot || !status.can_promote_members() {
                        // return Status::Error(400, "The bot can't be promoted in the chat");
                    }
                }
            }
            DialogType::SecretChat => {
                return Err(Status::error(400, "Can't share secret chats"));
            }
            DialogType::None => unreachable!("the C++ UNREACHABLE() fired"),
        }
        Ok(())
    }

    /// `check_shared_dialog_count` (RequestedDialogType.cpp:313-321)
    /// with the 26766da73 managed-bot cap: a CreateBot request accepts
    /// one dialog regardless of max_quantity.
    pub fn check_shared_dialog_count(&self, count: usize) -> Result<(), Status> {
        if count == 0 {
            return Err(Status::error(400, "Too few chats are chosen"));
        }
        let max_quantity = if self.type_ == RequestedDialogTypeType::CreateBot {
            1
        } else {
            self.max_quantity
        };
        if count > max_quantity as usize {
            return Err(Status::error(400, "Too many chats are chosen"));
        }
        Ok(())
    }

    /// `RequestedDialogType::store` (RequestedDialogType.hpp:11-50).
    pub fn store(&self, out: &mut Vec<u8>) {
        let has_max_quantity = self.max_quantity != 1;
        let has_suggested_name = !self.suggested_name.is_empty();
        let has_suggested_username = !self.suggested_username.is_empty();
        crate::store_parse::store_flags(
            out,
            [
                self.restrict_is_bot,
                self.is_bot,
                self.restrict_is_premium,
                self.is_premium,
                self.restrict_is_forum,
                self.is_forum,
                self.bot_is_participant,
                self.restrict_has_username,
                self.has_username,
                self.is_created,
                self.restrict_user_administrator_rights,
                self.restrict_bot_administrator_rights,
                has_max_quantity,
                has_suggested_name,
                has_suggested_username,
            ],
        );
        crate::store_parse::store_i32(out, self.type_.as_i32());
        crate::store_parse::store_i32(out, self.button_id);
        if self.restrict_user_administrator_rights {
            self.user_administrator_rights.store(out);
        }
        if self.restrict_bot_administrator_rights {
            self.bot_administrator_rights.store(out);
        }
        if has_max_quantity {
            crate::store_parse::store_i32(out, self.max_quantity);
        }
        if has_suggested_name {
            crate::store_parse::store_string(out, &self.suggested_name);
        }
        if has_suggested_username {
            crate::store_parse::store_string(out, &self.suggested_username);
        }
    }

    /// `RequestedDialogType::parse` (RequestedDialogType.hpp:52-94).
    pub fn parse(
        parser: &mut crate::store_parse::Parser<'_>,
    ) -> Result<RequestedDialogType, &'static str> {
        let [
            restrict_is_bot,
            is_bot,
            restrict_is_premium,
            is_premium,
            restrict_is_forum,
            is_forum,
            bot_is_participant,
            restrict_has_username,
            has_username,
            is_created,
            restrict_user_administrator_rights,
            restrict_bot_administrator_rights,
            has_max_quantity,
            has_suggested_name,
            has_suggested_username,
        ] = parser.parse_flags::<15>()?;
        let type_ = RequestedDialogTypeType::from_i32(parser.i32_()?);
        let button_id = parser.i32_()?;
        let user_administrator_rights = if restrict_user_administrator_rights {
            AdministratorRights::parse_impl(parser)?
        } else {
            AdministratorRights::default()
        };
        let bot_administrator_rights = if restrict_bot_administrator_rights {
            AdministratorRights::parse_impl(parser)?
        } else {
            AdministratorRights::default()
        };
        let max_quantity = if has_max_quantity { parser.i32_()? } else { 1 };
        let suggested_name = if has_suggested_name {
            parser.string()?
        } else {
            String::new()
        };
        let suggested_username = if has_suggested_username {
            parser.string()?
        } else {
            String::new()
        };
        Ok(RequestedDialogType {
            type_,
            button_id,
            max_quantity,
            restrict_is_bot,
            is_bot,
            restrict_is_premium,
            is_premium,
            request_name: false,
            request_username: false,
            request_photo: false,
            restrict_is_forum,
            is_forum,
            bot_is_participant,
            restrict_has_username,
            has_username,
            is_created,
            restrict_user_administrator_rights,
            restrict_bot_administrator_rights,
            user_administrator_rights,
            bot_administrator_rights,
            suggested_name,
            suggested_username,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_manager::RestrictedRights;

    /// The offline [`RequestedDialogTypeEnv`] stub: plain tables plus
    /// flags for the monoforum/public branches.
    struct TestEnv {
        user_bots: Vec<UserId>,
        user_premiums: Vec<UserId>,
        chat_active: bool,
        chat_status: DialogParticipantStatus,
        broadcast: bool,
        monoforum: bool,
        forum: bool,
        first_username: String,
        channel_status: DialogParticipantStatus,
        public: bool,
    }

    impl Default for TestEnv {
        fn default() -> TestEnv {
            TestEnv {
                user_bots: Vec::new(),
                user_premiums: Vec::new(),
                chat_active: true,
                chat_status: DialogParticipantStatus::left(),
                broadcast: false,
                monoforum: false,
                forum: false,
                first_username: String::new(),
                channel_status: DialogParticipantStatus::left(),
                public: false,
            }
        }
    }

    impl RequestedDialogTypeEnv for TestEnv {
        fn is_user_bot(&self, user_id: UserId) -> bool {
            self.user_bots.contains(&user_id)
        }
        fn is_user_premium(&self, user_id: UserId) -> bool {
            self.user_premiums.contains(&user_id)
        }
        fn get_chat_is_active(&self, _chat_id: ChatId) -> bool {
            self.chat_active
        }
        fn get_chat_status(&self, _chat_id: ChatId) -> DialogParticipantStatus {
            self.chat_status.clone()
        }
        fn is_broadcast_channel(&self, _channel_id: ChannelId) -> bool {
            self.broadcast
        }
        fn is_monoforum_channel(&self, _channel_id: ChannelId) -> bool {
            self.monoforum
        }
        fn is_forum_channel(&self, _channel_id: ChannelId) -> bool {
            self.forum
        }
        fn get_channel_first_username(&self, _channel_id: ChannelId) -> String {
            self.first_username.clone()
        }
        fn get_channel_status(&self, _channel_id: ChannelId) -> DialogParticipantStatus {
            self.channel_status.clone()
        }
        fn is_channel_public(&self, _channel_id: ChannelId) -> bool {
            self.public
        }
    }

    fn td_request_users() -> td_api::keyboardButtonTypeRequestUsers {
        td_api::keyboardButtonTypeRequestUsers {
            id: 7,
            restrict_user_is_bot: true,
            user_is_bot: false,
            restrict_user_is_premium: false,
            user_is_premium: true,
            max_quantity: 0, // clamped up to 1
            request_name: true,
            request_username: true,
            request_photo: true,
        }
    }

    #[test]
    fn request_users_intake_and_object() {
        let rdt = RequestedDialogType::from_td_api_request_users(td_request_users());
        assert_eq!(rdt.type_, RequestedDialogTypeType::User);
        assert_eq!(rdt.get_button_id(), 7);
        assert_eq!(rdt.max_quantity, 1, "max_quantity clamps up to 1");
        assert!(rdt.restrict_is_bot && !rdt.is_bot);
        assert!(rdt.is_premium);

        match rdt.get_keyboard_button_type_object() {
            td_api::KeyboardButtonType::keyboardButtonTypeRequestUsers(users) => {
                assert_eq!(users.max_quantity, 1);
                assert_eq!(users.id, 7);
                assert!(users.user_is_premium);
            }
            other => panic!("wrong object: {other:?}"),
        }
    }

    #[test]
    fn managed_bot_intake_cleans_non_utf8_names() {
        let request = td_api::keyboardButtonTypeRequestManagedBot {
            id: 3,
            // a 4-byte control char becomes a space, so this stays
            suggested_name: "bo\u{1}t".to_owned(),
            suggested_username: "\u{0}bad".to_owned(),
        };
        let rdt = RequestedDialogType::from_td_api_request_managed_bot(request);
        assert_eq!(rdt.type_, RequestedDialogTypeType::CreateBot);
        assert!(!rdt.suggested_name.is_empty());
        // The C++ only CLEARS the name when clean_input_string fails
        // (invalid UTF-8); a valid string keeps its raw bytes even with
        // control characters. Rust td_api strings are always valid UTF-8,
        // so the constructor never observes the clear branch here.
        assert_eq!(rdt.suggested_name, "bo\u{1}t");
        assert_eq!(rdt.suggested_username, "\u{0}bad");
    }

    #[test]
    fn request_chat_intake_administrator_rights() {
        let rights = td_api::ChatAdministratorRights {
            can_manage_chat: true,
            can_change_info: true,
            can_post_messages: true,
            can_edit_messages: true,
            can_delete_messages: true,
            can_invite_users: true,
            can_restrict_members: true,
            can_pin_messages: true,
            can_manage_topics: true,
            can_promote_members: true,
            can_manage_video_chats: true,
            is_anonymous: false,
            can_manage_direct_messages: false,
            can_manage_tags: false,
            can_post_stories: false,
            can_edit_stories: false,
            can_delete_stories: false,
            can_send_welcome_messages: false,
        };
        let request = td_api::keyboardButtonTypeRequestChat {
            id: 9,
            chat_is_channel: true,
            restrict_chat_is_forum: false,
            chat_is_forum: false,
            restrict_chat_has_username: true,
            chat_has_username: true,
            chat_is_created: false,
            user_administrator_rights: Some(rights.clone()),
            bot_administrator_rights: None,
            bot_is_member: true,
            request_title: true,
            request_username: false,
            request_photo: true,
        };
        let rdt = RequestedDialogType::from_td_api_request_chat(request);
        assert_eq!(rdt.type_, RequestedDialogTypeType::Channel);
        assert!(rdt.restrict_user_administrator_rights);
        assert!(!rdt.restrict_bot_administrator_rights);
        assert!(rdt.bot_is_participant);

        match rdt.get_keyboard_button_type_object() {
            td_api::KeyboardButtonType::keyboardButtonTypeRequestChat(chat) => {
                assert!(chat.chat_is_channel);
                assert!(chat.user_administrator_rights.is_some());
                assert!(chat.bot_administrator_rights.is_none());
            }
            other => panic!("wrong object: {other:?}"),
        }

        // The wire output marks both right-mask bits and carries only the
        // user rights object.
        match rdt.get_input_button_type_request_peer().peer_type {
            Some(telegram_api::RequestPeerType::requestPeerTypeBroadcast(broadcast)) => {
                assert_eq!(broadcast.flags, (1 << 1) | (1 << 3));
                assert!(broadcast.user_admin_rights.is_some());
                assert!(broadcast.bot_admin_rights.is_none());
                assert!(broadcast.has_username);
            }
            other => panic!("wrong wire type: {other:?}"),
        }
    }

    #[test]
    fn wire_user_intake_masks() {
        let rdt = RequestedDialogType::from_telegram(
            telegram_api::RequestPeerType::requestPeerTypeUser(telegram_api::requestPeerTypeUser {
                flags: (1 << 0) | (1 << 1),
                bot: true,
                premium: false,
            }),
            5,
            3,
        );
        assert_eq!(rdt.type_, RequestedDialogTypeType::User);
        assert!(rdt.restrict_is_bot && rdt.is_bot);
        assert!(rdt.restrict_is_premium && !rdt.is_premium);
        assert_eq!(rdt.max_quantity, 3);

        match rdt.get_input_request_peer_type_object() {
            telegram_api::RequestPeerType::requestPeerTypeUser(user) => {
                assert_eq!(user.flags, (1 << 0) | (1 << 1));
            }
            other => panic!("wrong wire type: {other:?}"),
        }
    }

    #[test]
    fn unmanaged_wire_bot_downgrades_to_restricted_user() {
        let rdt = RequestedDialogType::from_telegram(
            telegram_api::RequestPeerType::requestPeerTypeCreateBot(
                telegram_api::requestPeerTypeCreateBot {
                    flags: 0,
                    bot_managed: false,
                    suggested_name: "x".to_owned(),
                    suggested_username: "y".to_owned(),
                },
            ),
            1,
            1,
        );
        assert_eq!(rdt.type_, RequestedDialogTypeType::User);
        assert!(rdt.restrict_is_bot && rdt.is_bot);
    }

    #[test]
    fn store_parse_round_trip() {
        let mut rdt =
            RequestedDialogType::from_td_api_request_chat(td_api::keyboardButtonTypeRequestChat {
                id: 4,
                chat_is_channel: false,
                restrict_chat_is_forum: true,
                chat_is_forum: true,
                restrict_chat_has_username: false,
                chat_has_username: false,
                chat_is_created: false,
                user_administrator_rights: None,
                bot_administrator_rights: None,
                bot_is_member: true,
                request_title: true,
                request_username: false,
                request_photo: false,
            });
        let mut out = Vec::new();
        rdt.store(&mut out);
        let parsed = RequestedDialogType::parse(&mut Parser::new(&out)).expect("parse");
        // The request_* flags are deliberately not part of the C++ store
        // format, so the parsed copy loses them.
        rdt.request_name = false;
        assert_eq!(parsed, rdt);

        // Default: only the type and button id words.
        let mut default_out = Vec::new();
        RequestedDialogType::default().store(&mut default_out);
        assert_eq!(default_out.len(), 4 + 4 + 4); // flags + type + button_id
        let parsed =
            RequestedDialogType::parse(&mut Parser::new(&default_out)).expect("parse default");
        assert_eq!(parsed, RequestedDialogType::default());
    }

    use crate::store_parse::Parser;

    #[test]
    fn check_shared_dialog_user_table() {
        let env = TestEnv {
            user_bots: vec![UserId::new(100)],
            ..TestEnv::default()
        };
        // A restricted-to-bots request matches a bot user.
        let rdt = RequestedDialogType {
            restrict_is_bot: true,
            is_bot: true,
            ..RequestedDialogType::default()
        };
        assert!(rdt.check_shared_dialog(&env, DialogId::new(100)).is_ok());
        assert_eq!(
            rdt.check_shared_dialog(&env, DialogId::new(200))
                .expect_err("not a bot")
                .message(),
            "Wrong is_bot value"
        );
        // 26766da73 — a managed-bot (CreateBot) request shares a user
        // dialog, but never an ordinary user; its count caps at 1.
        let create_bot = RequestedDialogType {
            type_: RequestedDialogTypeType::CreateBot,
            max_quantity: 5,
            ..RequestedDialogType::default()
        };
        assert!(
            create_bot
                .check_shared_dialog(&env, DialogId::new(100))
                .is_ok()
        );
        assert_eq!(
            create_bot
                .check_shared_dialog(&env, DialogId::new(200))
                .expect_err("an ordinary user is not a managed bot")
                .message(),
            "Wrong is_bot value"
        );
        assert!(create_bot.check_shared_dialog_count(1).is_ok());
        assert_eq!(
            create_bot
                .check_shared_dialog_count(2)
                .expect_err("the managed-bot cap is 1")
                .message(),
            "Too many chats are chosen"
        );
        // A group request against a user dialog is a type mismatch.
        let group = RequestedDialogType {
            type_: RequestedDialogTypeType::Group,
            ..RequestedDialogType::default()
        };
        assert_eq!(
            group
                .check_shared_dialog(&env, DialogId::new(100))
                .expect_err("wrong type")
                .message(),
            "Wrong chat type"
        );
        // Premium mismatch.
        let premium = RequestedDialogType {
            restrict_is_premium: true,
            is_premium: true,
            ..RequestedDialogType::default()
        };
        assert_eq!(
            premium
                .check_shared_dialog(&env, DialogId::new(100))
                .expect_err("not premium")
                .message(),
            "Wrong is_premium value"
        );
        // Secret chats are never shareable.
        assert_eq!(
            rdt.check_shared_dialog(&env, DialogId::new(-2_000_000_000_500))
                .expect_err("secret chat")
                .message(),
            "Can't share secret chats"
        );
    }

    #[test]
    fn check_shared_dialog_chat_table() {
        let creator_status = creator_status_with_creator();
        let env = TestEnv {
            chat_status: creator_status.clone(),
            ..TestEnv::default()
        };
        let rdt = RequestedDialogType {
            type_: RequestedDialogTypeType::Group,
            is_created: true,
            ..RequestedDialogType::default()
        };
        assert!(rdt.check_shared_dialog(&env, DialogId::new(-42)).is_ok());

        // Deactivated chat is rejected before the status checks.
        let env = TestEnv {
            chat_active: false,
            ..TestEnv::default()
        };
        assert_eq!(
            rdt.check_shared_dialog(&env, DialogId::new(-42))
                .expect_err("deactivated")
                .message(),
            "Chat is deactivated"
        );

        // is_created against a non-creator member.
        let env = TestEnv::default();
        assert_eq!(
            rdt.check_shared_dialog(&env, DialogId::new(-42))
                .expect_err("not creator")
                .message(),
            "The chat must be created by the current user"
        );

        // The bot_is_participant invite-rights stand-in check: left()
        // members keep ALL_RESTRICTED_RIGHTS (so they may invite), the
        // table needs a rights set without the invite bit.
        let no_invite_rights = RestrictedRights::from_bools(
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            ChannelType::Unknown,
        );
        let env = TestEnv {
            chat_status: DialogParticipantStatus::restricted(
                no_invite_rights,
                false,
                0,
                ChannelType::Unknown,
                String::new(),
            ),
            ..TestEnv::default()
        };
        let rdt = RequestedDialogType {
            type_: RequestedDialogTypeType::Group,
            bot_is_participant: true,
            ..RequestedDialogType::default()
        };
        assert_eq!(
            rdt.check_shared_dialog(&env, DialogId::new(-42))
                .expect_err("no invite rights")
                .message(),
            "The bot can't be added to the chat"
        );

        // restrict_is_forum + is_forum is contradictory.
        let rdt = RequestedDialogType {
            type_: RequestedDialogTypeType::Group,
            restrict_is_forum: true,
            is_forum: true,
            ..RequestedDialogType::default()
        };
        assert_eq!(
            rdt.check_shared_dialog(&env, DialogId::new(-42))
                .expect_err("forum")
                .message(),
            "Wrong is_forum value"
        );
    }

    fn creator_status_with_creator() -> DialogParticipantStatus {
        // `DialogParticipantStatus::creator` (the C++ named constructor).
        DialogParticipantStatus::creator(true, false, String::new())
    }

    #[test]
    fn check_shared_dialog_channel_table() {
        // A broadcast channel request.
        let rdt = RequestedDialogType {
            type_: RequestedDialogTypeType::Channel,
            ..RequestedDialogType::default()
        };
        let env = TestEnv {
            broadcast: true,
            monoforum: true,
            ..TestEnv::default()
        };
        assert_eq!(
            rdt.check_shared_dialog(&env, DialogId::new(-1_000_000_000_001))
                .expect_err("monoforum")
                .message(),
            "Channel direct messages chats can't be shared with bots"
        );

        // A megagroup-typed request against a broadcast channel.
        let group_rdt = RequestedDialogType {
            type_: RequestedDialogTypeType::Group,
            ..RequestedDialogType::default()
        };
        let env = TestEnv {
            broadcast: true,
            ..TestEnv::default()
        };
        assert_eq!(
            group_rdt
                .check_shared_dialog(&env, DialogId::new(-1_000_000_000_001))
                .expect_err("wrong type")
                .message(),
            "Wrong chat type"
        );

        // restrict_has_username + has_username over an empty username.
        let rdt = RequestedDialogType {
            type_: RequestedDialogTypeType::Channel,
            restrict_has_username: true,
            has_username: true,
            ..RequestedDialogType::default()
        };
        let env = TestEnv {
            broadcast: true,
            ..TestEnv::default()
        };
        assert_eq!(
            rdt.check_shared_dialog(&env, DialogId::new(-1_000_000_000_001))
                .expect_err("username")
                .message(),
            "Wrong has_username value"
        );
    }

    #[test]
    fn check_shared_dialog_count_table() {
        let rdt = RequestedDialogType {
            max_quantity: 2,
            ..RequestedDialogType::default()
        };
        assert_eq!(
            rdt.check_shared_dialog_count(0)
                .expect_err("zero")
                .message(),
            "Too few chats are chosen"
        );
        assert!(rdt.check_shared_dialog_count(2).is_ok());
        assert_eq!(
            rdt.check_shared_dialog_count(3)
                .expect_err("three")
                .message(),
            "Too many chats are chosen"
        );
    }
}
