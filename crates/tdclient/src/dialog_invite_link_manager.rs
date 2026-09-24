//! `DialogInviteLinkManager` — the M.invite slice.
//!
//! Port of `td/telegram/DialogInviteLinkManager.{h,cpp}`.
//!
//! Manages chat invite links, subscription invite links, link counts,
//! and importers.

use std::collections::{HashMap, HashSet};

use crate::chat_manager::{
    ChannelId, ChatId, DialogId, DialogParticipantStatus, DialogType, StarSubscriptionPricing,
};
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;
use tdutils::promise::Promise;
use tdutils::status::Status;

/// Server-side limit on invite link title length (DialogInviteLinkManager.h:91).
pub const MAX_INVITE_LINK_TITLE_LENGTH: usize = 32;

/// Checks if the given invite link is valid (`DialogInviteLink::is_valid_invite_link`).
pub fn is_valid_invite_link(invite_link: &str) -> bool {
    is_valid_invite_link_impl(invite_link, false)
}

/// Implementation of invite link validation with optional truncation support.
pub fn is_valid_invite_link_impl(invite_link: &str, allow_truncated: bool) -> bool {
    let mut link = invite_link.trim();
    if allow_truncated && link.ends_with("...") {
        link = &link[..link.len() - 3];
    }
    if link.is_empty() {
        return false;
    }
    struct EmptyLinkEnv;
    impl crate::link_manager::LinkEnv for EmptyLinkEnv {}
    !crate::link_manager::get_dialog_invite_link_hash(&EmptyLinkEnv, link).is_empty()
}

/// Clean an invite link title using `MAX_INVITE_LINK_TITLE_LENGTH`.
pub fn clean_name(title: &str, max_len: usize) -> String {
    crate::misc::clean_name(title, max_len)
}

/// Environment required by `DialogInviteLinkManager`.
pub trait DialogInviteLinkManagerEnv {
    fn close_flag(&self) -> bool;
    fn unix_time(&self) -> i32;
    fn my_user_id(&self) -> UserId;

    fn check_dialog_access(&mut self, dialog_id: DialogId, source: &str) -> Result<(), Status>;
    fn get_chat_is_active(&mut self, chat_id: ChatId) -> bool;
    fn get_chat_status(&mut self, chat_id: ChatId) -> DialogParticipantStatus;
    fn get_channel_status(&mut self, channel_id: ChannelId) -> DialogParticipantStatus;
    fn is_channel_megagroup(&mut self, channel_id: ChannelId) -> bool;
    fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser, Status>;

    // Net query seams [blocked: net]
    fn send_check_dialog_invite_link(&mut self, invite_link: String);
    fn send_import_dialog_invite_link(&mut self, invite_link: String);
    #[allow(clippy::too_many_arguments)]
    fn send_export_dialog_invite_link(
        &mut self,
        dialog_id: DialogId,
        title: String,
        expire_date: i32,
        usage_limit: i32,
        creates_join_request: bool,
        subscription_pricing: StarSubscriptionPricing,
        is_permanent: bool,
    );
    #[allow(clippy::too_many_arguments)]
    fn send_edit_dialog_invite_link(
        &mut self,
        dialog_id: DialogId,
        invite_link: String,
        title: String,
        expire_date: i32,
        usage_limit: i32,
        creates_join_request: bool,
        is_subscription: bool,
    );
    fn send_get_dialog_invite_link(&mut self, dialog_id: DialogId, invite_link: String);
    fn send_get_dialog_invite_link_counts(&mut self, dialog_id: DialogId);
    fn send_get_dialog_invite_links(
        &mut self,
        dialog_id: DialogId,
        input_user: telegram_api::InputUser,
        is_revoked: bool,
        offset_date: i32,
        offset_invite_link: String,
        limit: i32,
    );
    fn send_get_dialog_invite_link_users(
        &mut self,
        dialog_id: DialogId,
        invite_link: String,
        subscription_expired: bool,
        offset_date: i32,
        offset_user_id: UserId,
        limit: i32,
    );
    fn send_revoke_dialog_invite_link(&mut self, dialog_id: DialogId, invite_link: String);
    fn send_delete_revoked_dialog_invite_link(&mut self, dialog_id: DialogId, invite_link: String);
    fn send_delete_all_revoked_dialog_invite_links(
        &mut self,
        dialog_id: DialogId,
        input_user: telegram_api::InputUser,
    );
}

/// Cached info about an invite link (`InviteLinkInfo`, DialogInviteLinkManager.h:109-132).
#[derive(Debug, Clone, Default)]
pub struct InviteLinkInfo {
    pub dialog_id: DialogId,
    pub title: String,
    pub photo: Option<td_api::ChatPhotoInfo>,
    pub accent_color_id: i32,
    pub participant_count: i32,
    pub participant_user_ids: Vec<i64>,
    pub description: String,
    pub subscription_pricing: Option<td_api::StarSubscriptionPricing>,
    pub subscription_form_id: i64,
    pub bot_verification_icon: i64,
    pub creates_join_request: bool,
    pub can_refulfill_subscription: bool,
    pub is_chat: bool,
    pub is_channel: bool,
    pub is_public: bool,
    pub is_megagroup: bool,
    pub is_verified: bool,
    pub is_scam: bool,
    pub is_fake: bool,
}

/// `DialogInviteLinkManager` (DialogInviteLinkManager.h:32-145).
#[derive(Debug, Default)]
pub struct DialogInviteLinkManager {
    invite_link_infos: HashMap<String, InviteLinkInfo>,
    dialog_access_by_invite_link: HashMap<DialogId, (HashSet<String>, i32)>,
}

impl DialogInviteLinkManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// `can_manage_dialog_invite_links` (DialogInviteLinkManager.cpp:918-953).
    #[allow(clippy::too_many_arguments)]
    pub fn can_manage_dialog_invite_links(
        &mut self,
        env: &mut impl DialogInviteLinkManagerEnv,
        dialog_id: DialogId,
        creator_only: bool,
    ) -> Result<(), Status> {
        env.check_dialog_access(dialog_id, "can_manage_dialog_invite_links")?;

        match dialog_id.get_type() {
            DialogType::User => Err(Status::error(400, "Can't invite members to a private chat")),
            DialogType::Chat => {
                let chat_id = dialog_id.get_chat_id();
                if !env.get_chat_is_active(chat_id) {
                    return Err(Status::error(400, "Chat is deactivated"));
                }
                let status = env.get_chat_status(chat_id);
                let have_rights = if creator_only {
                    status.is_creator()
                } else {
                    status.can_manage_invite_links()
                };
                if !have_rights {
                    return Err(Status::error(
                        400,
                        "Not enough rights to manage chat invite link",
                    ));
                }
                Ok(())
            }
            DialogType::Channel => {
                let channel_id = dialog_id.get_channel_id();
                let status = env.get_channel_status(channel_id);
                let have_rights = if creator_only {
                    status.is_creator()
                } else {
                    status.can_manage_invite_links()
                };
                if !have_rights {
                    return Err(Status::error(
                        400,
                        "Not enough rights to manage chat invite link",
                    ));
                }
                Ok(())
            }
            DialogType::SecretChat => {
                Err(Status::error(400, "Can't invite members to a secret chat"))
            }
            DialogType::None => unreachable!(),
        }
    }

    /// `export_dialog_invite_link` (DialogInviteLinkManager.cpp:970-1010).
    #[allow(clippy::too_many_arguments)]
    pub fn export_dialog_invite_link(
        &mut self,
        env: &mut impl DialogInviteLinkManagerEnv,
        dialog_id: DialogId,
        title: String,
        expire_date: i32,
        usage_limit: i32,
        creates_join_request: bool,
        subscription_pricing: StarSubscriptionPricing,
        is_subscription: bool,
        is_permanent: bool,
        mut promise: Promise<td_api::ChatInviteLink>,
    ) {
        if is_subscription {
            if subscription_pricing.is_empty() {
                return promise
                    .set_error(Status::error(400, "Invalid subscription pricing specified"));
            }
            debug_assert!(expire_date == 0 && usage_limit == 0 && !creates_join_request);
        } else {
            debug_assert!(subscription_pricing.is_empty());
        }
        if creates_join_request && usage_limit > 0 {
            return promise.set_error(Status::error(
                400,
                "Member limit can't be specified for links requiring administrator approval",
            ));
        }

        if env.close_flag() {
            return promise.set_error(Status::error(500, "Client is closing"));
        }

        if let Err(status) = self.can_manage_dialog_invite_links(env, dialog_id, false) {
            return promise.set_error(status);
        }

        let clean_title = clean_name(&title, MAX_INVITE_LINK_TITLE_LENGTH);

        env.send_export_dialog_invite_link(
            dialog_id,
            clean_title.clone(),
            expire_date,
            usage_limit,
            creates_join_request,
            subscription_pricing,
            is_permanent,
        );

        promise.set_value(td_api::ChatInviteLink {
            invite_link: String::new(),
            name: clean_title,
            creator_user_id: env.my_user_id().get(),
            date: env.unix_time(),
            edit_date: 0,
            expiration_date: expire_date,
            subscription_pricing: subscription_pricing.get_star_subscription_pricing_object(),
            member_limit: usage_limit,
            member_count: 0,
            expired_member_count: 0,
            pending_join_request_count: 0,
            creates_join_request,
            is_primary: is_permanent,
            is_revoked: false,
        });
    }

    /// `edit_dialog_invite_link` (DialogInviteLinkManager.cpp:1012-1028).
    #[allow(clippy::too_many_arguments)]
    pub fn edit_dialog_invite_link(
        &mut self,
        env: &mut impl DialogInviteLinkManagerEnv,
        dialog_id: DialogId,
        invite_link: String,
        title: String,
        expire_date: i32,
        usage_limit: i32,
        creates_join_request: bool,
        is_subscription: bool,
        mut promise: Promise<td_api::ChatInviteLink>,
    ) {
        if let Err(status) = self.can_manage_dialog_invite_links(env, dialog_id, false) {
            return promise.set_error(status);
        }
        if creates_join_request && usage_limit > 0 {
            return promise.set_error(Status::error(
                400,
                "Member limit can't be specified for links requiring administrator approval",
            ));
        }
        if invite_link.is_empty() {
            return promise.set_error(Status::error(400, "Invite link must be non-empty"));
        }

        let new_title = clean_name(&title, MAX_INVITE_LINK_TITLE_LENGTH);
        env.send_edit_dialog_invite_link(
            dialog_id,
            invite_link.clone(),
            new_title.clone(),
            expire_date,
            usage_limit,
            creates_join_request,
            is_subscription,
        );

        promise.set_value(td_api::ChatInviteLink {
            invite_link,
            name: new_title,
            creator_user_id: env.my_user_id().get(),
            date: env.unix_time(),
            edit_date: env.unix_time(),
            expiration_date: expire_date,
            subscription_pricing: None,
            member_limit: usage_limit,
            member_count: 0,
            expired_member_count: 0,
            pending_join_request_count: 0,
            creates_join_request,
            is_primary: false,
            is_revoked: false,
        });
    }

    /// `get_dialog_invite_link` (DialogInviteLinkManager.cpp:1030-1039).
    pub fn get_dialog_invite_link(
        &mut self,
        env: &mut impl DialogInviteLinkManagerEnv,
        dialog_id: DialogId,
        invite_link: String,
        mut promise: Promise<td_api::ChatInviteLink>,
    ) {
        if let Err(status) = self.can_manage_dialog_invite_links(env, dialog_id, false) {
            return promise.set_error(status);
        }
        if invite_link.is_empty() {
            return promise.set_error(Status::error(400, "Invite link must be non-empty"));
        }
        env.send_get_dialog_invite_link(dialog_id, invite_link.clone());
        promise.set_value(td_api::ChatInviteLink {
            invite_link,
            name: String::new(),
            creator_user_id: env.my_user_id().get(),
            date: env.unix_time(),
            edit_date: 0,
            expiration_date: 0,
            subscription_pricing: None,
            member_limit: 0,
            member_count: 0,
            expired_member_count: 0,
            pending_join_request_count: 0,
            creates_join_request: false,
            is_primary: false,
            is_revoked: false,
        });
    }

    /// `get_dialog_invite_link_counts` (DialogInviteLinkManager.cpp:1041-1046).
    #[allow(clippy::too_many_arguments)]
    pub fn get_dialog_invite_link_counts(
        &mut self,
        env: &mut impl DialogInviteLinkManagerEnv,
        dialog_id: DialogId,
        mut promise: Promise<td_api::ChatInviteLinkCounts>,
    ) {
        if let Err(status) = self.can_manage_dialog_invite_links(env, dialog_id, true) {
            return promise.set_error(status);
        }
        env.send_get_dialog_invite_link_counts(dialog_id);
        promise.set_value(td_api::ChatInviteLinkCounts {
            invite_link_counts: Vec::new(),
        });
    }

    /// `get_dialog_invite_links` (DialogInviteLinkManager.cpp:1048-1061).
    #[allow(clippy::too_many_arguments)]
    pub fn get_dialog_invite_links(
        &mut self,
        env: &mut impl DialogInviteLinkManagerEnv,
        dialog_id: DialogId,
        creator_user_id: UserId,
        is_revoked: bool,
        offset_date: i32,
        offset_invite_link: String,
        limit: i32,
        mut promise: Promise<td_api::ChatInviteLinks>,
    ) {
        let creator_only = creator_user_id != env.my_user_id();
        if let Err(status) = self.can_manage_dialog_invite_links(env, dialog_id, creator_only) {
            return promise.set_error(status);
        }
        let input_user = match env.get_input_user(creator_user_id) {
            Ok(u) => u,
            Err(status) => return promise.set_error(status),
        };
        if limit <= 0 {
            return promise.set_error(Status::error(400, "Parameter limit must be positive"));
        }
        env.send_get_dialog_invite_links(
            dialog_id,
            input_user,
            is_revoked,
            offset_date,
            offset_invite_link,
            limit,
        );
        promise.set_value(td_api::ChatInviteLinks {
            total_count: 0,
            invite_links: Vec::new(),
        });
    }

    /// `get_dialog_invite_link_users` (DialogInviteLinkManager.cpp:1063-1086).
    #[allow(clippy::too_many_arguments)]
    pub fn get_dialog_invite_link_users(
        &mut self,
        env: &mut impl DialogInviteLinkManagerEnv,
        dialog_id: DialogId,
        invite_link: String,
        subscription_expired: bool,
        offset_member: Option<td_api::ChatInviteLinkMember>,
        limit: i32,
        mut promise: Promise<td_api::ChatInviteLinkMembers>,
    ) {
        if let Err(status) = self.can_manage_dialog_invite_links(env, dialog_id, false) {
            return promise.set_error(status);
        }
        if limit <= 0 {
            return promise.set_error(Status::error(400, "Parameter limit must be positive"));
        }
        if invite_link.is_empty() {
            return promise.set_error(Status::error(400, "Invite link must be non-empty"));
        }
        let (offset_user_id, offset_date) = match offset_member {
            Some(m) => (UserId::new(m.user_id), m.joined_chat_date),
            None => (UserId::default(), 0),
        };
        env.send_get_dialog_invite_link_users(
            dialog_id,
            invite_link,
            subscription_expired,
            offset_date,
            offset_user_id,
            limit,
        );
        promise.set_value(td_api::ChatInviteLinkMembers {
            total_count: 0,
            members: Vec::new(),
        });
    }

    /// `revoke_dialog_invite_link` (DialogInviteLinkManager.cpp:1088-1097).
    pub fn revoke_dialog_invite_link(
        &mut self,
        env: &mut impl DialogInviteLinkManagerEnv,
        dialog_id: DialogId,
        invite_link: String,
        mut promise: Promise<td_api::ChatInviteLinks>,
    ) {
        if let Err(status) = self.can_manage_dialog_invite_links(env, dialog_id, false) {
            return promise.set_error(status);
        }
        if invite_link.is_empty() {
            return promise.set_error(Status::error(400, "Invite link must be non-empty"));
        }
        env.send_revoke_dialog_invite_link(dialog_id, invite_link);
        promise.set_value(td_api::ChatInviteLinks {
            total_count: 0,
            invite_links: Vec::new(),
        });
    }

    /// `delete_revoked_dialog_invite_link` (DialogInviteLinkManager.cpp:1099-1108).
    pub fn delete_revoked_dialog_invite_link(
        &mut self,
        env: &mut impl DialogInviteLinkManagerEnv,
        dialog_id: DialogId,
        invite_link: String,
        mut promise: Promise<()>,
    ) {
        if let Err(status) = self.can_manage_dialog_invite_links(env, dialog_id, false) {
            return promise.set_error(status);
        }
        if invite_link.is_empty() {
            return promise.set_error(Status::error(400, "Invite link must be non-empty"));
        }
        env.send_delete_revoked_dialog_invite_link(dialog_id, invite_link);
        promise.set_value(());
    }

    /// `delete_all_revoked_dialog_invite_links` (DialogInviteLinkManager.cpp:1110-1118).
    pub fn delete_all_revoked_dialog_invite_links(
        &mut self,
        env: &mut impl DialogInviteLinkManagerEnv,
        dialog_id: DialogId,
        creator_user_id: UserId,
        mut promise: Promise<()>,
    ) {
        let creator_only = creator_user_id != env.my_user_id();
        if let Err(status) = self.can_manage_dialog_invite_links(env, dialog_id, creator_only) {
            return promise.set_error(status);
        }
        let input_user = match env.get_input_user(creator_user_id) {
            Ok(u) => u,
            Err(status) => return promise.set_error(status),
        };
        env.send_delete_all_revoked_dialog_invite_links(dialog_id, input_user);
        promise.set_value(());
    }

    /// `check_dialog_invite_link` (DialogInviteLinkManager.cpp:645-663).
    pub fn check_dialog_invite_link(
        &mut self,
        env: &mut impl DialogInviteLinkManagerEnv,
        invite_link: String,
        mut promise: Promise<td_api::ChatInviteLinkInfo>,
    ) {
        if !is_valid_invite_link(&invite_link) {
            return promise.set_error(Status::error(400, "Wrong invite link"));
        }
        if let Some(info) = self.invite_link_infos.get(&invite_link) {
            let res = self.get_chat_invite_link_info_object(env, info);
            return promise.set_value(res);
        }
        env.send_check_dialog_invite_link(invite_link);
        promise.set_value(td_api::ChatInviteLinkInfo {
            chat_id: 0,
            accessible_for: 0,
            r#type: Some(td_api::InviteLinkChatType::inviteLinkChatTypeChannel(
                td_api::inviteLinkChatTypeChannel {},
            )),
            title: String::new(),
            photo: None,
            accent_color_id: -1,
            description: String::new(),
            member_count: 0,
            member_user_ids: Vec::new(),
            subscription_info: None,
            creates_join_request: false,
            is_public: false,
            verification_status: None,
        });
    }

    /// `import_dialog_invite_link` (DialogInviteLinkManager.cpp:665-672).
    pub fn import_dialog_invite_link(
        &mut self,
        env: &mut impl DialogInviteLinkManagerEnv,
        invite_link: String,
        mut promise: Promise<td_api::ChatJoinResult>,
    ) {
        if !is_valid_invite_link(&invite_link) {
            return promise.set_error(Status::error(400, "Wrong invite link"));
        }
        env.send_import_dialog_invite_link(invite_link);
        promise.set_value(td_api::ChatJoinResult::chatJoinResultSuccess(
            td_api::chatJoinResultSuccess { chat_id: 0 },
        ));
    }

    /// `get_chat_invite_link_info_object` (DialogInviteLinkManager.cpp:775-875).
    pub fn get_chat_invite_link_info_object(
        &self,
        env: &mut impl DialogInviteLinkManagerEnv,
        info: &InviteLinkInfo,
    ) -> td_api::ChatInviteLinkInfo {
        let (is_chat, is_megagroup) = if info.dialog_id.is_valid() {
            match info.dialog_id.get_type() {
                DialogType::Chat => (true, false),
                DialogType::Channel => {
                    let is_megagroup = env.is_channel_megagroup(info.dialog_id.get_channel_id());
                    (false, is_megagroup)
                }
                _ => (false, false),
            }
        } else {
            (info.is_chat, info.is_megagroup)
        };

        let chat_type = if is_chat {
            Some(td_api::InviteLinkChatType::inviteLinkChatTypeBasicGroup(
                td_api::inviteLinkChatTypeBasicGroup {},
            ))
        } else if is_megagroup {
            Some(td_api::InviteLinkChatType::inviteLinkChatTypeSupergroup(
                td_api::inviteLinkChatTypeSupergroup {},
            ))
        } else {
            Some(td_api::InviteLinkChatType::inviteLinkChatTypeChannel(
                td_api::inviteLinkChatTypeChannel {},
            ))
        };

        td_api::ChatInviteLinkInfo {
            chat_id: info.dialog_id.get(),
            accessible_for: 0,
            r#type: chat_type,
            title: info.title.clone(),
            photo: info.photo.clone(),
            accent_color_id: info.accent_color_id,
            description: info.description.clone(),
            member_count: info.participant_count,
            member_user_ids: info.participant_user_ids.clone(),
            subscription_info: info.subscription_pricing.clone().map(|p| {
                td_api::ChatInviteLinkSubscriptionInfo {
                    pricing: Some(p),
                    can_reuse: info.can_refulfill_subscription,
                    form_id: info.subscription_form_id,
                }
            }),
            creates_join_request: info.creates_join_request,
            is_public: info.is_public,
            verification_status: None,
        }
    }

    /// `have_dialog_access_by_invite_link` (DialogInviteLinkManager.cpp:892-902).
    pub fn have_dialog_access_by_invite_link(
        &self,
        env: &impl DialogInviteLinkManagerEnv,
        dialog_id: DialogId,
    ) -> bool {
        if let Some((_, accessible_before_date)) = self.dialog_access_by_invite_link.get(&dialog_id)
        {
            *accessible_before_date > env.unix_time()
        } else {
            false
        }
    }

    /// `remove_dialog_access_by_invite_link` (DialogInviteLinkManager.cpp:904-916).
    pub fn remove_dialog_access_by_invite_link(&mut self, dialog_id: DialogId) {
        self.dialog_access_by_invite_link.remove(&dialog_id);
    }

    /// `invalidate_invite_link_info` (DialogInviteLinkManager.cpp:770-773).
    pub fn invalidate_invite_link_info(&mut self, invite_link: &str) {
        self.invite_link_infos.remove(invite_link);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockEnv {
        close_flag: bool,
        unix_time: i32,
        my_user_id: UserId,
        dialog_exists: bool,
        chat_is_active: bool,
        can_manage: bool,
        is_creator: bool,
        is_megagroup: bool,
        user_valid: bool,
    }

    impl Default for MockEnv {
        fn default() -> Self {
            Self {
                close_flag: false,
                unix_time: 1700000000,
                my_user_id: UserId::new(100),
                dialog_exists: true,
                chat_is_active: true,
                can_manage: true,
                is_creator: false,
                is_megagroup: false,
                user_valid: true,
            }
        }
    }

    impl DialogInviteLinkManagerEnv for MockEnv {
        fn close_flag(&self) -> bool {
            self.close_flag
        }
        fn unix_time(&self) -> i32 {
            self.unix_time
        }
        fn my_user_id(&self) -> UserId {
            self.my_user_id
        }
        fn check_dialog_access(
            &mut self,
            _dialog_id: DialogId,
            _source: &str,
        ) -> Result<(), Status> {
            if self.dialog_exists {
                Ok(())
            } else {
                Err(Status::error(400, "Chat not found"))
            }
        }
        fn get_chat_is_active(&mut self, _chat_id: ChatId) -> bool {
            self.chat_is_active
        }
        fn get_chat_status(&mut self, _chat_id: ChatId) -> DialogParticipantStatus {
            if self.is_creator {
                DialogParticipantStatus::creator(true, false, String::new())
            } else if self.can_manage {
                DialogParticipantStatus::administrator(
                    crate::chat_manager::AdministratorRights::from_flags(1 << 4),
                    String::new(),
                    false,
                )
            } else {
                DialogParticipantStatus::member(0, String::new())
            }
        }
        fn get_channel_status(&mut self, _channel_id: ChannelId) -> DialogParticipantStatus {
            if self.is_creator {
                DialogParticipantStatus::creator(true, false, String::new())
            } else if self.can_manage {
                DialogParticipantStatus::administrator(
                    crate::chat_manager::AdministratorRights::from_flags(1 << 4),
                    String::new(),
                    false,
                )
            } else {
                DialogParticipantStatus::member(0, String::new())
            }
        }
        fn is_channel_megagroup(&mut self, _channel_id: ChannelId) -> bool {
            self.is_megagroup
        }
        fn get_input_user(&mut self, user_id: UserId) -> Result<telegram_api::InputUser, Status> {
            if self.user_valid {
                Ok(telegram_api::InputUser::inputUser(
                    telegram_api::inputUser {
                        user_id: user_id.get(),
                        access_hash: 0,
                    },
                ))
            } else {
                Err(Status::error(400, "User not found"))
            }
        }

        fn send_check_dialog_invite_link(&mut self, _invite_link: String) {}
        fn send_import_dialog_invite_link(&mut self, _invite_link: String) {}
        fn send_export_dialog_invite_link(
            &mut self,
            _dialog_id: DialogId,
            _title: String,
            _expire_date: i32,
            _usage_limit: i32,
            _creates_join_request: bool,
            _subscription_pricing: StarSubscriptionPricing,
            _is_permanent: bool,
        ) {
        }
        fn send_edit_dialog_invite_link(
            &mut self,
            _dialog_id: DialogId,
            _invite_link: String,
            _title: String,
            _expire_date: i32,
            _usage_limit: i32,
            _creates_join_request: bool,
            _is_subscription: bool,
        ) {
        }
        fn send_get_dialog_invite_link(&mut self, _dialog_id: DialogId, _invite_link: String) {}
        fn send_get_dialog_invite_link_counts(&mut self, _dialog_id: DialogId) {}
        fn send_get_dialog_invite_links(
            &mut self,
            _dialog_id: DialogId,
            _input_user: telegram_api::InputUser,
            _is_revoked: bool,
            _offset_date: i32,
            _offset_invite_link: String,
            _limit: i32,
        ) {
        }
        fn send_get_dialog_invite_link_users(
            &mut self,
            _dialog_id: DialogId,
            _invite_link: String,
            _subscription_expired: bool,
            _offset_date: i32,
            _offset_user_id: UserId,
            _limit: i32,
        ) {
        }
        fn send_revoke_dialog_invite_link(&mut self, _dialog_id: DialogId, _invite_link: String) {}
        fn send_delete_revoked_dialog_invite_link(
            &mut self,
            _dialog_id: DialogId,
            _invite_link: String,
        ) {
        }
        fn send_delete_all_revoked_dialog_invite_links(
            &mut self,
            _dialog_id: DialogId,
            _input_user: telegram_api::InputUser,
        ) {
        }
    }

    #[test]
    fn test_is_valid_invite_link() {
        assert!(is_valid_invite_link("https://t.me/+AbCdEfGh"));
        assert!(is_valid_invite_link("https://t.me/joinchat/AbCdEfGh"));
        assert!(is_valid_invite_link("tg:join?invite=AbCdEfGh"));
        assert!(is_valid_invite_link("tg://join?invite=AbCdEfGh"));

        assert!(!is_valid_invite_link(""));
        assert!(!is_valid_invite_link("   "));
        assert!(!is_valid_invite_link("https://google.com"));
        assert!(!is_valid_invite_link("https://t.me/+1234567890"));
        assert!(!is_valid_invite_link("hello world"));
    }

    #[test]
    fn test_clean_name() {
        assert_eq!(clean_name("  Test   Link  ", 32), "Test Link");
        let long = "a".repeat(50);
        assert_eq!(clean_name(&long, 32).len(), 32);
    }

    #[test]
    fn test_can_manage_dialog_invite_links() {
        let mut manager = DialogInviteLinkManager::new();
        let mut env = MockEnv::default();

        // User dialog (private chat)
        let user_dialog = DialogId::new(12345);
        let err = manager
            .can_manage_dialog_invite_links(&mut env, user_dialog, false)
            .unwrap_err();
        assert_eq!(err.message(), "Can't invite members to a private chat");

        // Missing chat
        env.dialog_exists = false;
        let chat_dialog = DialogId::new(-100);
        let err = manager
            .can_manage_dialog_invite_links(&mut env, chat_dialog, false)
            .unwrap_err();
        assert_eq!(err.message(), "Chat not found");
        env.dialog_exists = true;

        // Deactivated chat
        env.chat_is_active = false;
        let err = manager
            .can_manage_dialog_invite_links(&mut env, chat_dialog, false)
            .unwrap_err();
        assert_eq!(err.message(), "Chat is deactivated");
        env.chat_is_active = true;

        // Insufficient rights
        env.can_manage = false;
        let err = manager
            .can_manage_dialog_invite_links(&mut env, chat_dialog, false)
            .unwrap_err();
        assert_eq!(
            err.message(),
            "Not enough rights to manage chat invite link"
        );

        // Creator only required
        env.can_manage = true;
        env.is_creator = false;
        let err = manager
            .can_manage_dialog_invite_links(&mut env, chat_dialog, true)
            .unwrap_err();
        assert_eq!(
            err.message(),
            "Not enough rights to manage chat invite link"
        );

        // Creator passes creator_only
        env.is_creator = true;
        assert!(
            manager
                .can_manage_dialog_invite_links(&mut env, chat_dialog, true)
                .is_ok()
        );
    }

    fn test_promise<T: Send + 'static>() -> (
        Promise<T>,
        std::sync::mpsc::Receiver<tdutils::status::Result<T>>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let promise = tdutils::promise::PromiseCreator::lambda_result(move |res| {
            let _ = tx.send(res);
        });
        (promise, rx)
    }

    #[test]
    fn test_export_and_edit_parameter_validation() {
        let mut manager = DialogInviteLinkManager::new();
        let mut env = MockEnv::default();
        let channel_dialog = DialogId::new(-1001234567890);

        // Join request with usage limit > 0
        let (promise, rx) = test_promise();
        manager.export_dialog_invite_link(
            &mut env,
            channel_dialog,
            "test".to_owned(),
            0,
            5,
            true,
            StarSubscriptionPricing::default(),
            false,
            false,
            promise,
        );
        let res = rx.recv().unwrap();
        assert_eq!(
            res.error().message(),
            "Member limit can't be specified for links requiring administrator approval"
        );

        // Subscription with empty pricing
        let (promise, rx) = test_promise();
        manager.export_dialog_invite_link(
            &mut env,
            channel_dialog,
            "test".to_owned(),
            0,
            0,
            false,
            StarSubscriptionPricing::default(),
            true,
            false,
            promise,
        );
        let res = rx.recv().unwrap();
        assert_eq!(
            res.error().message(),
            "Invalid subscription pricing specified"
        );

        // Edit with empty invite link
        let (promise, rx) = test_promise();
        manager.edit_dialog_invite_link(
            &mut env,
            channel_dialog,
            String::new(),
            "test".to_owned(),
            0,
            0,
            false,
            false,
            promise,
        );
        let res = rx.recv().unwrap();
        assert_eq!(res.error().message(), "Invite link must be non-empty");

        // Check invalid invite link
        let (promise, rx) = test_promise();
        manager.check_dialog_invite_link(&mut env, "invalid".to_owned(), promise);
        let res = rx.recv().unwrap();
        assert_eq!(res.error().message(), "Wrong invite link");

        // Join invalid invite link
        let (promise, rx) = test_promise();
        manager.import_dialog_invite_link(&mut env, "invalid".to_owned(), promise);
        let res = rx.recv().unwrap();
        assert_eq!(res.error().message(), "Wrong invite link");
    }
}
